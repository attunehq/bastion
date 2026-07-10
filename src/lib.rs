//! Bastion: agentic code review.
//!
//! Bastion runs single-concern reviewers as fitness functions over a changeset,
//! both locally (this CLI) and in CI. Each reviewer is a focused agent prompt
//! with a trigger; matched reviewers run, return a structured [`verdict`], and
//! Bastion aggregates them into a single merge gate.
//!
//! This crate is the local surface described in `docs/developer-guide/local-surface.md`. The data and
//! routing layers, the parallel [`runner`], and the agent backends (Claude Code,
//! Codex, and Pi) are real and tested, each implementing the stable
//! [`backend::Backend`] trait.
//!
//! The module layout follows the domain rather than file kind:
//!
//! - [`reviewer`] / [`config`]: the declarative reviewer registry.
//! - [`routing`]: matching changed files to reviewers by trigger glob.
//! - [`verdict`] / [`event`]: the structured outputs reviewers and runs emit.
//! - [`git`]: the few git queries the CLI needs (changed files, branch).
//! - [`paths`] / [`store`]: the on-disk run history under the data directory.
//! - [`render`]: turning events into human or JSONL output.
//! - [`backend`]: the agent execution boundary (Claude Code and siblings).
//! - [`runner`]: the parallel, timeout-bounded runner and aggregation.
//! - [`limits`]: the per-run spend caps that bound a review's agent fan-out, so a
//!   broken or respawning run fails loud and fast instead of multiplying cost.
//! - [`carry`]: incremental re-review, carrying a prior pass forward when a
//!   reviewer's trigger-scoped diff is unchanged since the branch's last run.
//! - [`seal`]: the run seal, an HMAC over everything a verdict depends on, that
//!   makes a persisted run tamper-evident (`docs/developer-guide/attestation.md`).
//! - [`attest`]: `bastion attest`, which turns a sealed run into a signed git-note
//!   bundle CI can verify and replay (`docs/developer-guide/attestation.md`).
//! - [`skills`]: the agent skills bundled into the binary and installed into a
//!   consuming repo so its agents learn how to use Bastion.
//! - [`update`]: `bastion update`, the native self-updater that swaps the running
//!   binary for the latest GitHub release, plus the passive out-of-date nag.
//! - [`cli`] / [`commands`]: the argument surface and command handlers.

#![warn(missing_docs)]

pub mod attest;
pub mod backend;
pub mod base_freshness;
pub mod carry;
pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod event;
pub mod git;
pub mod github;
pub mod limits;
pub mod paths;
pub mod render;
pub mod reviewer;
pub mod routing;
pub mod runner;
pub mod seal;
pub mod skills;
pub mod store;
pub mod text;
pub mod update;
pub mod verdict;
pub mod version;

/// Install error reporting and tracing, then parse and dispatch the CLI.
///
/// This is the single entrypoint shared by [`main`](../src/main.rs) and by
/// integration tests that want to drive the CLI in-process. The returned
/// [`ExitCode`](std::process::ExitCode) carries the gate result: `bastion review`
/// yields a non-zero code when the aggregate verdict is `block`.
///
/// # Errors
///
/// Returns any error bubbled up from a command handler, already enriched with
/// `color_eyre` context for display.
pub async fn run() -> color_eyre::Result<std::process::ExitCode> {
    install()?;
    cli::run().await
}

/// Configure `color_eyre` panic/error reporting and a `tracing` subscriber.
///
/// Tracing defaults to `info` and is overridable via `RUST_LOG`. Logs go to
/// stderr so they never corrupt the JSONL event stream on stdout.
///
/// # Errors
///
/// Returns an error if `color_eyre` has already installed its hooks.
fn install() -> color_eyre::Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer, fmt};

    color_eyre::install()?;

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,bastion=info"));
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(env_filter),
        )
        .init();

    Ok(())
}
