//! The `bastion skills` handlers and the shared skills-freshness advisory.

use crate::git;
use crate::skills;
use color_eyre::eyre::Result;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// `bastion skills install`: write the bundled agent skills into the repository.
///
/// Resolves the repository root from `cwd`, writes each bundled skill into every
/// target directory (the defaults, or the `--dir` overrides), and prints what it
/// did. Existing files that differ are left untouched unless `force` is set, so a
/// local edit is never clobbered silently.
///
/// # Errors
///
/// Returns an error if a skill directory cannot be created or a file cannot be
/// read or written, or if writing the summary to stdout fails.
pub fn skills_install(cwd: &Path, dirs: &[PathBuf], force: bool) -> Result<()> {
    let root = skills_root(cwd);
    let targets = resolve_skill_dirs(dirs);
    let outcomes = skills::install(&root, &targets, force)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut skipped = 0usize;
    for outcome in &outcomes {
        let label = match outcome.status {
            skills::Installed::Created => "created",
            skills::Installed::Updated => "updated",
            skills::Installed::Unchanged => "unchanged",
            skills::Installed::Skipped => {
                skipped += 1;
                "skipped (exists)"
            }
        };
        writeln!(
            out,
            "  {label}: {}",
            skills::display_relative(&root, &outcome.path)
        )?;
    }
    if skipped > 0 {
        writeln!(
            out,
            "\n{skipped} file(s) already existed and were left as-is; re-run with --force to overwrite."
        )?;
    } else {
        writeln!(
            out,
            "\nCommit these files so your agents discover them on checkout."
        )?;
    }
    Ok(())
}

/// `bastion skills check`: verify the installed skills match this binary's
/// embedded source.
///
/// Prints one line per skill file and returns whether every one is up to date.
/// Returns `Ok(false)` when any file is missing or has drifted (a hand edit, or a
/// stale install left behind after the skill source changed), so the caller can
/// exit non-zero: a CI step can run this to fail when the checked-in skills fall
/// out of sync with the source.
///
/// # Errors
///
/// Returns an error if a skill file exists but cannot be read, or if writing the
/// summary to stdout fails.
pub fn skills_check(cwd: &Path, dirs: &[PathBuf]) -> Result<bool> {
    let root = skills_root(cwd);
    let targets = resolve_skill_dirs(dirs);
    let outcomes = skills::check(&root, &targets)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut current = true;
    for outcome in &outcomes {
        let label = match outcome.status {
            skills::Checked::UpToDate => "up to date",
            skills::Checked::Drifted => {
                current = false;
                "drifted"
            }
            skills::Checked::Missing => {
                current = false;
                "missing"
            }
        };
        writeln!(
            out,
            "  {label}: {}",
            skills::display_relative(&root, &outcome.path)
        )?;
    }
    if !current {
        writeln!(
            out,
            "\nChecked-in skills are out of sync; run `bastion skills install` to refresh them."
        )?;
    }
    Ok(current)
}

/// `bastion skills list`: show the skills bundled into this binary.
///
/// # Errors
///
/// Returns an error if writing to stdout fails.
pub fn skills_list() -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "Skills bundled in bastion {}:",
        crate::version::VERSION
    )?;
    for skill in skills::BUNDLED {
        writeln!(out, "  {} - {}", skill.slug, skill.summary)?;
    }
    writeln!(
        out,
        "\nInstall them with `bastion skills install` (default targets: {}).",
        skills::DEFAULT_DIRS.join(", ")
    )?;
    Ok(())
}

/// The repository root to install skills into: the git toplevel containing `cwd`,
/// or `cwd` itself when it is not inside a repo, so first-time setup still works.
fn skills_root(cwd: &Path) -> PathBuf {
    git::repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// The skills-freshness advisory for the repository containing `cwd`, or `None`
/// when every bundled skill is present and current.
///
/// Both review surfaces call this to warn when an agent may be working against
/// stale guidance. It is deliberately best effort, so a check error (an unreadable
/// skill file) maps to `None` rather than surfacing; a skills advisory must never
/// fail a review or a report. The default skills directories are checked, the same
/// ones `bastion skills install` writes.
pub(crate) fn stale_skills_warning(cwd: &Path) -> Option<skills::DriftWarning> {
    skills::assess(&skills_root(cwd), &skills::default_dirs())
        .ok()
        .flatten()
}

/// The skills-freshness advisory a local `bastion review` should surface, or `None`
/// when it should stay silent.
///
/// This gates [`stale_skills_warning`] on the repository having *adopted* Bastion: a
/// repository-level registry is present ([`crate::config::locate_kind`] resolves one).
/// A purely local review that merged in only the author's user-level reviewers has no
/// repo registry, and nudging that author to install skills into a project that has not
/// configured Bastion would be misdirected. Only the local surface is gated this way;
/// CI always has a repo registry, so the warning [`github_report`] folds into the
/// sticky comment is unaffected.
fn local_skills_warning(repo_root: &Path) -> Option<skills::DriftWarning> {
    // No repo registry (or an unreadable candidate): stay silent. The skills nudge is
    // meaningful only once the project itself has adopted Bastion, and a failed
    // presence check must never be the thing this advisory surfaces.
    if !matches!(crate::config::locate_kind(repo_root), Ok(Some(_))) {
        return None;
    }
    stale_skills_warning(repo_root)
}

/// Print the skills-freshness advisory to stderr, where the agent driving
/// `bastion review` sees it alongside the run. Silent when the skills are current or
/// the repository has not adopted Bastion (see [`local_skills_warning`]).
///
/// stderr keeps it out of the `--format jsonl` event stream on stdout (so a parsing
/// agent's input stays clean) while still landing somewhere both a human and an
/// agent read, matching how the GitHub-context notice is surfaced.
pub(crate) fn warn_on_stale_skills(repo_root: &Path) {
    if let Some(warning) = local_skills_warning(repo_root) {
        // Fail open on the write itself. This advisory runs before any reviewer, so a
        // failed stderr write (a broken pipe, say) must not abort an otherwise-passing
        // review the way `eprintln!` would by panicking; swallow the result instead.
        let _ = writeln!(io::stderr(), "bastion review: {}", warning.plain());
    }
}

/// The requested skill directories, falling back to the documented defaults when
/// none were passed.
fn resolve_skill_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    if dirs.is_empty() {
        skills::default_dirs()
    } else {
        dirs.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_skills_warning_fails_open_on_an_unreadable_skill() {
        // A skills-freshness check must never fail a review or a report. When the
        // assessment errors (here a directory where a SKILL.md should be, so reading
        // it fails), the warning maps to `None` rather than propagating the error.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".claude/skills/using-bastion/SKILL.md")).unwrap();
        assert!(
            stale_skills_warning(root).is_none(),
            "an assessment error should swallow to no warning, not surface"
        );
    }

    #[test]
    fn local_skills_warning_is_silent_without_a_repo_registry() {
        // A purely local review against a repo that has not adopted Bastion (no
        // `.bastion.yaml`) merges in only the author's user-level reviewers. Warning
        // there would tell the author to install skills into a project that has not
        // configured Bastion, which is misdirected. Even with every skill missing, the
        // local surface stays silent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(
            local_skills_warning(root).is_none(),
            "no repo registry should suppress the local skills advisory"
        );
    }

    #[test]
    fn local_skills_warning_fires_once_the_repo_adopts_bastion() {
        // With a repository registry present, the repo has adopted Bastion, so a stale
        // (here entirely missing) skills tree is worth flagging to the driving agent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join(crate::config::REGISTRY_FILE),
            "reviewers:\n  - name: r\n    trigger: [x]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        let warning = local_skills_warning(root).expect("a repo registry enables the advisory");
        assert!(warning.plain().contains("missing or out of date"));
    }
}
