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
fn log_empty_repo_reports_no_records_without_creating_store_dir() -> Result<()> {
    let repo = TestRepo::new("log_empty_repo")?;
    repo.write("src/lib.rs", "pub fn alpha() {}\n")?;
    repo.commit_all("Initial")?;
    assert!(!repo.path.join(".trueflow").exists());

    let stderr_log_out = repo.run(&["--logging-mode", "stderr", "log"])?;
    assert!(
        stderr_log_out.contains("No review records found"),
        "unexpected text output: {stderr_log_out}"
    );
    assert!(
        !repo.path.join(".trueflow").exists(),
        "read-only `trueflow --logging-mode stderr log` must not create .trueflow"
    );

    let text_out = repo.run(&["log"])?;
    assert!(
        text_out.contains("No review records found"),
        "unexpected text output: {text_out}"
    );
    assert!(
        !repo.path.join(".trueflow/reviews.jsonl").exists(),
        "read-only `trueflow log` must not create .trueflow/reviews.jsonl"
    );

    let json_out = repo.run(&["log", "--json"])?;
    let parsed = json_array(&json_out)?;
    assert!(parsed.is_empty());
    Ok(())
}

#[test]
fn log_defaults_to_latest_16_in_reverse_chronological_append_order_and_supports_all_and_limit()
-> Result<()> {
    let repo = TestRepo::new("log_ordering_and_limit")?;
    let mut source = String::new();
    for idx in 0..20 {
        source.push_str(&format!("pub fn fn_{idx:02}() -> usize {{ {idx} }}\n\n"));
    }
    repo.write("src/lib.rs", &source)?;
    repo.commit_all("Add 20 functions")?;

    let scanned = json_array(&repo.run(&["scan", "--json"])?)?;
    let blocks = scanned[0]["blocks"].as_array().context("blocks array")?;
    assert_eq!(blocks.len(), 20);

    // Append 20 records with controlled monotonically non-decreasing timestamps (including ties)
    // directly into `.trueflow/reviews.jsonl` so tie-breaking by append index is deterministic.
    let head = git_rev_parse_head(&repo)?;
    std::fs::create_dir_all(repo.path.join(".trueflow"))?;
    let mut jsonl = String::new();
    for (idx, block) in blocks.iter().enumerate() {
        let hash = block["hash"].as_str().context("block hash")?;
        let start_line = block["start_line"].as_u64().context("start_line")?;
        let timestamp = 1_700_000_000_i64 + i64::try_from(idx / 2)?;
        let record = serde_json::json!({
            "id": format!("rec-{idx:02}"),
            "version": 5,
            "target": { "kind": "block", "hash": hash },
            "check": "review",
            "verdict": "approved",
            "identity": { "type": "email", "email": "alice@example.com" },
            "repo_ref": { "type": "vcs", "system": "git", "revision": head },
            "block_state": "committed",
            "timestamp": timestamp,
            "path_hint": "src/lib.rs",
            "line_hint": start_line,
            "note": format!("note-{idx:02}"),
            "comment_scope": null,
            "comment_context": null,
            "tags": null
        });
        jsonl.push_str(&serde_json::to_string(&record)?);
        jsonl.push('\n');
    }
    std::fs::write(repo.path.join(".trueflow/reviews.jsonl"), jsonl)?;

    // Default `--limit` is 16: records 19 down to 4 in exact reverse append order.
    let default_json = json_array(&repo.run(&["log", "--json"])?)?;
    let default_ids: Vec<&str> = default_json
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default())
        .collect();
    let expected_default: Vec<String> = (4..20).rev().map(|idx| format!("rec-{idx:02}")).collect();
    assert_eq!(default_ids, expected_default);

    let default_text = repo.run(&["log"])?;
    assert!(
        default_text.contains("showing 16 of 20"),
        "expected truncation hint in default text output:\n{default_text}"
    );

    // `-n 3` returns newest 3.
    let top3_json = json_array(&repo.run(&["log", "-n", "3", "--json"])?)?;
    let top3_ids: Vec<&str> = top3_json
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(top3_ids, vec!["rec-19", "rec-18", "rec-17"]);

    // `--all` returns all 20.
    let all_json = json_array(&repo.run(&["log", "--all", "--json"])?)?;
    assert_eq!(all_json.len(), 20);
    assert_eq!(all_json[0]["id"].as_str(), Some("rec-19"));
    assert_eq!(all_json[19]["id"].as_str(), Some("rec-00"));

    Ok(())
}

#[test]
fn log_filters_by_verdict_check_and_subdir_path_and_renders_metadata() -> Result<()> {
    let repo = TestRepo::new("log_filters_and_rendering")?;
    repo.write("pkg/src/a.rs", "pub fn alpha() -> i32 {\n    1\n}\n")?;
    repo.write("pkg/src/b.rs", "pub fn beta() -> i32 {\n    2\n}\n")?;
    repo.commit_all("Initial")?;

    let scanned = json_array(&repo.run(&["scan", "--json"])?)?;
    let find_block = |target_path: &str| -> Result<(String, String)> {
        let file = scanned
            .iter()
            .find(|entry| entry["path"].as_str() == Some(target_path))
            .with_context(|| format!("missing scanned file {target_path}"))?;
        let block = &file["blocks"][0];
        Ok((
            block["hash"].as_str().context("hash")?.to_string(),
            block["start_line"]
                .as_u64()
                .context("start_line")?
                .to_string(),
        ))
    };
    let (hash_a, line_a) = find_block("pkg/src/a.rs")?;
    let (hash_b, line_b) = find_block("pkg/src/b.rs")?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &hash_a,
        "--verdict",
        "approved",
        "--path",
        "pkg/src/a.rs",
        "--line",
        &line_a,
    ])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash_b,
        "--verdict",
        "comment",
        "--check",
        "security",
        "--path",
        "pkg/src/b.rs",
        "--line",
        &line_b,
        "--deleted-in",
        "HEAD",
        "--note",
        "check auth boundary\nsecond line",
    ])?;

    let security_only: Vec<Value> =
        json_array(&repo.run(&["log", "--check", "security", "--json"])?)?;
    assert_eq!(security_only.len(), 1);
    assert_eq!(security_only[0]["verdict"].as_str(), Some("comment"));

    let approved_only: Vec<Value> =
        json_array(&repo.run(&["log", "--verdict", "approved", "--json"])?)?;
    assert_eq!(approved_only.len(), 1);
    assert_eq!(approved_only[0]["path_hint"].as_str(), Some("pkg/src/a.rs"));

    // Running from subdirectory `pkg` with relative `--path src/b.rs` resolves to `pkg/src/b.rs`.
    let subdir = repo.path.join("pkg");
    let subdir_filtered =
        json_array(&repo.run_in(&["log", "--path", "src/b.rs", "--json"], &subdir)?)?;
    assert_eq!(subdir_filtered.len(), 1);
    assert_eq!(
        subdir_filtered[0]["path_hint"].as_str(),
        Some("pkg/src/b.rs")
    );

    let text = repo.run(&["log"])?;
    assert!(text.contains("comment"), "missing verdict in:\n{text}");
    assert!(text.contains("security"), "missing check in:\n{text}");
    assert!(
        text.contains("pkg/src/b.rs:L1"),
        "missing 1-based path:line locator in:\n{text}"
    );
    assert!(
        text.contains("[deleted in "),
        "missing deletion marker in:\n{text}"
    );
    assert!(
        text.contains("check auth boundary") && text.contains("second line"),
        "missing multiline note in:\n{text}"
    );

    Ok(())
}
