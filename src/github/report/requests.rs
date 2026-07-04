//! Constructing the GitHub REST requests.

use super::*;

/// `GET` the PR's issue comments (to find an existing sticky comment).
pub(super) fn comment_list_request(ctx: &PrContext) -> ApiRequest {
    ApiRequest::get(format!(
        "/repos/{}/{}/issues/{}/comments?per_page=100",
        ctx.owner, ctx.repo, ctx.pr
    ))
}

/// `POST` a new issue comment.
pub(super) fn comment_create_request(ctx: &PrContext, body: &str) -> ApiRequest {
    ApiRequest::post(
        format!(
            "/repos/{}/{}/issues/{}/comments",
            ctx.owner, ctx.repo, ctx.pr
        ),
        serde_json::json!({ "body": body }),
    )
}

/// `PATCH` an existing issue comment in place.
pub(super) fn comment_update_request(ctx: &PrContext, comment_id: u64, body: &str) -> ApiRequest {
    ApiRequest::patch(
        format!(
            "/repos/{}/{}/issues/comments/{}",
            ctx.owner, ctx.repo, comment_id
        ),
        serde_json::json!({ "body": body }),
    )
}

/// `POST` a completed check run.
pub(super) fn check_run_request(ctx: &PrContext, check: &CheckRun) -> ApiRequest {
    let annotations: Vec<serde_json::Value> = check
        .annotations
        .iter()
        .map(|a| {
            serde_json::json!({
                "path": a.path,
                "start_line": a.start_line,
                "end_line": a.end_line,
                "annotation_level": a.level,
                "message": a.message,
            })
        })
        .collect();
    ApiRequest::post(
        format!("/repos/{}/{}/check-runs", ctx.owner, ctx.repo),
        serde_json::json!({
            "name": check.name,
            "head_sha": check.head_sha,
            "status": "completed",
            "conclusion": check.conclusion.as_str(),
            "output": {
                "title": check.title,
                "summary": check.summary,
                "annotations": annotations,
            },
        }),
    )
}
