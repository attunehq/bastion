//! Line-ending invariant for embedded text assets.
//!
//! Bastion embeds the bundled skills (`skills/<slug>/SKILL.md`) into the binary
//! with `include_str!` and compares installed copies against the embedded
//! source byte for byte (`bastion skills check`). Git's `core.autocrlf` is the
//! footgun: without an LF pin, a Windows checkout hands `include_str!` CRLF
//! source, the drift guard misfires, and the failure never appears in Linux CI.
//! Two defenses hold the invariant: the `eol=lf` pin in `.gitattributes` and
//! this test, which fails on any CR byte that reaches a checkout anyway (for
//! example when the pin is deleted or a new asset lands outside its glob).
//!
//! This check used to be an LLM reviewer (`embedded-asset-line-endings` in
//! `.bastion.yaml`); it is deterministic, so it belongs here.

// Test code: a panic is the failure report. allow-unwrap-in-tests does not
// reach an integration target's helper functions, so allow here explicitly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// Every bundled `SKILL.md` under `skills/`, the tree `src/skills.rs` embeds.
fn bundled_skill_files(manifest: &Path) -> Vec<PathBuf> {
    let skills = manifest.join("skills");
    let mut found = Vec::new();
    for entry in fs::read_dir(&skills).expect("skills/ exists at the crate root") {
        let dir = entry.expect("dir entry").path();
        if dir.is_dir() {
            let skill = dir.join("SKILL.md");
            if skill.is_file() {
                found.push(skill);
            }
        }
    }
    found
}

#[test]
fn bundled_skill_assets_are_lf_in_the_checkout() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = bundled_skill_files(manifest);
    assert!(
        !files.is_empty(),
        "no skills/*/SKILL.md found; the embedded-asset tree moved and this test must follow it"
    );

    for file in files {
        let content = fs::read(&file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        assert!(
            !content.contains(&b'\r'),
            "{} contains CR bytes in this checkout. The asset is embedded with \
             include_str! and byte-compared by `bastion skills check`, so it must \
             be LF everywhere; restore the `skills/**/SKILL.md text eol=lf` pin in \
             .gitattributes and renormalize (git add --renormalize).",
            file.display()
        );
    }
}

#[test]
fn gitattributes_pins_bundled_skills_to_lf() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let attributes = fs::read_to_string(manifest.join(".gitattributes"))
        .expect(".gitattributes exists at the repository root");
    let pinned = attributes.lines().any(|line| {
        let mut parts = line.split_whitespace();
        parts.next() == Some("skills/**/SKILL.md")
            && parts.clone().any(|a| a == "text")
            && parts.any(|a| a == "eol=lf")
    });
    assert!(
        pinned,
        ".gitattributes must keep `skills/**/SKILL.md text eol=lf`: the bundled \
         skills are embedded with include_str! and byte-compared, so a native-eol \
         checkout would break `bastion skills check` on Windows"
    );
}
