use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::code_comments;
use crate::hashing::TreeHash;
use crate::review_units::{MAX_REVIEW_UNIT_SPAN_LINES, block_line_span};
use crate::text_split::{paragraph_break_regex, split_by_paragraph_breaks};
use crate::{rust, swift};
use anyhow::{Context, Result};
use tracing::info;
use tree_sitter::Parser;
use tree_sitter_md;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubSplitSemantics {
    /// The returned blocks are the direct review units for the parent block.
    ReviewUnits,
    /// The returned blocks are semantic children that may themselves be split further.
    StructuralChildren,
}

impl SubSplitSemantics {
    pub fn supports_review_unit_invariant(self) -> bool {
        matches!(self, Self::ReviewUnits)
    }
}

#[derive(Debug, Clone)]
pub struct SubSplitResult {
    pub blocks: Vec<Block>,
    pub semantics: SubSplitSemantics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitPlan {
    IdentityReviewUnit,
    MarkdownChildren,
    MarkdownReviewUnits,
    SentenceReviewUnits,
    CodeReviewUnits,
    RustFunctionReviewUnits,
    RustImplReviewUnits,
    SwiftFunctionReviewUnits,
    SwiftTypeReviewUnits,
    PythonFunctionReviewUnits,
    JsFunctionReviewUnits,
    JavaFunctionReviewUnits,
    JavaTypeReviewUnits,
}

impl SplitPlan {
    fn semantics(self) -> SubSplitSemantics {
        match self {
            Self::MarkdownChildren => SubSplitSemantics::StructuralChildren,
            Self::IdentityReviewUnit
            | Self::MarkdownReviewUnits
            | Self::SentenceReviewUnits
            | Self::CodeReviewUnits
            | Self::RustFunctionReviewUnits
            | Self::RustImplReviewUnits
            | Self::SwiftFunctionReviewUnits
            | Self::SwiftTypeReviewUnits
            | Self::PythonFunctionReviewUnits
            | Self::JsFunctionReviewUnits
            | Self::JavaFunctionReviewUnits
            | Self::JavaTypeReviewUnits => SubSplitSemantics::ReviewUnits,
        }
    }
}

pub fn split(block: &Block, lang: Language) -> Result<Vec<Block>> {
    split_result(block, lang).map(|result| result.blocks)
}

pub fn split_result(block: &Block, lang: Language) -> Result<SubSplitResult> {
    info!(
        "sub_splitter start (lang={:?}, kind={}, bytes={}, hash={})",
        lang,
        block.kind.as_str(),
        block.content.len(),
        block.hash
    );

    let plan = determine_split_plan(block.kind, lang);
    if should_keep_parent_review_unit(plan, block) {
        let result = SubSplitResult {
            blocks: vec![block.clone()],
            semantics: SubSplitSemantics::ReviewUnits,
        };
        info!(
            "sub_splitter kept parent review unit (lines={}, max_lines={})",
            block_line_span(block),
            MAX_REVIEW_UNIT_SPAN_LINES
        );
        return Ok(result);
    }
    let blocks = match plan {
        SplitPlan::IdentityReviewUnit => vec![block.clone()],
        SplitPlan::MarkdownChildren => split_markdown_tree(block)?,
        SplitPlan::MarkdownReviewUnits => split_markdown_sentences(block)?,
        SplitPlan::SentenceReviewUnits => split_sentences(block)?,
        SplitPlan::CodeReviewUnits => split_code(block)?,
        SplitPlan::RustFunctionReviewUnits => split_rust_function(block)?,
        SplitPlan::RustImplReviewUnits => split_rust_impl(block)?,
        SplitPlan::SwiftFunctionReviewUnits => split_swift_function(block)?,
        SplitPlan::SwiftTypeReviewUnits => split_swift_type(block)?,
        SplitPlan::PythonFunctionReviewUnits => split_python_function(block)?,
        SplitPlan::JsFunctionReviewUnits => split_js_function(block, lang)?,
        SplitPlan::JavaFunctionReviewUnits => split_java_function(block)?,
        SplitPlan::JavaTypeReviewUnits => split_java_type(block)?,
    };

    let result = SubSplitResult {
        blocks,
        semantics: plan.semantics(),
    };

    info!(
        "sub_splitter done (blocks={}, semantics={:?})",
        result.blocks.len(),
        result.semantics
    );
    Ok(result)
}

fn should_keep_parent_review_unit(plan: SplitPlan, block: &Block) -> bool {
    !matches!(plan, SplitPlan::IdentityReviewUnit)
        && block_line_span(block) <= MAX_REVIEW_UNIT_SPAN_LINES
}

fn determine_split_plan(kind: BlockKind, lang: Language) -> SplitPlan {
    match lang {
        Language::Markdown if matches!(kind, BlockKind::Paragraph | BlockKind::ListItem) => {
            SplitPlan::MarkdownReviewUnits
        }
        Language::Markdown
            if matches!(
                kind,
                BlockKind::Header | BlockKind::CodeBlock | BlockKind::Quote | BlockKind::Element
            ) =>
        {
            SplitPlan::IdentityReviewUnit
        }
        Language::Markdown => SplitPlan::MarkdownChildren,
        Language::Text => SplitPlan::SentenceReviewUnits,
        Language::Toml | Language::Nix | Language::Just => SplitPlan::CodeReviewUnits,
        Language::Rust if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::RustFunctionReviewUnits
        }
        Language::Rust if matches!(kind, BlockKind::Impl | BlockKind::Interface) => {
            SplitPlan::RustImplReviewUnits
        }
        Language::Swift if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::SwiftFunctionReviewUnits
        }
        Language::Swift
            if matches!(
                kind,
                BlockKind::Impl
                    | BlockKind::Interface
                    | BlockKind::Class
                    | BlockKind::Struct
                    | BlockKind::Enum
            ) =>
        {
            SplitPlan::SwiftTypeReviewUnits
        }
        Language::Python if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::PythonFunctionReviewUnits
        }
        Language::JavaScript | Language::TypeScript
            if matches!(
                kind,
                BlockKind::Function | BlockKind::Method | BlockKind::Export
            ) =>
        {
            SplitPlan::JsFunctionReviewUnits
        }
        Language::Java if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::JavaFunctionReviewUnits
        }
        Language::Java
            if matches!(
                kind,
                BlockKind::Class
                    | BlockKind::Interface
                    | BlockKind::Enum
                    | BlockKind::Struct
                    | BlockKind::Type
            ) =>
        {
            SplitPlan::JavaTypeReviewUnits
        }
        _ => SplitPlan::CodeReviewUnits,
    }
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

fn split_sentences(block: &Block) -> Result<Vec<Block>> {
    split_markdown_sentences(block)
}

struct FunctionSplitConfig<'a> {
    language: tree_sitter::Language,
    function_kinds: &'a [&'a str],
    body_kind: &'a str,
    body_statement_kind: Option<&'a str>,
    signature_end: fn(&str, usize) -> usize,
    comment_kinds: &'a [&'a str],
    trim_closing_brace: bool,
}

fn split_rust_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_rust::LANGUAGE.into(),
            function_kinds: &["function_item"],
            body_kind: "block",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["line_comment", "block_comment"],
            trim_closing_brace: true,
        },
    )
}

fn split_rust_impl(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse impl")?;
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "impl_item" {
            let items = collect_rust_impl_items(block, child)?;
            if !items.is_empty() {
                return Ok(items);
            }
        }
    }

    split_code(block)
}

fn collect_rust_impl_items(parent: &Block, impl_node: tree_sitter::Node<'_>) -> Result<Vec<Block>> {
    Ok(rust::collect_impl_member_spans(impl_node)
        .into_iter()
        .map(|member| {
            create_sub_block_with_kind(
                parent,
                &parent.content[member.start_byte..member.end_byte],
                member.start_byte,
                member.end_byte,
                member.kind,
            )
        })
        .collect())
}

fn split_python_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_python::LANGUAGE.into(),
            function_kinds: &["function_definition"],
            body_kind: "block",
            body_statement_kind: None,
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
        &FunctionSplitConfig {
            language,
            function_kinds: &["function_declaration"],
            body_kind: "statement_block",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["comment", "line_comment", "block_comment"],
            trim_closing_brace: true,
        },
    )
}

fn split_swift_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_swift::LANGUAGE.into(),
            function_kinds: &["function_declaration"],
            body_kind: "function_body",
            body_statement_kind: Some("statements"),
            signature_end: signature_end_offset,
            comment_kinds: &["comment", "multiline_comment"],
            trim_closing_brace: true,
        },
    )
}

fn split_swift_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_swift::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse swift type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant(root, "class_declaration")
        .or_else(|| find_named_descendant(root, "protocol_declaration"));
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = type_node.child_by_field_name("body") else {
        return split_code(block);
    };
    if !swift::body_is_non_trivial(body, content) {
        return split_code(block);
    }

    let items = collect_swift_type_items(block, body, content);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_java_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_java::LANGUAGE.into(),
            function_kinds: &[
                "method_declaration",
                "constructor_declaration",
                "compact_constructor_declaration",
            ],
            body_kind: "block",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["line_comment", "block_comment"],
            trim_closing_brace: true,
        },
    )
}

fn split_java_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse java type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant_any(
        root,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "annotation_type_declaration",
        ],
    );
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = type_node.child_by_field_name("body") else {
        return split_code(block);
    };

    let items = collect_java_type_items(block, body);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_function_with_parser(
    block: &Block,
    config: &FunctionSplitConfig<'_>,
) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser.set_language(&config.language)?;

    let tree = parser
        .parse(&block.content, None)
        .context("Failed to parse function block")?;
    let root = tree.root_node();
    let Some(function_node) = find_named_descendant_any(root, config.function_kinds) else {
        return split_code(block);
    };
    let Some(body_node) = find_named_descendant(function_node, config.body_kind) else {
        return split_code(block);
    };
    let split_node = config
        .body_statement_kind
        .and_then(|kind| find_named_descendant(body_node, kind))
        .unwrap_or(body_node);

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

    let nodes = collect_body_nodes(split_node, config.comment_kinds);
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
    find_named_descendant_any(node, &[kind])
}

fn find_named_descendant_any<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if kinds.iter().any(|kind| *kind == node.kind()) {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_named_descendant_any(child, kinds) {
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

fn collect_swift_type_items(
    parent: &Block,
    body: tree_sitter::Node<'_>,
    content: &str,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    for member in swift::collect_type_member_spans(body, content) {
        let chunk = &parent.content[member.start_byte..member.end_byte];
        blocks.push(create_sub_block_with_kind(
            parent,
            chunk,
            member.start_byte,
            member.end_byte,
            member.kind,
        ));
    }

    blocks
}

fn collect_java_type_items(parent: &Block, body: tree_sitter::Node<'_>) -> Vec<Block> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|child| {
            let kind = match child.kind() {
                "field_declaration" => BlockKind::Variable,
                "constant_declaration" => BlockKind::Const,
                "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration" => BlockKind::Method,
                "class_declaration" => BlockKind::Class,
                "interface_declaration" => BlockKind::Interface,
                "enum_declaration" => BlockKind::Enum,
                "record_declaration" => BlockKind::Struct,
                "annotation_type_declaration" => BlockKind::Type,
                _ => return None,
            };
            Some(create_sub_block_with_kind(
                parent,
                &parent.content[child.start_byte()..child.end_byte()],
                child.start_byte(),
                child.end_byte(),
                kind,
            ))
        })
        .collect()
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

    if code_comments::chunk_is_hash_or_c_style_comment_only(chunk) {
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
        hash: TreeHash::from_content(content),
        content: content.to_string(),
        kind,
        tags: parent.tags.clone(),
        complexity: None,
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
        make_block_with_span(content, kind, 0, content.lines().count())
    }

    fn make_block_with_span(
        content: &str,
        kind: BlockKind,
        start_line: usize,
        end_line: usize,
    ) -> Block {
        Block {
            hash: TreeHash::new("test"),
            content: content.to_string(),
            kind,
            tags: Vec::new(),
            complexity: None,
            start_line,
            end_line,
        }
    }

    fn make_large_block(content: &str, kind: BlockKind) -> Block {
        make_block_with_span(
            content,
            kind,
            0,
            crate::review_units::MAX_REVIEW_UNIT_SPAN_LINES + 8,
        )
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
        assert_eq!(chunks[0].kind, BlockKind::Code);
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_split_code_multiple() {
        let content = "fn foo() {\n    part1();\n\n    part2();\n}";
        let block = make_large_block(content, BlockKind::Code);
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
        let block = make_large_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Markdown).unwrap();

        // Header
        // Gap (\n\n)
        // Paragraph
        // Gap (\n\n)
        // Paragraph

        let kinds: Vec<BlockKind> = chunks.iter().map(|b| b.kind).collect();
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
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, BlockKind::Paragraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_text_sentences_when_block_exceeds_threshold() {
        let content = "Line one. Line two?";
        let block = make_large_block(content, BlockKind::Paragraph);
        let chunks = split(&block, Language::Text).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, BlockKind::Sentence);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_toml_paragraphs_preserve_content() {
        let content = "key = \"value\"\n\nother = \"value\"";
        let block = make_large_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Toml).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_rust_impl_into_items() {
        let content = "impl Foo {\n    fn read_heavy(&self) {}\n    const MAX: usize = 1;\n}\n";
        let block = make_large_block(content, BlockKind::Impl);
        let chunks = split(&block, Language::Rust).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Const));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Impl));
    }

    #[test]
    fn test_split_swift_extension_into_members_when_non_trivial() {
        let content = "extension Context {\n    func fetchWorld() -> World {\n        world\n    }\n\n    func reset() async -> [UInt8] {\n        await world.transform([])\n    }\n}\n";
        let block = make_large_block(content, BlockKind::Impl);
        let chunks = split(&block, Language::Swift).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Impl));
    }

    #[test]
    fn test_split_java_class_into_members() {
        let content = "class Worker {\n    private final int scale;\n\n    Worker(int scale) {\n        this.scale = scale;\n    }\n\n    int process(int value) {\n        if (value > 0) {\n            return value * scale;\n        }\n        return 0;\n    }\n}\n";
        let block = make_large_block(content, BlockKind::Class);
        let chunks = split(&block, Language::Java).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Variable));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Class));
    }

    #[test]
    fn test_split_java_method_into_review_units() {
        let content = "int process(int value) {\n    int total = value;\n\n    // only positive values count\n    if (value > 0) {\n        total += scale;\n    }\n\n    return total;\n}\n";
        let block = make_large_block(content, BlockKind::Method);
        let chunks = split(&block, Language::Java).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::Comment,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
            ]
        );
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_nix_paragraphs_preserve_content() {
        let content = "{ foo = \"bar\"; }\n\n{ baz = \"qux\"; }";
        let block = make_large_block(content, BlockKind::Code);
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
        let block = make_large_block(content, BlockKind::Code);
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

    #[test]
    fn test_sub_blocks_do_not_inherit_parent_complexity() {
        let content = "fn foo() {\n    if true {\n        run();\n    }\n\n    finish();\n}";
        let mut block = make_large_block(content, BlockKind::Function);
        block.complexity = Some(7);

        let chunks = split(&block, Language::Rust).unwrap();

        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.complexity.is_none()));
    }
}
