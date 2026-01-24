use anyhow::Result;
use std::path::PathBuf;

use trueflow::analysis::Language;
use trueflow::block::BlockKind;
use trueflow::block_splitter;
use trueflow::finder::fuzzy_find_block;
use trueflow::sub_splitter;

fn assert_subblock_kinds(
    path: PathBuf,
    ident: &str,
    language: Language,
    expected: &[BlockKind],
) -> Result<()> {
    let block = fuzzy_find_block(&path, ident)?;
    let sub_blocks = sub_splitter::split(&block, language)?;
    let kinds: Vec<BlockKind> = sub_blocks
        .iter()
        .filter(|sub| sub.kind != BlockKind::Gap)
        .map(|sub| sub.kind.clone())
        .collect();

    assert_eq!(kinds, expected);
    Ok(())
}

#[test]
fn test_rust_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks/src/lib.rs");
    let expected = vec![
        BlockKind::Signature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(file_path, "process_data", Language::Rust, &expected)
}

#[test]
fn test_python_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_py/src/lib.py");
    let expected = vec![
        BlockKind::Signature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(file_path, "process_data", Language::Python, &expected)
}

#[test]
fn test_js_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_js/src/lib.js");
    let expected = vec![
        BlockKind::Signature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(file_path, "processData", Language::JavaScript, &expected)
}

#[test]
fn test_ts_function_subblock_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_ts/src/lib.ts");
    let expected = vec![
        BlockKind::Signature,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
        BlockKind::Comment,
        BlockKind::CodeParagraph,
        BlockKind::CodeParagraph,
    ];
    assert_subblock_kinds(file_path, "processData", Language::TypeScript, &expected)
}

#[test]
fn test_markdown_subblocks_and_sentences() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/complex_blocks_md/README.md");
    let content = std::fs::read_to_string(&file_path)?;

    let blocks = block_splitter::split(&content, Language::Markdown)?;
    let section = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Section)
        .expect("Expected markdown section block");

    let sub_blocks = sub_splitter::split(section, Language::Markdown)?;
    let kinds: Vec<BlockKind> = sub_blocks
        .iter()
        .filter(|sub| sub.kind != BlockKind::Gap)
        .map(|sub| sub.kind.clone())
        .collect();

    assert_eq!(
        kinds,
        vec![
            BlockKind::Header,
            BlockKind::Paragraph,
            BlockKind::Header,
            BlockKind::Paragraph,
            BlockKind::ListItem,
            BlockKind::ListItem,
        ]
    );

    let paragraph = sub_blocks
        .iter()
        .find(|block| block.kind == BlockKind::Paragraph)
        .expect("Expected paragraph block");
    let sentence_blocks = sub_splitter::split(paragraph, Language::Markdown)?;
    let sentence_kinds: Vec<BlockKind> = sentence_blocks
        .iter()
        .filter(|sub| sub.kind != BlockKind::Gap)
        .map(|sub| sub.kind.clone())
        .collect();

    assert!(
        sentence_kinds
            .iter()
            .all(|kind| *kind == BlockKind::Sentence)
    );
    assert_eq!(sentence_kinds.len(), 2);

    Ok(())
}
