//! Posting the report: the IO entry point and comment upsert.

use super::*;

/// What the report did to the sticky comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentAction {
    /// Posted a new comment.
    Created,
    /// Updated the existing sticky comment in place.
    Updated(u64),
}

/// A short account of what the report posted, for the CLI to print.
#[derive(Debug, Clone, Copy)]
pub struct ReportSummary {
    /// What happened to the sticky comment.
    pub comment: CommentAction,
    /// How many check runs were created (per reviewer plus the aggregate).
    pub checks: usize,
}

impl fmt::Display for ReportSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let comment = match self.comment {
            CommentAction::Created => "posted a new PR comment".to_string(),
            CommentAction::Updated(id) => format!("updated PR comment {id}"),
        };
        write!(f, "{comment}; created {} check run(s)", self.checks)
    }
}

/// Post a finished run's results to its pull request.
///
/// Creates a check run per reviewer plus the aggregate `bastion` check, then upserts
/// the sticky comment. Any non-2xx response aborts with a legible error.
///
/// The checks go first on purpose: GitHub stamps each created check run with the
/// `app` that posted it, so the first response tells the report which identity it is
/// acting under. When that is the shared `github-actions` app (the default
/// `GITHUB_TOKEN`, with no dedicated app configured), the checks cannot form their
/// own suite, so the comment closes with a nudge toward setting one up. This is
/// decided here from GitHub's own response, independent of how the workflow is
/// written.
///
/// `skills_warning` is an optional skills-freshness advisory folded into the comment
/// when the repo's bundled skills are missing or stale. It arrives as a parsed
/// [`crate::skills::DriftWarning`] (not raw Markdown), so only an advisory produced
/// by `skills::assess` can reach the comment, never arbitrary caller-supplied text.
/// It is advisory only and leaves every check-run conclusion untouched.
///
/// # Errors
///
/// Returns an error if a GitHub request fails to send or returns a non-2xx status.
pub async fn report<A: GitHubApi + ?Sized>(
    api: &A,
    ctx: &PrContext,
    events: &[RunEvent],
    skills_warning: Option<&crate::skills::DriftWarning>,
) -> Result<ReportSummary> {
    let digest = digest(events);

    let checks = check_runs(ctx, &digest);
    // The identity we posted under, classified from the first check-run response.
    let mut posting_app = PostingApp::Unknown;
    for (i, check) in checks.iter().enumerate() {
        let resp = send_checked(api, &check_run_request(ctx, check)).await?;
        if i == 0 {
            posting_app = PostingApp::from_check_run(&resp.body);
        }
    }

    // Render the parsed advisory to Markdown at the boundary of the private comment
    // builder, so the public API keeps requiring the proof type.
    let warning_md = skills_warning.map(crate::skills::DriftWarning::markdown);
    let body = comment_body(
        &digest,
        posting_app.should_suggest_dedicated_app(),
        warning_md.as_deref(),
    );
    let comment = upsert_comment(api, ctx, &body).await?;

    Ok(ReportSummary {
        comment,
        checks: checks.len(),
    })
}

/// Which GitHub App created the check runs, parsed once from the `app.slug` GitHub
/// stamps on a check-run response. This is the report's posting identity, and it is
/// what decides whether the checks can form their own check suite. Parsing it into a
/// type here keeps the raw `github-actions` string comparison out of [`report`] and
/// names the three cases so none is forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PostingApp {
    /// The shared `github-actions` app (the default `GITHUB_TOKEN`). Its check runs
    /// cannot form their own suite, so the report nudges toward a dedicated app.
    SharedActions,
    /// A distinct GitHub App: its check runs get their own named suite, so no nudge.
    Dedicated,
    /// The response carried no readable `app.slug` (an unexpected or fake shape).
    /// Leave the nudge off rather than guess.
    Unknown,
}

impl PostingApp {
    /// Classify the creating app from a check-run creation response. GitHub always
    /// stamps `app.slug` on a real response; a fake or truncated body may not, which
    /// maps to [`PostingApp::Unknown`].
    pub(super) fn from_check_run(check_run_body: &serde_json::Value) -> Self {
        match check_run_body
            .get("app")
            .and_then(|app| app.get("slug"))
            .and_then(|slug| slug.as_str())
        {
            Some(SHARED_APP_SLUG) => Self::SharedActions,
            Some(_) => Self::Dedicated,
            None => Self::Unknown,
        }
    }

    /// Whether the sticky comment should nudge toward a dedicated app.
    pub(super) fn should_suggest_dedicated_app(self) -> bool {
        matches!(self, Self::SharedActions)
    }
}

/// Create the sticky comment, or update it in place if one already exists.
pub(super) async fn upsert_comment<A: GitHubApi + ?Sized>(
    api: &A,
    ctx: &PrContext,
    body: &str,
) -> Result<CommentAction> {
    let listed = send_checked(api, &comment_list_request(ctx)).await?;
    match find_marker_comment(&listed.body)? {
        Some(id) => {
            send_checked(api, &comment_update_request(ctx, id, body)).await?;
            Ok(CommentAction::Updated(id))
        }
        None => {
            send_checked(api, &comment_create_request(ctx, body)).await?;
            Ok(CommentAction::Created)
        }
    }
}

/// Find the id of Bastion's own sticky comment in a comment-list response, by its
/// hidden [`MARKER`].
///
/// Fails closed on a malformed list body rather than collapsing a parse error into
/// "no existing comment": treating an unexpected response shape as "none found"
/// would post a fresh comment on every run, stacking duplicates. A body Bastion
/// cannot parse is an error to surface, not a silent create.
///
/// # Errors
///
/// Returns an error if the response body is not the expected array of comments.
pub(super) fn find_marker_comment(list_body: &serde_json::Value) -> Result<Option<u64>> {
    let comments: Vec<IssueComment> = serde_json::from_value(list_body.clone())
        .wrap_err("parsing the PR comment list from GitHub")?;
    Ok(comments
        .into_iter()
        .find(|c| c.body.contains(MARKER))
        .map(|c| c.id))
}

/// Send a request and treat any non-2xx status as an error, surfacing GitHub's
/// own message. The fail-closed posture: a reporting call that GitHub rejected is
/// a real failure, not something to swallow.
pub(super) async fn send_checked<A: GitHubApi + ?Sized>(
    api: &A,
    req: &ApiRequest,
) -> Result<ApiResponse> {
    let resp = api.send(req).await?;
    if !resp.is_success() {
        bail!(
            "GitHub {} {} returned {}: {}",
            req.method.as_str(),
            req.path,
            resp.status,
            resp.error_message().unwrap_or("(no message)"),
        );
    }
    Ok(resp)
}
