//! The run read-back commands: `transcript`, `show`, `runs`, `clean`.

use crate::event::RunEvent;
use crate::paths::Layout;
use crate::render;
use crate::render::Format;
use crate::store;
use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use std::io;
use std::io::Write;
use std::time::Duration;

/// `bastion transcript [<run>] <reviewer>`: print a saved session transcript.
///
/// # Errors
///
/// Returns an error if the run or transcript does not exist.
pub fn transcript(layout: &Layout, run: Option<&str>, reviewer: &str) -> Result<()> {
    let run = store::resolve_run(layout, run)?;
    let path = layout.transcript(&run, reviewer);
    let text = std::fs::read_to_string(&path).wrap_err_with(|| {
        format!(
            "no saved transcript for reviewer '{reviewer}' in run '{run}' ({})",
            path.display()
        )
    })?;
    io::stdout()
        .write_all(text.as_bytes())
        .wrap_err("writing transcript")?;
    Ok(())
}

/// `bastion show [<run>]`: re-emit a past run's verdicts and findings.
///
/// # Errors
///
/// Returns an error if the run does not exist or cannot be read.
pub fn show(layout: &Layout, run: Option<&str>, format: Format) -> Result<()> {
    let run = store::resolve_run(layout, run)?;
    let events = store::read_run(layout, &run)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for event in &events {
        if matches!(
            event,
            RunEvent::ReviewerResolved { .. } | RunEvent::RunCompleted { .. }
        ) {
            render::write_event(&mut out, format, event)?;
        }
    }
    Ok(())
}

/// `bastion runs`: list recorded runs.
///
/// # Errors
///
/// Returns an error if the runs directory cannot be read.
pub fn runs(layout: &Layout, format: Format) -> Result<()> {
    let runs = store::list_runs(layout)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render::write_runs(&mut out, format, &runs).wrap_err("rendering runs")?;
    Ok(())
}

/// `bastion clean`: prune saved runs.
///
/// # Errors
///
/// Returns an error if a run cannot be removed.
pub fn clean(layout: &Layout, keep: Option<usize>, older_than: Option<Duration>) -> Result<()> {
    let keep = if keep.is_none() && older_than.is_none() {
        Some(default_keep())
    } else {
        keep
    };
    let removed = store::prune(layout, keep, older_than)?;
    println!("removed {} run(s)", removed.len());
    for id in &removed {
        println!("  {id}");
    }
    Ok(())
}

/// How many runs to keep when `bastion clean` is given no arguments.
fn default_keep() -> usize {
    20
}
