//! The handful of git queries the CLI needs.
//!
//! Bastion does not own your VCS any more than it owns your CI; it just reads the
//! current branch and the set of files changed against a base. These shell out to
//! the `git` binary, the same one the surrounding workflow already uses.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use color_eyre::eyre::{Context, Result, bail};

/// The dedicated git-notes ref an attestation bundle and its signature are
/// attached under (`docs/developer-guide/attestation.md`, "Storage: a git
/// note"). A distinct ref keeps the note independent of any other notes usage
/// and lets it push/fetch as one unit (`git push origin refs/notes/bastion`).
pub const NOTES_REF: &str = "refs/notes/bastion";

/// Check a finished `git` process and decode its trimmed stdout, or bail with the
/// command and its stderr. The shared success/failure tail of the git runners.
fn finish(args: &[&str], output: Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8(output.stdout)
        .wrap_err("git produced non-UTF-8 output")?
        .trim()
        .to_string())
}

/// Run `git` with `args` in `cwd`, returning trimmed stdout on success.
fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .wrap_err("failed to invoke git; is it installed and on PATH?")?;
    finish(args, output)
}

/// Like [`run_git`], but pipes `stdin_bytes` to the child's stdin. Used for
/// `git patch-id`, which reads the diff to fingerprint from stdin rather than
/// taking it as an argument.
fn run_git_with_stdin(cwd: &Path, args: &[&str], stdin_bytes: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to invoke git; is it installed and on PATH?")?;

    // The child's stdin handle is always present for a `Stdio::piped()` spawn.
    #[expect(
        clippy::expect_used,
        reason = "stdin is present on a Stdio::piped() spawn"
    )]
    child
        .stdin
        .take()
        .expect("stdin was requested as piped")
        .write_all(stdin_bytes)
        .wrap_err("writing to git's stdin")?;

    let output = child
        .wait_with_output()
        .wrap_err("waiting for git to finish")?;
    finish(args, output)
}

/// The repository root containing `cwd`.
///
/// # Errors
///
/// Returns an error if `cwd` is not inside a git working tree.
pub fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root))
}

/// The current branch name, or `HEAD` when detached.
///
/// # Errors
///
/// Returns an error if `git` fails.
pub fn current_branch(cwd: &Path) -> Result<String> {
    run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// The set of files changed in the working tree relative to `base`.
///
/// This is the union of tracked changes against `base` and untracked,
/// non-ignored files, i.e. everything a PR from `base` would introduce,
/// including edits not yet committed. Paths are repository-relative and sorted.
///
/// # Errors
///
/// Returns an error if `git` fails (e.g. `base` does not resolve).
pub fn changed_files(cwd: &Path, base: &str) -> Result<Vec<String>> {
    let mut files = BTreeSet::new();

    let tracked = run_git(cwd, &["diff", "--name-only", base])?;
    files.extend(
        tracked
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty()),
    );

    files.extend(untracked_files(cwd)?);

    Ok(files.into_iter().collect())
}

/// The untracked, non-ignored files in `cwd`'s working tree, repository-relative.
///
/// Split out of [`changed_files`] because the incremental-review digest
/// ([`crate::carry`]) needs to know which changed files `git diff` cannot see
/// (an untracked file has no blob to diff against), so their content is hashed
/// directly instead.
///
/// # Errors
///
/// Returns an error if `git` fails.
pub fn untracked_files(cwd: &Path) -> Result<Vec<String>> {
    let untracked = run_git(cwd, &["ls-files", "--others", "--exclude-standard"])?;
    Ok(untracked
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The diff of the working tree against `base_commit`, restricted to `paths`:
/// `git diff <base_commit> -- <paths...>`.
///
/// This is the slice of the changeset a trigger-scoped reviewer's verdict is
/// keyed to (`crate::carry`): the working tree (not HEAD) on the left of the
/// comparison's right side, because `bastion review` reviews the working tree.
/// Untracked files never appear in this output; the caller hashes their content
/// separately. An empty `paths` yields an empty diff without invoking git,
/// since `git diff <rev> --` with no pathspec would diff *everything*.
///
/// # Errors
///
/// Returns an error if `base_commit` does not resolve or `git` fails.
pub fn scoped_diff(cwd: &Path, base_commit: &str, paths: &[&str]) -> Result<String> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let mut args = vec!["diff", base_commit, "--"];
    args.extend_from_slice(paths);
    run_git(cwd, &args)
}

/// Whether `cwd`'s working tree carries uncommitted changes: a modified tracked
/// file, a staged change, or an untracked, non-ignored file.
///
/// `git status --porcelain` prints one line per such change and nothing at all
/// for a clean tree, so emptiness of its output is exactly the dirty/clean
/// signal. The run seal uses this to record whether a review saw content HEAD's
/// committed tree does not name (`docs/developer-guide/attestation.md`): a dirty
/// run still seals, but `bastion attest` refuses to attest it.
///
/// # Errors
///
/// Returns an error if `git` fails.
pub fn is_dirty(cwd: &Path) -> Result<bool> {
    Ok(!run_git(cwd, &["status", "--porcelain"])?.is_empty())
}

/// The commit messages on `HEAD` since it diverged from `base`, oldest first, as the
/// local stand-in for a pull request description.
///
/// This is the local mirror of a PR body: the author's stated intent for the change,
/// drawn from the commits on this branch (`base..HEAD`). Returns `None` when the range
/// is empty (nothing committed against `base` yet, e.g. work still entirely in the
/// working tree) or git cannot resolve the range, so an absent intent simply leaves a
/// reviewer's prompt unchanged.
#[must_use]
pub fn commit_messages(cwd: &Path, base: &str) -> Option<String> {
    let range = format!("{base}..HEAD");
    // `%B` is the raw subject and body; `--reverse` orders oldest commit first so the
    // narrative reads in the order it was written.
    run_git(cwd, &["log", "--reverse", "--format=%B", &range])
        .ok()
        .map(|messages| messages.trim().to_string())
        .filter(|messages| !messages.is_empty())
}

/// The commit messages on `base_commit..HEAD` that touched any of `paths`,
/// oldest first: `git log --reverse --format=%B <base_commit>..HEAD -- <paths...>`.
///
/// This is the trigger-scoped slice of the intent that [`commit_messages`]
/// gathers for the prompt, used by the carry digest (`crate::carry`): a
/// reviewer's verdict is keyed to the stated intent for the files its trigger
/// covers, so rewording a commit that touched them re-runs the reviewer while
/// a commit that touched only unrelated files does not. An empty `paths`
/// yields an empty string without invoking git, since `git log -- ` with no
/// pathspec would cover every commit.
///
/// # Errors
///
/// Returns an error if `base_commit` does not resolve or `git` fails.
pub fn scoped_commit_messages(cwd: &Path, base_commit: &str, paths: &[&str]) -> Result<String> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let range = format!("{base_commit}..HEAD");
    let mut args = vec!["log", "--reverse", "--format=%B", &range, "--"];
    args.extend_from_slice(paths);
    run_git(cwd, &args)
}

/// The short commit hash of `HEAD`, or `None` when git cannot supply one (for
/// example a repository with no commits yet).
///
/// Used to key a local run by the changeset head; callers fall back to a fixed
/// marker when it is absent.
#[must_use]
pub fn short_head(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "--short", "HEAD"])
        .ok()
        .filter(|sha| !sha.is_empty())
}

/// The commit `base` and `HEAD` diverged from: `git merge-base <base> HEAD`.
///
/// This is the changeset's actual starting point, which may differ from `base`
/// itself if `base` has moved on since the branch was cut. The run seal binds to
/// this commit's tree, not `base`'s current tree, so a local review remains
/// attestable even after the target branch advances.
///
/// # Errors
///
/// Returns an error if `base` does not resolve or the two histories share no
/// common ancestor (e.g. unrelated histories).
pub fn merge_base(cwd: &Path, base: &str) -> Result<String> {
    run_git(cwd, &["merge-base", base, "HEAD"])
}

/// The git tree hash `rev` points at: `git rev-parse <rev>^{tree}`.
///
/// A tree hash names content, not history, so two commits with identical file
/// contents (an amend, a rebase that changes only the message) share a tree
/// hash. That is exactly the property the run seal wants: it binds to what was
/// reviewed, not to how it got committed.
///
/// # Errors
///
/// Returns an error if `rev` does not resolve.
pub fn tree_hash(cwd: &Path, rev: &str) -> Result<String> {
    run_git(cwd, &["rev-parse", &format!("{rev}^{{tree}}")])
}

/// The stable patch-id of the diff `base_commit..HEAD`: `git diff` piped through
/// `git patch-id --stable`.
///
/// `git patch-id` fingerprints a diff's *content* (the lines added and removed),
/// ignoring line numbers and blob hashes, so it is stable across a rebase that
/// reapplies the same change on a different base. `--stable` additionally makes
/// the id independent of line order within a hunk, so it agrees across git
/// versions.
///
/// An empty diff (`base_commit` and `HEAD` are identical) is a real, well-defined
/// case: `git patch-id` prints nothing for empty input, since there is no patch to
/// fingerprint. This function represents that case as the literal string
/// `"none"` rather than an empty string, so a caller can distinguish "no patch id
/// was computed" (a bug) from "the patch id is the documented empty-diff
/// sentinel" (expected) without special-casing an empty string everywhere a
/// patch id is displayed or compared.
///
/// # Errors
///
/// Returns an error if `base_commit` does not resolve or `git diff`/`git
/// patch-id` fails.
pub fn patch_id(cwd: &Path, base_commit: &str) -> Result<String> {
    let diff = run_git(cwd, &["diff", base_commit, "HEAD"])?;
    if diff.is_empty() {
        return Ok("none".to_string());
    }
    let output = run_git_with_stdin(
        cwd,
        &["patch-id", "--stable"],
        format!("{diff}\n").as_bytes(),
    )?;
    // `git patch-id` prints "<id> <commit>"; the commit half is the diff's first
    // line's blob (meaningless here, since stdin was a raw diff, not a commit),
    // so only the id itself is wanted.
    output
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("git patch-id produced no output for a non-empty diff")
        })
}

/// The configured `git config user.signingkey`, if any.
///
/// Non-fatal by design: an unset key is the ordinary case for most
/// repositories, not an error, so this reports `None` on any failure
/// (unset key, no git config at all) rather than propagating a `Result`.
/// `bastion attest` (`src/attest.rs`) uses this as one of the ways to resolve
/// a signing key, falling back to `--key` or refusing outright when both are
/// absent.
#[must_use]
pub fn run_git_config_signingkey(cwd: &Path) -> Option<String> {
    run_git(cwd, &["config", "user.signingkey"])
        .ok()
        .filter(|v| !v.is_empty())
}

/// Read the git note under `notes_ref` attached to `rev`, if one exists:
/// `git notes --ref=<notes_ref> show <rev>`.
///
/// Returns `Ok(None)` when `rev` simply has no note, which is the ordinary case
/// for nearly every commit; only a genuine git failure (not "no note") is
/// propagated as an error. `git notes show` exits non-zero for both cases, so
/// this distinguishes them by checking whether stderr names the "no note found"
/// condition rather than treating every non-zero exit as an error.
///
/// # Errors
///
/// Returns an error if `git` fails for a reason other than the note being
/// absent (e.g. `rev` does not resolve at all).
pub fn note_show(cwd: &Path, notes_ref: &str, rev: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["notes", &format!("--ref={notes_ref}"), "show", rev])
        .current_dir(cwd)
        .output()
        .wrap_err("failed to invoke git; is it installed and on PATH?")?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8(output.stdout)
                .wrap_err("git produced non-UTF-8 output")?
                .trim()
                .to_string(),
        ));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("no note found") {
        return Ok(None);
    }
    bail!("git notes show failed: {}", stderr.trim());
}

/// Write (force-overwriting) the git note under `notes_ref` attached to `rev`:
/// `git notes --ref=<notes_ref> add -f -F <tempfile> <rev>`.
///
/// Force-overwrite is deliberate: re-running `bastion attest` on a commit that
/// already carries a note (a re-review after fixing a finding, say) should
/// replace the stale note rather than fail. `content` is written through a
/// temporary file (`-F`) rather than `-m` on the command line, since a bundle can
/// be arbitrarily large and contain characters unsafe to pass as a single shell
/// argument.
///
/// # Errors
///
/// Returns an error if the temporary file cannot be written or `git notes add`
/// fails.
pub fn note_add(cwd: &Path, notes_ref: &str, rev: &str, content: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new().wrap_err("creating a temporary note file")?;
    file.write_all(content.as_bytes())
        .wrap_err("writing note content to a temporary file")?;
    file.flush().wrap_err("flushing the temporary note file")?;

    run_git(
        cwd,
        &[
            "notes",
            &format!("--ref={notes_ref}"),
            "add",
            "-f",
            "-F",
            &file.path().to_string_lossy(),
            rev,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git config flags that make a temp repo deterministic regardless of the
    /// developer's global git configuration.
    const ISOLATE: &[&str] = &[
        "-c",
        "user.email=test@bastion.dev",
        "-c",
        "user.name=Bastion Test",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "init.defaultBranch=main",
    ];

    fn git(cwd: &Path, args: &[&str]) {
        let full: Vec<&str> = ISOLATE
            .iter()
            .copied()
            .chain(args.iter().copied())
            .collect();
        run_git(cwd, &full).unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
        // The `-c` isolation above only covers commands issued through this
        // helper. Production code under test (`note_add`, say) runs plain `git`
        // in the same repo and needs an identity from config on a host that has
        // none (CI), so persist one repo-locally at init.
        if args.first() == Some(&"init") {
            git(cwd, &["config", "user.email", "grace@bastion.dev"]);
            git(cwd, &["config", "user.name", "Grace Hopper"]);
        }
    }

    #[test]
    fn changed_files_reports_tracked_edits_and_untracked_additions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        // Dirty the working tree: edit a tracked file, add an untracked one.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();

        let changed = changed_files(dir, "main").expect("diff against main");
        assert!(changed.contains(&"a.txt".to_string()), "got {changed:?}");
        assert!(changed.contains(&"b.txt".to_string()), "got {changed:?}");

        assert_eq!(current_branch(dir).unwrap(), "main");
        assert_eq!(
            repo_root(dir).unwrap().canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn commit_messages_reads_the_branch_log_oldest_first_and_trims_empty_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);
        // Mark the base, then add two commits on top of it.
        git(dir, &["branch", "base"]);
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        git(dir, &["commit", "-am", "first change"]);
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        git(dir, &["commit", "-am", "second change"]);

        // The two commits on top of `base`, oldest first.
        let messages = commit_messages(dir, "base").expect("commits exist past base");
        let first = messages.find("first change").expect("first commit present");
        let second = messages
            .find("second change")
            .expect("second commit present");
        assert!(
            first < second,
            "oldest commit should come first: {messages:?}"
        );
        // The base commit is below the range and must not appear.
        assert!(!messages.contains("base"), "base is excluded: {messages:?}");

        // An empty range (HEAD is at base) trims to `None` rather than an empty string.
        assert_eq!(commit_messages(dir, "HEAD"), None);
        // An unresolvable range is also `None`, not an error.
        assert_eq!(commit_messages(dir, "no-such-ref"), None);
    }

    #[test]
    fn short_head_reports_a_hash_after_a_commit_and_none_before() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);

        // No commits yet: HEAD does not resolve.
        assert!(short_head(dir).is_none());

        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        let sha = short_head(dir).expect("a commit exists");
        assert!(!sha.is_empty());
        // A short hash is a handful of hex characters with no whitespace.
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "got {sha:?}");
    }

    #[test]
    fn merge_base_and_tree_hash_resolve_over_a_diverged_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);
        let base_sha = run_git(dir, &["rev-parse", "HEAD"]).unwrap();

        git(dir, &["branch", "base"]);
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        git(dir, &["commit", "-am", "feature work"]);

        let merge_base = merge_base(dir, "base").expect("a common ancestor exists");
        assert_eq!(merge_base, base_sha);

        let head_tree = tree_hash(dir, "HEAD").expect("HEAD resolves a tree");
        let base_tree = tree_hash(dir, &merge_base).expect("merge base resolves a tree");
        assert_ne!(
            head_tree, base_tree,
            "the two commits have different content, so different trees"
        );
        assert!(!head_tree.is_empty());
    }

    #[test]
    fn identical_content_after_amend_or_rebase_yields_identical_tree_and_patch_id() {
        // Two commits with the same file content but different messages (standing
        // in for an amend or a rebase that only rewrites history, not content) must
        // produce the same tree hash and the same patch-id: the seal binds content,
        // not commit identity.
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        for dir in [dir_a.path(), dir_b.path()] {
            git(dir, &["init"]);
            std::fs::write(dir.join("a.txt"), "one\n").unwrap();
            git(dir, &["add", "a.txt"]);
            git(dir, &["commit", "-m", "base"]);
            git(dir, &["branch", "base"]);
        }

        std::fs::write(dir_a.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(dir_a.path(), &["commit", "-am", "message A"]);

        std::fs::write(dir_b.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(
            dir_b.path(),
            &["commit", "-am", "a completely different message B"],
        );

        let tree_a = tree_hash(dir_a.path(), "HEAD").unwrap();
        let tree_b = tree_hash(dir_b.path(), "HEAD").unwrap();
        assert_eq!(
            tree_a, tree_b,
            "identical content must yield identical trees"
        );

        let base_a = merge_base(dir_a.path(), "base").unwrap();
        let base_b = merge_base(dir_b.path(), "base").unwrap();
        let patch_a = patch_id(dir_a.path(), &base_a).unwrap();
        let patch_b = patch_id(dir_b.path(), &base_b).unwrap();
        assert_eq!(
            patch_a, patch_b,
            "identical diffs must yield identical patch ids regardless of commit message"
        );
    }

    #[test]
    fn patch_id_of_an_empty_diff_is_the_documented_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        // HEAD..HEAD is an empty diff.
        let id = patch_id(dir, "HEAD").expect("empty diff still resolves");
        assert_eq!(id, "none");
    }

    #[test]
    fn patch_id_differs_for_different_diffs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);
        let base_sha = run_git(dir, &["rev-parse", "HEAD"]).unwrap();

        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(dir, &["commit", "-am", "add a line"]);
        let id_1 = patch_id(dir, &base_sha).unwrap();

        std::fs::write(dir.join("a.txt"), "one\nthree\nfour\n").unwrap();
        git(dir, &["commit", "-am", "add different lines"]);
        let id_2 = patch_id(dir, &base_sha).unwrap();

        assert_ne!(id_1, id_2);
        assert_ne!(id_1, "none");
    }

    #[test]
    fn is_dirty_is_false_on_a_clean_repo_and_true_after_a_tracked_edit_or_an_untracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        assert!(!is_dirty(dir).unwrap(), "a freshly committed repo is clean");

        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        assert!(
            is_dirty(dir).unwrap(),
            "an edited tracked file makes the tree dirty"
        );

        git(dir, &["checkout", "--", "a.txt"]);
        assert!(
            !is_dirty(dir).unwrap(),
            "reverting the edit cleans the tree again"
        );

        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        assert!(
            is_dirty(dir).unwrap(),
            "an untracked file also makes the tree dirty"
        );
    }

    #[test]
    fn notes_round_trip_and_show_returns_none_without_a_note() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        assert_eq!(note_show(dir, NOTES_REF, "HEAD").unwrap(), None);

        note_add(dir, NOTES_REF, "HEAD", "bundle-v1").expect("note is written");
        assert_eq!(
            note_show(dir, NOTES_REF, "HEAD").unwrap(),
            Some("bundle-v1".to_string())
        );

        // A second commit with no note of its own still shows none.
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        git(dir, &["add", "b.txt"]);
        git(dir, &["commit", "-m", "second"]);
        assert_eq!(note_show(dir, NOTES_REF, "HEAD").unwrap(), None);

        // Force-overwrite replaces the existing note on the first commit.
        note_add(dir, NOTES_REF, "HEAD~1", "bundle-v2").expect("note is overwritten");
        assert_eq!(
            note_show(dir, NOTES_REF, "HEAD~1").unwrap(),
            Some("bundle-v2".to_string())
        );
    }
}
