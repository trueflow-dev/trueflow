use super::{
    LanguageRegistration, TopLevelRegistration, default_code_sub_split, default_map_kind,
    no_attribute_nodes, no_nested_blocks, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::complexity;

use anyhow::Result;
use tree_sitter::{Language as TsLanguage, Node};

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
        sub_split: default_code_sub_split,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_sql::LANGUAGE.into()
}

fn split_top_level(root: Node<'_>, content: &str, lang: Language) -> Result<Vec<Block>> {
    let mut cursor = root.walk();
    let children: Vec<_> = root.children(&mut cursor).collect();

    let mut blocks = Vec::new();
    let mut pending_comment_start = None;
    let mut pending_comment_end = 0usize;
    let mut last_end = 0usize;
    let mut index = 0usize;

    while index < children.len() {
        let child = children[index];
        let kind = child.kind();

        if is_comment_kind(kind) {
            if pending_comment_start.is_none() {
                pending_comment_start = Some(child.start_byte());
            }
            pending_comment_end = child.end_byte();
            index += 1;
            continue;
        }

        if kind == ";" || !child.is_named() {
            index += 1;
            continue;
        }

        let start = pending_comment_start.take().unwrap_or(child.start_byte());
        push_non_whitespace_gap(&mut blocks, content, last_end, start, lang)?;

        let (end, next_index) =
            extend_with_statement_terminator(&children, index, child.end_byte());
        blocks.push(create_file_block(
            &content[start..end],
            classify_top_level_kind(child, content),
            content,
            start,
            end,
            lang,
        )?);

        last_end = end;
        pending_comment_end = 0;
        index = next_index;
    }

    if let Some(start) = pending_comment_start {
        push_non_whitespace_gap(&mut blocks, content, last_end, start, lang)?;
        blocks.push(create_file_block(
            &content[start..pending_comment_end],
            BlockKind::Comment,
            content,
            start,
            pending_comment_end,
            lang,
        )?);
        last_end = pending_comment_end;
    }

    push_non_whitespace_gap(&mut blocks, content, last_end, content.len(), lang)?;

    Ok(blocks)
}

fn classify_top_level_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "statement" => classify_statement(node, content),
        "transaction" | "block" => BlockKind::Code,
        kind if is_comment_kind(kind) => BlockKind::Comment,
        _ => BlockKind::Code,
    }
}

fn classify_statement(statement: Node<'_>, content: &str) -> BlockKind {
    let statement_text = &content[statement.start_byte()..statement.end_byte()];
    statement_primary_node(statement).map_or_else(
        || classify_statement_text_prefix(statement_text),
        classify_statement_node,
    )
}

fn statement_primary_node(statement: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = statement.walk();
    statement.children(&mut cursor).find(|child| {
        child.is_named()
            && !is_comment_kind(child.kind())
            && matches!(
                child.kind(),
                "comment_statement"
                    | "create_database"
                    | "create_extension"
                    | "create_function"
                    | "create_index"
                    | "create_materialized_view"
                    | "create_role"
                    | "create_schema"
                    | "create_sequence"
                    | "create_table"
                    | "create_trigger"
                    | "create_type"
                    | "create_view"
                    | "delete"
                    | "drop_database"
                    | "drop_extension"
                    | "drop_function"
                    | "drop_index"
                    | "drop_role"
                    | "drop_schema"
                    | "drop_sequence"
                    | "drop_table"
                    | "drop_type"
                    | "drop_view"
                    | "insert"
                    | "reset_statement"
                    | "select"
                    | "set_operation"
                    | "set_statement"
                    | "update"
                    | "alter_database"
                    | "alter_index"
                    | "alter_role"
                    | "alter_schema"
                    | "alter_sequence"
                    | "alter_table"
                    | "alter_type"
                    | "alter_view"
            )
    })
}

fn classify_statement_node(node: Node<'_>) -> BlockKind {
    match node.kind() {
        "comment_statement" => BlockKind::Comment,
        "create_table" | "alter_table" | "drop_table" => BlockKind::Struct,
        "create_view" | "create_materialized_view" | "alter_view" | "drop_view" => {
            BlockKind::Module
        }
        "create_function" | "drop_function" | "create_trigger" => BlockKind::Function,
        "create_schema" | "create_database" | "create_extension" | "alter_schema"
        | "alter_database" | "drop_schema" | "drop_database" | "drop_extension" | "create_role"
        | "alter_role" | "drop_role" => BlockKind::Module,
        "create_sequence" | "alter_sequence" | "drop_sequence" => BlockKind::Variable,
        "create_index" | "alter_index" | "drop_index" => BlockKind::Type,
        "create_type" => classify_create_type(node),
        "alter_type" | "drop_type" => BlockKind::Type,
        "select" | "set_operation" | "insert" | "update" | "delete" | "set_statement"
        | "reset_statement" => BlockKind::Code,
        _ => BlockKind::Code,
    }
}

fn classify_create_type(node: Node<'_>) -> BlockKind {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "enum_elements" => return BlockKind::Enum,
            "column_definitions" => return BlockKind::Struct,
            _ => {}
        }
    }

    BlockKind::Type
}

fn classify_statement_text_prefix(statement_text: &str) -> BlockKind {
    let text = statement_text.trim_start().to_ascii_lowercase();

    if text.starts_with("comment ") {
        return BlockKind::Comment;
    }
    if text.starts_with("create view")
        || text.starts_with("create materialized view")
        || text.starts_with("alter view")
        || text.starts_with("drop view")
    {
        return BlockKind::Module;
    }
    if text.starts_with("create function")
        || text.starts_with("drop function")
        || text.starts_with("create trigger")
    {
        return BlockKind::Function;
    }
    if text.starts_with("create table")
        || text.starts_with("alter table")
        || text.starts_with("drop table")
    {
        return BlockKind::Struct;
    }
    if text.starts_with("create type")
        || text.starts_with("alter type")
        || text.starts_with("drop type")
    {
        return BlockKind::Type;
    }
    if text.starts_with("create schema")
        || text.starts_with("create database")
        || text.starts_with("alter schema")
        || text.starts_with("alter database")
        || text.starts_with("drop schema")
        || text.starts_with("drop database")
    {
        return BlockKind::Module;
    }

    BlockKind::Code
}

fn extend_with_statement_terminator(
    children: &[Node<'_>],
    index: usize,
    default_end: usize,
) -> (usize, usize) {
    let mut end = default_end;
    let mut next_index = index + 1;

    while let Some(next) = children.get(next_index) {
        if next.kind() == ";" {
            return (next.end_byte(), next_index + 1);
        }
        if is_comment_kind(next.kind()) {
            end = next.end_byte();
            next_index += 1;
            continue;
        }
        if !next.is_named() {
            next_index += 1;
            continue;
        }
        break;
    }

    (end, next_index)
}

fn is_comment_kind(kind: &str) -> bool {
    matches!(kind, "comment" | "marginalia")
}

fn push_non_whitespace_gap(
    blocks: &mut Vec<Block>,
    content: &str,
    start: usize,
    end: usize,
    lang: Language,
) -> Result<()> {
    if start >= end {
        return Ok(());
    }

    let chunk = &content[start..end];
    if chunk
        .trim_matches(|ch: char| ch.is_whitespace() || ch == ';')
        .is_empty()
    {
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

fn create_file_block(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Result<Block> {
    let mut block = Block::from_file_range(full_source, kind, ByteSpan::new(start_byte, end_byte))?;
    assert_eq!(block.content, text, "SQL top-level range must name content");
    block.complexity = complexity::calculate(&block.content, lang);
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_tree(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_sql::LANGUAGE.into())
            .unwrap_or_else(|error| panic!("set SQL language: {error}"));
        parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("parse SQL source"))
    }


    #[test]
    fn split_top_level_attaches_leading_comment_to_following_statement() {
        let source = "-- docs for report\nSELECT id FROM accounts;\n";
        let tree = parse_tree(source);
        let blocks = split_top_level(tree.root_node(), source, Language::Sql)
            .unwrap_or_else(|error| panic!("split SQL comments: {error}"));
        let select = blocks
            .iter()
            .find(|block| block.kind == BlockKind::Code)
            .unwrap_or_else(|| panic!("missing select block: {blocks:#?}"));

        assert!(select.content.starts_with("-- docs for report"));
        assert!(select.content.trim_end().ends_with(';'));
    }
}
