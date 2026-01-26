use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::hashing::hash_str;
use crate::text_split::{paragraph_break_regex, split_by_paragraph_breaks};
use anyhow::{Context, Result};
use log::info;
use tree_sitter::Parser;
use tree_sitter_md;

pub fn split(block: &Block, lang: Language) -> Result<Vec<Block>> {
    info!(
        "sub_splitter start (lang={:?}, kind={}, bytes={}, hash={})",
        lang,
        block.kind.as_str(),
        block.content.len(),
        block.hash
    );

    let blocks = match lang {
        Language::Markdown => split_markdown(block)?,
        Language::Text => split_sentences(block)?,
        Language::Toml | Language::Nix | Language::Just => split_code(block)?,
        Language::Rust if matches!(block.kind, BlockKind::Function | BlockKind::Method) => {
            split_rust_function(block)?
        }
        Language::Python if matches!(block.kind, BlockKind::Function | BlockKind::Method) => {
            split_python_function(block)?
        }
        Language::JavaScript | Language::TypeScript
            if matches!(
                block.kind,
                BlockKind::Function | BlockKind::Method | BlockKind::Export
            ) =>
        {
            split_js_function(block, lang)?
        }
        _ => split_code(block)?, // Default for Rust, Python, etc.
    };

    info!("sub_splitter done (blocks={})", blocks.len());
    Ok(blocks)
}

fn split_code(block: &Block) -> Result<Vec<Block>> {
    let content = &block.content;
    let blocks = split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let block_kind = if is_gap {
            BlockKind::Gap
        } else {
            classify_code_chunk(chunk)
        };
        create_sub_block_with_kind(block, chunk, start, end, block_kind)
    });
    Ok(blocks)
}

fn split_markdown_tree(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .context("Failed to load markdown grammar")?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse markdown")?;
    let root = tree.root_node();

    let mut spans = Vec::new();
    collect_markdown_spans(root, &mut spans);
    spans.sort_by_key(|span| span.start);

    let mut blocks = Vec::new();
    let mut last_end = 0;
    for span in spans {
        if span.start > last_end {
            let gap = &content[last_end..span.start];
            if !gap.is_empty() {
                blocks.push(create_sub_block_with_kind(
                    block,
                    gap,
                    last_end,
                    span.start,
                    BlockKind::Gap,
                ));
            }
        }

        let chunk = &content[span.start..span.end];
        blocks.push(create_sub_block_with_kind(
            block, chunk, span.start, span.end, span.kind,
        ));
        last_end = span.end;
    }

    if last_end < content.len() {
        let tail = &content[last_end..];
        if !tail.is_empty() {
            blocks.push(create_sub_block_with_kind(
                block,
                tail,
                last_end,
                content.len(),
                BlockKind::Gap,
            ));
        }
    }

    if blocks.is_empty() {
        return split_code(block);
    }

    Ok(blocks)
}

fn split_markdown_sentences(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut spans = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    let mut idx = 0;

    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if matches!(ch, '.' | '!' | '?') {
            let mut end = idx + 1;
            while end < bytes.len() && bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            spans.push((start, end));
            start = end;
            idx = end;
            continue;
        }
        idx += 1;
    }

    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }

    let mut blocks = Vec::new();
    for (start, end) in spans {
        let chunk = &content[start..end];
        if chunk.trim().is_empty() {
            continue;
        }
        blocks.push(create_sub_block_with_kind(
            block,
            chunk,
            start,
            end,
            BlockKind::Sentence,
        ));
    }

    if blocks.is_empty() {
        blocks.push(create_sub_block_with_kind(
            block,
            content,
            0,
            content.len(),
            BlockKind::Sentence,
        ));
    }

    Ok(blocks)
}

fn split_markdown(block: &Block) -> Result<Vec<Block>> {
    match block.kind {
        BlockKind::Paragraph | BlockKind::ListItem => split_markdown_sentences(block),
        _ => split_markdown_tree(block),
    }
}

fn split_sentences(block: &Block) -> Result<Vec<Block>> {
    split_markdown_sentences(block)
}

struct FunctionSplitConfig<'a> {
    language: tree_sitter::Language,
    function_kind: &'a str,
    body_kind: &'a str,
    signature_end: fn(&str, usize) -> usize,
    comment_kinds: &'a [&'a str],
    trim_closing_brace: bool,
}

fn split_rust_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        FunctionSplitConfig {
            language: tree_sitter_rust::LANGUAGE.into(),
            function_kind: "function_item",
            body_kind: "block",
            signature_end: signature_end_offset,
            comment_kinds: &["line_comment", "block_comment"],
            trim_closing_brace: true,
        },
    )
}

fn split_python_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        FunctionSplitConfig {
            language: tree_sitter_python::LANGUAGE.into(),
            function_kind: "function_definition",
            body_kind: "block",
            signature_end: signature_end_before_body,
            comment_kinds: &["comment", "line_comment", "block_comment"],
            trim_closing_brace: false,
        },
    )
}

fn split_js_function(block: &Block, lang: Language) -> Result<Vec<Block>> {
    let language = match lang {
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    };
    split_function_with_parser(
        block,
        FunctionSplitConfig {
            language,
            function_kind: "function_declaration",
            body_kind: "statement_block",
            signature_end: signature_end_offset,
            comment_kinds: &["comment", "line_comment", "block_comment"],
            trim_closing_brace: true,
        },
    )
}

fn split_function_with_parser(
    block: &Block,
    config: FunctionSplitConfig<'_>,
) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser.set_language(&config.language)?;

    let tree = parser
        .parse(&block.content, None)
        .context("Failed to parse function block")?;
    let root = tree.root_node();
    let Some(function_node) = find_named_descendant(root, config.function_kind) else {
        return split_code(block);
    };
    let Some(body_node) = find_named_descendant(function_node, config.body_kind) else {
        return split_code(block);
    };

    let mut blocks = Vec::new();
    let content = block.content.as_str();
    let signature_end = (config.signature_end)(content, body_node.start_byte());
    if signature_end > 0 {
        blocks.push(create_sub_block_with_kind(
            block,
            &content[..signature_end],
            0,
            signature_end,
            BlockKind::FunctionSignature,
        ));
    }

    let nodes = collect_body_nodes(body_node, config.comment_kinds);
    if nodes.is_empty() {
        return split_code(block);
    }

    let mut last_end = signature_end;
    let mut current_start: Option<usize> = None;
    let mut current_end = signature_end;
    let mut last_kind: Option<BlockKind> = None;

    for (idx, node) in nodes.iter().enumerate() {
        let start = node.start_byte();
        let gap = if start > last_end {
            &content[last_end..start]
        } else {
            ""
        };
        let gap_has_blank = paragraph_break_regex().is_match(gap);
        let gap_prefix_len = if gap_has_blank {
            gap_prefix_length(gap)
        } else {
            0
        };
        let leading_start = last_end + gap_prefix_len;

        let mut end = node.end_byte();
        if config.trim_closing_brace
            && idx == nodes.len().saturating_sub(1)
            && content[end..].trim() == "}"
        {
            end = content.len();
        }

        let node_kind = if config.comment_kinds.iter().any(|kind| *kind == node.kind()) {
            BlockKind::Comment
        } else {
            BlockKind::CodeParagraph
        };

        if (gap_has_blank
            || last_kind == Some(BlockKind::Comment)
            || node_kind == BlockKind::Comment)
            && let Some(start_idx) = current_start.take()
        {
            blocks.push(create_sub_block_with_kind(
                block,
                &content[start_idx..current_end],
                start_idx,
                current_end,
                BlockKind::CodeParagraph,
            ));
        }

        if gap_prefix_len > 0 {
            let gap_prefix_end = last_end + gap_prefix_len;
            blocks.push(create_sub_block_with_kind(
                block,
                &content[last_end..gap_prefix_end],
                last_end,
                gap_prefix_end,
                BlockKind::Gap,
            ));
        }

        if node_kind == BlockKind::Comment {
            blocks.push(create_sub_block_with_kind(
                block,
                &content[leading_start..end],
                leading_start,
                end,
                node_kind,
            ));
            last_kind = Some(BlockKind::Comment);
            last_end = end;
            continue;
        }

        if current_start.is_none() || gap_has_blank || last_kind == Some(BlockKind::Comment) {
            current_start = Some(leading_start);
            current_end = end;
        } else {
            current_end = end;
        }

        last_kind = Some(BlockKind::CodeParagraph);
        last_end = end;
    }

    if let Some(start_idx) = current_start.take() {
        blocks.push(create_sub_block_with_kind(
            block,
            &content[start_idx..current_end],
            start_idx,
            current_end,
            BlockKind::CodeParagraph,
        ));
    }

    if last_end < content.len() {
        let tail = &content[last_end..];
        let kind = classify_code_chunk(tail);
        if !tail.is_empty() && kind != BlockKind::Gap {
            blocks.push(create_sub_block_with_kind(
                block,
                tail,
                last_end,
                content.len(),
                kind,
            ));
        }
    }

    Ok(blocks)
}

#[derive(Debug, Clone)]
struct MarkdownSpan {
    start: usize,
    end: usize,
    kind: BlockKind,
}

fn collect_markdown_spans(node: tree_sitter::Node<'_>, spans: &mut Vec<MarkdownSpan>) {
    if let Some(kind) = markdown_kind(node.kind()) {
        spans.push(MarkdownSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_markdown_spans(child, spans);
    }
}

fn markdown_kind(kind: &str) -> Option<BlockKind> {
    match kind {
        "atx_heading" | "setext_heading" => Some(BlockKind::Header),
        "paragraph" => Some(BlockKind::Paragraph),
        "list_item" => Some(BlockKind::ListItem),
        "fenced_code_block" | "indented_code_block" => Some(BlockKind::CodeBlock),
        "block_quote" => Some(BlockKind::Quote),
        "thematic_break" | "html_block" | "link_reference_definition" | "table" => {
            Some(BlockKind::Element)
        }
        _ => None,
    }
}

fn signature_end_offset(content: &str, block_start: usize) -> usize {
    let bytes = content.as_bytes();
    if block_start >= bytes.len() {
        return bytes.len();
    }

    let mut end = block_start.saturating_add(1);
    if bytes.get(block_start + 1) == Some(&b'\r') && bytes.get(block_start + 2) == Some(&b'\n') {
        end = block_start + 3;
    } else if bytes.get(block_start + 1) == Some(&b'\n') {
        end = block_start + 2;
    }

    end.min(bytes.len())
}

fn signature_end_before_body(content: &str, block_start: usize) -> usize {
    if block_start == 0 || block_start > content.len() {
        return block_start.min(content.len());
    }

    let prefix = &content[..block_start];
    prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(block_start)
}

fn find_named_descendant<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_named_descendant(child, kind) {
            return Some(found);
        }
    }

    None
}

fn collect_body_nodes<'a>(
    body_node: tree_sitter::Node<'a>,
    comment_kinds: &[&str],
) -> Vec<tree_sitter::Node<'a>> {
    let mut nodes = Vec::new();
    let mut cursor = body_node.walk();
    for child in body_node.children(&mut cursor) {
        if child.is_named() || comment_kinds.iter().any(|kind| *kind == child.kind()) {
            nodes.push(child);
        }
    }
    nodes
}

fn gap_prefix_length(gap: &str) -> usize {
    if gap.is_empty() {
        return 0;
    }

    gap.rfind('\n').map(|idx| idx + 1).unwrap_or(gap.len())
}

fn classify_code_chunk(chunk: &str) -> BlockKind {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return BlockKind::Gap;
    }

    if trimmed.chars().all(|ch| ch == '}' || ch == ';') {
        return BlockKind::Gap;
    }

    let is_comment = trimmed.lines().all(|line| {
        line.trim_start().starts_with("//")
            || line.trim_start().starts_with("/*")
            || line.trim_start().starts_with('*')
            || line.trim_start().starts_with('#')
    });

    if is_comment {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn create_sub_block_with_kind(
    parent: &Block,
    content: &str,
    start_offset: usize,
    _end_offset: usize,
    kind: BlockKind,
) -> Block {
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&c| c == '\n').count();
    let chunk_newlines = content.chars().filter(|&c| c == '\n').count();

    let start_line = parent.start_line + offset_newlines;
    let end_line = start_line + chunk_newlines + if content.ends_with('\n') { 0 } else { 1 };

    Block {
        hash: hash_str(content),
        content: content.to_string(),
        kind,
        tags: parent.tags.clone(),
        complexity: parent.complexity, // Simplified: inherit complexity or re-calculate?
        // Re-calculation might be better if we split functions.
        // But for sub-blocks which are just parts of a function (paragraphs),
        // maybe we should just split the complexity proportionally or re-calc.
        // For now, let's inherit.
        // Or actually, `create_sub_block` implies it is smaller.
        // Let's set complexity to 0 for sub-blocks for now, as they are "sub-units".
        // Or re-calculate if we passed lang.
        // `create_sub_block_with_kind` doesn't have lang.
        // We can update it to take lang, or just set 0.
        // Let's set 0 for MVP to fix compilation.
        start_line,
        end_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::Block;

    fn make_block(content: &str, kind: BlockKind) -> Block {
        Block {
            hash: "test".to_string(),
            content: content.to_string(),
            kind,
            tags: Vec::new(),
            complexity: 0,
            start_line: 0,
            end_line: content.lines().count(),
        }
    }

    fn merge_blocks(blocks: Vec<Block>) -> String {
        blocks.into_iter().map(|b| b.content).collect()
    }

    #[test]
    fn test_split_code_simple() {
        let content = "fn foo() {\n    print();\n}";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_split_code_multiple() {
        let content = "fn foo() {\n    part1();\n\n    part2();\n}";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();

        // "fn foo() {\n    part1();" (CodeParagraph)
        // "\n\n" (Gap)
        // "    part2();\n}" (CodeParagraph)
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);

        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_markdown() {
        let content = "# Header\n\nPara 1.\n\nPara 2.";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Markdown).unwrap();

        // Header
        // Gap (\n\n)
        // Paragraph
        // Gap (\n\n)
        // Paragraph

        let kinds: Vec<BlockKind> = chunks.iter().map(|b| b.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Header,
                BlockKind::Gap,
                BlockKind::Paragraph,
                BlockKind::Gap,
                BlockKind::Paragraph
            ]
        );

        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_text_sentences() {
        let content = "Line one. Line two?";
        let block = make_block(content, BlockKind::Paragraph);
        let chunks = split(&block, Language::Text).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, BlockKind::Sentence);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_toml_paragraphs_preserve_content() {
        let content = "key = \"value\"\n\nother = \"value\"";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Toml).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_nix_paragraphs_preserve_content() {
        let content = "{ foo = \"bar\"; }\n\n{ baz = \"qux\"; }";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Nix).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_just_paragraphs_preserve_content() {
        let content = "build:\n\techo ok\n\ntest:\n\techo ok";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Just).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_round_trip_code() {
        let content = "A\n\nB\n\nC";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();
        assert_eq!(merge_blocks(chunks), content);
    }
}
