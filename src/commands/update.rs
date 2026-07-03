//! The `bastion update` self-updater handler.

use color_eyre::eyre::Context;
use color_eyre::eyre::Result;

/// `bastion update`: resolve the latest release and swap the running binary for it.
///
/// With `--check` it only reports where the running version stands. Otherwise it
/// downloads the platform archive, verifies it against the release SHA-256
/// checksums, and installs it over the running binary in place. `--force`
/// reinstalls even when already up to date.
///
/// # Errors
///
/// Returns an error if the latest release cannot be resolved, the download or
/// checksum verification fails, or the in-place binary swap fails.
pub async fn update(check: bool, force: bool) -> Result<()> {
    use crate::update::{self, Status};

    let current = crate::version::VERSION;
    let updater = update::Updater::new()?;
    let latest = updater
        .latest_tag()
        .await
        .wrap_err("resolve the latest bastion release")?;
    let status = update::status(current, &latest);

    if check {
        print_update_status(current, &latest, status);
        return Ok(());
    }
    if status == Status::UpToDate && !force {
        println!("bastion {current} is already the latest release.");
        return Ok(());
    }

    match status {
        // A development build has no meaningful ordering against the release, so be
        // explicit that this installs the latest published release over it.
        Status::Development => println!(
            "Installing the latest release {latest} (replacing development build {current})."
        ),
        _ => println!("Updating bastion from {current} to {latest}."),
    }

    // Stage the download in a temp file, then let self-replace swap it over the
    // running binary. Dropping the staged file afterward removes the leftover.
    let staged = tempfile::Builder::new()
        .prefix("bastion-update-")
        .tempfile()
        .wrap_err("create a staging file for the update")?;
    updater.fetch(&latest, staged.path()).await?;
    update::replace_running_exe(staged.path())?;
    drop(staged);

    println!("bastion updated to {latest}.");
    Ok(())
}

/// The detached background worker (`bastion __update-check`) spawned by the startup
/// check: refresh the cached latest release, silently, then exit. Any error is
/// swallowed inside [`crate::update::run_check_worker`], so a failed refresh never
/// surfaces; the cache is retried after its TTL.
///
/// # Errors
///
/// Never returns an error; the signature matches the other async handlers so the
/// dispatcher can treat it uniformly.
pub async fn update_check_worker() -> Result<()> {
    crate::update::run_check_worker().await;
    Ok(())
}

/// Print the result of `bastion update --check`.
fn print_update_status(current: &str, latest: &str, status: crate::update::Status) {
    use crate::update::Status;
    match status {
        Status::Development => {
            println!("bastion is a development build ({current}); the latest release is {latest}.");
            println!("Run `bastion update` to install it.");
        }
        Status::UpToDate => {
            println!("bastion {current} is up to date (latest release {latest}).");
        }
        Status::UpdateAvailable => {
            println!("update available: {latest} (current {current}).");
            println!("Run `bastion update` to install it.");
        }
    }
}
