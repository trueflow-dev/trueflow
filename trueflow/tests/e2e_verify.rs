use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::path::Path;

mod common;
use common::*;

#[test]
fn test_verify_unsigned_records() -> Result<()> {
    let repo = TestRepo::new("verify_unsigned")?;

    let record = serde_json::json!({
        "id": "unsigned",
        "version": 1,
        "fingerprint": "deadbeef",
        "check": "review",
        "verdict": "approved",
        "identity": { "type": "email", "email": "test@example.com" },
        "repo_ref": { "type": "vcs", "system": "git", "revision": "deadbeef" },
        "block_state": "committed",
        "timestamp": 1234,
        "path_hint": null,
        "line_hint": null,
        "note": null,
        "tags": null,
        "attestations": null
    });

    write_reviews(
        &repo.path.join(".trueflow").join("reviews.jsonl"),
        &[record],
    )?;

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

    let record = serde_json::json!({
        "id": "invalid",
        "version": 1,
        "fingerprint": "deadbeef",
        "check": "review",
        "verdict": "approved",
        "identity": { "type": "email", "email": "test@example.com" },
        "repo_ref": { "type": "vcs", "system": "git", "revision": "deadbeef" },
        "block_state": "committed",
        "timestamp": 1234,
        "path_hint": null,
        "line_hint": null,
        "note": null,
        "tags": null,
        "attestations": [
            {
                "kind": "PGP",
                "canonicalization": "JCS_V1",
                "signature": "invalid",
                "public_key": "invalid"
            }
        ]
    });

    write_reviews(
        &repo.path.join(".trueflow").join("reviews.jsonl"),
        &[record],
    )?;

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

fn write_reviews(path: &Path, records: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut content = String::new();
    for record in records {
        content.push_str(&serde_json::to_string(record)?);
        content.push('\n');
    }
    fs::write(path, content)?;
    Ok(())
}
