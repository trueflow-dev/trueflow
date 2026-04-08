use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use anyhow::Result;
use tree_sitter::{Language as TsLanguage, Node, Tree};

mod clojure;
mod css;
mod dart;
mod elixir;
mod haskell;
mod html;
mod json;
mod lua;
mod ocaml;
mod scala;
mod sql;
mod yaml;
mod zig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NestedBlock {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) kind: BlockKind,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LanguageSubSplitSemantics {
    ReviewUnits,
    StructuralChildren,
}

pub(crate) type ParserLanguageFn = fn(&str) -> TsLanguage;
pub(crate) type TopLevelKindMapper = for<'tree> fn(Node<'tree>, &str) -> BlockKind;
pub(crate) type AttributeNodeDetector = fn(&str) -> bool;
pub(crate) type NestedBlockCollector =
    for<'tree> fn(Node<'tree>, &str, Language) -> Vec<NestedBlock>;
pub(crate) type TestRangeCollector = fn(&Tree, &str) -> Result<Vec<ByteSpan>>;
pub(crate) type SubSplitter = fn(&Block) -> Result<Vec<Block>>;
pub(crate) type SubSplitRegistrationFn = fn(BlockKind) -> SubSplitRegistration;

#[derive(Clone, Copy)]
pub(crate) struct TopLevelRegistration {
    pub(crate) parser_language: ParserLanguageFn,
    pub(crate) map_kind: TopLevelKindMapper,
    pub(crate) is_attribute_node: AttributeNodeDetector,
    pub(crate) collect_nested_blocks: NestedBlockCollector,
    pub(crate) collect_test_ranges: TestRangeCollector,
}

#[derive(Clone, Copy)]
pub(crate) struct SubSplitRegistration {
    pub(crate) semantics: LanguageSubSplitSemantics,
    pub(crate) splitter: SubSplitter,
}

#[derive(Clone, Copy)]
pub(crate) struct LanguageRegistration {
    pub(crate) top_level: TopLevelRegistration,
    pub(crate) sub_split: SubSplitRegistrationFn,
}

#[cfg(test)]
const REGISTERED_LANGUAGES: &[Language] = &[
    Language::Zig,
    Language::Lua,
    Language::Dart,
    Language::Scala,
    Language::Haskell,
    Language::OCaml,
    Language::Elixir,
    Language::Clojure,
    Language::Sql,
    Language::Yaml,
    Language::Json,
    Language::Html,
    Language::Css,
];

#[cfg(test)]
pub(crate) fn registered_languages() -> &'static [Language] {
    REGISTERED_LANGUAGES
}

pub(crate) fn registration(lang: Language) -> Option<LanguageRegistration> {
    match lang {
        Language::Zig => Some(zig::registration()),
        Language::Lua => Some(lua::registration()),
        Language::Dart => Some(dart::registration()),
        Language::Scala => Some(scala::registration()),
        Language::Haskell => Some(haskell::registration()),
        Language::OCaml => Some(ocaml::registration()),
        Language::Elixir => Some(elixir::registration()),
        Language::Clojure => Some(clojure::registration()),
        Language::Sql => Some(sql::registration()),
        Language::Yaml => Some(yaml::registration()),
        Language::Json => Some(json::registration()),
        Language::Html => Some(html::registration()),
        Language::Css => Some(css::registration()),
        _ => None,
    }
}

pub(crate) fn generic_tree_sitter_registration(parser_language: ParserLanguageFn) -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind: default_map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks: no_nested_blocks,
            collect_test_ranges: no_test_ranges,
        },
        sub_split: default_code_sub_split,
    }
}

pub(crate) fn default_map_kind(_node: Node<'_>, _content: &str) -> BlockKind {
    BlockKind::Code
}

pub(crate) fn no_attribute_nodes(_kind: &str) -> bool {
    false
}

pub(crate) fn no_nested_blocks(
    _node: Node<'_>,
    _content: &str,
    _lang: Language,
) -> Vec<NestedBlock> {
    Vec::new()
}

pub(crate) fn no_test_ranges(_tree: &Tree, _source: &str) -> Result<Vec<ByteSpan>> {
    Ok(Vec::new())
}

pub(crate) fn default_code_sub_split(_kind: BlockKind) -> SubSplitRegistration {
    SubSplitRegistration {
        semantics: LanguageSubSplitSemantics::ReviewUnits,
        splitter: crate::sub_splitter::split_code_review_units,
    }
}
