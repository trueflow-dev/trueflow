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

const FUNCTION_LIKE_DECLARATIONS: &[&str] = &["function_declaration", "method_declaration"];

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
    tree_sitter_go::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    classify_node(node, content, false).unwrap_or(BlockKind::Code)
}

fn classify_node(node: Node<'_>, content: &str, in_container: bool) -> Option<BlockKind> {
    match node.kind() {
        "package_clause" => Some(BlockKind::Module),
        "import_declaration" => Some(BlockKind::Import),
        "const_declaration" | "const_spec" => Some(BlockKind::Const),
        "var_declaration" | "var_spec" | "short_var_declaration" => Some(BlockKind::Variable),
        "type_declaration" | "type_spec" | "type_alias" => Some(classify_type_declaration(node)),
        "function_declaration" => Some(BlockKind::Function),
        "method_declaration" | "method_elem" => Some(BlockKind::Method),
        "field_declaration" => Some(classify_field_declaration(node, content, in_container)),
        "type_elem" => Some(classify_type_element(node)),
        "comment" => Some(BlockKind::Comment),
        _ => None,
    }
}

fn classify_type_declaration(node: Node<'_>) -> BlockKind {
    let Some(type_node) = type_like_node(node) else {
        return BlockKind::Type;
    };

    match type_node.kind() {
        "struct_type" => BlockKind::Struct,
        "interface_type" => BlockKind::Interface,
        _ => BlockKind::Type,
    }
}

fn classify_field_declaration(node: Node<'_>, content: &str, _in_container: bool) -> BlockKind {
    let text = node.utf8_text(content.as_bytes()).unwrap_or_default();
    if text.contains("func(") {
        BlockKind::Method
    } else {
        BlockKind::Variable
    }
}

fn classify_type_element(node: Node<'_>) -> BlockKind {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(kind) = classify_node(child, "", true)
            && !matches!(kind, BlockKind::Code)
        {
            return kind;
        }
    }

    BlockKind::Type
}

fn type_like_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    match node.kind() {
        "struct_type" | "interface_type" => Some(node),
        "type_declaration" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).find_map(type_like_node)
        }
        "type_spec" | "type_alias" => node.child_by_field_name("type"),
        _ => None,
    }
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    let Some(type_node) = type_like_node(node) else {
        return Vec::new();
    };

    collect_container_member_spans(type_node, content)
}

fn collect_container_member_spans(type_node: Node<'_>, content: &str) -> Vec<NestedBlock> {
    match type_node.kind() {
        "struct_type" => {
            let Some(body) = first_named_child(type_node) else {
                return Vec::new();
            };
            let mut cursor = body.walk();
            body.named_children(&mut cursor)
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
        "interface_type" => {
            let mut cursor = type_node.walk();
            type_node
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
        _ => Vec::new(),
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
    if matches!(node.kind(), "function_declaration" | "method_declaration")
        && let Some(name) = function_name(node, source)
        && is_go_test_name(&name)
    {
        ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_test_ranges_from_node(child, source, ranges)?;
    }

    Ok(())
}

fn function_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn is_go_test_name(name: &str) -> bool {
    ["Test", "Benchmark", "Example", "Fuzz"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn sub_split(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like,
        },
        BlockKind::Struct | BlockKind::Interface => SubSplitRegistration {
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
    let tree = parse_tree(&block.content).context("Failed to parse Go function-like block")?;
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

fn split_type_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Go type-like block")?;
    let root = tree.root_node();
    let Some(type_node) = find_type_like_descendant(root) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let items = collect_container_member_blocks(block, type_node);
    if items.is_empty() {
        crate::sub_splitter::split_code_review_units(block)
    } else {
        Ok(items)
    }
}

fn parse_tree(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .context("Failed to load Go grammar")?;
    parser.parse(source, None).context("Failed to parse Go")
}

fn find_type_like_descendant<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if let Some(type_node) = type_like_node(node) {
        return Some(type_node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_type_like_descendant(child) {
            return Some(found);
        }
    }

    None
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

fn first_named_child<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn collect_container_member_blocks(parent: &Block, type_node: Node<'_>) -> Vec<Block> {
    collect_container_member_spans(type_node, &parent.content)
        .into_iter()
        .map(|span| {
            create_sub_block_with_kind(
                parent,
                &parent.content[span.start_byte..span.end_byte],
                span.start_byte,
                span.kind,
            )
        })
        .collect()
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
