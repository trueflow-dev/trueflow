use super::{
    LanguageRegistration, LanguageSubSplitSemantics, SubSplitRegistration, TopLevelRegistration,
    default_map_kind, no_attribute_nodes, no_nested_blocks, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::complexity;

use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser};

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
    tree_sitter_json::LANGUAGE.into()
}

fn split_top_level(root: Node<'_>, content: &str, lang: Language) -> Result<Vec<Block>> {
    let values = root_json_values(root);
    if values.is_empty() {
        return Ok(vec![create_file_block(
            content,
            BlockKind::Content,
            content,
            0,
            content.len(),
            lang,
        )?]);
    }

    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for value in values {
        push_non_empty_gap(&mut blocks, content, last_end, value.start_byte(), lang)?;
        blocks.extend(split_json_value_at_file_scope(value, content, lang)?);
        last_end = value.end_byte();
    }

    push_non_empty_gap(&mut blocks, content, last_end, content.len(), lang)?;
    Ok(blocks)
}

fn split_json_value_at_file_scope(
    value: Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    Ok(match value.kind() {
        "object" => {
            split_object_children(value.start_byte(), value.end_byte(), value, content, lang)?
        }
        "array" => {
            split_array_children(value.start_byte(), value.end_byte(), value, content, lang)?
        }
        _ => vec![create_file_block(
            &content[value.start_byte()..value.end_byte()],
            BlockKind::Content,
            content,
            value.start_byte(),
            value.end_byte(),
            lang,
        )?],
    })
}

fn split_object_children(
    range_start: usize,
    range_end: usize,
    object: Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let pairs = object_pairs(object);
    if pairs.is_empty() {
        return Ok(vec![create_file_block(
            &content[range_start..range_end],
            BlockKind::Content,
            content,
            range_start,
            range_end,
            lang,
        )?]);
    }

    let mut blocks = Vec::new();
    let mut last_end = range_start;
    for pair in pairs {
        push_non_empty_gap(&mut blocks, content, last_end, pair.start_byte(), lang)?;
        blocks.push(create_file_block(
            &content[pair.start_byte()..pair.end_byte()],
            classify_json_pair(pair),
            content,
            pair.start_byte(),
            pair.end_byte(),
            lang,
        )?);
        last_end = pair.end_byte();
    }

    push_non_empty_gap(&mut blocks, content, last_end, range_end, lang)?;
    Ok(blocks)
}

fn split_array_children(
    range_start: usize,
    range_end: usize,
    array: Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let items = array_items(array);
    if items.is_empty() {
        return Ok(vec![create_file_block(
            &content[range_start..range_end],
            BlockKind::Content,
            content,
            range_start,
            range_end,
            lang,
        )?]);
    }

    let mut blocks = Vec::new();
    let mut last_end = range_start;
    for item in items {
        push_non_empty_gap(&mut blocks, content, last_end, item.start_byte(), lang)?;
        blocks.push(create_file_block(
            &content[item.start_byte()..item.end_byte()],
            classify_json_value_node(item),
            content,
            item.start_byte(),
            item.end_byte(),
            lang,
        )?);
        last_end = item.end_byte();
    }

    push_non_empty_gap(&mut blocks, content, last_end, range_end, lang)?;
    Ok(blocks)
}

fn root_json_values(root: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

fn object_pairs(object: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = object.walk();
    object
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "pair")
        .collect()
}

fn array_items(array: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = array.walk();
    array.named_children(&mut cursor).collect()
}

fn classify_json_pair(pair: Node<'_>) -> BlockKind {
    pair.child_by_field_name("value")
        .map(classify_json_value_node)
        .unwrap_or(BlockKind::Content)
}

fn classify_json_value_node(node: Node<'_>) -> BlockKind {
    match node.kind() {
        "object" if object_pairs(node).len() > 1 => BlockKind::Section,
        "array" if array_items(node).len() > 1 => BlockKind::List,
        _ => BlockKind::Content,
    }
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Section | BlockKind::List => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::StructuralChildren,
            splitter: split_composite_children,
        },
        _ => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: identity_review_unit,
        },
    }
}

fn identity_review_unit(block: &Block) -> Result<Vec<Block>> {
    Ok(vec![block.clone()])
}

fn split_composite_children(block: &Block) -> Result<Vec<Block>> {
    let needs_pair_wrapper = looks_like_json_pair(&block.content);
    let source = if needs_pair_wrapper {
        format!("{{{}}}", block.content)
    } else {
        block.content.clone()
    };
    let offset_adjustment = usize::from(needs_pair_wrapper);

    let tree = parse_tree(&source)?;
    let root = tree.root_node();
    let Some(composite) = composite_node_from_block(root, needs_pair_wrapper) else {
        return Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )?]);
    };

    let mut child_spans = match composite.kind() {
        "object" => object_pairs(composite),
        "array" => array_items(composite),
        _ => Vec::new(),
    };
    child_spans.retain(|child| child.end_byte() > child.start_byte());

    if child_spans.is_empty() {
        return Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )?]);
    }

    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for child in child_spans {
        let start = child.start_byte().saturating_sub(offset_adjustment);
        let end = child.end_byte().saturating_sub(offset_adjustment);
        push_container_interstitial(
            &mut blocks,
            block,
            last_end,
            start,
            is_json_structural_noise,
        )?;

        let kind = if child.kind() == "pair" {
            classify_json_pair(child)
        } else {
            classify_json_value_node(child)
        };
        blocks.push(create_sub_block(block, start, end, kind)?);
        last_end = end;
    }

    push_container_interstitial(
        &mut blocks,
        block,
        last_end,
        block.content.len(),
        is_json_structural_noise,
    )?;

    if blocks.is_empty() {
        Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )?])
    } else {
        Ok(blocks)
    }
}

fn looks_like_json_pair(content: &str) -> bool {
    content.trim_start().starts_with('"')
}

fn composite_node_from_block(root: Node<'_>, needs_pair_wrapper: bool) -> Option<Node<'_>> {
    let value = root_json_values(root).into_iter().next()?;
    if !needs_pair_wrapper {
        return matches!(value.kind(), "object" | "array").then_some(value);
    }

    let pair = object_pairs(value).into_iter().next()?;
    let value = pair.child_by_field_name("value")?;
    matches!(value.kind(), "object" | "array").then_some(value)
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

    let chunk = &content[start..end];
    if chunk.is_empty() || chunk.trim().is_empty() {
        return Ok(());
    }

    blocks.push(create_file_block(
        chunk,
        BlockKind::Gap,
        content,
        start,
        end,
        lang,
    )?);
    Ok(())
}

fn push_container_interstitial(
    blocks: &mut Vec<Block>,
    parent: &Block,
    start: usize,
    end: usize,
    is_noise: fn(&str) -> bool,
) -> Result<()> {
    if end <= start {
        return Ok(());
    }

    let chunk = &parent.content[start..end];
    if chunk.is_empty() {
        return Ok(());
    }

    let kind = if chunk.trim().is_empty() || is_noise(chunk) {
        BlockKind::Gap
    } else {
        BlockKind::Preamble
    };
    blocks.push(create_sub_block(parent, start, end, kind)?);
    Ok(())
}

fn is_json_structural_noise(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '{' | '}' | '[' | ']' | ','))
}

fn parse_tree(source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .context("Failed to load JSON grammar")?;
    parser.parse(source, None).context("Failed to parse JSON")
}

fn create_file_block(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Result<Block> {
    let mut block = Block::from_file_range(full_source, kind, ByteSpan::new(start_byte, end_byte))?;
    assert_eq!(
        block.content, text,
        "JSON top-level range must name content"
    );
    block.complexity = complexity::calculate(&block.content, lang);
    Ok(block)
}

fn create_sub_block(
    parent: &Block,
    start_offset: usize,
    end_offset: usize,
    kind: BlockKind,
) -> Result<Block> {
    let mut block =
        Block::from_parent_range(parent, kind, ByteSpan::new(start_offset, end_offset))?;
    block.tags = parent.tags.clone();
    Ok(block)
}
