use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
use crate::code_comments;
use crate::complexity;
use crate::optimizer;
use crate::review_units::MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES;
use crate::text_split::paragraph_break_regex;
use crate::tree_sitter_support::{
    classify_kotlin_class_kind, classify_kotlin_property_kind, elisp_list_head_symbol,
    find_named_descendant, first_child_of_kind, kotlin_type_body, markdown_heading_level,
    ruby_assignment_targets_constant, ruby_call_method_name,
};
use crate::{languages, rust, swift, toml_blocks};
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
static CSHARP_ATTR_METHOD_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_c_sharp::LANGUAGE.into(),
        "(method_declaration (attribute_list) @attr name: (identifier) @name) @method",
        "csharp attribute method test query",
    )
});
static CSHARP_METHOD_TEST_QUERY: LazyLock<Query> = LazyLock::new(|| {
    compile_query(
        &tree_sitter_c_sharp::LANGUAGE.into(),
        "(method_declaration name: (identifier) @name) @method",
        "csharp method test query",
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

    /// Convert raw split blocks into optimized review blocks using the exact
    /// source previously passed to `split`.
    pub fn into_review_blocks(self, source: &str) -> Vec<Block> {
        if self.blocks.is_empty() {
            Vec::new()
        } else {
            optimizer::optimize(self.blocks, source)
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
        Language::Toml => attempt_split(content, lang, split_toml(content), FallbackMode::Code),
        Language::Nix => attempt_split(content, lang, split_nix(content), FallbackMode::Code),
        _ if languages::registration(lang).is_some() => attempt_split(
            content,
            lang,
            split_tree_sitter(content, lang),
            FallbackMode::Code,
        ),
        _ if lang.uses_text_fallback() || matches!(lang, Language::Unknown) => complete_split(
            split_paragraphs(content, lang),
            BlockSplitStrategy::Textual,
            Vec::new(),
        ),
        Language::Rust
        | Language::Swift
        | Language::Elisp
        | Language::JavaScript
        | Language::TypeScript
        | Language::Java
        | Language::Kotlin
        | Language::CSharp
        | Language::Python
        | Language::Ruby
        | Language::Php
        | Language::Shell
        | Language::C => attempt_split(
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
    let registration = languages::registration(lang);
    let mut parser = Parser::new();
    let language = tree_sitter_language_for(lang, content)
        .ok_or_else(|| anyhow::anyhow!("No tree-sitter grammar configured for {lang:?}"))?;
    parser.set_language(&language)?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse source with tree-sitter")?;
    let root = tree.root_node();
    let test_spans = collect_test_line_spans(lang, &tree, content)?;

    if let Some(registration) = registration
        && let Some(custom_splitter) = registration.top_level.custom_splitter
    {
        let mut blocks = custom_splitter(root, content, lang)?;
        apply_test_tags(&mut blocks, &test_spans);
        return Ok(blocks);
    }

    let mut blocks = Vec::new();

    let mut cursor = root.walk();
    let mut last_end_byte = 0;

    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    for child in root.children(&mut cursor) {
        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        let ts_kind = child.kind();

        let is_attribute = if let Some(registration) = registration {
            (registration.top_level.is_attribute_node)(ts_kind)
        } else {
            match lang {
                Language::Rust => {
                    ts_kind == "attribute_item"
                        || ts_kind == "line_comment"
                        || ts_kind == "block_comment"
                }
                Language::Python => ts_kind == "decorator",
                Language::Swift => swift::is_attribute_node(ts_kind),
                Language::CSharp => ts_kind == "attribute_list",
                Language::Php => ts_kind == "php_tag",
                _ => false,
            }
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
                        )?);
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
                    )?);
                }
            }
            start_byte
        };

        let node_content = &content[block_start..end_byte];
        blocks.push(create_block_with_complexity(
            node_content,
            map_kind_for_node(lang, child, content),
            content,
            block_start,
            end_byte,
            complexity::calculate_from_node(child, lang, content),
        )?);

        if let Some(registration) = registration {
            blocks.extend(
                (registration.top_level.collect_nested_blocks)(child, content, lang)
                    .into_iter()
                    .filter_map(|nested| {
                        create_block(
                            &content[nested.start_byte..nested.end_byte],
                            nested.kind,
                            content,
                            nested.start_byte,
                            nested.end_byte,
                            lang,
                        )
                        .ok()
                    }),
            );
        } else {
            if matches!(lang, Language::Rust) && matches!(ts_kind, "impl_item" | "trait_item") {
                blocks.extend(collect_rust_impl_items(child, content, lang)?);
            }
            if matches!(lang, Language::Swift)
                && matches!(ts_kind, "class_declaration" | "protocol_declaration")
            {
                blocks.extend(collect_swift_type_items(child, content, lang)?);
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
                blocks.extend(collect_java_type_items(child, content, lang)?);
            }
            if matches!(lang, Language::Kotlin)
                && matches!(ts_kind, "class_declaration" | "object_declaration")
            {
                blocks.extend(collect_kotlin_type_items(child, content, lang)?);
            }
            if matches!(lang, Language::CSharp)
                && matches!(
                    ts_kind,
                    "namespace_declaration" | "file_scoped_namespace_declaration"
                )
            {
                blocks.extend(collect_csharp_namespace_items(child, content, lang)?);
            }
            if matches!(lang, Language::CSharp)
                && matches!(
                    ts_kind,
                    "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                        | "struct_declaration"
                )
            {
                blocks.extend(collect_csharp_type_items(child, content, lang)?);
            }
            if matches!(lang, Language::Ruby) && matches!(ts_kind, "class" | "module") {
                blocks.extend(collect_ruby_scope_items(child, content, lang)?);
            }
            if matches!(lang, Language::Php)
                && matches!(
                    ts_kind,
                    "class_declaration"
                        | "interface_declaration"
                        | "trait_declaration"
                        | "enum_declaration"
                )
            {
                blocks.extend(collect_php_type_items(child, content, lang)?);
            }
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
        )?);
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
            )?);
        }
    }

    apply_test_tags(&mut blocks, &test_spans);

    Ok(blocks)
}

fn tree_sitter_language_for(lang: Language, content: &str) -> Option<TsLanguage> {
    if let Some(registration) = languages::registration(lang) {
        return Some((registration.top_level.parser_language)(content));
    }

    match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
        Language::Elisp => Some(tree_sitter_elisp::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Language::Kotlin => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
        Language::CSharp => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Ruby => Some(tree_sitter_ruby::LANGUAGE.into()),
        Language::Php => Some(php_tree_sitter_language(content)),
        Language::C => Some(tree_sitter_c::LANGUAGE.into()),
        Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

fn php_tree_sitter_language(content: &str) -> TsLanguage {
    if content.contains("<?") {
        tree_sitter_php::LANGUAGE_PHP.into()
    } else {
        tree_sitter_php::LANGUAGE_PHP_ONLY.into()
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

    if headings.is_empty() {
        return split_markdown_flat_range(content, 0, content.len());
    }

    let mut blocks = Vec::new();
    let root_sections = markdown_immediate_sections(&headings, 0, content.len());
    let first_section_start = root_sections
        .first()
        .map(|section| section.start)
        .unwrap_or(content.len());
    if first_section_start > 0 && !content[..first_section_start].trim().is_empty() {
        blocks.extend(split_markdown_flat_range(content, 0, first_section_start)?);
    }

    let force_single_root_refinement = root_sections.len() == 1
        && markdown_section_child_sections(&root_sections[0], &headings)
            .is_some_and(|children| !children.is_empty());

    for section in root_sections {
        // Most Markdown docs have one H1 wrapping the entire file. Review its
        // immediate subsections instead of presenting the whole document as one block.
        blocks.extend(split_markdown_section_for_review(
            content,
            section,
            &headings,
            force_single_root_refinement,
        )?);
    }

    Ok(blocks)
}

pub(crate) fn fallback_split_blocks(
    content: &str,
    mode: FallbackMode,
    lang: Language,
) -> Vec<Block> {
    collect_valid_paragraph_blocks(content, |chunk, start, end, is_gap| {
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

    if code_comments::chunk_is_comment_only(chunk) {
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
) -> Option<Block> {
    let mut block = Block::from_file_range(full_source, kind, ByteSpan::new(start, end)).ok()?;
    assert_eq!(
        block.content, chunk,
        "fallback splitter range must name the supplied chunk"
    );
    block.complexity = complexity::calculate(&block.content, lang);
    Some(block)
}

fn collect_valid_paragraph_blocks<F>(content: &str, mut make_block: F) -> Vec<Block>
where
    F: FnMut(&str, usize, usize, bool) -> Option<Block>,
{
    let re = paragraph_break_regex();
    let mut blocks = Vec::new();
    let mut start_offset = 0;

    for mat in re.find_iter(content) {
        let end_offset = mat.start();
        if start_offset < end_offset
            && let Some(chunk) = content.get(start_offset..end_offset)
            && !chunk.is_empty()
            && let Some(block) = make_block(chunk, start_offset, end_offset, false)
        {
            blocks.push(block);
        }

        if let Some(gap_chunk) = content.get(mat.start()..mat.end())
            && let Some(block) = make_block(gap_chunk, mat.start(), mat.end(), true)
        {
            blocks.push(block);
        }

        start_offset = mat.end();
    }

    if start_offset < content.len()
        && let Some(chunk) = content.get(start_offset..)
        && !chunk.is_empty()
        && let Some(block) = make_block(chunk, start_offset, content.len(), false)
    {
        blocks.push(block);
    }

    blocks
}

fn split_paragraphs(content: &str, lang: Language) -> Vec<Block> {
    collect_valid_paragraph_blocks(content, |chunk, start, end, is_gap| {
        let kind = if is_gap {
            BlockKind::Gap
        } else {
            BlockKind::Paragraph
        };
        create_block(chunk, kind, content, start, end, lang).ok()
    })
}

fn split_toml(content: &str) -> Result<Vec<Block>> {
    let spans = toml_blocks::split_document(content)?;
    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for span in spans {
        push_non_empty_toml_gap(&mut blocks, content, last_end, span.start)?;
        blocks.push(create_block(
            &content[span.start..span.end],
            span.kind,
            content,
            span.start,
            span.end,
            Language::Toml,
        )?);
        last_end = span.end;
    }

    push_non_empty_toml_gap(&mut blocks, content, last_end, content.len())?;
    Ok(blocks)
}

fn push_non_empty_toml_gap(
    blocks: &mut Vec<Block>,
    content: &str,
    start: usize,
    end: usize,
) -> Result<()> {
    if end <= start {
        return Ok(());
    }

    let chunk = &content[start..end];
    if chunk.is_empty() || chunk.trim().is_empty() {
        return Ok(());
    }

    let kind = if code_comments::chunk_is_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::Gap
    };

    blocks.push(create_block(
        chunk,
        kind,
        content,
        start,
        end,
        Language::Toml,
    )?);
    Ok(())
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
        return Ok(collect_valid_paragraph_blocks(
            content,
            |chunk, start, end, is_gap| {
                let kind = if is_gap {
                    BlockKind::Gap
                } else if is_nix_comment_chunk(chunk) {
                    BlockKind::Comment
                } else {
                    BlockKind::Code
                };
                create_block(chunk, kind, content, start, end, Language::Nix).ok()
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
        )?]);
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
        )?);
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
        )?);
    }

    Ok(blocks)
}

fn collect_nix_boundaries(mut node: tree_sitter::Node<'_>) -> Vec<NixBoundary> {
    let mut boundaries = Vec::new();

    loop {
        match node.kind() {
            "function_expression" => {
                let Some(body) = node.child_by_field_name("body") else {
                    boundaries.push(NixBoundary {
                        end: node.end_byte(),
                        kind: BlockKind::Function,
                    });
                    break;
                };

                if body.start_byte() > node.start_byte() {
                    boundaries.push(NixBoundary {
                        end: body.start_byte(),
                        kind: BlockKind::FunctionSignature,
                    });
                }
                node = body;
            }
            "let_expression" => {
                let boundary_count = boundaries.len();
                if let Some(binding_set) = first_child_of_kind(node, "binding_set") {
                    boundaries.extend(collect_nix_binding_boundaries(
                        binding_set,
                        binding_set.end_byte(),
                    ));
                }

                if let Some(body) = node.child_by_field_name("body") {
                    node = body;
                    continue;
                }

                if boundaries.len() == boundary_count {
                    boundaries.push(NixBoundary {
                        end: node.end_byte(),
                        kind: BlockKind::Code,
                    });
                }
                break;
            }
            "with_expression" | "assert_expression" => {
                let Some(body) = node.child_by_field_name("body") else {
                    boundaries.push(NixBoundary {
                        end: node.end_byte(),
                        kind: BlockKind::Code,
                    });
                    break;
                };

                if body.start_byte() > node.start_byte() {
                    boundaries.push(NixBoundary {
                        end: body.start_byte(),
                        kind: BlockKind::Code,
                    });
                }
                node = body;
            }
            "attrset_expression" | "let_attrset_expression" | "rec_attrset_expression" => {
                if let Some(binding_set) = first_child_of_kind(node, "binding_set") {
                    boundaries.extend(collect_nix_binding_boundaries(binding_set, node.end_byte()));
                } else {
                    boundaries.push(NixBoundary {
                        end: node.end_byte(),
                        kind: BlockKind::Code,
                    });
                }
                break;
            }
            _ => {
                boundaries.push(NixBoundary {
                    end: node.end_byte(),
                    kind: classify_nix_node_kind(node.kind()),
                });
                break;
            }
        }
    }

    boundaries
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

#[derive(Debug, Clone)]
struct MarkdownHeading {
    start: usize,
    end: usize,
    level: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarkdownSectionSpan {
    start: usize,
    end: usize,
}

fn split_markdown_section_for_review(
    content: &str,
    section: MarkdownSectionSpan,
    headings: &[MarkdownHeading],
    force_refine: bool,
) -> Result<Vec<Block>> {
    let section_content = &content[section.start..section.end];
    if section_content.trim().is_empty() {
        return Ok(Vec::new());
    }

    let line_count = markdown_span_line_count(content, section.start, section.end)?;
    let child_sections = markdown_section_child_sections(&section, headings).unwrap_or_default();
    let should_refine = force_refine
        || line_count > MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES
        || (child_sections.len() == 1
            && markdown_span_line_count(content, child_sections[0].start, child_sections[0].end)?
                > MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES);

    if !should_refine {
        return Ok(vec![create_block(
            section_content,
            BlockKind::Section,
            content,
            section.start,
            section.end,
            Language::Markdown,
        )?]);
    }

    if child_sections.is_empty() {
        let mut blocks = vec![create_block(
            section_content,
            BlockKind::Section,
            content,
            section.start,
            section.end,
            Language::Markdown,
        )?];
        let body_start = headings
            .iter()
            .find(|heading| heading.start == section.start)
            .map(|heading| heading.end)
            .unwrap_or(section.start);
        if content[body_start..section.end].trim().is_empty() {
            return Ok(blocks);
        }
        let attached_blocks = split_markdown_flat_range_attaching_heading(
            content,
            section.start,
            body_start,
            section.end,
        )?;
        if attached_blocks.len() == 1 && attached_blocks[0].content == section_content {
            return Ok(attached_blocks);
        }
        blocks.extend(attached_blocks);
        return Ok(blocks);
    }

    let mut blocks = vec![create_block(
        section_content,
        BlockKind::Section,
        content,
        section.start,
        section.end,
        Language::Markdown,
    )?];
    let root_heading = headings
        .iter()
        .find(|heading| heading.start == section.start);
    let root_content_end = child_sections[0].start;
    let root_intro_body = root_heading
        .and_then(|heading| content.get(heading.end..root_content_end))
        .unwrap_or_default();
    if !root_intro_body.trim().is_empty() {
        blocks.extend(split_markdown_section_for_review(
            content,
            MarkdownSectionSpan {
                start: section.start,
                end: root_content_end,
            },
            headings,
            markdown_span_line_count(content, section.start, root_content_end)?
                > MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES,
        )?);
    }

    for child in child_sections {
        blocks.extend(split_markdown_section_for_review(
            content, child, headings, false,
        )?);
    }

    Ok(blocks)
}

fn markdown_section_child_sections(
    section: &MarkdownSectionSpan,
    headings: &[MarkdownHeading],
) -> Option<Vec<MarkdownSectionSpan>> {
    let root_heading = headings
        .iter()
        .find(|heading| heading.start == section.start)?;
    let child_headings = headings
        .iter()
        .filter(|heading| heading.start > section.start && heading.start < section.end)
        .cloned()
        .collect::<Vec<_>>();
    Some(markdown_immediate_sections(
        &child_headings,
        root_heading.level,
        section.end,
    ))
}

fn markdown_span_line_count(content: &str, start: usize, end: usize) -> Result<usize> {
    Ok(Block::line_span_from_file_range(content, ByteSpan::new(start, end))?.len())
}

#[derive(Debug, Clone)]
struct MarkdownSpan {
    start: usize,
    end: usize,
    kind: BlockKind,
}

#[derive(Debug, Clone, Copy)]
struct MarkdownFlatPart {
    start: usize,
    end: usize,
    kind: BlockKind,
}

fn split_markdown_flat_range(content: &str, start: usize, end: usize) -> Result<Vec<Block>> {
    split_markdown_flat_range_with_attached_heading(content, None, start, end)
}

fn split_markdown_flat_range_attaching_heading(
    content: &str,
    heading_start: usize,
    body_start: usize,
    end: usize,
) -> Result<Vec<Block>> {
    split_markdown_flat_range_with_attached_heading(content, Some(heading_start), body_start, end)
}

fn split_markdown_flat_range_with_attached_heading(
    content: &str,
    attached_heading_start: Option<usize>,
    start: usize,
    end: usize,
) -> Result<Vec<Block>> {
    if start >= end {
        return Ok(Vec::new());
    }

    let slice = &content[start..end];
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .context("Failed to load markdown grammar")?;

    let tree = parser
        .parse(slice, None)
        .context("Failed to parse markdown")?;
    let root = tree.root_node();

    let mut spans = Vec::new();
    collect_markdown_spans(root, &mut spans);
    spans.sort_by_key(|span| span.start);

    let mut parts = Vec::new();
    let mut last_end = 0;
    for span in spans {
        if span.start > last_end {
            push_markdown_unstructured_part(slice, start, last_end, span.start, &mut parts);
        }

        parts.push(MarkdownFlatPart {
            start: start + span.start,
            end: start + span.end,
            kind: span.kind,
        });
        last_end = span.end;
    }

    if last_end < slice.len() {
        push_markdown_unstructured_part(slice, start, last_end, slice.len(), &mut parts);
    }

    attach_markdown_heading_to_first_content(content, &mut parts, attached_heading_start);

    parts
        .into_iter()
        .map(|part| {
            create_block(
                &content[part.start..part.end],
                part.kind,
                content,
                part.start,
                part.end,
                Language::Markdown,
            )
        })
        .collect()
}

fn push_markdown_unstructured_part(
    slice: &str,
    offset: usize,
    start: usize,
    end: usize,
    parts: &mut Vec<MarkdownFlatPart>,
) {
    let chunk = &slice[start..end];
    if chunk.is_empty() {
        return;
    }
    let kind = if chunk.trim().is_empty() {
        BlockKind::Gap
    } else {
        BlockKind::Paragraph
    };
    parts.push(MarkdownFlatPart {
        start: offset + start,
        end: offset + end,
        kind,
    });
}

fn attach_markdown_heading_to_first_content(
    content: &str,
    parts: &mut Vec<MarkdownFlatPart>,
    attached_heading_start: Option<usize>,
) {
    let Some(heading_start) = attached_heading_start else {
        return;
    };
    let Some(first_content_index) = parts.iter().position(|part| {
        part.kind != BlockKind::Gap && !content[part.start..part.end].trim().is_empty()
    }) else {
        return;
    };

    parts.drain(..first_content_index);
    if let Some(first) = parts.first_mut() {
        first.start = heading_start;
    }
}

fn collect_markdown_spans(node: tree_sitter::Node<'_>, spans: &mut Vec<MarkdownSpan>) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if let Some(kind) = markdown_leaf_kind(current.kind()) {
            spans.push(MarkdownSpan {
                start: current.start_byte(),
                end: current.end_byte(),
                kind,
            });
            continue;
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
}

fn markdown_leaf_kind(kind: &str) -> Option<BlockKind> {
    match kind {
        "paragraph" => Some(BlockKind::Paragraph),
        "list" => Some(BlockKind::List),
        "list_item" => Some(BlockKind::ListItem),
        "fenced_code_block" | "indented_code_block" => Some(BlockKind::CodeBlock),
        "block_quote" => Some(BlockKind::Quote),
        "thematic_break"
        | "html_block"
        | "link_reference_definition"
        | "pipe_table"
        | "minus_metadata"
        | "plus_metadata" => Some(BlockKind::Element),
        _ => None,
    }
}

fn markdown_immediate_sections(
    headings: &[MarkdownHeading],
    root_level: u8,
    content_len: usize,
) -> Vec<MarkdownSectionSpan> {
    let mut sections: Vec<MarkdownSectionSpan> = Vec::new();
    let mut level_stack = vec![root_level];

    for heading in headings {
        while level_stack
            .last()
            .is_some_and(|level| *level >= heading.level)
        {
            level_stack.pop();
        }

        let parent_level = level_stack.last().copied().unwrap_or(root_level);
        if parent_level == root_level {
            if let Some(previous) = sections.last_mut() {
                previous.end = heading.start;
            }
            sections.push(MarkdownSectionSpan {
                start: heading.start,
                end: content_len,
            });
        }

        level_stack.push(heading.level);
    }

    sections
}

fn collect_markdown_headings(
    node: tree_sitter::Node<'_>,
    content: &str,
    headings: &mut Vec<MarkdownHeading>,
) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if let Some(level) = markdown_heading_level(current.kind(), current.start_byte(), content) {
            headings.push(MarkdownHeading {
                start: current.start_byte(),
                end: current.end_byte(),
                level,
            });
            continue;
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
}

fn map_kind_for_node(lang: Language, node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    if let Some(registration) = languages::registration(lang) {
        return (registration.top_level.map_kind)(node, content);
    }

    match lang {
        Language::Swift => swift::map_kind(node, content),
        Language::Elisp => map_elisp_kind(node, content),
        Language::Kotlin => map_kotlin_kind(node, content),
        Language::Ruby => map_ruby_kind(node, content),
        Language::C => map_c_kind(node, content),
        _ => map_kind(lang, node.kind()),
    }
}

fn map_elisp_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "function_definition" => BlockKind::Function,
        "macro_definition" => BlockKind::Macro,
        "special_form" => match elisp_special_form_head(node) {
            Some("defconst") => BlockKind::Const,
            Some("defvar") => BlockKind::Variable,
            _ => BlockKind::Code,
        },
        "list" => match elisp_list_head_symbol(node, content) {
            Some("require") | Some("use-package") => BlockKind::Import,
            Some("provide") => BlockKind::Module,
            Some("defcustom") => BlockKind::Variable,
            Some("ert-deftest") => BlockKind::Function,
            _ => BlockKind::Code,
        },
        "comment" => BlockKind::Comment,
        _ => BlockKind::Code,
    }
}

fn elisp_special_form_head(node: tree_sitter::Node<'_>) -> Option<&str> {
    Some(node.child(1)?.kind())
}

fn elisp_form_name<'a>(node: tree_sitter::Node<'a>, source: &'a str) -> Option<&'a str> {
    match node.kind() {
        "function_definition" | "macro_definition" => node
            .child_by_field_name("name")?
            .utf8_text(source.as_bytes())
            .ok(),
        "list" if matches!(elisp_list_head_symbol(node, source), Some("ert-deftest")) => {
            node.named_child(1)?.utf8_text(source.as_bytes()).ok()
        }
        _ => None,
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
        Language::CSharp => match kind {
            "namespace_declaration" | "file_scoped_namespace_declaration" => BlockKind::Module,
            "using_directive" => BlockKind::Import,
            "class_declaration" => BlockKind::Class,
            "interface_declaration" => BlockKind::Interface,
            "enum_declaration" => BlockKind::Enum,
            "record_declaration" | "struct_declaration" => BlockKind::Struct,
            "field_declaration" | "property_declaration" | "event_declaration" => {
                BlockKind::Variable
            }
            "method_declaration" | "constructor_declaration" => BlockKind::Method,
            _ => BlockKind::Code,
        },
        Language::Php => match kind {
            "namespace_definition" => BlockKind::Module,
            "namespace_use_declaration" => BlockKind::Import,
            "class_declaration" => BlockKind::Class,
            "interface_declaration" => BlockKind::Interface,
            "trait_declaration" => BlockKind::Impl,
            "enum_declaration" => BlockKind::Enum,
            "function_definition" => BlockKind::Function,
            "method_declaration" => BlockKind::Method,
            "const_declaration" | "enum_case" => BlockKind::Const,
            "property_declaration" => BlockKind::Variable,
            "use_declaration" => BlockKind::Impl,
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

fn map_kotlin_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "package_header" => BlockKind::Module,
        "import" => BlockKind::Import,
        "function_declaration" => BlockKind::Function,
        "property_declaration" => {
            classify_kotlin_property_kind(&content[node.start_byte()..node.end_byte()])
        }
        "class_declaration" => classify_kotlin_class_kind(node, content),
        "object_declaration" => BlockKind::Class,
        _ => BlockKind::Code,
    }
}

fn map_ruby_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "class" => BlockKind::Class,
        "module" => BlockKind::Module,
        "method" | "singleton_method" => BlockKind::Method,
        "assignment" => {
            if ruby_assignment_targets_constant(node) {
                BlockKind::Const
            } else {
                BlockKind::Code
            }
        }
        "call" => ruby_call_kind(node, content),
        "comment" => BlockKind::Comment,
        _ => BlockKind::Code,
    }
}

fn ruby_call_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match ruby_call_method_name(node, content).as_deref() {
        Some("require") | Some("require_relative") => BlockKind::Import,
        _ => BlockKind::Code,
    }
}

fn map_c_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "comment" => BlockKind::Comment,
        "preproc_include" => BlockKind::Import,
        "type_definition" => BlockKind::Type,
        "struct_specifier" | "union_specifier" => BlockKind::Struct,
        "enum_specifier" => BlockKind::Enum,
        "function_definition" => BlockKind::Function,
        "declaration" => map_c_declaration_kind(node, content),
        _ => BlockKind::Code,
    }
}

fn map_c_declaration_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    if node_contains_kind(node, "function_declarator") {
        return BlockKind::FunctionSignature;
    }

    let Some(type_node) = node.child_by_field_name("type") else {
        return BlockKind::Code;
    };

    match type_node.kind() {
        "struct_specifier" | "union_specifier" => BlockKind::Struct,
        "enum_specifier" => BlockKind::Enum,
        _ if c_node_contains_const_qualifier(node, content) => BlockKind::Const,
        _ => BlockKind::Variable,
    }
}

fn node_contains_kind(node: tree_sitter::Node<'_>, kind: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == kind {
            return true;
        }

        let mut cursor = current.walk();
        let children = current.children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    false
}

fn c_node_contains_const_qualifier(node: tree_sitter::Node<'_>, content: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "type_qualifier"
            && current
                .utf8_text(content.as_bytes())
                .map(|text| text.trim() == "const")
                .unwrap_or(false)
        {
            return true;
        }

        let mut cursor = current.walk();
        let children = current.children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    false
}

fn collect_swift_type_items(
    type_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };
    if !swift::body_is_non_trivial(body, content) {
        return Ok(Vec::new());
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
        )?);
    }

    Ok(blocks)
}

fn collect_rust_impl_items(
    impl_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
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
) -> Result<Vec<Block>> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = map_kind(lang, child.kind());
        if matches!(kind, BlockKind::Code) {
            continue;
        }
        blocks.push(create_block(
            &content[child.start_byte()..child.end_byte()],
            kind,
            content,
            child.start_byte(),
            child.end_byte(),
            lang,
        )?);
    }
    Ok(blocks)
}

fn collect_kotlin_type_items(
    type_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let Some(body) = kotlin_type_body(type_node) else {
        return Ok(Vec::new());
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = map_kotlin_type_member_kind(child, content);
        if matches!(kind, BlockKind::Code) {
            continue;
        }
        blocks.push(create_block(
            &content[child.start_byte()..child.end_byte()],
            kind,
            content,
            child.start_byte(),
            child.end_byte(),
            lang,
        )?);
    }
    Ok(blocks)
}

fn map_kotlin_type_member_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "function_declaration" => {
            if find_named_descendant(node, "function_body").is_some() {
                BlockKind::Method
            } else {
                BlockKind::FunctionSignature
            }
        }
        "property_declaration" => {
            classify_kotlin_property_kind(&content[node.start_byte()..node.end_byte()])
        }
        "class_declaration" => classify_kotlin_class_kind(node, content),
        "object_declaration" | "companion_object" => BlockKind::Class,
        "secondary_constructor" => BlockKind::Method,
        _ => BlockKind::Code,
    }
}

fn collect_csharp_namespace_items(
    namespace_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let Some(body) = namespace_node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        blocks.extend(collect_csharp_declaration_blocks(child, content, lang)?);
    }
    Ok(blocks)
}

fn collect_csharp_declaration_blocks(
    node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let kind = map_kind(lang, node.kind());
    let mut blocks = Vec::new();

    if !matches!(kind, BlockKind::Code) {
        blocks.push(create_block(
            &content[node.start_byte()..node.end_byte()],
            kind,
            content,
            node.start_byte(),
            node.end_byte(),
            lang,
        )?);
    }

    match node.kind() {
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            blocks.extend(collect_csharp_namespace_items(node, content, lang)?);
        }
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "struct_declaration" => {
            blocks.extend(collect_csharp_type_items(node, content, lang)?);
        }
        _ => {}
    }

    Ok(blocks)
}

fn collect_ruby_scope_items(
    scope_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let Some(body) = scope_node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };

    let mut blocks = Vec::new();
    let mut pending = ruby_scope_body_children(body);
    while let Some(child) = pending.pop() {
        let kind = map_kind_for_node(lang, child, content);
        if matches!(kind, BlockKind::Code) {
            continue;
        }

        blocks.push(create_block(
            &content[child.start_byte()..child.end_byte()],
            kind,
            content,
            child.start_byte(),
            child.end_byte(),
            lang,
        )?);

        if matches!(child.kind(), "class" | "module")
            && let Some(body) = child.child_by_field_name("body")
        {
            pending.extend(ruby_scope_body_children(body));
        }
    }

    Ok(blocks)
}

fn ruby_scope_body_children(body: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut cursor = body.walk();
    let mut children = body.named_children(&mut cursor).collect::<Vec<_>>();
    children.reverse();
    children
}

fn collect_csharp_type_items(
    type_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        blocks.extend(collect_csharp_declaration_blocks(child, content, lang)?);
    }
    Ok(blocks)
}

fn collect_php_type_items(
    type_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
) -> Result<Vec<Block>> {
    let Some(body) = type_node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = map_kind(lang, child.kind());
        if matches!(kind, BlockKind::Code) {
            continue;
        }
        blocks.push(create_block(
            &content[child.start_byte()..child.end_byte()],
            kind,
            content,
            child.start_byte(),
            child.end_byte(),
            lang,
        )?);
    }
    Ok(blocks)
}

fn create_block(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Result<Block> {
    create_block_with_complexity(
        text,
        kind,
        full_source,
        start_byte,
        end_byte,
        complexity::calculate(text, lang),
    )
}

fn create_block_with_complexity(
    text: &str,
    kind: BlockKind,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    complexity: Option<u32>,
) -> Result<Block> {
    let mut block = Block::from_file_range(full_source, kind, ByteSpan::new(start_byte, end_byte))?;
    assert_eq!(
        block.content, text,
        "parser split range must name the supplied block content"
    );
    block.complexity = complexity;
    Ok(block)
}

fn collect_test_ranges(
    lang: Language,
    tree: &tree_sitter::Tree,
    source: &str,
) -> Result<Vec<ByteSpan>> {
    if let Some(registration) = languages::registration(lang) {
        return (registration.top_level.collect_test_ranges)(tree, source);
    }

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
        Language::Elisp => collect_elisp_test_ranges(tree.root_node(), source, &mut ranges)?,
        Language::Kotlin => {
            collect_kotlin_test_ranges(tree.root_node(), source, &mut ranges)?;
        }
        Language::Php => {
            collect_php_test_ranges(tree.root_node(), source, &mut ranges)?;
        }
        Language::C => {
            collect_c_test_ranges(tree.root_node(), source, &mut ranges)?;
        }
        Language::CSharp => {
            collect_csharp_test_ranges(tree, source, &mut ranges)?;
        }
        Language::JavaScript => {
            collect_js_test_ranges(&JAVASCRIPT_ARROW_TEST_QUERY, tree, source, &mut ranges)?;
            collect_js_test_ranges(&JAVASCRIPT_MEMBER_TEST_QUERY, tree, source, &mut ranges)?;
        }
        Language::TypeScript => {
            collect_js_test_ranges(&TYPESCRIPT_ARROW_TEST_QUERY, tree, source, &mut ranges)?;
            collect_js_test_ranges(&TYPESCRIPT_MEMBER_TEST_QUERY, tree, source, &mut ranges)?;
        }
        Language::Ruby => {
            collect_ruby_test_ranges(tree.root_node(), source, &mut ranges)?;
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
    collect_test_ranges(lang, tree, source)?
        .into_iter()
        .map(|range| Block::line_span_from_file_range(source, range))
        .collect()
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
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "class_declaration" {
            let header_end = current
                .child_by_field_name("body")
                .map_or(current.end_byte(), |body| body.start_byte());
            let header = &source[current.start_byte()..header_end];
            if header.contains("XCTestCase") {
                ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
            }
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    Ok(())
}

fn collect_kotlin_test_ranges(
    node: tree_sitter::Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "function_declaration" {
            let name_is_test = current
                .child_by_field_name("name")
                .map(|name| name.utf8_text(source.as_bytes()))
                .transpose()?
                .is_some_and(|name| name.starts_with("test"));
            let has_test_annotation = first_child_of_kind(current, "modifiers")
                .map(|modifiers| &source[modifiers.start_byte()..modifiers.end_byte()])
                .is_some_and(|modifiers| modifiers.contains("Test"));

            if name_is_test || has_test_annotation {
                ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
            }
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
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

fn collect_elisp_test_ranges(
    node: tree_sitter::Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "function_definition" | "macro_definition" => {
                if let Some(name) = elisp_form_name(current, source)
                    && name.starts_with("test-")
                {
                    ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
                }
            }
            "list" => {
                if matches!(elisp_list_head_symbol(current, source), Some("ert-deftest")) {
                    ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
                }
            }
            _ => {}
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
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

fn collect_php_test_ranges(
    node: tree_sitter::Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(current.kind(), "function_definition" | "method_declaration")
            && let Some(name_node) = current.child_by_field_name("name")
        {
            let name = name_node.utf8_text(source.as_bytes())?;
            if name.starts_with("test") {
                ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
            }
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    Ok(())
}

fn collect_c_test_ranges(
    node: tree_sitter::Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "function_definition"
            && let Some(name) = c_function_name(current, source)
            && name.starts_with("test_")
        {
            ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    Ok(())
}

fn c_function_name(function_node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let declarator = function_node.child_by_field_name("declarator")?;
    c_declarator_name(declarator, source)
}

fn c_declarator_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" {
            return current
                .utf8_text(source.as_bytes())
                .ok()
                .map(std::string::ToString::to_string);
        }

        let mut children = Vec::new();
        if let Some(declarator) = current.child_by_field_name("declarator") {
            children.push(declarator);
        }
        let mut cursor = current.walk();
        children.extend(current.named_children(&mut cursor));
        stack.extend(children.into_iter().rev());
    }

    None
}

fn collect_csharp_test_ranges(
    tree: &tree_sitter::Tree,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(
        &CSHARP_ATTR_METHOD_TEST_QUERY,
        tree.root_node(),
        source.as_bytes(),
    );
    while let Some(match_) = matches.next() {
        let mut attr_text = None;
        let mut method_range = None;
        for capture in match_.captures {
            let cap_name = &CSHARP_ATTR_METHOD_TEST_QUERY.capture_names()[capture.index as usize];
            match *cap_name {
                "attr" => {
                    attr_text = Some(capture.node.utf8_text(source.as_bytes())?.to_string());
                }
                "method" => {
                    method_range = Some((capture.node.start_byte(), capture.node.end_byte()));
                }
                _ => {}
            }
        }
        if let (Some(attr_text), Some(range)) = (attr_text, method_range)
            && csharp_attribute_text_looks_like_test(&attr_text)
        {
            ranges.push(ByteSpan::new(range.0, range.1));
        }
    }

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(
        &CSHARP_METHOD_TEST_QUERY,
        tree.root_node(),
        source.as_bytes(),
    );
    while let Some(match_) = matches.next() {
        let mut name = None;
        let mut method_range = None;
        for capture in match_.captures {
            let cap_name = &CSHARP_METHOD_TEST_QUERY.capture_names()[capture.index as usize];
            match *cap_name {
                "name" => name = Some(capture.node.utf8_text(source.as_bytes())?.to_string()),
                "method" => {
                    method_range = Some((capture.node.start_byte(), capture.node.end_byte()));
                }
                _ => {}
            }
        }
        if let (Some(name), Some(range)) = (name, method_range)
            && name.starts_with("Test")
        {
            ranges.push(ByteSpan::new(range.0, range.1));
        }
    }

    Ok(())
}

fn csharp_attribute_text_looks_like_test(attr_text: &str) -> bool {
    ["Fact", "Theory", "Test", "TestMethod"]
        .iter()
        .any(|name| attr_text.contains(name))
}

fn collect_ruby_test_ranges(
    node: tree_sitter::Node<'_>,
    source: &str,
    ranges: &mut Vec<ByteSpan>,
) -> Result<()> {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if matches!(current.kind(), "method" | "singleton_method")
            && let Some(name) = ruby_definition_name(current, source)?
            && name.starts_with("test_")
        {
            ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
        }

        if current.kind() == "class"
            && let Some(name) = ruby_definition_name(current, source)?
            && matches!(name.as_str(), value if value.ends_with("Test") || value.ends_with("Tests"))
        {
            ranges.push(ByteSpan::new(current.start_byte(), current.end_byte()));
        }

        let mut cursor = current.walk();
        let children = current.named_children(&mut cursor).collect::<Vec<_>>();
        pending.extend(children.into_iter().rev());
    }

    Ok(())
}

fn ruby_definition_name(node: tree_sitter::Node<'_>, source: &str) -> Result<Option<String>> {
    node.child_by_field_name("name")
        .map(|name| name.utf8_text(source.as_bytes()).map(str::to_string))
        .transpose()
        .map_err(Into::into)
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

    #[test]
    fn test_kotlin_structural_blocks_and_test_detection() {
        let content = "package demo.kotlin\n\nimport kotlin.test.Test\n\nconst val defaultScale = 2\nvar globalCounter = 0\n\ninterface WorkerPort {\n    fun load(id: String): Worker\n}\n\nclass Worker {\n    val name = \"worker\"\n    var enabled = true\n\n    fun process(values: List<Int>): Int {\n        return values.sum()\n    }\n}\n\nobject Registry {\n    val defaultWorker = Worker()\n}\n\nenum class Mode {\n    FAST,\n    SAFE,\n}\n\n@Test\nfun testWorker() {\n    check(true)\n}\n";
        let result = split_result(content, Language::Kotlin);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Module));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Const));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Variable));
        assert!(
            blocks
                .iter()
                .any(|block| block.kind == BlockKind::Interface)
        );
        assert!(
            blocks
                .iter()
                .filter(|block| block.kind == BlockKind::Class)
                .count()
                >= 2
        );
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Enum));
        let test_block = blocks
            .iter()
            .find(|block| block.content.contains("fun testWorker"));
        assert!(test_block.is_some(), "expected Kotlin test block");
        assert!(test_block.is_some_and(|block| block.tags.iter().any(|tag| tag == "test")));
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Paragraph))
        );
    }

    #[test]
    fn test_ruby_structural_blocks_and_test_detection() {
        let content = "require \"json\"\n\nmodule Trueflow\n  DEFAULT_LIMIT = 4\n\n  class Processor\n    SCALE = 2\n\n    def process(values)\n      values.map { |value| value * SCALE }\n    end\n  end\nend\n\nclass ProcessorTest\n  def test_process_formats_non_zero_values\n    processor = Trueflow::Processor.new\n    rendered = processor.process([0, 1, 2])\n\n    raise \"unexpected output\" unless rendered == [0, 2, 4]\n  end\nend\n";
        let result = split_result(content, Language::Ruby);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Module));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Const));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Method));
        assert!(
            blocks.iter().any(|block| {
                block.kind == BlockKind::Method && block.tags.iter().any(|tag| tag == "test")
            }),
            "expected at least one Ruby method to be tagged as a test"
        );
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Paragraph))
        );
        assert!(
            blocks
                .iter()
                .filter(|block| block.kind != BlockKind::Gap)
                .all(|block| block.complexity.is_some())
        );
    }

    #[test]
    fn test_csharp_structural_blocks_and_test_detection() {
        let content = "using System;\nusing Xunit;\n\nnamespace Demo.Workflow {\n    public interface IGreeter {\n        string Name { get; }\n        string BuildGreeting(string target);\n    }\n\n    public readonly record struct GreetingResult(string Message, int Parts);\n\n    public enum WorkflowStatus {\n        Idle,\n        Running,\n    }\n\n    public readonly struct GreetingOptions {\n        public string Prefix { get; }\n    }\n\n    public class GreeterTests {\n        public string Name { get; }\n\n        [Fact]\n        public void BuildGreeting_uses_the_target_name() {\n        }\n    }\n}\n";
        let result = split_result(content, Language::CSharp);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Module));
        assert!(
            blocks
                .iter()
                .any(|block| block.kind == BlockKind::Interface)
        );
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Struct));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Enum));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Variable));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Method));
        assert!(
            blocks
                .iter()
                .filter(|block| block.kind != BlockKind::Gap)
                .all(|block| block.complexity.is_some())
        );
        assert!(
            blocks.iter().any(|block| block.kind == BlockKind::Method
                && block.tags.iter().any(|tag| tag == "test"))
        );
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
    fn markdown_single_root_splits_into_intro_and_child_sections() {
        let content = "# Guide\nIntro.\n\n## Install\nInstall steps.\n\n## Usage\nUsage details.\n";
        let blocks = split_blocks(content, Language::Markdown);

        let kinds = blocks.iter().map(|block| block.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Section,
                BlockKind::Section,
                BlockKind::Section,
                BlockKind::Section
            ]
        );
        assert_eq!(blocks[0].content, content);
        assert_eq!(blocks[1].content, "# Guide\nIntro.\n\n");
        assert_eq!(blocks[2].content, "## Install\nInstall steps.\n\n");
        assert_eq!(blocks[3].content, "## Usage\nUsage details.\n");
    }

    #[test]
    fn markdown_single_root_without_intro_starts_at_child_sections() {
        let content = "# Guide\n\n## Install\nInstall steps.\n\n## Usage\nUsage details.\n";
        let blocks = split_blocks(content, Language::Markdown);

        let kinds = blocks.iter().map(|block| block.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![BlockKind::Section, BlockKind::Section, BlockKind::Section]
        );
        assert_eq!(blocks[0].content, content);
        assert_eq!(blocks[1].content, "## Install\nInstall steps.\n\n");
        assert_eq!(blocks[2].content, "## Usage\nUsage details.\n");
    }

    #[test]
    fn markdown_oversized_leaf_section_splits_to_heading_attached_children() {
        let mut content = String::from("# Long Notes\n");
        for index in 0..55 {
            content.push_str(&format!("\nParagraph {index} explains one idea.\n"));
        }
        let blocks = split_blocks(&content, Language::Markdown);

        assert_eq!(blocks[0].kind, BlockKind::Section);
        assert_eq!(blocks[0].content, content);
        assert!(blocks[0].line_span().len() > MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES);
        let review_children = blocks[1..]
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .collect::<Vec<_>>();
        assert_eq!(review_children[0].kind, BlockKind::Paragraph);
        assert!(
            review_children[0]
                .content
                .starts_with("# Long Notes\n\nParagraph 0 explains one idea.")
        );
        assert!(
            review_children[1..]
                .iter()
                .any(|block| block.kind == BlockKind::Paragraph)
        );
        assert!(
            !blocks[1..]
                .iter()
                .any(|block| block.kind == BlockKind::Header)
        );
        assert!(blocks[1..].iter().all(|block| {
            block.kind == BlockKind::Gap
                || block.line_span().len() <= MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES
        }));
    }

    #[test]
    fn markdown_oversized_leaf_section_keeps_large_list_whole() {
        let mut content = String::from("# Tasks\n\n");
        for index in 0..60 {
            content.push_str(&format!("- Task {index} stays with the list.\n"));
        }
        let blocks = split_blocks(&content, Language::Markdown);

        assert!(
            !blocks
                .iter()
                .any(|block| { block.kind == BlockKind::Section && block.content == content })
        );
        let lists = blocks
            .iter()
            .filter(|block| block.kind == BlockKind::List)
            .collect::<Vec<_>>();
        assert_eq!(lists.len(), 1);
        assert!(
            lists[0]
                .content
                .starts_with("# Tasks\n\n- Task 0 stays with the list.")
        );
        assert!(lists[0].content.contains("- Task 59 stays with the list."));
    }

    #[test]
    fn markdown_oversized_leaf_section_keeps_code_fence_whole() {
        let mut content = String::from("# Example\n\n```rust\n");
        for index in 0..60 {
            content.push_str(&format!("println!(\"line {index}\");\n"));
        }
        content.push_str("```\n");
        let blocks = split_blocks(&content, Language::Markdown);

        assert!(
            !blocks
                .iter()
                .any(|block| { block.kind == BlockKind::Section && block.content == content })
        );
        let code_blocks = blocks
            .iter()
            .filter(|block| block.kind == BlockKind::CodeBlock)
            .collect::<Vec<_>>();
        assert_eq!(code_blocks.len(), 1);
        assert!(code_blocks[0].content.starts_with("# Example\n\n```rust\n"));
        assert!(code_blocks[0].content.contains("println!(\"line 59\");"));
        assert!(code_blocks[0].content.ends_with("```\n"));
    }

    #[test]
    fn markdown_without_headings_splits_into_paragraph_review_blocks() {
        let content = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
        let blocks = split_blocks(content, Language::Markdown);

        let kinds = blocks.iter().map(|block| block.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Paragraph,
                BlockKind::Gap,
                BlockKind::Paragraph,
                BlockKind::Gap,
                BlockKind::Paragraph
            ]
        );
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.content.as_str())
                .collect::<String>(),
            content
        );
    }

    #[test]
    fn test_split_text_paragraphs() {
        assert_paragraph_split(Language::Text);
    }

    #[test]
    fn test_split_toml_uses_structural_blocks() {
        let content = "title = \"deploy\"\nkeywords = [\"blue\", \"green\"]\n\n[owner]\nname = \"platform\"\n\n[database]\nports = [8001, 8002]\n";
        let result = split_result(content, Language::Toml);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        let kinds: Vec<_> = blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .map(|block| block.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Content,
                BlockKind::List,
                BlockKind::Section,
                BlockKind::Section,
            ]
        );
        assert!(
            !blocks
                .iter()
                .any(|block| block.kind == BlockKind::Paragraph)
        );
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
        let result = split_result("fn foo() {}\n\nstruct Bar;", Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        assert!(result.diagnostics.is_empty());

        let review_blocks = result
            .blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .collect::<Vec<_>>();
        assert_eq!(review_blocks.len(), 2);
        assert_eq!(review_blocks[0].kind, BlockKind::Function);
        assert_eq!(review_blocks[0].content, "fn foo() {}");
        assert_eq!(review_blocks[1].kind, BlockKind::Struct);
        assert_eq!(review_blocks[1].content, "struct Bar;");
    }

    #[test]
    fn test_split_go_simple_maps_import_struct_and_function() {
        let result = split_result(
            "package main\n\nimport \"fmt\"\n\ntype Worker struct{}\n\nfunc run() {\n    fmt.Println(\"ok\")\n}\n",
            Language::Go,
        );
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
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
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Function));
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
    fn test_split_elisp_structural_blocks_and_test_detection() {
        let content = "(require 'cl-lib)\n\n(use-package seq\n  :ensure t)\n\n(defconst elisp-support-limit 3\n  \"Retry count.\")\n\n(defvar elisp-support-name \"trueflow\"\n  \"Display name.\")\n\n(defcustom elisp-support-enabled t\n  \"Whether support is enabled.\"\n  :type 'boolean)\n\n(defmacro elisp-support-with-message (label &rest body)\n  `(progn\n     (message \"running %s\" ,label)\n     ,@body))\n\n(defun elisp-support-run ()\n  (message \"ok\"))\n\n(ert-deftest elisp-support-run-test ()\n  (should t))\n\n(provide 'elisp-support)\n";
        let result = split_result(content, Language::Elisp);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.blocks;
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Const));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Variable));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Macro));
        assert!(
            blocks
                .iter()
                .filter(|block| block.kind == BlockKind::Function)
                .count()
                >= 2
        );
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Module));
        assert!(
            blocks
                .iter()
                .any(|block| block.content.contains("ert-deftest") && block.has_tag("test"))
        );
        assert!(
            blocks
                .iter()
                .filter(|block| block.kind != BlockKind::Gap)
                .all(|block| block.complexity.is_some())
        );
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Paragraph))
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
    fn tree_sitter_blocks_preserve_absolute_utf8_byte_spans() {
        let source = "// é\nconst α: &str = \"λ\";\n\nfn later() {}\n";
        let result = split_result(source, Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);

        let later_start = source.find("fn later").unwrap();
        let later_end = later_start + "fn later() {}".len();
        let later = result
            .blocks
            .iter()
            .find(|block| block.content == "fn later() {}")
            .unwrap();
        assert_eq!(later.byte_span(), ByteSpan::new(later_start, later_end));

        for block in result.blocks {
            assert!(source.is_char_boundary(block.start_byte));
            assert!(source.is_char_boundary(block.end_byte));
            assert_eq!(
                block.content,
                source[block.start_byte..block.end_byte],
                "tree-sitter block must retain its absolute source slice"
            );
        }
    }

    #[test]
    fn fallback_blocks_preserve_absolute_utf8_byte_spans() {
        let source = "éclair paragraph.\n\nλater paragraph.";
        let blocks = fallback_split_blocks(source, FallbackMode::Text, Language::Unknown);
        assert_eq!(blocks.len(), 3);

        for block in blocks {
            assert!(source.is_char_boundary(block.start_byte));
            assert!(source.is_char_boundary(block.end_byte));
            assert_eq!(
                block.content,
                source[block.start_byte..block.end_byte],
                "fallback block must retain its absolute source slice"
            );
        }
    }

    #[test]
    fn test_split_includes_optimization_pipeline() {
        let source = "use std::fmt;\n\nuse std::io;\n";
        let expected = &source[..source.len() - 1];
        let result = split_result(source, Language::Rust);
        assert_eq!(result.strategy, BlockSplitStrategy::Structured);
        let blocks = result.into_review_blocks(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Imports);
        assert_eq!(blocks[0].content, expected);
        assert_eq!(blocks[0].byte_span(), ByteSpan::new(0, expected.len()));
    }
}
