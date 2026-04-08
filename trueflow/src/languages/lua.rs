use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, default_code_sub_split, no_attribute_nodes, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::hashing::TreeHash;
use crate::sub_splitter;
use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser};

pub(crate) fn registration() -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks,
            collect_test_ranges: no_test_ranges,
        },
        sub_split,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_lua::LANGUAGE.into()
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "comment" | "hash_bang_line" => BlockKind::Comment,
        "function_declaration" => node
            .child_by_field_name("name")
            .map_or(BlockKind::Function, kind_for_function_target),
        "variable_declaration" | "implicit_variable_declaration" | "assignment_statement" => {
            classify_assignment_like(node, content)
        }
        "function_call" => {
            if call_is_import_like(node, content) {
                BlockKind::Import
            } else {
                BlockKind::Code
            }
        }
        "return_statement" => classify_return_statement(node, content),
        _ => BlockKind::Code,
    }
}

fn classify_assignment_like(node: Node<'_>, content: &str) -> BlockKind {
    let Some((target, value)) = assignment_binding(node) else {
        return BlockKind::Variable;
    };

    if let Some(value) = value {
        if call_is_import_like(value, content) {
            return BlockKind::Import;
        }
        if value.kind() == "table_constructor" {
            return BlockKind::Module;
        }
        if value.kind() == "function_definition" {
            return classify_function_value(
                target,
                value,
                content,
                matches!(
                    target.kind(),
                    "dot_index_expression" | "method_index_expression"
                ),
            );
        }
    }

    if name_is_constant(target_name(target, content).as_deref()) {
        BlockKind::Const
    } else {
        BlockKind::Variable
    }
}

fn classify_return_statement(node: Node<'_>, content: &str) -> BlockKind {
    let Some(expression_list) = first_child_of_kind(node, "expression_list") else {
        return BlockKind::Export;
    };

    let mut cursor = expression_list.walk();
    let mut values = expression_list.named_children(&mut cursor);
    let Some(value) = values.next() else {
        return BlockKind::Export;
    };
    if values.next().is_some() {
        return BlockKind::Export;
    }

    if call_is_import_like(value, content) {
        BlockKind::Import
    } else {
        match value.kind() {
            "table_constructor" => BlockKind::Module,
            "function_definition" => BlockKind::Function,
            _ => BlockKind::Export,
        }
    }
}

fn kind_for_function_target(target: Node<'_>) -> BlockKind {
    if target.kind() == "method_index_expression" {
        BlockKind::Method
    } else {
        BlockKind::Function
    }
}

fn classify_function_value(
    target: Node<'_>,
    value: Node<'_>,
    content: &str,
    member_like: bool,
) -> BlockKind {
    if target.kind() == "method_index_expression"
        || (member_like && function_uses_self(value, content))
    {
        BlockKind::Method
    } else {
        BlockKind::Function
    }
}

fn function_uses_self(node: Node<'_>, content: &str) -> bool {
    node.child_by_field_name("parameters")
        .and_then(first_named_child)
        .is_some_and(|parameter| {
            parameter.kind() == "identifier"
                && parameter
                    .utf8_text(content.as_bytes())
                    .is_ok_and(|name| name == "self")
        })
}

fn assignment_binding(node: Node<'_>) -> Option<(Node<'_>, Option<Node<'_>>)> {
    match node.kind() {
        "assignment_statement" => {
            let variable_list = first_child_of_kind(node, "variable_list")?;
            let target = sole_named_child(variable_list)?;
            let expression_list = first_child_of_kind(node, "expression_list")?;
            let value = sole_named_child(expression_list)?;
            Some((target, Some(value)))
        }
        "variable_declaration" | "implicit_variable_declaration" => {
            if let Some(assignment) = first_child_of_kind(node, "assignment_statement") {
                assignment_binding(assignment)
            } else {
                let variable_list = first_child_of_kind(node, "variable_list")?;
                Some((sole_named_child(variable_list)?, None))
            }
        }
        "field" => {
            let target = node.child_by_field_name("name")?;
            let value = node.child_by_field_name("value");
            Some((target, value))
        }
        _ => None,
    }
}

fn first_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn first_named_child<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn sole_named_child<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

fn target_name(node: Node<'_>, content: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "string" => node.utf8_text(content.as_bytes()).ok().map(str::to_string),
        "dot_index_expression" => node
            .child_by_field_name("field")
            .and_then(|field| target_name(field, content)),
        "method_index_expression" => node
            .child_by_field_name("method")
            .and_then(|method| target_name(method, content)),
        "field" => node
            .child_by_field_name("name")
            .and_then(|name| target_name(name, content)),
        _ => None,
    }
}

fn name_is_constant(name: Option<&str>) -> bool {
    let Some(name) = name else {
        return false;
    };
    let normalized = name.trim_matches(|ch| matches!(ch, '"' | '\'' | '[' | ']'));
    normalized.chars().any(|ch| ch.is_ascii_alphabetic())
        && normalized
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn call_is_import_like(node: Node<'_>, content: &str) -> bool {
    node.kind() == "function_call"
        && matches!(
            call_name(node, content).as_deref(),
            Some("require") | Some("dofile") | Some("loadfile")
        )
}

fn call_name(node: Node<'_>, content: &str) -> Option<String> {
    let callee = node.child_by_field_name("name")?;
    target_name(callee, content)
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    if map_kind(node, content) != BlockKind::Module {
        return Vec::new();
    }

    let Some(table) = primary_table_constructor(node) else {
        return Vec::new();
    };

    collect_named_table_items(table, content)
        .into_iter()
        .map(|(field, kind)| NestedBlock {
            start_byte: field.start_byte(),
            end_byte: field.end_byte(),
            kind,
        })
        .collect()
}

fn primary_table_constructor(node: Node<'_>) -> Option<Node<'_>> {
    assignment_binding(node)
        .and_then(|(_, value)| value)
        .filter(|value| value.kind() == "table_constructor")
        .or_else(|| {
            if node.kind() == "return_statement" {
                first_child_of_kind(node, "expression_list")
                    .and_then(first_named_child)
                    .filter(|value| value.kind() == "table_constructor")
            } else {
                None
            }
        })
}

fn collect_named_table_items<'tree>(
    table: Node<'tree>,
    content: &str,
) -> Vec<(Node<'tree>, BlockKind)> {
    let mut cursor = table.walk();
    table
        .named_children(&mut cursor)
        .filter_map(|child| {
            if child.kind() != "field" {
                return None;
            }

            let kind = classify_field(child, content);
            (!matches!(kind, BlockKind::Code | BlockKind::Gap)).then_some((child, kind))
        })
        .collect()
}

fn collect_table_review_items<'tree>(
    table: Node<'tree>,
    content: &str,
) -> Vec<(Node<'tree>, BlockKind)> {
    let mut cursor = table.walk();
    table
        .named_children(&mut cursor)
        .filter_map(|child| {
            let kind = if child.kind() == "field" {
                classify_field(child, content)
            } else {
                classify_positional_table_child(child, content)
            };
            (!matches!(kind, BlockKind::Code | BlockKind::Gap)).then_some((child, kind))
        })
        .collect()
}

fn classify_field(node: Node<'_>, content: &str) -> BlockKind {
    let Some(value) = node.child_by_field_name("value") else {
        return BlockKind::CodeParagraph;
    };
    let Some(target) = node.child_by_field_name("name") else {
        return classify_positional_table_child(value, content);
    };

    if call_is_import_like(value, content) {
        return BlockKind::Import;
    }
    if value.kind() == "table_constructor" {
        return BlockKind::Module;
    }
    if value.kind() == "function_definition" {
        return classify_function_value(target, value, content, true);
    }

    if name_is_constant(target_name(target, content).as_deref()) {
        BlockKind::Const
    } else {
        BlockKind::Variable
    }
}

fn classify_positional_table_child(node: Node<'_>, content: &str) -> BlockKind {
    if call_is_import_like(node, content) {
        return BlockKind::Import;
    }

    match node.kind() {
        "comment" => BlockKind::Comment,
        "table_constructor" => BlockKind::Module,
        "function_definition" => BlockKind::Function,
        _ => BlockKind::CodeParagraph,
    }
}

fn sub_split(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like,
        },
        BlockKind::Module => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_module_like,
        },
        _ => default_code_sub_split(kind),
    }
}

fn split_function_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse(&block.content).context("Failed to parse lua function-like block")?;
    let root = tree.root_node();
    let signature_end = primary_function_node(root)
        .and_then(|function_node| {
            function_node
                .child_by_field_name("body")
                .map(|body| signature_end_for_function(&block.content, function_node, body))
                .or_else(|| signature_end_after_parameters(&block.content, function_node))
        })
        .unwrap_or(0);
    if signature_end == 0 || signature_end >= block.content.len() {
        return sub_splitter::split_code_review_units(block);
    }

    let mut blocks = vec![create_sub_block(
        block,
        &block.content[..signature_end],
        0,
        signature_end,
        BlockKind::FunctionSignature,
    )];
    blocks.extend(split_function_body_review_units(
        block,
        &block.content[signature_end..],
        signature_end,
    ));

    Ok(blocks)
}

fn primary_function_node(root: Node<'_>) -> Option<Node<'_>> {
    let top_level = first_named_child(root)?;
    match top_level.kind() {
        "function_declaration" => Some(top_level),
        "assignment_statement" | "variable_declaration" | "implicit_variable_declaration" => {
            assignment_binding(top_level)
                .and_then(|(_, value)| value)
                .filter(|value| value.kind() == "function_definition")
        }
        _ => find_named_descendant_any(root, &["function_declaration", "function_definition"]),
    }
}

fn split_module_like(block: &Block) -> Result<Vec<Block>> {
    let tree = parse(&block.content).context("Failed to parse lua module-like block")?;
    let root = tree.root_node();
    let Some(top_level) = first_module_statement(root) else {
        return sub_splitter::split_code_review_units(block);
    };
    let Some(table) = primary_table_constructor(top_level) else {
        return sub_splitter::split_code_review_units(block);
    };

    let items = collect_table_review_items(table, &block.content)
        .into_iter()
        .map(|(field, kind)| {
            create_sub_block(
                block,
                &block.content[field.start_byte()..field.end_byte()],
                field.start_byte(),
                field.end_byte(),
                kind,
            )
        })
        .collect::<Vec<_>>();

    if items.is_empty() {
        sub_splitter::split_code_review_units(block)
    } else {
        Ok(items)
    }
}

fn first_module_statement(root: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| primary_table_constructor(*child).is_some())
}

fn parse(source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&parser_language(source))?;
    parser
        .parse(source, None)
        .context("Failed to parse lua source")
}

fn find_named_descendant_any<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
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

fn signature_end_for_function(content: &str, function_node: Node<'_>, body: Node<'_>) -> usize {
    signature_end_after_parameters(content, function_node)
        .unwrap_or_else(|| signature_end_before_body(content, body.start_byte()))
}

fn signature_end_after_parameters(content: &str, function_node: Node<'_>) -> Option<usize> {
    let parameters = function_node.child_by_field_name("parameters")?;
    let search_start = parameters.end_byte().min(content.len());
    if let Some(newline_offset) = content[search_start..].find('\n') {
        Some(search_start + newline_offset + 1)
    } else {
        Some(search_start)
    }
}

fn signature_end_before_body(content: &str, body_start: usize) -> usize {
    if body_start == 0 || body_start > content.len() {
        return body_start.min(content.len());
    }

    let prefix = &content[..body_start];
    prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(body_start)
}

fn split_function_body_review_units(
    parent: &Block,
    content: &str,
    base_offset: usize,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut start_offset = 0;

    for mat in paragraph_break_regex().find_iter(content) {
        let end_offset = mat.start();
        if start_offset < end_offset {
            push_review_chunk(
                &mut blocks,
                parent,
                content,
                base_offset,
                start_offset,
                end_offset,
            );
        }

        blocks.push(create_sub_block(
            parent,
            &content[mat.start()..mat.end()],
            base_offset + mat.start(),
            base_offset + mat.end(),
            BlockKind::Gap,
        ));

        start_offset = mat.end();
    }

    if start_offset < content.len() {
        push_review_chunk(
            &mut blocks,
            parent,
            content,
            base_offset,
            start_offset,
            content.len(),
        );
    }

    blocks
}

fn push_review_chunk(
    blocks: &mut Vec<Block>,
    parent: &Block,
    source: &str,
    base_offset: usize,
    start: usize,
    end: usize,
) {
    let chunk = &source[start..end];
    let kind = classify_review_chunk(chunk);
    if matches!(kind, BlockKind::Gap) {
        return;
    }

    if let Some(prefix_len) = leading_lua_comment_prefix_len(chunk)
        && prefix_len < chunk.len()
    {
        blocks.push(create_sub_block(
            parent,
            &chunk[..prefix_len],
            base_offset + start,
            base_offset + start + prefix_len,
            BlockKind::Comment,
        ));

        let rest = &chunk[prefix_len..];
        let rest_kind = classify_review_chunk(rest);
        if !matches!(rest_kind, BlockKind::Gap) {
            blocks.push(create_sub_block(
                parent,
                rest,
                base_offset + start + prefix_len,
                base_offset + end,
                rest_kind,
            ));
        }
        return;
    }

    blocks.push(create_sub_block(
        parent,
        chunk,
        base_offset + start,
        base_offset + end,
        kind,
    ));
}

fn leading_lua_comment_prefix_len(chunk: &str) -> Option<usize> {
    let trimmed = chunk.trim_start();
    if let Some(span) = lua_long_comment_span(trimmed) {
        let mut prefix_len = chunk.len().saturating_sub(trimmed.len()) + span;
        if chunk[prefix_len..].starts_with("\r\n") {
            prefix_len += 2;
        } else if chunk[prefix_len..].starts_with('\n') {
            prefix_len += 1;
        }
        if prefix_len < chunk.len() {
            return Some(prefix_len);
        }
    }

    let mut offset = 0;
    let mut saw_comment = false;

    while offset < chunk.len() {
        let rest = &chunk[offset..];
        let line = rest.split_inclusive('\n').next().unwrap_or(rest);
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            offset += line.len();
            continue;
        }
        if trimmed.starts_with("--") {
            saw_comment = true;
            offset += line.len();
            continue;
        }

        return saw_comment.then_some(offset);
    }

    None
}

fn lua_long_comment_span(source: &str) -> Option<usize> {
    let rest = source.strip_prefix("--[")?;
    let eq_count = rest.chars().take_while(|ch| *ch == '=').count();
    let rest = &rest[eq_count..];
    let body = rest.strip_prefix('[')?;
    let closer = format!("]{}]", "=".repeat(eq_count));
    let close_idx = body.find(&closer)?;
    Some(4 + eq_count + close_idx + closer.len())
}

fn chunk_is_lua_comment_only(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Some(span) = lua_long_comment_span(trimmed)
        && trimmed[span..].trim().is_empty()
    {
        return true;
    }

    chunk
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| line.trim_start().starts_with("--"))
}

fn classify_review_chunk(chunk: &str) -> BlockKind {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return BlockKind::Gap;
    }
    if trimmed.chars().all(|ch| ch == ',') {
        return BlockKind::Gap;
    }
    if chunk_is_lua_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn create_sub_block(
    parent: &Block,
    content: &str,
    start_offset: usize,
    _end_offset: usize,
    kind: BlockKind,
) -> Block {
    let offset_newlines = parent.content[..start_offset]
        .chars()
        .filter(|ch| *ch == '\n')
        .count();
    let chunk_newlines = content.chars().filter(|ch| *ch == '\n').count();

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
