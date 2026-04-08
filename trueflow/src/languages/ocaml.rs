use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, default_code_sub_split, no_attribute_nodes, no_nested_blocks,
    no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::hashing::TreeHash;
use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

pub(crate) fn registration() -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks: no_nested_blocks,
            collect_test_ranges: no_test_ranges,
            custom_splitter: Some(split_top_level),
        },
        sub_split: sub_split_registration,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_ocaml::LANGUAGE_OCAML.into()
}

fn split_top_level(_root: Node<'_>, content: &str, lang: Language) -> Result<Vec<Block>> {
    let tree = parse_best_tree(content).context("Failed to parse OCaml source")?;
    let root = tree.root_node();

    let mut blocks = Vec::new();
    let mut cursor = root.walk();
    let mut last_end = 0;
    let mut pending_start = None;
    let mut pending_end = 0;

    for child in root.named_children(&mut cursor) {
        let start = child.start_byte();
        let end = child.end_byte();

        if is_ocaml_attribute_like(child.kind()) {
            if pending_start.is_none() {
                push_non_whitespace_gap(&mut blocks, content, last_end, start, lang);
                pending_start = Some(start);
            }
            pending_end = end;
            continue;
        }

        let block_start = pending_start.take().unwrap_or(start);
        if pending_end == 0 {
            push_non_whitespace_gap(&mut blocks, content, last_end, block_start, lang);
        }

        let kind = map_kind(child, content);
        blocks.push(create_file_block(
            &content[block_start..end],
            kind,
            content,
            block_start,
            end,
            lang,
        ));
        blocks.extend(collect_nested_file_blocks(child, content, lang));

        last_end = end;
        pending_end = 0;
    }

    if let Some(start) = pending_start {
        blocks.push(create_file_block(
            &content[start..pending_end],
            BlockKind::Decorator,
            content,
            start,
            pending_end,
            lang,
        ));
        last_end = pending_end;
    }

    push_non_whitespace_gap(&mut blocks, content, last_end, content.len(), lang);

    Ok(blocks)
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "comment" | "directive" | "line_number_directive" | "shebang" => BlockKind::Comment,
        "open_module" | "include_module" => BlockKind::Import,
        "module_definition" => BlockKind::Module,
        "module_type_definition" | "class_type_definition" => BlockKind::Interface,
        "class_definition" => BlockKind::Class,
        "type_definition" => classify_type_definition(node),
        "value_definition" => classify_value_definition(node),
        "value_specification" => classify_value_specification(node),
        "external" => BlockKind::FunctionSignature,
        "exception_definition" => BlockKind::Type,
        "floating_attribute" | "item_attribute" => BlockKind::Decorator,
        "item_extension" | "quoted_item_extension" => BlockKind::Macro,
        _ => {
            let _ = content;
            BlockKind::Code
        }
    }
}

fn classify_type_definition(node: Node<'_>) -> BlockKind {
    let bindings = type_bindings(node);
    if bindings.len() != 1 {
        return BlockKind::Type;
    }

    match bindings[0]
        .child_by_field_name("body")
        .map(|body| body.kind())
    {
        Some("record_declaration") => BlockKind::Struct,
        Some("variant_declaration") => BlockKind::Enum,
        _ => BlockKind::Type,
    }
}

fn classify_value_definition(node: Node<'_>) -> BlockKind {
    let Some(binding) = first_named_child_of_kind(node, "let_binding") else {
        return BlockKind::Code;
    };

    if has_named_child_of_kind(binding, "parameter") {
        return BlockKind::Function;
    }

    if binding
        .child_by_field_name("body")
        .is_some_and(|body| matches!(body.kind(), "fun_expression" | "function_expression"))
    {
        return BlockKind::Function;
    }

    BlockKind::Const
}

fn classify_value_specification(node: Node<'_>) -> BlockKind {
    let Some(type_node) = node.child_by_field_name("type") else {
        return BlockKind::Const;
    };

    if node_contains_kind(type_node, "function_type") {
        BlockKind::FunctionSignature
    } else {
        BlockKind::Const
    }
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like,
        },
        BlockKind::Module | BlockKind::Interface => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_module_like,
        },
        BlockKind::Struct | BlockKind::Enum | BlockKind::Type => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_type_like,
        },
        _ => default_code_sub_split(kind),
    }
}

fn split_function_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_best_tree(&block.content).context("Failed to parse OCaml function block")?;
    let root = tree.root_node();
    let Some(value_definition) = find_named_descendant_any(root, &["value_definition"]) else {
        return crate::sub_splitter::split_code_review_units(block);
    };
    let Some(binding) = first_named_child_of_kind(value_definition, "let_binding") else {
        return crate::sub_splitter::split_code_review_units(block);
    };
    if classify_value_definition(value_definition) != BlockKind::Function {
        return crate::sub_splitter::split_code_review_units(block);
    }

    let signature_end = signature_end_for_value_binding(&block.content, binding);
    if signature_end == 0 || signature_end >= block.content.len() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    split_review_units_after_signature(block, signature_end)
}

fn split_module_like(block: &Block) -> Result<Vec<Block>> {
    let tree =
        parse_best_tree(&block.content).context("Failed to parse OCaml module-like block")?;
    let root = tree.root_node();
    let Some(module_node) = find_named_descendant_any(
        root,
        &[
            "module_definition",
            "module_type_definition",
            "class_type_definition",
        ],
    ) else {
        return crate::sub_splitter::split_code_review_units(block);
    };
    let Some(container) = stable_module_container(module_node) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let mut children = collect_immediate_nested_blocks(container, &block.content);
    if children.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }
    children.sort_by_key(|child| child.start_byte);

    Ok(build_child_preserving_review_units(block, &children))
}

fn split_type_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_best_tree(&block.content).context("Failed to parse OCaml type-like block")?;
    let root = tree.root_node();
    let Some(type_definition) = find_named_descendant_any(root, &["type_definition"]) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let bindings = type_bindings(type_definition);
    if bindings.len() != 1 {
        return crate::sub_splitter::split_code_review_units(block);
    }
    let binding = bindings[0];

    let children = if let Some(record) = binding.child_by_field_name("body") {
        match record.kind() {
            "record_declaration" => collect_record_fields(record),
            "variant_declaration" => collect_variant_constructors(record),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if children.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    Ok(build_child_preserving_review_units(block, &children))
}

fn collect_nested_file_blocks(node: Node<'_>, content: &str, lang: Language) -> Vec<Block> {
    let mut nested = Vec::new();
    collect_nested_file_blocks_into(node, content, lang, &mut nested);
    nested
}

fn collect_nested_file_blocks_into(
    node: Node<'_>,
    content: &str,
    lang: Language,
    blocks: &mut Vec<Block>,
) {
    let Some(container) = stable_module_container(node) else {
        return;
    };

    let children = collect_immediate_nested_blocks(container, content);
    for child in children {
        blocks.push(create_file_block(
            &content[child.start_byte..child.end_byte],
            child.kind,
            content,
            child.start_byte,
            child.end_byte,
            lang,
        ));

        if matches!(child.kind, BlockKind::Module | BlockKind::Interface) {
            let child_node = find_named_node_by_span(node, child.start_byte, child.end_byte)
                .or_else(|| find_named_node_by_span(container, child.start_byte, child.end_byte));
            if let Some(child_node) = child_node {
                collect_nested_file_blocks_into(child_node, content, lang, blocks);
            }
        }
    }
}

fn collect_immediate_nested_blocks(container: Node<'_>, content: &str) -> Vec<NestedBlock> {
    let mut blocks = Vec::new();
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        let kind = map_kind(child, content);
        if matches!(
            kind,
            BlockKind::Code | BlockKind::Comment | BlockKind::Decorator
        ) {
            continue;
        }
        blocks.push(NestedBlock {
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
            kind,
        });
    }
    blocks
}

fn collect_record_fields(record: Node<'_>) -> Vec<NestedBlock> {
    let mut fields = Vec::new();
    let mut cursor = record.walk();
    for child in record.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        fields.push(NestedBlock {
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
            kind: BlockKind::Variable,
        });
    }
    fields
}

fn collect_variant_constructors(variant: Node<'_>) -> Vec<NestedBlock> {
    let mut constructors = Vec::new();
    let mut cursor = variant.walk();
    for child in variant.named_children(&mut cursor) {
        if child.kind() != "constructor_declaration" {
            continue;
        }
        constructors.push(NestedBlock {
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
            kind: BlockKind::Enum,
        });
    }
    constructors
}

fn build_child_preserving_review_units(block: &Block, children: &[NestedBlock]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current = 0;

    for child in children {
        if current < child.start_byte {
            push_non_child_chunk(block, current, child.start_byte, &mut blocks);
        }

        blocks.push(create_sub_block(
            block,
            child.start_byte,
            child.end_byte,
            child.kind,
        ));
        current = child.end_byte;
    }

    if current < block.content.len() {
        push_non_child_chunk(block, current, block.content.len(), &mut blocks);
    }

    blocks
}

fn split_review_units_after_signature(block: &Block, signature_end: usize) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut blocks = vec![create_sub_block(
        block,
        0,
        signature_end,
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

        blocks.push(create_sub_block(
            block,
            signature_end + gap.start(),
            signature_end + gap.end(),
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
    if let Some(prefix_len) = leading_ocaml_comment_prefix_len(chunk)
        && prefix_len < chunk.len()
    {
        blocks.push(create_sub_block(
            parent,
            start_offset,
            start_offset + prefix_len,
            BlockKind::Comment,
        ));

        let remainder = &chunk[prefix_len..];
        let remainder_kind = classify_ocaml_chunk(remainder);
        if !matches!(remainder_kind, BlockKind::Gap) {
            blocks.push(create_sub_block(
                parent,
                start_offset + prefix_len,
                start_offset + chunk.len(),
                remainder_kind,
            ));
        }
        return;
    }

    let kind = classify_ocaml_chunk(chunk);
    if !matches!(kind, BlockKind::Gap) {
        blocks.push(create_sub_block(
            parent,
            start_offset,
            start_offset + chunk.len(),
            kind,
        ));
    }
}

fn push_non_child_chunk(parent: &Block, start: usize, end: usize, blocks: &mut Vec<Block>) {
    if start >= end {
        return;
    }

    let chunk = &parent.content[start..end];
    let kind = classify_ocaml_chunk(chunk);
    blocks.push(create_sub_block(parent, start, end, kind));
}

fn push_non_whitespace_gap(
    blocks: &mut Vec<Block>,
    content: &str,
    start: usize,
    end: usize,
    lang: Language,
) {
    if start >= end {
        return;
    }

    let chunk = &content[start..end];
    if chunk.trim().is_empty() {
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

fn signature_end_for_value_binding(content: &str, binding: Node<'_>) -> usize {
    let Some(equals_end) = first_child_token_end(binding, "=") else {
        return 0;
    };

    let line_end = line_end_after(content, equals_end);
    let same_line_tail = content[equals_end..line_end].trim_start();
    if same_line_tail.starts_with("function") || same_line_tail.starts_with("fun") {
        return line_end;
    }

    let first_post_equals_start =
        first_post_equals_named_start(binding, equals_end).unwrap_or(equals_end);
    if content[equals_end..first_post_equals_start].contains('\n') || same_line_tail.is_empty() {
        return line_end;
    }

    0
}

fn line_end_after(content: &str, index: usize) -> usize {
    if index >= content.len() {
        return content.len();
    }

    let tail = &content[index..];
    if let Some(offset) = tail.find('\n') {
        index + offset + 1
    } else {
        content.len()
    }
}

fn first_post_equals_named_start(binding: Node<'_>, equals_end: usize) -> Option<usize> {
    let mut cursor = binding.walk();
    binding
        .named_children(&mut cursor)
        .filter(|child| child.start_byte() >= equals_end)
        .map(|child| child.start_byte())
        .min()
}

fn first_child_token_end(node: Node<'_>, token: &str) -> Option<usize> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == token)
        .map(|child| child.end_byte())
}

fn type_bindings(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "type_binding")
        .collect()
}

fn stable_module_container(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "module_definition" => first_named_child_of_kind(node, "module_binding")
            .and_then(module_binding_container)
            .or_else(|| find_named_descendant_any(node, &["structure", "signature"])),
        "module_type_definition" | "class_type_definition" => node
            .child_by_field_name("body")
            .and_then(module_type_container)
            .or_else(|| find_named_descendant_any(node, &["signature"])),
        "module_binding" => module_binding_container(node),
        "structure" | "signature" => Some(node),
        _ => None,
    }
}

fn module_binding_container(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
        .and_then(module_type_container)
        .or_else(|| {
            node.child_by_field_name("module_type")
                .and_then(module_type_container)
        })
        .or_else(|| find_named_descendant_any(node, &["structure", "signature"]))
}

fn module_type_container(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "structure" | "signature" => Some(node),
        _ => find_named_descendant_any(node, &["structure", "signature"]),
    }
}

fn first_named_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn has_named_child_of_kind(node: Node<'_>, kind: &str) -> bool {
    first_named_child_of_kind(node, kind).is_some()
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

fn find_named_node_by_span(node: Node<'_>, start: usize, end: usize) -> Option<Node<'_>> {
    if node.is_named() && node.start_byte() == start && node.end_byte() == end {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_named_node_by_span(child, start, end) {
            return Some(found);
        }
    }

    None
}

fn node_contains_kind(node: Node<'_>, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| node_contains_kind(child, kind))
}

fn is_ocaml_attribute_like(kind: &str) -> bool {
    matches!(
        kind,
        "floating_attribute" | "item_attribute" | "item_extension" | "quoted_item_extension"
    )
}

fn classify_ocaml_chunk(chunk: &str) -> BlockKind {
    if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if chunk_is_ocaml_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn leading_ocaml_comment_prefix_len(chunk: &str) -> Option<usize> {
    let mut index = 0;
    let mut saw_comment = false;

    while index < chunk.len() {
        let whitespace = chunk[index..]
            .char_indices()
            .take_while(|(_, ch)| ch.is_whitespace())
            .map(|(offset, ch)| offset + ch.len_utf8())
            .last()
            .unwrap_or(0);
        index += whitespace;

        if index >= chunk.len() {
            return saw_comment.then_some(index);
        }

        let Some(comment_end) = consume_ocaml_comment(chunk, index) else {
            return saw_comment.then_some(index);
        };
        saw_comment = true;
        index = comment_end;
    }

    saw_comment.then_some(index)
}

fn chunk_is_ocaml_comment_only(chunk: &str) -> bool {
    leading_ocaml_comment_prefix_len(chunk)
        .is_some_and(|prefix_len| chunk[prefix_len..].trim().is_empty())
}

fn consume_ocaml_comment(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if start + 1 >= bytes.len() || &bytes[start..start + 2] != b"(*" {
        return None;
    }

    let mut index = start;
    let mut depth = 0usize;
    while index + 1 < bytes.len() {
        match &bytes[index..index + 2] {
            b"(*" => {
                depth += 1;
                index += 2;
            }
            b"*)" => {
                depth = depth.saturating_sub(1);
                index += 2;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => index += 1,
        }
    }

    None
}

fn parse_best_tree(source: &str) -> Result<Tree> {
    let implementation = parse_with_language(source, &tree_sitter_ocaml::LANGUAGE_OCAML.into())
        .context("Failed to parse with OCaml implementation grammar")?;
    let interface = parse_with_language(source, &tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into())
        .context("Failed to parse with OCaml interface grammar")?;

    let implementation_score = syntax_error_score(implementation.root_node());
    let interface_score = syntax_error_score(interface.root_node());

    if interface_score < implementation_score {
        Ok(interface)
    } else {
        Ok(implementation)
    }
}

fn parse_with_language(source: &str, language: &TsLanguage) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language)?;
    parser
        .parse(source, None)
        .context("tree-sitter parse failed")
}

fn syntax_error_score(node: Node<'_>) -> usize {
    let mut score = usize::from(node.is_error()) + usize::from(node.is_missing());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        score += syntax_error_score(child);
    }
    score
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
        complexity: crate::complexity::calculate(text, lang),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_best_tree_prefers_interface_grammar_for_value_specifications() {
        let source = "val render : int -> string\n";
        let tree = parse_best_tree(source).unwrap();
        assert!(!tree.root_node().has_error());
        assert!(
            find_named_descendant_any(tree.root_node(), &["value_specification"]).is_some(),
            "expected interface grammar to produce a value_specification node"
        );
    }

    #[test]
    fn ocaml_comment_helpers_recognize_nested_comment_only_chunks() {
        let chunk = "  (* outer (* inner *) comment *)\n\n";
        assert!(chunk_is_ocaml_comment_only(chunk));
        assert_eq!(leading_ocaml_comment_prefix_len(chunk), Some(chunk.len()));
    }

    #[test]
    fn classify_value_definition_distinguishes_functions_and_constants() {
        let tree = parse_best_tree("let value = 1\nlet run x = x + 1\n").unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let nodes = root
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "value_definition")
            .collect::<Vec<_>>();

        assert_eq!(classify_value_definition(nodes[0]), BlockKind::Const);
        assert_eq!(classify_value_definition(nodes[1]), BlockKind::Function);
    }
}
