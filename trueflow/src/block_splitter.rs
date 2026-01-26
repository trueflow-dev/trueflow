use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::complexity;
use crate::hashing::hash_str;
use crate::text_split::split_by_paragraph_breaks;
use anyhow::{Context, Result};
use log::info;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

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
            let blocks = split_paragraphs(content, lang);
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

    let test_ranges = collect_test_ranges(&lang, &tree, content)?;

    // State for pending attributes/comments that should be attached to the next node
    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    // Iterate over children of root
    for child in root.children(&mut cursor) {
        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        let ts_kind = child.kind();
        let is_test = is_test_span(&test_ranges, crate::block::Span::new(start_byte, end_byte));

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
                            &lang,
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
                        &lang,
                    ));
                }
            }
            start_byte
        };

        let node_content = &content[block_start..end_byte];
        let mut block = create_block(
            node_content,
            map_kind(lang.clone(), ts_kind),
            content,
            block_start,
            end_byte,
            &lang,
        );
        if is_test {
            block.tags.push("test".to_string());
        }
        blocks.push(block);

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
            &lang,
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
                &lang,
            ));
        }
    }

    info!("block_splitter done (blocks={})", blocks.len());
    Ok(blocks)
}

fn split_markdown(content: &str) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .context("Failed to load markdown grammar")?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse markdown")?;
    let root = tree.root_node();

    let mut headings = Vec::new();
    collect_markdown_headings(root, content, &mut headings);
    headings.sort_by_key(|heading| heading.start);

    let mut blocks = Vec::new();
    let mut section_start = 0;
    let mut current_level = 0;

    for heading in headings {
        if current_level == 0 {
            if heading.start > section_start {
                let chunk = &content[section_start..heading.start];
                if !chunk.trim().is_empty() {
                    blocks.push(create_block(
                        chunk,
                        BlockKind::Preamble,
                        content,
                        section_start,
                        heading.start,
                        &Language::Markdown,
                    ));
                }
            }
            section_start = heading.start;
            current_level = heading.level;
            continue;
        }

        if heading.level <= current_level {
            let chunk = &content[section_start..heading.start];
            if !chunk.trim().is_empty() {
                blocks.push(create_block(
                    chunk,
                    BlockKind::Section,
                    content,
                    section_start,
                    heading.start,
                    &Language::Markdown,
                ));
            }
            section_start = heading.start;
            current_level = heading.level;
        }
    }

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
                &Language::Markdown,
            ));
        }
    }

    Ok(blocks)
}

fn split_paragraphs(content: &str, lang: Language) -> Vec<Block> {
    split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let kind = if is_gap {
            BlockKind::Gap
        } else {
            BlockKind::Paragraph
        };
        create_block(chunk, kind, content, start, end, &lang)
    })
}

#[derive(Debug, Clone)]
struct MarkdownHeading {
    start: usize,
    level: u8,
}

fn collect_markdown_headings(
    node: tree_sitter::Node<'_>,
    content: &str,
    headings: &mut Vec<MarkdownHeading>,
) {
    if let Some(level) = markdown_heading_level(node.kind(), node.start_byte(), content) {
        headings.push(MarkdownHeading {
            start: node.start_byte(),
            level,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_markdown_headings(child, content, headings);
    }
}

fn markdown_heading_level(kind: &str, start: usize, content: &str) -> Option<u8> {
    match kind {
        "atx_heading" => {
            let line = content.get(start..)?.lines().next()?;
            let level = line.chars().take_while(|ch| *ch == '#').count();
            if level > 0 {
                Some(level.min(6) as u8)
            } else {
                None
            }
        }
        "setext_heading" => {
            let line = content.get(start..)?.lines().next()?;
            if line.chars().all(|ch| ch == '=') {
                Some(1)
            } else if line.chars().all(|ch| ch == '-') {
                Some(2)
            } else {
                None
            }
        }
        _ => None,
    }
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
    lang: &Language,
) -> Block {
    let hash = hash_str(text);
    let complexity = complexity::calculate(text, lang.clone());

    // Line mapping (byte -> line index)
    // Reusing the logic from previous implementation
    let (start_line, end_line) = byte_range_to_lines(full_source, start_byte, end_byte);

    Block {
        hash,
        content: text.to_string(),
        kind,
        tags: Vec::new(),
        complexity,
        start_line,
        end_line,
    }
}

fn collect_test_ranges(
    lang: &Language,
    tree: &tree_sitter::Tree,
    source: &str,
) -> Result<Vec<crate::block::Span>> {
    let mut ranges: Vec<crate::block::Span> = Vec::new();
    match lang {
        Language::Rust => {
            let attr_query =
                Query::new(&tree_sitter_rust::LANGUAGE.into(), "(attribute_item) @attr")?;
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&attr_query, tree.root_node(), source.as_bytes());
            while let Some(match_) = matches.next() {
                for capture in match_.captures {
                    let name = &attr_query.capture_names()[capture.index as usize];
                    if *name != "attr" {
                        continue;
                    }
                    let attr_text = capture.node.utf8_text(source.as_bytes())?;
                    if attr_text.contains("#[test]")
                        && let Some(function_item) =
                            next_named_sibling_of_kind(capture.node, "function_item")
                    {
                        ranges.push(crate::block::Span::new(
                            function_item.start_byte(),
                            function_item.end_byte(),
                        ));
                    }
                    if attr_text.contains("cfg")
                        && attr_text.contains("test")
                        && let Some(mod_item) = next_named_sibling_of_kind(capture.node, "mod_item")
                    {
                        ranges.push(crate::block::Span::new(
                            mod_item.start_byte(),
                            mod_item.end_byte(),
                        ));
                    }
                }
            }
        }
        Language::Python => {
            let query = Query::new(
                &tree_sitter_python::LANGUAGE.into(),
                "(decorated_definition (decorator) @decor (function_definition name: (identifier) @name) @func)",
            )?;
            collect_python_test_ranges(&query, tree, source, &mut ranges)?;

            let query = Query::new(
                &tree_sitter_python::LANGUAGE.into(),
                "(function_definition name: (identifier) @name) @func",
            )?;
            collect_python_test_ranges(&query, tree, source, &mut ranges)?;
        }
        Language::JavaScript => {
            let query = Query::new(
                &tree_sitter_javascript::LANGUAGE.into(),
                "(call_expression function: (identifier) @name arguments: (arguments (arrow_function) @fn)) @call",
            )?;
            collect_js_test_ranges(&query, tree, source, &mut ranges)?;

            let query = Query::new(
                &tree_sitter_javascript::LANGUAGE.into(),
                "(call_expression function: (member_expression object: (identifier) @name)) @call",
            )?;
            collect_js_test_ranges(&query, tree, source, &mut ranges)?;
        }
        Language::TypeScript => {
            let query = Query::new(
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "(call_expression function: (identifier) @name arguments: (arguments (arrow_function) @fn)) @call",
            )?;
            collect_js_test_ranges(&query, tree, source, &mut ranges)?;

            let query = Query::new(
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "(call_expression function: (member_expression object: (identifier) @name)) @call",
            )?;
            collect_js_test_ranges(&query, tree, source, &mut ranges)?;
        }
        Language::Shell => {
            let query = Query::new(
                &tree_sitter_bash::LANGUAGE.into(),
                "(function_definition name: (word) @name) @func",
            )?;
            collect_shell_test_ranges(&query, tree, source, &mut ranges)?;
        }
        _ => {}
    }

    Ok(ranges)
}

fn next_named_sibling_of_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut current = node;
    while let Some(next) = current.next_named_sibling() {
        if next.kind() == kind {
            return Some(next);
        }
        current = next;
    }
    None
}

fn collect_python_test_ranges(
    query: &Query,
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<crate::block::Span>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut func_range = None;
        let mut decorator_text = None;
        for capture in match_.captures {
            let cap_name = &query.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "func" => func_range = Some((capture.node.start_byte(), capture.node.end_byte())),
                "decor" => {
                    decorator_text = Some(capture.node.utf8_text(source.as_bytes())?.to_string())
                }
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, func_range)
            && name.starts_with("test_")
        {
            ranges.push(crate::block::Span::new(range.0, range.1));
            continue;
        }
        if let (Some(decor_text), Some(range)) = (decorator_text, func_range)
            && decor_text.contains("test_")
        {
            ranges.push(crate::block::Span::new(range.0, range.1));
        }
    }
    Ok(())
}

fn collect_js_test_ranges(
    query: &Query,
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<crate::block::Span>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut call_range = None;
        for capture in match_.captures {
            let cap_name = &query.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "call" => call_range = Some((capture.node.start_byte(), capture.node.end_byte())),
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, call_range)
            && matches!(name.as_str(), "describe" | "it" | "test")
        {
            ranges.push(crate::block::Span::new(range.0, range.1));
        }
    }
    Ok(())
}

fn collect_shell_test_ranges(
    query: &Query,
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<crate::block::Span>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut func_range = None;
        for capture in match_.captures {
            let cap_name = &query.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "func" => func_range = Some((capture.node.start_byte(), capture.node.end_byte())),
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, func_range)
            && name.starts_with("test_")
        {
            ranges.push(crate::block::Span::new(range.0, range.1));
        }
    }
    Ok(())
}

fn is_test_span(ranges: &[crate::block::Span], block_span: crate::block::Span) -> bool {
    ranges.iter().any(|range| range.overlaps(&block_span))
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

    #[test]
    fn test_rust_test_detection() {
        let content = "#[test]
fn test_foo() {}
";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(!blocks.is_empty());
        let test_block = blocks.iter().find(|b| b.content.contains("fn test_foo"));
        assert!(test_block.is_some());
        assert!(test_block.unwrap().tags.contains(&"test".to_string()));
    }

    #[test]
    fn test_rust_cfg_test_module_tagging() {
        let content = "#[cfg(test)]
mod tests {
    #[test]
    fn test_inner() {}
}
";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(!blocks.is_empty());
        let module_block = blocks.iter().find(|b| b.content.contains("mod tests"));
        assert!(module_block.is_some());
        assert!(module_block.unwrap().tags.contains(&"test".to_string()));
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

    fn assert_block_hashes_match(blocks: &[Block]) {
        for block in blocks {
            let expected_hash = crate::hashing::hash_str(&block.content);
            assert_eq!(
                block.hash, expected_hash,
                "Hash mismatch for block kind {:?}:\nContent:\n{:?}",
                block.kind, block.content
            );
        }
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
