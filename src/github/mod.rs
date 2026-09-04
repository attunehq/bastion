//! The GitHub adapter.
//!
//! The core review surface (`src/runner.rs`, `src/verdict.rs`, ...) is
//! forge-agnostic; this module is the concrete GitHub binding used by local PR
//! detection and the CI surface. It does three things:
//!
//! - [`codeowners`] generates the governance block that protects the reviewer
//!   policy paths (pure text generation, no network).
//! - [`context`] detects a pull request with `gh`, supplies its direct base for
//!   automatic changeset selection, and gathers reviewer context (identity
//!   through `gh pr view`, discussion through `gh api`, with the REST seam as
//!   a compatibility fallback);
//! - [`report`] posts a finished run back to a pull request as a sticky comment
//!   and per-reviewer check runs, over the REST seam in [`client`].
//!
//! The HTTP boundary lives behind [`client::GitHubApi`] so the reporting logic is
//! driven against a recording double or a local fake server in tests, mirroring how
//! the backend boundary is driven against a fake agent.

pub mod client;
pub mod codeowners;
pub mod context;
pub mod report;
pub mod signing;

use color_eyre::eyre::{Result, eyre};

/// The pull-request coordinates a [`report`] posts against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrContext {
    /// The repository owner (`acme` in `acme/app`).
    pub owner: String,
    /// The repository name (`app` in `acme/app`).
    pub repo: String,
    /// The pull request number.
    pub pr: u64,
    /// The head commit SHA the check runs attach to.
    pub head_sha: String,
}

impl PrContext {
    /// Build a context from an `owner/name` slug (the shape of `GITHUB_REPOSITORY`),
    /// a PR number, and the head SHA.
    ///
    /// # Errors
    ///
    /// Returns an error if `slug` is not exactly `owner/name` with both halves
    /// non-empty.
    pub fn new(slug: &str, pr: u64, head_sha: impl Into<String>) -> Result<Self> {
        let (owner, repo) = parse_slug(slug)?;
        Ok(Self {
            owner,
            repo,
            pr,
            head_sha: head_sha.into(),
        })
    }
}

/// Split an `owner/name` repository slug into its two halves.
fn parse_slug(slug: &str) -> Result<(String, String)> {
    let mut parts = slug.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(owner), Some(repo))
            if !owner.is_empty() && !repo.is_empty() && !repo.contains('/') =>
        {
            Ok((owner.to_string(), repo.to_string()))
        }
        _ => Err(eyre!("expected an 'owner/name' repository, got '{slug}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_context_parses_a_slug() {
        let ctx = PrContext::new("acme/app", 7, "sha").unwrap();
        assert_eq!(ctx.owner, "acme");
        assert_eq!(ctx.repo, "app");
        assert_eq!(ctx.pr, 7);
        assert_eq!(ctx.head_sha, "sha");
    }

    #[test]
    fn pr_context_rejects_a_malformed_slug() {
        assert!(PrContext::new("noslash", 1, "s").is_err());
        assert!(PrContext::new("a/b/c", 1, "s").is_err());
        assert!(PrContext::new("/app", 1, "s").is_err());
        assert!(PrContext::new("acme/", 1, "s").is_err());
    }
}
