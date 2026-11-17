use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::*;

fn git_rev_parse_head(repo: &TestRepo) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo.path)
        .output()?;
    anyhow::ensure!(out.status.success(), "git rev-parse HEAD failed");
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

#[test]
fn reset_removes_only_current_user_records_for_current_head_and_supports_dry_run_and_json()
-> Result<()> {
    let repo = TestRepo::new("reset_user_and_rev_scoping")?;
    repo.git(&["config", "user.email", "alice@example.com"])?;
    repo.write(
        "src/lib.rs",
        "pub fn v1_kept() -> i32 { 1 }\n\npub fn v1_deleted_later() -> i32 { 2 }\n",
    )?;
    repo.commit_all("Commit 1")?;
    let rev1 = git_rev_parse_head(&repo)?;

    let scan1 = json_array(&repo.run(&["scan", "--json"])?)?;
    let b1_hash = scan1[0]["blocks"][0]["hash"]
        .as_str()
        .context("b1 hash")?
        .to_string();
    let b1_line = scan1[0]["blocks"][0]["start_line"]
        .as_u64()
        .context("b1 line")?
        .to_string();
    let deleted_hash = scan1[0]["blocks"][1]["hash"]
        .as_str()
        .context("deleted hash")?
        .to_string();
    let deleted_line = scan1[0]["blocks"][1]["start_line"]
        .as_u64()
        .context("deleted line")?
        .to_string();

    // Alice approves `v1_kept` on rev1 (historical review: MUST survive reset at rev2).
    repo.run(&[
        "mark",
        "--fingerprint",
        &b1_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--line",
        &b1_line,
    ])?;

    // Advance to Commit 2 (`rev2`): replace `v1_deleted_later` with `v2_added_a` and `v2_added_b`.
    repo.write(
        "src/lib.rs",
        "pub fn v1_kept() -> i32 { 1 }\n\npub fn v2_added_a() -> i32 { 10 }\n\npub fn v2_added_b() -> i32 { 20 }\n",
    )?;
    repo.commit_all("Commit 2")?;
    let rev2 = git_rev_parse_head(&repo)?;
    assert_ne!(rev1, rev2);

    let scan2 = json_array(&repo.run(&["scan", "--json"])?)?;
    let blocks2 = scan2[0]["blocks"].as_array().context("blocks2")?;
    let a_hash = blocks2[1]["hash"].as_str().context("a hash")?.to_string();
    let a_line = blocks2[1]["start_line"]
        .as_u64()
        .context("a line")?
        .to_string();
    let b_hash = blocks2[2]["hash"].as_str().context("b hash")?.to_string();
    let b_line = blocks2[2]["start_line"]
        .as_u64()
        .context("b line")?
        .to_string();

    // Bob approves `v2_added_b` on rev2 (other user on current rev: MUST survive Alice's reset).
    repo.git(&["config", "user.email", "bob@example.com"])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &b_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--line",
        &b_line,
    ])?;

    // Switch back to Alice on rev2: Alice approves `v2_added_a` AND approves deletion of `v1_deleted_later` in HEAD (`rev2`).
    repo.git(&["config", "user.email", "alice@example.com"])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &a_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--line",
        &a_line,
    ])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &deleted_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--line",
        &deleted_line,
        "--deleted-in",
        "HEAD",
    ])?;

    let before_log = json_array(&repo.run(&["log", "--all", "--json"])?)?;
    assert_eq!(before_log.len(), 4);

    // `--dry-run --json` reports 2 would-be removed records and 2 kept without mutating store.
    let dry_json: Value = serde_json::from_str(&repo.run(&["reset", "--dry-run", "--json"])?)?;
    assert_eq!(dry_json["dry_run"].as_bool(), Some(true));
    assert_eq!(dry_json["removed_count"].as_u64(), Some(2));
    assert_eq!(dry_json["remaining_count"].as_u64(), Some(2));
    assert_eq!(dry_json["revision"].as_str(), Some(rev2.as_str()));

    let still_four = json_array(&repo.run(&["log", "--all", "--json"])?)?;
    assert_eq!(still_four.len(), 4);

    // Real `reset` deletes Alice's 2 records on rev2 and keeps Alice's rev1 record + Bob's rev2 record.
    let reset_text = repo.run(&["reset"])?;
    assert!(
        reset_text.contains("Removed 2 review record(s)"),
        "unexpected reset output: {reset_text}"
    );

    let after_log = json_array(&repo.run(&["log", "--all", "--json"])?)?;
    assert_eq!(after_log.len(), 2);
    assert!(after_log.iter().any(|rec| {
        rec["identity"]["email"].as_str() == Some("bob@example.com")
            && rec["repo_ref"]["revision"].as_str() == Some(rev2.as_str())
    }));
    assert!(after_log.iter().any(|rec| {
        rec["identity"]["email"].as_str() == Some("alice@example.com")
            && rec["repo_ref"]["revision"].as_str() == Some(rev1.as_str())
    }));

    Ok(())
}

#[test]
fn reset_reconciles_feedback_cursor_so_subsequent_feedback_since_last_succeeds() -> Result<()> {
    let repo = TestRepo::new("reset_reconciles_feedback_cursor")?;
    repo.git(&["config", "user.email", "alice@example.com"])?;
    repo.write(
        "src/lib.rs",
        "pub fn base_fn() -> i32 {\n    1\n}\n\npub fn next_fn() -> i32 {\n    2\n}\n",
    )?;
    repo.commit_all("Commit 1")?;

    let scan1 = json_array(&repo.run(&["scan", "--json"])?)?;
    let base_hash = scan1[0]["blocks"][0]["hash"]
        .as_str()
        .context("base hash")?
        .to_string();
    let base_line = scan1[0]["blocks"][0]["start_line"]
        .as_u64()
        .context("base line")?
        .to_string();

    // Record a comment on Commit 1 and advance feedback cursor (`--since last`).
    repo.run(&[
        "mark",
        "--fingerprint",
        &base_hash,
        "--verdict",
        "comment",
        "--path",
        "src/lib.rs",
        "--line",
        &base_line,
        "--note",
        "commit 1 comment",
    ])?;
    let first_export = repo.run(&["feedback", "--since", "last", "--format", "json"])?;
    let first_items = json_array(&first_export)?;
    assert_eq!(first_items.len(), 1);
    assert!(repo.path.join(".trueflow/feedback.cursor").exists());

    // Advance to Commit 2, record a comment by Bob and a comment by Alice, export only `--only struct`
    // (delivering 0 comments, leaving both in `pending_record_ids` inside the advanced cursor checkpoint!),
    // then run `trueflow reset` as Alice!
    repo.write(
        "src/lib.rs",
        "pub fn base_fn() -> i32 {\n    1\n}\n\npub fn next_fn() -> i32 {\n    3\n}\n",
    )?;
    repo.commit_all("Commit 2")?;

    let scan2 = json_array(&repo.run(&["scan", "--json"])?)?;
    let next_hash = scan2[0]["blocks"][1]["hash"]
        .as_str()
        .context("next hash")?
        .to_string();
    let next_line = scan2[0]["blocks"][1]["start_line"]
        .as_u64()
        .context("next line")?
        .to_string();

    repo.git(&["config", "user.email", "bob@example.com"])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &next_hash,
        "--verdict",
        "comment",
        "--path",
        "src/lib.rs",
        "--line",
        &next_line,
        "--note",
        "bob pending comment",
    ])?;

    repo.git(&["config", "user.email", "alice@example.com"])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &next_hash,
        "--verdict",
        "comment",
        "--path",
        "src/lib.rs",
        "--line",
        &next_line,
        "--note",
        "alice comment to reset",
    ])?;

    // Advance checkpoint across all 3 records while keeping function comments pending (`--only struct`).
    let struct_only = json_array(&repo.run(&[
        "feedback", "--since", "last", "--only", "struct", "--format", "json",
    ])?)?;
    assert!(struct_only.is_empty());

    // Reset Alice's Commit 2 record.
    repo.run(&["reset"])?;

    // Now `feedback --since last --format json` MUST succeed without cursor digest errors
    // and deliver Bob's surviving pending comment while not re-delivering Commit 1's comment!
    let after_reset_export =
        json_array(&repo.run(&["feedback", "--since", "last", "--format", "json"])?)?;
    assert_eq!(after_reset_export.len(), 1, "{after_reset_export:?}");
    let reviews = after_reset_export[0]["reviews"]
        .as_array()
        .context("reviews array")?;
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["note"].as_str(), Some("bob pending comment"));

    Ok(())
}

#[test]
fn reset_errors_cleanly_on_unborn_head() -> Result<()> {
    let repo = TestRepo::new("reset_unborn_head")?;
    repo.write("src/lib.rs", "pub fn unborn() {}\n")?;

    let stderr = repo.run_err(&["reset"])?;
    assert!(
        stderr.contains("HEAD"),
        "expected unborn HEAD error message, got: {stderr}"
    );
    Ok(())
}
