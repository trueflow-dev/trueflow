use anyhow::{Context, Result};

mod common;
use common::*;

#[test]
fn test_empty_repo() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    // 1. Review default (should match nothing if clean, or nothing if empty)
    let output = repo.run(&["review", "--json"])?;

    // Parse output
    let files = json_array(&output)?;
    assert!(files.is_empty());

    // 2. Review all
    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty());

    Ok(())
}

#[test]
fn test_basic_changes() -> Result<()> {
    let repo = TestRepo::fixture("basic_changes")?;

    // 1. Review All
    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;

    // Should find src/main.rs
    assert_eq!(files.len(), 1, "Should find exactly 1 file");
    let file_obj = &files[0];
    let path = file_obj["path"].as_str().context("path")?;
    assert!(path.contains("src/main.rs"));

    let blocks = file_obj["blocks"].as_array().context("blocks")?;
    // main and helper functions
    assert!(blocks.len() >= 2, "Should have at least 2 blocks");

    // Check for semantic kinds
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();

    assert!(kinds.contains(&"function"));

    Ok(())
}

#[test]
fn test_dirty_tree_filtering() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    // 1. Create a clean file
    repo.write("clean.rs", "fn clean() {}")?;
    repo.add("clean.rs")?;
    repo.commit("Add clean file")?;

    // 2. Create a dirty file (committed first, then modified)
    repo.write("dirty.rs", "fn dirty_v1() {}")?;
    repo.add("dirty.rs")?;
    repo.commit("Add dirty file")?;

    // Modify it
    repo.write("dirty.rs", "fn dirty_v2() {}")?;

    // 3. Create a purely untracked file
    repo.write("untracked.rs", "fn untracked() {}")?;

    // 4. Run review (default = dirty only)
    let output = repo.run(&["review", "--json"])?;
    let files = json_array(&output)?;

    // Expect: dirty.rs and untracked.rs.
    // Expect NOT: clean.rs

    let paths: Vec<&str> = files
        .iter()
        .filter_map(|obj| obj["path"].as_str())
        .collect();

    // Check presence
    assert!(
        paths.iter().any(|p| p.contains("dirty.rs")),
        "dirty.rs should be present"
    );
    assert!(
        paths.iter().any(|p| p.contains("untracked.rs")),
        "untracked.rs should be present"
    );
    assert!(
        !paths.iter().any(|p| p.contains("clean.rs")),
        "clean.rs should NOT be present"
    );

    Ok(())
}

#[test]
fn test_mark_flow() -> Result<()> {
    // Reuse empty repo to start fresh
    let repo = TestRepo::fixture("empty")?;

    // 1. Create content
    repo.write("src/main.rs", "fn main() { println!(\"Review me\"); }")?;
    repo.add("src/main.rs")?; // Make it tracked so we can use default review if we wanted, but we use --all
    repo.commit("Add main")?;

    // 2. Get hash
    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    // 3. Mark Approved
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // 4. Verify gone
    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty(), "Should have no unreviewed files");

    // 5. Mark Rejected
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--quiet",
    ])?;

    // 6. Verify back
    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert!(!blocks.is_empty());
    let returned_hash = blocks[0]["hash"].as_str().context("hash")?;
    assert_eq!(returned_hash, hash);

    Ok(())
}

#[test]
fn test_feedback_export() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    // 1. Create content
    repo.write("src/lib.rs", "fn core() { }")?;
    repo.add("src/lib.rs")?;
    repo.commit("Add lib")?;

    // 2. Get hash
    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    // 3. Mark with Comment
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--note",
        "Needs optimization",
        "--quiet",
    ])?;

    // 4. Run Feedback
    let xml_output = repo.run(&["feedback", "--format", "xml"])?;

    // 5. Assertions
    assert!(xml_output.contains("<trueflow_feedback>"));
    assert!(xml_output.contains("path=\"src/lib.rs\""));
    assert!(xml_output.contains("verdict=\"rejected\""));
    assert!(xml_output.contains("<comment>Needs optimization</comment>"));
    assert!(xml_output.contains("<![CDATA[\nfn core() { }\n]]>"));

    Ok(())
}

#[test]
fn test_feedback_json_includes_non_review_check() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    repo.write("src/lib.rs", "fn core() { }")?;
    repo.add("src/lib.rs")?;
    repo.commit("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--check",
        "security",
        "--quiet",
    ])?;

    let output = repo.run(&["feedback", "--format", "json"])?;
    let feedback = json_array(&output)?;
    let entry = feedback.first().context("Expected feedback entry")?;

    let latest_verdict = entry["latest_verdict"].as_str().context("latest_verdict")?;
    assert_eq!(latest_verdict, "rejected");
    assert!(
        entry["file"]
            .as_str()
            .unwrap_or_default()
            .contains("src/lib.rs")
    );

    let reviews = entry["reviews"]
        .as_array()
        .context("Reviews should be array")?;
    let review = reviews.first().context("Expected review entry")?;
    let check = review["check"].as_str().context("check")?;
    let verdict = review["verdict"].as_str().context("verdict")?;
    assert_eq!(check, "security");
    assert_eq!(verdict, "rejected");

    Ok(())
}

#[test]
fn test_half_reviewed_blocks() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    repo.write("src/main.rs", "fn alpha() {}\n\nfn beta() {}\n")?;
    repo.add("src/main.rs")?;
    repo.commit("Add functions")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert!(blocks.len() >= 2, "Expected at least 2 blocks");

    let approved_hash = blocks[0]["hash"]
        .as_str()
        .context("Hash should be string")?;
    repo.run(&[
        "mark",
        "--fingerprint",
        approved_hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks_after = first_file_blocks(&output)?;

    assert_eq!(blocks_after.len(), blocks.len() - 1);
    assert!(
        !blocks_after
            .iter()
            .any(|block| block["hash"] == approved_hash)
    );

    Ok(())
}

#[test]
fn test_file_hash_approval_hides_blocks() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    repo.write("src/lib.rs", "pub fn alpha() {}\n")?;
    repo.add("src/lib.rs")?;
    repo.commit("Add lib")?;

    let output = repo.run(&["scan", "--json"])?;
    let file_hash = first_file_hash(&output)?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &file_hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty());

    Ok(())
}

#[test]
fn test_directory_hash_approval_hides_blocks() -> Result<()> {
    let repo = TestRepo::fixture("empty")?;

    repo.write("src/lib.rs", "pub fn alpha() {}\n")?;
    repo.write("src/utils.rs", "pub fn beta() {}\n")?;
    repo.commit_all("Add libs")?;

    let output = repo.run(&["scan", "--tree", "--json"])?;
    let tree = json(&output)?;
    let dir_hash = find_tree_hash(&tree, "src")?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &dir_hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty());

    Ok(())
}

#[test]
fn test_review_only_filters_block_kinds() -> Result<()> {
    let repo = TestRepo::fixture("only_filter")?;
    repo.write("src/lib.rs", "struct Alpha;\n\nfn beta() {}\n")?;

    let output = repo.run(&["review", "--all", "--only", "function", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();
    assert!(!kinds.is_empty());
    assert!(
        kinds
            .iter()
            .all(|kind| kind.eq_ignore_ascii_case("function"))
    );

    Ok(())
}

#[test]
fn test_review_config_only_filters_block_kinds() -> Result<()> {
    let repo = TestRepo::fixture("only_config")?;
    repo.write("trueflow.toml", "[review]\nonly = [\"struct\"]\n")?;
    repo.write("src/lib.rs", "struct Alpha;\n\nfn beta() {}\n")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();
    assert!(!kinds.is_empty());
    assert!(kinds.iter().all(|kind| kind.eq_ignore_ascii_case("struct")));

    Ok(())
}

#[test]
fn test_review_hides_imports_outside_lib_by_default() -> Result<()> {
    let repo = TestRepo::fixture("hide_imports_default")?;
    repo.write(
        "src/main.rs",
        "use std::fmt;\n\nmod helpers;\n\nfn main() {}\n",
    )?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();
    assert!(!kinds.iter().any(|kind| {
        kind.eq_ignore_ascii_case("import")
            || kind.eq_ignore_ascii_case("imports")
            || kind.eq_ignore_ascii_case("module")
            || kind.eq_ignore_ascii_case("modules")
    }));

    Ok(())
}

#[test]
fn test_review_keeps_imports_in_lib_rs() -> Result<()> {
    let repo = TestRepo::fixture("imports_in_lib")?;
    repo.write(
        "src/lib.rs",
        "use std::fmt;\n\nmod helpers;\n\nstruct Alpha;\n\nfn beta() {}\n",
    )?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();
    assert!(kinds.iter().any(|kind| {
        kind.eq_ignore_ascii_case("import")
            || kind.eq_ignore_ascii_case("imports")
            || kind.eq_ignore_ascii_case("module")
            || kind.eq_ignore_ascii_case("modules")
    }));

    Ok(())
}

#[test]
fn test_review_only_includes_imports_when_filtered() -> Result<()> {
    let repo = TestRepo::fixture("imports_only_filter")?;
    repo.write("src/main.rs", "use std::fmt;\n\nfn main() {}\n")?;

    let output = repo.run(&["review", "--all", "--only", "import", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();
    assert!(!kinds.is_empty());
    assert!(kinds.iter().all(|kind| kind.eq_ignore_ascii_case("import")));

    Ok(())
}

#[test]
fn test_review_orders_imports_after_functions_in_lib() -> Result<()> {
    let repo = TestRepo::fixture("imports_order")?;
    repo.write(
        "src/lib.rs",
        "use std::fmt;\n\nstruct Alpha;\n\nfn beta() {}\n",
    )?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();
    let import_pos = kinds
        .iter()
        .position(|kind| kind.eq_ignore_ascii_case("import"))
        .context("expected import")?;
    let struct_pos = kinds
        .iter()
        .position(|kind| kind.eq_ignore_ascii_case("struct"))
        .context("expected struct")?;
    let function_pos = kinds
        .iter()
        .position(|kind| kind.eq_ignore_ascii_case("function"))
        .context("expected function")?;
    assert!(import_pos > struct_pos);
    assert!(import_pos > function_pos);

    Ok(())
}
