//! Opt-in handoff of native agent sessions to the local Akari client.
//!
//! Disabled by default. A checkout cannot turn it on: enablement comes from a
//! user-level `akari.yaml` next to the personal reviewer registry, or from
//! `BASTION_AKARI=1`. `--repo`/`--pr` withholds the user config directory, so CI
//! stays off unless the environment variable is set explicitly.
//!
//! When enabled, Bastion isolates each reviewer's native session files under the
//! data directory and shells out to `akari ingest` after a fresh execute. A
//! handoff failure is recorded and logged; it never changes a verdict.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::backend::command::{CommandRunner, CommandSpec};
use crate::backend::concrete_backend;
use crate::reviewer::Backend;

/// User-level settings file that can enable Akari handoff.
pub const SETTINGS_FILE: &str = "akari.yaml";

/// Environment variable that enables or disables handoff (`1`/`true`/`on` or
/// `0`/`false`/`off`). When set, it wins over [`SETTINGS_FILE`].
pub const ENABLED_ENV: &str = "BASTION_AKARI";

/// Environment variable that overrides the `akari` program path.
pub const PROGRAM_ENV: &str = "BASTION_AKARI_BIN";

/// Default program name, resolved on `PATH` when [`PROGRAM_ENV`] is unset.
pub const DEFAULT_PROGRAM: &str = "akari";

/// The enabled Akari client: a program to shell out to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AkariHandoff {
    /// The `akari` program (a path or a `PATH` name).
    pub program: OsString,
}

/// Outcome of handing a native-session directory to Akari.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRecord {
    /// What happened.
    pub status: HandoffStatus,
    /// Isolation directory passed to `akari ingest --root`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Skip or error detail. Never a credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Result of one handoff attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HandoffStatus {
    /// `akari ingest` uploaded new bytes.
    Uploaded,
    /// The server already had the file.
    Uptodate,
    /// No session files were discovered under the root, or Akari skipped them.
    Skipped,
    /// `akari ingest` failed. The review verdict is unchanged.
    Error,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    enabled: bool,
    #[serde(default)]
    program: Option<String>,
}

/// Resolve whether Akari handoff is on for this review.
///
/// `user_dir` is the user-level config directory (`None` on a `--repo`/`--pr`
/// run). A missing or malformed settings file leaves handoff off.
#[must_use]
pub fn resolve(user_dir: Option<&Path>) -> Option<AkariHandoff> {
    resolve_with(user_dir, |key| std::env::var_os(key))
}

fn resolve_with(
    user_dir: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Option<AkariHandoff> {
    match env_flag(env(ENABLED_ENV).as_deref().and_then(|v| v.to_str())) {
        Some(false) => return None,
        Some(true) => {
            return Some(AkariHandoff {
                program: program_from(None, &env),
            });
        }
        None => {}
    }
    let raw = user_dir.and_then(load_settings)?;
    if !raw.enabled {
        return None;
    }
    Some(AkariHandoff {
        program: program_from(raw.program.as_deref(), &env),
    })
}

fn env_flag(value: Option<&str>) -> Option<bool> {
    let value = value?.trim();
    match value {
        "1" | "true" | "TRUE" | "on" | "ON" | "yes" | "YES" => Some(true),
        "0" | "false" | "FALSE" | "off" | "OFF" | "no" | "NO" => Some(false),
        _ => None,
    }
}

fn program_from(settings: Option<&str>, env: impl Fn(&str) -> Option<OsString>) -> OsString {
    if let Some(path) = env(PROGRAM_ENV).filter(|v| !v.is_empty()) {
        return path;
    }
    if let Some(path) = settings.map(str::trim).filter(|v| !v.is_empty()) {
        return OsString::from(path);
    }
    OsString::from(DEFAULT_PROGRAM)
}

fn load_settings(dir: &Path) -> Option<RawSettings> {
    let path = dir.join(SETTINGS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "ignoring unreadable Akari settings"
            );
            return None;
        }
    };
    match serde_yaml_ng::from_str::<RawSettings>(&text) {
        Ok(raw) => Some(raw),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "ignoring malformed Akari settings"
            );
            None
        }
    }
}

/// Environment overlay that relocates native session files into `dir`.
///
/// Empty when the backend has no sessions-only override (Muse, and Grok whose
/// `GROK_HOME` is the whole agent home including credentials).
#[must_use]
pub fn session_env(backend: Backend, dir: &Path) -> BTreeMap<String, String> {
    let dir = dir.to_string_lossy().into_owned();
    let mut env = BTreeMap::new();
    match concrete_backend(backend) {
        Backend::Pi => {
            env.insert("PI_CODING_AGENT_SESSION_DIR".into(), dir);
        }
        Backend::ClaudeCode => {
            env.insert("CLAUDE_PROJECTS_DIR".into(), dir);
        }
        Backend::Codex => {
            env.insert("CODEX_SESSIONS_DIR".into(), dir);
        }
        Backend::Grok | Backend::Muse | Backend::Any => {}
    }
    env
}

/// Hand the isolation directory to `akari ingest --root`.
///
/// Never returns an error: a missing binary, a failed ingest, and an empty
/// directory become a [`HandoffRecord`]. The review verdict is unchanged.
pub async fn handoff<R: CommandRunner>(
    runner: &R,
    client: &AkariHandoff,
    native_dir: &Path,
    repo_root: &Path,
) -> HandoffRecord {
    match ingest_root(runner, client, native_dir, repo_root).await {
        Ok(status) => HandoffRecord {
            status,
            path: Some(native_dir.display().to_string()),
            detail: None,
        },
        Err(err) => {
            tracing::warn!(
                path = %native_dir.display(),
                error = %err,
                "Akari handoff failed; review verdict is unchanged"
            );
            HandoffRecord {
                status: HandoffStatus::Error,
                path: Some(native_dir.display().to_string()),
                detail: Some(err.to_string()),
            }
        }
    }
}

async fn ingest_root<R: CommandRunner>(
    runner: &R,
    client: &AkariHandoff,
    root: &Path,
    repo_root: &Path,
) -> Result<HandoffStatus> {
    let mut spec = CommandSpec::new(client.program.clone(), repo_root);
    spec.arg("ingest").arg("--root").arg(root.as_os_str());
    let output = runner.run(&spec).await.wrap_err("running akari ingest")?;
    if !output.success() {
        let detail = output.stderr.trim();
        let detail = if detail.is_empty() {
            output.stdout.trim()
        } else {
            detail
        };
        color_eyre::eyre::bail!(
            "akari ingest exited {}: {detail}",
            output
                .code
                .map_or_else(|| "signal".to_string(), |c| c.to_string())
        );
    }
    Ok(status_from_ingest_stdout(&output.stdout))
}

fn status_from_ingest_stdout(stdout: &str) -> HandoffStatus {
    let text = stdout.to_ascii_lowercase();
    if text.contains("0 file(s)") {
        HandoffStatus::Skipped
    } else if text.contains("uploaded") {
        HandoffStatus::Uploaded
    } else if text.contains("uptodate") || text.contains("up to date") {
        HandoffStatus::Uptodate
    } else {
        HandoffStatus::Uploaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::command::CommandOutput;
    use std::sync::{Arc, Mutex};

    #[test]
    fn resolve_defaults_to_off() {
        assert!(resolve_with(None, |_| None).is_none());
    }

    #[test]
    fn env_one_enables_without_a_settings_file() {
        let got = resolve_with(None, |key| {
            (key == ENABLED_ENV).then(|| OsString::from("1"))
        });
        assert_eq!(
            got,
            Some(AkariHandoff {
                program: OsString::from(DEFAULT_PROGRAM)
            })
        );
    }

    #[test]
    fn env_zero_disables_even_with_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(SETTINGS_FILE), "enabled: true\n").expect("write");
        let got = resolve_with(Some(dir.path()), |key| {
            (key == ENABLED_ENV).then(|| OsString::from("0"))
        });
        assert!(got.is_none());
    }

    #[test]
    fn settings_file_enables() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "enabled: true\nprogram: /opt/akari\n",
        )
        .expect("write");
        let got = resolve_with(Some(dir.path()), |_| None);
        assert_eq!(
            got,
            Some(AkariHandoff {
                program: OsString::from("/opt/akari")
            })
        );
    }

    #[test]
    fn malformed_settings_leave_handoff_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(SETTINGS_FILE), "enabled: maybe\n").expect("write");
        assert!(resolve_with(Some(dir.path()), |_| None).is_none());
    }

    #[test]
    fn program_env_wins_over_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(SETTINGS_FILE), "enabled: true\n").expect("write");
        let got = resolve_with(Some(dir.path()), |key| match key {
            ENABLED_ENV => None,
            PROGRAM_ENV => Some(OsString::from("/tmp/akari-fake")),
            _ => None,
        });
        assert_eq!(
            got.and_then(|h| h.program.into_string().ok()).as_deref(),
            Some("/tmp/akari-fake")
        );
    }

    #[test]
    fn session_env_isolates_pi_sessions_only() {
        let dir = Path::new("/tmp/native/pi");
        let env = session_env(Backend::Pi, dir);
        assert_eq!(
            env.get("PI_CODING_AGENT_SESSION_DIR").map(String::as_str),
            Some("/tmp/native/pi")
        );
        assert!(!env.contains_key("PI_CODING_AGENT_DIR"));
    }

    #[test]
    fn session_env_skips_muse_and_grok() {
        assert!(session_env(Backend::Muse, Path::new("/x")).is_empty());
        assert!(session_env(Backend::Grok, Path::new("/x")).is_empty());
    }

    struct RecordingRunner {
        specs: Arc<Mutex<Vec<CommandSpec>>>,
        output: CommandOutput,
    }

    impl CommandRunner for RecordingRunner {
        async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
            self.specs.lock().expect("specs").push(spec.clone());
            Ok(self.output.clone())
        }
    }

    #[tokio::test]
    async fn handoff_invokes_akari_ingest_with_the_isolation_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let specs = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner {
            specs: specs.clone(),
            output: CommandOutput {
                code: Some(0),
                stdout: "1 file(s): 1 uploaded, 0 reset, 0 up to date, 0 skipped, 0 failed, 0 discovery error(s) (12 bytes sent)\n".into(),
                stderr: String::new(),
            },
        };
        let client = AkariHandoff {
            program: OsString::from("akari"),
        };
        let record = handoff(&runner, &client, dir.path(), Path::new("/repo")).await;
        assert_eq!(record.status, HandoffStatus::Uploaded);
        assert_eq!(
            record.path.as_deref(),
            Some(dir.path().to_str().expect("utf8"))
        );
        let specs = specs.lock().expect("specs");
        assert_eq!(specs.len(), 1);
        let args: Vec<String> = specs[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["ingest", "--root", dir.path().to_str().expect("utf8")]
        );
    }

    #[tokio::test]
    async fn handoff_treats_an_empty_root_as_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runner = RecordingRunner {
            specs: Arc::new(Mutex::new(Vec::new())),
            output: CommandOutput {
                code: Some(0),
                stdout: "0 file(s): 0 uploaded, 0 reset, 0 up to date, 0 skipped, 0 failed, 0 discovery error(s) (0 bytes sent)\n".into(),
                stderr: String::new(),
            },
        };
        let client = AkariHandoff {
            program: OsString::from("akari"),
        };
        let record = handoff(&runner, &client, dir.path(), Path::new("/repo")).await;
        assert_eq!(record.status, HandoffStatus::Skipped);
    }

    #[tokio::test]
    async fn handoff_error_does_not_propagate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runner = RecordingRunner {
            specs: Arc::new(Mutex::new(Vec::new())),
            output: CommandOutput {
                code: Some(1),
                stdout: String::new(),
                stderr: "akari: no config\n".into(),
            },
        };
        let client = AkariHandoff {
            program: OsString::from("akari"),
        };
        let record = handoff(&runner, &client, dir.path(), Path::new("/repo")).await;
        assert_eq!(record.status, HandoffStatus::Error);
        assert!(
            record
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("no config"))
        );
    }
}
