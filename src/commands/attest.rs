//! The `bastion attest` handler.

use crate::git;
use crate::paths::Layout;
use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use std::io;
use std::path::Path;

/// `bastion attest`: sign the latest sealed run (or `run`, when given) as an
/// attestation note on HEAD.
///
/// Resolves the repository root from the current directory and hands off to
/// [`crate::attest::attest`] with the build's embedded sealing secret; the real
/// verification and signing logic lives there so it stays testable without a
/// CLI harness.
///
/// # Errors
///
/// Returns an error under any of the refusals documented on
/// [`crate::attest::attest`] (an unsealed run, a seam-tainted seal, a
/// tampered run store, repository drift since the run, or no resolvable
/// signing key), or if writing to stdout fails.
pub fn attest(layout: &Layout, run: Option<&str>, key: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
    let root = git::repo_root(&cwd)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    crate::attest::attest(
        &root,
        layout,
        run,
        key,
        crate::seal::embedded_secret(),
        &mut out,
    )
}
