use anyhow::{Context, Result};

use trueflow::store::{BlockState, RepoRef};
use trueflow_test_support::{FeedbackScenario, ReviewRecordOverrides};

#[test]
fn feedback_scenario_review_block_uses_cli_record_defaults() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_harness_cli_defaults")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    let revision = scenario.commit_all("Initial")?;

    let record = scenario.review_block("src/lib.rs", "rejected")?;

    assert_eq!(record.block_state, BlockState::Committed);
    assert_eq!(
        record.path_hint.as_ref().map(|path| path.as_str()),
        Some("src/lib.rs")
    );
    assert_eq!(record.line_hint, Some(0));
    match &record.repo_ref {
        RepoRef::Vcs {
            revision: record_revision,
            ..
        } => {
            assert_eq!(record_revision.as_str(), revision);
        }
        RepoRef::Unknown => panic!("expected review_block to record the current git revision"),
    }

    Ok(())
}

#[test]
fn feedback_scenario_review_block_with_overrides_requires_explicit_patch() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_harness_patch")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("Initial")?;

    let record = scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("patched"),
            verdict: Some("rejected"),
            timestamp: Some(1234),
            ..Default::default()
        },
    )?;
    let records = scenario.reviews()?;
    let stored = records.last().context("expected stored review record")?;

    assert_eq!(record.id, "patched");
    assert_eq!(record.timestamp, 1234);
    assert_eq!(record.verdict.as_str(), "rejected");
    assert_eq!(stored.id, "patched");
    assert_eq!(stored.timestamp, 1234);
    assert_eq!(stored.verdict.as_str(), "rejected");

    Ok(())
}
