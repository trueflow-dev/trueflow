use super::{
    LanguageRegistration, LanguageSubSplitSemantics, NestedBlock, SubSplitRegistration,
    TopLevelRegistration, default_code_sub_split, no_attribute_nodes,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};

use crate::sub_splitter;
use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            collect_test_ranges,
            custom_splitter: None,
        },
        sub_split: sub_split_registration,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_clojure::LANGUAGE.into()
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function | BlockKind::Method | BlockKind::Macro => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like_block,
        },
        BlockKind::Module | BlockKind::Interface | BlockKind::Struct | BlockKind::Type => {
            SubSplitRegistration {
                semantics: LanguageSubSplitSemantics::ReviewUnits,
                splitter: split_container_like_block,
            }
        }
        _ => default_code_sub_split(kind),
    }
}

fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "comment" | "dis_expr" => BlockKind::Comment,
        "list_lit" => form_kind(node, content),
        _ => BlockKind::Code,
    }
}

fn form_kind(node: Node<'_>, content: &str) -> BlockKind {
    match normalized_form_head(node, content).as_deref() {
        Some("ns" | "in-ns") => BlockKind::Module,
        Some("require" | "require-macros" | "import" | "use" | "refer-clojure") => {
            BlockKind::Import
        }
        Some("def" | "defonce" | "definline") => BlockKind::Variable,
        Some("defn" | "defn-" | "defmulti" | "deftest") => BlockKind::Function,
        Some("defmethod") => BlockKind::Method,
        Some("defmacro" | "defmacro-") => BlockKind::Macro,
        Some("defprotocol") => BlockKind::Interface,
        Some("defrecord") => BlockKind::Struct,
        Some("deftype") => BlockKind::Type,
        Some("extend-type" | "extend-protocol") => BlockKind::Impl,
        Some("comment") => BlockKind::Comment,
        _ => BlockKind::Code,
    }
}

fn collect_nested_blocks(node: Node<'_>, content: &str, _lang: Language) -> Vec<NestedBlock> {
    let spans = match map_kind(node, content) {
        BlockKind::Module => collect_ns_clause_spans(node, content),
        BlockKind::Interface => collect_protocol_method_spans(node, content),
        BlockKind::Struct | BlockKind::Type => collect_record_like_method_spans(node, content),
        _ => Vec::new(),
    };

    spans
        .into_iter()
        .map(|span| NestedBlock {
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            kind: span.kind,
        })
        .collect()
}

fn collect_test_ranges(tree: &Tree, source: &str) -> Result<Vec<ByteSpan>> {
    let mut ranges = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "list_lit"
            && normalized_form_head(node, source).as_deref() == Some("deftest")
        {
            ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
        }

        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    Ok(ranges)
}

fn split_function_like_block(block: &Block) -> Result<Vec<Block>> {
    let tree = parse(&block.content).context("Failed to parse Clojure function-like block")?;
    let root = tree.root_node();
    let Some(form) = primary_form(root) else {
        return sub_splitter::split_code_review_units(block);
    };

    let Some(signature_end) = function_body_start(form, &block.content) else {
        return sub_splitter::split_code_review_units(block);
    };
    if signature_end == 0 || signature_end >= block.content.len() {
        return sub_splitter::split_code_review_units(block);
    }

    let mut blocks = vec![create_sub_block(
        block,
        0,
        signature_end,
        BlockKind::FunctionSignature,
    )?];
    blocks.extend(split_review_tail(block, signature_end)?);
    Ok(blocks)
}

fn split_container_like_block(block: &Block) -> Result<Vec<Block>> {
    let tree = parse(&block.content).context("Failed to parse Clojure container block")?;
    let root = tree.root_node();
    let Some(form) = primary_form(root) else {
        return sub_splitter::split_code_review_units(block);
    };

    let spans = match block.kind {
        BlockKind::Module => collect_ns_clause_spans(form, &block.content),
        BlockKind::Interface => collect_protocol_method_spans(form, &block.content),
        BlockKind::Struct | BlockKind::Type => {
            collect_record_like_method_spans(form, &block.content)
        }
        _ => Vec::new(),
    };
    if spans.is_empty() {
        return sub_splitter::split_code_review_units(block);
    }

    let mut blocks = Vec::new();
    let mut current = 0;

    if let Some(first) = spans.first().copied()
        && first.start_byte > 0
    {
        blocks.push(create_sub_block(block, 0, first.start_byte, block.kind)?);
        current = first.start_byte;
    }

    for span in spans {
        if current < span.start_byte {
            push_non_child_chunk(block, current, span.start_byte, &mut blocks)?;
        }
        blocks.push(create_sub_block(
            block,
            span.start_byte,
            span.end_byte,
            span.kind,
        )?);
        current = span.end_byte;
    }

    if current < block.content.len() {
        push_non_child_chunk(block, current, block.content.len(), &mut blocks)?;
    }

    Ok(blocks)
}

fn parse(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&parser_language(source))?;
    parser
        .parse(source, None)
        .context("Failed to parse Clojure source")
}

fn primary_form(root: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "list_lit")
}

fn function_body_start(form: Node<'_>, source: &str) -> Option<usize> {
    let values = value_children(form);
    let head = normalized_form_head(form, source)?;

    match head.as_str() {
        "defn" | "defn-" | "defmacro" | "defmacro-" => {
            let mut index = skip_optional_doc_and_attr_map(&values, 2);
            match values.get(index).map(|node| node.kind()) {
                Some("vec_lit") => {
                    index += 1;
                    if matches!(values.get(index).map(|node| node.kind()), Some("map_lit")) {
                        index += 1;
                    }
                    values.get(index).map(|node| node.start_byte())
                }
                Some("list_lit") => values.get(index).map(|node| node.start_byte()),
                _ => generic_callable_body_start(&values),
            }
        }
        "deftest" => values
            .get(skip_optional_doc_and_attr_map(&values, 2))
            .map(|node| node.start_byte()),
        "defmethod" => {
            let mut index = skip_optional_doc_and_attr_map(&values, 3);
            if matches!(values.get(index).map(|node| node.kind()), Some("vec_lit")) {
                index += 1;
                if matches!(values.get(index).map(|node| node.kind()), Some("map_lit")) {
                    index += 1;
                }
                values.get(index).map(|node| node.start_byte())
            } else {
                None
            }
        }
        "defmulti" => values
            .get(skip_optional_doc_and_attr_map(&values, 2))
            .map(|node| node.start_byte()),
        _ => generic_callable_body_start(&values),
    }
}

fn generic_callable_body_start(values: &[Node<'_>]) -> Option<usize> {
    match values.get(1).map(|node| node.kind()) {
        Some("vec_lit") => {
            let mut index = 2;
            if matches!(values.get(index).map(|node| node.kind()), Some("map_lit")) {
                index += 1;
            }
            values.get(index).map(|node| node.start_byte())
        }
        Some("list_lit") => values.get(1).map(|node| node.start_byte()),
        _ => None,
    }
}

fn collect_ns_clause_spans(form: Node<'_>, source: &str) -> Vec<SemanticSpan> {
    let values = value_children(form);
    let start = skip_optional_doc_and_attr_map(&values, 2);

    values[start..]
        .iter()
        .copied()
        .filter_map(|value| {
            if value.kind() != "list_lit" {
                return None;
            }

            (ns_clause_kind(value, source) == Some(BlockKind::Import)).then_some(SemanticSpan {
                start_byte: value.start_byte(),
                end_byte: value.end_byte(),
                kind: BlockKind::Import,
            })
        })
        .collect()
}

fn ns_clause_kind(form: Node<'_>, source: &str) -> Option<BlockKind> {
    let head = first_form_value(form)?;
    let keyword = keyword_text(head, source)?;
    match keyword.as_str() {
        ":require" | ":require-macros" | ":use" | ":import" | ":refer-clojure" => {
            Some(BlockKind::Import)
        }
        _ => None,
    }
}

fn collect_protocol_method_spans(form: Node<'_>, _source: &str) -> Vec<SemanticSpan> {
    let values = value_children(form);
    let start = skip_optional_doc_and_attr_map(&values, 2);

    values[start..]
        .iter()
        .copied()
        .filter_map(|value| {
            callable_list_has_parameter_vector(value).then_some(SemanticSpan {
                start_byte: value.start_byte(),
                end_byte: value.end_byte(),
                kind: BlockKind::FunctionSignature,
            })
        })
        .collect()
}

fn collect_record_like_method_spans(form: Node<'_>, _source: &str) -> Vec<SemanticSpan> {
    let values = value_children(form);
    let start = values
        .iter()
        .position(|node| node.kind() == "vec_lit")
        .map_or(values.len(), |index| index + 1);

    values[start..]
        .iter()
        .copied()
        .filter_map(|value| {
            callable_list_has_parameter_vector(value).then_some(SemanticSpan {
                start_byte: value.start_byte(),
                end_byte: value.end_byte(),
                kind: BlockKind::Method,
            })
        })
        .collect()
}

fn callable_list_has_parameter_vector(node: Node<'_>) -> bool {
    if node.kind() != "list_lit" {
        return false;
    }

    let values = value_children(node);
    matches!(values.first().map(|value| value.kind()), Some("sym_lit"))
        && matches!(values.get(1).map(|value| value.kind()), Some("vec_lit"))
}

fn split_review_tail(parent: &Block, start_offset: usize) -> Result<Vec<Block>> {
    let rest = &parent.content[start_offset..];
    let re = paragraph_break_regex();
    let mut blocks = Vec::new();

    let mut push_chunk = |chunk: &str, start: usize, end: usize, is_gap: bool| -> Result<()> {
        let start = start_offset + start;
        let end = start_offset + end;

        if is_gap {
            blocks.push(create_sub_block(parent, start, end, BlockKind::Gap)?);
            return Ok(());
        }

        if let Some(comment_end) = leading_semicolon_comment_prefix_len(chunk) {
            let comment_end_abs = start + comment_end;
            blocks.push(create_sub_block(
                parent,
                start,
                comment_end_abs,
                BlockKind::Comment,
            )?);

            if !chunk[comment_end..].trim().is_empty() {
                blocks.push(create_sub_block(
                    parent,
                    comment_end_abs,
                    end,
                    BlockKind::CodeParagraph,
                )?);
            }
            return Ok(());
        }

        let kind = if chunk_is_clojure_comment_only(chunk) {
            BlockKind::Comment
        } else {
            BlockKind::CodeParagraph
        };
        blocks.push(create_sub_block(parent, start, end, kind)?);
        Ok(())
    };

    let mut start = 0;
    for mat in re.find_iter(rest) {
        if start < mat.start() {
            let chunk = &rest[start..mat.start()];
            if !chunk.is_empty() {
                push_chunk(chunk, start, mat.start(), false)?;
            }
        }

        let gap = &rest[mat.start()..mat.end()];
        push_chunk(gap, mat.start(), mat.end(), true)?;
        start = mat.end();
    }

    if start < rest.len() {
        let chunk = &rest[start..];
        if !chunk.is_empty() {
            push_chunk(chunk, start, rest.len(), false)?;
        }
    }

    Ok(blocks)
}

fn leading_semicolon_comment_prefix_len(chunk: &str) -> Option<usize> {
    let mut offset = 0;
    let mut saw_comment = false;

    for line in chunk.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            offset += line.len();
            continue;
        }
        if trimmed.starts_with(';') {
            saw_comment = true;
            offset += line.len();
            continue;
        }

        return saw_comment.then_some(offset);
    }

    saw_comment.then_some(offset)
}

fn chunk_is_clojure_comment_only(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return false;
    }

    chunk
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| line.trim_start().starts_with(';'))
}

fn push_non_child_chunk(
    parent: &Block,
    start: usize,
    end: usize,
    blocks: &mut Vec<Block>,
) -> Result<()> {
    if start >= end {
        return Ok(());
    }

    let chunk = &parent.content[start..end];
    let kind = if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if chunk_is_clojure_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    };

    blocks.push(create_sub_block(parent, start, end, kind)?);
    Ok(())
}

fn create_sub_block(
    parent: &Block,
    start_offset: usize,
    end_offset: usize,
    kind: BlockKind,
) -> Result<Block> {
    let mut block = Block::from_parent_range(parent, kind, ByteSpan::new(start_offset, end_offset))
        .context("Clojure sub-split range must be a valid parent UTF-8 slice")?;
    block.tags = parent.tags.clone();
    Ok(block)
}

fn value_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children_by_field_name("value", &mut cursor).collect()
}

fn first_form_value(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children_by_field_name("value", &mut cursor).next()
}

fn normalized_form_head(node: Node<'_>, source: &str) -> Option<String> {
    let head = first_form_value(node)?;
    let symbol = symbol_text(head, source)?;
    Some(
        symbol
            .rsplit('/')
            .next()
            .unwrap_or(symbol.as_str())
            .to_string(),
    )
}

fn symbol_text(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() != "sym_lit" {
        return None;
    }

    let name = node
        .child_by_field_name("name")?
        .utf8_text(source.as_bytes())
        .ok()?;

    let namespace = node
        .child_by_field_name("namespace")
        .and_then(|ns| ns.utf8_text(source.as_bytes()).ok());

    Some(match namespace {
        Some(namespace) => format!("{namespace}/{name}"),
        None => name.to_string(),
    })
}

fn keyword_text(node: Node<'_>, source: &str) -> Option<String> {
    (node.kind() == "kwd_lit")
        .then(|| node.utf8_text(source.as_bytes()).ok())
        .flatten()
        .map(str::to_string)
}

fn skip_optional_doc_and_attr_map(values: &[Node<'_>], start: usize) -> usize {
    let mut index = start;
    if matches!(values.get(index).map(|node| node.kind()), Some("str_lit")) {
        index += 1;
    }
    if matches!(values.get(index).map(|node| node.kind()), Some("map_lit")) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_form_kind_maps_supported_heads() {
        let source = "(ns demo.core (:require [clojure.string :as str]))\n(defn run [x] x)\n(defmacro with-x [x] x)\n(defmulti render :kind)\n(defmethod render :text [value] value)\n(defprotocol Renderable (render [this]))\n(defrecord User [name] Renderable (render [this] name))\n(deftype Counter [value] Object (toString [_] (str value)))\n(deftest run-test (is true))\n(require '[clojure.set :as set])\n";
        let tree = parse(source).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let kinds = root
            .named_children(&mut cursor)
            .map(|node| map_kind(node, source))
            .collect::<Vec<_>>();

        assert_eq!(
            kinds,
            vec![
                BlockKind::Module,
                BlockKind::Function,
                BlockKind::Macro,
                BlockKind::Function,
                BlockKind::Method,
                BlockKind::Interface,
                BlockKind::Struct,
                BlockKind::Type,
                BlockKind::Function,
                BlockKind::Import,
            ]
        );
    }

    #[test]
    fn test_collect_nested_blocks_finds_ns_and_record_children() {
        let source = "(ns demo.core (:require [clojure.string :as str]) (:import (java.time Instant)))\n(defrecord User [name] Renderable (render [this] name) (label [this prefix] prefix))\n(defprotocol Renderable (render [this]))\n";
        let tree = parse(source).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let nodes = root.named_children(&mut cursor).collect::<Vec<_>>();

        let ns_children = collect_nested_blocks(nodes[0], source, Language::Clojure);
        assert_eq!(
            ns_children
                .iter()
                .map(|child| child.kind)
                .collect::<Vec<_>>(),
            vec![BlockKind::Import, BlockKind::Import]
        );

        let record_children = collect_nested_blocks(nodes[1], source, Language::Clojure);
        assert_eq!(
            record_children
                .iter()
                .map(|child| child.kind)
                .collect::<Vec<_>>(),
            vec![BlockKind::Method, BlockKind::Method]
        );

        let protocol_children = collect_nested_blocks(nodes[2], source, Language::Clojure);
        assert_eq!(
            protocol_children
                .iter()
                .map(|child| child.kind)
                .collect::<Vec<_>>(),
            vec![BlockKind::FunctionSignature]
        );
    }

    #[test]
    fn test_ns_sub_split_does_not_fold_non_import_tail_into_import_clause() {
        let source = "(ns demo.core\n  (:require [clojure.string :as str])\n  (:gen-class))\n";
        let block =
            Block::from_file_range(source, BlockKind::Module, ByteSpan::new(0, source.len()))
                .unwrap();

        let blocks = split_container_like_block(&block).unwrap();
        let Some(import) = blocks.iter().find(|block| block.kind == BlockKind::Import) else {
            panic!("expected import block");
        };

        assert!(import.content.contains(":require"));
        assert!(!import.content.contains(":gen-class"));
    }

    #[test]
    fn test_function_body_start_supports_named_and_method_like_forms() {
        let source = "(defn run\n  [value]\n  (inc value))\n(render-item [this]\n  name)\n";
        let tree = parse(source).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let forms = root.named_children(&mut cursor).collect::<Vec<_>>();

        assert!(function_body_start(forms[0], source).is_some());
        assert!(function_body_start(forms[1], source).is_some());
    }
}
