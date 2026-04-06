#![cfg(feature = "bench")]

#[path = "common/review_bench_support.rs"]
mod review_bench_support;

use anyhow::Result;
use review_bench_support::ReviewBenchRepo;

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
