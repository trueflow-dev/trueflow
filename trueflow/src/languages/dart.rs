use super::{
    LanguageRegistration, LanguageSubSplitSemantics, SubSplitRegistration, TopLevelRegistration,
    default_code_sub_split, default_map_kind, no_attribute_nodes, no_nested_blocks, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::code_comments;
use crate::complexity;

use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser};

#[derive(Debug, Clone)]
struct SemanticSpan {
    start_byte: usize,
    end_byte: usize,
    kind: BlockKind,
    tags: Vec<String>,
}

pub(crate) fn registration() -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind: default_map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks: no_nested_blocks,
            collect_test_ranges: no_test_ranges,
            custom_splitter: Some(split_top_level),
        },
        sub_split: sub_split_registration,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_dart::LANGUAGE.into()
}

fn split_top_level(root: Node<'_>, content: &str, lang: Language) -> Result<Vec<Block>> {
    let mut blocks = Vec::new();
    let mut cursor = root.walk();
    let children: Vec<_> = root.children(&mut cursor).collect();

    let mut pending_start: Option<usize> = None;
    let mut pending_end = 0usize;
    let mut last_end = 0usize;
    let mut index = 0usize;

    while index < children.len() {
        let child = children[index];
        let kind = child.kind();

        if !child.is_named() || is_top_level_fragment_node(kind) {
            index += 1;
            continue;
        }

        if is_leading_node(kind) {
            if pending_start.is_none() {
                pending_start = Some(child.start_byte());
            }
            pending_end = child.end_byte();
            index += 1;
            continue;
        }

        if matches!(
            kind,
            "function_signature" | "getter_signature" | "setter_signature"
        ) && let Some(next) = children.get(index + 1).copied()
            && next.kind() == "function_body"
        {
            let start = pending_start.unwrap_or(child.start_byte());
            push_non_empty_gap(&mut blocks, content, last_end, start, lang)?;

            let end = next.end_byte();
            blocks.push(create_file_block(
                &content[start..end],
                BlockKind::Function,
                function_like_tags(child, content),
                content,
                start,
                end,
                lang,
            )?);
            blocks.extend(collect_test_invocation_blocks(next, content, lang)?);

            last_end = end;
            pending_start = None;
            pending_end = 0;
            index += 2;
            continue;
        }

        let span = match kind {
            "script_tag" => Some(SemanticSpan {
                start_byte: pending_start.unwrap_or(child.start_byte()),
                end_byte: child.end_byte(),
                kind: BlockKind::Module,
                tags: Vec::new(),
            }),
            "library_name" | "part_of_directive" => Some(SemanticSpan {
                start_byte: pending_start.unwrap_or(child.start_byte()),
                end_byte: child.end_byte(),
                kind: BlockKind::Module,
                tags: Vec::new(),
            }),
            "import_or_export" | "part_directive" => Some(SemanticSpan {
                start_byte: pending_start.unwrap_or(child.start_byte()),
                end_byte: child.end_byte(),
                kind: BlockKind::Import,
                tags: Vec::new(),
            }),
            "function_signature" | "getter_signature" | "setter_signature" => {
                let start = pending_start
                    .unwrap_or_else(|| declaration_start(content, last_end, child.start_byte()));
                Some(SemanticSpan {
                    start_byte: start,
                    end_byte: extend_to_semicolon(content, child.end_byte()),
                    kind: BlockKind::FunctionSignature,
                    tags: function_like_tags(child, content),
                })
            }
            "static_final_declaration_list" | "initialized_identifier_list" | "identifier_list" => {
                let start = pending_start
                    .unwrap_or_else(|| declaration_start(content, last_end, child.start_byte()));
                let end = extend_to_semicolon(content, child.end_byte());
                let text = &content[start..end.min(content.len())];
                Some(SemanticSpan {
                    start_byte: start,
                    end_byte: end,
                    kind: classify_variable_kind(text),
                    tags: Vec::new(),
                })
            }
            "class_declaration"
            | "mixin_declaration"
            | "extension_declaration"
            | "extension_type_declaration"
            | "enum_declaration"
            | "type_alias" => Some(SemanticSpan {
                start_byte: pending_start.unwrap_or(child.start_byte()),
                end_byte: child.end_byte(),
                kind: map_type_declaration_kind(child),
                tags: Vec::new(),
            }),
            _ => Some(SemanticSpan {
                start_byte: pending_start.unwrap_or(child.start_byte()),
                end_byte: child.end_byte().max(pending_end),
                kind: BlockKind::Code,
                tags: Vec::new(),
            }),
        };

        if let Some(span) = span {
            push_non_empty_gap(&mut blocks, content, last_end, span.start_byte, lang)?;
            blocks.push(create_file_block(
                &content[span.start_byte..span.end_byte],
                span.kind,
                span.tags,
                content,
                span.start_byte,
                span.end_byte,
                lang,
            )?);

            if matches!(
                kind,
                "class_declaration"
                    | "mixin_declaration"
                    | "extension_declaration"
                    | "extension_type_declaration"
                    | "enum_declaration"
            ) {
                blocks.extend(collect_type_member_blocks(child, content, lang)?);
            }

            last_end = span.end_byte;
        }

        pending_start = None;
        pending_end = 0;
        index += 1;
    }

    if let Some(start) = pending_start {
        let end = pending_end.max(start);
        if end > start {
            push_non_empty_gap(&mut blocks, content, last_end, start, lang)?;
            blocks.push(create_file_block(
                &content[start..end],
                BlockKind::Comment,
                Vec::new(),
                content,
                start,
                end,
                lang,
            )?);
            last_end = end;
        }
    }

    push_non_empty_gap(&mut blocks, content, last_end, content.len(), lang)?;

    Ok(blocks)
}

fn collect_type_member_blocks(
    type_node: Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    collect_type_member_spans(type_node, content)
        .into_iter()
        .map(|span| {
            create_file_block(
                &content[span.start_byte..span.end_byte],
                span.kind,
                span.tags,
                content,
                span.start_byte,
                span.end_byte,
                lang,
            )
        })
        .collect()
}

fn collect_type_member_spans(type_node: Node<'_>, content: &str) -> Vec<SemanticSpan> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    let mut cursor = body.walk();
    let mut pending_start: Option<usize> = None;
    let mut pending_end = 0usize;

    for child in body.children(&mut cursor) {
        let kind = child.kind();
        if matches!(kind, "{" | "}" | "," | ";") {
            continue;
        }

        if is_leading_node(kind) {
            if pending_start.is_none() {
                pending_start = Some(child.start_byte());
            }
            pending_end = child.end_byte();
            continue;
        }

        let Some(member_kind) = map_type_member_kind(child, content) else {
            continue;
        };

        spans.push(SemanticSpan {
            start_byte: pending_start.unwrap_or(child.start_byte()),
            end_byte: child.end_byte(),
            kind: member_kind,
            tags: member_tags(child, content),
        });
        pending_start = None;
        pending_end = 0;
    }

    if let Some(start) = pending_start {
        let end = pending_end.max(start);
        if end > start {
            spans.push(SemanticSpan {
                start_byte: start,
                end_byte: end,
                kind: BlockKind::Comment,
                tags: Vec::new(),
            });
        }
    }

    spans
}

fn collect_test_invocation_blocks(
    function_body: Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let mut spans = Vec::new();
    collect_test_invocation_spans(function_body, content, &mut spans);
    spans
        .into_iter()
        .map(|span| {
            create_file_block(
                &content[span.start_byte..span.end_byte],
                span.kind,
                span.tags,
                content,
                span.start_byte,
                span.end_byte,
                lang,
            )
        })
        .collect()
}

fn collect_test_invocation_spans(node: Node<'_>, content: &str, spans: &mut Vec<SemanticSpan>) {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.kind() == "expression_statement"
            && let Some(name) = leading_invocation_name(current, content)
            && is_stable_test_invocation(name)
        {
            spans.push(SemanticSpan {
                start_byte: current.start_byte(),
                end_byte: current.end_byte(),
                kind: BlockKind::Function,
                tags: vec!["test".to_string()],
            });
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        pending.extend(children.into_iter().rev());
    }
}

fn leading_invocation_name<'a>(node: Node<'a>, content: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find_map(|child| {
        (child.kind() == "identifier")
            .then(|| child.utf8_text(content.as_bytes()).ok())
            .flatten()
    })
}

fn is_stable_test_invocation(name: &str) -> bool {
    matches!(
        name,
        "group" | "test" | "testWidgets" | "setUp" | "tearDown" | "setUpAll" | "tearDownAll"
    )
}

fn is_leading_node(kind: &str) -> bool {
    matches!(kind, "annotation" | "comment")
}

fn is_top_level_fragment_node(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier" | "void_type" | "function_type" | "record_type" | "type_arguments"
    )
}

fn declaration_start(content: &str, last_end: usize, node_start: usize) -> usize {
    if node_start <= last_end {
        return node_start;
    }

    content[last_end..node_start]
        .char_indices()
        .find_map(|(offset, ch)| (!ch.is_whitespace()).then_some(last_end + offset))
        .unwrap_or(node_start)
}

fn extend_to_semicolon(content: &str, from: usize) -> usize {
    if from >= content.len() {
        return content.len();
    }

    content[from..]
        .find(';')
        .map(|offset| from + offset + 1)
        .unwrap_or(content.len())
}

fn push_non_empty_gap(
    blocks: &mut Vec<Block>,
    content: &str,
    start: usize,
    end: usize,
    lang: Language,
) -> Result<()> {
    if end <= start {
        return Ok(());
    }

    let gap = &content[start..end];
    if gap.trim().is_empty() {
        return Ok(());
    }

    blocks.push(create_file_block(
        gap,
        BlockKind::Gap,
        Vec::new(),
        content,
        start,
        end,
        lang,
    )?);
    Ok(())
}

fn map_type_declaration_kind(node: Node<'_>) -> BlockKind {
    match node.kind() {
        "class_declaration" => BlockKind::Class,
        "mixin_declaration" | "extension_declaration" => BlockKind::Impl,
        "extension_type_declaration" | "type_alias" => BlockKind::Type,
        "enum_declaration" => BlockKind::Enum,
        _ => BlockKind::Code,
    }
}

fn map_type_member_kind(node: Node<'_>, content: &str) -> Option<BlockKind> {
    match node.kind() {
        "comment" | "annotation" => None,
        "enum_constant" => Some(BlockKind::Const),
        "class_member" => classify_class_member_kind(node, content),
        _ => None,
    }
}

fn classify_class_member_kind(node: Node<'_>, content: &str) -> Option<BlockKind> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "annotation" => continue,
            "method_signature" => return Some(BlockKind::Method),
            "declaration" => return classify_member_declaration_kind(node, child, content),
            _ => {}
        }
    }

    None
}

fn classify_member_declaration_kind(
    member_node: Node<'_>,
    declaration_node: Node<'_>,
    content: &str,
) -> Option<BlockKind> {
    if contains_named_descendant_any(
        declaration_node,
        &[
            "constructor_signature",
            "constant_constructor_signature",
            "factory_constructor_signature",
            "redirecting_factory_constructor_signature",
        ],
    ) {
        return Some(BlockKind::Method);
    }

    if contains_named_descendant_any(
        declaration_node,
        &[
            "function_signature",
            "getter_signature",
            "setter_signature",
            "operator_signature",
        ],
    ) {
        return Some(BlockKind::FunctionSignature);
    }

    if contains_named_descendant_any(
        declaration_node,
        &[
            "static_final_declaration_list",
            "initialized_identifier_list",
            "identifier_list",
        ],
    ) {
        return Some(classify_variable_kind(
            &content[member_node.start_byte()..member_node.end_byte()],
        ));
    }

    None
}

fn contains_named_descendant_any(node: Node<'_>, kinds: &[&str]) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if kinds.iter().any(|kind| *kind == current.kind()) {
            return true;
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    false
}

fn classify_variable_kind(text: &str) -> BlockKind {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("const ") || lowered.contains("final ") {
        BlockKind::Const
    } else {
        BlockKind::Variable
    }
}

fn function_like_tags(node: Node<'_>, content: &str) -> Vec<String> {
    declaration_name(node, content)
        .filter(|name| is_test_like_name(name))
        .map(|_| vec!["test".to_string()])
        .unwrap_or_default()
}

fn member_tags(node: Node<'_>, content: &str) -> Vec<String> {
    declaration_name(node, content)
        .filter(|name| is_test_like_name(name))
        .map(|_| vec!["test".to_string()])
        .unwrap_or_default()
}

fn declaration_name<'a>(node: Node<'a>, content: &'a str) -> Option<String> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if let Some(name) = current
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(content.as_bytes()).ok())
        {
            return Some(name.to_string());
        }

        let mut cursor = current.walk();
        let children = current
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "annotation")
            .collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    None
}

fn is_test_like_name(name: &str) -> bool {
    name.strip_prefix("test").is_some_and(|suffix| {
        suffix.is_empty() || !suffix.chars().next().is_some_and(char::is_lowercase)
    }) || name.starts_with("test_")
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like_review_units,
        },
        BlockKind::Class | BlockKind::Impl | BlockKind::Enum | BlockKind::Type => {
            SubSplitRegistration {
                semantics: LanguageSubSplitSemantics::ReviewUnits,
                splitter: split_type_like_review_units,
            }
        }
        _ => default_code_sub_split(kind),
    }
}

fn split_function_like_review_units(block: &Block) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_dart::LANGUAGE.into())?;

    let tree = parser
        .parse(&block.content, None)
        .context("Failed to parse dart function-like block")?;
    let root = tree.root_node();
    let Some(body_node) = find_named_descendant(root, "function_body") else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let signature_end = signature_end_for_function_body(&block.content, body_node.start_byte());
    if signature_end == 0 || signature_end > block.content.len() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    let mut blocks = vec![create_sub_block(
        block,
        &block.content[..signature_end],
        0,
        signature_end,
        BlockKind::FunctionSignature,
        Vec::new(),
    )?];

    let body = &block.content[signature_end..];
    let mut start = 0usize;
    for gap in paragraph_break_regex().find_iter(body) {
        if start < gap.start() {
            let chunk = &body[start..gap.start()];
            if !chunk.is_empty() {
                blocks.push(create_sub_block(
                    block,
                    chunk,
                    signature_end + start,
                    signature_end + gap.start(),
                    classify_review_chunk(chunk),
                    Vec::new(),
                )?);
            }
        }

        let gap_chunk = &body[gap.start()..gap.end()];
        blocks.push(create_sub_block(
            block,
            gap_chunk,
            signature_end + gap.start(),
            signature_end + gap.end(),
            BlockKind::Gap,
            Vec::new(),
        )?);
        start = gap.end();
    }

    if start < body.len() {
        let chunk = &body[start..];
        if !chunk.is_empty() {
            blocks.push(create_sub_block(
                block,
                chunk,
                signature_end + start,
                signature_end + body.len(),
                classify_review_chunk(chunk),
                Vec::new(),
            )?);
        }
    }

    Ok(blocks)
}

fn signature_end_for_function_body(content: &str, body_start: usize) -> usize {
    if body_start >= content.len() {
        return content.len();
    }

    let tail = &content[body_start..];
    let arrow = tail.find("=>");
    let brace = tail.find('{');

    match (arrow, brace) {
        (Some(arrow_pos), Some(brace_pos)) if arrow_pos < brace_pos => {
            advance_past_trivia(content, body_start + arrow_pos + 2)
        }
        (_, Some(brace_pos)) => advance_past_brace(content, body_start + brace_pos),
        (Some(arrow_pos), None) => advance_past_trivia(content, body_start + arrow_pos + 2),
        (None, None) => body_start,
    }
}

fn advance_past_brace(content: &str, brace_index: usize) -> usize {
    let mut end = brace_index.saturating_add(1);
    let bytes = content.as_bytes();
    if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
        end += 2;
    } else if bytes.get(end) == Some(&b'\n') {
        end += 1;
    }
    end.min(content.len())
}

fn advance_past_trivia(content: &str, mut index: usize) -> usize {
    let bytes = content.as_bytes();
    while let Some(byte) = bytes.get(index) {
        if !byte.is_ascii_whitespace() {
            break;
        }
        index += 1;
    }
    index.min(content.len())
}

fn classify_review_chunk(chunk: &str) -> BlockKind {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return BlockKind::Gap;
    }
    if trimmed.chars().all(|ch| matches!(ch, '}' | ';')) {
        return BlockKind::Gap;
    }
    if code_comments::chunk_is_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn split_type_like_review_units(block: &Block) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_dart::LANGUAGE.into())?;

    let tree = parser
        .parse(&block.content, None)
        .context("Failed to parse dart type-like block")?;
    let root = tree.root_node();

    if find_named_descendant(root, "type_alias").is_some() {
        return Ok(vec![block.clone()]);
    }

    let Some(type_node) = find_named_descendant_any(
        root,
        &[
            "class_declaration",
            "mixin_declaration",
            "extension_declaration",
            "extension_type_declaration",
            "enum_declaration",
        ],
    ) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let spans = collect_type_member_spans(type_node, &block.content);
    if spans.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    review_unit_blocks_from_spans(block, spans)
}

fn review_unit_blocks_from_spans(
    block: &Block,
    mut spans: Vec<SemanticSpan>,
) -> Result<Vec<Block>> {
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    let mut blocks = Vec::new();
    let mut cursor = 0;

    for span in spans {
        if cursor < span.start_byte {
            let chunk = &block.content[cursor..span.start_byte];
            blocks.push(create_sub_block(
                block,
                chunk,
                cursor,
                span.start_byte,
                classify_review_chunk(chunk),
                Vec::new(),
            )?);
        }
        blocks.push(create_sub_block(
            block,
            &block.content[span.start_byte..span.end_byte],
            span.start_byte,
            span.end_byte,
            span.kind,
            span.tags,
        )?);
        cursor = span.end_byte.max(cursor);
    }

    if cursor < block.content.len() {
        let chunk = &block.content[cursor..];
        blocks.push(create_sub_block(
            block,
            chunk,
            cursor,
            block.content.len(),
            classify_review_chunk(chunk),
            Vec::new(),
        )?);
    }

    Ok(blocks)
}

fn find_named_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    find_named_descendant_any(node, &[kind])
}

fn find_named_descendant_any<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if kinds.iter().any(|kind| *kind == current.kind()) {
            return Some(current);
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    None
}

fn create_file_block(
    text: &str,
    kind: BlockKind,
    tags: Vec<String>,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Result<Block> {
    let mut block = Block::from_file_range(full_source, kind, ByteSpan::new(start_byte, end_byte))
        .context("Dart top-level range must be a valid UTF-8 source slice")?;
    assert_eq!(
        block.content, text,
        "Dart top-level range must name content"
    );
    block.tags = tags;
    block.complexity = complexity::calculate(&block.content, lang);
    Ok(block)
}

fn create_sub_block(
    parent: &Block,
    text: &str,
    start_offset: usize,
    end_offset: usize,
    kind: BlockKind,
    tags: Vec<String>,
) -> Result<Block> {
    let mut block = Block::from_parent_range(parent, kind, ByteSpan::new(start_offset, end_offset))
        .context("Dart sub-split range must be a valid parent UTF-8 slice")?;
    block.tags = parent.tags.clone();
    assert_eq!(
        block.content, text,
        "Dart sub-split range must name content"
    );
    for tag in tags {
        if !block.tags.iter().any(|existing| existing == &tag) {
            block.tags.push(tag);
        }
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_top_level_keeps_real_dart_kinds() {
        let source = "import 'dart:math';\n\nconst answer = 42;\n\nString greet(String name) {\n  return 'hi $name';\n}\n\nmixin Counter {\n  int hits = 0;\n}\n\nclass Worker {\n  final String name;\n  Worker(this.name);\n\n  void run() {}\n}\n";
        let tree = parse_tree(source);
        let root = tree.root_node();

        let blocks = split_top_level(root, source, Language::Dart).unwrap();
        let kinds = blocks.iter().map(|block| block.kind).collect::<Vec<_>>();

        assert!(kinds.contains(&BlockKind::Import));
        assert!(kinds.contains(&BlockKind::Const));
        assert!(kinds.contains(&BlockKind::Function));
        assert!(kinds.contains(&BlockKind::Impl));
        assert!(kinds.contains(&BlockKind::Class));
        assert!(kinds.contains(&BlockKind::Method));
    }

    #[test]
    fn split_top_level_collects_nested_group_and_test_calls() {
        let source = "void main() {\n  group('suite', () {\n    test('works', () {\n      expect(true, isTrue);\n    });\n  });\n}\n";
        let tree = parse_tree(source);
        let root = tree.root_node();

        let blocks = split_top_level(root, source, Language::Dart).unwrap();
        let tagged = blocks
            .iter()
            .filter(|block| block.tags.iter().any(|tag| tag == "test"))
            .map(|block| block.content.as_str())
            .collect::<Vec<_>>();

        assert!(
            tagged
                .iter()
                .any(|content| content.contains("group('suite'"))
        );
        assert!(
            tagged
                .iter()
                .any(|content| content.contains("test('works'"))
        );
    }

    #[test]
    fn split_type_like_review_units_preserves_full_class_content() {
        let source =
            "class Worker {\n  final String name;\n\n  Worker(this.name);\n\n  void run() {}\n}\n";
        let block =
            Block::from_file_range(source, BlockKind::Class, ByteSpan::new(0, source.len()))
                .unwrap();

        let blocks = split_type_like_review_units(&block).unwrap();
        let rebuilt = blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>();

        assert_eq!(rebuilt, source);
    }

    fn parse_tree(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_dart::LANGUAGE.into())
            .unwrap_or_else(|error| panic!("load dart grammar: {error}"));
        parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("parse dart"))
    }
}
