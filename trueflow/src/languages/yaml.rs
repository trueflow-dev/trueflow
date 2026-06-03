use super::{
    LanguageRegistration, LanguageSubSplitSemantics, SubSplitRegistration, TopLevelRegistration,
    default_map_kind, no_attribute_nodes, no_nested_blocks, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::complexity;
use crate::hashing::TreeHash;
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
    tree_sitter_yaml::LANGUAGE.into()
}

fn split_top_level(root: Node<'_>, content: &str, lang: Language) -> Result<Vec<Block>> {
    let values = root_yaml_values(root);
    if values.is_empty() {
        return Ok(vec![create_file_block(
            content,
            BlockKind::Content,
            content,
            0,
            content.len(),
            lang,
        )]);
    }

    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for value in values {
        push_non_empty_gap(&mut blocks, content, last_end, value.start_byte(), lang);
        blocks.extend(split_yaml_value_at_file_scope(value, content, lang));
        last_end = value.end_byte();
    }

    push_non_empty_gap(&mut blocks, content, last_end, content.len(), lang);
    Ok(blocks)
}

fn split_yaml_value_at_file_scope(value: Node<'_>, content: &str, lang: Language) -> Vec<Block> {
    let semantic = semantic_yaml_node(value);
    match semantic.kind() {
        "block_mapping" | "flow_mapping" => split_mapping_children(
            value.start_byte(),
            value.end_byte(),
            semantic,
            content,
            lang,
        ),
        "block_sequence" | "flow_sequence" => split_sequence_children(
            value.start_byte(),
            value.end_byte(),
            semantic,
            content,
            lang,
        ),
        _ => vec![create_file_block(
            &content[value.start_byte()..value.end_byte()],
            BlockKind::Content,
            content,
            value.start_byte(),
            value.end_byte(),
            lang,
        )],
    }
}

fn split_mapping_children(
    range_start: usize,
    range_end: usize,
    mapping: Node<'_>,
    content: &str,
    lang: Language,
) -> Vec<Block> {
    let pairs = mapping_pairs(mapping);
    if pairs.is_empty() {
        return vec![create_file_block(
            &content[range_start..range_end],
            BlockKind::Content,
            content,
            range_start,
            range_end,
            lang,
        )];
    }

    let mut blocks = Vec::new();
    let mut last_end = range_start;
    for pair in pairs {
        push_non_empty_gap(&mut blocks, content, last_end, pair.start_byte(), lang);
        blocks.push(create_file_block(
            &content[pair.start_byte()..pair.end_byte()],
            classify_mapping_pair(pair),
            content,
            pair.start_byte(),
            pair.end_byte(),
            lang,
        ));
        last_end = pair.end_byte();
    }

    push_non_empty_gap(&mut blocks, content, last_end, range_end, lang);
    blocks
}

fn split_sequence_children(
    range_start: usize,
    range_end: usize,
    sequence: Node<'_>,
    content: &str,
    lang: Language,
) -> Vec<Block> {
    let items = sequence_items(sequence);
    if items.is_empty() {
        return vec![create_file_block(
            &content[range_start..range_end],
            BlockKind::Content,
            content,
            range_start,
            range_end,
            lang,
        )];
    }

    let mut blocks = Vec::new();
    let mut last_end = range_start;
    for item in items {
        let start = item.start_byte();
        let end = item.end_byte();
        push_non_empty_gap(&mut blocks, content, last_end, start, lang);
        blocks.push(create_file_block(
            &content[start..end],
            classify_sequence_item(item),
            content,
            start,
            end,
            lang,
        ));
        last_end = end;
    }

    push_non_empty_gap(&mut blocks, content, last_end, range_end, lang);
    blocks
}

fn root_yaml_values(root: Node<'_>) -> Vec<Node<'_>> {
    let mut values = Vec::new();
    let mut root_cursor = root.walk();
    for child in root.named_children(&mut root_cursor) {
        if child.kind() != "document" {
            continue;
        }

        let mut doc_cursor = child.walk();
        for doc_child in child.named_children(&mut doc_cursor) {
            if matches!(
                doc_child.kind(),
                "yaml_directive" | "tag_directive" | "reserved_directive"
            ) {
                continue;
            }
            values.push(doc_child);
        }
    }
    values
}

fn semantic_yaml_node(mut node: Node<'_>) -> Node<'_> {
    while matches!(node.kind(), "block_node" | "flow_node") {
        let mut cursor = node.walk();
        let Some(child) = node
            .named_children(&mut cursor)
            .find(|child| !matches!(child.kind(), "anchor" | "tag"))
        else {
            return node;
        };
        node = child;
    }
    node
}

fn mapping_pairs(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    match node.kind() {
        "block_mapping" => node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "block_mapping_pair")
            .collect(),
        "flow_mapping" => node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "flow_pair")
            .collect(),
        _ => Vec::new(),
    }
}

fn sequence_items(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    match node.kind() {
        "block_sequence" => node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "block_sequence_item")
            .collect(),
        "flow_sequence" => node.named_children(&mut cursor).collect(),
        _ => Vec::new(),
    }
}

fn sequence_item_value(item: Node<'_>) -> Option<Node<'_>> {
    match item.kind() {
        "block_sequence_item" => {
            let mut cursor = item.walk();
            item.named_children(&mut cursor).next()
        }
        _ => Some(item),
    }
}

fn classify_mapping_pair(pair: Node<'_>) -> BlockKind {
    pair.child_by_field_name("value")
        .map(classify_yaml_value_node)
        .unwrap_or(BlockKind::Content)
}

fn classify_sequence_item(item: Node<'_>) -> BlockKind {
    sequence_item_value(item)
        .map(classify_yaml_value_node)
        .unwrap_or(BlockKind::Content)
}

fn classify_yaml_value_node(node: Node<'_>) -> BlockKind {
    let semantic = semantic_yaml_node(node);
    match semantic.kind() {
        "block_mapping" | "flow_mapping" if mapping_pairs(semantic).len() > 1 => BlockKind::Section,
        "block_sequence" | "flow_sequence" if sequence_items(semantic).len() > 1 => BlockKind::List,
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
    let tree = parse_tree(&block.content)?;
    let root = tree.root_node();
    let Some(root_value) = root_yaml_values(root).into_iter().next() else {
        return Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )]);
    };
    let Some(composite) = composite_node_from_block(root_value) else {
        return Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )]);
    };

    let child_nodes = match composite.kind() {
        "block_mapping" | "flow_mapping" => mapping_pairs(composite),
        "block_sequence" | "flow_sequence" => sequence_items(composite),
        _ => Vec::new(),
    };

    if child_nodes.is_empty() {
        return Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )]);
    }

    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for child in child_nodes {
        let start = child.start_byte();
        let end = child.end_byte();
        push_container_interstitial(
            &mut blocks,
            block,
            last_end,
            start,
            is_yaml_structural_noise,
        );

        let kind = match child.kind() {
            "block_mapping_pair" | "flow_pair" => classify_mapping_pair(child),
            _ => classify_sequence_item(child),
        };
        blocks.push(create_sub_block(block, start, end, kind));
        last_end = end;
    }

    push_container_interstitial(
        &mut blocks,
        block,
        last_end,
        block.content.len(),
        is_yaml_structural_noise,
    );

    if blocks.is_empty() {
        Ok(vec![create_sub_block(
            block,
            0,
            block.content.len(),
            BlockKind::Content,
        )])
    } else {
        Ok(blocks)
    }
}

fn composite_node_from_block(root_value: Node<'_>) -> Option<Node<'_>> {
    let semantic = semantic_yaml_node(root_value);
    match semantic.kind() {
        "block_mapping" | "flow_mapping" => {
            let pairs = mapping_pairs(semantic);
            if pairs.len() == 1 {
                let pair = pairs[0];
                if let Some(value) = pair.child_by_field_name("value") {
                    let value = semantic_yaml_node(value);
                    if matches!(
                        classify_yaml_value_node(value),
                        BlockKind::Section | BlockKind::List
                    ) {
                        return Some(value);
                    }
                }
            }
            Some(semantic)
        }
        "block_sequence" | "flow_sequence" => {
            let items = sequence_items(semantic);
            if items.len() == 1
                && let Some(value) = sequence_item_value(items[0]).map(semantic_yaml_node)
                && matches!(
                    classify_yaml_value_node(value),
                    BlockKind::Section | BlockKind::List
                )
            {
                return Some(value);
            }
            Some(semantic)
        }
        _ => None,
    }
}

fn push_non_empty_gap(
    blocks: &mut Vec<Block>,
    content: &str,
    start: usize,
    end: usize,
    lang: Language,
) {
    if end <= start {
        return;
    }

    let chunk = &content[start..end];
    if chunk.is_empty() || chunk.trim().is_empty() {
        return;
    }

    blocks.push(create_file_block(
        chunk,
        BlockKind::Gap,
        content,
        start,
        end,
        lang,
    ));
}

fn push_container_interstitial(
    blocks: &mut Vec<Block>,
    parent: &Block,
    start: usize,
    end: usize,
    is_noise: fn(&str) -> bool,
) {
    if end <= start {
        return;
    }

    let chunk = &parent.content[start..end];
    if chunk.is_empty() {
        return;
    }

    let kind = if chunk.trim().is_empty() || is_noise(chunk) {
        BlockKind::Gap
    } else {
        BlockKind::Preamble
    };
    blocks.push(create_sub_block(parent, start, end, kind));
}

fn is_yaml_structural_noise(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | '[' | ']' | '{' | '}' | ','))
}

fn parse_tree(source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .context("Failed to load YAML grammar")?;
    parser.parse(source, None).context("Failed to parse YAML")
}

fn create_file_block(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Block {
    let (start_line, end_line) = byte_range_to_lines(full_source, start_byte, end_byte);
    Block {
        hash: TreeHash::from_content(text),
        content: text.to_string(),
        kind,
        tags: Vec::new(),
        complexity: complexity::calculate(text, lang),
        start_line,
        end_line,
    }
}

fn create_sub_block(
    parent: &Block,
    start_offset: usize,
    end_offset: usize,
    kind: BlockKind,
) -> Block {
    let text = &parent.content[start_offset..end_offset];
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&ch| ch == '\n').count();
    let chunk_newlines = text.chars().filter(|&ch| ch == '\n').count();

    let start_line = parent.start_line + offset_newlines;
    let end_line = start_line + chunk_newlines + usize::from(!text.ends_with('\n'));

    Block {
        hash: TreeHash::from_content(text),
        content: text.to_string(),
        kind,
        tags: parent.tags.clone(),
        complexity: None,
        start_line,
        end_line,
    }
}

fn byte_range_to_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let start_line = source[..start].chars().filter(|&ch| ch == '\n').count();
    let chunk_newlines = source[start..end].chars().filter(|&ch| ch == '\n').count();
    let end_line = start_line + chunk_newlines + usize::from(!source[start..end].ends_with('\n'));
    (start_line, end_line)
}
