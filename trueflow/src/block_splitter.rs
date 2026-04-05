use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
use crate::code_comments;
use crate::complexity;
use crate::hashing::TreeHash;
use crate::optimizer;
use crate::text_split::split_by_paragraph_breaks;
use crate::{rust, swift};
use anyhow::{Context, Result};
use std::sync::LazyLock;
use tracing::info;
use tree_sitter::{Language as TsLanguage, Parser, Query, QueryCursor, StreamingIterator};

static RUST_ATTR_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_rust::LANGUAGE.into(),
        "(attribute_item) @attr",
        "rust attribute query",
    )
});
static SWIFT_ATTR_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_swift::LANGUAGE.into(),
        "(attribute) @attr",
        "swift attribute query",
    )
});
static PYTHON_DECORATED_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_python::LANGUAGE.into(),
        "(decorated_definition (decorator) @decor (function_definition name: (identifier) @name) @func)",
        "python decorated test query",
    )
});
static PYTHON_FUNCTION_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_python::LANGUAGE.into(),
        "(function_definition name: (identifier) @name) @func",
        "python function test query",
    )
});
static JAVASCRIPT_ARROW_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_javascript::LANGUAGE.into(),
        "(call_expression function: (identifier) @name arguments: (arguments (arrow_function) @fn)) @call",
        "javascript arrow test query",
    )
});
static JAVASCRIPT_MEMBER_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_javascript::LANGUAGE.into(),
        "(call_expression function: (member_expression object: (identifier) @name)) @call",
        "javascript member test query",
    )
});
static TYPESCRIPT_ARROW_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "(call_expression function: (identifier) @name arguments: (arguments (arrow_function) @fn)) @call",
        "typescript arrow test query",
    )
});
static TYPESCRIPT_MEMBER_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "(call_expression function: (member_expression object: (identifier) @name)) @call",
        "typescript member test query",
    )
});
static SHELL_FUNCTION_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_bash::LANGUAGE.into(),
        "(function_definition name: (word) @name) @func",
        "shell function test query",
    )
});

fn compile_query(language: &TsLanguage, source: &str, name: &str) -> Query {
    match Query::new(language, source) {
        Ok(query) => query,
        Err(err) => panic!("invalid {name}: {err}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSplitStrategy {
    /// The input was empty, so there are no top-level blocks.
    EmptyInput,
    Structured,
    /// Language-specific heuristics were used instead of a parser-backed structure walk.
    Heuristic,
    /// Plain textual paragraph splitting was used intentionally.
    Textual,
    /// A code-oriented fallback splitter was used after a structured attempt degraded.
    FallbackCode,
    /// A text-oriented fallback splitter was used after a structured attempt degraded.
    FallbackText,
    /// The language is recognized as code but has no dedicated structured splitter yet.
    UnsupportedCode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSplitDiagnostic {
    pub reason: String,
}

impl BlockSplitDiagnostic {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockSplitResult {
    /// Raw top-level blocks produced by the splitter before review-time optimization.
    pub blocks: Vec<Block>,
    pub strategy: BlockSplitStrategy,
    pub diagnostics: Vec<BlockSplitDiagnostic>,
}

impl BlockSplitResult {
    fn new(
        blocks: Vec<Block>,
        strategy: BlockSplitStrategy,
        diagnostics: Vec<BlockSplitDiagnostic>,
    ) -> Self {
        Self {
            blocks,
            strategy,
            diagnostics,
        }
    }

    /// Convert raw split blocks into the optimized review-time block set.
    pub fn into_review_blocks(self) -> Vec<Block> {
        if self.blocks.is_empty() {
            Vec::new()
        } else {
            optimizer::optimize(self.blocks)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackMode {
    Code,
    Text,
}

impl FallbackMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Text => "text",
        }
    }

    fn strategy(self) -> BlockSplitStrategy {
        match self {
            Self::Code => BlockSplitStrategy::FallbackCode,
            Self::Text => BlockSplitStrategy::FallbackText,
        }
    }
}

/// Split a text file into raw top-level review blocks.
pub fn split(content: &str, lang: Language) -> BlockSplitResult {
    info!(
        "block_splitter start (lang={:?}, bytes={})",
        lang,
        content.len()
    );

    let result = split_non_empty(content, lang);

    info!(
        "block_splitter done (strategy={:?}, blocks={}, diagnostics={})",
        result.strategy,
        result.blocks.len(),
        result.diagnostics.len()
    );
    result
}

fn split_non_empty(content: &str, lang: Language) -> BlockSplitResult {
    if content.is_empty() {
        return BlockSplitResult::new(Vec::new(), BlockSplitStrategy::EmptyInput, Vec::new());
    }

    match lang {
        Language::Markdown => {
            attempt_split(content, lang, split_markdown(content), FallbackMode::Text)
        }
        Language::Just => complete_split(
            fallback_split_blocks(content, FallbackMode::Code, lang),
            BlockSplitStrategy::Heuristic,
            Vec::new(),
        ),
        Language::Go => {
            complete_split(split_go(content), BlockSplitStrategy::Heuristic, Vec::new())
        }
        Language::Cpp => complete_split(
            split_cpp(content),
            BlockSplitStrategy::Heuristic,
            Vec::new(),
        ),
        Language::Nix => attempt_split(content, lang, split_nix(content), FallbackMode::Code),
        _ if lang.uses_text_fallback() || matches!(lang, Language::Unknown) => complete_split(
            split_paragraphs(content, lang),
            BlockSplitStrategy::Textual,
            Vec::new(),
        ),
        Language::Rust
        | Language::Swift
        | Language::JavaScript
        | Language::TypeScript
        | Language::Java
        | Language::Python
        | Language::Shell => attempt_split(
            content,
            lang,
            split_tree_sitter(content, lang),
            FallbackMode::Code,
        ),
        _ => fallback_result(
            content,
            lang,
            FallbackMode::Code,
            BlockSplitStrategy::UnsupportedCode,
            format!("unsupported language {lang:?}; used code fallback"),
        ),
    }
}

fn attempt_split(
    content: &str,
    lang: Language,
    attempt: Result<Vec<Block>>,
    fallback_mode: FallbackMode,
) -> BlockSplitResult {
    match attempt {
        Ok(blocks) if !blocks.is_empty() => {
            complete_split(blocks, BlockSplitStrategy::Structured, Vec::new())
        }
        Ok(_) => fallback_result(
            content,
            lang,
            fallback_mode,
            fallback_mode.strategy(),
            format!(
                "{lang:?} splitter returned no blocks; used {} fallback",
                fallback_mode.as_str()
            ),
        ),
        Err(err) => fallback_result(
            content,
            lang,
            fallback_mode,
            fallback_mode.strategy(),
            format!(
                "{lang:?} splitter failed; used {} fallback: {err}",
                fallback_mode.as_str()
            ),
        ),
    }
}

fn fallback_result(
    content: &str,
    lang: Language,
    fallback_mode: FallbackMode,
    strategy: BlockSplitStrategy,
    reason: String,
) -> BlockSplitResult {
    let blocks = fallback_split_blocks(content, fallback_mode, lang);
    complete_split(blocks, strategy, vec![BlockSplitDiagnostic::new(reason)])
}

fn complete_split(
    blocks: Vec<Block>,
    strategy: BlockSplitStrategy,
    diagnostics: Vec<BlockSplitDiagnostic>,
) -> BlockSplitResult {
    BlockSplitResult::new(blocks, strategy, diagnostics)
}

fn split_tree_sitter(content: &str, lang: Language) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    let language = tree_sitter_language_for(lang)
        .ok_or_else(|| anyhow::anyhow!("No tree-sitter grammar configured for {lang:?}"))?;
    parser.set_language(&language)?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse source with tree-sitter")?;
    let root = tree.root_node();
    let mut blocks = Vec::new();

    let mut cursor = root.walk();
    let mut last_end_byte = 0;

    let test_spans = collect_test_line_spans(lang, &tree, content)?;

    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    for child in root.children(&mut cursor) {
        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        let ts_kind = child.kind();

        let is_attribute = match lang {
            Language::Rust => {
                ts_kind == "attribute_item"
                    || ts_kind == "line_comment"
                    || ts_kind == "block_comment"
            }
            Language::Python => ts_kind == "decorator",
            Language::Swift => swift::is_attribute_node(ts_kind),
            _ => false,
        };

        if is_attribute {
            if pending_start.is_none() {
                if start_byte > last_end_byte {
                    let gap = &content[last_end_byte..start_byte];
                    if !gap.trim().is_empty() {
                        blocks.push(create_block(
                            gap,
                            BlockKind::Gap,
                            content,
                            last_end_byte,
                            start_byte,
                            lang,
                        ));
                    }
                }
                pending_start = Some(start_byte);
            }
            pending_end = end_byte;
            continue;
        }

        let block_start = if let Some(ps) = pending_start {
            ps
        } else {
            if start_byte > last_end_byte {
                let gap = &content[last_end_byte..start_byte];
                if !gap.trim().is_empty() {
                    blocks.push(create_block(
                        gap,
                        BlockKind::Gap,
                        content,
                        last_end_byte,
                        start_byte,
                        lang,
                    ));
                }
            }
            start_byte
        };

        let node_content = &content[block_start..end_byte];
        blocks.push(create_block(
            node_content,
            map_kind_for_node(lang, child, content),
            content,
            block_start,
            end_byte,
            lang,
        ));

        if matches!(lang, Language::Rust) && matches!(ts_kind, "impl_item" | "trait_item") {
            blocks.extend(collect_rust_impl_items(child, content, lang));
        }
        if matches!(lang, Language::Swift)
            && matches!(ts_kind, "class_declaration" | "protocol_declaration")
        {
            blocks.extend(collect_swift_type_items(child, content, lang));
        }
        if matches!(lang, Language::Java)
            && matches!(
                ts_kind,
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
            )
        {
            blocks.extend(collect_java_type_items(child, content, lang));
        }

        last_end_byte = end_byte;
        pending_start = None;
        pending_end = 0;
    }

    if let Some(start) = pending_start {
        let node_content = &content[start..pending_end];
        blocks.push(create_block(
            node_content,
            BlockKind::Code,
            content,
            start,
            pending_end,
            lang,
        ));
        last_end_byte = pending_end;
    }

    if last_end_byte < content.len() {
        let gap = &content[last_end_byte..];
        if !gap.trim().is_empty() {
            blocks.push(create_block(
                gap,
                BlockKind::Gap,
                content,
                last_end_byte,
                content.len(),
                lang,
            ));
        }
    }

    apply_test_tags(&mut blocks, &test_spans);

    Ok(blocks)
}

fn tree_sitter_language_for(lang: Language) -> Option<TsLanguage> {
    match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

fn split_markdown(content: &str) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .context("Failed to load markdown grammar")?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse markdown")?;
    let root = tree.root_node();

    let mut headings = Vec::new();
    collect_markdown_headings(root, content, &mut headings);
    headings.sort_by_key(|heading| heading.start);

    let mut blocks = Vec::new();
    let mut section_start = 0;
    let mut current_level = 0;

    for heading in headings {
        if current_level == 0 {
            if heading.start > section_start {
                let chunk = &content[section_start..heading.start];
                if !chunk.trim().is_empty() {
                    blocks.push(create_block(
                        chunk,
                        BlockKind::Preamble,
                        content,
                        section_start,
                        heading.start,
                        Language::Markdown,
                    ));
                }
            }
            section_start = heading.start;
            current_level = heading.level;
            continue;
        }

        if heading.level <= current_level {
            let chunk = &content[section_start..heading.start];
            if !chunk.trim().is_empty() {
                blocks.push(create_block(
                    chunk,
                    BlockKind::Section,
                    content,
                    section_start,
                    heading.start,
                    Language::Markdown,
                ));
            }
            section_start = heading.start;
            current_level = heading.level;
        }
    }

    if section_start < content.len() {
        let chunk = &content[section_start..];
        if !chunk.trim().is_empty() {
            blocks.push(create_block(
                chunk,
                if current_level == 0 {
                    BlockKind::Preamble
                } else {
                    BlockKind::Section
                },
                content,
                section_start,
                content.len(),
                Language::Markdown,
            ));
        }
    }

    Ok(blocks)
}

pub(crate) fn fallback_split_blocks(
    content: &str,
    mode: FallbackMode,
    lang: Language,
) -> Vec<Block> {
    split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let kind = classify_fallback_chunk(chunk, mode, is_gap);
        create_fallback_block(content, chunk, kind, start, end, lang)
    })
}

fn classify_fallback_chunk(chunk: &str, mode: FallbackMode, is_gap: bool) -> BlockKind {
    if is_gap || chunk.trim().is_empty() {
        return BlockKind::Gap;
    }

    match mode {
        FallbackMode::Code => classify_code_paragraph(chunk),
        FallbackMode::Text => BlockKind::Paragraph,
    }
}

fn classify_code_paragraph(chunk: &str) -> BlockKind {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return BlockKind::Gap;
    }

    if code_comments::chunk_is_hash_or_c_style_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn create_fallback_block(
    full_source: &str,
    chunk: &str,
    kind: BlockKind,
    start: usize,
    end: usize,
    lang: Language,
) -> Block {
    let (start_line, end_line) = byte_range_to_lines(full_source, start, end);
    Block {
        hash: TreeHash::from_content(chunk),
        content: chunk.to_string(),
        kind,
        tags: Vec::new(),
        complexity: complexity::calculate(chunk, lang),
        start_line,
        end_line,
    }
}

fn split_paragraphs(content: &str, lang: Language) -> Vec<Block> {
    split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let kind = if is_gap {
            BlockKind::Gap
        } else {
            BlockKind::Paragraph
        };
        create_block(chunk, kind, content, start, end, lang)
    })
}

fn split_go(content: &str) -> Vec<Block> {
    split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let kind = if is_gap {
            BlockKind::Gap
        } else {
            classify_go_chunk(chunk)
        };
        create_block(chunk, kind, content, start, end, Language::Go)
    })
}

fn split_cpp(content: &str) -> Vec<Block> {
    split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let kind = if is_gap {
            BlockKind::Gap
        } else {
            classify_cpp_chunk(chunk)
        };
        create_block(chunk, kind, content, start, end, Language::Cpp)
    })
}

#[derive(Debug, Clone, Copy)]
struct NixBoundary {
    end: usize,
    kind: BlockKind,
}

fn split_nix(content: &str) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_nix::LANGUAGE.into())
        .context("Failed to load nix grammar")?;

    let tree = parser.parse(content, None).context("Failed to parse nix")?;
    let root = tree.root_node();

    let Some(expression) = root.child_by_field_name("expression") else {
        return Ok(split_by_paragraph_breaks(
            content,
            |chunk, start, end, is_gap| {
                let kind = if is_gap {
                    BlockKind::Gap
                } else if is_nix_comment_chunk(chunk) {
                    BlockKind::Comment
                } else {
                    BlockKind::Code
                };
                create_block(chunk, kind, content, start, end, Language::Nix)
            },
        ));
    };

    let mut boundaries = collect_nix_boundaries(expression);
    if let Some(last_boundary) = boundaries.last_mut()
        && content[last_boundary.end..].trim().is_empty()
    {
        last_boundary.end = content.len();
    }

    if boundaries.is_empty() {
        return Ok(vec![create_block(
            content,
            classify_nix_node_kind(expression.kind()),
            content,
            0,
            content.len(),
            Language::Nix,
        )]);
    }

    let mut blocks = Vec::new();
    let mut last_end = 0;
    for boundary in boundaries {
        if boundary.end <= last_end {
            continue;
        }

        let chunk = &content[last_end..boundary.end];
        let kind = if chunk.trim().is_empty() {
            BlockKind::Gap
        } else {
            boundary.kind
        };
        blocks.push(create_block(
            chunk,
            kind,
            content,
            last_end,
            boundary.end,
            Language::Nix,
        ));
        last_end = boundary.end;
    }

    if last_end < content.len() {
        let chunk = &content[last_end..];
        let kind = if chunk.trim().is_empty() {
            BlockKind::Gap
        } else if is_nix_comment_chunk(chunk) {
            BlockKind::Comment
        } else {
            BlockKind::Code
        };
        blocks.push(create_block(
            chunk,
            kind,
            content,
            last_end,
            content.len(),
            Language::Nix,
        ));
    }

    Ok(blocks)
}

fn collect_nix_boundaries(node: tree_sitter::Node<'_>) -> Vec<NixBoundary> {
    match node.kind() {
        "function_expression" => collect_nix_function_boundaries(node),
        "let_expression" => collect_nix_let_boundaries(node),
        "with_expression" | "assert_expression" => collect_nix_prefix_and_body_boundaries(node),
        "attrset_expression" | "let_attrset_expression" | "rec_attrset_expression" => {
            collect_nix_attrset_boundaries(node)
        }
        _ => vec![NixBoundary {
            end: node.end_byte(),
            kind: classify_nix_node_kind(node.kind()),
        }],
    }
}

fn collect_nix_function_boundaries(node: tree_sitter::Node<'_>) -> Vec<NixBoundary> {
    let Some(body) = node.child_by_field_name("body") else {
        return vec![NixBoundary {
            end: node.end_byte(),
            kind: BlockKind::Function,
        }];
    };

    let mut boundaries = Vec::with_capacity(2);
    if body.start_byte() > node.start_byte() {
        boundaries.push(NixBoundary {
            end: body.start_byte(),
            kind: BlockKind::FunctionSignature,
        });
    }
    boundaries.extend(collect_nix_boundaries(body));
    boundaries
}

fn collect_nix_let_boundaries(node: tree_sitter::Node<'_>) -> Vec<NixBoundary> {
    let mut boundaries = Vec::new();

    if let Some(binding_set) = first_child_of_kind(node, "binding_set") {
        boundaries.extend(collect_nix_binding_boundaries(
            binding_set,
            binding_set.end_byte(),
        ));
    }

    if let Some(body) = node.child_by_field_name("body") {
        boundaries.extend(collect_nix_boundaries(body));
    }

    if boundaries.is_empty() {
        boundaries.push(NixBoundary {
            end: node.end_byte(),
            kind: BlockKind::Code,
        });
    }

    boundaries
}

fn collect_nix_prefix_and_body_boundaries(node: tree_sitter::Node<'_>) -> Vec<NixBoundary> {
    let Some(body) = node.child_by_field_name("body") else {
        return vec![NixBoundary {
            end: node.end_byte(),
            kind: BlockKind::Code,
        }];
    };

    let mut boundaries = Vec::with_capacity(2);
    if body.start_byte() > node.start_byte() {
        boundaries.push(NixBoundary {
            end: body.start_byte(),
            kind: BlockKind::Code,
        });
    }
    boundaries.extend(collect_nix_boundaries(body));
    boundaries
}

fn collect_nix_attrset_boundaries(node: tree_sitter::Node<'_>) -> Vec<NixBoundary> {
    let Some(binding_set) = first_child_of_kind(node, "binding_set") else {
        return vec![NixBoundary {
            end: node.end_byte(),
            kind: BlockKind::Code,
        }];
    };

    collect_nix_binding_boundaries(binding_set, node.end_byte())
}

fn collect_nix_binding_boundaries(
    binding_set: tree_sitter::Node<'_>,
    terminal_end: usize,
) -> Vec<NixBoundary> {
    let mut cursor = binding_set.walk();
    let items: Vec<_> = binding_set
        .children(&mut cursor)
        .filter(|child| matches!(child.kind(), "binding" | "inherit" | "inherit_from"))
        .collect();

    if items.is_empty() {
        return Vec::new();
    }

    items
        .iter()
        .enumerate()
        .map(|(index, item)| NixBoundary {
            end: if index + 1 == items.len() {
                terminal_end
            } else {
                item.end_byte()
            },
            kind: classify_nix_node_kind(item.kind()),
        })
        .collect()
}

fn first_child_of_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn classify_nix_node_kind(kind: &str) -> BlockKind {
    match kind {
        "binding" | "variable_expression" => BlockKind::Variable,
        "inherit" | "inherit_from" => BlockKind::Import,
        "function_expression" => BlockKind::Function,
        "comment" => BlockKind::Comment,
        _ => BlockKind::Code,
    }
}

fn is_nix_comment_chunk(chunk: &str) -> bool {
    let trimmed = chunk.trim_start();
    trimmed.starts_with('#') || trimmed.starts_with("/*")
}

fn classify_go_chunk(chunk: &str) -> BlockKind {
    let mut saw_non_empty = false;
    let mut first_code_line = None;
    for line in chunk.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        saw_non_empty = true;
        if is_comment_line(trimmed) {
            continue;
        }
        first_code_line = Some(trimmed);
        break;
    }

    if !saw_non_empty {
        return BlockKind::Gap;
    }

    let Some(line) = first_code_line else {
        return BlockKind::Comment;
    };

    if line.starts_with("package ") {
        return BlockKind::Module;
    }
    if line.starts_with("import ") {
        return BlockKind::Import;
    }
    if line.starts_with("type ") {
        if line.contains("interface") {
            return BlockKind::Interface;
        }
        return BlockKind::Struct;
    }
    if let Some(rest) = line.strip_prefix("func ") {
        if rest.trim_start().starts_with('(') {
            return BlockKind::Method;
        }
        return BlockKind::Function;
    }
    if line.starts_with("const ") {
        return BlockKind::Const;
    }
    if line.starts_with("var ") {
        return BlockKind::Variable;
    }

    BlockKind::Code
}

fn classify_cpp_chunk(chunk: &str) -> BlockKind {
    let mut saw_non_empty = false;
    let mut first_code_line = None;
    for line in chunk.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        saw_non_empty = true;
        if is_comment_line(trimmed) {
            continue;
        }
        first_code_line = Some(trimmed);
        break;
    }

    if !saw_non_empty {
        return BlockKind::Gap;
    }

    let Some(line) = first_code_line else {
        return BlockKind::Comment;
    };

    if line.starts_with("#include ") || line.starts_with("import ") {
        return BlockKind::Import;
    }
    if line.starts_with("namespace ") {
        return BlockKind::Module;
    }
    if line.starts_with("class ") {
        return BlockKind::Class;
    }
    if line.starts_with("struct ") {
        return BlockKind::Struct;
    }
    if line.starts_with("enum ") {
        return BlockKind::Enum;
    }
    if line.starts_with("constexpr ") || line.starts_with("const ") {
        return BlockKind::Const;
    }
    if looks_like_cpp_function(chunk, line) {
        return BlockKind::Function;
    }

    BlockKind::Code
}

fn looks_like_cpp_function(chunk: &str, signature_line: &str) -> bool {
    if !signature_line.contains('(') || !signature_line.contains(')') {
        return false;
    }

    let disallowed = [
        "if ",
        "for ",
        "while ",
        "switch ",
        "return ",
        "catch ",
        "static_assert",
    ];
    if disallowed
        .iter()
        .any(|prefix| signature_line.starts_with(prefix))
    {
        return false;
    }

    chunk.contains('{')
}

fn is_comment_line(trimmed_line: &str) -> bool {
    code_comments::line_is_c_style_comment(trimmed_line)
}

#[derive(Debug, Clone)]
struct MarkdownHeading {
    start: usize,
    level: u8,
}

fn collect_markdown_headings(
    node: tree_sitter::Node<'_>,
    content: &str,
    headings: &mut Vec<MarkdownHeading>,
) {
    if let Some(level) = markdown_heading_level(node.kind(), node.start_byte(), content) {
        headings.push(MarkdownHeading {
            start: node.start_byte(),
            level,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_markdown_headings(child, content, headings);
    }
}

fn markdown_heading_level(kind: &str, start: usize, content: &str) -> Option<u8> {
    match kind {
        "atx_heading" => {
            let line = content.get(start..)?.lines().next()?;
            let level = line.chars().take_while(|ch| *ch == '#').count();
            if level > 0 {
                u8::try_from(level.min(6)).ok()
            } else {
                None
            }
        }
        "setext_heading" => {
            let line = content.get(start..)?.lines().next()?;
            if line.chars().all(|ch| ch == '=') {
                Some(1)
            } else if line.chars().all(|ch| ch == '-') {
                Some(2)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn map_kind_for_node(lang: Language, node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match lang {
        Language::Swift => swift::map_kind(node, content),
        _ => map_kind(lang, node.kind()),
    }
}

fn map_kind(lang: Language, kind: &str) -> BlockKind {
    match lang {
        Language::Rust => match kind {
            "function_item" => BlockKind::Function,
            "struct_item" | "union_item" => BlockKind::Struct,
            "enum_item" => BlockKind::Enum,
            "impl_item" => BlockKind::Impl,
            "trait_item" => BlockKind::Interface,
            "mod_item" | "foreign_mod_item" => BlockKind::Module,
            "use_declaration" | "extern_crate_declaration" => BlockKind::Import,
            "const_item" => BlockKind::Const,
            "static_item" => BlockKind::Static,
            "macro_invocation" | "macro_definition" => BlockKind::Macro,
            "type_item" | "associated_type" => BlockKind::Type,
            "function_signature_item" => BlockKind::FunctionSignature,
            _ => BlockKind::Code,
        },
        Language::Python => match kind {
            "function_definition" => BlockKind::Function,
            "class_definition" => BlockKind::Class,
            "import_statement" | "import_from_statement" => BlockKind::Import,
            "decorated_definition" => BlockKind::Decorator,
            _ => BlockKind::Code,
        },
        Language::Java => match kind {
            "package_declaration" | "module_declaration" => BlockKind::Module,
            "import_declaration" => BlockKind::Import,
            "class_declaration" => BlockKind::Class,
            "interface_declaration" => BlockKind::Interface,
            "enum_declaration" => BlockKind::Enum,
            "record_declaration" => BlockKind::Struct,
            "annotation_type_declaration" => BlockKind::Type,
            "field_declaration" => BlockKind::Variable,
            "constant_declaration" => BlockKind::Const,
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration" => BlockKind::Method,
            _ => BlockKind::Code,
        },
        Language::JavaScript | Language::TypeScript => match kind {
            "function_declaration" => BlockKind::Function,
            "class_declaration" => BlockKind::Class,
            "import_statement" => BlockKind::Import,
            "export_statement" => BlockKind::Export,
            "variable_declaration" => BlockKind::Variable,
            "lexical_declaration" => BlockKind::Variable,
            _ => BlockKind::Code,
        },
        Language::Shell => match kind {
            "function_definition" => BlockKind::Function,
            "command" => BlockKind::Command,
            _ => BlockKind::Code,
        },
        _ => BlockKind::Code,
    }
}

fn collect_swift_type_items(
    type_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Vec<Block> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Vec::new();
    };
    if !swift::body_is_non_trivial(body, content) {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    for member in swift::collect_type_member_spans(body, content) {
        let node_content = &content[member.start_byte..member.end_byte];
        blocks.push(create_block(
            node_content,
            member.kind,
            content,
            member.start_byte,
            member.end_byte,
            lang,
        ));
    }

    blocks
}

fn collect_rust_impl_items(
    impl_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Vec<Block> {
    rust::collect_impl_member_spans(impl_node)
        .into_iter()
        .map(|member| {
            create_block(
                &content[member.start_byte..member.end_byte],
                member.kind,
                content,
                member.start_byte,
                member.end_byte,
                lang,
            )
        })
        .collect()
}

fn collect_java_type_items(
    type_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Vec<Block> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Vec::new();
    };

    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|child| {
            let kind = map_kind(lang, child.kind());
            (!matches!(kind, BlockKind::Code)).then(|| {
                create_block(
                    &content[child.start_byte()..child.end_byte()],
                    kind,
                    content,
                    child.start_byte(),
                    child.end_byte(),
                    lang,
                )
            })
        })
        .collect()
}

fn create_block(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Block {
    let hash = TreeHash::from_content(text);
    let complexity = complexity::calculate(text, lang);

    // Line mapping (byte -> line index)
    // Reusing the logic from previous implementation
    let (start_line, end_line) = byte_range_to_lines(full_source, start_byte, end_byte);

    Block {
        hash,
        content: text.to_string(),
        kind,
        tags: Vec::new(),
        complexity,
        start_line,
        end_line,
    }
}

fn collect_test_ranges(
    lang: Language,
    tree: &tree_sitter::Tree,
    source: &str,
) -> Result<Vec<ByteSpan>> {
    let mut ranges: Vec<ByteSpan> = Vec::new();
    match lang {
        Language::Rust => {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&RUST_ATTR_QUERY, tree.root_node(), source.as_bytes());
            while let Some(match_) = matches.next() {
                for capture in match_.captures {
                    let name = &RUST_ATTR_QUERY.capture_names()[capture.index as usize];
                    if *name != "attr" {
                        continue;
                    }
                    let attr_text = capture.node.utf8_text(source.as_bytes())?;
                    if attr_text.contains("#[test]")
                        && let Some(function_item) =
                            next_named_sibling_of_kind(capture.node, "function_item")
                    {
                        ranges.push(ByteSpan::new(
                            function_item.start_byte(),
                            function_item.end_byte(),
                        ));
                    }
                    if attr_text.contains("cfg")
                        && attr_text.contains("test")
                        && let Some(mod_item) = next_named_sibling_of_kind(capture.node, "mod_item")
                    {
                        ranges.push(ByteSpan::new(mod_item.start_byte(), mod_item.end_byte()));
                    }
                }
            }
        }
        Language::Swift => {
            let mut cursor = QueryCursor::new();
            let mut matches =
                cursor.matches(&SWIFT_ATTR_QUERY, tree.root_node(), source.as_bytes());
            while let Some(match_) = matches.next() {
                for capture in match_.captures {
                    let name = &SWIFT_ATTR_QUERY.capture_names()[capture.index as usize];
                    if *name != "attr" {
                        continue;
                    }
                    let attr_text = capture.node.utf8_text(source.as_bytes())?;
                    if (attr_text.contains("Test") || attr_text.contains("Suite"))
                        && let Some(target) = ancestor_or_self_of_kinds(
                            capture.node,
                            &[
                                "function_declaration",
                                "class_declaration",
                                "protocol_declaration",
                            ],
                        )
                        .or_else(|| {
                            next_named_sibling_of_kinds(
                                capture.node,
                                &[
                                    "function_declaration",
                                    "class_declaration",
                                    "protocol_declaration",
                                ],
                            )
                        })
                    {
                        ranges.push(ByteSpan::new(target.start_byte(), target.end_byte()));
                    }
                }
            }
            collect_swift_xctest_ranges(tree.root_node(), source, &mut ranges)?;
        }
        Language::Python => {
            collect_python_test_ranges(&PYTHON_DECORATED_TEST_QUERY, tree, source, &mut ranges)?;
            collect_python_test_ranges(&PYTHON_FUNCTION_TEST_QUERY, tree, source, &mut ranges)?;
        }
        Language::JavaScript => {
            collect_js_test_ranges(&JAVASCRIPT_ARROW_TEST_QUERY, tree, source, &mut ranges)?;
            collect_js_test_ranges(&JAVASCRIPT_MEMBER_TEST_QUERY, tree, source, &mut ranges)?;
        }
        Language::TypeScript => {
            collect_js_test_ranges(&TYPESCRIPT_ARROW_TEST_QUERY, tree, source, &mut ranges)?;
            collect_js_test_ranges(&TYPESCRIPT_MEMBER_TEST_QUERY, tree, source, &mut ranges)?;
        }
        Language::Shell => {
            collect_shell_test_ranges(&SHELL_FUNCTION_TEST_QUERY, tree, source, &mut ranges)?;
        }
        _ => {}
    }

    Ok(ranges)
}

fn collect_test_line_spans(
    lang: Language,
    tree: &tree_sitter::Tree,
    source: &str,
) -> Result<Vec<LineSpan>> {
    collect_test_ranges(lang, tree, source).map(|ranges| {
        ranges
            .into_iter()
            .map(|range| {
                let (start_line, end_line) =
                    byte_range_to_lines(source, range.start_byte, range.end_byte);
                LineSpan::new(start_line, end_line)
            })
            .collect()
    })
}

fn apply_test_tags(blocks: &mut [Block], test_spans: &[LineSpan]) {
    for block in blocks {
        if test_spans
            .iter()
            .any(|test_span| block.line_span().overlaps(test_span))
            && !block.has_tag("test")
        {
            block.tags.push("test".to_string());
        }
    }
}

fn next_named_sibling_of_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut current = node;
    while let Some(next) = current.next_named_sibling() {
        if next.kind() == kind {
            return Some(next);
        }
        current = next;
    }
    None
}

fn next_named_sibling_of_kinds<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    let mut current = node;
    while let Some(next) = current.next_named_sibling() {
        if kinds.iter().any(|kind| *kind == next.kind()) {
            return Some(next);
        }
        current = next;
    }
    None
}

fn ancestor_or_self_of_kinds<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if kinds.iter().any(|kind| *kind == candidate.kind()) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

fn collect_swift_xctest_ranges(
    node: tree_sitter::Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    if node.kind() == "class_declaration" {
        let header_end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |body| body.start_byte());
        let header = &source[node.start_byte()..header_end];
        if header.contains("XCTestCase") {
            ranges.push(ByteSpan::new(node.start_byte(), node.end_byte()));
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_swift_xctest_ranges(child, source, ranges)?;
    }

    Ok(())
}

fn collect_python_test_ranges(
    query: &Query,
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut func_range = None;
        let mut decorator_text = None;
        for capture in match_.captures {
            let cap_name = &query.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "func" => func_range = Some((capture.node.start_byte(), capture.node.end_byte())),
                "decor" => {
                    decorator_text = Some(capture.node.utf8_text(source.as_bytes())?.to_string());
                }
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, func_range)
            && name.starts_with("test_")
        {
            ranges.push(ByteSpan::new(range.0, range.1));
            continue;
        }
        if let (Some(decor_text), Some(range)) = (decorator_text, func_range)
            && decor_text.contains("test_")
        {
            ranges.push(ByteSpan::new(range.0, range.1));
        }
    }
    Ok(())
}

fn collect_js_test_ranges(
    query: &Query,
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut call_range = None;
        for capture in match_.captures {
            let cap_name = &query.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "call" => call_range = Some((capture.node.start_byte(), capture.node.end_byte())),
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, call_range)
            && matches!(name.as_str(), "describe" | "it" | "test")
        {
            ranges.push(ByteSpan::new(range.0, range.1));
        }
    }
    Ok(())
}

fn collect_shell_test_ranges(
    query: &Query,
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut func_range = None;
        for capture in match_.captures {
            let cap_name = &query.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "func" => func_range = Some((capture.node.start_byte(), capture.node.end_byte())),
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, func_range)
            && name.starts_with("test_")
        {
            ranges.push(ByteSpan::new(range.0, range.1));
        }
    }
    Ok(())
}

fn byte_range_to_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let pre = &source[..start];
    let start_line = pre.lines().count();
    let start_line = if start > 0 && pre.ends_with('\n') {
        start_line
    } else {
        start_line.saturating_sub(1)
    };

    let mid = &source[start..end];
    let new_lines = mid.chars().filter(|&c| c == '\n').count();
    let end_line = start_line + new_lines + if mid.ends_with('\n') { 0 } else { 1 };

    (start_line, end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_result(content: &str, language: Language) -> BlockSplitResult {
        split(content, language)
    }

    fn split_blocks(content: &str, language: Language) -> Vec<Block> {
        split_result(content, language).blocks
    }

    #[test]
    fn test_rust_test_detection() {
        let content = "#[test]
fn test_foo() {}
";
        let result = split_result(content, Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        assert!(!result.blocks.is_empty());
        let test_block = result
            .blocks
            .iter()
            .find(|b| b.content.contains("fn test_foo"));
        assert!(test_block.is_some());
        assert!(test_block.unwrap().tags.contains(&"test".to_string()));
    }

    #[test]
    fn test_rust_cfg_test_module_tagging() {
        let content = "#[cfg(test)]
mod tests {
    #[test]
    fn test_inner() {}
}
";
        let result = split_result(content, Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        assert!(!result.blocks.is_empty());
        let module_block = result
            .blocks
            .iter()
            .find(|b| b.content.contains("mod tests"));
        assert!(module_block.is_some());
        assert!(module_block.unwrap().tags.contains(&"test".to_string()));
    }

    #[test]
    fn test_swift_structural_blocks_and_test_detection() {
        let content = "import Foundation\nimport Testing\n\n\
typealias Payload = [UInt8]\n\n\
actor Worker {\n    func run() {}\n}\n\n\
extension Worker {\n    func stop() {}\n    var body: some View { Text(\"hi\") }\n}\n\n\
@Test\nfunc test_worker() async throws {}\n\n\
final class LegacyWorkerTests: XCTestCase {\n    func testLegacyPath() {}\n}\n";
        let result = split_result(content, Language::Swift);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Type));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Impl));
        assert!(
            blocks
                .iter()
                .filter(|block| block.tags.iter().any(|tag| tag == "test"))
                .count()
                >= 2
        );
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Paragraph))
        );
    }

    #[test]
    fn test_swift_package_manifest_is_structural() {
        let content = "import PackageDescription\n\n\
let package = Package(\n    name: \"Demo\",\n    products: [\n        .library(name: \"Demo\", targets: [\"Demo\"]),\n    ],\n    targets: [\n        .target(name: \"Demo\"),\n        .testTarget(name: \"DemoTests\", dependencies: [\"Demo\"]),\n    ]\n)\n";
        let result = split_result(content, Language::Swift);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let kinds: Vec<_> = result.blocks.iter().map(|block| block.kind).collect();
        assert!(kinds.contains(&BlockKind::Import));
        assert!(kinds.contains(&BlockKind::Const) || kinds.contains(&BlockKind::Variable));
    }

    #[test]
    fn test_java_structural_blocks() {
        let content = "package demo;\n\nimport java.util.List;\n\npublic class Worker {\n    private final int scale;\n\n    public Worker(int scale) {\n        this.scale = scale;\n    }\n\n    public int process(List<Integer> values) {\n        int total = 0;\n        for (int value : values) {\n            if (value > 0) {\n                total += value * scale;\n            }\n        }\n        return total;\n    }\n}\n";
        let result = split_result(content, Language::Java);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Module));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Variable));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Method));
        assert!(
            !blocks
                .iter()
                .any(|block| block.kind == BlockKind::Paragraph)
        );
    }

    fn assert_paragraph_split(language: Language) {
        let content = "Para 1.\n\nPara 2.";
        let result = split_result(content, language);
        assert_eq!(result.strategy, BlockSplitStrategy::Textual);
        let blocks = result.blocks;
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(blocks[1].kind, BlockKind::Gap);
        assert_eq!(blocks[2].kind, BlockKind::Paragraph);
        assert_eq!(blocks[0].content, "Para 1.");
        assert_eq!(blocks[1].content, "\n\n");
        assert_eq!(blocks[2].content, "Para 2.");
        let merged: String = blocks.into_iter().map(|block| block.content).collect();
        assert_eq!(merged, content);
    }

    fn assert_block_hashes_match(blocks: &[Block]) {
        for block in blocks {
            let expected_hash = crate::hashing::TreeHash::from_content(&block.content);
            assert_eq!(
                block.hash, expected_hash,
                "Hash mismatch for block kind {:?}:\nContent:\n{:?}",
                block.kind, block.content
            );
        }
    }

    #[test]
    fn test_split_markdown_headers() {
        let result = split_result(
            "# Section 1\nText.\n# Section 2\nMore text.",
            Language::Markdown,
        );
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Section);
        assert_eq!(blocks[0].content, "# Section 1\nText.\n");
        assert_eq!(blocks[1].kind, BlockKind::Section);
        assert_eq!(blocks[1].content, "# Section 2\nMore text.");
    }

    #[test]
    fn test_split_markdown_hierarchy() {
        let blocks = split_blocks("# Root\n## Sub\n### SubSub\n# Root 2", Language::Markdown);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].content, "# Root\n## Sub\n### SubSub\n");
        assert_eq!(blocks[1].content, "# Root 2");
    }

    #[test]
    fn test_split_text_paragraphs() {
        assert_paragraph_split(Language::Text);
    }

    #[test]
    fn test_split_toml_paragraphs() {
        assert_paragraph_split(Language::Toml);
    }

    #[test]
    fn test_split_nix_attrset_bindings() {
        let content = "{\n  foo = \"bar\";\n  inherit pkgs;\n}\n";
        let result = split_result(content, Language::Nix);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Variable);
        assert!(blocks[0].content.contains("foo = \"bar\";"));
        assert_eq!(blocks[1].kind, BlockKind::Import);
        assert!(blocks[1].content.contains("inherit pkgs;"));
        assert_eq!(
            blocks
                .into_iter()
                .map(|block| block.content)
                .collect::<String>(),
            content
        );
    }

    #[test]
    fn test_split_nix_function_let_and_body_attrset() {
        let content =
            "{ pkgs }:\nlet\n  foo = \"bar\";\n  inherit pkgs;\nin {\n  inherit foo;\n}\n";
        let result = split_result(content, Language::Nix);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        let kinds: Vec<_> = blocks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::Variable,
                BlockKind::Import,
                BlockKind::Import,
            ]
        );
        assert!(blocks[0].content.contains("{ pkgs }:"));
        assert!(blocks[1].content.contains("foo = \"bar\";"));
        assert!(blocks[2].content.contains("inherit pkgs;"));
        assert!(blocks[3].content.contains("inherit foo;"));
        assert_eq!(
            blocks
                .into_iter()
                .map(|block| block.content)
                .collect::<String>(),
            content
        );
    }

    #[test]
    fn test_split_nix_falls_back_to_single_code_block_for_simple_if_expression() {
        let content = "if enabled then package else fallback\n";
        let result = split_result(content, Language::Nix);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_nix_comment_only_file_is_comment_block() {
        let content = "# comment only\n# still comment\n";
        let blocks = split_blocks(content, Language::Nix);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Comment);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_just_uses_code_fallback() {
        let content = "build:\n\t echo ok\n\ntest:\n\t echo ok";
        let result = split_result(content, Language::Just);
        assert_eq!(result.strategy, BlockSplitStrategy::Heuristic);

        let blocks = result.blocks;
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(blocks[1].kind, BlockKind::Gap);
        assert_eq!(blocks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(blocks[0].content, "build:\n\t echo ok");
        assert_eq!(blocks[1].content, "\n\n");
        assert_eq!(blocks[2].content, "test:\n\t echo ok");
        let merged: String = blocks.into_iter().map(|block| block.content).collect();
        assert_eq!(merged, content);
    }

    #[test]
    fn test_split_rust_simple() {
        let blocks = split_blocks("fn foo() {}\n\nstruct Bar;", Language::Rust);
        assert!(!blocks.is_empty());
    }

    #[test]
    fn test_split_go_simple_maps_import_struct_and_function() {
        let result = split_result(
            "package main\n\nimport \"fmt\"\n\ntype Worker struct{}\n\nfunc run() {\n    fmt.Println(\"ok\")\n}\n",
            Language::Go,
        );
        assert_eq!(result.strategy, BlockSplitStrategy::Heuristic);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Struct));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Function));
    }

    #[test]
    fn test_split_cpp_simple_maps_import_class_and_function() {
        let result = split_result(
            "#include <vector>\n\nclass Worker {\npublic:\n    int value = 1;\n};\n\nint run() {\n    return 1;\n}\n",
            Language::Cpp,
        );
        assert_eq!(result.strategy, BlockSplitStrategy::Heuristic);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Function));
    }

    #[test]
    fn test_block_hashes_match_content_rust() {
        let blocks = split_blocks("use std::fmt;\n\nfn foo() {}\n", Language::Rust);
        assert!(!blocks.is_empty());
        assert_block_hashes_match(&blocks);
        assert!(!blocks.iter().any(|block| block.kind == BlockKind::Gap));
    }

    #[test]
    fn test_block_hashes_match_content_markdown() {
        let content = "# Title\nParagraph text.\n";
        let blocks = split_blocks(content, Language::Markdown);
        assert_eq!(blocks.len(), 1);
        assert_block_hashes_match(&blocks);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_rust_impl_methods() {
        let blocks = split_blocks(
            "struct Foo;\n\nimpl Foo {\n    fn read_heavy(&self) {}\n    const MAX: usize = 1;\n}\n",
            Language::Rust,
        );
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Impl));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Method));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Const));
    }

    #[test]
    fn test_split_rust_top_level_static_maps_to_static_kind() {
        let blocks = split_blocks("static MAX: usize = 1;\n", Language::Rust);
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Static));
    }

    #[test]
    fn test_markdown_discards_whitespace_only_preamble() {
        let blocks = split_blocks("\n\n# Title\nBody", Language::Markdown);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "# Title\nBody");
    }

    #[test]
    fn test_split_unsupported_code_language_falls_back_to_code_blocks() {
        let content = "(message \"hello\")\n\n(defun greet ()\n  (message \"hi\"))\n";
        let result = split_result(content, Language::Elisp);
        assert_eq!(result.strategy, BlockSplitStrategy::UnsupportedCode);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason.contains("unsupported language"))
        );
        let blocks = result.blocks;
        assert!(!blocks.is_empty());
        assert!(
            blocks
                .iter()
                .any(|block| block.kind == BlockKind::CodeParagraph)
        );
    }

    #[test]
    fn test_split_whitespace_only_code_returns_gap_block() {
        let content = "\n\n    \n";
        let result = split_result(content, Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::FallbackCode);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason.contains("returned no blocks"))
        );
        let blocks = result.blocks;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Gap);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_includes_optimization_pipeline() {
        let result = split_result("use std::fmt;\n\nuse std::io;\n", Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.into_review_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Imports);
        assert_eq!(blocks[0].content, "use std::fmt;\nuse std::io;");
    }
}
