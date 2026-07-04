//! The `bastion validate` handler.

use crate::config::Config;
use crate::git;
use crate::reviewer::Mode;
use crate::reviewer::ModelId;
use color_eyre::eyre::Result;
use std::io;
use std::io::Write;
use std::path::Path;

/// `bastion validate`: parse the reviewer registry and report any problems.
///
/// Loads the registry (the explicit `file`, or the merged set discovered by
/// walking up from `cwd` and layering in the user-level config dir) through the
/// same [`Config`] path `bastion review` uses, so it surfaces exactly the errors a
/// real review would hit at load time: malformed YAML, an unknown field, a
/// duplicate reviewer name, or a model pinned under `backend: any`. On success it
/// prints a one-line summary and the parsed reviewers and returns `Ok`; on any
/// problem it returns the error, which `color_eyre` renders before the process
/// exits non-zero, so the command doubles as a CI lint and a cheap local check that
/// never spends a model call.
///
/// `user_dir` is the user-level config directory layered into discovery (`None` to
/// skip it). An explicit `file` is validated on its own, with no layering, since it
/// is a deliberate single-file check.
///
/// # Errors
///
/// Returns an error if no registry is found, the file cannot be read, or it fails
/// to parse or validate.
pub fn validate(cwd: &Path, file: Option<&Path>, user_dir: Option<&Path>) -> Result<()> {
    let (label, config) = match file {
        Some(file) => (file.display().to_string(), Config::load(file)?),
        None => {
            // Resolve from the repo root when we are inside one (so the command
            // works from any subdirectory, like `review`), falling back to `cwd`
            // when git cannot tell us, which keeps a not-yet-initialized repo
            // working. `discover_merged_located` warns on the deprecated location,
            // gives the clear "no registry found" error, and hands back the sources
            // it loaded, so the summary reports exactly the files that were merged.
            let root = git::repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let (sources, config) = Config::discover_merged_located(&root, user_dir)?;
            (describe_sources(&sources), config)
        }
    };

    let gates = config
        .reviewers
        .iter()
        .filter(|r| r.mode == Mode::Gate)
        .count();
    let advisors = config.reviewers.len() - gates;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{label} is valid: {} reviewer(s), {gates} gate(s), {advisors} advisor(s).",
        config.reviewers.len(),
    )?;
    for reviewer in &config.reviewers {
        let model = reviewer.model.as_ref().map_or("default", ModelId::as_str);
        writeln!(
            out,
            "  - {} ({}, backend: {}, model: {model})",
            reviewer.name,
            reviewer.mode.as_str(),
            reviewer.backend.as_str(),
        )?;
    }
    Ok(())
}

/// Describe the registry [`Sources`] that fed a merged config, for the `validate`
/// summary line. A single source reads as its own path (so the common case matches
/// the pre-merge wording); both sources name each file so it is clear what was
/// merged. At least one is always present, since discovery errors otherwise.
fn describe_sources(sources: &crate::config::Sources) -> String {
    match (&sources.repo, &sources.user) {
        (Some(repo), Some(user)) => format!(
            "the merged registry (repo: {}, user: {})",
            repo.path.display(),
            user.display()
        ),
        (Some(repo), None) => repo.path.display().to_string(),
        (None, Some(user)) => user.display().to_string(),
        (None, None) => unreachable!("discover_merged_located errors when both sources are absent"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_a_well_formed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: a\n    trigger: [src/**]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        validate(tmp.path(), Some(&path), None).expect("a well-formed file validates");
    }

    #[test]
    fn validate_reports_a_duplicate_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: dup\n    trigger: [a]\n    mode: gate\n    prompt: p\n  - name: dup\n    trigger: [b]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        let err = validate(tmp.path(), Some(&path), None).unwrap_err();
        assert!(
            format!("{err:#}").contains("duplicate reviewer name"),
            "error should name the duplicate, got: {err:#}"
        );
    }

    #[test]
    fn validate_reports_an_unknown_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: typo\n    trigger: [src/**]\n    mode: gate\n    bakend: codex\n    prompt: p\n",
        )
        .unwrap();
        let err = validate(tmp.path(), Some(&path), None).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown field `bakend`"),
            "validate should reject an unknown field, got: {err:#}"
        );
    }

    #[test]
    fn validate_reports_a_model_under_backend_any() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: stray\n    trigger: [src/**]\n    mode: gate\n    model: gpt-5\n    prompt: p\n",
        )
        .unwrap();
        let err = validate(tmp.path(), Some(&path), None).unwrap_err();
        assert!(format!("{err:#}").contains("backend: any"), "got: {err:#}");
    }

    #[test]
    fn validate_discovers_from_the_directory_when_no_file_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".bastion.yaml"),
            "reviewers:\n  - name: a\n    trigger: [x]\n    mode: advisor\n    prompt: p\n",
        )
        .unwrap();
        validate(tmp.path(), None, None).expect("discovered registry validates");
    }

    #[test]
    fn validate_errors_clearly_when_no_registry_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = validate(tmp.path(), None, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("no reviewer registry found"),
            "got: {err:#}"
        );
    }
}
