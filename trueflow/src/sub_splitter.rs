use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::code_comments;
use crate::hashing::TreeHash;
use crate::review_units::{MAX_REVIEW_UNIT_SPAN_LINES, block_line_span};
use crate::text_split::{paragraph_break_regex, split_by_paragraph_breaks};
use crate::{languages, nix_blocks, rust, swift, toml_blocks};
use anyhow::{Context, Result};
use tracing::info;
use tree_sitter::Parser;
use tree_sitter_md;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubSplitSemantics {
    /// The returned blocks are the direct review units for the parent block.
    ReviewUnits,
    /// The returned blocks are semantic children that may themselves be split further.
    StructuralChildren,
}

impl SubSplitSemantics {
    pub fn supports_review_unit_invariant(self) -> bool {
        matches!(self, Self::ReviewUnits)
    }
}

#[derive(Debug, Clone)]
pub struct SubSplitResult {
    pub blocks: Vec<Block>,
    pub semantics: SubSplitSemantics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitPlan {
    IdentityReviewUnit,
    MarkdownChildren,
    MarkdownReviewUnits,
    SentenceReviewUnits,
    CodeReviewUnits,
    RustFunctionReviewUnits,
    ElispFunctionReviewUnits,
    RustImplReviewUnits,
    SwiftFunctionReviewUnits,
    SwiftTypeReviewUnits,
    KotlinFunctionReviewUnits,
    KotlinTypeReviewUnits,
    PythonFunctionReviewUnits,
    RubyMethodReviewUnits,
    RubyScopeReviewUnits,
    JsFunctionReviewUnits,
    JavaFunctionReviewUnits,
    JavaTypeReviewUnits,
    CSharpFunctionReviewUnits,
    CSharpTypeReviewUnits,
    PhpFunctionReviewUnits,
    PhpTypeReviewUnits,
    CFunctionReviewUnits,
    CTypeReviewUnits,
}

impl SplitPlan {
    fn semantics(self) -> SubSplitSemantics {
        match self {
            Self::MarkdownChildren => SubSplitSemantics::StructuralChildren,
            Self::IdentityReviewUnit
            | Self::MarkdownReviewUnits
            | Self::SentenceReviewUnits
            | Self::CodeReviewUnits
            | Self::RustFunctionReviewUnits
            | Self::ElispFunctionReviewUnits
            | Self::RustImplReviewUnits
            | Self::SwiftFunctionReviewUnits
            | Self::SwiftTypeReviewUnits
            | Self::KotlinFunctionReviewUnits
            | Self::KotlinTypeReviewUnits
            | Self::PythonFunctionReviewUnits
            | Self::RubyMethodReviewUnits
            | Self::RubyScopeReviewUnits
            | Self::JsFunctionReviewUnits
            | Self::JavaFunctionReviewUnits
            | Self::JavaTypeReviewUnits
            | Self::CSharpFunctionReviewUnits
            | Self::CSharpTypeReviewUnits
            | Self::PhpFunctionReviewUnits
            | Self::PhpTypeReviewUnits
            | Self::CFunctionReviewUnits
            | Self::CTypeReviewUnits => SubSplitSemantics::ReviewUnits,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SplitOptions {
    pub force_expand_children: bool,
}

pub fn split(block: &Block, lang: Language) -> Result<Vec<Block>> {
    split_result(block, lang).map(|result| result.blocks)
}

pub fn split_result(block: &Block, lang: Language) -> Result<SubSplitResult> {
    split_result_with_options(block, lang, SplitOptions::default())
}

pub fn split_result_for_child_navigation(block: &Block, lang: Language) -> Result<SubSplitResult> {
    split_result_with_options(
        block,
        lang,
        SplitOptions {
            force_expand_children: true,
        },
    )
}

fn split_result_with_options(
    block: &Block,
    lang: Language,
    options: SplitOptions,
) -> Result<SubSplitResult> {
    info!(
        "sub_splitter start (lang={:?}, kind={}, bytes={}, hash={}, force_expand_children={})",
        lang,
        block.kind.as_str(),
        block.content.len(),
        block.hash,
        options.force_expand_children
    );

    if let Some(language_registration) = languages::registration(lang) {
        let registration = (language_registration.sub_split)(block.kind);
        if !options.force_expand_children
            && matches!(
                registration.semantics,
                languages::LanguageSubSplitSemantics::ReviewUnits
            )
            && block_line_span(block) <= MAX_REVIEW_UNIT_SPAN_LINES
        {
            let result = SubSplitResult {
                blocks: vec![block.clone()],
                semantics: SubSplitSemantics::ReviewUnits,
            };
            info!(
                "sub_splitter kept parent review unit (lines={}, max_lines={})",
                block_line_span(block),
                MAX_REVIEW_UNIT_SPAN_LINES
            );
            return Ok(result);
        }

        let result = SubSplitResult {
            blocks: (registration.splitter)(block)?,
            semantics: match registration.semantics {
                languages::LanguageSubSplitSemantics::ReviewUnits => SubSplitSemantics::ReviewUnits,
                languages::LanguageSubSplitSemantics::StructuralChildren => {
                    SubSplitSemantics::StructuralChildren
                }
            },
        };

        info!(
            "sub_splitter done (blocks={}, semantics={:?})",
            result.blocks.len(),
            result.semantics
        );
        return Ok(result);
    }

    if matches!(lang, Language::Toml) && matches!(block.kind, BlockKind::Section | BlockKind::List)
    {
        let blocks = split_toml_structural_children(block, block.kind)?;
        let result = SubSplitResult {
            blocks,
            semantics: SubSplitSemantics::StructuralChildren,
        };
        info!(
            "sub_splitter done (blocks={}, semantics={:?})",
            result.blocks.len(),
            result.semantics
        );
        return Ok(result);
    }

    if matches!(lang, Language::Nix)
        && matches!(
            block.kind,
            BlockKind::Variable
                | BlockKind::Section
                | BlockKind::List
                | BlockKind::Code
                | BlockKind::Function
        )
        && let Some(blocks) = split_nix_structural_children(block)?
    {
        let result = SubSplitResult {
            blocks,
            semantics: SubSplitSemantics::StructuralChildren,
        };
        info!(
            "sub_splitter done (blocks={}, semantics={:?})",
            result.blocks.len(),
            result.semantics
        );
        return Ok(result);
    }

    let plan = determine_split_plan(block.kind, lang);
    if !options.force_expand_children && should_keep_parent_review_unit(plan, block) {
        let result = SubSplitResult {
            blocks: vec![block.clone()],
            semantics: SubSplitSemantics::ReviewUnits,
        };
        info!(
            "sub_splitter kept parent review unit (lines={}, max_lines={})",
            block_line_span(block),
            MAX_REVIEW_UNIT_SPAN_LINES
        );
        return Ok(result);
    }
    let blocks = match plan {
        SplitPlan::IdentityReviewUnit => vec![block.clone()],
        SplitPlan::MarkdownChildren => split_markdown_tree(block)?,
        SplitPlan::MarkdownReviewUnits => split_markdown_sentences(block)?,
        SplitPlan::SentenceReviewUnits => split_sentences(block)?,
        SplitPlan::CodeReviewUnits => split_code(block)?,
        SplitPlan::RustFunctionReviewUnits => split_rust_function(block)?,
        SplitPlan::ElispFunctionReviewUnits => split_elisp_function(block)?,
        SplitPlan::RustImplReviewUnits => split_rust_impl(block)?,
        SplitPlan::SwiftFunctionReviewUnits => split_swift_function(block)?,
        SplitPlan::SwiftTypeReviewUnits => split_swift_type(block)?,
        SplitPlan::KotlinFunctionReviewUnits => split_kotlin_function(block)?,
        SplitPlan::KotlinTypeReviewUnits => split_kotlin_type(block)?,
        SplitPlan::PythonFunctionReviewUnits => split_python_function(block)?,
        SplitPlan::RubyMethodReviewUnits => split_ruby_method(block)?,
        SplitPlan::RubyScopeReviewUnits => split_ruby_scope(block)?,
        SplitPlan::JsFunctionReviewUnits => split_js_function(block, lang)?,
        SplitPlan::JavaFunctionReviewUnits => split_java_function(block)?,
        SplitPlan::JavaTypeReviewUnits => split_java_type(block)?,
        SplitPlan::CSharpFunctionReviewUnits => split_csharp_function(block)?,
        SplitPlan::CSharpTypeReviewUnits => split_csharp_type(block)?,
        SplitPlan::PhpFunctionReviewUnits => split_php_function(block)?,
        SplitPlan::PhpTypeReviewUnits => split_php_type(block)?,
        SplitPlan::CFunctionReviewUnits => split_c_function(block)?,
        SplitPlan::CTypeReviewUnits => split_c_type(block)?,
    };

    let result = SubSplitResult {
        blocks,
        semantics: plan.semantics(),
    };

    info!(
        "sub_splitter done (blocks={}, semantics={:?})",
        result.blocks.len(),
        result.semantics
    );
    Ok(result)
}

fn should_keep_parent_review_unit(plan: SplitPlan, block: &Block) -> bool {
    !matches!(plan, SplitPlan::IdentityReviewUnit)
        && block_line_span(block) <= MAX_REVIEW_UNIT_SPAN_LINES
}

fn determine_split_plan(kind: BlockKind, lang: Language) -> SplitPlan {
    match lang {
        Language::Markdown if matches!(kind, BlockKind::Paragraph | BlockKind::ListItem) => {
            SplitPlan::MarkdownReviewUnits
        }
        Language::Markdown
            if matches!(
                kind,
                BlockKind::Header | BlockKind::CodeBlock | BlockKind::Quote | BlockKind::Element
            ) =>
        {
            SplitPlan::IdentityReviewUnit
        }
        Language::Markdown => SplitPlan::MarkdownChildren,
        Language::Text => SplitPlan::SentenceReviewUnits,
        Language::Toml | Language::Nix | Language::Just => SplitPlan::CodeReviewUnits,
        Language::Elisp if matches!(kind, BlockKind::Function | BlockKind::Macro) => {
            SplitPlan::ElispFunctionReviewUnits
        }
        Language::Rust if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::RustFunctionReviewUnits
        }
        Language::Rust if matches!(kind, BlockKind::Impl | BlockKind::Interface) => {
            SplitPlan::RustImplReviewUnits
        }
        Language::Swift if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::SwiftFunctionReviewUnits
        }
        Language::Swift
            if matches!(
                kind,
                BlockKind::Impl
                    | BlockKind::Interface
                    | BlockKind::Class
                    | BlockKind::Struct
                    | BlockKind::Enum
            ) =>
        {
            SplitPlan::SwiftTypeReviewUnits
        }
        Language::Kotlin if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::KotlinFunctionReviewUnits
        }
        Language::Kotlin
            if matches!(
                kind,
                BlockKind::Class | BlockKind::Interface | BlockKind::Enum
            ) =>
        {
            SplitPlan::KotlinTypeReviewUnits
        }
        Language::Python if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::PythonFunctionReviewUnits
        }
        Language::Ruby if matches!(kind, BlockKind::Method) => SplitPlan::RubyMethodReviewUnits,
        Language::Ruby if matches!(kind, BlockKind::Module | BlockKind::Class) => {
            SplitPlan::RubyScopeReviewUnits
        }
        Language::JavaScript | Language::TypeScript
            if matches!(
                kind,
                BlockKind::Function | BlockKind::Method | BlockKind::Export
            ) =>
        {
            SplitPlan::JsFunctionReviewUnits
        }
        Language::Java if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::JavaFunctionReviewUnits
        }
        Language::Java
            if matches!(
                kind,
                BlockKind::Class
                    | BlockKind::Interface
                    | BlockKind::Enum
                    | BlockKind::Struct
                    | BlockKind::Type
            ) =>
        {
            SplitPlan::JavaTypeReviewUnits
        }
        Language::CSharp if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::CSharpFunctionReviewUnits
        }
        Language::CSharp
            if matches!(
                kind,
                BlockKind::Class | BlockKind::Interface | BlockKind::Enum | BlockKind::Struct
            ) =>
        {
            SplitPlan::CSharpTypeReviewUnits
        }
        Language::Php if matches!(kind, BlockKind::Function | BlockKind::Method) => {
            SplitPlan::PhpFunctionReviewUnits
        }
        Language::Php
            if matches!(
                kind,
                BlockKind::Class | BlockKind::Interface | BlockKind::Enum | BlockKind::Impl
            ) =>
        {
            SplitPlan::PhpTypeReviewUnits
        }
        Language::C if matches!(kind, BlockKind::Function) => SplitPlan::CFunctionReviewUnits,
        Language::C if matches!(kind, BlockKind::Struct | BlockKind::Enum | BlockKind::Type) => {
            SplitPlan::CTypeReviewUnits
        }
        _ => SplitPlan::CodeReviewUnits,
    }
}

pub(crate) fn split_code_review_units(block: &Block) -> Result<Vec<Block>> {
    let content = &block.content;
    let blocks = split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let block_kind = if is_gap {
            BlockKind::Gap
        } else {
            classify_code_chunk(chunk)
        };
        create_sub_block_with_kind(block, chunk, start, end, block_kind)
    });
    Ok(blocks)
}

fn split_code(block: &Block) -> Result<Vec<Block>> {
    split_code_review_units(block)
}

fn split_toml_structural_children(block: &Block, kind: BlockKind) -> Result<Vec<Block>> {
    let spans = match kind {
        BlockKind::Section => toml_blocks::split_section_children(&block.content)?,
        BlockKind::List => toml_blocks::split_list_children(&block.content)?,
        _ => Vec::new(),
    };

    Ok(spans_to_sub_blocks(block, &spans))
}

fn split_nix_structural_children(block: &Block) -> Result<Option<Vec<Block>>> {
    let Some(spans) = nix_blocks::split_structural_children(&block.content, block.kind)? else {
        return Ok(None);
    };
    Ok(Some(spans_to_sub_blocks(block, &spans)))
}

fn spans_to_sub_blocks<T>(block: &Block, spans: &[T]) -> Vec<Block>
where
    T: StructuralSpan,
{
    spans
        .iter()
        .map(|span| {
            create_sub_block_with_kind(
                block,
                &block.content[span.start()..span.end()],
                span.start(),
                span.end(),
                span.kind(),
            )
        })
        .collect()
}

trait StructuralSpan {
    fn start(&self) -> usize;
    fn end(&self) -> usize;
    fn kind(&self) -> BlockKind;
}

impl StructuralSpan for toml_blocks::TomlSpan {
    fn start(&self) -> usize {
        self.start
    }

    fn end(&self) -> usize {
        self.end
    }

    fn kind(&self) -> BlockKind {
        self.kind
    }
}

impl StructuralSpan for nix_blocks::NixSpan {
    fn start(&self) -> usize {
        self.start
    }

    fn end(&self) -> usize {
        self.end
    }

    fn kind(&self) -> BlockKind {
        self.kind
    }
}

fn split_markdown_tree(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .context("Failed to load markdown grammar")?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse markdown")?;
    let root = tree.root_node();

    let mut headings = Vec::new();
    collect_markdown_heading_spans(root, content, &mut headings);
    headings.sort_by_key(|heading| heading.start);

    let Some(root_heading) = headings
        .first()
        .copied()
        .filter(|heading| heading.start == 0)
    else {
        return split_markdown_flat_range(block, 0, content.len());
    };

    let mut blocks = vec![create_sub_block_with_kind(
        block,
        &content[root_heading.start..root_heading.end],
        root_heading.start,
        root_heading.end,
        BlockKind::Header,
    )];

    let child_sections =
        immediate_markdown_child_sections(&headings, root_heading.level, content.len());
    let content_end = child_sections
        .first()
        .map(|section| section.start)
        .unwrap_or(content.len());
    blocks.extend(split_markdown_flat_range(
        block,
        root_heading.end,
        content_end,
    )?);

    for section in child_sections {
        blocks.push(create_sub_block_with_kind(
            block,
            &content[section.start..section.end],
            section.start,
            section.end,
            BlockKind::Section,
        ));
    }

    if blocks.is_empty() {
        return split_code(block);
    }

    Ok(blocks)
}

#[derive(Debug, Clone, Copy)]
struct MarkdownHeadingSpan {
    start: usize,
    end: usize,
    level: u8,
}

#[derive(Debug, Clone, Copy)]
struct MarkdownSectionSpan {
    start: usize,
    end: usize,
}

fn split_markdown_flat_range(block: &Block, start: usize, end: usize) -> Result<Vec<Block>> {
    if start >= end {
        return Ok(Vec::new());
    }

    let content = block.content.as_str();
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

    let mut blocks = Vec::new();
    let mut last_end = 0;
    for span in spans {
        if span.start > last_end {
            let gap = &slice[last_end..span.start];
            if !gap.is_empty() {
                blocks.push(create_sub_block_with_kind(
                    block,
                    gap,
                    start + last_end,
                    start + span.start,
                    BlockKind::Gap,
                ));
            }
        }

        let chunk = &slice[span.start..span.end];
        blocks.push(create_sub_block_with_kind(
            block,
            chunk,
            start + span.start,
            start + span.end,
            span.kind,
        ));
        last_end = span.end;
    }

    if last_end < slice.len() {
        let tail = &slice[last_end..];
        if !tail.is_empty() {
            let kind = if tail.trim().is_empty() {
                BlockKind::Gap
            } else {
                BlockKind::Paragraph
            };
            blocks.push(create_sub_block_with_kind(
                block,
                tail,
                start + last_end,
                end,
                kind,
            ));
        }
    }

    Ok(blocks)
}

fn collect_markdown_heading_spans(
    node: tree_sitter::Node<'_>,
    content: &str,
    headings: &mut Vec<MarkdownHeadingSpan>,
) {
    if let Some(level) = markdown_heading_level(node.kind(), node.start_byte(), content) {
        headings.push(MarkdownHeadingSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            level,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_markdown_heading_spans(child, content, headings);
    }
}

fn immediate_markdown_child_sections(
    headings: &[MarkdownHeadingSpan],
    root_level: u8,
    content_len: usize,
) -> Vec<MarkdownSectionSpan> {
    let mut sections: Vec<MarkdownSectionSpan> = Vec::new();
    let mut level_stack = vec![root_level];

    for heading in headings.iter().skip(1) {
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

fn split_markdown_sentences(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut spans = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    let mut idx = 0;

    while idx < bytes.len() {
        let ch = bytes[idx] as char;
        if matches!(ch, '.' | '!' | '?') {
            let mut end = idx + 1;
            while end < bytes.len() && bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            spans.push((start, end));
            start = end;
            idx = end;
            continue;
        }
        idx += 1;
    }

    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }

    let mut blocks = Vec::new();
    for (start, end) in spans {
        let chunk = &content[start..end];
        if chunk.trim().is_empty() {
            continue;
        }
        blocks.push(create_sub_block_with_kind(
            block,
            chunk,
            start,
            end,
            BlockKind::Sentence,
        ));
    }

    if blocks.is_empty() {
        blocks.push(create_sub_block_with_kind(
            block,
            content,
            0,
            content.len(),
            BlockKind::Sentence,
        ));
    }

    Ok(blocks)
}

fn split_sentences(block: &Block) -> Result<Vec<Block>> {
    split_markdown_sentences(block)
}

struct FunctionSplitConfig<'a> {
    language: tree_sitter::Language,
    function_kinds: &'a [&'a str],
    body_kind: &'a str,
    body_statement_kind: Option<&'a str>,
    signature_end: fn(&str, usize) -> usize,
    comment_kinds: &'a [&'a str],
    trailing_delimiters: &'a [&'a str],
}

fn split_rust_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_rust::LANGUAGE.into(),
            function_kinds: &["function_item"],
            body_kind: "block",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["line_comment", "block_comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_rust_impl(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse impl")?;
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "impl_item" {
            let items = collect_rust_impl_items(block, child)?;
            if !items.is_empty() {
                return Ok(items);
            }
        }
    }

    split_code(block)
}

fn split_elisp_function(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_elisp::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse elisp function")?;
    let root = tree.root_node();
    let Some(form_node) =
        find_named_descendant_any(root, &["function_definition", "macro_definition", "list"])
    else {
        return split_code(block);
    };

    let signature_end = match form_node.kind() {
        "function_definition" | "macro_definition" => elisp_definition_signature_end(form_node),
        "list" => elisp_ert_test_signature_end(form_node, content),
        _ => None,
    };
    let Some(signature_end) = signature_end else {
        return split_code(block);
    };
    if signature_end >= content.len() {
        return split_code(block);
    }

    let mut blocks = vec![create_sub_block_with_kind(
        block,
        &content[..signature_end],
        0,
        signature_end,
        BlockKind::FunctionSignature,
    )];
    blocks.extend(split_by_paragraph_breaks(
        &content[signature_end..],
        |chunk, start, end, is_gap| {
            let kind = if is_gap {
                BlockKind::Gap
            } else {
                classify_code_chunk(chunk)
            };
            create_sub_block_with_kind(
                block,
                chunk,
                signature_end + start,
                signature_end + end,
                kind,
            )
        },
    ));

    Ok(blocks)
}

fn elisp_definition_signature_end(node: tree_sitter::Node<'_>) -> Option<usize> {
    let name = node.child_by_field_name("name")?;
    let parameters = node.child_by_field_name("parameters");
    let docstring = node.child_by_field_name("docstring");
    collect_elisp_form_items(node, |child| {
        same_tree_sitter_node(child, name)
            || parameters.is_some_and(|parameters| same_tree_sitter_node(child, parameters))
            || docstring.is_some_and(|docstring| same_tree_sitter_node(child, docstring))
    })
    .first()
    .map(|node| node.start_byte())
}

fn elisp_ert_test_signature_end(node: tree_sitter::Node<'_>, content: &str) -> Option<usize> {
    if !matches!(elisp_list_head_symbol(node, content), Some("ert-deftest")) {
        return None;
    }

    let head = node.named_child(0)?;
    let name = node.named_child(1)?;
    let parameters = node.named_child(2)?;
    let docstring = node.named_child(3).filter(|node| node.kind() == "string");
    collect_elisp_form_items(node, |child| {
        same_tree_sitter_node(child, head)
            || same_tree_sitter_node(child, name)
            || same_tree_sitter_node(child, parameters)
            || docstring.is_some_and(|docstring| same_tree_sitter_node(child, docstring))
    })
    .first()
    .map(|node| node.start_byte())
}

fn collect_elisp_form_items<'a, F>(
    node: tree_sitter::Node<'a>,
    mut should_skip: F,
) -> Vec<tree_sitter::Node<'a>>
where
    F: FnMut(tree_sitter::Node<'a>) -> bool,
{
    let mut items = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if (child.is_named() || child.kind() == "comment") && !should_skip(child) {
            items.push(child);
        }
    }
    items
}

fn same_tree_sitter_node(left: tree_sitter::Node<'_>, right: tree_sitter::Node<'_>) -> bool {
    left.kind() == right.kind()
        && left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
}

fn elisp_list_head_symbol<'a>(node: tree_sitter::Node<'a>, content: &'a str) -> Option<&'a str> {
    let head = node.named_child(0)?;
    (head.kind() == "symbol")
        .then(|| head.utf8_text(content.as_bytes()).ok())
        .flatten()
}

fn collect_rust_impl_items(parent: &Block, impl_node: tree_sitter::Node<'_>) -> Result<Vec<Block>> {
    Ok(rust::collect_impl_member_spans(impl_node)
        .into_iter()
        .map(|member| {
            create_sub_block_with_kind(
                parent,
                &parent.content[member.start_byte..member.end_byte],
                member.start_byte,
                member.end_byte,
                member.kind,
            )
        })
        .collect())
}

fn split_python_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_python::LANGUAGE.into(),
            function_kinds: &["function_definition"],
            body_kind: "block",
            body_statement_kind: None,
            signature_end: signature_end_before_body,
            comment_kinds: &["comment", "line_comment", "block_comment"],
            trailing_delimiters: &[],
        },
    )
}

fn split_ruby_method(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_ruby::LANGUAGE.into(),
            function_kinds: &["method", "singleton_method"],
            body_kind: "body_statement",
            body_statement_kind: None,
            signature_end: signature_end_before_body,
            comment_kinds: &["comment"],
            trailing_delimiters: &["end"],
        },
    )
}

fn split_js_function(block: &Block, lang: Language) -> Result<Vec<Block>> {
    let language = match lang {
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    };
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language,
            function_kinds: &["function_declaration"],
            body_kind: "statement_block",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["comment", "line_comment", "block_comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_swift_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_swift::LANGUAGE.into(),
            function_kinds: &["function_declaration"],
            body_kind: "function_body",
            body_statement_kind: Some("statements"),
            signature_end: signature_end_offset,
            comment_kinds: &["comment", "multiline_comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_swift_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_swift::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse swift type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant(root, "class_declaration")
        .or_else(|| find_named_descendant(root, "protocol_declaration"));
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = type_node.child_by_field_name("body") else {
        return split_code(block);
    };
    if !swift::body_is_non_trivial(body, content) {
        return split_code(block);
    }

    let items = collect_swift_type_items(block, body, content);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_kotlin_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_kotlin_ng::LANGUAGE.into(),
            function_kinds: &["function_declaration"],
            body_kind: "function_body",
            body_statement_kind: Some("block"),
            signature_end: signature_end_offset,
            comment_kinds: &["line_comment", "block_comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_kotlin_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse kotlin type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant_any(root, &["class_declaration", "object_declaration"]);
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = kotlin_type_body(type_node) else {
        return split_code(block);
    };

    let items = collect_kotlin_type_items(block, body, content);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_java_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_java::LANGUAGE.into(),
            function_kinds: &[
                "method_declaration",
                "constructor_declaration",
                "compact_constructor_declaration",
            ],
            body_kind: "block",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["line_comment", "block_comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_java_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse java type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant_any(
        root,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "annotation_type_declaration",
        ],
    );
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = type_node.child_by_field_name("body") else {
        return split_code(block);
    };

    let items = collect_java_type_items(block, body);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_csharp_function(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let Some(body_start) = content.find('{') else {
        return split_code(block);
    };

    let signature_end = signature_end_offset(content, body_start);
    if signature_end == 0 || signature_end > content.len() {
        return split_code(block);
    }

    let mut blocks = vec![create_sub_block_with_kind(
        block,
        &content[..signature_end],
        0,
        signature_end,
        BlockKind::FunctionSignature,
    )];

    let rest = &content[signature_end..];
    let mut push_chunk = |chunk: &str, start: usize, end: usize, is_gap: bool| {
        if is_gap {
            blocks.push(create_sub_block_with_kind(
                block,
                chunk,
                signature_end + start,
                signature_end + end,
                BlockKind::Gap,
            ));
            return;
        }

        if let Some(comment_end) = leading_comment_prefix_len(chunk) {
            let comment = &chunk[..comment_end];
            blocks.push(create_sub_block_with_kind(
                block,
                comment,
                signature_end + start,
                signature_end + start + comment_end,
                BlockKind::Comment,
            ));

            let remainder = &chunk[comment_end..];
            if !remainder.trim().is_empty() {
                blocks.push(create_sub_block_with_kind(
                    block,
                    remainder,
                    signature_end + start + comment_end,
                    signature_end + end,
                    BlockKind::CodeParagraph,
                ));
            }
            return;
        }

        let kind = classify_code_chunk(chunk);
        blocks.push(create_sub_block_with_kind(
            block,
            chunk,
            signature_end + start,
            signature_end + end,
            kind,
        ));
    };

    let re = paragraph_break_regex();
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

    Ok(blocks)
}

fn split_csharp_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse csharp type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant_any(
        root,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "struct_declaration",
        ],
    );
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = type_node.child_by_field_name("body") else {
        return split_code(block);
    };

    let items = collect_csharp_type_items(block, body);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_ruby_scope(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_ruby::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse ruby scope")?;
    let root = tree.root_node();
    let scope_node = find_named_descendant_any(root, &["module", "class"]);
    let Some(scope_node) = scope_node else {
        return split_code(block);
    };

    let items = collect_ruby_scope_items(block, scope_node);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_php_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
            function_kinds: &["function_definition", "method_declaration"],
            body_kind: "compound_statement",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_php_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_php::LANGUAGE_PHP_ONLY.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse php type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant_any(
        root,
        &[
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
            "enum_declaration",
        ],
    );
    let Some(type_node) = type_node else {
        return split_code(block);
    };
    let Some(body) = type_node.child_by_field_name("body") else {
        return split_code(block);
    };

    let items = collect_php_type_items(block, body);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_c_function(block: &Block) -> Result<Vec<Block>> {
    split_function_with_parser(
        block,
        &FunctionSplitConfig {
            language: tree_sitter_c::LANGUAGE.into(),
            function_kinds: &["function_definition"],
            body_kind: "compound_statement",
            body_statement_kind: None,
            signature_end: signature_end_offset,
            comment_kinds: &["comment"],
            trailing_delimiters: &["}"],
        },
    )
}

fn split_c_type(block: &Block) -> Result<Vec<Block>> {
    let content = block.content.as_str();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into())?;
    let tree = parser
        .parse(content, None)
        .context("Failed to parse C type")?;
    let root = tree.root_node();
    let type_node = find_named_descendant_any(
        root,
        &[
            "type_definition",
            "struct_specifier",
            "union_specifier",
            "enum_specifier",
        ],
    );
    let Some(type_node) = type_node else {
        return split_code(block);
    };

    let items = collect_c_type_items(block, type_node);
    if items.is_empty() {
        split_code(block)
    } else {
        Ok(items)
    }
}

fn split_function_with_parser(
    block: &Block,
    config: &FunctionSplitConfig<'_>,
) -> Result<Vec<Block>> {
    let mut parser = Parser::new();
    parser.set_language(&config.language)?;

    let tree = parser
        .parse(&block.content, None)
        .context("Failed to parse function block")?;
    let root = tree.root_node();
    let Some(function_node) = find_named_descendant_any(root, config.function_kinds) else {
        return split_code(block);
    };
    let Some(body_node) = find_named_descendant(function_node, config.body_kind) else {
        return split_code(block);
    };
    let split_node = config
        .body_statement_kind
        .and_then(|kind| find_named_descendant(body_node, kind))
        .unwrap_or(body_node);

    let mut blocks = Vec::new();
    let content = block.content.as_str();
    let signature_end = (config.signature_end)(content, body_node.start_byte());
    if signature_end > 0 {
        blocks.push(create_sub_block_with_kind(
            block,
            &content[..signature_end],
            0,
            signature_end,
            BlockKind::FunctionSignature,
        ));
    }

    let nodes = collect_body_nodes(split_node, config.comment_kinds);
    if nodes.is_empty() {
        return split_code(block);
    }

    let mut last_end = signature_end;
    let mut current_start: Option<usize> = None;
    let mut current_end = signature_end;
    let mut last_kind: Option<BlockKind> = None;

    for (idx, node) in nodes.iter().enumerate() {
        let start = node.start_byte();
        let gap = if start > last_end {
            &content[last_end..start]
        } else {
            ""
        };
        let gap_has_blank = paragraph_break_regex().is_match(gap);
        let gap_prefix_len = if gap_has_blank {
            gap_prefix_length(gap)
        } else {
            0
        };
        let leading_start = last_end + gap_prefix_len;

        let mut end = node.end_byte();
        if idx == nodes.len().saturating_sub(1)
            && config
                .trailing_delimiters
                .iter()
                .any(|delimiter| content[end..].trim() == *delimiter)
        {
            end = content.len();
        }

        let node_kind = if config.comment_kinds.iter().any(|kind| *kind == node.kind()) {
            BlockKind::Comment
        } else {
            BlockKind::CodeParagraph
        };

        if (gap_has_blank
            || last_kind == Some(BlockKind::Comment)
            || node_kind == BlockKind::Comment)
            && let Some(start_idx) = current_start.take()
        {
            blocks.push(create_sub_block_with_kind(
                block,
                &content[start_idx..current_end],
                start_idx,
                current_end,
                BlockKind::CodeParagraph,
            ));
        }

        if gap_prefix_len > 0 {
            let gap_prefix_end = last_end + gap_prefix_len;
            blocks.push(create_sub_block_with_kind(
                block,
                &content[last_end..gap_prefix_end],
                last_end,
                gap_prefix_end,
                BlockKind::Gap,
            ));
        }

        if node_kind == BlockKind::Comment {
            blocks.push(create_sub_block_with_kind(
                block,
                &content[leading_start..end],
                leading_start,
                end,
                node_kind,
            ));
            last_kind = Some(BlockKind::Comment);
            last_end = end;
            continue;
        }

        if current_start.is_none() || gap_has_blank || last_kind == Some(BlockKind::Comment) {
            current_start = Some(leading_start);
            current_end = end;
        } else {
            current_end = end;
        }

        last_kind = Some(BlockKind::CodeParagraph);
        last_end = end;
    }

    if let Some(start_idx) = current_start.take() {
        blocks.push(create_sub_block_with_kind(
            block,
            &content[start_idx..current_end],
            start_idx,
            current_end,
            BlockKind::CodeParagraph,
        ));
    }

    if last_end < content.len() {
        let tail = &content[last_end..];
        let kind = classify_code_chunk(tail);
        if !tail.is_empty() && kind != BlockKind::Gap {
            blocks.push(create_sub_block_with_kind(
                block,
                tail,
                last_end,
                content.len(),
                kind,
            ));
        }
    }

    Ok(blocks)
}

#[derive(Debug, Clone)]
struct MarkdownSpan {
    start: usize,
    end: usize,
    kind: BlockKind,
}

fn collect_markdown_spans(node: tree_sitter::Node<'_>, spans: &mut Vec<MarkdownSpan>) {
    if let Some(kind) = markdown_kind(node.kind()) {
        spans.push(MarkdownSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_markdown_spans(child, spans);
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

fn markdown_kind(kind: &str) -> Option<BlockKind> {
    match kind {
        "atx_heading" | "setext_heading" => Some(BlockKind::Header),
        "paragraph" => Some(BlockKind::Paragraph),
        "list_item" => Some(BlockKind::ListItem),
        "fenced_code_block" | "indented_code_block" => Some(BlockKind::CodeBlock),
        "block_quote" => Some(BlockKind::Quote),
        "thematic_break" | "html_block" | "link_reference_definition" | "table" => {
            Some(BlockKind::Element)
        }
        _ => None,
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

fn signature_end_before_body(content: &str, block_start: usize) -> usize {
    if block_start == 0 || block_start > content.len() {
        return block_start.min(content.len());
    }

    let prefix = &content[..block_start];
    prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(block_start)
}

fn find_named_descendant<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    find_named_descendant_any(node, &[kind])
}

fn find_named_descendant_any<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
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

fn collect_body_nodes<'a>(
    body_node: tree_sitter::Node<'a>,
    comment_kinds: &[&str],
) -> Vec<tree_sitter::Node<'a>> {
    let mut nodes = Vec::new();
    let mut cursor = body_node.walk();
    for child in body_node.children(&mut cursor) {
        if child.is_named() || comment_kinds.iter().any(|kind| *kind == child.kind()) {
            nodes.push(child);
        }
    }
    nodes
}

fn collect_swift_type_items(
    parent: &Block,
    body: tree_sitter::Node<'_>,
    content: &str,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    for member in swift::collect_type_member_spans(body, content) {
        let chunk = &parent.content[member.start_byte..member.end_byte];
        blocks.push(create_sub_block_with_kind(
            parent,
            chunk,
            member.start_byte,
            member.end_byte,
            member.kind,
        ));
    }

    blocks
}

fn collect_java_type_items(parent: &Block, body: tree_sitter::Node<'_>) -> Vec<Block> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|child| {
            let kind = match child.kind() {
                "field_declaration" => BlockKind::Variable,
                "constant_declaration" => BlockKind::Const,
                "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration" => BlockKind::Method,
                "class_declaration" => BlockKind::Class,
                "interface_declaration" => BlockKind::Interface,
                "enum_declaration" => BlockKind::Enum,
                "record_declaration" => BlockKind::Struct,
                "annotation_type_declaration" => BlockKind::Type,
                _ => return None,
            };
            Some(create_sub_block_with_kind(
                parent,
                &parent.content[child.start_byte()..child.end_byte()],
                child.start_byte(),
                child.end_byte(),
                kind,
            ))
        })
        .collect()
}

fn kotlin_type_body(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    first_child_of_kind(node, "class_body").or_else(|| first_child_of_kind(node, "enum_class_body"))
}

fn first_child_of_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn collect_kotlin_type_items(
    parent: &Block,
    body: tree_sitter::Node<'_>,
    content: &str,
) -> Vec<Block> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|child| {
            let kind = match child.kind() {
                "function_declaration" => {
                    if find_named_descendant(child, "function_body").is_some() {
                        BlockKind::Method
                    } else {
                        BlockKind::FunctionSignature
                    }
                }
                "property_declaration" => {
                    classify_kotlin_property_kind(&content[child.start_byte()..child.end_byte()])
                }
                "class_declaration" => classify_kotlin_class_kind(child, content),
                "object_declaration" | "companion_object" => BlockKind::Class,
                "secondary_constructor" => BlockKind::Method,
                _ => return None,
            };
            Some(create_sub_block_with_kind(
                parent,
                &parent.content[child.start_byte()..child.end_byte()],
                child.start_byte(),
                child.end_byte(),
                kind,
            ))
        })
        .collect()
}

fn collect_csharp_type_items(parent: &Block, body: tree_sitter::Node<'_>) -> Vec<Block> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|child| {
            let kind = match child.kind() {
                "field_declaration" | "property_declaration" | "event_declaration" => {
                    BlockKind::Variable
                }
                "method_declaration" | "constructor_declaration" => BlockKind::Method,
                "class_declaration" => BlockKind::Class,
                "interface_declaration" => BlockKind::Interface,
                "enum_declaration" => BlockKind::Enum,
                "record_declaration" | "struct_declaration" => BlockKind::Struct,
                _ => return None,
            };
            Some(create_sub_block_with_kind(
                parent,
                &parent.content[child.start_byte()..child.end_byte()],
                child.start_byte(),
                child.end_byte(),
                kind,
            ))
        })
        .collect()
}

fn classify_kotlin_class_kind(node: tree_sitter::Node<'_>, content: &str) -> BlockKind {
    let name_start = node
        .child_by_field_name("name")
        .map(|name| name.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let header = &content[node.start_byte()..name_start.min(node.end_byte())];

    if header.contains("interface") {
        BlockKind::Interface
    } else if header.contains("enum") {
        BlockKind::Enum
    } else {
        BlockKind::Class
    }
}

fn classify_kotlin_property_kind(text: &str) -> BlockKind {
    if text.contains("var ") {
        BlockKind::Variable
    } else {
        BlockKind::Const
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
        if code_comments::line_is_c_style_comment(trimmed) || trimmed.starts_with('#') {
            saw_comment = true;
            offset += line.len();
            continue;
        }

        return saw_comment.then_some(offset);
    }

    None
}

fn collect_ruby_scope_items(parent: &Block, scope_node: tree_sitter::Node<'_>) -> Vec<Block> {
    let Some(body) = scope_node.child_by_field_name("body") else {
        return Vec::new();
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = match child.kind() {
            "class" => BlockKind::Class,
            "module" => BlockKind::Module,
            "method" | "singleton_method" => BlockKind::Method,
            "assignment" if ruby_assignment_targets_constant(child) => BlockKind::Const,
            "call" if ruby_call_is_import(child, &parent.content) => BlockKind::Import,
            _ => continue,
        };

        blocks.push(create_sub_block_with_kind(
            parent,
            &parent.content[child.start_byte()..child.end_byte()],
            child.start_byte(),
            child.end_byte(),
            kind,
        ));

        if matches!(child.kind(), "class" | "module") {
            blocks.extend(collect_ruby_scope_items(parent, child));
        }
    }

    blocks
}

fn ruby_assignment_targets_constant(node: tree_sitter::Node<'_>) -> bool {
    let Some(left) = node.child_by_field_name("left") else {
        return false;
    };

    ruby_lhs_targets_constant(left)
}

fn ruby_lhs_targets_constant(node: tree_sitter::Node<'_>) -> bool {
    match node.kind() {
        "constant" => true,
        "scope_resolution" => node
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "constant"),
        "left_assignment_list" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .all(ruby_lhs_targets_constant)
        }
        _ => false,
    }
}

fn ruby_call_is_import(node: tree_sitter::Node<'_>, content: &str) -> bool {
    matches!(
        ruby_call_method_name(node, content).as_deref(),
        Some("require") | Some("require_relative")
    )
}

fn ruby_call_method_name(node: tree_sitter::Node<'_>, content: &str) -> Option<String> {
    node.child_by_field_name("method")
        .and_then(|method| method.utf8_text(content.as_bytes()).ok())
        .map(str::to_string)
}

fn collect_php_type_items(parent: &Block, body: tree_sitter::Node<'_>) -> Vec<Block> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|child| {
            let kind = match child.kind() {
                "const_declaration" | "enum_case" => BlockKind::Const,
                "property_declaration" => BlockKind::Variable,
                "method_declaration" => BlockKind::Method,
                "use_declaration" => BlockKind::Impl,
                _ => return None,
            };
            Some(create_sub_block_with_kind(
                parent,
                &parent.content[child.start_byte()..child.end_byte()],
                child.start_byte(),
                child.end_byte(),
                kind,
            ))
        })
        .collect()
}

fn collect_c_type_items(parent: &Block, type_node: tree_sitter::Node<'_>) -> Vec<Block> {
    let specifier = if type_node.kind() == "type_definition" {
        type_node.child_by_field_name("type").unwrap_or(type_node)
    } else {
        type_node
    };

    let Some(body) = specifier.child_by_field_name("body") else {
        return Vec::new();
    };

    let mut cursor = body.walk();
    body.children(&mut cursor)
        .filter_map(|child| {
            let kind = match child.kind() {
                "field_declaration" => BlockKind::Variable,
                "enumerator" => BlockKind::Const,
                "comment" => BlockKind::Comment,
                _ => return None,
            };

            Some(create_sub_block_with_kind(
                parent,
                &parent.content[child.start_byte()..child.end_byte()],
                child.start_byte(),
                child.end_byte(),
                kind,
            ))
        })
        .collect()
}

fn gap_prefix_length(gap: &str) -> usize {
    if gap.is_empty() {
        return 0;
    }

    gap.rfind('\n').map(|idx| idx + 1).unwrap_or(gap.len())
}

fn classify_code_chunk(chunk: &str) -> BlockKind {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return BlockKind::Gap;
    }

    if trimmed.chars().all(|ch| ch == '}' || ch == ';') {
        return BlockKind::Gap;
    }

    if code_comments::chunk_is_hash_or_c_style_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn create_sub_block_with_kind(
    parent: &Block,
    content: &str,
    start_offset: usize,
    _end_offset: usize,
    kind: BlockKind,
) -> Block {
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&c| c == '\n').count();
    let chunk_newlines = content.chars().filter(|&c| c == '\n').count();

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::Block;

    fn make_block(content: &str, kind: BlockKind) -> Block {
        make_block_with_span(content, kind, 0, content.lines().count())
    }

    fn make_block_with_span(
        content: &str,
        kind: BlockKind,
        start_line: usize,
        end_line: usize,
    ) -> Block {
        Block {
            hash: TreeHash::new("test"),
            content: content.to_string(),
            kind,
            tags: Vec::new(),
            complexity: None,
            start_line,
            end_line,
        }
    }

    fn make_large_block(content: &str, kind: BlockKind) -> Block {
        make_block_with_span(
            content,
            kind,
            0,
            crate::review_units::MAX_REVIEW_UNIT_SPAN_LINES + 8,
        )
    }

    fn merge_blocks(blocks: Vec<Block>) -> String {
        blocks.into_iter().map(|b| b.content).collect()
    }

    #[test]
    fn test_split_code_simple() {
        let content = "fn foo() {\n    print();\n}";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, BlockKind::Code);
        assert_eq!(chunks[0].content, content);
    }

    #[test]
    fn test_split_code_multiple() {
        let content = "fn foo() {\n    part1();\n\n    part2();\n}";
        let block = make_large_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();

        // "fn foo() {\n    part1();" (CodeParagraph)
        // "\n\n" (Gap)
        // "    part2();\n}" (CodeParagraph)
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);

        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_markdown() {
        let content = "# Header\n\nPara 1.\n\nPara 2.";
        let block = make_large_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Markdown).unwrap();

        // Header
        // Gap (\n\n)
        // Paragraph
        // Gap (\n\n)
        // Paragraph

        let kinds: Vec<BlockKind> = chunks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Header,
                BlockKind::Gap,
                BlockKind::Paragraph,
                BlockKind::Gap,
                BlockKind::Paragraph
            ]
        );

        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_text_sentences() {
        let content = "Line one. Line two?";
        let block = make_block(content, BlockKind::Paragraph);
        let chunks = split(&block, Language::Text).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, BlockKind::Paragraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_text_sentences_when_block_exceeds_threshold() {
        let content = "Line one. Line two?";
        let block = make_large_block(content, BlockKind::Paragraph);
        let chunks = split(&block, Language::Text).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, BlockKind::Sentence);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn forced_markdown_section_split_creates_header_and_nested_section_children_under_threshold() {
        let content =
            "# Root\nIntro paragraph.\n\n## Coding\nDetails live here.\n\n### Dev Guide\nSteps.\n";
        let block = make_block(content, BlockKind::Section);
        let result = split_result_for_child_navigation(&block, Language::Markdown).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);

        let kinds: Vec<_> = result
            .blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .map(|block| block.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![BlockKind::Header, BlockKind::Paragraph, BlockKind::Section]
        );
        assert_eq!(merge_blocks(result.blocks.clone()), content);

        let coding_section = result
            .blocks
            .into_iter()
            .find(|block| block.kind == BlockKind::Section)
            .unwrap_or_else(|| panic!("expected nested section child"));
        let nested =
            split_result_for_child_navigation(&coding_section, Language::Markdown).unwrap();
        let nested_kinds: Vec<_> = nested
            .blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .map(|block| block.kind)
            .collect();
        assert_eq!(
            nested_kinds,
            vec![BlockKind::Header, BlockKind::Paragraph, BlockKind::Section]
        );
    }

    #[test]
    fn forced_markdown_paragraph_split_creates_sentence_children_under_threshold() {
        let content = "Sentence one. Sentence two?";
        let block = make_block(content, BlockKind::Paragraph);
        let result = split_result_for_child_navigation(&block, Language::Markdown).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
        let kinds: Vec<_> = result.blocks.iter().map(|block| block.kind).collect();
        assert_eq!(kinds, vec![BlockKind::Sentence, BlockKind::Sentence]);
        assert_eq!(merge_blocks(result.blocks), content);
    }

    #[test]
    fn test_split_toml_table_into_structural_children() {
        let content = "[database]\nports = [8001, 8002]\ntargets = { primary = \"cache\", secondary = \"backup\" }\n";
        let block = make_block(content, BlockKind::Section);
        let result = split_result(&block, Language::Toml).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
        let chunks = result.blocks;
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert!(!kinds.contains(&BlockKind::Preamble));
        assert!(kinds.contains(&BlockKind::List));
        assert!(kinds.contains(&BlockKind::Section));
        assert!(
            chunks
                .iter()
                .any(|block| block.content.contains("ports = [8001, 8002]"))
        );
        assert!(
            chunks
                .iter()
                .any(|block| block.content.contains("targets = { primary = \"cache\""))
        );
    }

    #[test]
    fn test_small_toml_table_still_splits_structurally_under_threshold() {
        let content = "[owner]\nname = \"sample\"\nactive = true\n";
        let block = make_block(content, BlockKind::Section);
        let result = split_result(&block, Language::Toml).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
        let kinds: Vec<_> = result.blocks.iter().map(|block| block.kind).collect();
        assert_eq!(kinds, vec![BlockKind::Content, BlockKind::Content]);
    }

    #[test]
    fn test_small_toml_list_still_splits_structurally_under_threshold() {
        let content = "keywords = [\"blue\", \"green\"]";
        let block = make_block(content, BlockKind::List);
        let result = split_result(&block, Language::Toml).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
        let kinds: Vec<_> = result.blocks.iter().map(|block| block.kind).collect();
        assert_eq!(kinds, vec![BlockKind::Content, BlockKind::Content]);
        assert!(
            result
                .blocks
                .iter()
                .any(|block| block.content == "\"blue\"")
        );
        assert!(
            result
                .blocks
                .iter()
                .any(|block| block.content == "\"green\"")
        );
    }

    #[test]
    fn test_small_nix_variable_still_splits_structurally_under_threshold() {
        let content =
            "selected = if enabled then { system = \"linux\"; } else { system = \"other\"; };";
        let block = make_block(content, BlockKind::Variable);
        let result = split_result(&block, Language::Nix).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
        let kinds: Vec<_> = result
            .blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .map(|block| block.kind)
            .collect();
        assert!(kinds.contains(&BlockKind::Preamble));
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == BlockKind::Section)
                .count(),
            2
        );
        assert_eq!(merge_blocks(result.blocks), content);
    }

    #[test]
    fn test_small_nix_attrset_still_splits_structurally_under_threshold() {
        let content = "{ inherit name; meta = { role = \"worker\"; }; }";
        let block = make_block(content, BlockKind::Section);
        let result = split_result(&block, Language::Nix).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
        let kinds: Vec<_> = result
            .blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .map(|block| block.kind)
            .collect();
        assert!(kinds.contains(&BlockKind::Import));
        assert!(kinds.contains(&BlockKind::Variable));
        assert_eq!(merge_blocks(result.blocks), content);
    }

    #[test]
    fn test_small_nix_list_still_splits_structurally_under_threshold() {
        let content = "[ pkgs.git { name = \"helper\"; enabled = true; } ]";
        let block = make_block(content, BlockKind::List);
        let result = split_result(&block, Language::Nix).unwrap();
        assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
        let kinds: Vec<_> = result
            .blocks
            .iter()
            .filter(|block| block.kind != BlockKind::Gap)
            .map(|block| block.kind)
            .collect();
        assert!(kinds.contains(&BlockKind::Content));
        assert!(kinds.contains(&BlockKind::Section));
        assert_eq!(merge_blocks(result.blocks), content);
    }

    #[test]
    fn test_split_rust_impl_into_items() {
        let content = "impl Foo {\n    fn read_heavy(&self) {}\n    const MAX: usize = 1;\n}\n";
        let block = make_large_block(content, BlockKind::Impl);
        let chunks = split(&block, Language::Rust).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Const));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Impl));
    }

    #[test]
    fn test_split_swift_extension_into_members_when_non_trivial() {
        let content = "extension Context {\n    func fetchWorld() -> World {\n        world\n    }\n\n    func reset() async -> [UInt8] {\n        await world.transform([])\n    }\n}\n";
        let block = make_large_block(content, BlockKind::Impl);
        let chunks = split(&block, Language::Swift).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Impl));
    }

    #[test]
    fn test_split_java_class_into_members() {
        let content = "class Worker {\n    private final int scale;\n\n    Worker(int scale) {\n        this.scale = scale;\n    }\n\n    int process(int value) {\n        if (value > 0) {\n            return value * scale;\n        }\n        return 0;\n    }\n}\n";
        let block = make_large_block(content, BlockKind::Class);
        let chunks = split(&block, Language::Java).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Variable));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Class));
    }

    #[test]
    fn test_split_kotlin_class_into_members() {
        let content = "class Worker {\n    val name = \"worker\"\n    var enabled = true\n\n    fun load(id: String): Worker {\n        return this\n    }\n\n    fun reset() {\n        enabled = true\n    }\n}\n";
        let block = make_large_block(content, BlockKind::Class);
        let chunks = split(&block, Language::Kotlin).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Const));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Variable));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Class));
    }

    #[test]
    fn test_split_kotlin_function_into_review_units() {
        let content = "fun process(values: List<Int>): Int {\n    var total = 0\n\n    // accumulate positive values\n    for (value in values) {\n        if (value > 0) {\n            total += value\n        }\n    }\n\n    return total\n}\n";
        let block = make_large_block(content, BlockKind::Function);
        let chunks = split(&block, Language::Kotlin).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::Comment,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
            ]
        );
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_java_method_into_review_units() {
        let content = "int process(int value) {\n    int total = value;\n\n    // only positive values count\n    if (value > 0) {\n        total += scale;\n    }\n\n    return total;\n}\n";
        let block = make_large_block(content, BlockKind::Method);
        let chunks = split(&block, Language::Java).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::Comment,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
            ]
        );
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_csharp_class_into_members() {
        let content = "public class Greeter {\n    public string Name { get; }\n\n    public WorkflowStatus Status { get; private set; }\n\n    public Greeter(string name) {\n        Name = name;\n    }\n\n    public GreetingResult BuildGreeting(string target) {\n        return new GreetingResult(target, 1);\n    }\n}\n";
        let block = make_large_block(content, BlockKind::Class);
        let chunks = split(&block, Language::CSharp).unwrap();
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Variable));
        assert!(chunks.iter().any(|b| b.kind == BlockKind::Method));
        assert!(!chunks.iter().any(|b| b.kind == BlockKind::Class));
    }

    #[test]
    fn test_split_csharp_method_into_review_units() {
        let content = "public GreetingResult BuildGreeting(string target) {\n    var parts = new List<string>();\n\n    // normalize the target before storing it\n    if (target.Length > 0) {\n        parts.Add(target.ToUpperInvariant());\n    }\n\n    return new GreetingResult(string.Join(\",\", parts), parts.Count);\n}\n";
        let block = make_large_block(content, BlockKind::Method);
        let chunks = split(&block, Language::CSharp).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::Comment,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
            ]
        );
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_ruby_method_into_review_units() {
        let content = "def process(values)\n  output = []\n\n  # Preserve only meaningful slices.\n  values.each do |value|\n    output << value * SCALE\n  end\n\n  Formatting.render(output)\nend\n";
        let block = make_large_block(content, BlockKind::Method);
        let chunks = split(&block, Language::Ruby).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::Comment,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
            ]
        );
        assert_eq!(merge_blocks(chunks.clone()), content);
        assert!(chunks.iter().all(|chunk| chunk.complexity.is_none()));
    }

    #[test]
    fn test_split_ruby_module_into_members() {
        let content = "module Trueflow\n  DEFAULT_LIMIT = 4\n\n  module Formatting\n    def self.render(values)\n      values.join(\",\")\n    end\n  end\n\n  class Processor\n    SCALE = 2\n\n    def process(values)\n      values.map { |value| value * SCALE }\n    end\n  end\nend\n";
        let block = make_large_block(content, BlockKind::Module);
        let chunks = split(&block, Language::Ruby).unwrap();
        assert!(chunks.iter().any(|block| block.kind == BlockKind::Const));
        assert!(chunks.iter().any(|block| block.kind == BlockKind::Module));
        assert!(chunks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(
            !chunks
                .iter()
                .any(|block| block.kind == BlockKind::Module && block.content == content)
        );
    }

    #[test]
    fn test_split_nix_paragraphs_preserve_content() {
        let content = "{ foo = \"bar\"; }\n\n{ baz = \"qux\"; }";
        let block = make_large_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Nix).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_just_paragraphs_preserve_content() {
        let content = "build:\n\techo ok\n\ntest:\n\techo ok";
        let block = make_large_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Just).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(chunks[1].kind, BlockKind::Gap);
        assert_eq!(chunks[2].kind, BlockKind::CodeParagraph);
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_split_elisp_function_into_review_units() {
        let content = "(defun elisp-support-run (items)\n  \"Normalize ITEMS and report the active entries.\"\n  (let ((normalized (seq-filter #'identity items))\n        (results nil))\n    ;; keep only truthy values\n    (dolist (item normalized)\n      (push (string-trim item) results))\n\n    (when elisp-support-enabled\n      (message \"%s\" elisp-support-mode-name))\n\n    (nreverse results)))\n";
        let block = make_large_block(content, BlockKind::Function);
        let chunks = split(&block, Language::Elisp).unwrap();
        let kinds: Vec<_> = chunks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::FunctionSignature,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
                BlockKind::Gap,
                BlockKind::CodeParagraph,
            ]
        );
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_round_trip_code() {
        let content = "A\n\nB\n\nC";
        let block = make_block(content, BlockKind::Code);
        let chunks = split(&block, Language::Rust).unwrap();
        assert_eq!(merge_blocks(chunks), content);
    }

    #[test]
    fn test_sub_blocks_do_not_inherit_parent_complexity() {
        let content = "fn foo() {\n    if true {\n        run();\n    }\n\n    finish();\n}";
        let mut block = make_large_block(content, BlockKind::Function);
        block.complexity = Some(7);

        let chunks = split(&block, Language::Rust).unwrap();

        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.complexity.is_none()));
    }
}
