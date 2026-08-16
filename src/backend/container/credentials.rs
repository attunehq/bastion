use std::ffi::OsString;

/// Provider credential variable names forwarded into a container by name, so the
/// in-container agent can reach its model provider. Only names actually present in
/// Bastion's environment are forwarded, and they are passed by name (the engine
/// reads the value), so Bastion never copies a secret through its own argv.
const CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CODEX_API_KEY",
    "XAI_API_KEY",
];

/// The provider credential variable names present (and non-empty) in Bastion's
/// environment, to be forwarded into a container.
///
/// An empty value is treated as absent: forwarding `-e NAME` for an empty `NAME=`
/// would set an empty variable in the container, masking auth an image may have baked
/// in under that name.
#[must_use]
pub fn credential_passthrough() -> Vec<String> {
    credentials_present(|name| is_present(std::env::var_os(name)))
}

/// Whether a looked-up environment value counts as present: set and non-empty.
///
/// An empty value is treated as absent so a blank `NAME=` is not forwarded (see
/// [`credential_passthrough`]). Split out from the environment lookup so the rule is
/// testable without mutating the process environment.
fn is_present(value: Option<OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// The subset of [`CREDENTIAL_VARS`] for which `present` reports a value, in order.
///
/// The environment lookup is injected so this is testable without mutating the
/// process environment (which is unsound under the parallel test harness).
fn credentials_present(present: impl Fn(&str) -> bool) -> Vec<String> {
    CREDENTIAL_VARS
        .iter()
        .filter(|name| present(name))
        .map(|name| (*name).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_present_forwards_only_present_vars_in_order() {
        // Inject the lookup so the test never mutates the process environment (which
        // is unsound under the parallel test harness). Only `ANTHROPIC_API_KEY` and
        // `OPENAI_BASE_URL` are "present".
        let present = |name: &str| matches!(name, "ANTHROPIC_API_KEY" | "OPENAI_BASE_URL");
        let forwarded = credentials_present(present);
        assert_eq!(forwarded, vec!["ANTHROPIC_API_KEY", "OPENAI_BASE_URL"]);
        // A var that is not "present" is not forwarded; nor is an unknown name.
        assert!(!forwarded.iter().any(|v| v == "OPENAI_API_KEY"));
        assert!(credentials_present(|name| name == "NOT_A_CREDENTIAL").is_empty());
    }

    #[test]
    fn the_forwarded_allowlist_is_exactly_the_provider_credentials() {
        // Pin the full set so dropping (or silently renaming) a provider variable is
        // caught here. These names are user-visible behavior and documented in the
        // user guide; the two must stay in lockstep.
        let all = credentials_present(|_| true);
        assert_eq!(
            all,
            vec![
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_MODEL",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "OPENAI_API_KEY",
                "OPENAI_BASE_URL",
                "CODEX_API_KEY",
                "XAI_API_KEY",
            ]
        );
    }

    #[test]
    fn an_empty_credential_is_treated_as_absent() {
        // A set-but-empty credential must not be forwarded: an empty `-e NAME` would
        // shadow auth an image may have baked in under that name.
        assert!(is_present(Some(OsString::from("sk-live-123"))));
        assert!(!is_present(Some(OsString::new())));
        assert!(!is_present(None));
    }
}
