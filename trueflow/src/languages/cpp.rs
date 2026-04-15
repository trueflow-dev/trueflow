use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, no_attribute_nodes,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::code_comments;
use crate::hashing::TreeHash;
use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

const FUNCTION_LIKE_DECLARATIONS: &[&str] = &["function_definition"];

pub(crate) fn registration() -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks,
            collect_test_ranges,
            custom_splitter: None,
        },
        sub_split,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_cpp::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    classify_node(node, content, false).unwrap_or(BlockKind::Code)
}

fn classify_node(node: Node<'_>, content: &str, in_container: bool) -> Option<BlockKind> {
    match node.kind() {
        "comment" => Some(BlockKind::Comment),
        "preproc_include" => Some(BlockKind::Import),
        "namespace_definition" => Some(BlockKind::Module),
        "alias_declaration" => Some(BlockKind::Type),
        "using_declaration" => Some(BlockKind::Import),
        "class_specifier" => Some(BlockKind::Class),
        "struct_specifier" => Some(BlockKind::Struct),
        "enum_specifier" => Some(BlockKind::Enum),
        "type_definition" => Some(classify_type_definition(node, content)),
        "template_declaration" => Some(classify_template_declaration(node, content, in_container)),
        "function_definition" => Some(classify_function_definition(node, content, in_container)),
        "field_declaration" => Some(classify_field_declaration(node, content)),
        "declaration" => Some(classify_declaration(node, content, in_container)),
        _ => None,
    }
}

fn classify_type_definition(node: Node<'_>, content: &str) -> BlockKind {
    let Some(type_node) = node.child_by_field_name("type") else {
        return BlockKind::Type;
    };
    classify_node(type_node, content, false).unwrap_or(BlockKind::Type)
}

fn classify_template_declaration(node: Node<'_>, content: &str, in_container: bool) -> BlockKind {
    let Some(inner) = template_inner_node(node) else {
        return BlockKind::Code;
    };
    classify_node(inner, content, in_container).unwrap_or(BlockKind::Code)
}

fn template_inner_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if node.kind() != "template_declaration" {
        return None;
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| !matches!(child.kind(), "template_parameter_list" | "requires_clause"))
}

fn classify_function_definition(node: Node<'_>, content: &str, in_container: bool) -> BlockKind {
    if in_container {
        return BlockKind::Method;
    }

    let Some(name) = cpp_function_name(node, content) else {
        return BlockKind::Function;
    };
    if name.contains("::") {
        BlockKind::Method
    } else {
        BlockKind::Function
    }
}

fn classify_field_declaration(node: Node<'_>, content: &str) -> BlockKind {
    if node_contains_kind(node, "function_declarator")
        || node_contains_kind(node, "function_field_declarator")
    {
        return BlockKind::Method;
    }

    if node_contains_const_qualifier(node, content) {
        BlockKind::Const
    } else {
        BlockKind::Variable
    }
}

fn classify_declaration(node: Node<'_>, content: &str, in_container: bool) -> BlockKind {
    if node_contains_kind(node, "function_declarator")
        || node_contains_kind(node, "function_field_declarator")
    {
        return if in_container {
            BlockKind::Method
        } else {
            BlockKind::FunctionSignature
        };
    }

    let text = node.utf8_text(content.as_bytes()).unwrap_or_default();
    if text.trim_start().starts_with("using ") {
        return BlockKind::Import;
    }
    if text.trim_start().starts_with("typedef ") {
        return BlockKind::Type;
    }

    if node_contains_const_qualifier(node, content)
        || text.trim_start().starts_with("const ")
        || text.trim_start().starts_with("constexpr ")
        || text.trim_start().starts_with("consteval ")
        || text.trim_start().starts_with("constinit ")
    {
        BlockKind::Const
    } else {
        BlockKind::Variable
    }
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    collect_nested_blocks_for_node(node, content)
}

fn collect_nested_blocks_for_node(node: Node<'_>, content: &str) -> Vec<NestedBlock> {
    if node.kind() == "template_declaration"
        && let Some(inner) = template_inner_node(node)
    {
        return collect_nested_blocks_for_node(inner, content);
    }

    match node.kind() {
        "namespace_definition" => {
            let Some(body) = node.child_by_field_name("body") else {
                return Vec::new();
            };
            collect_recursive_child_blocks(body, content, false)
        }
        "class_specifier" | "struct_specifier" => {
            let Some(body) = node.child_by_field_name("body") else {
                return Vec::new();
            };
            collect_recursive_child_blocks(body, content, true)
        }
        _ => Vec::new(),
    }
}

fn collect_recursive_child_blocks(
    body: Node<'_>,
    content: &str,
    in_container: bool,
) -> Vec<NestedBlock> {
    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "access_specifier" {
            continue;
        }

        if let Some(kind) = classify_node(child, content, in_container)
            && !matches!(kind, BlockKind::Code)
        {
            blocks.push(NestedBlock {
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                kind,
            });
        }

        blocks.extend(collect_nested_blocks_for_node(child, content));
    }

    blocks
}

fn collect_test_ranges(tree: &Tree, source: &str) -> Result<Vec<ByteSpan>> {
    let mut ranges = Vec::new();
    collect_test_ranges_from_node(tree.root_node(), source, &mut ranges)?;
    Ok(ranges)
}

fn collect_test_ranges_from_node(
    node: Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    if node.kind() == "function_definition"
        && let Some(name) = cpp_function_name(node, source)
        && is_cpp_test_name(&name)
    {
        ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_test_ranges_from_node(child, source, ranges)?;
    }

    Ok(())
}

fn is_cpp_test_name(name: &str) -> bool {
    let base_name = name.rsplit("::").next().unwrap_or(name);
    base_name.starts_with("test_") || base_name.starts_with("Test")
}

fn cpp_function_name(function_node: Node<'_>, source: &str) -> Option<String> {
    let declarator = function_node.child_by_field_name("declarator")?;
    cpp_declarator_name(declarator, source)
}

fn cpp_declarator_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "type_identifier"
        | "qualified_identifier"
        | "destructor_name"
        | "operator_name" => {
            return node
                .utf8_text(source.as_bytes())
                .ok()
                .map(std::string::ToString::to_string);
        }
        _ => {}
    }

    if let Some(name) = node.child_by_field_name("name")
        && let Ok(text) = name.utf8_text(source.as_bytes())
    {
        return Some(text.to_string());
    }

    if let Some(declarator) = node.child_by_field_name("declarator")
        && let Some(name) = cpp_declarator_name(declarator, source)
    {
        return Some(name);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name) = cpp_declarator_name(child, source) {
            return Some(name);
        }
    }

    None
}

fn node_contains_kind(node: Node<'_>, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }

    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| node_contains_kind(child, kind))
}

fn node_contains_const_qualifier(node: Node<'_>, content: &str) -> bool {
    if node.kind() == "type_qualifier" {
        return node
            .utf8_text(content.as_bytes())
            .map(|text| text.trim() == "const")
            .unwrap_or(false);
    }

    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| node_contains_const_qualifier(child, content))
}

fn sub_split(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like,
        },
        BlockKind::Class | BlockKind::Struct | BlockKind::Enum | BlockKind::Module => {
            SubSplitRegistration {
                semantics: LanguageSubSplitSemantics::StructuralChildren,
                splitter: split_container_like,
            }
        }
        _ => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: crate::sub_splitter::split_code_review_units,
        },
    }
}

fn split_function_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse C++ function-like block")?;
    let root = tree.root_node();
    let Some(function_node) = find_named_descendant_any(root, FUNCTION_LIKE_DECLARATIONS) else {
        return crate::sub_splitter::split_code_review_units(block);
    };
    let Some(body_node) = function_node.child_by_field_name("body") else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let signature_end = signature_end_offset(&block.content, body_node.start_byte());
    if signature_end == 0 || signature_end > block.content.len() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    split_review_units_after_signature(block, signature_end)
}

fn split_container_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse C++ container block")?;
    let root = tree.root_node();
    let Some(container) = find_structural_container_descendant(root) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let items = match container.kind() {
        "namespace_definition" => {
            let Some(body) = container.child_by_field_name("body") else {
                return crate::sub_splitter::split_code_review_units(block);
            };
            collect_direct_child_blocks(block, body, false)
        }
        "class_specifier" | "struct_specifier" => {
            let Some(body) = container.child_by_field_name("body") else {
                return crate::sub_splitter::split_code_review_units(block);
            };
            collect_direct_child_blocks(block, body, true)
        }
        "enum_specifier" => collect_enum_member_blocks(block, container),
        _ => Vec::new(),
    };

    if items.is_empty() {
        crate::sub_splitter::split_code_review_units(block)
    } else {
        Ok(items)
    }
}

fn parse_tree(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .context("Failed to load C++ grammar")?;
    parser.parse(source, None).context("Failed to parse C++")
}

fn find_named_descendant_any<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
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

fn find_structural_container_descendant<'a>(node: Node<'a>) -> Option<Node<'a>> {
    match node.kind() {
        "namespace_definition" | "class_specifier" | "struct_specifier" | "enum_specifier" => {
            return Some(node);
        }
        "template_declaration" => {
            if let Some(inner) = template_inner_node(node)
                && let Some(found) = find_structural_container_descendant(inner)
            {
                return Some(found);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_structural_container_descendant(child) {
            return Some(found);
        }
    }

    None
}

fn collect_direct_child_blocks(parent: &Block, body: Node<'_>, in_container: bool) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "access_specifier" {
            continue;
        }
        let Some(kind) = classify_node(child, &parent.content, in_container) else {
            continue;
        };
        if matches!(kind, BlockKind::Code) {
            continue;
        }

        blocks.push(create_sub_block_with_kind(
            parent,
            &parent.content[child.start_byte()..child.end_byte()],
            child.start_byte(),
            kind,
        ));
    }
    blocks
}

fn collect_enum_member_blocks(parent: &Block, enum_node: Node<'_>) -> Vec<Block> {
    let Some(body) = first_named_child(enum_node) else {
        return Vec::new();
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "enumerator" {
            continue;
        }
        blocks.push(create_sub_block_with_kind(
            parent,
            &parent.content[child.start_byte()..child.end_byte()],
            child.start_byte(),
            BlockKind::Const,
        ));
    }
    blocks
}

fn first_named_child<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn split_review_units_after_signature(block: &Block, signature_end: usize) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut blocks = vec![create_sub_block_with_kind(
        block,
        &content[..signature_end],
        0,
        BlockKind::FunctionSignature,
    )];

    let rest = &content[signature_end..];
    let mut start = 0;
    for gap in paragraph_break_regex().find_iter(rest) {
        if start < gap.start() {
            let chunk = &rest[start..gap.start()];
            if !chunk.is_empty() {
                push_review_chunk(block, &mut blocks, chunk, signature_end + start);
            }
        }

        blocks.push(create_sub_block_with_kind(
            block,
            &rest[gap.start()..gap.end()],
            signature_end + gap.start(),
            BlockKind::Gap,
        ));
        start = gap.end();
    }

    if start < rest.len() {
        let chunk = &rest[start..];
        if !chunk.is_empty() {
            push_review_chunk(block, &mut blocks, chunk, signature_end + start);
        }
    }

    Ok(blocks)
}

fn push_review_chunk(parent: &Block, blocks: &mut Vec<Block>, chunk: &str, start_offset: usize) {
    if let Some(comment_end) = leading_comment_prefix_len(chunk) {
        let comment = &chunk[..comment_end];
        blocks.push(create_sub_block_with_kind(
            parent,
            comment,
            start_offset,
            BlockKind::Comment,
        ));

        let remainder = &chunk[comment_end..];
        if !remainder.trim().is_empty() {
            blocks.push(create_sub_block_with_kind(
                parent,
                remainder,
                start_offset + comment_end,
                BlockKind::CodeParagraph,
            ));
        }
        return;
    }

    let kind = classify_code_chunk(chunk);
    if !matches!(kind, BlockKind::Gap) {
        blocks.push(create_sub_block_with_kind(
            parent,
            chunk,
            start_offset,
            kind,
        ));
    }
}

fn leading_comment_prefix_len(chunk: &str) -> Option<usize> {
    let mut offset = 0;
    let mut saw_comment = false;

    for line in chunk.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            offset += line.len();
            continue;
        }
        if code_comments::line_is_c_style_comment(trimmed) {
            saw_comment = true;
            offset += line.len();
            continue;
        }

        return saw_comment.then_some(offset);
    }

    None
}

fn classify_code_chunk(chunk: &str) -> BlockKind {
    if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if code_comments::chunk_is_hash_or_c_style_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
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

fn create_sub_block_with_kind(
    parent: &Block,
    content: &str,
    start_offset: usize,
    kind: BlockKind,
) -> Block {
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&ch| ch == '\n').count();
    let chunk_newlines = content.chars().filter(|&ch| ch == '\n').count();

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
