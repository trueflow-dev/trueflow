use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, default_code_sub_split, no_attribute_nodes, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};

use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser};

#[derive(Debug, Clone, Copy)]
struct SemanticSpan {
    start_byte: usize,
    end_byte: usize,
    kind: BlockKind,
}

pub(crate) fn registration() -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks,
            collect_test_ranges: no_test_ranges,
            custom_splitter: None,
        },
        sub_split: sub_split_registration,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_css::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, _content: &str) -> BlockKind {
    match node.kind() {
        "comment" => BlockKind::Comment,
        "charset_statement" | "import_statement" | "namespace_statement" => BlockKind::Import,
        "rule_set"
        | "media_statement"
        | "supports_statement"
        | "scope_statement"
        | "keyframes_statement" => BlockKind::Section,
        "keyframe_block" | "declaration" => BlockKind::Element,
        "at_rule" => {
            if has_named_child_of_kind(node, "block") {
                BlockKind::Section
            } else {
                BlockKind::Element
            }
        }
        _ => BlockKind::Code,
    }
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    let mut blocks = Vec::new();
    collect_nested_blocks_into(node, content, &mut blocks);
    blocks
}

fn collect_nested_blocks_into(node: Node<'_>, content: &str, blocks: &mut Vec<NestedBlock>) {
    match node.kind() {
        "media_statement" | "supports_statement" | "scope_statement" => {
            if let Some(block_node) = first_named_child_of_kind(node, "block") {
                collect_block_children(block_node, content, blocks);
            }
        }
        "keyframes_statement" => {
            if let Some(list) = first_named_child_of_kind(node, "keyframe_block_list") {
                collect_keyframe_children(list, blocks);
            }
        }
        "at_rule" => {
            if let Some(block_node) = first_named_child_of_kind(node, "block") {
                collect_block_children(block_node, content, blocks);
            }
        }
        _ => {}
    }
}

fn collect_block_children(block_node: Node<'_>, content: &str, blocks: &mut Vec<NestedBlock>) {
    let mut cursor = block_node.walk();
    for child in block_node.named_children(&mut cursor) {
        let kind = map_kind(child, content);
        if matches!(
            kind,
            BlockKind::Section | BlockKind::Element | BlockKind::Comment
        ) && child.kind() != "declaration"
        {
            blocks.push(NestedBlock {
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                kind,
            });
        }

        if matches!(
            child.kind(),
            "media_statement"
                | "supports_statement"
                | "scope_statement"
                | "keyframes_statement"
                | "at_rule"
        ) {
            collect_nested_blocks_into(child, content, blocks);
        }
    }
}

fn collect_keyframe_children(list_node: Node<'_>, blocks: &mut Vec<NestedBlock>) {
    let mut cursor = list_node.walk();
    for child in list_node.named_children(&mut cursor) {
        if child.kind() != "keyframe_block" {
            continue;
        }

        blocks.push(NestedBlock {
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
            kind: BlockKind::Element,
        });
    }
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Section => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_section_review_units,
        },
        _ => default_code_sub_split(kind),
    }
}

fn split_section_review_units(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_css(&block.content).context("Failed to parse CSS review block")?;
    let root = tree.root_node();
    let Some(review_root) = find_primary_review_root(root) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    match review_root.kind() {
        "rule_set" => split_rule_like(block, review_root),
        "media_statement" | "supports_statement" | "scope_statement" => {
            split_container_like(block, review_root, "block")
        }
        "keyframes_statement" => split_container_like(block, review_root, "keyframe_block_list"),
        "at_rule" if has_named_child_of_kind(review_root, "block") => {
            split_container_like(block, review_root, "block")
        }
        _ => crate::sub_splitter::split_code_review_units(block),
    }
}

fn split_rule_like(block: &Block, review_root: Node<'_>) -> Result<Vec<Block>> {
    let Some(body) = first_named_child_of_kind(review_root, "block") else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let mut spans = vec![SemanticSpan {
        start_byte: 0,
        end_byte: body.start_byte().saturating_add(1),
        kind: BlockKind::FunctionSignature,
    }];
    spans.extend(collect_container_spans(body, &block.content));

    if spans.len() == 1 {
        return crate::sub_splitter::split_code_review_units(block);
    }

    build_review_blocks(block, &spans)
}

fn split_container_like(
    block: &Block,
    review_root: Node<'_>,
    body_kind: &str,
) -> Result<Vec<Block>> {
    let Some(body) = first_named_child_of_kind(review_root, body_kind) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let mut spans = vec![SemanticSpan {
        start_byte: 0,
        end_byte: body.start_byte().saturating_add(1),
        kind: BlockKind::FunctionSignature,
    }];
    spans.extend(collect_container_spans(body, &block.content));

    if spans.len() == 1 {
        return crate::sub_splitter::split_code_review_units(block);
    }

    build_review_blocks(block, &spans)
}

fn collect_container_spans(container: Node<'_>, content: &str) -> Vec<SemanticSpan> {
    let mut spans = Vec::new();
    let mut pending_group: Option<SemanticSpan> = None;

    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        match child.kind() {
            "comment" => {
                flush_pending_group(&mut spans, &mut pending_group);
                spans.push(SemanticSpan {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    kind: BlockKind::Comment,
                });
            }
            "declaration" => {
                let should_split_group = pending_group.as_ref().is_some_and(|group| {
                    gap_has_blank_line(&content[group.end_byte..child.start_byte()])
                });
                if should_split_group {
                    flush_pending_group(&mut spans, &mut pending_group);
                }

                match pending_group.as_mut() {
                    Some(group) => group.end_byte = child.end_byte(),
                    None => {
                        pending_group = Some(SemanticSpan {
                            start_byte: child.start_byte(),
                            end_byte: child.end_byte(),
                            kind: BlockKind::CodeParagraph,
                        });
                    }
                }
            }
            "rule_set"
            | "media_statement"
            | "supports_statement"
            | "scope_statement"
            | "keyframes_statement" => {
                flush_pending_group(&mut spans, &mut pending_group);
                spans.push(SemanticSpan {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    kind: BlockKind::Section,
                });
            }
            "keyframe_block" => {
                flush_pending_group(&mut spans, &mut pending_group);
                spans.push(SemanticSpan {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    kind: BlockKind::Element,
                });
            }
            "at_rule" => {
                flush_pending_group(&mut spans, &mut pending_group);
                spans.push(SemanticSpan {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    kind: if has_named_child_of_kind(child, "block") {
                        BlockKind::Section
                    } else {
                        BlockKind::Element
                    },
                });
            }
            _ => flush_pending_group(&mut spans, &mut pending_group),
        }
    }

    flush_pending_group(&mut spans, &mut pending_group);
    spans
}

fn flush_pending_group(spans: &mut Vec<SemanticSpan>, pending_group: &mut Option<SemanticSpan>) {
    if let Some(group) = pending_group.take() {
        spans.push(group);
    }
}

fn build_review_blocks(parent: &Block, spans: &[SemanticSpan]) -> Result<Vec<Block>> {
    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for span in spans {
        push_fragment_block(&mut blocks, parent, last_end, span.start_byte)?;
        blocks.push(create_sub_block(
            parent,
            &parent.content[span.start_byte..span.end_byte],
            span.start_byte,
            span.kind,
        )?);
        last_end = span.end_byte;
    }

    push_fragment_block(&mut blocks, parent, last_end, parent.content.len())?;
    Ok(blocks)
}

fn push_fragment_block(
    blocks: &mut Vec<Block>,
    parent: &Block,
    start: usize,
    end: usize,
) -> Result<()> {
    if start >= end {
        return Ok(());
    }

    let fragment = &parent.content[start..end];
    let kind = classify_fragment(fragment);
    blocks.push(create_sub_block(parent, fragment, start, kind)?);
    Ok(())
}

fn classify_fragment(fragment: &str) -> BlockKind {
    let trimmed = fragment.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .all(|ch| matches!(ch, '{' | '}' | ';' | ','))
    {
        BlockKind::Gap
    } else {
        BlockKind::CodeParagraph
    }
}

fn create_sub_block(
    parent: &Block,
    content: &str,
    start_offset: usize,
    kind: BlockKind,
) -> Result<Block> {
    let end_offset = start_offset
        .checked_add(content.len())
        .context("CSS sub-split end offset overflow")?;
    let mut block = Block::from_parent_range(parent, kind, ByteSpan::new(start_offset, end_offset))
        .context("CSS sub-split range must be a valid parent UTF-8 slice")?;
    assert_eq!(
        block.content, content,
        "CSS sub-split range must name content"
    );
    block.tags = parent.tags.clone();
    Ok(block)
}

fn parse_css(content: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_css::LANGUAGE.into())
        .context("Failed to load CSS grammar")?;
    parser.parse(content, None).context("Failed to parse CSS")
}

fn find_primary_review_root(root: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "rule_set"
                | "media_statement"
                | "supports_statement"
                | "scope_statement"
                | "keyframes_statement"
                | "at_rule"
        )
    })
}

fn first_named_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn has_named_child_of_kind(node: Node<'_>, kind: &str) -> bool {
    first_named_child_of_kind(node, kind).is_some()
}

fn gap_has_blank_line(gap: &str) -> bool {
    gap.contains("\n\n") || gap.contains("\r\n\r\n")
}
