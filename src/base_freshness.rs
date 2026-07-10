//! Freshness checks for branch bases used by `bastion review`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use color_eyre::eyre::{Context, Result, bail};

/// A fetched local-branch/upstream pair that can be sampled again after a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseFreshness {
    repo: PathBuf,
    base: String,
    upstream: String,
    remote: String,
    initial_upstream_commit: String,
}

/// The result of checking a review base before any reviewer starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseStatus {
    /// The base is a commit, tag, remote-tracking ref, or local branch without an upstream.
    NotTracked,
    /// The local branch contains the freshly fetched upstream tip.
    Current(BaseFreshness),
    /// The local branch does not contain the freshly fetched upstream tip.
    Outdated {
        /// The fetched upstream snapshot retained for the post-review check.
        freshness: BaseFreshness,
        /// The commit the local base branch resolves to.
        base_commit: String,
        /// The freshly fetched upstream tip.
        upstream_commit: String,
    },
}

impl BaseStatus {
    /// Keep the post-review watcher, when the base has a configured upstream.
    #[must_use]
    pub fn freshness(&self) -> Option<&BaseFreshness> {
        match self {
            Self::NotTracked => None,
            Self::Current(freshness) | Self::Outdated { freshness, .. } => Some(freshness),
        }
    }

    /// Describe an outdated start state for user-facing output.
    #[must_use]
    pub fn outdated_warning(&self) -> Option<String> {
        let Self::Outdated {
            freshness,
            base_commit,
            upstream_commit,
        } = self
        else {
            return None;
        };
        Some(format!(
            "review base '{}' is outdated: it resolves to {}, but its freshly fetched upstream '{}' is at {}; rebase your changes against the updated remote",
            freshness.base, base_commit, freshness.upstream, upstream_commit,
        ))
    }
}

impl BaseFreshness {
    /// Fetch the same upstream again and report its new tip when it moved.
    ///
    /// # Errors
    ///
    /// Returns an error when the remote cannot be fetched or the upstream ref no
    /// longer resolves. Callers surface this as a warning because the completed
    /// review remains a valid review of its start-time snapshot.
    pub fn moved_upstream(&self) -> Result<Option<String>> {
        fetch(&self.repo, &self.remote)?;
        let now = git(&self.repo, &["rev-parse", &self.upstream])?;
        Ok((now != self.initial_upstream_commit).then_some(now))
    }

    /// The local branch name supplied as the review base.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The configured upstream ref.
    #[must_use]
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// The upstream commit observed immediately before the review.
    #[must_use]
    pub fn initial_upstream_commit(&self) -> &str {
        &self.initial_upstream_commit
    }

    /// Re-fetch after a review and describe either a moved upstream or a failed
    /// post-run check. Both are warnings because the review intentionally uses
    /// the start-time snapshot.
    #[must_use]
    pub fn post_review_warning(&self) -> Option<String> {
        match self.moved_upstream() {
            Ok(Some(now)) => Some(format!(
                "upstream '{}' moved from {} to {} while the review was running; rebase your changes against the updated remote and review again",
                self.upstream, self.initial_upstream_commit, now,
            )),
            Ok(None) => None,
            Err(err) => Some(format!(
                "could not verify whether upstream '{}' moved while the review was running: {err:#}",
                self.upstream,
            )),
        }
    }
}

/// Fetch and compare `base` with its configured upstream when `base` names a
/// local branch. Explicit commits and every other ref shape return
/// [`BaseStatus::NotTracked`].
///
/// # Errors
///
/// Returns an error when git cannot resolve the base, fetch its configured
/// remote, or compare the two commits.
pub fn check(repo: &Path, base: &str) -> Result<BaseStatus> {
    let full = git(
        repo,
        &["rev-parse", "--verify", "--symbolic-full-name", base],
    )?;
    if let Some(remote_branch) = full.strip_prefix("refs/remotes/") {
        let Some((remote, _branch)) = remote_branch.split_once('/') else {
            return Ok(BaseStatus::NotTracked);
        };
        let remote = remote.to_string();
        fetch(repo, &remote)?;
        let commit = git(repo, &["rev-parse", &full])?;
        return Ok(BaseStatus::Current(BaseFreshness {
            repo: repo.to_path_buf(),
            base: base.to_string(),
            upstream: full,
            remote,
            initial_upstream_commit: commit,
        }));
    }
    let Some(branch) = full.strip_prefix("refs/heads/") else {
        return Ok(BaseStatus::NotTracked);
    };
    let upstream = git(repo, &["for-each-ref", "--format=%(upstream)", &full])?;
    if upstream.is_empty() {
        return Ok(BaseStatus::NotTracked);
    }
    let remote = git(
        repo,
        &["for-each-ref", "--format=%(upstream:remotename)", &full],
    )?;
    if remote.is_empty() || remote == "." {
        return Ok(BaseStatus::NotTracked);
    }

    fetch(repo, &remote)?;
    let base_commit = git(repo, &["rev-parse", &full])?;
    let upstream_commit = git(repo, &["rev-parse", &upstream])?;
    let freshness = BaseFreshness {
        repo: repo.to_path_buf(),
        base: branch.to_string(),
        upstream,
        remote,
        initial_upstream_commit: upstream_commit.clone(),
    };
    let contains_upstream = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            &upstream_commit,
            &base_commit,
        ])
        .current_dir(repo)
        .status()
        .wrap_err("failed to invoke git; is it installed and on PATH?")?;
    match contains_upstream.code() {
        Some(0) => Ok(BaseStatus::Current(freshness)),
        Some(1) => Ok(BaseStatus::Outdated {
            freshness,
            base_commit,
            upstream_commit,
        }),
        _ => bail!("git merge-base --is-ancestor failed while checking review-base freshness"),
    }
}

fn fetch(repo: &Path, remote: &str) -> Result<()> {
    finish(
        &["fetch", "--quiet", remote],
        Command::new("git")
            .args(["fetch", "--quiet", remote])
            .current_dir(repo)
            .output()
            .wrap_err("failed to invoke git; is it installed and on PATH?")?,
    )
    .map(|_| ())
    .wrap_err_with(|| format!("fetching review-base remote '{remote}'"))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .wrap_err("failed to invoke git; is it installed and on PATH?")?;
    finish(args, output)
}

fn finish(args: &[&str], output: Output) -> Result<String> {
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .wrap_err("git produced non-UTF-8 output")?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Repos {
        _temp: TempDir,
        local: PathBuf,
        other: PathBuf,
    }

    impl Repos {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let remote = temp.path().join("remote.git");
            command(
                temp.path(),
                &["init", "--bare", remote.to_str().expect("path")],
            );
            let seed = temp.path().join("seed");
            command(
                temp.path(),
                &["init", "-b", "main", seed.to_str().expect("path")],
            );
            configure(&seed);
            std::fs::write(seed.join("file.txt"), "one\n").expect("writes fixture");
            command(&seed, &["add", "."]);
            command(&seed, &["commit", "-m", "initial"]);
            command(
                &seed,
                &["remote", "add", "origin", remote.to_str().expect("path")],
            );
            command(&seed, &["push", "-u", "origin", "main"]);

            let local = temp.path().join("local");
            let other = temp.path().join("other");
            command(
                temp.path(),
                &[
                    "clone",
                    "-b",
                    "main",
                    remote.to_str().expect("path"),
                    local.to_str().expect("path"),
                ],
            );
            command(
                temp.path(),
                &[
                    "clone",
                    "-b",
                    "main",
                    remote.to_str().expect("path"),
                    other.to_str().expect("path"),
                ],
            );
            configure(&local);
            configure(&other);
            Self {
                _temp: temp,
                local,
                other,
            }
        }

        fn advance_remote(&self, content: &str) {
            std::fs::write(self.other.join("file.txt"), content).expect("writes fixture");
            command(&self.other, &["add", "."]);
            command(&self.other, &["commit", "-m", "advance"]);
            command(&self.other, &["push", "origin", "main"]);
        }
    }

    fn configure(repo: &Path) {
        command(repo, &["config", "user.email", "grace.hopper@example.com"]);
        command(repo, &["config", "user.name", "Grace Hopper"]);
    }

    fn command(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("runs git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn explicit_commit_has_no_upstream_check() {
        let repos = Repos::new();
        let commit = git(&repos.local, &["rev-parse", "HEAD"]).expect("head");
        assert_eq!(
            check(&repos.local, &commit).expect("checks"),
            BaseStatus::NotTracked
        );
    }

    #[test]
    fn tracked_branch_is_current_when_it_contains_upstream() {
        let repos = Repos::new();
        assert!(matches!(
            check(&repos.local, "main").expect("checks"),
            BaseStatus::Current(_)
        ));
    }

    #[test]
    fn remote_tracking_base_is_fetched_and_watched() {
        let repos = Repos::new();
        repos.advance_remote("two\n");
        let status = check(&repos.local, "origin/main").expect("checks");
        let freshness = status.freshness().expect("remote base is watched");
        assert!(matches!(status, BaseStatus::Current(_)));
        assert_eq!(
            freshness.initial_upstream_commit(),
            git(&repos.other, &["rev-parse", "HEAD"]).expect("head")
        );
    }

    #[test]
    fn tracked_branch_is_outdated_after_remote_advances() {
        let repos = Repos::new();
        repos.advance_remote("two\n");
        let status = check(&repos.local, "main").expect("checks");
        assert!(matches!(status, BaseStatus::Outdated { .. }));
        assert!(
            status
                .outdated_warning()
                .expect("warning")
                .contains("rebase")
        );
    }

    #[test]
    fn post_review_check_reports_an_upstream_move() {
        let repos = Repos::new();
        let status = check(&repos.local, "main").expect("checks");
        repos.advance_remote("two\n");
        let warning = status
            .freshness()
            .and_then(BaseFreshness::post_review_warning)
            .expect("warning");
        assert!(warning.contains("moved"));
        assert!(warning.contains("review again"));
    }
}
