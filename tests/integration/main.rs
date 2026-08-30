//! End-to-end integration suite.
//!
//! Everything else in this crate is exercised by inline `#[cfg(test)]` modules
//! over real pure functions and the injectable backend seam. This file is the
//! missing top: it drives the *real compiled `bastion` binary* (via
//! `CARGO_BIN_EXE_bastion`) as a black box, each scenario in its own isolated
//! environment -- a throwaway `git` repository, a private `BASTION_DATA_DIR`, and
//! a compiled fake agent standing in for the heavyweight Claude Code / Codex / Pi /
//! Grok Build / Muse Code subprocesses the real backends shell out to.
//!
//! The fake agent ([`fakes::FAKE_AGENT_SRC`]) is compiled once with `rustc` and
//! pointed at through `BASTION_CLAUDE_BIN` / `BASTION_CODEX_BIN` / `BASTION_PI_BIN` /
//! `BASTION_GROK_BIN` / `BASTION_MUSE_BIN`,
//! so the binary takes
//! the genuine subprocess path: real spawn, real stdin/argv, real stdout capture, real
//! parse, real fail-closed/fail-open aggregation, real persistence. The fake reads
//! per-reviewer `env` (which Bastion propagates into the child) to choose how to
//! behave -- pass, block, return malformed output, crash, or hang.
//!
//! Crucially, the fake is also a *contract checker*: before it emits anything it
//! validates the invocation it received (the structured-output flags, the piped
//! prompt, the resume/session identifiers on a reprompt) and exits non-zero on any
//! mismatch. A backend that stopped passing the schema, dropped the prompt, or
//! botched a session resume would therefore turn these green tests red, even
//! though the assertions never look at the argv directly.
//!
//! Scenarios that need a toolchain we cannot guarantee (no `rustc`, no `git`)
//! detect-and-skip rather than fail locally, mirroring the existing backend tests;
//! in CI (where `CI` is set) the tools must be present so the suite cannot silently
//! become a no-op.

// This whole target is test code, but clippy's allow-unwrap-in-tests only
// exempts `#[test]` fns and `cfg(test)`, not the helper modules of an
// integration target; in a test, a panic IS the failure report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fakes;
mod fixtures;
mod github;

mod accounting;
mod aggregation;
mod akari;
mod attestation;
mod carry;
mod cli_surface;
mod container;
mod conversation;
mod github_report;
mod persistence;
