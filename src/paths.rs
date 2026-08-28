//! The on-disk layout of Bastion's per-user data directory.
//!
//! Every run is persisted under a platform data directory so it can be inspected
//! or replayed after the fact (`docs/developer-guide/local-surface.md`):
//!
//! ```text
//! <data-dir>/
//!   runs/
//!     r-0f3a/
//!       run.jsonl                 # the full event stream
//!       seal.json                 # the run seal, when the run was sealed
//!       reviewers/
//!         tenant-isolation/
//!           transcript.jsonl      # the reconstructed agent session
//!           verdict.json          # the raw structured verdict; absent on a skip
//!           meta.json             # backend, timing, usage, matched trigger
//!     latest                      # pointer to the most recent run
//!   native/                       # isolated native agent sessions (Akari handoff)
//! ```

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, eyre};

use crate::event::RunId;

/// Environment variable that overrides the data directory (used in tests and for
/// callers who want runs stored somewhere specific).
pub const DATA_DIR_ENV: &str = "BASTION_DATA_DIR";

/// Resolves paths within Bastion's data directory.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// Resolve the data directory by platform convention, honoring
    /// [`DATA_DIR_ENV`] when set:
    ///
    /// - Linux: `$XDG_DATA_HOME/bastion` (default `~/.local/share/bastion`)
    /// - macOS: `~/Library/Application Support/bastion`
    /// - Windows: `%APPDATA%\bastion`
    ///
    /// # Errors
    ///
    /// Returns an error if no home directory can be determined and no override is
    /// set.
    pub fn resolve() -> Result<Self> {
        if let Some(over) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
            return Ok(Self::with_root(PathBuf::from(over)));
        }
        let base = directories::BaseDirs::new()
            .ok_or_else(|| eyre!("could not determine a home directory; set {DATA_DIR_ENV}"))?;
        Ok(Self::with_root(base.data_dir().join("bastion")))
    }

    /// Build a layout rooted at an explicit path.
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// The data directory root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding all runs (`<root>/runs`).
    #[must_use]
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// The directory for a single run (`<root>/runs/<id>`).
    #[must_use]
    pub fn run_dir(&self, id: &RunId) -> PathBuf {
        self.runs_dir().join(id.as_str())
    }

    /// The full event stream for a run (`.../run.jsonl`).
    #[must_use]
    pub fn run_jsonl(&self, id: &RunId) -> PathBuf {
        self.run_dir(id).join("run.jsonl")
    }

    /// A run's seal (`.../seal.json`), when the run was sealed. See
    /// [`crate::seal`] and `docs/developer-guide/attestation.md`.
    #[must_use]
    pub fn seal(&self, id: &RunId) -> PathBuf {
        self.run_dir(id).join("seal.json")
    }

    /// A single reviewer's directory within a run.
    ///
    /// The reviewer name is mapped to a portable path component first: names are
    /// author-controlled and usually plain, but a merged user/repo registry scopes a
    /// colliding name with the `repo:` sentinel (see [`crate::config`]), and `:` is
    /// invalid in a Windows path component. This is the single place a reviewer name
    /// becomes a path, so the write side and every read side ([`Self::transcript`],
    /// [`Self::verdict`], [`Self::meta`]) sanitize identically and stay in agreement.
    #[must_use]
    pub fn reviewer_dir(&self, id: &RunId, reviewer: &str) -> PathBuf {
        self.run_dir(id)
            .join("reviewers")
            .join(path_component(reviewer))
    }

    /// A reviewer's saved session transcript.
    #[must_use]
    pub fn transcript(&self, id: &RunId, reviewer: &str) -> PathBuf {
        self.reviewer_dir(id, reviewer).join("transcript.jsonl")
    }

    /// A reviewer's raw structured verdict (`.../verdict.json`), absent when an
    /// agent trigger skipped the full review.
    #[must_use]
    pub fn verdict(&self, id: &RunId, reviewer: &str) -> PathBuf {
        self.reviewer_dir(id, reviewer).join("verdict.json")
    }

    /// A reviewer's metadata: backend, timing, usage, matched trigger
    /// (`.../meta.json`).
    #[must_use]
    pub fn meta(&self, id: &RunId, reviewer: &str) -> PathBuf {
        self.reviewer_dir(id, reviewer).join("meta.json")
    }

    /// Durable native agent-session tree (`<root>/native`), a sibling of `runs/`
    /// so `bastion clean` does not delete it. Isolated reviewer sessions land
    /// here so Akari can ingest them after an ephemeral worktree is gone.
    #[must_use]
    pub fn native_sessions_dir(&self) -> PathBuf {
        self.root.join("native")
    }

    /// One reviewer's native session directory within a run
    /// (`<root>/native/<run>/<reviewer>`).
    #[must_use]
    pub fn native_session_dir(&self, id: &RunId, reviewer: &str) -> PathBuf {
        self.native_sessions_dir()
            .join(id.as_str())
            .join(path_component(reviewer))
    }

    /// The pointer file recording the most recent run id (`<root>/runs/latest`).
    ///
    /// A plain file holding the id is used rather than a symlink, since symlink
    /// creation is not universally available on Windows.
    #[must_use]
    pub fn latest_pointer(&self) -> PathBuf {
        self.runs_dir().join("latest")
    }
}

/// Map a reviewer name to a single path component safe on every platform.
///
/// Replaces any character not permitted in a portable file name (the Windows
/// reserved set plus control characters) with `-`. For an ordinary name this is the
/// identity, so existing layouts are unchanged; it exists so the `repo:` scope
/// sentinel a merged registry can introduce does not produce an unwritable path.
///
/// The mapping is not injective (`a:b` and `a-b` collapse together), so the registry
/// boundary ([`crate::config`]) rejects two names that reduce to the same component
/// before any run persists, keeping this the single, consistent place a name becomes
/// a path while the uniqueness guarantee lives where names are validated.
pub(crate) fn path_component(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_takes_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        // Safety: single-threaded test; no other thread reads the environment here.
        unsafe { std::env::set_var(DATA_DIR_ENV, tmp.path()) };
        let layout = Layout::resolve().expect("resolves from override");
        assert_eq!(layout.root(), tmp.path());
        unsafe { std::env::remove_var(DATA_DIR_ENV) };
    }

    #[test]
    fn layout_paths_compose_as_documented() {
        let layout = Layout::with_root(PathBuf::from("/data"));
        let id = RunId("r-0f3a".into());
        assert!(layout.run_jsonl(&id).ends_with("runs/r-0f3a/run.jsonl"));
        assert!(
            layout
                .transcript(&id, "tenant-isolation")
                .ends_with("runs/r-0f3a/reviewers/tenant-isolation/transcript.jsonl")
        );
        assert!(layout.latest_pointer().ends_with("runs/latest"));
        assert!(layout.seal(&id).ends_with("runs/r-0f3a/seal.json"));
    }

    #[test]
    fn a_scoped_reviewer_name_maps_to_a_portable_path_component() {
        // The `repo:` collision sentinel carries a colon, which is invalid in a
        // Windows path component; the reviewer dir sanitizes it so the run store
        // stays writable, and every accessor agrees on the same sanitized form.
        let layout = Layout::with_root(PathBuf::from("/data"));
        let id = RunId("r-0f3a".into());
        let dir = layout.reviewer_dir(&id, "repo:test-coverage");
        assert!(
            dir.ends_with("runs/r-0f3a/reviewers/repo-test-coverage"),
            "colon should be replaced, got {}",
            dir.display()
        );
        assert!(
            layout
                .verdict(&id, "repo:test-coverage")
                .ends_with("runs/r-0f3a/reviewers/repo-test-coverage/verdict.json")
        );
    }

    #[test]
    fn an_ordinary_reviewer_name_is_left_untouched() {
        // The sanitizer is the identity for a plain name, so existing layouts and
        // their tests are unaffected.
        assert_eq!(path_component("tenant-isolation"), "tenant-isolation");
    }
}
