//! Opt-in Akari handoff through the real binary.

use crate::fakes::*;
use crate::fixtures::*;

use bastion::store;

#[test]
fn akari_handoff_is_off_by_default() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("n", "pi", "gate")]));
    let run = repo.review(fake);
    assert_eq!(run.code, Some(0));

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    let meta_json = std::fs::read_to_string(layout.meta(&runs[0].run, "n")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert!(
        meta.get("akari").is_none(),
        "akari must be omitted from meta when handoff is off: {meta}"
    );
}

#[test]
fn akari_handoff_records_a_skip_when_enabled_and_no_native_file_exists() {
    let Some(fake) = tooling() else { return };
    let Some(akari) = fake_akari() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("n", "pi", "gate")]));
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_AKARI", "1"),
            ("BASTION_AKARI_BIN", akari.to_str().unwrap()),
        ],
    );
    assert_eq!(run.code, Some(0), "handoff must not change the verdict");

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    let meta_json = std::fs::read_to_string(layout.meta(&runs[0].run, "n")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(meta["akari"]["status"], "skipped");
}

#[test]
fn user_level_akari_yaml_enables_handoff() {
    let Some(fake) = tooling() else { return };
    let Some(akari) = fake_akari() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("n", "pi", "gate")]))
        .with_akari_settings("enabled: true\n");
    let run = repo.review_base(
        fake,
        "main",
        &[("BASTION_AKARI_BIN", akari.to_str().unwrap())],
    );
    assert_eq!(run.code, Some(0));

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    let meta_json = std::fs::read_to_string(layout.meta(&runs[0].run, "n")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(meta["akari"]["status"], "skipped");
}

#[test]
fn a_checkout_akari_yaml_does_not_enable_handoff() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("n", "pi", "gate")]));
    std::fs::write(repo.path().join("akari.yaml"), "enabled: true\n").unwrap();
    let run = repo.review(fake);
    assert_eq!(run.code, Some(0));

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    let meta_json = std::fs::read_to_string(layout.meta(&runs[0].run, "n")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert!(
        meta.get("akari").is_none(),
        "repository akari.yaml must not enable handoff: {meta}"
    );
}
