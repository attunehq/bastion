//! Conversation continuity across complete CLI review invocations.

use crate::fakes::*;
use crate::fixtures::*;

use bastion::verdict::Decision;

#[test]
fn a_rework_run_continues_the_prior_reviewer_conversation() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("continuity", "codex", "gate")
        .behavior("block")
        .env("FAKE_SUMMARY", "conversation continued")
        .env("FAKE_REQUIRE_RESUME_IF_FILE_EXISTS", "resume-required")]));
    let first = repo.review(fake);
    assert_eq!(first.code, Some(1));
    assert_eq!(first.resolved("continuity").1, "conversation continued");

    std::fs::write(repo.path().join("resume-required"), "armed\n").unwrap();
    let second = repo.review(fake);

    assert_eq!(second.code, Some(1));
    assert_eq!(second.resolved("continuity").0, Decision::Block);
    assert_eq!(second.resolved("continuity").1, "conversation continued");
}

#[test]
fn unavailable_conversation_state_falls_back_to_a_fresh_review() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("fallback", "codex", "gate")
        .behavior("block")
        .env("FAKE_SUMMARY", "fresh fallback completed")
        .env("FAKE_REJECT_RESUME_IF_FILE_EXISTS", "reject-next-resume")]));
    let first = repo.review(fake);
    assert_eq!(first.code, Some(1));

    let marker = repo.path().join("reject-next-resume");
    std::fs::write(&marker, "armed\n").unwrap();
    let second = repo.review(fake);

    assert_eq!(second.code, Some(1));
    assert_eq!(second.resolved("fallback").0, Decision::Block);
    assert_eq!(second.resolved("fallback").1, "fresh fallback completed");
    assert!(
        !marker.exists(),
        "the fake rejected exactly one resume attempt"
    );
}
