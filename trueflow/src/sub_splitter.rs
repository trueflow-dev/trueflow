use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::hashing::hash_str;
use anyhow::{Context, Result};
use log::info;
use regex::Regex;
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
        Language::Text => split_text(block)?,
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
    // Split by double newline (paragraph style)
    let re = Regex::new(r"\n\s*\n").unwrap();
    let content = &block.content;
    let mut blocks = Vec::new();
    let mut start_offset = 0;

    for mat in re.find_iter(content) {
        let end_offset = mat.start(); // End before the delimiter

        // Code chunk
        if start_offset < end_offset {
            let chunk = &content[start_offset..end_offset];
            if !chunk.is_empty() {
                blocks.push(create_sub_block_with_kind(
                    block,
                    chunk,
                    start_offset,
                    end_offset,
                    classify_code_chunk(chunk),
                ));
            }
        }

        // Gap chunk (the delimiter)
        let gap_chunk = &content[mat.start()..mat.end()];
        blocks.push(create_sub_block_with_kind(
            block,
            gap_chunk,
            mat.start(),
            mat.end(),
            BlockKind::Gap,
        ));

        start_offset = mat.end(); // Start after the delimiter
    }

    // Trailing chunk
    if start_offset < content.len() {
        let chunk = &content[start_offset..];
        if !chunk.is_empty() {
            blocks.push(create_sub_block_with_kind(
                block,
                chunk,
                start_offset,
                content.len(),
                classify_code_chunk(chunk),
            ));
        }
    }

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

fn split_text(block: &Block) -> Result<Vec<Block>> {
    split_markdown_sentences(block)
}

fn split_rust_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        tree_sitter_rust::LANGUAGE.into(),
        "function_item",
        "block",
        signature_end_offset,
        &["line_comment", "block_comment"],
        true,
    )
}

fn split_python_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        tree_sitter_python::LANGUAGE.into(),
        "function_definition",
        "block",
        signature_end_before_body,
        &["comment", "line_comment", "block_comment"],
        false,
    )
}

fn split_js_function(block: &Block, lang: Language) -> Result<Vec<Block>> {
    let language = match lang {
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    };
    split_function_with_parser(
        block,
        language,
        "function_declaration",
        "statement_block",
        signature_end_offset,
        &["comment", "line_comment", "block_comment"],
        true,
    )
}

fn split_function_with_parser(
    block: &Block,
    language: tree_sitter::Language,
    function_kind: &str,
    body_kind: &str,
    signature_end: fn(&str, usize) -> usize,
    comment_kinds: &[&str],
    trim_closing_brace: bool,
) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser.set_language(&language)?;

    let tree = parser
        .parse(&block.content, None)
        .context("Failed to parse function block")?;
    let root = tree.root_node();
    let Some(function_node) = find_named_descendant(root, function_kind) else {
        return split_code(block);
    };
    let Some(body_node) = find_named_descendant(function_node, body_kind) else {
        return split_code(block);
    };

    let mut blocks = Vec::new();
    let content = block.content.as_str();
    let signature_end = signature_end(content, body_node.start_byte());
    if signature_end > 0 {
        blocks.push(create_sub_block_with_kind(
            block,
            &content[..signature_end],
            0,
            signature_end,
            BlockKind::Signature,
        ));
    }

    let nodes = collect_body_nodes(body_node, comment_kinds);
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
        let gap_has_blank = gap_has_blank_line(gap);
        let gap_prefix_len = if gap_has_blank {
            gap_prefix_length(gap)
        } else {
            0
        };
        let leading_start = last_end + gap_prefix_len;

        let mut end = node.end_byte();
        if trim_closing_brace
            && idx == nodes.len().saturating_sub(1)
            && content[end..].trim() == "}"
        {
            end = content.len();
        }

        let node_kind = if comment_kinds.iter().any(|kind| *kind == node.kind()) {
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
        if !tail.is_empty() {
            let kind = classify_code_chunk(tail);
            if kind != BlockKind::Gap {
                blocks.push(create_sub_block_with_kind(
                    block,
                    tail,
                    last_end,
                    content.len(),
                    kind,
                ));
            }
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

fn gap_has_blank_line(gap: &str) -> bool {
    let mut saw_newline = false;
    let mut has_non_whitespace = false;

    for ch in gap.chars() {
        if ch == '\n' {
            if saw_newline && !has_non_whitespace {
                return true;
            }
            saw_newline = true;
            has_non_whitespace = false;
        } else if !ch.is_whitespace() {
            has_non_whitespace = true;
        }
    }

    false
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
    fn test_round_trip_code() {
        let content = "A\n\nB\n\nC";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();
        assert_eq!(merge_blocks(chunks), content);
    }
}
