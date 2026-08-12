//! Trigger routing: selecting reviewers whose cheap path prefilter matches.
//!
//! Routing is shared between the local and CI surfaces: the prompt scopes a
//! reviewer's *attention*. Path triggers decide whether it runs at all. Agent
//! triggers use their optional paths only as a cheap prefilter; the runner makes
//! the semantic decision over the actual changeset.
//!
//! Trigger paths are stored as ordered strings on [`Reviewer`]. A leading `!`
//! excludes a path, and the last matching pattern wins. [`TriggerMatcher`]
//! compiles that policy once for routing and is also shared with carry scoping.

use color_eyre::eyre::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::reviewer::{Reviewer, Trigger};

/// A compiled routing table over a set of reviewers.
pub struct Router<'a> {
    entries: Vec<Entry<'a>>,
}

struct Entry<'a> {
    reviewer: &'a Reviewer,
    matcher: TriggerMatcher,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternEffect {
    Include,
    Exclude,
}

/// A compiled ordered trigger matcher.
///
/// Every pattern keeps its position in the [`GlobSet`]. Matching indices are
/// therefore enough to apply the effect of the last matching pattern without
/// changing Bastion's existing glob grammar.
pub(crate) struct TriggerMatcher {
    globs: GlobSet,
    effects: Vec<PatternEffect>,
}

impl TriggerMatcher {
    /// Compile one reviewer's path trigger or agent path prefilter.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty path-only trigger, a bare exclusion, a
    /// non-empty list with no positive pattern, or a malformed glob.
    pub(crate) fn compile(reviewer: &Reviewer) -> Result<Self> {
        let patterns = reviewer.trigger.paths();
        let agent_without_paths = reviewer.trigger.agent().is_some() && patterns.is_empty();
        if patterns.is_empty() && !agent_without_paths {
            color_eyre::eyre::bail!(
                "reviewer '{}' has an empty path trigger; add at least one positive glob",
                reviewer.name
            );
        }

        let mut builder = GlobSetBuilder::new();
        let mut effects = Vec::with_capacity(patterns.len());
        for raw in patterns {
            let (effect, pattern) = match raw.strip_prefix('!') {
                Some("") => color_eyre::eyre::bail!(
                    "reviewer '{}' has a bare `!` trigger pattern; add a glob after `!`",
                    reviewer.name
                ),
                Some(pattern) => (PatternEffect::Exclude, pattern),
                None => (PatternEffect::Include, raw.as_str()),
            };
            let glob = Glob::new(pattern).wrap_err_with(|| {
                format!(
                    "reviewer '{}' has an invalid trigger glob: {raw}",
                    reviewer.name
                )
            })?;
            builder.add(glob);
            effects.push(effect);
        }

        if !patterns.is_empty() && !effects.contains(&PatternEffect::Include) {
            color_eyre::eyre::bail!(
                "reviewer '{}' trigger must contain at least one positive glob",
                reviewer.name
            );
        }

        let globs = builder.build().wrap_err_with(|| {
            format!("building trigger matcher for reviewer '{}'", reviewer.name)
        })?;
        Ok(Self { globs, effects })
    }

    /// Return whether `path` is included after applying the last matching rule.
    #[must_use]
    pub(crate) fn is_match(&self, path: &str) -> bool {
        self.globs
            .matches(path)
            .last()
            .is_some_and(|index| self.effects[*index] == PatternEffect::Include)
    }
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
            let matcher = TriggerMatcher::compile(reviewer)?;
            entries.push(Entry { reviewer, matcher });
        }
        Ok(Self { entries })
    }

    /// Return the reviewers triggered by `changed`, in registry order.
    ///
    /// A path trigger is a candidate when one changed path is included by its
    /// ordered patterns. Agent-trigger paths use the same rule; without paths,
    /// the agent is a candidate for every non-empty changeset.
    #[must_use]
    pub fn matched<S: AsRef<str>>(&self, changed: &[S]) -> Vec<&'a Reviewer> {
        self.entries
            .iter()
            .filter(|entry| {
                !changed.is_empty()
                    && (matches!(
                        &entry.reviewer.trigger,
                        Trigger::Agent(agent) if agent.paths.is_empty()
                    ) || changed
                        .iter()
                        .any(|path| entry.matcher.is_match(path.as_ref())))
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
    use crate::reviewer::{Mode, Trigger};

    fn reviewer(name: &str, triggers: &[&str]) -> Reviewer {
        Reviewer {
            name: name.into(),
            trigger: Trigger::Paths(triggers.iter().map(|s| (*s).to_string()).collect()),
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

    #[test]
    fn an_empty_path_trigger_is_rejected() {
        let reviewers = [reviewer("empty", &[])];
        let error = Router::compile(&reviewers).err().expect("must fail");
        assert!(error.to_string().contains("empty path trigger"));
    }

    #[test]
    fn ordered_patterns_exclude_and_reinclude_paths() {
        let reviewers = [reviewer(
            "docs",
            &[
                "docs/**/*.md",
                "!docs/audit-reports/**",
                "docs/audit-reports/current.md",
            ],
        )];
        let router = Router::compile(&reviewers).unwrap();

        assert_eq!(router.matched(&["docs/guide.md"]).len(), 1);
        assert!(router.matched(&["docs/audit-reports/old.md"]).is_empty());
        assert_eq!(router.matched(&["docs/audit-reports/current.md"]).len(), 1);
    }

    #[test]
    fn later_pattern_wins_and_another_changed_path_can_trigger() {
        let excluded_last = [reviewer("docs", &["docs/**", "!docs/private/**"])];
        let included_last = [reviewer("docs", &["!docs/private/**", "docs/**"])];
        let excluded_router = Router::compile(&excluded_last).unwrap();
        let included_router = Router::compile(&included_last).unwrap();

        assert!(
            excluded_router
                .matched(&["docs/private/secret.md"])
                .is_empty()
        );
        assert_eq!(
            included_router.matched(&["docs/private/secret.md"]).len(),
            1
        );
        assert_eq!(
            excluded_router
                .matched(&["docs/private/secret.md", "docs/public.md"])
                .len(),
            1
        );
    }

    #[test]
    fn character_class_negation_remains_glob_syntax() {
        let reviewers = [reviewer("not-a", &["docs/[!a]*/**"])];
        let router = Router::compile(&reviewers).unwrap();
        assert_eq!(router.matched(&["docs/foo/x.md"]).len(), 1);
        assert!(router.matched(&["docs/audit/x.md"]).is_empty());
    }

    #[test]
    fn agent_trigger_without_paths_is_a_candidate_for_any_non_empty_changeset() {
        let mut semantic = reviewer("semantic", &[]);
        semantic.trigger = Trigger::Agent(crate::reviewer::AgentTrigger {
            kind: crate::reviewer::AgentTriggerKind::Agent,
            prompt: "decide".into(),
            backend: crate::reviewer::Backend::Codex,
            model: None,
            effort: None,
            timeout: None,
            paths: vec![],
        });
        let reviewers = [semantic];
        let router = Router::compile(&reviewers).expect("compiles");
        assert!(router.matched::<&str>(&[]).is_empty());
        assert_eq!(router.matched(&["README.md"]).len(), 1);
    }

    #[test]
    fn agent_trigger_paths_are_a_cheap_and_prefilter() {
        let mut semantic = reviewer("semantic", &[]);
        semantic.trigger = Trigger::Agent(crate::reviewer::AgentTrigger {
            kind: crate::reviewer::AgentTriggerKind::Agent,
            prompt: "decide".into(),
            backend: crate::reviewer::Backend::Codex,
            model: None,
            effort: None,
            timeout: None,
            paths: vec!["src/**/*.rs".into()],
        });
        let reviewers = [semantic];
        let router = Router::compile(&reviewers).expect("compiles");
        assert!(router.matched(&["docs/guide.md"]).is_empty());
        assert_eq!(router.matched(&["src/lib.rs"]).len(), 1);
    }

    #[test]
    fn agent_trigger_paths_support_exclusion() {
        let mut semantic = reviewer("semantic", &[]);
        semantic.trigger = Trigger::Agent(crate::reviewer::AgentTrigger {
            kind: crate::reviewer::AgentTriggerKind::Agent,
            prompt: "decide".into(),
            backend: crate::reviewer::Backend::Codex,
            model: None,
            effort: None,
            timeout: None,
            paths: vec!["src/**".into(), "!src/generated/**".into()],
        });
        let reviewers = [semantic];
        let router = Router::compile(&reviewers).unwrap();
        assert_eq!(router.matched(&["src/lib.rs"]).len(), 1);
        assert!(router.matched(&["src/generated/schema.rs"]).is_empty());
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
