//! Structural invariants of the user guide under `docs/user-guide`.
//!
//! The guide is single-sourced: the site build (site/src/content.config.ts)
//! routes and sorts chapters by their frontmatter and rewrites relative `.md`
//! links to `/guide/*` routes (links that leave the guide become GitHub URLs).
//! A change that reads fine on GitHub can therefore 404 or vanish on the
//! published site. These checks are deterministic (no agent, no network), so
//! they run in `cargo test` instead of costing a review pass:
//!
//! - every chapter carries `title` (string), `summary` (string), and `order`
//!   (number) frontmatter, or the site build drops it;
//! - no two chapters share an `order` (the sidebar sorts by it);
//! - every in-guide relative link resolves to a real chapter, and its
//!   `#anchor` (when present) to a real heading in that chapter;
//! - every relative link that leaves the guide resolves to a real repository
//!   path (the build turns these into GitHub links).

// Test code: a panic is the failure report. allow-unwrap-in-tests does not
// reach an integration target's helper functions, so allow here explicitly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const GUIDE: &str = "docs/user-guide";

#[derive(Deserialize)]
struct Frontmatter {
    #[expect(dead_code, reason = "presence and type are the invariant")]
    title: String,
    #[expect(dead_code, reason = "presence and type are the invariant")]
    summary: String,
    order: f64,
}

struct Chapter {
    /// File name within the guide directory (`concepts.md`).
    name: String,
    frontmatter: Frontmatter,
    /// Markdown body with fenced code blocks blanked out, so link and heading
    /// scans never match CLI examples or YAML snippets.
    prose: String,
}

/// Blank every line inside a fenced code block (``` or ~~~), preserving line
/// count so nothing shifts.
fn strip_code_fences(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if !in_fence {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Split a chapter file into its YAML frontmatter block and the body after it.
fn split_frontmatter(name: &str, content: &str) -> (String, String) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---")
        .unwrap_or_else(|| panic!("{name}: missing frontmatter opening `---`"));
    let end = rest
        .find("\n---")
        .unwrap_or_else(|| panic!("{name}: unterminated frontmatter block"));
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['-']).to_string();
    (yaml.to_string(), body)
}

fn load_chapters(guide_dir: &Path) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    for entry in fs::read_dir(guide_dir).expect("reading docs/user-guide") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let name = path
            .file_name()
            .expect("md file has a name")
            .to_string_lossy()
            .into_owned();
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
            .replace("\r\n", "\n");
        let (yaml, body) = split_frontmatter(&name, &content);
        let frontmatter: Frontmatter = serde_yaml_ng::from_str(&yaml).unwrap_or_else(|e| {
            panic!("{name}: frontmatter must carry title (string), summary (string), and order (number): {e}")
        });
        chapters.push(Chapter {
            name,
            frontmatter,
            prose: strip_code_fences(&body),
        });
    }
    assert!(!chapters.is_empty(), "{GUIDE} contains no chapters");
    chapters
}

/// GitHub-style anchor slug for a heading: lowercase, spaces to hyphens,
/// everything else alphanumeric-or-hyphen only.
fn slugify(heading: &str) -> String {
    heading
        .trim()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == ' ' || c == '-' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn heading_slugs(prose: &str) -> Vec<String> {
    let mut slugs = Vec::new();
    for line in prose.lines() {
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let text = rest.trim_start_matches('#').trim();
        if !text.is_empty() {
            let base = slugify(text);
            // GitHub disambiguates duplicate headings with -1, -2, ...
            let dupes = slugs.iter().filter(|s| **s == base).count();
            if dupes > 0 {
                slugs.push(format!("{base}-{dupes}"));
            } else {
                slugs.push(base);
            }
        }
    }
    slugs
}

/// Every `](target)` destination in the prose.
fn link_targets(prose: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = prose.as_bytes();
    let mut i = 0;
    while let Some(open) = prose[i..].find("](") {
        let start = i + open + 2;
        let Some(close) = prose[start..].find(')') else {
            break;
        };
        let target = prose[start..start + close].trim();
        // A `[text](url "title")` form keeps only the url.
        let target = target.split_whitespace().next().unwrap_or("");
        if !target.is_empty() {
            targets.push(target.to_string());
        }
        i = start + close + 1;
        if i >= bytes.len() {
            break;
        }
    }
    targets
}

#[test]
fn chapters_have_unique_orders() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let chapters = load_chapters(&manifest.join(GUIDE));

    let mut by_order: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for chapter in &chapters {
        by_order
            .entry(format!("{}", chapter.frontmatter.order))
            .or_default()
            .push(&chapter.name);
    }
    let collisions: Vec<_> = by_order
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .collect();
    assert!(
        collisions.is_empty(),
        "chapters share an `order` value, which makes the site's navigation \
         order undefined: {collisions:?}"
    );
}

#[test]
fn relative_links_resolve() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let guide_dir = manifest.join(GUIDE);
    let chapters = load_chapters(&guide_dir);
    let slugs_by_name: BTreeMap<&str, Vec<String>> = chapters
        .iter()
        .map(|c| (c.name.as_str(), heading_slugs(&c.prose)))
        .collect();

    let mut broken = Vec::new();
    for chapter in &chapters {
        for target in link_targets(&chapter.prose) {
            if target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            let (path_part, anchor) = match target.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (target.as_str(), None),
            };
            // A bare `#anchor` points into the current chapter.
            let resolved: PathBuf = if path_part.is_empty() {
                guide_dir.join(&chapter.name)
            } else {
                guide_dir.join(path_part)
            };
            let Ok(resolved) = normalize(&resolved) else {
                broken.push(format!(
                    "{}: {target} (escapes the repository)",
                    chapter.name
                ));
                continue;
            };
            if !resolved.exists() {
                broken.push(format!("{}: {target} (no such path)", chapter.name));
                continue;
            }
            // Anchors are only checkable for in-guide chapters; a link that
            // leaves the guide becomes a GitHub URL where existence of the
            // path is the strongest deterministic claim.
            if let Some(anchor) = anchor
                && resolved.parent() == Some(guide_dir.as_path())
                && resolved.extension().is_some_and(|e| e == "md")
            {
                let target_name = resolved
                    .file_name()
                    .expect("md file has a name")
                    .to_string_lossy()
                    .into_owned();
                let known = slugs_by_name
                    .get(target_name.as_str())
                    .is_some_and(|slugs| slugs.iter().any(|s| s == anchor));
                if !known {
                    broken.push(format!(
                        "{}: {target} (no heading with anchor `#{anchor}` in {target_name})",
                        chapter.name
                    ));
                }
            }
        }
    }
    assert!(
        broken.is_empty(),
        "broken relative links in the user guide (these 404 once the site \
         build rewrites them):\n  {}",
        broken.join("\n  ")
    );
}

/// Lexically resolve `..` and `.` segments without touching the filesystem, so
/// a dangling target still yields the path we report as missing.
fn normalize(path: &Path) -> Result<PathBuf, ()> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return Err(());
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    Ok(out)
}
