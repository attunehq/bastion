//! Trigger routing: selecting the reviewers whose globs match a changeset.
//!
//! Routing is shared between the local and CI surfaces: the prompt scopes a
//! reviewer's *attention*, but its `trigger` globs scope *whether it runs at
//! all*. A reviewer runs when any changed file matches any of its trigger globs.
//!
//! Triggers are stored as raw strings on [`Reviewer`]; here they are compiled
//! once into a [`Router`] (parse-don't-validate), so a malformed glob is an error
//! at compile time rather than a silent non-match at routing time.

use color_eyre::eyre::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::reviewer::Reviewer;

/// A compiled routing table over a set of reviewers.
pub struct Router<'a> {
    entries: Vec<Entry<'a>>,
}

struct Entry<'a> {
    reviewer: &'a Reviewer,
    globs: GlobSet,
}

impl<'a> Router<'a> {
    /// Compile every reviewer's trigger globs into a matcher.
    ///
    /// # Errors
    ///
    /// Returns an error naming the reviewer and pattern if any trigger glob is
    /// syntactically invalid.
    pub fn compile(reviewers: &'a [Reviewer]) -> Result<Self> {
        let mut entries = Vec::with_capacity(reviewers.len());
        for reviewer in reviewers {
            let mut builder = GlobSetBuilder::new();
            for pattern in &reviewer.trigger {
                let glob = Glob::new(pattern).wrap_err_with(|| {
                    format!(
                        "reviewer '{}' has an invalid trigger glob: {pattern}",
                        reviewer.name
                    )
                })?;
                builder.add(glob);
            }
            let globs = builder.build().wrap_err_with(|| {
                format!("building trigger matcher for reviewer '{}'", reviewer.name)
            })?;
            entries.push(Entry { reviewer, globs });
        }
        Ok(Self { entries })
    }

    /// Return the reviewers triggered by `changed`, in registry order.
    ///
    /// A reviewer is triggered when at least one changed path matches at least
    /// one of its trigger globs.
    #[must_use]
    pub fn matched<S: AsRef<str>>(&self, changed: &[S]) -> Vec<&'a Reviewer> {
        self.entries
            .iter()
            .filter(|entry| {
                changed
                    .iter()
                    .any(|path| entry.globs.is_match(path.as_ref()))
            })
            .map(|entry| entry.reviewer)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::{Config, REGISTRY_FILE};
    use crate::reviewer::Mode;

    fn reviewer(name: &str, triggers: &[&str]) -> Reviewer {
        Reviewer {
            name: name.into(),
            trigger: triggers.iter().map(|s| (*s).to_string()).collect(),
            mode: Mode::Gate,
            backend: crate::reviewer::Backend::Any,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Default::default(),
            inputs: Default::default(),
            attestation: None,
            prompt: "p".into(),
        }
    }

    #[test]
    fn routes_changed_files_to_matching_reviewers() {
        let reviewers = vec![
            reviewer("ts-files", &["src/**/*.ts"]),
            reviewer("server", &["src/server/**", "src/client/**"]),
            reviewer("docs", &["docs/**/*.md"]),
        ];
        let router = Router::compile(&reviewers).expect("compiles");

        let changed = ["src/server/db.ts", "README.md"];
        let matched: Vec<&str> = router
            .matched(&changed)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(matched, ["ts-files", "server"]);
    }

    #[test]
    fn star_star_matches_nested_and_top_level() {
        let reviewers = vec![reviewer("all-src", &["src/**"])];
        let router = Router::compile(&reviewers).unwrap();
        assert_eq!(router.matched(&["src/a/b/c.rs"]).len(), 1);
        assert_eq!(router.matched(&["src/top.rs"]).len(), 1);
        assert_eq!(router.matched(&["other/x.rs"]).len(), 0);
    }

    #[test]
    fn invalid_glob_is_reported_with_the_reviewer_name() {
        let reviewers = vec![reviewer("bad", &["src/[unclosed"])];
        let err = Router::compile(&reviewers)
            .err()
            .expect("invalid glob should fail");
        assert!(err.to_string().contains("bad"));
    }

    fn shipped_registry() -> Config {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(REGISTRY_FILE);
        Config::load(&path).expect("shipped .bastion.yaml loads")
    }

    fn matched_names<'a>(router: &Router<'a>, file: &str) -> Vec<String> {
        router
            .matched(&[file])
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    #[test]
    fn the_shipped_prose_gate_routes_on_prose_and_not_code() {
        // prose-anti-slop is a gate, so its trigger globs are load-bearing: a
        // changeset that should be reviewed for slop must actually route to it,
        // and a code-only changeset must not. A typo in those globs would silently
        // stop the gate from running, so pin both directions against the shipped
        // registry.
        let config = shipped_registry();
        let router = Router::compile(&config.reviewers).expect("triggers compile");

        for prose in [
            "site/src/components/Hero.astro",
            "site/DESIGN.md",
            "docs/user-guide/concepts.md",
            "docs/developer-guide/architecture.md",
            "skills/using-bastion/SKILL.md",
            ".github/PULL_REQUEST_TEMPLATE.md",
            "README.md",
            "CONTRIBUTING.md",
            "SECURITY.md",
            "CODE_OF_CONDUCT.md",
            "AGENTS.md",
            "CLAUDE.md",
        ] {
            let names = matched_names(&router, prose);
            assert!(
                names.iter().any(|n| n == "prose-anti-slop"),
                "prose path {prose} should route to prose-anti-slop, got {names:?}"
            );
        }

        // Code never routes to the prose gate; neither do the vendored skill trees
        // or the stop-slop skill's own reference files, which carry intentional
        // "before" slop the gate must not flag.
        for excluded in [
            "src/main.rs",
            "Cargo.toml",
            ".bastion.yaml",
            ".agents/skills/parse-dont-validate/SKILL.md",
            ".agents/skills/readme.md",
            ".claude/skills/stop-slop/references/examples.md",
        ] {
            let names = matched_names(&router, excluded);
            assert!(
                !names.iter().any(|n| n == "prose-anti-slop"),
                "excluded path {excluded} must not route to prose-anti-slop, got {names:?}"
            );
        }
    }

    #[test]
    fn the_shipped_prose_reviewer_is_governed_as_a_gate() {
        // The whole point of routing slop to this reviewer is that it can block.
        // If it were demoted to an advisor its findings would be clamped to a
        // pass, so guard the mode here.
        let config = shipped_registry();
        let reviewer = config
            .reviewers
            .iter()
            .find(|r| r.name == "prose-anti-slop")
            .expect("prose-anti-slop is in the shipped registry");
        assert_eq!(
            reviewer.mode,
            Mode::Gate,
            "prose-anti-slop is governed as a gate, so slop blocks the merge"
        );
    }
}
