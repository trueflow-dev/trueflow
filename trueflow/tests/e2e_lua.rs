use anyhow::{Context, Result};
use std::path::PathBuf;

use trueflow_test_support::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn expand_block_for_review_splitting(mut block: Block) -> Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

#[test]
fn test_lua_fixture_scan_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("lua_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let lua_file = files
        .iter()
        .find(|file| file["path"].as_str() == Some("app.lua"))
        .context("missing scan output for app.lua")?;

    assert_eq!(lua_file["language"].as_str(), Some("Lua"));

    let blocks = lua_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);

    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "expected import-like block in lua fixture (kinds={kinds:?})"
    );

    for expected_kind in ["module", "function", "method", "const", "variable"] {
        assert!(
            kinds.contains(&expected_kind),
            "expected {expected_kind} block in lua fixture (kinds={kinds:?})"
        );
    }

    assert!(
        !kinds.contains(&"code"),
        "did not expect generic code blocks in lua fixture (kinds={kinds:?})"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in lua fixture (kinds={kinds:?})"
    );

    Ok(())
}

#[test]
fn test_lua_fixture_inspect_split_returns_method_review_units() -> Result<()> {
    let repo = TestRepo::fixture("lua_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let lua_file = files
        .iter()
        .find(|file| file["path"].as_str() == Some("app.lua"))
        .context("missing scan output for app.lua")?;
    let blocks = lua_file["blocks"]
        .as_array()
        .context("blocks should be array")?;

    let process_method = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("function Processor:process(values)"))
        })
        .context("expected Processor:process method block")?;
    let process_hash = process_method["hash"]
        .as_str()
        .context("hash should be string")?;

    let output = repo.run(&["inspect", "--fingerprint", process_hash, "--split"])?;
    let subblocks = json_array(&output)?;
    let kinds = block_kinds_without_gaps(&subblocks);

    assert_eq!(kinds.first().copied(), Some("FunctionSignature"));
    assert!(
        kinds
            .iter()
            .filter(|kind| **kind == "CodeParagraph")
            .count()
            >= 3,
        "expected multiple Lua code review units: {kinds:?}"
    );
    assert!(
        kinds.contains(&"comment"),
        "expected Lua comment review unit in split method: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect textual fallback review units: {kinds:?}"
    );

    Ok(())
}

#[test]
fn test_lua_fixture_sub_splitting_supports_assigned_functions_and_modules() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/lua_support/app.lua");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Lua).blocks;

    let assigned_function = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function
                    && block
                        .content
                        .contains("local test_helper = function(values)")
            })
            .cloned()
            .context("expected assigned Lua function block")?,
    );

    let assigned_function_result = sub_splitter::split_result(&assigned_function, Language::Lua)?;
    assert_eq!(
        assigned_function_result.semantics,
        SubSplitSemantics::ReviewUnits
    );
    let assigned_function_kinds = assigned_function_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        assigned_function_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert!(
        assigned_function_kinds
            .iter()
            .filter(|kind| **kind == BlockKind::CodeParagraph)
            .count()
            >= 2,
        "expected assigned Lua function to split into code review units: {assigned_function_kinds:?}"
    );

    let defaults_module = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Module && block.content.contains("local Defaults = {")
            })
            .cloned()
            .context("expected Defaults module block")?,
    );

    let defaults_result = sub_splitter::split_result(&defaults_module, Language::Lua)?;
    assert_eq!(defaults_result.semantics, SubSplitSemantics::ReviewUnits);
    let defaults_kinds = defaults_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        defaults_kinds.contains(&BlockKind::Const),
        "expected Defaults module review units to expose const fields: {defaults_kinds:?}"
    );
    assert!(
        defaults_kinds.contains(&BlockKind::Variable),
        "expected Defaults module review units to expose variable fields: {defaults_kinds:?}"
    );
    assert!(
        defaults_kinds.contains(&BlockKind::Module),
        "expected Defaults module review units to expose nested tables: {defaults_kinds:?}"
    );
    assert!(
        defaults_kinds.contains(&BlockKind::Function),
        "expected Defaults module review units to expose function fields: {defaults_kinds:?}"
    );
    assert!(
        defaults_kinds.contains(&BlockKind::Method),
        "expected Defaults module review units to expose self-style method fields: {defaults_kinds:?}"
    );

    let aliases_module = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Module
                    && block.content.trim_start().starts_with("aliases = {")
            })
            .cloned()
            .context("expected aliases module block")?,
    );

    let aliases_result = sub_splitter::split_result(&aliases_module, Language::Lua)?;
    assert_eq!(aliases_result.semantics, SubSplitSemantics::ReviewUnits);
    let aliases_kinds = aliases_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        aliases_kinds
            .iter()
            .filter(|kind| **kind == BlockKind::CodeParagraph)
            .count(),
        4,
        "expected Lua positional table header, entries, and trailer to remain reviewable: {aliases_kinds:?}"
    );
    assert!(
        aliases_kinds.contains(&BlockKind::Comment),
        "expected Lua positional table comment to remain reviewable: {aliases_kinds:?}"
    );

    Ok(())
}

#[test]
fn test_lua_function_sub_splitting_handles_long_comments_and_trailing_commas() -> Result<()> {
    let block = expand_block_for_review_splitting(Block::new(
        "handler = function(self)\r\n  --[[Long-form reviewer note.]]\r\n  return self\r\nend,\r\n"
            .to_string(),
        BlockKind::Function,
        0,
        4,
    ));

    let result = sub_splitter::split_result(&block, Language::Lua)?;
    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            BlockKind::FunctionSignature,
            BlockKind::Comment,
            BlockKind::CodeParagraph
        ],
        "expected long Lua comments and trailing commas to stay stable: {kinds:?}"
    );

    Ok(())
}

#[test]
fn test_lua_module_sub_splitting_handles_leading_comments() -> Result<()> {
    let block = expand_block_for_review_splitting(Block::new(
        "-- Module preface\nlocal M = {\n  value = 1,\n}\n".to_string(),
        BlockKind::Module,
        0,
        4,
    ));

    let result = sub_splitter::split_result(&block, Language::Lua)?;
    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&BlockKind::Variable),
        "expected leading-comment module block to still split structurally: {kinds:?}"
    );

    Ok(())
}

#[test]
fn test_lua_multi_value_returns_do_not_misclassify_as_module_or_import() {
    let import_like =
        block_splitter::split("return require(\"json\"), other\n", Language::Lua).blocks;
    assert_eq!(import_like.len(), 1);
    assert_eq!(import_like[0].kind, BlockKind::Export);

    let module_like = block_splitter::split("return {}, other\n", Language::Lua).blocks;
    assert_eq!(module_like.len(), 1);
    assert_eq!(module_like[0].kind, BlockKind::Export);

    let multi_assignment_module =
        block_splitter::split("local a, M = 1, {}\n", Language::Lua).blocks;
    assert_eq!(multi_assignment_module.len(), 1);
    assert_eq!(multi_assignment_module[0].kind, BlockKind::Variable);

    let multi_assignment_import = block_splitter::split(
        "local first, second = 1, require(\"json\")\n",
        Language::Lua,
    )
    .blocks;
    assert_eq!(multi_assignment_import.len(), 1);
    assert_eq!(multi_assignment_import[0].kind, BlockKind::Variable);
}
