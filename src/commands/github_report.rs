//! The `bastion github report` handler.

use super::skills::stale_skills_warning;
use crate::paths::Layout;
use crate::store;
use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use std::io;
use std::io::Write;
use std::path::Path;

/// `bastion github report`: post a finished run's results to its pull request.
///
/// Reads the persisted run (the latest, or `run` when given), builds the GitHub
/// client from the Actions environment (`GITHUB_TOKEN`, `GITHUB_API_URL`), and
/// upserts the sticky PR comment plus a check run per reviewer and the aggregate
/// `bastion` check. The run is already persisted by `bastion review`, so this is a
/// pure read-and-post step that can run after the gate has decided.
///
/// `slug` is the `owner/name` repository, `pr` the pull request number, and `sha`
/// the head commit the check runs attach to (all supplied by the workflow from the
/// pull-request event).
///
/// # Errors
///
/// Returns an error if the run cannot be read, the client cannot be built (e.g. a
/// missing token), or a GitHub request fails. A missing run is reported as a
/// non-fatal notice (so a report step running after an infrastructure failure does
/// not pile a second, confusing error on top of the real one).
pub async fn github_report(
    layout: &Layout,
    cwd: &Path,
    slug: &str,
    pr: u64,
    sha: &str,
    run: Option<&str>,
) -> Result<()> {
    let ctx = crate::github::PrContext::new(slug, pr, sha)?;

    let run = match store::resolve_run(layout, run) {
        Ok(run) => run,
        Err(err) => {
            // No run to report: surface it as a notice and stop, rather than failing
            // the step on top of whatever already went wrong upstream.
            eprintln!("bastion github report: nothing to report ({err:#})");
            return Ok(());
        }
    };
    let events = store::read_run(layout, &run)?;

    // Fold a skills-freshness advisory into the comment when the checked-out repo's
    // bundled skills are missing or stale, mirroring the stderr notice the local
    // review prints. Best effort, so a check error never fails the report step.
    let skills_warning = stale_skills_warning(cwd);

    let client = crate::github::client::RestClient::from_env()?;
    let summary =
        crate::github::report::report(&client, &ctx, &events, skills_warning.as_ref()).await?;

    writeln!(
        io::stdout(),
        "Reported run {run} to {slug}#{pr}: {summary}."
    )
    .wrap_err("writing report summary")?;
    Ok(())
}
