#![cfg(feature = "bench")]

use anyhow::Result;
use trueflow_test_support::ReviewBenchRepo;

#[test]
fn test_full_review_bench_fixture_smoke() -> Result<()> {
    let repo = ReviewBenchRepo::fixture("review_bench_workspace")?;
    let summary = repo.full_review_summary()?;

    assert!(
        summary.files.len() >= 12,
        "expected a realistic fixture with at least 12 reviewable files, got {}",
        summary.files.len()
    );
    assert!(
        summary.total_blocks >= 40,
        "expected a realistic fixture with at least 40 review blocks, got {}",
        summary.total_blocks
    );

    Ok(())
}

#[test]
fn test_generated_main_diff_bench_fixture_smoke() -> Result<()> {
    let repo = ReviewBenchRepo::generated_main_diff("generated_main_diff_smoke", 100)?;
    let summary = repo.main_diff_review_summary()?;

    assert_eq!(summary.files.len(), 100);
    assert_eq!(
        summary.files.first().map(|file| file.path.as_str()),
        Some("src/generated_000.rs")
    );
    assert_eq!(
        summary.files.last().map(|file| file.path.as_str()),
        Some("src/generated_099.rs")
    );
    assert_eq!(summary.total_blocks, 100);

    Ok(())
}
