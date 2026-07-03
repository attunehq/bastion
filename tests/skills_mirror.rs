//! Mirror invariant: `.agents/skills/` and `.claude/skills/` are byte-identical
//! trees.
//!
//! Skills must be available through both surfaces an agent might read: the
//! agent-neutral convention (`.agents/skills/`) and Claude Code's native skill
//! path (`.claude/skills/`). Rather than symlink (awkward on Windows and in git),
//! the two directories are kept as exact copies. A change to a skill under one
//! path must be copied to the other; this test fails closed until they match.
//!
//! It is a deterministic, content-only check (no agent, no network), so it runs
//! in CI as part of `cargo test`. Line endings are normalized (CR stripped) so
//! the comparison holds identically on LF and CRLF checkouts.

// Test code: a panic is the failure report. allow-unwrap-in-tests does not
// reach an integration target's helper functions, so allow here explicitly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const AGENTS: &str = ".agents/skills";
const CLAUDE: &str = ".claude/skills";

/// Map every file under `root` to its content, keyed by a `/`-joined relative
/// path with carriage returns stripped.
fn collect(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("dir entry");
            let name = entry.file_name();
            // OS junk is never committed; ignore it so a stray local file does
            // not fail an otherwise-clean mirror.
            if name == ".DS_Store" || name == "Thumbs.db" {
                continue;
            }
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .expect("path is under root")
                .to_string_lossy()
                .replace('\\', "/");
            let content =
                fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let normalized = content.into_iter().filter(|b| *b != b'\r').collect();
            files.insert(rel, normalized);
        }
    }
    files
}

#[test]
fn agents_and_claude_skill_trees_are_mirrors() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let agents = collect(&manifest.join(AGENTS));
    let claude = collect(&manifest.join(CLAUDE));

    // A wrong root would otherwise make an empty-vs-empty comparison pass.
    assert!(!agents.is_empty(), "{AGENTS} contains no files");
    assert!(!claude.is_empty(), "{CLAUDE} contains no files");

    let only_agents: Vec<&String> = agents.keys().filter(|k| !claude.contains_key(*k)).collect();
    let only_claude: Vec<&String> = claude.keys().filter(|k| !agents.contains_key(*k)).collect();
    assert!(
        only_agents.is_empty() && only_claude.is_empty(),
        "skill trees differ in which files exist.\n  \
         only under {AGENTS}: {only_agents:?}\n  \
         only under {CLAUDE}: {only_claude:?}\n\
         The two trees must be exact copies; copy the missing files across."
    );

    let differing: Vec<&String> = agents
        .iter()
        .filter(|(rel, content)| claude.get(*rel).is_some_and(|other| other != *content))
        .map(|(rel, _)| rel)
        .collect();
    assert!(
        differing.is_empty(),
        "these skill files differ between {AGENTS} and {CLAUDE}: {differing:?}\n\
         The two trees must be byte-identical (ignoring line endings); copy the change across."
    );
}
