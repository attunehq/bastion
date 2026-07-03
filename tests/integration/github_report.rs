//! `bastion github report` driven against a fake GitHub.
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;
use crate::github::*;

#[test]
fn github_report_posts_a_comment_and_checks_for_a_blocked_run() {
    let Some(fake) = tooling() else { return };

    // A single blocking gate, so the run blocks and carries a located finding.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "tenant-isolation",
        "claude-code",
        "gate",
    )
    .behavior("block")]));

    // Persist a real run by driving `bastion review` through the fake agent.
    let review = repo.review(fake);
    assert!(!review.exited_zero(), "a blocking review exits non-zero");

    // Now report that run to a fake GitHub, exercising the real binary's argument
    // parsing, env-driven client, run resolution, and HTTP posting end to end.
    let github = FakeGitHub::start();
    let output = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    assert!(
        output.status.success(),
        "report should succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = github.finish();

    // The sticky comment is upserted: a GET to list, then a POST to create it
    // (the fake returns an empty list, so there is nothing to update in place).
    let list = requests
        .iter()
        .find(|r| r.method == "GET" && r.path.starts_with("/repos/acme/app/issues/7/comments"))
        .expect("a GET listing the PR comments");
    assert!(list.path.contains("per_page=100"));

    let comment = requests
        .iter()
        .find(|r| r.method == "POST" && r.path == "/repos/acme/app/issues/7/comments")
        .expect("a POST creating the sticky comment");
    // The comment carries the hidden marker (for future in-place updates) and the
    // reviewer's blocking finding, so a reader never has to open the artifact.
    assert!(
        comment.body.contains("bastion-report"),
        "marker missing: {}",
        comment.body
    );
    assert!(comment.body.contains("Bastion review"));
    assert!(comment.body.contains("simulated blocking finding"));
    // This repo never installed the bundled skills, so the report folds in the
    // freshness advisory (a GitHub `[!WARNING]` callout) pointing at `skills install`.
    assert!(
        comment.body.contains("[!WARNING]")
            && comment
                .body
                .contains("bundled agent skills are missing or out of date")
            && comment.body.contains("bastion skills install"),
        "the skills advisory is missing from the comment: {}",
        comment.body
    );
    // The fake stamps check runs with the shared `github-actions` app (as the
    // default GITHUB_TOKEN does), so the report detects the missing dedicated app
    // from the check-run response on its own and closes the comment with the nudge.
    assert!(
        comment.body.contains("bastion.jessica.black/github-app"),
        "report should detect the shared app and nudge toward a dedicated one: {}",
        comment.body
    );

    // One check run per reviewer plus the always-present aggregate `bastion` check.
    let checks: Vec<&CapturedRequest> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/repos/acme/app/check-runs")
        .collect();
    assert_eq!(checks.len(), 2, "expected reviewer + aggregate check runs");
    // The reviewer's gate blocked, so its check concludes failure against the head SHA...
    assert!(
        checks
            .iter()
            .any(|c| c.body.contains("bastion / tenant-isolation")
                && c.body.contains(r#""conclusion":"failure""#)
                && c.body.contains("deadcafe")),
        "a failing reviewer check run is missing: {checks:?}"
    );
    // ...and the aggregate reflects the blocked run.
    assert!(
        checks.iter().any(|c| c.body.contains(r#""name":"bastion""#)
            && c.body.contains(r#""conclusion":"failure""#)),
        "the aggregate bastion check is missing: {checks:?}"
    );

    // Once the skills are installed, re-reporting the same run through the command
    // handler no longer folds in the advisory: the handler assesses the working tree
    // itself, so an up-to-date repo produces a clean comment.
    assert!(repo.run(fake, &["skills", "install"], &[]).status.success());
    let github2 = FakeGitHub::start();
    let output2 = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &[
            ("GITHUB_API_URL", github2.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    assert!(output2.status.success());
    let requests2 = github2.finish();
    let comment2 = requests2
        .iter()
        .find(|r| r.method == "POST" && r.path == "/repos/acme/app/issues/7/comments")
        .expect("a POST creating the sticky comment");
    assert!(
        !comment2.body.contains("[!WARNING]") && !comment2.body.contains("bundled agent skills"),
        "an up-to-date repo should not carry the skills advisory: {}",
        comment2.body
    );
}

#[test]
fn github_report_with_no_recorded_run_exits_zero_with_a_notice() {
    let Some(fake) = tooling() else { return };

    // A repo whose private data dir holds no runs: we never ran `bastion review`.
    let repo = TestRepo::new(&registry(&[
        Reviewer::new("unused", "claude-code", "gate").behavior("pass")
    ]));

    // Reporting with nothing persisted must not fail the step (it would pile a second
    // error on top of whatever upstream failure left no run). It prints a notice and
    // exits 0. No GitHub is contacted, so no fake server is needed.
    let output = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &[("GITHUB_TOKEN", "ghs-fake-token")],
    );
    assert!(
        output.status.success(),
        "missing-run report should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nothing to report"),
        "expected a 'nothing to report' notice; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
