//! The attestation trust root on the GitHub side: the PR author's registered
//! SSH signing keys.
//!
//! Kept apart from [`super::context`], which produces the reviewer-facing
//! `ReviewContext`; this lookup feeds signature verification for attestation
//! replay (`docs/developer-guide/attestation.md`, "Signing"), not any
//! reviewer's prompt.

use color_eyre::eyre::Result;
use serde::Deserialize;

use super::client::{ApiRequest, GitHubApi};
use super::context::get_json;

/// Fetch the SSH signing keys `username` has registered with GitHub
/// (`GET /users/{username}/ssh_signing_keys`), as the raw key lines CI
/// assembles into an ephemeral `allowed_signers` file
/// (`docs/developer-guide/attestation.md`, "Signing"). Enrolling a key with
/// GitHub is something the coding agent under review cannot do on the
/// author's behalf, which is what makes this the trust root: a signature by
/// any other key, including one freshly minted on the author's machine, is
/// not in this list and so cannot verify.
///
/// # Errors
///
/// Returns an error if the request cannot be sent, returns a non-2xx status,
/// or returns a body that does not parse as the expected shape.
pub async fn ssh_signing_keys<A: GitHubApi + ?Sized>(
    api: &A,
    username: &str,
) -> Result<Vec<String>> {
    let keys: Vec<SshSigningKey> = get_json(api, &ssh_signing_keys_request(username)).await?;
    Ok(keys.into_iter().map(|k| k.key).collect())
}

/// `GET` a user's registered SSH signing keys.
fn ssh_signing_keys_request(username: &str) -> ApiRequest {
    ApiRequest::get(format!("/users/{username}/ssh_signing_keys"))
}

/// One entry in a `GET /users/{username}/ssh_signing_keys` response: just the
/// key line CI needs for the `allowed_signers` file.
#[derive(Debug, Deserialize)]
struct SshSigningKey {
    key: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::ApiResponse;
    use crate::github::client::test_support::RecordingClient;

    #[tokio::test]
    async fn ssh_signing_keys_parses_the_key_lines() {
        let client = RecordingClient::with_responder(|_req| ApiResponse {
            status: 200,
            body: serde_json::json!([
                { "key": "ssh-ed25519 AAAAC3Nz... ada@example.com", "id": 1 },
                { "key": "ssh-rsa AAAAB3Nz... ada-backup", "id": 2 },
            ]),
        });
        let keys = ssh_signing_keys(&client, "ada").await.expect("fetches");
        assert_eq!(
            keys,
            vec![
                "ssh-ed25519 AAAAC3Nz... ada@example.com".to_string(),
                "ssh-rsa AAAAB3Nz... ada-backup".to_string(),
            ]
        );
        let calls = client.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/users/ada/ssh_signing_keys");
    }

    #[tokio::test]
    async fn ssh_signing_keys_is_empty_for_a_user_with_none_registered() {
        let client = RecordingClient::with_responder(|_req| ApiResponse {
            status: 200,
            body: serde_json::json!([]),
        });
        let keys = ssh_signing_keys(&client, "nobody").await.expect("fetches");
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn ssh_signing_keys_fails_closed_on_a_non_2xx_response() {
        let client = RecordingClient::with_responder(|_req| ApiResponse {
            status: 404,
            body: serde_json::json!({ "message": "Not Found" }),
        });
        let err = ssh_signing_keys(&client, "ghost").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
