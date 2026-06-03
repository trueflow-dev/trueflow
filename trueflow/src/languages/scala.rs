use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::code_comments;
use crate::hashing::TreeHash;
use crate::text_split::split_by_paragraph_breaks;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

const TYPE_LIKE_DECLARATIONS: &[&str] = &[
    "object_definition",
    "class_definition",
    "trait_definition",
    "enum_definition",
    "given_definition",
    "extension_definition",
    "package_object",
];
const FUNCTION_LIKE_DECLARATIONS: &[&str] = &["function_definition"];
const TEST_CASE_NAMES: &[&str] = &["test", "it", "scenario", "property"];
const TEST_SUITE_MARKERS: &[&str] = &[
    "AnyFunSuite",
    "FunSuite",
    "FunSpec",
    "AnyFunSpec",
    "FlatSpec",
    "AnyFlatSpec",
    "WordSpec",
    "AnyWordSpec",
    "FreeSpec",
    "AnyFreeSpec",
    "PropSpec",
    "AnyPropSpec",
    "munit.FunSuite",
    "ScalaTest",
];

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
            map_kind,
            is_attribute_node,
            collect_nested_blocks,
            collect_test_ranges,
            custom_splitter: None,
        },
        sub_split: sub_split_registration,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_scala::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    classify_node(node, content, false)
}

fn classify_node(node: Node<'_>, content: &str, in_container: bool) -> BlockKind {
    match node.kind() {
        "comment" | "block_comment" => BlockKind::Comment,
        "package_clause" => BlockKind::Module,
        "import_declaration" => BlockKind::Import,
        "export_declaration" => BlockKind::Export,
        "object_definition" | "package_object" => BlockKind::Class,
        "class_definition" => BlockKind::Class,
        "trait_definition" => BlockKind::Interface,
        "enum_definition" => BlockKind::Enum,
        "given_definition" | "extension_definition" => BlockKind::Impl,
        "type_definition" => BlockKind::Type,
        "val_definition" | "val_declaration" => BlockKind::Const,
        "var_definition" | "var_declaration" => BlockKind::Variable,
        "function_definition" => {
            if in_container {
                BlockKind::Method
            } else {
                BlockKind::Function
            }
        }
        "function_declaration" => BlockKind::FunctionSignature,
        "simple_enum_case" | "full_enum_case" => BlockKind::Const,
        "call_expression" if is_test_case_call(node, content) => BlockKind::Function,
        _ => BlockKind::Code,
    }
}

fn is_attribute_node(kind: &str) -> bool {
    kind == "annotation"
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    if !can_contain_nested_members(node.kind()) {
        return Vec::new();
    }

    let mut spans = Vec::new();
    collect_container_spans(node, content, node.kind() == "package_clause", &mut spans);
    spans
        .into_iter()
        .map(|span| NestedBlock {
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            kind: span.kind,
        })
        .collect()
}

fn can_contain_nested_members(kind: &str) -> bool {
    matches!(
        kind,
        "package_clause"
            | "object_definition"
            | "package_object"
            | "class_definition"
            | "trait_definition"
            | "enum_definition"
            | "given_definition"
            | "extension_definition"
    )
}

fn functions_are_methods_in_container(kind: &str) -> bool {
    matches!(
        kind,
        "object_definition"
            | "package_object"
            | "class_definition"
            | "trait_definition"
            | "enum_definition"
            | "given_definition"
            | "extension_definition"
    )
}

fn collect_container_spans(
    container: Node<'_>,
    content: &str,
    recurse: bool,
    spans: &mut Vec<SemanticSpan>,
) {
    let mut pending_start: Option<usize> = None;
    let mut pending_end = 0usize;

    for child in container_children(container) {
        let kind = child.kind();
        if !child.is_named() && !matches!(kind, "comment" | "block_comment") {
            continue;
        }

        if is_leading_node(kind) {
            if pending_start.is_none() {
                pending_start = Some(child.start_byte());
            }
            pending_end = child.end_byte();
            continue;
        }

        if child.kind() == "self_type" {
            pending_start = None;
            pending_end = 0;
            continue;
        }

        if child.kind() == "enum_case_definitions" {
            collect_enum_case_spans(child, pending_start, content, spans);
            pending_start = None;
            pending_end = 0;
            continue;
        }

        let block_kind = classify_node(
            child,
            content,
            functions_are_methods_in_container(container.kind()),
        );
        if matches!(block_kind, BlockKind::Code | BlockKind::Comment) {
            pending_start = None;
            pending_end = 0;
            continue;
        }

        spans.push(SemanticSpan {
            start_byte: pending_start.unwrap_or(child.start_byte()),
            end_byte: child.end_byte().max(pending_end),
            kind: block_kind,
            tags: semantic_tags(child, content),
        });
        pending_start = None;
        pending_end = 0;

        if recurse && can_contain_nested_members(child.kind()) {
            collect_container_spans(child, content, child.kind() == "package_clause", spans);
        }
    }
}

fn collect_enum_case_spans(
    case_group: Node<'_>,
    inherited_pending_start: Option<usize>,
    content: &str,
    spans: &mut Vec<SemanticSpan>,
) {
    let mut pending_start = inherited_pending_start;
    let mut cursor = case_group.walk();
    for child in case_group.children(&mut cursor) {
        let kind = child.kind();
        if !child.is_named() && !matches!(kind, "comment" | "block_comment") {
            continue;
        }

        if is_leading_node(kind) {
            if pending_start.is_none() {
                pending_start = Some(child.start_byte());
            }
            continue;
        }

        if matches!(kind, "simple_enum_case" | "full_enum_case") {
            spans.push(SemanticSpan {
                start_byte: pending_start.unwrap_or(child.start_byte()),
                end_byte: child.end_byte(),
                kind: BlockKind::Const,
                tags: semantic_tags(child, content),
            });
            pending_start = None;
        }
    }
}

fn container_children(container: Node<'_>) -> Vec<Node<'_>> {
    let Some(target) = container_children_target(container) else {
        return Vec::new();
    };

    let mut cursor = target.walk();
    target.children(&mut cursor).collect()
}

fn container_children_target(container: Node<'_>) -> Option<Node<'_>> {
    match container.kind() {
        "package_clause" | "object_definition" | "package_object" | "class_definition"
        | "trait_definition" | "enum_definition" => container.child_by_field_name("body"),
        "given_definition" => container
            .child_by_field_name("body")
            .filter(|body| is_body_container(body.kind())),
        "extension_definition" => Some(container),
        _ => None,
    }
}

fn is_body_container(kind: &str) -> bool {
    matches!(
        kind,
        "template_body" | "enum_body" | "with_template_body" | "indented_block" | "block"
    )
}

fn is_leading_node(kind: &str) -> bool {
    matches!(kind, "annotation" | "comment" | "block_comment")
}

fn semantic_tags(node: Node<'_>, content: &str) -> Vec<String> {
    let mut tags = Vec::new();
    if node_is_test(node, content) {
        tags.push("test".to_string());
    }
    tags
}

fn node_is_test(node: Node<'_>, content: &str) -> bool {
    is_test_suite_node(node, content) || is_test_case_node(node, content)
}

fn is_test_case_node(node: Node<'_>, content: &str) -> bool {
    match node.kind() {
        "function_definition" | "function_declaration" => {
            declaration_name(node, content).is_some_and(|name| is_test_like_name(name.as_str()))
        }
        "call_expression" => is_test_case_call(node, content),
        _ => false,
    }
}

fn is_test_suite_node(node: Node<'_>, content: &str) -> bool {
    if header_text(node, content).is_some_and(contains_test_suite_marker) {
        return true;
    }

    declaration_name(node, content).is_some_and(|name| {
        is_test_suite_name(name.as_str()) && container_has_obvious_test_members(node, content)
    })
}

fn container_has_obvious_test_members(node: Node<'_>, content: &str) -> bool {
    let mut stack = container_children(node);
    while let Some(child) = stack.pop() {
        if is_test_case_call(child, content)
            || matches!(child.kind(), "function_definition" | "function_declaration")
                && declaration_name(child, content)
                    .is_some_and(|name| is_test_like_name(name.as_str()))
        {
            return true;
        }

        if can_contain_nested_members(child.kind()) {
            stack.extend(container_children(child));
        }
    }
    false
}

fn header_text<'a>(node: Node<'_>, content: &'a str) -> Option<&'a str> {
    let end = node
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    content.get(node.start_byte()..end)
}

fn contains_test_suite_marker(header: &str) -> bool {
    TEST_SUITE_MARKERS
        .iter()
        .any(|marker| header.contains(marker))
}

fn is_test_suite_name(name: &str) -> bool {
    ["Suite", "Spec", "Test", "Tests"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn is_test_like_name(name: &str) -> bool {
    name.strip_prefix("test").is_some_and(|suffix| {
        suffix.is_empty() || !suffix.chars().next().is_some_and(char::is_lowercase)
    }) || name.starts_with("test_")
}

fn declaration_name(node: Node<'_>, content: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(content.as_bytes()).ok())
        .map(str::to_string)
}

fn is_test_case_call(node: Node<'_>, content: &str) -> bool {
    call_target_name(node, content).is_some_and(|name| TEST_CASE_NAMES.contains(&name))
}

fn call_target_name<'a>(mut node: Node<'a>, content: &'a str) -> Option<&'a str> {
    loop {
        match node.kind() {
            "identifier" | "operator_identifier" => return node.utf8_text(content.as_bytes()).ok(),
            "call_expression" | "generic_function" => {
                node = node.child_by_field_name("function")?;
            }
            "field_expression" => {
                return node
                    .child_by_field_name("field")
                    .and_then(|field| field.utf8_text(content.as_bytes()).ok());
            }
            _ => return None,
        }
    }
}

fn find_test_case_call<'a>(node: Node<'a>, content: &str) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "call_expression"
            && is_test_case_call(current, content)
            && current
                .child_by_field_name("arguments")
                .is_some_and(|arguments| {
                    matches!(arguments.kind(), "block" | "case_block" | "colon_argument")
                })
        {
            return Some(current);
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    None
}

fn collect_test_ranges(tree: &Tree, source: &str) -> Result<Vec<ByteSpan>> {
    let mut ranges = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if is_test_case_node(node, source) {
            ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
            continue;
        }

        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    ranges.sort_by_key(|range| (range.start_byte, range.end_byte));
    ranges.dedup_by(|left, right| {
        left.start_byte == right.start_byte && left.end_byte == right.end_byte
    });
    Ok(ranges)
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like,
        },
        BlockKind::Class | BlockKind::Interface | BlockKind::Enum | BlockKind::Impl => {
            SubSplitRegistration {
                semantics: LanguageSubSplitSemantics::StructuralChildren,
                splitter: split_type_like,
            }
        }
        _ => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: crate::sub_splitter::split_code_review_units,
        },
    }
}

fn split_function_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Scala function-like block")?;
    let root = tree.root_node();

    if let Some(function_node) = find_named_descendant_any(root, FUNCTION_LIKE_DECLARATIONS) {
        let Some(body_node) = function_node.child_by_field_name("body") else {
            return crate::sub_splitter::split_code_review_units(block);
        };

        let signature_end = signature_end_for_body(&block.content, body_node.start_byte());
        if signature_end == 0 || signature_end > block.content.len() {
            return crate::sub_splitter::split_code_review_units(block);
        }

        return split_review_units_after_signature(block, signature_end);
    }

    if let Some(test_call) = find_test_case_call(root, &block.content)
        && let Some(body_node) = test_call.child_by_field_name("arguments")
    {
        let signature_end = signature_end_for_body(&block.content, body_node.start_byte());
        if signature_end == 0 || signature_end > block.content.len() {
            return crate::sub_splitter::split_code_review_units(block);
        }

        return split_review_units_after_signature(block, signature_end);
    }

    crate::sub_splitter::split_code_review_units(block)
}

fn split_review_units_after_signature(block: &Block, signature_end: usize) -> Result<Vec<Block>> {
    let mut blocks = vec![create_sub_block(
        block,
        &block.content[..signature_end],
        0,
        signature_end,
        BlockKind::FunctionSignature,
        Vec::new(),
    )];

    let body = &block.content[signature_end..];
    let mut tail_blocks = split_by_paragraph_breaks(body, |chunk, start, end, is_gap| {
        let kind = if is_gap {
            BlockKind::Gap
        } else {
            classify_review_chunk(chunk)
        };
        create_sub_block(
            block,
            chunk,
            signature_end + start,
            signature_end + end,
            kind,
            Vec::new(),
        )
    });
    blocks.append(&mut tail_blocks);

    Ok(blocks)
}

fn signature_end_for_body(content: &str, body_start: usize) -> usize {
    if body_start >= content.len() {
        return content.len();
    }

    let bytes = content.as_bytes();
    if bytes.get(body_start) == Some(&b'{') {
        let mut end = body_start + 1;
        if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
            end += 2;
        } else if bytes.get(end) == Some(&b'\n') {
            end += 1;
        }
        return end.min(content.len());
    }

    body_start
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

fn split_type_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Scala type-like block")?;
    let root = tree.root_node();
    let Some(container) = find_named_descendant_any(root, TYPE_LIKE_DECLARATIONS) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let mut spans = Vec::new();
    collect_container_spans(container, &block.content, false, &mut spans);
    if spans.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    spans.sort_by_key(|span| span.start_byte);

    let mut blocks = Vec::new();
    let mut current = 0usize;
    for span in spans {
        if current < span.start_byte {
            push_non_child_chunk(block, current, span.start_byte, &mut blocks);
        }

        blocks.push(create_sub_block(
            block,
            &block.content[span.start_byte..span.end_byte],
            span.start_byte,
            span.end_byte,
            span.kind,
            span.tags,
        ));
        current = span.end_byte;
    }

    if current < block.content.len() {
        push_non_child_chunk(block, current, block.content.len(), &mut blocks);
    }

    Ok(blocks)
}

fn push_non_child_chunk(parent: &Block, start: usize, end: usize, blocks: &mut Vec<Block>) {
    if end <= start {
        return;
    }

    let chunk = &parent.content[start..end];
    let kind = classify_review_chunk(chunk);
    blocks.push(create_sub_block(
        parent,
        chunk,
        start,
        end,
        kind,
        Vec::new(),
    ));
}

fn parse_tree(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_scala::LANGUAGE.into())
        .context("Failed to load Scala grammar")?;
    parser.parse(source, None).context("Failed to parse Scala")
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

fn create_sub_block(
    parent: &Block,
    text: &str,
    start_offset: usize,
    _end_offset: usize,
    kind: BlockKind,
    tags: Vec<String>,
) -> Block {
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&ch| ch == '\n').count();
    let chunk_newlines = text.chars().filter(|&ch| ch == '\n').count();

    let start_line = parent.start_line + offset_newlines;
    let end_line = start_line + chunk_newlines + if text.ends_with('\n') { 0 } else { 1 };

    let mut combined_tags = parent.tags.clone();
    for tag in tags {
        if !combined_tags.iter().any(|existing| existing == &tag) {
            combined_tags.push(tag);
        }
    }

    Block {
        hash: TreeHash::from_content(text),
        content: text.to_string(),
        kind,
        tags: combined_tags,
        complexity: None,
        start_line,
        end_line,
    }
}
