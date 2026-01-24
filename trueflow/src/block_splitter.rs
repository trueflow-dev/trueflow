use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::hashing::hash_str;
use anyhow::Result;
use log::info;
use regex::Regex;
use tree_sitter::Parser;

pub fn split(content: &str, lang: Language) -> Result<Vec<Block>> {
    info!(
        "block_splitter start (lang={:?}, bytes={})",
        lang,
        content.len()
    );
    match lang {
        Language::Markdown => {
            let blocks = split_markdown(content)?;
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        _ if lang.uses_text_fallback() => {
            let blocks = split_text(content);
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        _ => {}
    }

    let mut parser = Parser::new();

    // Select grammar based on language
    let language = match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
        Language::Markdown => None,
        _ => None,
    };

    let Some(language) = language else {
        info!("block_splitter unsupported language, returning empty blocks");
        info!("block_splitter done (blocks=0)");
        return Ok(Vec::new());
    };

    parser.set_language(&language)?;

    let tree = parser.parse(content, None).unwrap();
    let root = tree.root_node();
    let mut blocks = Vec::new();

    let mut cursor = root.walk();
    let mut last_end_byte = 0;

    // State for pending attributes/comments that should be attached to the next node
    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    // Iterate over children of root
    for child in root.children(&mut cursor) {
        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        let ts_kind = child.kind();

        // Check if this node is an attribute or comment that should be grouped
        let is_attribute = match lang {
            Language::Rust => {
                ts_kind == "attribute_item"
                    || ts_kind == "line_comment"
                    || ts_kind == "block_comment"
            }
            Language::Python => ts_kind == "decorator",
            _ => false,
        };

        if is_attribute {
            if pending_start.is_none() {
                // First attribute in a potential group. Handle gap prior to it.
                if start_byte > last_end_byte {
                    let gap = &content[last_end_byte..start_byte];
                    if !gap.trim().is_empty() {
                        blocks.push(create_block(
                            gap,
                            BlockKind::Gap,
                            content,
                            last_end_byte,
                            start_byte,
                        ));
                    }
                }
                pending_start = Some(start_byte);
            }
            pending_end = end_byte;
            continue;
        }

        // It is a "real" item

        // Determine the actual start byte for this block (including pending attributes)
        let block_start = if let Some(ps) = pending_start {
            ps
        } else {
            // No pending attributes, handle gap now
            if start_byte > last_end_byte {
                let gap = &content[last_end_byte..start_byte];
                if !gap.trim().is_empty() {
                    blocks.push(create_block(
                        gap,
                        BlockKind::Gap,
                        content,
                        last_end_byte,
                        start_byte,
                    ));
                }
            }
            start_byte
        };

        let node_content = &content[block_start..end_byte];
        let friendly_kind = map_kind(lang.clone(), ts_kind);

        blocks.push(create_block(
            node_content,
            friendly_kind,
            content,
            block_start,
            end_byte,
        ));

        last_end_byte = end_byte;
        pending_start = None;
        pending_end = 0;
    }

    // If we have pending attributes left at the end (e.g. trailing comments or attribute at EOF)
    if let Some(start) = pending_start {
        let node_content = &content[start..pending_end];
        blocks.push(create_block(
            node_content,
            BlockKind::Code,
            content,
            start,
            pending_end,
        ));
        last_end_byte = pending_end;
    }

    // Trailing gap
    if last_end_byte < content.len() {
        let gap = &content[last_end_byte..];
        if !gap.trim().is_empty() {
            blocks.push(create_block(
                gap,
                BlockKind::Gap,
                content,
                last_end_byte,
                content.len(),
            ));
        }
    }

    info!("block_splitter done (blocks={})", blocks.len());
    Ok(blocks)
}

fn split_markdown(content: &str) -> Result<Vec<Block>> {
    use pulldown_cmark::{Event, HeadingLevel, Parser, Tag};

    let mut blocks = Vec::new();
    let mut section_start = 0;
    let mut current_level = 0; // 0 = preamble

    // Iterate events
    // We only care about Heading Start to trigger splits.
    let parser = Parser::new(content).into_offset_iter();

    for (event, range) in parser {
        if let Event::Start(Tag::Heading(level, _, _)) = event {
            let level_val = match level {
                HeadingLevel::H1 => 1,
                HeadingLevel::H2 => 2,
                HeadingLevel::H3 => 3,
                HeadingLevel::H4 => 4,
                HeadingLevel::H5 => 5,
                HeadingLevel::H6 => 6,
            };

            // If new header is same or higher level (numerically lower or equal)
            if current_level > 0 && level_val <= current_level {
                let chunk = &content[section_start..range.start];
                if !chunk.trim().is_empty() {
                    blocks.push(create_block(
                        chunk,
                        BlockKind::Section,
                        content,
                        section_start,
                        range.start,
                    ));
                }
                section_start = range.start;
                current_level = level_val;
            } else if current_level == 0 {
                // End preamble
                if range.start > section_start {
                    let chunk = &content[section_start..range.start];
                    if !chunk.trim().is_empty() {
                        blocks.push(create_block(
                            chunk,
                            BlockKind::Preamble,
                            content,
                            section_start,
                            range.start,
                        ));
                    }
                }
                section_start = range.start;
                current_level = level_val;
            }
        }
    }

    // Flush
    if section_start < content.len() {
        let chunk = &content[section_start..];
        if !chunk.trim().is_empty() {
            blocks.push(create_block(
                chunk,
                if current_level == 0 {
                    BlockKind::Preamble
                } else {
                    BlockKind::Section
                },
                content,
                section_start,
                content.len(),
            ));
        }
    }

    Ok(blocks)
}

fn split_text(content: &str) -> Vec<Block> {
    let re = Regex::new(r"\n\s*\n").unwrap();
    let mut blocks = Vec::new();
    let mut start_offset = 0;

    for mat in re.find_iter(content) {
        let end_offset = mat.start();
        if start_offset < end_offset {
            let chunk = &content[start_offset..end_offset];
            if !chunk.is_empty() {
                blocks.push(create_block(
                    chunk,
                    BlockKind::Paragraph,
                    content,
                    start_offset,
                    end_offset,
                ));
            }
        }

        let gap_chunk = &content[mat.start()..mat.end()];
        blocks.push(create_block(
            gap_chunk,
            BlockKind::Gap,
            content,
            mat.start(),
            mat.end(),
        ));

        start_offset = mat.end();
    }

    if start_offset < content.len() {
        let chunk = &content[start_offset..];
        if !chunk.is_empty() {
            blocks.push(create_block(
                chunk,
                BlockKind::Paragraph,
                content,
                start_offset,
                content.len(),
            ));
        }
    }

    blocks
}

fn map_kind(lang: Language, kind: &str) -> BlockKind {
    match lang {
        Language::Rust => match kind {
            "function_item" => BlockKind::Function,
            "struct_item" => BlockKind::Struct,
            "enum_item" => BlockKind::Enum,
            "impl_item" => BlockKind::Impl,
            "mod_item" => BlockKind::Module,
            "use_declaration" => BlockKind::Import,
            "const_item" | "static_item" => BlockKind::Const,
            "macro_invocation" => BlockKind::Macro,
            _ => BlockKind::Code,
        },
        Language::Python => match kind {
            "function_definition" => BlockKind::Function,
            "class_definition" => BlockKind::Class,
            "import_statement" | "import_from_statement" => BlockKind::Import,
            "decorated_definition" => BlockKind::Decorator,
            _ => BlockKind::Code,
        },
        Language::JavaScript | Language::TypeScript => match kind {
            "function_declaration" => BlockKind::Function,
            "class_declaration" => BlockKind::Class,
            "import_statement" => BlockKind::Import,
            "export_statement" => BlockKind::Export,
            "variable_declaration" => BlockKind::Variable,
            "lexical_declaration" => BlockKind::Variable,
            _ => BlockKind::Code,
        },
        Language::Shell => match kind {
            "function_definition" => BlockKind::Function,
            "command" => BlockKind::Command,
            _ => BlockKind::Code,
        },
        _ => BlockKind::Code,
    }
}

fn create_block(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
) -> Block {
    let hash = hash_str(text);

    // Line mapping (byte -> line index)
    // Reusing the logic from previous implementation
    let (start_line, end_line) = byte_range_to_lines(full_source, start_byte, end_byte);

    Block {
        hash,
        content: text.to_string(),
        kind,
        start_line,
        end_line,
    }
}

fn byte_range_to_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let pre = &source[..start];
    let start_line = pre.lines().count();
    let start_line = if start > 0 && pre.ends_with('\n') {
        start_line
    } else {
        start_line.saturating_sub(1)
    };

    let mid = &source[start..end];
    let new_lines = mid.chars().filter(|&c| c == '\n').count();
    let end_line = start_line + new_lines + if mid.ends_with('\n') { 0 } else { 1 };

    (start_line, end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_block_hashes_match(blocks: &[Block]) {
        for block in blocks {
            assert_eq!(block.hash, hash_str(&block.content));
        }
    }

    fn assert_paragraph_split(language: Language) {
        let content = "Para 1.\n\nPara 2.";
        let blocks = split(content, language).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(blocks[1].kind, BlockKind::Gap);
        assert_eq!(blocks[2].kind, BlockKind::Paragraph);
        assert_eq!(blocks[0].content, "Para 1.");
        assert_eq!(blocks[1].content, "\n\n");
        assert_eq!(blocks[2].content, "Para 2.");
        let merged: String = blocks.into_iter().map(|block| block.content).collect();
        assert_eq!(merged, content);
    }

    #[test]
    fn test_split_markdown_headers() {
        let content = "# Section 1\nText.\n# Section 2\nMore text.";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Section);
        assert_eq!(blocks[0].content, "# Section 1\nText.\n");
        assert_eq!(blocks[1].kind, BlockKind::Section);
        assert_eq!(blocks[1].content, "# Section 2\nMore text.");
    }

    #[test]
    fn test_split_markdown_hierarchy() {
        let content = "# Root\n## Sub\n### SubSub\n# Root 2";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 2);
        // First block contains Root, Sub, SubSub
        assert_eq!(blocks[0].content, "# Root\n## Sub\n### SubSub\n");
        // Second block contains Root 2
        assert_eq!(blocks[1].content, "# Root 2");
    }

    #[test]
    fn test_split_text_paragraphs() {
        assert_paragraph_split(Language::Text);
    }

    #[test]
    fn test_split_toml_paragraphs() {
        assert_paragraph_split(Language::Toml);
    }

    #[test]
    fn test_split_nix_paragraphs() {
        assert_paragraph_split(Language::Nix);
    }

    #[test]
    fn test_split_just_paragraphs() {
        assert_paragraph_split(Language::Just);
    }

    #[test]
    fn test_split_rust_simple() {
        let content = "fn foo() {}\n\nstruct Bar;";
        let blocks = split(content, Language::Rust).unwrap();
        // Tree-sitter splitting is complex but should return items
        assert!(!blocks.is_empty());
    }

    #[test]
    fn test_block_hashes_match_content_rust() {
        let content = "use std::fmt;\n\nfn foo() {}\n";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(!blocks.is_empty());
        assert_block_hashes_match(&blocks);
        assert!(!blocks.iter().any(|block| block.kind == BlockKind::Gap));
    }

    #[test]
    fn test_block_hashes_match_content_markdown() {
        let content = "# Title\nParagraph text.\n";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block_hashes_match(&blocks);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_markdown_discards_whitespace_only_preamble() {
        let content = "\n\n# Title\nBody";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "# Title\nBody");
    }
}
