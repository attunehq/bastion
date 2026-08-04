//! Self-update for the `bastion` binary from GitHub Releases.
//!
//! `bastion update` is a native updater with no dependency on a shell or `curl`:
//! it resolves the latest published release, downloads the release archive that
//! matches this binary's build target, verifies it against the release
//! `checksums.txt`, extracts the `bastion` binary, and atomically swaps it over
//! the running executable. It mirrors what `scripts/install.sh` and
//! `scripts/install.ps1` do, so a script-installed user and a self-updated user
//! converge on the same bits (down to the same SHA-256 check).
//!
//! HTTP goes through the same `reqwest` client the GitHub adapter already links,
//! so the updater adds no second TLS stack and cross-builds on every release
//! target the adapter already does. The archive handling leans on two focused,
//! pure-Rust crates: `flate2` + `tar` for extraction, and [`self_replace`] for the
//! in-place binary swap (an atomic rename on Unix, the move-aside dance on Windows
//! where a running `.exe` cannot be overwritten).
//!
//! The latest release is resolved from the `releases/latest` redirect rather than
//! `api.github.com`, matching the install scripts: the unauthenticated API is
//! rate limited to 60 requests/hour/IP and 403s from shared NATs and CI runners,
//! while the `github.com` redirect has no such limit.
//!
//! This module also drives the passive out-of-date nag ([`warn_if_outdated`])
//! that every other command runs at startup: it reads a cached latest-release
//! lookup to decide whether to warn, and refreshes that cache in a detached
//! background process ([`run_check_worker`]), so the check never blocks or fails
//! the command the user actually ran.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use color_eyre::eyre::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The GitHub `owner/repo` releases are pulled from. `BASTION_REPO` overrides it,
/// matching the `REPO` the install scripts pin.
pub const DEFAULT_REPO: &str = "attunehq/bastion";

/// The target triple this binary was built for, baked by `build.rs`. It is the
/// exact infix of the release asset name (`bastion-<target>.tar.gz`), so the
/// updater downloads the same build variant, crucially the musl vs gnu Linux
/// split that runtime OS/arch detection cannot distinguish.
pub const TARGET: &str = env!("BASTION_TARGET");

/// User-Agent sent on every request. GitHub rejects requests without one, and a
/// descriptive value makes the traffic identifiable in logs.
const USER_AGENT: &str = "bastion-selfupdate";

/// Bound on metadata reads (the `checksums.txt` body). It is tiny; this only
/// guards against a misbehaving endpoint streaming without end.
const MAX_METADATA: u64 = 4 << 20; // 4 MiB

/// Bound on the archive download and extraction, a defensive sanity cap well
/// above any real `bastion` release.
const MAX_ARCHIVE: u64 = 512 << 20; // 512 MiB

/// A whole-operation timeout: the update is a few small requests plus one archive
/// download, so a stalled network should never wedge the command.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the running version stands relative to the latest published release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The running version is a clean release at or ahead of the latest, so no
    /// update is needed (a local build one patch ahead is not told to downgrade).
    UpToDate,
    /// The running version is a clean release older than the latest.
    UpdateAvailable,
    /// The running version is not a comparable release: a `git describe` dev build
    /// (`v0.2.0-3-gabc1234`, `-dirty`), a prerelease, or an unparseable string.
    /// `bastion update` reinstalls the latest release in this case, matching what
    /// re-running the install script would do.
    Development,
}

/// Compare the running `current` version against the `latest` release tag.
///
/// Either may carry a leading `v`. `current` counts as comparable only when it is
/// a clean `X.Y.Z` release (empty semver prerelease and build metadata); a dev
/// build or prerelease resolves to [`Status::Development`] so it is always offered
/// the update rather than being compared with a misleading result.
#[must_use]
pub fn status(current: &str, latest: &str) -> Status {
    let (Some(cur), Some(lat)) = (parse_release(current), parse_release(latest)) else {
        return Status::Development;
    };
    if cur >= lat {
        Status::UpToDate
    } else {
        Status::UpdateAvailable
    }
}

/// Parse a `vX.Y.Z` release tag into a [`Version`], returning `None` for anything
/// that is not a clean release: a prerelease, build metadata, or a `git describe`
/// dev-build string that semver cannot parse. The leading `v` is optional.
fn parse_release(tag: &str) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

/// The binary's file name inside the release archive for this platform. The
/// running binary was built for [`TARGET`], so the host it runs on matches, and a
/// compile-time `cfg` is exactly right.
pub(crate) fn binary_name() -> &'static str {
    if cfg!(windows) {
        "bastion.exe"
    } else {
        "bastion"
    }
}

/// Resolves and downloads `bastion` release assets from GitHub.
pub struct Updater {
    /// The `owner/repo` to download from.
    repo: String,
    /// When set (via `BASTION_BASE_URL`), overrides the `https://github.com/<repo>`
    /// base that holds `releases/latest` and `releases/download/...`. It lets tests
    /// point at a local server.
    base_url: Option<String>,
    /// Shared HTTPS client with a global timeout, reused across the handful of
    /// requests a single update makes.
    client: reqwest::Client,
}

impl Updater {
    /// Build an updater honoring the `BASTION_REPO` and `BASTION_BASE_URL`
    /// overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be constructed.
    pub fn new() -> Result<Self> {
        let repo = env_nonempty("BASTION_REPO").unwrap_or_else(|| DEFAULT_REPO.to_string());
        Self::with_endpoints(repo, env_nonempty("BASTION_BASE_URL"))
    }

    /// Build an updater with an explicit base-URL override. Tests use this to
    /// point at a local server; [`Updater::new`] wraps it with the GitHub default.
    fn with_endpoints(repo: String, base_url: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .wrap_err("build the HTTP client for the updater")?;
        Ok(Self {
            repo,
            base_url,
            client,
        })
    }

    /// Resolve the tag of the latest published release, the same release the
    /// install scripts resolve.
    ///
    /// Follows the `releases/latest` redirect and reads the tag off the resolved
    /// `.../releases/tag/vX.Y.Z` URL rather than calling `api.github.com`, so it is
    /// not subject to the unauthenticated API rate limit. GitHub excludes
    /// prereleases from `releases/latest`, so this is always a stable `vX.Y.Z` tag.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, the response is an error status, or
    /// the resolved URL carries no parseable release tag.
    pub async fn latest_tag(&self) -> Result<String> {
        let url = format!("{}/releases/latest", self.web_base());
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .wrap_err_with(|| format!("GET {url}"))?
            .error_for_status()
            .wrap_err_with(|| format!("resolve the latest release for {}", self.repo))?;

        // The redirect lands on `.../releases/tag/vX.Y.Z`; take the segment after
        // the last `/tag/`, mirroring the install scripts' parse of the same URL.
        let resolved = response.url().as_str();
        let Some((_, after)) = resolved.rsplit_once("/tag/") else {
            bail!("could not parse a release tag from {resolved}");
        };
        let tag = after
            .split(['?', '#'])
            .next()
            .unwrap_or(after)
            .trim_end_matches('/');
        if tag.is_empty() {
            bail!("resolved latest-release URL had an empty tag: {resolved}");
        }
        Ok(tag.to_string())
    }

    /// The release archive filename for this build target, matching the name the
    /// release workflow produces (`bastion-<target>.tar.gz`).
    #[must_use]
    pub fn asset_name(&self) -> String {
        format!("bastion-{TARGET}.tar.gz")
    }

    /// The `https://github.com/<repo>` base (or the `BASTION_BASE_URL` override)
    /// holding `releases/latest` and `releases/download/...`.
    fn web_base(&self) -> String {
        match &self.base_url {
            Some(base) => base.trim_end_matches('/').to_string(),
            None => format!("https://github.com/{}", self.repo),
        }
    }

    /// The base URL holding the archive and `checksums.txt` for `tag`.
    fn download_base(&self, tag: &str) -> String {
        format!("{}/releases/download/{tag}", self.web_base())
    }

    /// Download the release archive for `tag`, verify it against the release
    /// `checksums.txt`, and extract the `bastion` binary to `dest` (0755 on Unix).
    ///
    /// The running binary is left untouched; the caller installs the extracted
    /// file with [`replace_running_exe`]. The archive is checksum-verified before a
    /// single byte is extracted, so a corrupted or tampered archive never reaches
    /// the swap.
    ///
    /// # Errors
    ///
    /// Returns an error if either download fails, the archive's SHA-256 does not
    /// match the published checksum, or the archive does not contain the binary.
    pub async fn fetch(&self, tag: &str, dest: &Path) -> Result<()> {
        let asset = self.asset_name();
        let base = self.download_base(tag);

        let sums = self
            .get_bytes(&format!("{base}/checksums.txt"))
            .await
            .wrap_err("download checksums.txt")?;
        let want = checksum_for(&sums, &asset)?;

        let (archive, got) = self
            .download_hashed(&format!("{base}/{asset}"))
            .await
            .wrap_err_with(|| format!("download {asset}"))?;
        if got != want {
            bail!("checksum mismatch for {asset} (expected {want}, got {got})");
        }

        extract_binary(io::Cursor::new(archive), dest)
    }

    /// GET `url`, erroring on a non-success status. The shared prologue for the
    /// metadata and archive fetches.
    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        self.client
            .get(url)
            .send()
            .await
            .wrap_err_with(|| format!("GET {url}"))?
            .error_for_status()
            .wrap_err_with(|| format!("GET {url}"))
    }

    /// GET `url` and return the response body, bounded by [`MAX_METADATA`].
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self.get(url).await?;
        read_capped(&mut response, MAX_METADATA)
            .await
            .wrap_err_with(|| format!("read the response from {url}"))
    }

    /// Download `url` fully into memory, returning the bytes and their hex-encoded
    /// SHA-256 so the caller can verify it against the published checksum. Bounded
    /// by [`MAX_ARCHIVE`].
    async fn download_hashed(&self, url: &str) -> Result<(Vec<u8>, String)> {
        let mut response = self.get(url).await?;
        let bytes = read_capped(&mut response, MAX_ARCHIVE)
            .await
            .wrap_err_with(|| format!("download the archive at {url}"))?;
        let hash = hex::encode(Sha256::digest(&bytes));
        Ok((bytes, hash))
    }
}

/// Read a response body into memory, failing if it exceeds `cap` bytes.
async fn read_capped(response: &mut reqwest::Response, cap: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut total: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .wrap_err("read the response stream")?
    {
        total += chunk.len() as u64;
        if total > cap {
            bail!("response exceeded {cap} bytes");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Install `new_binary` over the currently running executable.
///
/// Delegates to [`self_replace`], which owns the platform split: on Unix an atomic
/// rename swaps the file while the live process keeps its already-open image
/// mapped; on Windows, where a running `.exe` cannot be overwritten, it moves the
/// image aside and cleans it up once the process exits. `new_binary` may live in
/// any directory; `self_replace` stages its own adjacent temp for the swap, so a
/// cross-filesystem source is fine.
///
/// # Errors
///
/// Returns an error if the in-place swap fails (a permissions problem on the
/// install directory being the common cause).
pub fn replace_running_exe(new_binary: &Path) -> Result<()> {
    self_replace::self_replace(new_binary).wrap_err_with(|| {
        format!(
            "replace the running executable with {}",
            new_binary.display()
        )
    })
}

/// The hex SHA-256 listed for `asset` in a `checksums.txt` body.
///
/// The file has one `<hex>  <name>` line per asset. The release workflow runs
/// `sha256sum *.tar.gz`, so match on the trailing file name (trimming a leading
/// `./` a different `sha256sum` invocation might emit) rather than the raw field.
fn checksum_for(sums: &[u8], asset: &str) -> Result<String> {
    let text = std::str::from_utf8(sums).wrap_err("checksums.txt was not valid UTF-8")?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(hex), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name.trim_start_matches("./") == asset {
            return Ok(hex.to_string());
        }
    }
    bail!("no checksum for {asset} in checksums.txt")
}

/// Extract the `bastion` binary from a release `tar.gz` (read from `archive`) to
/// `dest`.
///
/// The archive holds a `bastion-<target>/` directory with the binary (`bastion` or
/// `bastion.exe`) alongside README/LICENSE/NOTICE, so match by base name rather
/// than a fixed path, keeping extraction robust to a layout change.
fn extract_binary(archive: impl Read, dest: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let wanted = binary_name();
    for entry in tar.entries().wrap_err("read the archive")? {
        let mut entry = entry.wrap_err("read an archive entry")?;
        let path = entry.path().wrap_err("read an archive entry path")?;
        if path.file_name().and_then(|n| n.to_str()) == Some(wanted) {
            return write_binary(&mut entry, dest)
                .wrap_err_with(|| format!("extract {wanted} to {}", dest.display()));
        }
    }
    bail!("the archive did not contain {wanted}")
}

/// Write `reader` to `dest` as an executable, replacing any existing file. `dest`
/// is a throwaway staging path; [`replace_running_exe`] installs it over the live
/// binary.
fn write_binary(reader: &mut impl Read, dest: &Path) -> Result<()> {
    let mut out =
        std::fs::File::create(dest).wrap_err_with(|| format!("create {}", dest.display()))?;
    io::copy(&mut reader.take(MAX_ARCHIVE), &mut out).wrap_err("write the extracted binary")?;
    out.flush().wrap_err("flush the extracted binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .wrap_err("mark the extracted binary executable")?;
    }
    Ok(())
}

/// Read an environment variable, returning `None` when it is unset or empty (an
/// empty override should not shadow the default).
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// How long a cached latest-release lookup stays fresh before the startup check
/// refreshes it in the background. A day keeps the nag current without checking on
/// every invocation.
const CHECK_TTL_SECS: u64 = 24 * 60 * 60;

/// The latest release the last background refresh observed, cached so the startup
/// check can decide whether to nag without touching the network on the hot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCheck {
    /// The latest release tag seen by the most recent successful refresh.
    latest: String,
    /// Unix seconds when that refresh ran, for TTL-based staleness.
    checked_at: u64,
}

/// Warn on stderr when a newer release is available, and refresh the cached latest
/// version in a detached background process for next time.
///
/// This is the passive counterpart to `bastion update`: every other command calls
/// it at startup so a user on an old build is nudged to upgrade. It is
/// deliberately cheap and non-blocking: the decision to warn reads only a small
/// on-disk cache, and the network refresh runs in a spawned-and-forgotten process
/// (see [`spawn_background_refresh`]), so it never adds latency or a failure mode
/// to the command the user actually ran.
///
/// It stays silent unless all of these hold, so it never interferes with scripts,
/// pipes, CI, or a JSONL event stream: the running binary is a tagged release (a
/// `git describe` dev build has no release to compare against), stderr is a
/// terminal, and `BASTION_NO_UPDATE_CHECK` is unset.
pub fn warn_if_outdated(current: &str) {
    // A source build has no clean release to compare against, so skip the check and
    // its background refresh entirely: developers building from a checkout should
    // never be nagged or have a worker spawned on their behalf.
    if parse_release(current).is_none() {
        return;
    }
    if env_nonempty("BASTION_NO_UPDATE_CHECK").is_some() {
        return;
    }
    // The nag is for interactive use. Staying silent when stderr is redirected
    // keeps automation output clean and, just as importantly, avoids spawning a
    // background worker on every scripted invocation.
    if !io::stderr().is_terminal() {
        return;
    }

    let cache = read_cache();

    // Warn from the last known latest release, before kicking off the refresh that
    // updates it for next time.
    if let Some(cache) = &cache
        && let Some(message) = outdated_warning(current, &cache.latest)
    {
        eprintln!("{message}");
    }

    // Refresh when the cache is missing or past its TTL. Record the attempt first
    // (a reserve write) so a burst of commands, or a run of offline invocations
    // whose worker never succeeds, backs off for a full TTL instead of respawning a
    // worker every time.
    let now = now_unix();
    let stale = cache.as_ref().is_none_or(|c| is_stale(c.checked_at, now));
    if stale {
        let latest = cache.map_or_else(|| current.to_string(), |c| c.latest);
        let _ = write_cache(&CachedCheck {
            latest,
            checked_at: now,
        });
        spawn_background_refresh();
    }
}

/// The nag to print when `current` is a released build behind `latest`, or `None`
/// when it is up to date, ahead, or not a comparable release.
fn outdated_warning(current: &str, latest: &str) -> Option<String> {
    match status(current, latest) {
        Status::UpdateAvailable => Some(format!(
            "A new bastion release is available: {latest} (you have {current}).\n\
             Run `bastion update` to upgrade, or set BASTION_NO_UPDATE_CHECK=1 to silence this."
        )),
        Status::UpToDate | Status::Development => None,
    }
}

/// The refresh body run by the detached `bastion __update-check` worker: resolve
/// the latest release and rewrite the cache. Silent and best-effort, so a failure
/// (offline, rate-limited, client build error) just leaves the previous cache to
/// be retried after the TTL.
pub async fn run_check_worker() {
    let Ok(updater) = Updater::new() else {
        return;
    };
    let Ok(latest) = updater.latest_tag().await else {
        return;
    };
    let _ = write_cache(&CachedCheck {
        latest,
        checked_at: now_unix(),
    });
}

/// Spawn a detached `bastion __update-check` process that refreshes the cache and
/// exits.
///
/// Detaching (rather than a background task) is what lets the refresh finish even
/// when the current command exits immediately: a fast command like `bastion runs`
/// would otherwise tear the task down mid-request and never update the cache. The
/// worker reports back only by writing the cache the next run reads, so its stdio
/// is discarded and its handle dropped without waiting.
fn spawn_background_refresh() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .arg("__update-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS drops the worker's tie to this command's console (so it
        // outlives it), and CREATE_NO_WINDOW keeps it from flashing a window.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    // Best effort: never wait on it (fully detached), and ignore a spawn failure.
    let _ = command.spawn();
}

/// The path of the cross-workspace update-check cache, under the user's cache
/// directory. `None` when no cache directory can be resolved (a headless or
/// misconfigured environment), which simply disables the passive check.
fn cache_path() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(base.cache_dir().join("bastion").join("update-check.json"))
}

/// Read the cached latest-release lookup, or `None` when it is absent or
/// unreadable (a corrupt or older-format cache is treated as missing and
/// refreshed).
fn read_cache() -> Option<CachedCheck> {
    let bytes = std::fs::read(cache_path()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the cache atomically (temp file then rename) so a concurrent reader never
/// sees a half-written file. A missing cache directory or write error is returned
/// for the caller to ignore: the passive check is best-effort.
fn write_cache(cache: &CachedCheck) -> io::Result<()> {
    let Some(path) = cache_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(cache).map_err(io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

/// Whether a cache entry stamped at `checked_at` is older than the refresh TTL as
/// of `now` (both Unix seconds). Saturating so a clock that moved backward reads as
/// fresh rather than panicking.
fn is_stale(checked_at: u64, now: u64) -> bool {
    now.saturating_sub(checked_at) >= CHECK_TTL_SECS
}

/// The current time in Unix seconds, or 0 if the clock is before the epoch (which
/// only makes a cache entry read as stale, the safe direction).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::net::TcpListener;

    #[test]
    fn status_reports_up_to_date_when_current_is_the_latest() {
        assert_eq!(status("v0.2.0", "v0.2.0"), Status::UpToDate);
    }

    #[test]
    fn status_reports_up_to_date_when_current_is_ahead() {
        // A local build one patch ahead of the latest release must not be told to
        // downgrade.
        assert_eq!(status("v0.3.0", "v0.2.0"), Status::UpToDate);
    }

    #[test]
    fn status_reports_update_available_when_current_is_behind() {
        assert_eq!(status("v0.1.0", "v0.2.0"), Status::UpdateAvailable);
    }

    #[test]
    fn status_treats_a_git_describe_build_as_development() {
        // The build.rs dev version format: a tag, commits since, and a short SHA.
        assert_eq!(status("v0.2.0-3-gabc1234", "v0.2.0"), Status::Development);
        assert_eq!(
            status("v0.2.0-3-gabc1234-dirty", "v0.2.0"),
            Status::Development
        );
    }

    #[test]
    fn status_treats_a_prerelease_as_development() {
        assert_eq!(status("v0.2.0-rc.1", "v0.2.0"), Status::Development);
    }

    #[test]
    fn status_treats_a_bare_hash_as_development() {
        // `git describe --always` with no tags yields a bare commit hash.
        assert_eq!(status("abc1234", "v0.2.0"), Status::Development);
    }

    #[test]
    fn outdated_warning_fires_only_for_a_release_behind_the_latest() {
        assert!(outdated_warning("v0.4.0", "v0.5.0").is_some());
        assert!(outdated_warning("v0.5.0", "v0.5.0").is_none());
        assert!(outdated_warning("v0.6.0", "v0.5.0").is_none());
        // A development build is never nagged: it has no clean release to compare.
        assert!(outdated_warning("v0.4.0-3-gabc1234", "v0.5.0").is_none());
    }

    #[test]
    fn is_stale_respects_the_ttl() {
        assert!(!is_stale(1000, 1000));
        assert!(!is_stale(1000, 1000 + CHECK_TTL_SECS - 1));
        assert!(is_stale(1000, 1000 + CHECK_TTL_SECS));
        // A backward clock jump reads as fresh, not a panic.
        assert!(!is_stale(1000, 500));
    }

    #[test]
    fn cached_check_round_trips_through_json() {
        let cache = CachedCheck {
            latest: "v1.2.3".to_string(),
            checked_at: 42,
        };
        let bytes = serde_json::to_vec(&cache).unwrap();
        let back: CachedCheck = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.latest, "v1.2.3");
        assert_eq!(back.checked_at, 42);
    }

    #[test]
    fn checksum_for_matches_the_bare_and_dot_slash_prefixed_name() {
        // The release workflow's `sha256sum *.tar.gz` writes a bare name; a
        // `./`-prefixed name (from a `./*.tar.gz` invocation) must match too.
        let sums = b"deadbeef  bastion-x86_64-unknown-linux-gnu.tar.gz\ncafef00d  ./bastion-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "bastion-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "deadbeef"
        );
        assert_eq!(
            checksum_for(sums, "bastion-aarch64-apple-darwin.tar.gz").unwrap(),
            "cafef00d"
        );
    }

    #[test]
    fn checksum_for_errors_on_a_missing_asset() {
        let sums = b"deadbeef  bastion-x86_64-unknown-linux-gnu.tar.gz\n";
        let err = checksum_for(sums, "bastion-aarch64-apple-darwin.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no checksum"), "got: {err}");
    }

    /// Build a gzip tar archive matching the release layout: a `bastion-<target>/`
    /// directory containing the platform binary with the given `contents`.
    fn build_release_archive(contents: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);

        let entry_path = format!("bastion-{TARGET}/{}", binary_name());
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_path, contents)
            .unwrap();

        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extract_binary_pulls_the_binary_out_of_the_release_layout() {
        let archive = build_release_archive(b"#!/fake bastion binary\n");
        let dest = tempfile::NamedTempFile::new().unwrap();

        extract_binary(Cursor::new(archive), dest.path()).unwrap();

        let mut got = Vec::new();
        std::fs::File::open(dest.path())
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"#!/fake bastion binary\n");
    }

    #[test]
    fn extract_binary_errors_when_the_binary_is_absent() {
        // A gzip tar with an unrelated entry: extraction must not silently succeed.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        let body = b"read me";
        header.set_size(body.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "bastion-somewhere/README.md", &body[..])
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();

        let dest = tempfile::NamedTempFile::new().unwrap();
        let err = extract_binary(Cursor::new(archive), dest.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not contain"), "got: {err}");
    }

    /// A minimal single-shot HTTP server: it serves each canned `(path, raw
    /// response)` route once, on its own connection, then the accept loop moves on.
    /// It exercises the real reqwest client without a network or extra dependency.
    fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for (path, response) in routes {
                let (mut stream, _) = listener.accept().unwrap();

                // Read the request head so the client's write side is drained
                // before we reply (GET has no body, so headers end the request).
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read(&mut byte).unwrap_or(0) == 1 {
                    req.push(byte[0]);
                    if req.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }

                let request_line = String::from_utf8_lossy(&req);
                let served = request_line.lines().next().unwrap_or("");
                assert!(
                    served.contains(&path),
                    "expected a request for {path}, got: {served}"
                );

                stream.write_all(&response).unwrap();
                stream.flush().unwrap();
            }
        });
        base
    }

    /// A `200 OK` response carrying `body`.
    fn ok(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// A `302 Found` redirect to `location` (relative locations resolve against the
    /// request URL, so this needs no knowledge of the bound port).
    fn redirect(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn latest_tag_reads_the_release_redirect() {
        // `releases/latest` 302s to `.../releases/tag/vX.Y.Z`; the updater reads the
        // tag off the resolved URL, exactly as the install scripts do.
        let base = serve(vec![
            (
                "/releases/latest".to_string(),
                redirect("/releases/tag/v9.9.9"),
            ),
            ("/releases/tag/v9.9.9".to_string(), ok(b"release page")),
        ]);
        let updater = Updater::with_endpoints("attunehq/bastion".to_string(), Some(base)).unwrap();
        assert_eq!(updater.latest_tag().await.unwrap(), "v9.9.9");
    }

    #[tokio::test]
    async fn fetch_verifies_the_checksum_and_extracts_the_binary() {
        let archive = build_release_archive(b"the new bastion\n");
        let digest = hex::encode(Sha256::digest(&archive));
        let asset = format!("bastion-{TARGET}.tar.gz");
        let checksums = format!("{digest}  {asset}\n");

        // The download base serves checksums.txt first, then the archive, in the
        // order fetch() requests them.
        let base = serve(vec![
            (
                "/releases/download/v9.9.9/checksums.txt".to_string(),
                ok(checksums.as_bytes()),
            ),
            (format!("/releases/download/v9.9.9/{asset}"), ok(&archive)),
        ]);
        let updater = Updater::with_endpoints("attunehq/bastion".to_string(), Some(base)).unwrap();

        let dest = tempfile::NamedTempFile::new().unwrap();
        updater.fetch("v9.9.9", dest.path()).await.unwrap();

        let mut got = Vec::new();
        std::fs::File::open(dest.path())
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"the new bastion\n");
    }

    #[tokio::test]
    async fn fetch_rejects_a_checksum_mismatch() {
        let archive = build_release_archive(b"tampered\n");
        let asset = format!("bastion-{TARGET}.tar.gz");
        // A checksum that does not match the served archive.
        let checksums = format!("{}  {asset}\n", "0".repeat(64));

        let base = serve(vec![
            (
                "/releases/download/v9.9.9/checksums.txt".to_string(),
                ok(checksums.as_bytes()),
            ),
            (format!("/releases/download/v9.9.9/{asset}"), ok(&archive)),
        ]);
        let updater = Updater::with_endpoints("attunehq/bastion".to_string(), Some(base)).unwrap();

        let dest = tempfile::NamedTempFile::new().unwrap();
        let err = updater
            .fetch("v9.9.9", dest.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }
}
