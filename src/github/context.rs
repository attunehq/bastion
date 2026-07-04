//! Gathering a pull request's context for the reviewers, from the GitHub side.
//!
//! This is the GitHub *producer* for the transport-neutral
//! [`ReviewContext`](crate::context::ReviewContext): it reads
//! a PR's description and its discussion over the same REST seam the reporting half
//! uses, and maps them onto the generic shape the runner and backends consume. It
//! converts each GitHub-specific field here so none reaches the core:
//!
//! - a non-empty PR `body` becomes the [`ReviewContext::intent`](crate::context::ReviewContext::intent)
//!   (an empty body supplies none, so the local commit-message intent stands);
//! - each human comment becomes a [`ContextComment`], with GitHub's `author_association`
//!   mapped onto the generic [`Standing`] so a reviewer can weight a maintainer's word
//!   above an outsider's;
//! - Bastion's own past comments are filtered out by their hidden marker, so a reviewer
//!   never reacts to a paraphrase of itself;
//! - a review-comment reply whose thread root is a Bastion finding (carrying a finding
//!   marker) is routed back to that [`FindingId`], so the reply reaches the reviewer that
//!   raised it.
//!
//! The prior-findings half of a [`ReviewContext`](crate::context::ReviewContext) is recalled from the local run store
//! (`crate::store::prior_findings`), the same way regardless of transport, and merged in
//! by the caller; this module supplies only the intent and the discussion.
//!
//! Comments are authored by the gate's subject and by bystanders. The mapping preserves
//! who said what (for weighting) but grants no authority; see the framing in
//! [`crate::context`].

use std::num::NonZeroU64;

use color_eyre::eyre::{Context, Result};
use serde::Deserialize;

use crate::context::{ContextComment, FindingId, Standing};

use super::client::{ApiRequest, GitHubApi, send_checked};

/// The hidden marker carrying a finding's [`FindingId`] on a comment. A reply whose
/// thread root carries this marker resolves back to that [`FindingId`] and reaches the
/// reviewer that raised the finding. The reporter posts one sticky comment and check
/// runs, so PR comments arrive as general discussion.
const FINDING_MARKER_PREFIX: &str = "<!-- bastion-finding:";

/// Any comment whose body carries a `<!-- bastion` marker is Bastion's own (the sticky
/// report comment or a per-finding comment), excluded so a reviewer never ingests its
/// own past output as if it were human discussion.
const BASTION_MARKER_PREFIX: &str = "<!-- bastion";

/// The intent and discussion gathered for a pull request, ready to merge into a
/// [`ReviewContext`](crate::context::ReviewContext).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatheredContext {
    /// The PR description, as the author's stated intent. `None` for an empty body.
    pub intent: Option<String>,
    /// The human discussion: top-level PR comments and inline review comments, with
    /// Bastion's own comments removed.
    pub comments: Vec<ContextComment>,
    /// The PR author's GitHub login, when the response carried one. This is the
    /// attestation signature's principal (`docs/developer-guide/attestation.md`,
    /// "Signing") and the key for the signing-key lookup
    /// ([`super::signing::ssh_signing_keys`]); a login the API omits (a deleted account) leaves
    /// attestation with nothing to verify against, so the caller falls back.
    pub author_login: Option<String>,
    /// The PR's head commit SHA, when the response carried one. CI's checkout can
    /// be a merge commit while the attestation note hangs off this commit, so a
    /// caller retries the note lookup here when `HEAD` carries none.
    pub head_sha: Option<String>,
}

/// Gather a pull request's intent and discussion over `api`.
///
/// Reads the PR body, its issue (conversation) comments, and its review (inline)
/// comments, filters out Bastion's own, and normalizes the rest. Best effort by
/// contract: the caller treats any error as "no GitHub context" and proceeds with the
/// local context alone, so a flaky API never fails a review.
///
/// # Errors
///
/// Returns an error if a request cannot be sent, returns a non-2xx status, or returns a
/// body that does not parse as the expected shape.
pub async fn gather<A: GitHubApi + ?Sized>(
    api: &A,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<GatheredContext> {
    // The three reads are independent, so drive them concurrently: one round-trip's
    // latency instead of three in series. try_join! short-circuits on the first error,
    // preserving the fail-on-any-error contract. The requests are bound first so they
    // outlive the borrows the join holds across its awaits.
    let pull_req = pull_request_request(owner, repo, pr);
    let issue_req = issue_comments_request(owner, repo, pr);
    let review_req = review_comments_request(owner, repo, pr);
    let (pull, issue_comments, review_comments): (PullRequest, Vec<RawComment>, Vec<RawComment>) =
        tokio::try_join!(
            get_json(api, &pull_req),
            get_json(api, &issue_req),
            get_json(api, &review_req),
        )?;

    let intent = pull
        .body
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty());
    let author_login = pull.user.and_then(|u| u.login);
    let head_sha = pull.head.map(|h| h.sha);

    let mut comments = Vec::new();

    // Top-level conversation comments never thread to a specific finding.
    for raw in &issue_comments {
        if let Some(comment) = raw.to_context(None) {
            comments.push(comment);
        }
    }

    // Inline review comments can thread: GitHub's `in_reply_to_id` points at the
    // thread's root comment. When that root is a Bastion finding comment, the reply is
    // routed back to the finding it answers. Build the id->body map over *all* review
    // comments (Bastion's included) so a reply onto a Bastion root resolves.
    let roots: std::collections::HashMap<CommentId, &str> = review_comments
        .iter()
        .map(|raw| (raw.id, raw.body.as_str()))
        .collect();
    for raw in &review_comments {
        let routed = raw
            .in_reply_to_id
            .and_then(|root_id| roots.get(&root_id))
            .and_then(|root_body| finding_marker(root_body));
        if let Some(comment) = raw.to_context(routed) {
            comments.push(comment);
        }
    }

    Ok(GatheredContext {
        intent,
        comments,
        author_login,
        head_sha,
    })
}

/// Map GitHub's `author_association` onto the generic [`Standing`].
///
/// `OWNER` governs the repo; `MEMBER`/`COLLABORATOR` have write access; `CONTRIBUTOR`
/// has merged before but holds none; everything else (`NONE`, `FIRST_TIME_CONTRIBUTOR`,
/// an unknown value) has no established standing. Mapping rather than carrying the raw
/// string keeps the GitHub vocabulary out of the core.
fn standing_from_association(association: Option<&str>) -> Standing {
    match association {
        Some("OWNER") => Standing::Owner,
        Some("MEMBER" | "COLLABORATOR") => Standing::Member,
        Some("CONTRIBUTOR") => Standing::Contributor,
        _ => Standing::Outsider,
    }
}

/// Whether a comment body is Bastion's own (the sticky report or a per-finding marker),
/// which must be excluded from the discussion so a reviewer never reads itself.
fn is_bastion_comment(body: &str) -> bool {
    body.contains(BASTION_MARKER_PREFIX)
}

/// Extract the [`FindingId`] from a Bastion finding comment's body, if present. The
/// marker is `<!-- bastion-finding:HEX -->`; the id is the hex between the prefix and
/// the closing `-->`.
fn finding_marker(body: &str) -> Option<FindingId> {
    let start = body.find(FINDING_MARKER_PREFIX)? + FINDING_MARKER_PREFIX.len();
    let rest = &body[start..];
    let end = rest.find("-->")?;
    // A checked parse: an empty, truncated, or otherwise malformed id resolves to no
    // finding rather than a bogus id that could never match a real one.
    FindingId::from_hex(rest[..end].trim())
}

/// `GET` the pull request itself (for its body).
fn pull_request_request(owner: &str, repo: &str, pr: u64) -> ApiRequest {
    ApiRequest::get(format!("/repos/{owner}/{repo}/pulls/{pr}"))
}

/// `GET` the PR's issue (conversation) comments.
fn issue_comments_request(owner: &str, repo: &str, pr: u64) -> ApiRequest {
    ApiRequest::get(format!(
        "/repos/{owner}/{repo}/issues/{pr}/comments?per_page=100"
    ))
}

/// `GET` the PR's review (inline diff) comments.
fn review_comments_request(owner: &str, repo: &str, pr: u64) -> ApiRequest {
    ApiRequest::get(format!(
        "/repos/{owner}/{repo}/pulls/{pr}/comments?per_page=100"
    ))
}

/// Send a `GET` through [`send_checked`] and deserialize its body, so a rejected
/// gather is a real error the caller can log. Shared with [`super::signing`], the
/// other GET-shaped consumer of the seam.
pub(super) async fn get_json<A, T>(api: &A, req: &ApiRequest) -> Result<T>
where
    A: GitHubApi + ?Sized,
    T: serde::de::DeserializeOwned,
{
    let resp = send_checked(api, req).await?;
    serde_json::from_value(resp.body).wrap_err_with(|| {
        format!(
            "parsing the response to {} {}",
            req.method.as_str(),
            req.path
        )
    })
}

/// The slice of a GitHub pull request Bastion reads: the description body, the
/// author, and the head commit. The GET already happens in [`gather`], so the
/// author login and head SHA ride the same response rather than a duplicate
/// request.
#[derive(Debug, Deserialize)]
struct PullRequest {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    head: Option<PullRequestHead>,
}

/// The slice of a pull request's `head` Bastion reads: just the commit SHA.
#[derive(Debug, Deserialize)]
struct PullRequestHead {
    sha: String,
}

/// A GitHub comment id: the key that threads a review-comment reply onto its root. A
/// `NonZeroU64` newtype so a comment id cannot be confused with any other number (a PR
/// number, a finding hash) and so neither a missing id nor a `0` is representable: a real
/// GitHub comment id is positive, so an absent or zero id is a parse error, never a value
/// that could collide in the routing map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
struct CommentId(NonZeroU64);

/// The slice of a GitHub comment Bastion reads, shared by issue and review comments.
/// Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct RawComment {
    /// The comment's own id. Required: every GitHub comment carries one, and a thread
    /// reply routes by it, so a payload without it is malformed rather than defaultable.
    id: CommentId,
    #[serde(default)]
    body: String,
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    author_association: Option<String>,
    /// Present on a review comment that replies within a thread: the id of the thread's
    /// root comment. Absent on issue comments and on a thread's first comment.
    #[serde(default)]
    in_reply_to_id: Option<CommentId>,
}

impl RawComment {
    /// Normalize into a [`ContextComment`], or `None` if it is Bastion's own or empty.
    /// `routed` is the finding this comment replies to, already resolved by the caller.
    fn to_context(&self, routed: Option<FindingId>) -> Option<ContextComment> {
        let body = self.body.trim();
        if body.is_empty() || is_bastion_comment(&self.body) {
            return None;
        }
        Some(ContextComment {
            author: self.user.as_ref().and_then(|u| u.login.clone()),
            standing: standing_from_association(self.author_association.as_deref()),
            body: body.to_string(),
            in_reply_to: routed,
        })
    }
}

/// The slice of a GitHub user Bastion reads: the login, for display only.
#[derive(Debug, Deserialize)]
struct User {
    #[serde(default)]
    login: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::ApiResponse;
    use crate::github::client::test_support::RecordingClient;

    /// A recording client that answers each gather request from a small routing table
    /// keyed by a substring of the path.
    fn responder(
        pull_body: serde_json::Value,
        issue_comments: serde_json::Value,
        review_comments: serde_json::Value,
    ) -> RecordingClient {
        RecordingClient::with_responder(move |req| {
            let body = if req.path.contains("/issues/") {
                issue_comments.clone()
            } else if req.path.contains("/pulls/") && req.path.contains("/comments") {
                review_comments.clone()
            } else {
                pull_body.clone()
            };
            ApiResponse { status: 200, body }
        })
    }

    #[tokio::test]
    async fn gathers_intent_and_filters_bastions_own_comment() {
        let client = responder(
            serde_json::json!({ "body": "## Why\nDeliberate schema nuke." }),
            serde_json::json!([
                { "id": 1, "body": "Looks good to me.", "user": { "login": "grace" }, "author_association": "OWNER" },
                { "id": 2, "body": "<!-- bastion-report -->\n## Bastion review\nBlocked.", "user": { "login": "github-actions[bot]" }, "author_association": "NONE" },
                { "id": 3, "body": "   ", "user": { "login": "ada" }, "author_association": "CONTRIBUTOR" }
            ]),
            serde_json::json!([]),
        );

        let gathered = gather(&client, "acme", "app", 7).await.expect("gathers");
        assert_eq!(
            gathered.intent.as_deref(),
            Some("## Why\nDeliberate schema nuke.")
        );
        // Bastion's own sticky comment and the whitespace-only comment are dropped;
        // only the human owner comment survives.
        assert_eq!(gathered.comments.len(), 1);
        assert_eq!(gathered.comments[0].author.as_deref(), Some("grace"));
        assert_eq!(gathered.comments[0].standing, Standing::Owner);
        assert_eq!(gathered.comments[0].in_reply_to, None);
    }

    #[tokio::test]
    async fn maps_author_association_to_standing() {
        let client = responder(
            serde_json::json!({ "body": "" }),
            serde_json::json!([
                { "id": 1, "body": "owner", "user": { "login": "a" }, "author_association": "OWNER" },
                { "id": 2, "body": "member", "user": { "login": "b" }, "author_association": "MEMBER" },
                { "id": 3, "body": "collab", "user": { "login": "c" }, "author_association": "COLLABORATOR" },
                { "id": 4, "body": "contrib", "user": { "login": "d" }, "author_association": "CONTRIBUTOR" },
                { "id": 5, "body": "none", "user": { "login": "e" }, "author_association": "NONE" },
                { "id": 6, "body": "weird", "user": { "login": "f" }, "author_association": "FIRST_TIMER" }
            ]),
            serde_json::json!([]),
        );
        let gathered = gather(&client, "o", "r", 1).await.expect("gathers");
        let standing = |body: &str| {
            gathered
                .comments
                .iter()
                .find(|c| c.body == body)
                .unwrap()
                .standing
        };
        assert_eq!(standing("owner"), Standing::Owner);
        assert_eq!(standing("member"), Standing::Member);
        assert_eq!(standing("collab"), Standing::Member);
        assert_eq!(standing("contrib"), Standing::Contributor);
        assert_eq!(standing("none"), Standing::Outsider);
        assert_eq!(standing("weird"), Standing::Outsider);
        // An empty PR body yields no intent.
        assert_eq!(gathered.intent, None);
    }

    #[tokio::test]
    async fn routes_a_review_reply_to_its_finding_via_the_marker() {
        // A Bastion finding comment (root, id 100, carrying a finding marker) and a human
        // reply onto it (in_reply_to_id 100). The reply must route to that FindingId; the
        // Bastion root itself is filtered out of the discussion.
        let client = responder(
            serde_json::json!({ "body": "intent" }),
            serde_json::json!([]),
            serde_json::json!([
                {
                    "id": 100,
                    "body": "<!-- bastion-finding:abc123def4560000 -->\n**blocking**: O(n^2) append",
                    "user": { "login": "github-actions[bot]" },
                    "author_association": "NONE"
                },
                {
                    "id": 101,
                    "in_reply_to_id": 100,
                    "body": "This is intentional, here is why.",
                    "user": { "login": "ada" },
                    "author_association": "CONTRIBUTOR"
                }
            ]),
        );
        let gathered = gather(&client, "o", "r", 1).await.expect("gathers");
        // Only the human reply survives; it is routed to the finding id from the marker.
        assert_eq!(gathered.comments.len(), 1);
        let reply = &gathered.comments[0];
        assert_eq!(reply.author.as_deref(), Some("ada"));
        assert_eq!(
            reply.in_reply_to.as_ref().map(FindingId::as_str),
            Some("abc123def4560000")
        );
    }

    #[tokio::test]
    async fn a_reply_to_a_non_bastion_root_is_general() {
        // A human review-comment thread (no Bastion marker on the root) carries no
        // routing: both comments are general discussion.
        let client = responder(
            serde_json::json!({ "body": "intent" }),
            serde_json::json!([]),
            serde_json::json!([
                { "id": 1, "body": "what about this?", "user": { "login": "ada" }, "author_association": "CONTRIBUTOR" },
                { "id": 2, "in_reply_to_id": 1, "body": "good point", "user": { "login": "grace" }, "author_association": "OWNER" }
            ]),
        );
        let gathered = gather(&client, "o", "r", 1).await.expect("gathers");
        assert_eq!(gathered.comments.len(), 2);
        assert!(gathered.comments.iter().all(|c| c.in_reply_to.is_none()));
    }

    #[tokio::test]
    async fn a_non_2xx_response_is_an_error() {
        let client = RecordingClient::with_responder(|_req| ApiResponse {
            status: 404,
            body: serde_json::json!({ "message": "Not Found" }),
        });
        let err = gather(&client, "o", "r", 1).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[test]
    fn finding_marker_parses_and_rejects() {
        // A well-formed 16-hex-digit id parses.
        assert_eq!(
            finding_marker("<!-- bastion-finding:abc123def4560000 -->\nbody")
                .map(|f| f.as_str().to_string()),
            Some("abc123def4560000".to_string())
        );
        // No marker, or an empty id, yields nothing.
        assert_eq!(finding_marker("just a comment"), None);
        assert_eq!(finding_marker("<!-- bastion-finding: -->"), None);
        // A malformed id (wrong length, non-hex, or uppercase) is rejected by the
        // checked parse rather than producing a bogus id that can never match.
        assert_eq!(finding_marker("<!-- bastion-finding:deadbeef -->"), None);
        assert_eq!(
            finding_marker("<!-- bastion-finding:abc123def456000g -->"),
            None
        );
        assert_eq!(
            finding_marker("<!-- bastion-finding:ABC123DEF4560000 -->"),
            None
        );
    }

    #[tokio::test]
    async fn gathers_the_author_login_and_head_sha_from_the_same_pull_request_response() {
        let client = responder(
            serde_json::json!({
                "body": "intent",
                "user": { "login": "ada" },
                "head": { "sha": "abc123deadbeef" },
            }),
            serde_json::json!([]),
            serde_json::json!([]),
        );
        let gathered = gather(&client, "acme", "app", 1).await.expect("gathers");
        assert_eq!(gathered.author_login.as_deref(), Some("ada"));
        assert_eq!(gathered.head_sha.as_deref(), Some("abc123deadbeef"));

        // Exactly one request reached the pull-request endpoint: no duplicate GET
        // for the author or head SHA.
        let pr_requests = client
            .calls()
            .into_iter()
            .filter(|c| c.path.ends_with("/pulls/1"))
            .count();
        assert_eq!(pr_requests, 1);
    }

    #[tokio::test]
    async fn missing_author_and_head_fields_degrade_to_none() {
        let client = responder(
            serde_json::json!({ "body": "" }),
            serde_json::json!([]),
            serde_json::json!([]),
        );
        let gathered = gather(&client, "o", "r", 1).await.expect("gathers");
        assert_eq!(gathered.author_login, None);
        assert_eq!(gathered.head_sha, None);
    }
}
