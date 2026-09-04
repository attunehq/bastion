//! Gathering a pull request's context for the reviewers, from the GitHub side.
//!
//! This is the GitHub *producer* for the transport-neutral
//! [`ReviewContext`](crate::context::ReviewContext): it reads
//! a PR's identity and description through `gh`, with the Actions REST client as
//! a compatibility fallback for explicitly selected PRs. It also reads discussion
//! through `gh api`, with the same REST seam as the fallback when a token is
//! available. The module maps GitHub fields onto the generic shape the
//! runner and backends consume:
//!
//! - the PR's direct base supplies the automatic changeset base when the user did
//!   not pass `--base`;
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
//! A missing `gh`, a branch with no pull request, and a failed `gh` invocation are
//! distinct outcomes so a local review can stay silent in the first two cases and
//! warn only when `gh` actually ran and failed.
//!
//! The prior-findings half of a [`ReviewContext`](crate::context::ReviewContext) is recalled from the local run store
//! (`crate::store::findings_from_events`, over the branch's latest run), the same way regardless of transport, and merged in
//! by the caller; this module supplies only the intent and the discussion.
//!
//! Comments are authored by the gate's subject and by bystanders. The mapping preserves
//! who said what (for weighting) but grants no authority; see the framing in
//! [`crate::context`].

use std::num::NonZeroU64;
use std::path::Path;

use color_eyre::eyre::{Context, Result, bail};
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

/// Test seam for the GitHub CLI executable. Production resolves `gh` from
/// `PATH`; integration tests point this at their compiled fake.
pub(crate) const PROGRAM_ENV: &str = "BASTION_GH_BIN";

/// The fields one `gh pr view` call needs for target selection, intent,
/// discussion follow-up, and attestation identity.
const GH_PR_FIELDS: &str = "number,title,body,author,headRefOid,baseRefName,baseRefOid";

/// `gh api` substitutes these from the repository of the current directory.
const GH_OWNER_PLACEHOLDER: &str = "{owner}";
const GH_REPO_PLACEHOLDER: &str = "{repo}";

/// The identity and author intent of the pull request under review.
///
/// The head and base revisions are required and non-empty. The direct base
/// defines the automatic comparison point when the user did not pass `--base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequest {
    /// The PR description, as the author's stated intent. `None` when both the
    /// body and the title are empty.
    intent: Option<String>,
    /// The PR author's GitHub login, when the response carried one. This is the
    /// attestation signature's principal (`docs/developer-guide/attestation.md`,
    /// "Signing") and the key for the signing-key lookup
    /// ([`super::signing::ssh_signing_keys`]); a login the API omits (a deleted account) leaves
    /// attestation with nothing to verify against, so the caller falls back.
    author_login: Option<String>,
    /// The pull request number, when GitHub supplied a positive one. Discussion
    /// gathering needs it; a missing number leaves the rest of the identity usable
    /// and skips comments.
    number: Option<NonZeroU64>,
    /// The exact head commit GitHub records for the PR.
    head_sha: String,
    /// The PR's direct target branch, such as `main` or the preceding branch in a
    /// stack.
    base_ref: String,
    /// The exact target-branch commit GitHub records for the PR.
    base_sha: String,
}

impl PullRequest {
    /// The author's stated intent: a non-empty PR body, otherwise a non-empty title.
    #[must_use]
    pub(crate) fn intent(&self) -> Option<&str> {
        self.intent.as_deref()
    }

    /// The PR author's GitHub login, when GitHub supplied one.
    #[must_use]
    pub(crate) fn author_login(&self) -> Option<&str> {
        self.author_login.as_deref()
    }

    /// The pull request number, when GitHub supplied a positive one.
    #[must_use]
    pub(crate) fn number(&self) -> Option<NonZeroU64> {
        self.number
    }

    /// The exact head commit GitHub records for the PR.
    #[must_use]
    pub(crate) fn head_sha(&self) -> &str {
        &self.head_sha
    }

    /// The PR's direct target branch.
    #[must_use]
    pub(crate) fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// The exact target-branch commit GitHub records for the PR.
    #[must_use]
    pub(crate) fn base_sha(&self) -> &str {
        &self.base_sha
    }
}

/// Outcome of asking `gh` for the current branch's pull request.
///
/// The three non-found cases are distinct so a local review can stay silent when
/// there is no PR or `gh` cannot run, and warn only when `gh` ran and failed.
#[derive(Debug)]
#[must_use]
pub(crate) enum PullRequestLookup {
    /// The current branch has an open pull request.
    Found(PullRequest),
    /// `gh` ran and reported no pull request for this branch.
    None,
    /// `gh` could not be started (missing binary, not executable, or any other
    /// spawn failure).
    Unavailable,
    /// `gh` started but failed or returned unusable output.
    Failed(color_eyre::eyre::Error),
}

/// Ask `gh` for the pull request associated with the current branch.
pub(crate) async fn detect_pull_request(cwd: &Path) -> PullRequestLookup {
    gh_pull_request(cwd, None, true).await
}

/// Ask `gh` for one explicitly selected pull request.
///
/// # Errors
///
/// Returns an error if `gh` cannot run, rejects the request, or emits malformed
/// JSON.
pub(crate) async fn get_pull_request_with_gh(
    cwd: &Path,
    repository: &str,
    pr: u64,
) -> Result<PullRequest> {
    match gh_pull_request(cwd, Some((repository, pr)), false).await {
        PullRequestLookup::Found(pull) => Ok(pull),
        PullRequestLookup::None => Err(color_eyre::eyre::eyre!("gh returned no pull request")),
        PullRequestLookup::Unavailable => Err(color_eyre::eyre::eyre!(
            "could not run `gh` to read the pull request"
        )),
        PullRequestLookup::Failed(err) => Err(err),
    }
}

async fn gh_pull_request(
    cwd: &Path,
    selected: Option<(&str, u64)>,
    no_pr_is_none: bool,
) -> PullRequestLookup {
    let program = gh_program();
    let mut command = tokio::process::Command::new(&program);
    command.args(["pr", "view"]);
    if let Some((repository, pr)) = selected {
        command.arg(pr.to_string()).args(["--repo", repository]);
    }
    let output = match command
        .args(["--json", GH_PR_FIELDS])
        .current_dir(cwd)
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(output) => output,
        Err(_) => return PullRequestLookup::Unavailable,
    };

    lookup_from_process(
        output.status.success(),
        &output.stdout,
        &String::from_utf8_lossy(&output.stderr),
        no_pr_is_none,
    )
}

fn lookup_from_process(
    success: bool,
    stdout: &[u8],
    stderr: &str,
    no_pr_is_none: bool,
) -> PullRequestLookup {
    if !success {
        if no_pr_is_none && stderr.contains("no pull requests found for branch") {
            return PullRequestLookup::None;
        }
        return PullRequestLookup::Failed(color_eyre::eyre::eyre!(
            "`gh pr view` failed: {}",
            stderr.trim()
        ));
    }

    match serde_json::from_slice::<GhPullRequest>(stdout)
        .wrap_err("parsing the output of `gh pr view`")
        .and_then(GhPullRequest::parse)
    {
        Ok(pull) => PullRequestLookup::Found(pull),
        Err(err) => PullRequestLookup::Failed(err),
    }
}

fn gh_program() -> std::ffi::OsString {
    std::env::var_os(PROGRAM_ENV).unwrap_or_else(|| "gh".into())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    author: Option<User>,
    head_ref_oid: String,
    base_ref_name: String,
    base_ref_oid: String,
}

impl GhPullRequest {
    fn parse(self) -> Result<PullRequest> {
        Ok(PullRequest {
            intent: intent_from(self.title, self.body),
            author_login: self.author.and_then(|author| author.login),
            number: self.number.and_then(NonZeroU64::new),
            head_sha: non_empty_field(self.head_ref_oid, "headRefOid")?,
            base_ref: non_empty_field(self.base_ref_name, "baseRefName")?,
            base_sha: non_empty_field(self.base_ref_oid, "baseRefOid")?,
        })
    }
}

fn intent_from(title: Option<String>, body: Option<String>) -> Option<String> {
    nonempty_text(body).or_else(|| nonempty_text(title))
}

fn nonempty_text(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Gather the identity and author intent of one pull request over `api`.
///
/// This is the Actions REST compatibility path when `gh` is unavailable. The
/// returned [`PullRequest`] carries non-empty head and base revisions.
///
/// # Errors
///
/// Returns an error if a request cannot be sent, returns a non-2xx status, or returns a
/// body without the required pull-request identity fields.
pub(crate) async fn get_pull_request<A: GitHubApi + ?Sized>(
    api: &A,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<PullRequest> {
    let pull_req = pull_request_request(owner, repo, pr);
    let pull: RawPullRequest = get_json(api, &pull_req).await?;
    let author_login = pull.user.and_then(|u| u.login);
    let head_sha = non_empty_field(pull.head.sha, "head.sha")?;
    let base_ref = non_empty_field(pull.base.name, "base.ref")?;
    let base_sha = non_empty_field(pull.base.sha, "base.sha")?;

    Ok(PullRequest {
        intent: intent_from(pull.title, pull.body),
        author_login,
        number: NonZeroU64::new(pr),
        head_sha,
        base_ref,
        base_sha,
    })
}

/// Gather a pull request's human discussion over `api`.
///
/// Reads top-level conversation comments and inline review comments, filters out
/// Bastion's own comments, and normalizes the rest. This context is advisory: the
/// review command logs a failure and continues after the required pull-request
/// identity has resolved.
///
/// # Errors
///
/// Returns an error if either request cannot be sent, returns a non-2xx status,
/// or returns a body that does not parse as a comment list.
pub(crate) async fn gather_discussion<A: GitHubApi + ?Sized>(
    api: &A,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<Vec<ContextComment>> {
    let issue_req = issue_comments_request(owner, repo, pr);
    let review_req = review_comments_request(owner, repo, pr);
    let (issue_comments, review_comments): (Vec<RawComment>, Vec<RawComment>) =
        tokio::try_join!(get_json(api, &issue_req), get_json(api, &review_req))?;

    Ok(discussion_from_raw(&issue_comments, &review_comments))
}

/// Outcome of asking `gh api` for a pull request's discussion.
///
/// Unavailable (cannot spawn `gh`) stays silent at the caller; a failed
/// invocation is the case that warns.
#[derive(Debug)]
#[must_use]
pub(crate) enum DiscussionLookup {
    /// Normalized, filtered comments from the conversation and review threads.
    Comments(Vec<ContextComment>),
    /// `gh` could not be started.
    Unavailable,
    /// `gh` started but failed or returned unusable output.
    Failed(color_eyre::eyre::Error),
}

/// Gather a pull request's human discussion through `gh api`.
///
/// Uses the same REST JSON shapes as [`gather_discussion`], so filtering and
/// reply routing stay in one place. `{owner}` / `{repo}` in `owner` and `repo`
/// are left for `gh api` to substitute from the current directory's repository.
pub(crate) async fn gather_discussion_with_gh(
    cwd: &Path,
    owner: &str,
    repo: &str,
    pr: u64,
) -> DiscussionLookup {
    let issue_path = format!("/repos/{owner}/{repo}/issues/{pr}/comments?per_page=100");
    let review_path = format!("/repos/{owner}/{repo}/pulls/{pr}/comments?per_page=100");
    match tokio::try_join!(
        gh_api_json::<Vec<RawComment>>(cwd, &issue_path),
        gh_api_json::<Vec<RawComment>>(cwd, &review_path),
    ) {
        Ok((issue_comments, review_comments)) => {
            DiscussionLookup::Comments(discussion_from_raw(&issue_comments, &review_comments))
        }
        Err(GhCall::Unavailable) => DiscussionLookup::Unavailable,
        Err(GhCall::Failed(err)) => DiscussionLookup::Failed(err),
    }
}

/// Gather discussion for the pull request on the current repository, letting
/// `gh api` fill `{owner}` and `{repo}` from the checkout.
pub(crate) async fn gather_current_repo_discussion(cwd: &Path, pr: NonZeroU64) -> DiscussionLookup {
    gather_discussion_with_gh(cwd, GH_OWNER_PLACEHOLDER, GH_REPO_PLACEHOLDER, pr.get()).await
}

fn discussion_from_raw(
    issue_comments: &[RawComment],
    review_comments: &[RawComment],
) -> Vec<ContextComment> {
    let mut comments = Vec::new();

    // Top-level conversation comments never thread to a specific finding.
    for raw in issue_comments {
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
    for raw in review_comments {
        let routed = raw
            .in_reply_to_id
            .and_then(|root_id| roots.get(&root_id))
            .and_then(|root_body| finding_marker(root_body));
        if let Some(comment) = raw.to_context(routed) {
            comments.push(comment);
        }
    }

    comments
}

enum GhCall {
    Unavailable,
    Failed(color_eyre::eyre::Error),
}

async fn gh_api_json<T>(cwd: &Path, path: &str) -> Result<T, GhCall>
where
    T: serde::de::DeserializeOwned,
{
    let program = gh_program();
    let output = match tokio::process::Command::new(&program)
        .args(["api", "--method", "GET", path])
        .current_dir(cwd)
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(output) => output,
        Err(_) => return Err(GhCall::Unavailable),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GhCall::Failed(color_eyre::eyre::eyre!(
            "`gh api {}` failed: {}",
            path,
            stderr.trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .wrap_err_with(|| format!("parsing the output of `gh api {path}`"))
        .map_err(GhCall::Failed)
}

fn non_empty_field(value: String, field: &str) -> Result<String> {
    if value.is_empty() {
        bail!("GitHub's pull-request response has an empty `{field}` field");
    }
    Ok(value)
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
struct RawPullRequest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<User>,
    head: RawPullRequestRef,
    base: RawPullRequestRef,
}

/// The slice of a pull request's `head` or `base` that identifies a branch and
/// its current commit.
#[derive(Debug, Deserialize)]
struct RawPullRequestRef {
    #[serde(rename = "ref")]
    name: String,
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

    fn pull(body: &str) -> serde_json::Value {
        serde_json::json!({
            "body": body,
            "head": { "ref": "feature", "sha": "head123" },
            "base": { "ref": "main", "sha": "base123" },
        })
    }

    #[test]
    fn gh_pull_request_fields_parse_into_the_review_target() {
        let raw: GhPullRequest = serde_json::from_value(serde_json::json!({
            "number": 7,
            "title": "stack layer",
            "body": "  stack layer  ",
            "author": { "login": "ada" },
            "headRefOid": "head123",
            "baseRefName": "feature-a",
            "baseRefOid": "base123"
        }))
        .unwrap();
        let pull = raw.parse().unwrap();

        assert_eq!(pull.intent(), Some("stack layer"));
        assert_eq!(pull.author_login(), Some("ada"));
        assert_eq!(pull.number().map(NonZeroU64::get), Some(7));
        assert_eq!(pull.head_sha(), "head123");
        assert_eq!(pull.base_ref(), "feature-a");
        assert_eq!(pull.base_sha(), "base123");
    }

    #[test]
    fn empty_body_falls_back_to_the_title_for_intent() {
        let raw: GhPullRequest = serde_json::from_value(serde_json::json!({
            "number": 3,
            "title": "  title only  ",
            "body": "   ",
            "headRefOid": "head123",
            "baseRefName": "main",
            "baseRefOid": "base123"
        }))
        .unwrap();
        let pull = raw.parse().unwrap();
        assert_eq!(pull.intent(), Some("title only"));
    }

    #[test]
    fn lookup_treats_a_missing_current_branch_pr_as_none() {
        let lookup = lookup_from_process(
            false,
            b"",
            "no pull requests found for branch \"feature\"",
            true,
        );
        assert!(matches!(lookup, PullRequestLookup::None));
    }

    #[test]
    fn lookup_treats_other_gh_failures_as_failed() {
        let lookup = lookup_from_process(false, b"", "authentication required", true);
        match lookup {
            PullRequestLookup::Failed(err) => {
                assert!(format!("{err:#}").contains("authentication required"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn lookup_parses_a_successful_gh_payload() {
        let stdout = serde_json::json!({
            "number": 9,
            "title": "feat",
            "body": "why",
            "author": { "login": "ada" },
            "headRefOid": "head123",
            "baseRefName": "main",
            "baseRefOid": "base123"
        })
        .to_string();
        let lookup = lookup_from_process(true, stdout.as_bytes(), "", true);
        match lookup {
            PullRequestLookup::Found(pull) => {
                assert_eq!(pull.intent(), Some("why"));
                assert_eq!(pull.number().map(NonZeroU64::get), Some(9));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gathers_intent_and_filters_bastions_own_comment() {
        let client = responder(
            pull("## Why\nDeliberate schema nuke."),
            serde_json::json!([
                { "id": 1, "body": "Looks good to me.", "user": { "login": "grace" }, "author_association": "OWNER" },
                { "id": 2, "body": "<!-- bastion-report -->\n## Bastion review\nBlocked.", "user": { "login": "github-actions[bot]" }, "author_association": "NONE" },
                { "id": 3, "body": "   ", "user": { "login": "ada" }, "author_association": "CONTRIBUTOR" }
            ]),
            serde_json::json!([]),
        );

        let pull = get_pull_request(&client, "acme", "app", 7)
            .await
            .expect("gathers pull request");
        let comments = gather_discussion(&client, "acme", "app", 7)
            .await
            .expect("gathers discussion");
        assert_eq!(pull.intent(), Some("## Why\nDeliberate schema nuke."));
        assert_eq!(pull.number().map(NonZeroU64::get), Some(7));
        // Bastion's own sticky comment and the whitespace-only comment are dropped;
        // only the human owner comment survives.
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author.as_deref(), Some("grace"));
        assert_eq!(comments[0].standing, Standing::Owner);
        assert_eq!(comments[0].in_reply_to, None);
    }

    #[tokio::test]
    async fn maps_author_association_to_standing() {
        let client = responder(
            pull(""),
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
        let comments = gather_discussion(&client, "o", "r", 1)
            .await
            .expect("gathers discussion");
        let standing = |body: &str| comments.iter().find(|c| c.body == body).unwrap().standing;
        assert_eq!(standing("owner"), Standing::Owner);
        assert_eq!(standing("member"), Standing::Member);
        assert_eq!(standing("collab"), Standing::Member);
        assert_eq!(standing("contrib"), Standing::Contributor);
        assert_eq!(standing("none"), Standing::Outsider);
        assert_eq!(standing("weird"), Standing::Outsider);
    }

    #[tokio::test]
    async fn routes_a_review_reply_to_its_finding_via_the_marker() {
        // A Bastion finding comment (root, id 100, carrying a finding marker) and a human
        // reply onto it (in_reply_to_id 100). The reply must route to that FindingId; the
        // Bastion root itself is filtered out of the discussion.
        let client = responder(
            pull("intent"),
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
        let comments = gather_discussion(&client, "o", "r", 1)
            .await
            .expect("gathers discussion");
        // Only the human reply survives; it is routed to the finding id from the marker.
        assert_eq!(comments.len(), 1);
        let reply = &comments[0];
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
            pull("intent"),
            serde_json::json!([]),
            serde_json::json!([
                { "id": 1, "body": "what about this?", "user": { "login": "ada" }, "author_association": "CONTRIBUTOR" },
                { "id": 2, "in_reply_to_id": 1, "body": "good point", "user": { "login": "grace" }, "author_association": "OWNER" }
            ]),
        );
        let comments = gather_discussion(&client, "o", "r", 1)
            .await
            .expect("gathers discussion");
        assert_eq!(comments.len(), 2);
        assert!(comments.iter().all(|c| c.in_reply_to.is_none()));
    }

    #[tokio::test]
    async fn a_non_2xx_response_is_an_error() {
        let client = RecordingClient::with_responder(|_req| ApiResponse {
            status: 404,
            body: serde_json::json!({ "message": "Not Found" }),
        });
        let err = get_pull_request(&client, "o", "r", 1).await.unwrap_err();
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
                "head": { "ref": "feature", "sha": "abc123deadbeef" },
                "base": { "ref": "parent", "sha": "def456deadbeef" },
            }),
            serde_json::json!([]),
            serde_json::json!([]),
        );
        let pull = get_pull_request(&client, "acme", "app", 1)
            .await
            .expect("gathers pull request");
        assert_eq!(pull.author_login(), Some("ada"));
        assert_eq!(pull.head_sha(), "abc123deadbeef");
        assert_eq!(pull.base_ref(), "parent");
        assert_eq!(pull.base_sha(), "def456deadbeef");

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
    async fn a_missing_author_is_allowed_but_missing_revisions_are_rejected() {
        let client = responder(pull(""), serde_json::json!([]), serde_json::json!([]));
        let pull = get_pull_request(&client, "o", "r", 1)
            .await
            .expect("required revisions are present");
        assert_eq!(pull.author_login(), None);
        assert_eq!(pull.intent(), None);

        let missing = responder(
            serde_json::json!({ "body": "", "head": { "ref": "feature", "sha": "head" } }),
            serde_json::json!([]),
            serde_json::json!([]),
        );
        let err = get_pull_request(&missing, "o", "r", 1).await.unwrap_err();
        assert!(format!("{err:#}").contains("base"), "got: {err:#}");

        let empty = responder(
            serde_json::json!({
                "body": "",
                "head": { "ref": "feature", "sha": "" },
                "base": { "ref": "main", "sha": "base" }
            }),
            serde_json::json!([]),
            serde_json::json!([]),
        );
        let err = get_pull_request(&empty, "o", "r", 1).await.unwrap_err();
        assert!(format!("{err:#}").contains("head.sha"), "got: {err:#}");
    }
}
