use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, no_attribute_nodes,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan, ParentBlock};
use crate::code_comments::{self, CommentSyntax};

use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Tree};

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
    if node.kind() == "type_declaration" {
        return collect_type_declaration_blocks(node, content);
    }

    let Some(type_node) = type_like_node(node) else {
        return Vec::new();
    };

    collect_container_member_spans(type_node, content)
}

fn type_declaration_specs(declaration: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = declaration.walk();
    declaration
        .named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "type_spec" | "type_alias"))
        .collect()
}

/// A grouped `type ( A ...; B ... )` declaration exposes every spec as its
/// own block (with its members); a single spec exposes only its members.
fn collect_type_declaration_blocks(declaration: Node<'_>, content: &str) -> Vec<NestedBlock> {
    match type_declaration_specs(declaration).as_slice() {
        [] => Vec::new(),
        [spec] => type_like_node(*spec)
            .map(|type_node| collect_container_member_spans(type_node, content))
            .unwrap_or_default(),
        specs => specs
            .iter()
            .flat_map(|spec| {
                let spec_block = NestedBlock {
                    start_byte: spec.start_byte(),
                    end_byte: spec.end_byte(),
                    kind: classify_type_declaration(*spec),
                    complexity: crate::complexity::calculate_from_node(
                        *spec,
                        Language::Go,
                        content,
                    ),
                };
                let members = type_like_node(*spec)
                    .map(|type_node| collect_container_member_spans(type_node, content))
                    .unwrap_or_default();
                std::iter::once(spec_block).chain(members)
            })
            .collect(),
    }
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
                    (!matches!(kind, BlockKind::Code)).then(|| NestedBlock {
                        start_byte: child.start_byte(),
                        end_byte: child.end_byte(),
                        kind,
                        complexity: crate::complexity::calculate_from_node(
                            child,
                            Language::Go,
                            content,
                        ),
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
                    (!matches!(kind, BlockKind::Code)).then(|| NestedBlock {
                        start_byte: child.start_byte(),
                        end_byte: child.end_byte(),
                        kind,
                        complexity: crate::complexity::calculate_from_node(
                            child,
                            Language::Go,
                            content,
                        ),
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn collect_test_ranges(tree: &Tree, source: &str) -> Result<Vec<ByteSpan>> {
    let mut ranges = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "function_declaration" | "method_declaration")
            && let Some(name) = function_name(node, source)
            && is_go_test_name(&name)
        {
            ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
        }

        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    Ok(ranges)
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

fn split_function_like(block: &ParentBlock<'_>) -> Result<Vec<Block>> {
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

fn split_type_like(block: &ParentBlock<'_>) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Go type-like block")?;
    let root = tree.root_node();
    if let Some(declaration) = find_named_descendant_any(root, &["type_declaration"])
        && let specs @ [_, _, ..] = type_declaration_specs(declaration).as_slice()
    {
        return specs
            .iter()
            .map(|spec| {
                create_sub_block_with_kind(
                    block,
                    &block.content[spec.start_byte()..spec.end_byte()],
                    spec.start_byte(),
                    classify_type_declaration(*spec),
                )
            })
            .collect();
    }

    let Some(type_node) = find_type_like_descendant(root) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let items = collect_container_member_blocks(block, type_node)?;
    if items.is_empty() {
        crate::sub_splitter::split_code_review_units(block)
    } else {
        Ok(items)
    }
}

fn parse_tree(source: &str) -> Result<Tree> {
    crate::tree_sitter_support::parse_with_language(source, &tree_sitter_go::LANGUAGE.into())
        .context("Failed to parse Go")
}

fn find_type_like_descendant<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if let Some(type_node) = type_like_node(current) {
            return Some(type_node);
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

fn first_named_child<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn collect_container_member_blocks(
    parent: &ParentBlock<'_>,
    type_node: Node<'_>,
) -> Result<Vec<Block>> {
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

fn split_review_units_after_signature(
    block: &ParentBlock<'_>,
    signature_end: usize,
) -> Result<Vec<Block>> {
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
    parent: &ParentBlock<'_>,
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
    code_comments::leading_c_style_comment_prefix_len(chunk)
}

fn classify_code_chunk(chunk: &str) -> BlockKind {
    if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if code_comments::chunk_is_comment_only(chunk, CommentSyntax::of(Language::Go)) {
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
    parent: &ParentBlock<'_>,
    content: &str,
    start_offset: usize,
    kind: BlockKind,
) -> Result<Block> {
    let end_offset = start_offset
        .checked_add(content.len())
        .context("Go sub-split end offset overflow")?;
    let mut block = parent
        .child(kind, ByteSpan::new(start_offset, end_offset))
        .context("Go sub-split range must be a valid parent UTF-8 slice")?;
    assert_eq!(
        block.content, content,
        "Go sub-split range must name content"
    );
    block.tags = parent.tags.clone();
    Ok(block)
}
