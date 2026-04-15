use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::code_comments;
use crate::hashing::TreeHash;
use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

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
    tree_sitter_elixir::LANGUAGE.into()
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Macro => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like_block,
        },
        BlockKind::Module | BlockKind::Interface | BlockKind::Impl => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_module_like_block,
        },
        _ => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: crate::sub_splitter::split_code_review_units,
        },
    }
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "comment" => BlockKind::Comment,
        "call" => map_call_kind(node, content),
        _ => BlockKind::Code,
    }
}

fn map_call_kind(node: Node<'_>, content: &str) -> BlockKind {
    match call_target_name(node, content).as_deref() {
        Some("alias" | "import" | "require" | "use") => BlockKind::Import,
        Some("defmodule" | "describe") => BlockKind::Module,
        Some("defprotocol") => BlockKind::Interface,
        Some("defimpl") => BlockKind::Impl,
        Some("defmacro" | "defmacrop" | "defguard" | "defguardp") => BlockKind::Macro,
        Some("def" | "defp" | "defdelegate" | "test") => BlockKind::Function,
        _ => BlockKind::Code,
    }
}

fn is_attribute_node(kind: &str) -> bool {
    kind == "comment"
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    if !matches!(
        map_kind(node, content),
        BlockKind::Module | BlockKind::Interface | BlockKind::Impl
    ) {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    collect_nested_blocks_in_container(node, content, &mut blocks);
    blocks
}

fn collect_nested_blocks_in_container(
    container: Node<'_>,
    content: &str,
    blocks: &mut Vec<NestedBlock>,
) {
    collect_container_blocks(container, content, true, blocks);
}

fn collect_immediate_nested_blocks_in_container(
    container: Node<'_>,
    content: &str,
) -> Vec<NestedBlock> {
    let mut blocks = Vec::new();
    collect_container_blocks(container, content, false, &mut blocks);
    blocks
}

fn collect_container_blocks(
    container: Node<'_>,
    content: &str,
    recurse: bool,
    blocks: &mut Vec<NestedBlock>,
) {
    let Some(do_block) = first_named_child_of_kind(container, "do_block") else {
        return;
    };

    let mut cursor = do_block.walk();
    let mut pending_start: Option<usize> = None;

    for child in do_block.children(&mut cursor) {
        let ts_kind = child.kind();
        if !child.is_named() && ts_kind != "comment" {
            continue;
        }

        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        if is_attribute_node(ts_kind) || is_elixir_attribute_node(child, content) {
            if pending_start.is_none() {
                pending_start = Some(start_byte);
            }
            continue;
        }

        let kind = map_kind(child, content);
        if matches!(kind, BlockKind::Code | BlockKind::Comment) {
            pending_start = None;
            continue;
        }

        blocks.push(NestedBlock {
            start_byte: pending_start.unwrap_or(start_byte),
            end_byte,
            kind,
        });
        pending_start = None;

        if recurse
            && matches!(
                kind,
                BlockKind::Module | BlockKind::Interface | BlockKind::Impl
            )
        {
            collect_container_blocks(child, content, true, blocks);
        }
    }
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
    if node.kind() == "call"
        && let Some(target) = call_target_name(node, source)
        && target == "test"
        && is_within_exunit_case_module(node, source)
    {
        ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_test_ranges_from_node(child, source, ranges)?;
    }

    Ok(())
}

fn split_function_like_block(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_elixir::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse Elixir function-like block")?;
    let root = tree.root_node();
    let Some(call_node) = find_call_with_targets(
        root,
        content,
        &[
            "def",
            "defp",
            "defdelegate",
            "defmacro",
            "defmacrop",
            "defguard",
            "defguardp",
            "test",
        ],
    ) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let Some(do_block) = first_named_child_of_kind(call_node, "do_block") else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let signature_end = signature_end_offset(content, do_block.start_byte());
    if signature_end == 0 || signature_end >= content.len() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    let mut blocks = vec![create_sub_block(
        block,
        0,
        signature_end,
        BlockKind::FunctionSignature,
    )];
    blocks.extend(split_review_tail(block, signature_end));
    Ok(blocks)
}

fn split_module_like_block(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_elixir::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse Elixir module-like block")?;
    let root = tree.root_node();
    let Some(container) = find_call_with_targets(
        root,
        content,
        &["defmodule", "defprotocol", "defimpl", "describe"],
    ) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let mut nested_blocks = collect_immediate_nested_blocks_in_container(container, content);
    if nested_blocks.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }
    nested_blocks.sort_by_key(|nested| nested.start_byte);

    let mut blocks = Vec::new();
    let mut current = 0;
    for nested in nested_blocks {
        if current < nested.start_byte {
            push_non_child_chunk(block, current, nested.start_byte, &mut blocks);
        }

        blocks.push(create_sub_block(
            block,
            nested.start_byte,
            nested.end_byte,
            nested.kind,
        ));
        current = nested.end_byte;
    }

    if current < container.end_byte() {
        push_non_child_chunk(block, current, container.end_byte(), &mut blocks);
    }

    if blocks.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    Ok(blocks)
}

fn split_review_tail(parent: &Block, start_offset: usize) -> Vec<Block> {
    let rest = &parent.content[start_offset..];
    let re = paragraph_break_regex();
    let mut blocks = Vec::new();

    let mut push_chunk = |chunk: &str, start: usize, end: usize, is_gap: bool| {
        let start = start_offset + start;
        let end = start_offset + end;

        if is_gap {
            blocks.push(create_sub_block(parent, start, end, BlockKind::Gap));
            return;
        }

        if let Some(comment_end) = leading_hash_comment_prefix_len(chunk) {
            let comment_end_abs = start + comment_end;
            blocks.push(create_sub_block(
                parent,
                start,
                comment_end_abs,
                BlockKind::Comment,
            ));

            if !chunk[comment_end..].trim().is_empty() {
                blocks.push(create_sub_block(
                    parent,
                    comment_end_abs,
                    end,
                    BlockKind::CodeParagraph,
                ));
            }
            return;
        }

        let kind = if code_comments::chunk_is_comment_only(chunk) {
            BlockKind::Comment
        } else {
            BlockKind::CodeParagraph
        };
        blocks.push(create_sub_block(parent, start, end, kind));
    };

    let mut start = 0;
    for mat in re.find_iter(rest) {
        if start < mat.start() {
            let chunk = &rest[start..mat.start()];
            if !chunk.is_empty() {
                push_chunk(chunk, start, mat.start(), false);
            }
        }

        let gap = &rest[mat.start()..mat.end()];
        push_chunk(gap, mat.start(), mat.end(), true);
        start = mat.end();
    }

    if start < rest.len() {
        let chunk = &rest[start..];
        if !chunk.is_empty() {
            push_chunk(chunk, start, rest.len(), false);
        }
    }

    blocks
}

fn leading_hash_comment_prefix_len(chunk: &str) -> Option<usize> {
    let mut offset = 0;
    let mut saw_comment = false;

    for line in chunk.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            offset += line.len();
            continue;
        }
        if trimmed.starts_with('#') {
            saw_comment = true;
            offset += line.len();
            continue;
        }

        return saw_comment.then_some(offset);
    }

    saw_comment.then_some(offset)
}

fn push_non_child_chunk(parent: &Block, start: usize, end: usize, blocks: &mut Vec<Block>) {
    if start >= end {
        return;
    }

    let chunk = &parent.content[start..end];
    let kind = if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if code_comments::chunk_is_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    };

    blocks.push(create_sub_block(parent, start, end, kind));
}

fn is_within_exunit_case_module(node: Node<'_>, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(candidate) = current {
        if candidate.kind() == "call"
            && let Some(target) = call_target_name(candidate, source)
            && target == "defmodule"
        {
            return module_uses_exunit_case(candidate, source);
        }
        current = candidate.parent();
    }

    false
}

fn module_uses_exunit_case(module_node: Node<'_>, source: &str) -> bool {
    let Some(do_block) = first_named_child_of_kind(module_node, "do_block") else {
        return false;
    };

    let mut cursor = do_block.walk();
    for child in do_block.named_children(&mut cursor) {
        if child.kind() != "call" {
            continue;
        }
        if call_target_name(child, source).as_deref() != Some("use") {
            continue;
        }
        if first_call_argument_text(child, source).as_deref() == Some("ExUnit.Case") {
            return true;
        }
    }

    false
}

fn first_call_argument_text(node: Node<'_>, source: &str) -> Option<String> {
    let arguments = first_named_child_of_kind(node, "arguments")?;
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .next()
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn is_elixir_attribute_node(node: Node<'_>, source: &str) -> bool {
    node.kind() == "unary_operator"
        && node
            .utf8_text(source.as_bytes())
            .map(|text| text.trim_start().starts_with('@'))
            .unwrap_or(false)
}

fn call_target_name(node: Node<'_>, source: &str) -> Option<String> {
    (node.kind() == "call")
        .then(|| node.child_by_field_name("target"))
        .flatten()
        .and_then(|target| target.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn find_call_with_targets<'a>(node: Node<'a>, source: &str, targets: &[&str]) -> Option<Node<'a>> {
    if node.kind() == "call"
        && let Some(target) = call_target_name(node, source)
        && targets.iter().any(|candidate| *candidate == target)
    {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_call_with_targets(child, source, targets) {
            return Some(found);
        }
    }

    None
}

fn first_named_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn signature_end_offset(content: &str, do_start: usize) -> usize {
    let bytes = content.as_bytes();
    if do_start >= bytes.len() {
        return bytes.len();
    }

    let mut end = do_start.saturating_add(2);
    if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
        end += 2;
    } else if bytes.get(end) == Some(&b'\n') {
        end += 1;
    }

    end.min(bytes.len())
}

fn create_sub_block(
    parent: &Block,
    start_offset: usize,
    end_offset: usize,
    kind: BlockKind,
) -> Block {
    let content = &parent.content[start_offset..end_offset];
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&c| c == '\n').count();
    let chunk_newlines = content.chars().filter(|&c| c == '\n').count();

    let start_line = parent.start_line + offset_newlines;
    let end_line = start_line + chunk_newlines + usize::from(!content.ends_with('\n'));

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
