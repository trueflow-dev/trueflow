use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::*;

fn scan_blocks(repo: &TestRepo) -> Result<Vec<Value>> {
    let output = repo.run(&["scan", "--json"])?;
    json_array(&output)
}

#[test]
fn test_inspect_errors_on_missing_block() -> Result<()> {
    let repo = TestRepo::new("inspect_missing")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;

    let output = repo.run_raw(&["inspect", "--fingerprint", "deadbeef"])?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Block not found"));

    Ok(())
}

#[test]
fn test_inspect_errors_on_duplicate_fingerprint() -> Result<()> {
    let repo = TestRepo::new("inspect_duplicate")?;
    repo.write("src/lib.rs", "pub fn alpha() {}\n\npub fn alpha() {}\n")?;

    let blocks = scan_blocks(&repo)?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    let output = repo.run_raw(&["inspect", "--fingerprint", hash])?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Multiple blocks matched"));

    Ok(())
}

#[test]
fn inspect_full_hash_with_duplicate_content_is_resolvable() -> Result<()> {
    let repo = TestRepo::new("inspect_duplicate_paths")?;
    repo.write("src/a.rs", "fn dup() {}\n")?;
    repo.write("src/b.rs", "fn dup() {}\n")?;
    repo.commit_all("Add duplicates")?;

    let blocks = scan_blocks(&repo)?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;
    let start_line = blocks[0]["blocks"][0]["start_line"]
        .as_u64()
        .context("start_line")?
        .to_string();

    let output = repo.run_raw(&["inspect", "--fingerprint", hash])?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Multiple blocks matched"), "{stderr}");
    assert!(stderr.contains("--path"), "{stderr}");
    assert!(!stderr.contains("longer prefix"), "{stderr}");

    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--path",
        "src/b.rs",
        "--line",
        &start_line,
        "--quiet",
    ])?;

    let direct_verdict = |path: &str| -> Result<Value> {
        let output = repo.run(&[
            "inspect",
            "--fingerprint",
            hash,
            "--path",
            path,
            "--coverage",
        ])?;
        let inspected: Value = serde_json::from_str(&output)?;
        assert_eq!(inspected["block"]["hash"].as_str(), Some(hash));
        Ok(inspected["coverage"]["checks"]["review"]["direct_latest_verdict"].clone())
    };
    assert_eq!(direct_verdict("src/b.rs")?.as_str(), Some("approved"));
    assert_eq!(direct_verdict("src/a.rs")?, Value::Null);

    Ok(())
}

#[test]
fn inspect_path_from_subdirectory_resolves_like_mark_path() -> Result<()> {
    let repo = TestRepo::new("inspect_subdir_paths")?;
    repo.write("src/a.rs", "fn dup() {}\n")?;
    repo.write("src/b.rs", "fn dup() {}\n")?;
    repo.commit_all("Add duplicates")?;
    let subdir = repo.path.join("src");

    let blocks = scan_blocks(&repo)?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    repo.run_in(
        &[
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--path",
            "src/b.rs",
            "--quiet",
        ],
        &subdir,
    )?;

    // `mark --path` accepts both a repo-relative path and one relative to the
    // current directory; `inspect --path` must accept the same forms.
    let direct_verdict = |path: &str| -> Result<Value> {
        let output = repo.run_in(
            &[
                "inspect",
                "--fingerprint",
                hash,
                "--path",
                path,
                "--coverage",
            ],
            &subdir,
        )?;
        let inspected: Value = serde_json::from_str(&output)?;
        Ok(inspected["coverage"]["checks"]["review"]["direct_latest_verdict"].clone())
    };
    assert_eq!(direct_verdict("src/b.rs")?.as_str(), Some("approved"));
    assert_eq!(direct_verdict("b.rs")?.as_str(), Some("approved"));
    assert_eq!(direct_verdict("src/a.rs")?, Value::Null);

    let error = repo
        .run_in(&["inspect", "--fingerprint", hash], &subdir)
        .err()
        .context("duplicate content is ambiguous without --path")?
        .to_string();
    assert!(
        error.contains("src/a.rs line") && error.contains("src/b.rs line"),
        "ambiguity lists the repo-relative paths --path accepts: {error}"
    );

    Ok(())
}

#[test]
fn inspect_line_selects_duplicate_content_within_one_file() -> Result<()> {
    let repo = TestRepo::new("inspect_duplicate_lines")?;
    repo.write("src/lib.rs", "pub fn alpha() {}\n\npub fn alpha() {}\n")?;

    let blocks = scan_blocks(&repo)?;
    let file_blocks = blocks[0]["blocks"].as_array().context("blocks")?;
    let hash = file_blocks[0]["hash"].as_str().context("hash")?;
    let second_line = file_blocks
        .iter()
        .filter(|block| block["hash"].as_str() == Some(hash))
        .nth(1)
        .and_then(|block| block["start_line"].as_u64())
        .context("second duplicate start_line")?;

    let output = repo.run_raw(&["inspect", "--fingerprint", hash, "--path", "src/lib.rs"])?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--line"), "{stderr}");

    let output = repo.run(&[
        "inspect",
        "--fingerprint",
        hash,
        "--path",
        "src/lib.rs",
        "--line",
        &second_line.to_string(),
    ])?;
    let inspected: Value = serde_json::from_str(&output)?;
    assert_eq!(inspected["start_line"].as_u64(), Some(second_line));

    Ok(())
}

#[test]
fn test_inspect_split_preserves_order() -> Result<()> {
    let repo = TestRepo::new("inspect_split")?;
    let content = "fn main() {\n    part1();\n\n    part2();\n}\n";
    repo.write("src/main.rs", content)?;

    let blocks = scan_blocks(&repo)?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    let output = repo.run(&["inspect", "--fingerprint", hash, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;
    let reconstructed: String = sub_blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect();

    assert_eq!(
        reconstructed.trim_end_matches('\n'),
        content.trim_end_matches('\n')
    );

    Ok(())
}

#[test]
fn test_inspect_coverage_reports_direct_and_effective_review_facts() -> Result<()> {
    let repo = TestRepo::new("inspect_coverage")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let scan_output = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan_output)?;
    let file_hash = first_file_tree_hash(&scan_output)?;
    let file = files.first().context("expected file")?;
    let block = file["blocks"].as_array().context("blocks")?[0].clone();
    let block_hash = block["hash"].as_str().context("block hash")?;
    let start_line = block["start_line"]
        .as_u64()
        .context("block start line")?
        .to_string();

    repo.run(&[
        "mark",
        "--fingerprint",
        &file_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--quiet",
    ])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        block_hash,
        "--verdict",
        "approved",
        "--check",
        "security",
        "--path",
        "src/lib.rs",
        "--line",
        &start_line,
        "--quiet",
    ])?;

    let output = repo.run(&["inspect", "--fingerprint", block_hash, "--coverage"])?;
    let inspected: Value = serde_json::from_str(&output)?;

    assert_eq!(inspected["block"]["hash"].as_str(), Some(block_hash));
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["direct_latest_verdict"],
        Value::Null
    );
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["effective_latest_verdict"].as_str(),
        Some("approved")
    );
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["direct_identity_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["effective_identity_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        inspected["coverage"]["checks"]["security"]["direct_latest_verdict"].as_str(),
        Some("approved")
    );
    assert_eq!(
        inspected["coverage"]["checks"]["security"]["effective_latest_verdict"].as_str(),
        Some("approved")
    );
    assert_eq!(
        inspected["coverage"]["policies"]["single_review_effective"].as_bool(),
        Some(true)
    );
    assert_eq!(
        inspected["coverage"]["policies"]["single_review_direct"].as_bool(),
        Some(false)
    );

    Ok(())
}

#[test]
fn test_inspect_split_reports_absolute_byte_spans() -> Result<()> {
    let repo = TestRepo::new("inspect_absolute_byte_spans")?;
    let body = (0..34)
        .map(|index| format!("    let value_{index} = \"é\";\n"))
        .collect::<String>();
    let source = format!("fn main() {{\n{body}}}\n");
    repo.write("src/main.rs", &source)?;

    let blocks = scan_blocks(&repo)?;
    let function = blocks[0]["blocks"]
        .as_array()
        .context("expected scanned blocks")?
        .iter()
        .find(|block| block["kind"].as_str() == Some("function"))
        .context("expected function block")?;
    let hash = function["hash"].as_str().context("function hash")?;
    let output = repo.run(&["inspect", "--fingerprint", hash, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;
    assert!(!sub_blocks.is_empty());

    for block in sub_blocks {
        let start_byte = usize::try_from(block["start_byte"].as_u64().context("start byte")?)?;
        let end_byte = usize::try_from(block["end_byte"].as_u64().context("end byte")?)?;
        assert!(source.is_char_boundary(start_byte));
        assert!(source.is_char_boundary(end_byte));
        assert_eq!(block["content"].as_str(), source.get(start_byte..end_byte));
    }

    Ok(())
}

#[test]
fn inspect_split_from_subdirectory_discovers_repo_root() -> Result<()> {
    let repo = TestRepo::new("inspect_subdir_root")?;
    let body = (0..36)
        .map(|idx| format!("    step_{idx}();\n\n"))
        .collect::<String>();
    repo.write("lib/service.rs", &format!("pub fn run() {{\n{body}}}\n"))?;
    repo.write("cmd/main.rs", "fn main() {}\n")?;
    repo.commit_all("initial")?;

    let blocks = scan_blocks(&repo)?;
    let service_file = blocks
        .iter()
        .find(|file| file["path"].as_str() == Some("lib/service.rs"))
        .context("expected lib/service.rs")?;
    let run_fn = service_file["blocks"]
        .as_array()
        .context("blocks")?
        .iter()
        .find(|block| block["kind"].as_str() == Some("function"))
        .context("expected Rust function")?;
    let hash = run_fn["hash"].as_str().context("hash")?;

    let subdir = repo.path.join("cmd");
    let output = repo.run_in(&["inspect", "--fingerprint", hash, "--split"], &subdir)?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;
    assert!(
        sub_blocks.len() >= 2,
        "inspect --split from sibling subdir must find and split function: {sub_blocks:#?}"
    );
    Ok(())
}
