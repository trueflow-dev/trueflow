use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, no_attribute_nodes,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::code_comments;

use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result, anyhow};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

const TYPE_LIKE_DECLARATIONS: &[&str] = &[
    "struct_declaration",
    "enum_declaration",
    "union_declaration",
    "opaque_declaration",
];
const FUNCTION_LIKE_DECLARATIONS: &[&str] = &["function_declaration", "test_declaration"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageKind {
    Const,
    Var,
}

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
    tree_sitter_zig::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    classify_node(node, content, false).unwrap_or(BlockKind::Code)
}

fn classify_node(node: Node<'_>, content: &str, in_container: bool) -> Option<BlockKind> {
    match node.kind() {
        "variable_declaration" => Some(classify_variable_declaration(node, content)),
        "function_declaration" => Some(if in_container {
            BlockKind::Method
        } else {
            BlockKind::Function
        }),
        "test_declaration" => Some(BlockKind::Function),
        "using_namespace_declaration" => Some(BlockKind::Import),
        "container_field" => Some(BlockKind::Variable),
        _ => None,
    }
}

fn classify_variable_declaration(node: Node<'_>, content: &str) -> BlockKind {
    if let Some(kind) = stable_container_kind(node) {
        return kind;
    }

    if variable_declaration_is_import(node, content) {
        return BlockKind::Import;
    }

    match variable_storage_kind(node, content) {
        Some(StorageKind::Const) => BlockKind::Const,
        Some(StorageKind::Var) => BlockKind::Variable,
        None => BlockKind::Code,
    }
}

fn stable_container_kind(node: Node<'_>) -> Option<BlockKind> {
    let container = stable_container_node(node)?;
    Some(match container.kind() {
        "struct_declaration" => BlockKind::Struct,
        "enum_declaration" => BlockKind::Enum,
        "union_declaration" | "opaque_declaration" => BlockKind::Type,
        _ => return None,
    })
}

fn stable_container_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    match node.kind() {
        "variable_declaration" => direct_named_child_any(node, TYPE_LIKE_DECLARATIONS),
        kind if TYPE_LIKE_DECLARATIONS.contains(&kind) => Some(node),
        _ => None,
    }
}

fn variable_storage_kind(node: Node<'_>, content: &str) -> Option<StorageKind> {
    let first_child = first_named_child(node)?;
    let prefix = &content[node.start_byte()..first_child.start_byte()];
    if contains_keyword(prefix, "const") {
        Some(StorageKind::Const)
    } else if contains_keyword(prefix, "var") {
        Some(StorageKind::Var)
    } else {
        None
    }
}

fn variable_declaration_is_import(node: Node<'_>, content: &str) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| {
        child.kind() == "builtin_function"
            && builtin_identifier_text(child, content)
                .is_some_and(|name| matches!(name, "@import" | "@cImport"))
    })
}

fn builtin_identifier_text<'a>(node: Node<'a>, content: &'a str) -> Option<&'a str> {
    direct_named_child_any(node, &["builtin_identifier"])?
        .utf8_text(content.as_bytes())
        .ok()
}

fn contains_keyword(text: &str, keyword: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|token| token == keyword)
}

fn first_named_child<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn direct_named_child_any<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| kinds.iter().any(|kind| *kind == child.kind()))
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    let Some(container) = stable_container_node(node) else {
        return Vec::new();
    };

    collect_container_member_spans(container, content)
}

fn collect_container_member_spans(container: Node<'_>, content: &str) -> Vec<NestedBlock> {
    let mut cursor = container.walk();
    container
        .named_children(&mut cursor)
        .filter_map(|child| {
            let kind = classify_node(child, content, true)?;
            (!matches!(kind, BlockKind::Code)).then_some(NestedBlock {
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                kind,
            })
        })
        .collect()
}

fn collect_test_ranges(tree: &Tree, _source: &str) -> Result<Vec<ByteSpan>> {
    let mut ranges = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "test_declaration" {
            ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
            continue;
        }

        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    Ok(ranges)
}

fn sub_split(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like,
        },
        BlockKind::Struct | BlockKind::Enum | BlockKind::Type => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::StructuralChildren,
            splitter: split_type_like,
        },
        _ => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: crate::sub_splitter::split_code_review_units,
        },
    }
}

fn split_function_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Zig function-like block")?;
    let root = tree.root_node();
    let Some(function_node) = find_named_descendant_any(root, FUNCTION_LIKE_DECLARATIONS) else {
        return crate::sub_splitter::split_code_review_units(block);
    };
    let Some(body_node) = function_like_body(function_node) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let signature_end = signature_end_offset(&block.content, body_node.start_byte());
    if signature_end == 0 || signature_end > block.content.len() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    split_review_units_after_signature(block, signature_end)
}

fn split_type_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Zig type-like block")?;
    let root = tree.root_node();
    let Some(container) = find_stable_container_descendant(root) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let items = collect_container_member_blocks(block, container)?;
    if items.is_empty() {
        crate::sub_splitter::split_code_review_units(block)
    } else {
        Ok(items)
    }
}

fn parse_tree(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_zig::LANGUAGE.into())
        .context("Failed to load Zig grammar")?;
    parser.parse(source, None).context("Failed to parse Zig")
}

fn function_like_body(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
        .or_else(|| direct_named_child_any(node, &["block"]))
}

fn find_stable_container_descendant<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if let Some(container) = stable_container_node(current) {
            return Some(container);
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    None
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

fn collect_container_member_blocks(parent: &Block, container: Node<'_>) -> Result<Vec<Block>> {
    let mut cursor = container.walk();
    let mut blocks = Vec::new();
    for child in container.named_children(&mut cursor) {
        let Some(kind) = classify_node(child, &parent.content, true) else {
            continue;
        };
        if matches!(kind, BlockKind::Code) {
            continue;
        }

        blocks.push(create_container_member_block(
            parent,
            &parent.content[child.start_byte()..child.end_byte()],
            child.start_byte(),
            kind,
        )?);
    }
    Ok(blocks)
}

fn create_container_member_block(
    parent: &Block,
    content: &str,
    start_offset: usize,
    kind: BlockKind,
) -> Result<Block> {
    let mut block = create_sub_block_with_kind(parent, content, start_offset, kind)?;
    block.tags.retain(|tag| tag != "test");
    if content.trim_start().starts_with("test ") && !block.has_tag("test") {
        block.tags.push("test".to_string());
    }
    Ok(block)
}

fn split_review_units_after_signature(block: &Block, signature_end: usize) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut blocks = vec![create_sub_block_with_kind(
        block,
        &content[..signature_end],
        0,
        BlockKind::FunctionSignature,
    )?];

    let rest = &content[signature_end..];
    let mut start = 0;
    for gap in paragraph_break_regex().find_iter(rest) {
        if start < gap.start() {
            let chunk = &rest[start..gap.start()];
            if !chunk.is_empty() {
                push_review_chunk(block, &mut blocks, chunk, signature_end + start)?;
            }
        }

        blocks.push(create_sub_block_with_kind(
            block,
            &rest[gap.start()..gap.end()],
            signature_end + gap.start(),
            BlockKind::Gap,
        )?);
        start = gap.end();
    }

    if start < rest.len() {
        let chunk = &rest[start..];
        if !chunk.is_empty() {
            push_review_chunk(block, &mut blocks, chunk, signature_end + start)?;
        }
    }

    Ok(blocks)
}

fn push_review_chunk(
    parent: &Block,
    blocks: &mut Vec<Block>,
    chunk: &str,
    start_offset: usize,
) -> Result<()> {
    if let Some(comment_end) = leading_comment_prefix_len(chunk) {
        let comment = &chunk[..comment_end];
        blocks.push(create_sub_block_with_kind(
            parent,
            comment,
            start_offset,
            BlockKind::Comment,
        )?);

        let remainder = &chunk[comment_end..];
        if !remainder.trim().is_empty() {
            blocks.push(create_sub_block_with_kind(
                parent,
                remainder,
                start_offset + comment_end,
                BlockKind::CodeParagraph,
            )?);
        }
        return Ok(());
    }

    let kind = classify_code_chunk(chunk);
    if !matches!(kind, BlockKind::Gap) {
        blocks.push(create_sub_block_with_kind(
            parent,
            chunk,
            start_offset,
            kind,
        )?);
    }
    Ok(())
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
    } else if code_comments::chunk_is_comment_only(chunk) {
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
) -> Result<Block> {
    let end_offset = start_offset
        .checked_add(content.len())
        .ok_or_else(|| anyhow!("Zig sub-split end offset overflow"))?;
    let mut block =
        Block::from_parent_range(parent, kind, ByteSpan::new(start_offset, end_offset))?;
    assert_eq!(
        block.content, content,
        "Zig sub-split range must name content"
    );
    block.tags = parent.tags.clone();
    Ok(block)
}
