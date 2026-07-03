//! The `bastion github codeowners` handler.

use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use std::io;
use std::io::Write;

/// `bastion github codeowners`: print a CODEOWNERS block for the reviewer-policy paths.
///
/// Covers the reviewer registry, the Bastion workflow, and the CODEOWNERS file
/// itself, so any PR touching review policy requires a human review.
///
/// # Errors
///
/// Returns an error if writing to stdout fails.
pub fn codeowners(owners: &[String]) -> Result<()> {
    io::stdout()
        .write_all(crate::github::codeowners::block(owners).as_bytes())
        .wrap_err("writing CODEOWNERS block")?;
    Ok(())
}
