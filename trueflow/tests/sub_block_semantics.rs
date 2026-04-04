use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter;
use trueflow::finder::fuzzy_find_block;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn expand_block_for_review_splitting(mut block: Block) -> Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

fn assert_subblock_kinds(
    path: &Path,
    ident: &str,
    language: Language,
    expected: &[BlockKind],
) -> Result<()> {
    let block = expand_block_for_review_splitting(fuzzy_find_block(path, ident)?);
    let sub_blocks = sub_splitter::split(&block, language)?;
    let kinds: Vec<BlockKind> = sub_blocks
        .iter()
        .filter(|sub| sub.kind != BlockKind::Gap)
        .map(|sub| sub.kind)
        .collect();

    assert_eq!(kinds, expected);
    Ok(())
}

#[test]
fn test_rust_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks/src/lib.rs");
    let expected = vec![
        BlockKind::FunctionSignature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(&file_path, "process_data", Language::Rust, &expected)
}

#[test]
fn test_python_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_py/src/lib.py");
    let expected = vec![
        BlockKind::FunctionSignature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(&file_path, "process_data", Language::Python, &expected)
}

#[test]
fn test_js_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_js/src/lib.js");
    let expected = vec![
        BlockKind::FunctionSignature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(&file_path, "processData", Language::JavaScript, &expected)
}

#[test]
fn test_ts_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_ts/src/lib.ts");
    let expected = vec![
        BlockKind::FunctionSignature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(&file_path, "processData", Language::TypeScript, &expected)
}

#[test]
fn test_swift_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_swift/Sources/App/Core.swift");
    let expected = vec![
        BlockKind::FunctionSignature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(&file_path, "processData", Language::Swift, &expected)
}

#[test]
fn test_function_subblocks_are_review_units() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks/src/lib.rs");
    let block = expand_block_for_review_splitting(fuzzy_find_block(&file_path, "process_data")?);

    let result = sub_splitter::split_result(&block, Language::Rust)?;

    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    assert!(
        result
            .blocks
            .iter()
            .any(|block| block.kind == BlockKind::FunctionSignature)
    );

    Ok(())
}

#[test]
fn test_small_function_stays_whole_under_threshold() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks/src/lib.rs");
    let block = fuzzy_find_block(&file_path, "process_data")?;

    let result = sub_splitter::split_result(&block, Language::Rust)?;

    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(result.blocks.len(), 1);
    assert_eq!(result.blocks[0].kind, BlockKind::Function);
    assert_eq!(result.blocks[0].content, block.content);

    Ok(())
}

#[test]
fn test_rust_impl_subblocks_match_top_level_impl_members() -> Result<()> {
    let content = "struct Foo;\n\nimpl Foo {\n    #[cfg(test)]\n    fn read_heavy(&self) {}\n\n    // limit\n    const MAX: usize = 1;\n}\n";
    let blocks = block_splitter::split(content, Language::Rust).blocks;
    let impl_block = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Impl)
        .context("expected impl block")?;

    let top_level_members: Vec<_> = blocks
        .iter()
        .filter(|block| matches!(block.kind, BlockKind::Method | BlockKind::Const))
        .map(|block| {
            (
                block.kind,
                block.hash.clone(),
                block.content.clone(),
                block.start_line,
                block.end_line,
            )
        })
        .collect();

    let large_impl_block = expand_block_for_review_splitting(impl_block.clone());
    let sub_members: Vec<_> = sub_splitter::split(&large_impl_block, Language::Rust)?
        .into_iter()
        .filter(|block| matches!(block.kind, BlockKind::Method | BlockKind::Const))
        .map(|block| {
            (
                block.kind,
                block.hash,
                block.content,
                block.start_line,
                block.end_line,
            )
        })
        .collect();

    assert_eq!(sub_members, top_level_members);
    Ok(())
}

#[test]
fn test_small_markdown_section_stays_whole() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_md/README.md");
    let content = std::fs::read_to_string(&file_path)?;

    let blocks = block_splitter::split(&content, Language::Markdown).blocks;
    let section = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Section)
        .unwrap();

    let section_result = sub_splitter::split_result(section, Language::Markdown)?;
    assert_eq!(section_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(section_result.blocks.len(), 1);
    assert_eq!(section_result.blocks[0].kind, BlockKind::Section);
    assert_eq!(section_result.blocks[0].content, section.content);

    Ok(())
}
