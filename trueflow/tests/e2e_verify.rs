use anyhow::Result;
mod common;
use common::*;

#[test]
fn test_verify_unsigned_records() -> Result<()> {
    let repo = TestRepo::new("verify_unsigned")?;

    let record = build_review_record(
        "deadbeef",
        ReviewRecordOverrides {
            id: Some("unsigned"),
            email: Some("test@example.com"),
            timestamp: Some(1234),
            ..Default::default()
        },
    );

    write_reviews_jsonl(&repo.path.join(".trueflow"), &[record])?;

    let output = repo.run_raw(&["verify", "--all"])?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Attested: 0"));
    assert!(stdout.contains("Unattested: 1"));
    assert!(stdout.contains("Invalid: 0"));

    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.trim().is_empty());

    Ok(())
}

#[test]
fn test_verify_invalid_attestation() -> Result<()> {
    let repo = TestRepo::new("verify_invalid")?;

    let attestations = serde_json::json!([
        {
            "kind": "PGP",
            "canonicalization": "JCS_V1",
            "signature": "invalid",
            "public_key": "invalid"
        }
    ]);
    let record = build_review_record(
        "deadbeef",
        ReviewRecordOverrides {
            id: Some("invalid"),
            email: Some("test@example.com"),
            timestamp: Some(1234),
            attestations: Some(attestations),
            ..Default::default()
        },
    );

    write_reviews_jsonl(&repo.path.join(".trueflow"), &[record])?;

    let output = repo.run_raw(&["verify", "--all"])?;
    assert!(!output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Attested: 0"));
    assert!(stdout.contains("Unattested: 0"));
    assert!(stdout.contains("Invalid: 1"));

    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Signature verification failed"));

    Ok(())
}
