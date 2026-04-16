use anyhow::Result;

use trueflow_test_support::TestRepo;

#[test]
fn test_fixture_errors_when_missing() -> Result<()> {
    let err = match TestRepo::fixture("missing_fixture_name") {
        Ok(_) => anyhow::bail!("expected missing fixture lookup to fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("test fixture not found:"));
    Ok(())
}
