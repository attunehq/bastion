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
    fn the_shipped_safety_gate_routes_on_the_trust_boundary() {
        // fail-closed-gates is the safety gate, so its trigger globs are
        // load-bearing: a change to the live-decision path must actually route
        // to it. A stale glob fails silently; the original trigger listed
        // `src/runner.rs`, and when the runner became the `src/runner/`
        // directory the gate quietly stopped reviewing runner changes. Pin the
        // globs against the real module paths so a module split cannot detach
        // the gate again.
        let config = shipped_registry();
        let router = Router::compile(&config.reviewers).expect("triggers compile");

        for boundary in [
            "src/runner/mod.rs",
            "src/runner/verdicts.rs",
            "src/runner/persist.rs",
            "src/backend/mod.rs",
            "src/backend/codex.rs",
            "src/backend/container/mod.rs",
            "src/verdict.rs",
            "src/commands/review.rs",
            "src/carry.rs",
            "src/seal.rs",
            "src/attest/replay.rs",
        ] {
            let names = matched_names(&router, boundary);
            assert!(
                names.iter().any(|n| n == "fail-closed-gates"),
                "trust-boundary path {boundary} should route to fail-closed-gates, got {names:?}"
            );
        }

        // A change that cannot violate the invariant must not pay for the
        // reviewer: docs, the site, and src modules off the live-decision path.
        for excluded in [
            "src/cli.rs",
            "src/render.rs",
            "src/github/mod.rs",
            "docs/user-guide/concepts.md",
            "site/src/components/Hero.astro",
            "README.md",
        ] {
            let names = matched_names(&router, excluded);
            assert!(
                !names.iter().any(|n| n == "fail-closed-gates"),
                "excluded path {excluded} must not route to fail-closed-gates, got {names:?}"
            );
        }
    }

    #[test]
    fn the_shipped_docs_gate_routes_on_the_user_surface() {
        // user-docs-in-sync triggers on the user-visible surface plus the guide
        // itself, and must not fire on internal modules a user cannot observe.
        let config = shipped_registry();
        let router = Router::compile(&config.reviewers).expect("triggers compile");

        for surface in [
            "src/cli.rs",
            "src/commands/review.rs",
            "src/config.rs",
            "src/reviewer.rs",
            "src/verdict.rs",
            "src/event.rs",
            "docs/user-guide/concepts.md",
        ] {
            let names = matched_names(&router, surface);
            assert!(
                names.iter().any(|n| n == "user-docs-in-sync"),
                "user-surface path {surface} should route to user-docs-in-sync, got {names:?}"
            );
        }

        for excluded in [
            "src/runner/mod.rs",
            "src/backend/codex.rs",
            "docs/developer-guide/architecture.md",
            "README.md",
        ] {
            let names = matched_names(&router, excluded);
            assert!(
                !names.iter().any(|n| n == "user-docs-in-sync"),
                "excluded path {excluded} must not route to user-docs-in-sync, got {names:?}"
            );
        }
    }

    #[test]
    fn the_shipped_reviewers_are_governed_as_gates() {
        // Both shipped reviewers exist to block: the safety gate on a
        // fail-closed violation, the docs gate on user-guide drift. Demoted to
        // an advisor either would have its findings clamped to a pass, so
        // guard the mode here.
        let config = shipped_registry();
        for name in ["fail-closed-gates", "user-docs-in-sync"] {
            let reviewer = config
                .reviewers
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("{name} is in the shipped registry"));
            assert_eq!(
                reviewer.mode,
                Mode::Gate,
                "{name} is governed as a gate, so its findings block the merge"
            );
        }
    }
}
