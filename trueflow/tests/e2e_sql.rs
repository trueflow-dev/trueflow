use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

use trueflow_test_support::*;

use trueflow::analysis::Language;
use trueflow::block::BlockKind;
use trueflow::block_splitter::{self, BlockSplitStrategy};

fn non_gap_kinds(blocks: &[Value]) -> Vec<&str> {
    blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !is_gap(kind))
        .collect()
}

fn path_matches(file: &Value, expected: &str) -> bool {
    file["path"]
        .as_str()
        .is_some_and(|path| path.trim_start_matches("./") == expected)
}

fn find_block<'a>(blocks: &'a [Value], needle: &str) -> Result<&'a Value> {
    blocks
        .iter()
        .find(|block| {
            block["content"]
                .as_str()
                .is_some_and(|content| content.contains(needle))
        })
        .with_context(|| format!("missing block containing {needle:?}"))
}

#[test]
fn test_sql_fixture_detects_language_and_structural_statement_blocks() -> Result<()> {
    let repo = TestRepo::fixture("sql_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let schema_file = files
        .iter()
        .find(|file| path_matches(file, "schema.sql"))
        .context("missing scan output for schema.sql")?;
    assert_eq!(schema_file["language"].as_str(), Some("Sql"));

    let schema_blocks = schema_file["blocks"]
        .as_array()
        .context("schema blocks should be array")?;
    let schema_kinds = non_gap_kinds(schema_blocks);

    assert!(schema_kinds.contains(&"struct"), "kinds={schema_kinds:?}");
    assert!(
        schema_kinds.contains(&"enum") || schema_kinds.contains(&"type"),
        "kinds={schema_kinds:?}"
    );
    assert!(schema_kinds.contains(&"module"), "kinds={schema_kinds:?}");
    assert!(schema_kinds.contains(&"function"), "kinds={schema_kinds:?}");
    assert!(schema_kinds.contains(&"code"), "kinds={schema_kinds:?}");
    assert!(
        !schema_kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks in SQL fixture: {schema_kinds:?}"
    );

    assert_eq!(
        find_block(schema_blocks, "CREATE TABLE accounts")?["kind"].as_str(),
        Some("struct")
    );
    assert!(
        matches!(
            find_block(schema_blocks, "CREATE TYPE account_status")?["kind"].as_str(),
            Some("enum") | Some("type")
        ),
        "expected create type block to be enum/type: {schema_blocks:#?}"
    );
    assert_eq!(
        find_block(schema_blocks, "CREATE VIEW active_accounts")?["kind"].as_str(),
        Some("module")
    );
    assert_eq!(
        find_block(schema_blocks, "CREATE FUNCTION normalize_email")?["kind"].as_str(),
        Some("function")
    );
    assert_eq!(
        find_block(schema_blocks, "CREATE TRIGGER accounts_set_updated_at")?["kind"].as_str(),
        Some("function")
    );
    assert_eq!(
        find_block(schema_blocks, "ALTER TABLE accounts")?["kind"].as_str(),
        Some("struct")
    );
    assert_eq!(
        find_block(schema_blocks, "DROP VIEW IF EXISTS legacy_accounts")?["kind"].as_str(),
        Some("module")
    );
    for needle in [
        "SELECT id, email\nFROM active_accounts",
        "INSERT INTO accounts (id, email, status)",
        "UPDATE accounts\nSET status = 'disabled'",
        "DELETE FROM accounts\nWHERE status = 'disabled'",
    ] {
        assert_eq!(
            find_block(schema_blocks, needle)?["kind"].as_str(),
            Some("code")
        );
    }

    let reports_file = files
        .iter()
        .find(|file| path_matches(file, "reports.sql"))
        .context("missing scan output for reports.sql")?;
    assert_eq!(reports_file["language"].as_str(), Some("Sql"));

    let reports_blocks = reports_file["blocks"]
        .as_array()
        .context("reports blocks should be array")?;
    let reports_kinds = non_gap_kinds(reports_blocks);
    assert!(
        reports_kinds
            .iter()
            .all(|kind| *kind == "code" || *kind == "comment")
    );
    assert!(
        !reports_kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks in SQL reports fixture: {reports_kinds:?}"
    );

    Ok(())
}

#[test]
fn test_sql_top_level_split_is_statement_aligned_and_structured() -> Result<()> {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("example_repos/sql_support/schema.sql");
    let content = std::fs::read_to_string(&fixture)?;
    let result = block_splitter::split(&content, Language::Sql);

    assert_eq!(result.strategy, BlockSplitStrategy::Structured);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics={:?}",
        result.diagnostics
    );

    let non_gap_blocks = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(non_gap_blocks.len() >= 10, "blocks={:#?}", result.blocks);

    let create_view = non_gap_blocks
        .iter()
        .find(|block| block.content.contains("CREATE VIEW active_accounts"))
        .context("missing create view block")?;
    assert_eq!(create_view.kind, BlockKind::Module);
    assert!(create_view.content.contains("SELECT id, email, status"));
    assert!(create_view.content.trim_end().ends_with(';'));
    assert!(
        !create_view
            .content
            .contains("CREATE FUNCTION normalize_email")
    );

    let create_function = non_gap_blocks
        .iter()
        .find(|block| block.content.contains("CREATE FUNCTION normalize_email"))
        .context("missing create function block")?;
    assert_eq!(create_function.kind, BlockKind::Function);
    assert!(create_function.content.trim_end().ends_with(';'));
    assert!(
        create_function
            .content
            .contains("SELECT lower(trim(input));")
    );

    for needle in [
        "CREATE TABLE accounts",
        "CREATE TYPE account_status",
        "CREATE TRIGGER accounts_set_updated_at",
        "ALTER TABLE accounts",
        "DROP VIEW IF EXISTS legacy_accounts",
        "SELECT id, email\nFROM active_accounts",
        "INSERT INTO accounts (id, email, status)",
        "UPDATE accounts\nSET status = 'disabled'",
        "DELETE FROM accounts\nWHERE status = 'disabled'",
    ] {
        let block = non_gap_blocks
            .iter()
            .find(|block| block.content.contains(needle))
            .with_context(|| format!("missing statement block containing {needle:?}"))?;
        assert!(
            block.content.trim_end().ends_with(';'),
            "expected SQL statement block to retain its terminator: {block:#?}"
        );
    }

    Ok(())
}

#[test]
fn test_sql_named_routine_inspect_split_stays_conservative() -> Result<()> {
    let repo = TestRepo::fixture("sql_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let schema_file = files
        .iter()
        .find(|file| path_matches(file, "schema.sql"))
        .context("missing scan output for schema.sql")?;
    let blocks = schema_file["blocks"]
        .as_array()
        .context("schema blocks should be array")?;

    let function_hash = find_block(blocks, "CREATE FUNCTION normalize_email")?["hash"]
        .as_str()
        .context("hash should be string")?;

    let output = repo.run(&["inspect", "--fingerprint", function_hash, "--split"])?;
    let sub_blocks = json_array(&output)?;
    let kinds = block_kinds_without_gaps(&sub_blocks);

    assert_eq!(kinds, vec!["function"]);
    assert_eq!(sub_blocks.len(), 1, "sub_blocks={sub_blocks:#?}");

    Ok(())
}
