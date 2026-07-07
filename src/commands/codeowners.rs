//! The `bastion github codeowners` handler.

use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::config::Config;
use crate::git;

/// `bastion github codeowners`: print a CODEOWNERS block for the reviewer-policy paths.
///
/// Covers the reviewer registry, the Bastion workflow, and the CODEOWNERS file
/// itself, so any PR touching review policy requires a human review. When a
/// registry is present, the block also covers every file it pulls in (included
/// registry files and prompt files), since those carry policy exactly like the
/// root file. `cwd` is where the registry is discovered from.
///
/// # Errors
///
/// Returns an error if a discovered registry fails to load (the block would
/// otherwise silently omit the files it pulls in), or if writing to stdout
/// fails. No registry at all is fine: the block then covers the static paths
/// only.
pub fn codeowners(cwd: &Path, owners: &[String]) -> Result<()> {
    // Outside a repository there is nothing to discover relative to; the static
    // block is still useful, so print it rather than fail.
    let extra = match git::repo_root(cwd) {
        Ok(root) => registry_policy_paths(&root)?,
        Err(_) => Vec::new(),
    };
    io::stdout()
        .write_all(crate::github::codeowners::block(owners, &extra).as_bytes())
        .wrap_err("writing CODEOWNERS block")?;
    Ok(())
}

/// The root-relative, slash-separated paths of every extra file the repository
/// registry pulls in: `include:` entries (recursively) and `prompt: {file: ...}`
/// files. Empty when no registry is discoverable from `root`. A file that
/// resolves outside `root` cannot be protected by CODEOWNERS and is skipped.
fn registry_policy_paths(root: &Path) -> Result<Vec<String>> {
    let Some(found) = crate::config::locate_kind(root)? else {
        return Ok(Vec::new());
    };
    let (_, files) = Config::load_layer(&found.path, &[])?;
    let canonical_root = std::fs::canonicalize(root)
        .wrap_err_with(|| format!("resolving the repository root {}", root.display()))?;
    let mut paths: Vec<String> = files
        .includes
        .iter()
        .chain(files.prompts.iter())
        .filter_map(|path| repo_relative(&canonical_root, path))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Express `path` relative to the canonicalized repository root with forward
/// slashes (the CODEOWNERS form), or `None` when it lies outside the root.
fn repo_relative(canonical_root: &Path, path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let relative: PathBuf = canonical.strip_prefix(canonical_root).ok()?.to_path_buf();
    let text = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_paths_cover_includes_and_prompt_files_root_relative() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("reviewers/prompts")).unwrap();
        std::fs::write(
            root.path().join(".bastion.yaml"),
            "include: [reviewers/security.yaml]\nreviewers: []\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("reviewers/security.yaml"),
            "reviewers:\n  - name: sec\n    trigger: [src/**]\n    mode: gate\n    prompt: {file: prompts/sec.md}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("reviewers/prompts/sec.md"),
            "Review for security.\n",
        )
        .unwrap();

        let paths = registry_policy_paths(root.path()).unwrap();
        assert_eq!(
            paths,
            ["reviewers/prompts/sec.md", "reviewers/security.yaml"],
            "paths are root-relative, slash-separated, and sorted"
        );
    }

    #[test]
    fn policy_paths_skip_a_file_outside_the_repository() {
        // A prompt file pulled from outside the repo cannot be protected by
        // CODEOWNERS; it is skipped rather than emitted as a bogus entry.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("prompt.md"), "Review it.\n").unwrap();
        let root = tempfile::tempdir().unwrap();
        let prompt = outside.path().join("prompt.md");
        std::fs::write(
            root.path().join(".bastion.yaml"),
            format!(
                "reviewers:\n  - name: far\n    trigger: [src/**]\n    mode: gate\n    prompt: {{file: '{}'}}\n",
                prompt.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();

        let paths = registry_policy_paths(root.path()).unwrap();
        assert!(paths.is_empty(), "got: {paths:?}");
    }

    #[test]
    fn policy_paths_are_empty_without_a_registry() {
        let root = tempfile::tempdir().unwrap();
        assert!(registry_policy_paths(root.path()).unwrap().is_empty());
    }
}
