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
    tree_sitter_html::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "doctype" => BlockKind::Preamble,
        "text" | "raw_text" | "entity" => {
            if node_text(node, content)
                .map(|text| text.trim().is_empty())
                .unwrap_or(false)
            {
                BlockKind::Gap
            } else {
                BlockKind::Content
            }
        }
        "style_element" | "script_element" | "self_closing_tag" => BlockKind::Element,
        "element" => classify_element(node, content),
        _ => BlockKind::Element,
    }
}

fn classify_element(node: Node<'_>, content: &str) -> BlockKind {
    let Some(tag_name) = element_tag_name(node, content) else {
        return BlockKind::Element;
    };

    if is_structural_tag(tag_name) || should_promote_generic_container(node, content, tag_name) {
        BlockKind::Section
    } else {
        BlockKind::Element
    }
}

fn is_structural_tag(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "html"
            | "head"
            | "body"
            | "main"
            | "section"
            | "article"
            | "nav"
            | "header"
            | "footer"
            | "aside"
            | "form"
            | "table"
            | "ul"
            | "ol"
            | "template"
            | "dialog"
            | "details"
            | "figure"
            | "fieldset"
    )
}

fn should_promote_generic_container(node: Node<'_>, content: &str, tag_name: &str) -> bool {
    if !matches!(tag_name, "div") && !tag_name.contains('-') {
        return false;
    }

    let child_elements = immediate_element_children(node)
        .into_iter()
        .filter(|child| !is_inline_like_child(*child, content))
        .count();
    let structural_children = immediate_element_children(node)
        .into_iter()
        .filter(|child| matches!(classify_element(*child, content), BlockKind::Section))
        .count();
    let line_span = node_line_span(node, content);

    structural_children > 0 || (child_elements >= 2 && line_span >= 8)
}

fn is_inline_like_child(node: Node<'_>, content: &str) -> bool {
    let Some(tag_name) = element_tag_name(node, content) else {
        return false;
    };

    matches!(
        tag_name,
        "a" | "abbr"
            | "b"
            | "button"
            | "em"
            | "i"
            | "img"
            | "input"
            | "label"
            | "option"
            | "select"
            | "small"
            | "span"
            | "strong"
            | "textarea"
    )
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    let mut blocks = Vec::new();
    collect_semantic_descendants(node, content, &mut blocks);
    blocks
}

fn collect_semantic_descendants(node: Node<'_>, content: &str, blocks: &mut Vec<NestedBlock>) {
    let mut pending = immediate_content_children(node);
    pending.reverse();

    while let Some(child) = pending.pop() {
        if !matches!(child.kind(), "element" | "style_element" | "script_element") {
            continue;
        }

        let kind = map_kind(child, content);
        if should_collect_nested_child(child, content, kind) {
            blocks.push(NestedBlock {
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                kind,
            });
        }

        if kind == BlockKind::Section || child.kind() == "element" {
            let mut children = immediate_content_children(child);
            children.reverse();
            pending.extend(children);
        }
    }
}

fn should_collect_nested_child(node: Node<'_>, content: &str, kind: BlockKind) -> bool {
    match kind {
        BlockKind::Section => true,
        BlockKind::Element => element_tag_name(node, content)
            .map(|tag_name| matches!(tag_name, "style" | "script"))
            .unwrap_or(false),
        _ => false,
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
    let tree = parse_html(&block.content).context("Failed to parse HTML review block")?;
    let root = tree.root_node();
    let Some(review_root) = find_primary_review_root(root) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let spans = collect_review_spans(review_root, &block.content);
    if spans.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    build_review_blocks(block, &spans)
}

fn parse_html(content: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_html::LANGUAGE.into())
        .context("Failed to load HTML grammar")?;
    parser.parse(content, None).context("Failed to parse HTML")
}

fn find_primary_review_root(root: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "element" | "style_element" | "script_element"))
}

fn collect_review_spans(node: Node<'_>, content: &str) -> Vec<SemanticSpan> {
    let semantic_spans = immediate_content_children(node)
        .into_iter()
        .filter_map(|child| semantic_child_span(child, content))
        .collect::<Vec<_>>();
    if !semantic_spans.is_empty() {
        return semantic_spans;
    }

    immediate_content_children(node)
        .into_iter()
        .filter_map(|child| leaf_child_span(child, content))
        .collect()
}

fn semantic_child_span(node: Node<'_>, content: &str) -> Option<SemanticSpan> {
    if !matches!(node.kind(), "element" | "style_element" | "script_element") {
        return None;
    }

    let kind = map_kind(node, content);
    if !matches!(kind, BlockKind::Section | BlockKind::Element) {
        return None;
    }

    if kind == BlockKind::Section || should_collect_nested_child(node, content, kind) {
        Some(SemanticSpan {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind,
        })
    } else {
        None
    }
}

fn leaf_child_span(node: Node<'_>, content: &str) -> Option<SemanticSpan> {
    match node.kind() {
        "text" | "raw_text" | "entity" => node_text(node, content)
            .filter(|text| !text.trim().is_empty())
            .map(|_| SemanticSpan {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                kind: BlockKind::Content,
            }),
        "element" | "style_element" | "script_element" => Some(SemanticSpan {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind: map_kind(node, content),
        }),
        _ => None,
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
    if fragment.trim().is_empty() {
        BlockKind::Gap
    } else {
        BlockKind::Content
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
        .context("HTML sub-split end offset overflow")?;
    let mut block = Block::from_parent_range(parent, kind, ByteSpan::new(start_offset, end_offset))
        .context("HTML sub-split range must be a valid parent UTF-8 slice")?;
    assert_eq!(
        block.content, content,
        "HTML sub-split range must name content"
    );
    block.tags = parent.tags.clone();
    Ok(block)
}

fn immediate_content_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "start_tag" | "end_tag"))
        .collect()
}

fn immediate_element_children(node: Node<'_>) -> Vec<Node<'_>> {
    immediate_content_children(node)
        .into_iter()
        .filter(|child| matches!(child.kind(), "element" | "style_element" | "script_element"))
        .collect()
}

fn element_tag_name<'a>(node: Node<'a>, content: &'a str) -> Option<&'a str> {
    match node.kind() {
        "element" | "style_element" | "script_element" => {
            let mut cursor = node.walk();
            let start_tag = node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "start_tag")?;
            tag_name_from_tag_node(start_tag, content)
        }
        "self_closing_tag" | "start_tag" | "end_tag" => tag_name_from_tag_node(node, content),
        _ => None,
    }
}

fn tag_name_from_tag_node<'a>(node: Node<'a>, content: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "tag_name" | "erroneous_end_tag_name"))?
        .utf8_text(content.as_bytes())
        .ok()
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> Option<&'a str> {
    node.utf8_text(content.as_bytes()).ok()
}

fn node_line_span(node: Node<'_>, content: &str) -> usize {
    content[node.start_byte()..node.end_byte()]
        .chars()
        .filter(|ch| *ch == '\n')
        .count()
        + 1
}
