use anyhow::Result;

use trueflow_test_support::TestRepo;

#[test]
fn test_repo_review_summary_collects_in_process_review() -> Result<()> {
    let repo = TestRepo::new("review_summary_helper")?;
    repo.write("src/lib.rs", "pub fn helper() {}\n")?;

    let summary = repo.review_summary(
        trueflow::commands::review::ReviewRequest::AllFiles,
        &[],
        &[],
    )?;

    assert!(
        summary
            .files
            .iter()
            .any(|file| file.path.as_str() == "src/lib.rs")
    );
    Ok(())
}

#[test]
fn test_repo_scan_without_cache_returns_scanner_results() -> Result<()> {
    let repo = TestRepo::new("scan_without_cache_helper")?;
    repo.write("src/lib.rs", "pub fn helper() {}\n")?;

    let scan = repo.scan_without_cache()?;

    assert_eq!(
        scan.cache.read,
        trueflow::scanner::ScanCacheReadStatus::Disabled
    );
    assert_eq!(
        scan.cache.write,
        trueflow::scanner::ScanCacheWriteStatus::Disabled
    );
    assert!(
        scan.files
            .iter()
            .any(|file| file.path.as_str() == "src/lib.rs")
    );
    Ok(())
}

#[test]
fn test_fixture_errors_when_missing() -> Result<()> {
    let err = match TestRepo::fixture("missing_fixture_name") {
        Ok(_) => anyhow::bail!("expected missing fixture lookup to fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("test fixture not found:"));
    Ok(())
}
