use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::complexity;
use crate::hashing::TreeHash;
use crate::text_split::split_by_paragraph_breaks;
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

pub fn split(content: &str, lang: Language) -> Result<Vec<Block>> {
    info!(
        "block_splitter start (lang={:?}, bytes={})",
        lang,
        content.len()
    );
    match lang {
        Language::Markdown => {
            let blocks = split_markdown(content)?;
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        Language::Go => {
            let blocks = split_go(content);
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        Language::Cpp => {
            let blocks = split_cpp(content);
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        Language::Nix => {
            let blocks = split_nix(content)?;
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        _ if lang.uses_text_fallback() => {
            let blocks = split_paragraphs(content, lang);
            info!("block_splitter done (blocks={})", blocks.len());
            return Ok(blocks);
        }
        _ => {}
    }

    let mut parser = Parser::new();

    // Select grammar based on language
    let language = match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    };

    let Some(language) = language else {
        info!("block_splitter unsupported language, returning empty blocks");
        info!("block_splitter done (blocks=0)");
        return Ok(Vec::new());
    };

    parser.set_language(&language)?;

    let tree = parser
        .parse(content, None)
        .context("Failed to parse source with tree-sitter")?;
    let root = tree.root_node();
    let mut blocks = Vec::new();

    let mut cursor = root.walk();
    let mut last_end_byte = 0;

    let test_ranges = collect_test_ranges(lang, &tree, content)?;

    // State for pending attributes/comments that should be attached to the next node
    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    // Iterate over children of root
    for child in root.children(&mut cursor) {
        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        let ts_kind = child.kind();
        let is_test = is_test_span(&test_ranges, ByteSpan::new(start_byte, end_byte));

        // Check if this node is an attribute or comment that should be grouped
        let is_attribute = match lang {
            Language::Rust => {
                ts_kind == "attribute_item"
                    || ts_kind == "line_comment"
                    || ts_kind == "block_comment"
            }
            Language::Python => ts_kind == "decorator",
            _ => false,
        };

        if is_attribute {
            if pending_start.is_none() {
                // First attribute in a potential group. Handle gap prior to it.
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

        // It is a "real" item

        // Determine the actual start byte for this block (including pending attributes)
        let block_start = if let Some(ps) = pending_start {
            ps
        } else {
            // No pending attributes, handle gap now
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
        let mut block = create_block(
            node_content,
            map_kind(lang, ts_kind),
            content,
            block_start,
            end_byte,
            lang,
        );
        if is_test {
            block.tags.push("test".to_string());
        }
        blocks.push(block);

        if matches!(lang, Language::Rust) && matches!(ts_kind, "impl_item" | "trait_item") {
            blocks.extend(collect_rust_impl_items(child, content, lang, &test_ranges));
        }

        last_end_byte = end_byte;
        pending_start = None;
        pending_end = 0;
    }

    // If we have pending attributes left at the end (e.g. trailing comments or attribute at EOF)
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

    // Trailing gap
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

    info!("block_splitter done (blocks={})", blocks.len());
    Ok(blocks)
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
    trimmed_line.starts_with("//")
        || trimmed_line.starts_with("/*")
        || trimmed_line.starts_with('*')
        || trimmed_line.starts_with("*/")
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

fn map_rust_impl_child_kind(kind: &str) -> Option<BlockKind> {
    match kind {
        "function_item" => Some(BlockKind::Method),
        "function_signature_item" => Some(BlockKind::FunctionSignature),
        "const_item" => Some(BlockKind::Const),
        "static_item" => Some(BlockKind::Static),
        "type_item" | "associated_type" => Some(BlockKind::Type),
        "macro_invocation" | "macro_definition" => Some(BlockKind::Macro),
        _ => None,
    }
}

fn is_rust_attribute_node(kind: &str) -> bool {
    matches!(
        kind,
        "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment"
    )
}

fn collect_rust_impl_items(
    impl_node: tree_sitter::Node<'_>,
    content: &str,
    lang: Language,
    test_ranges: &[ByteSpan],
) -> Vec<Block> {
    let Some(body) = impl_node.child_by_field_name("body") else {
        return Vec::new();
    };

    let mut blocks = Vec::new();
    let mut cursor = body.walk();
    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    for child in body.children(&mut cursor) {
        let ts_kind = child.kind();
        if matches!(ts_kind, "{" | "}") {
            continue;
        }
        let start_byte = child.start_byte();
        let end_byte = child.end_byte();

        if is_rust_attribute_node(ts_kind) {
            if pending_start.is_none() {
                pending_start = Some(start_byte);
            }
            pending_end = end_byte;
            continue;
        }

        let Some(kind) = map_rust_impl_child_kind(ts_kind) else {
            continue;
        };

        let block_start = pending_start.unwrap_or(start_byte);
        let node_content = &content[block_start..end_byte];
        let mut block = create_block(node_content, kind, content, block_start, end_byte, lang);
        let is_test = is_test_span(test_ranges, ByteSpan::new(start_byte, end_byte));
        if is_test {
            block.tags.push("test".to_string());
        }
        blocks.push(block);

        pending_start = None;
        pending_end = 0;
    }

    if let Some(start) = pending_start {
        let end = pending_end.max(start);
        if end > start {
            let chunk = &content[start..end];
            let block = create_block(chunk, BlockKind::Code, content, start, end, lang);
            blocks.push(block);
        }
    }

    blocks
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

fn is_test_span(ranges: &[ByteSpan], block_span: ByteSpan) -> bool {
    ranges.iter().any(|range| range.overlaps(&block_span))
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

    #[test]
    fn test_rust_test_detection() {
        let content = "#[test]
fn test_foo() {}
";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(!blocks.is_empty());
        let test_block = blocks.iter().find(|b| b.content.contains("fn test_foo"));
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
        let blocks = split(content, Language::Rust).unwrap();
        assert!(!blocks.is_empty());
        let module_block = blocks.iter().find(|b| b.content.contains("mod tests"));
        assert!(module_block.is_some());
        assert!(module_block.unwrap().tags.contains(&"test".to_string()));
    }

    fn assert_paragraph_split(language: Language) {
        let content = "Para 1.\n\nPara 2.";
        let blocks = split(content, language).unwrap();
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
        let content = "# Section 1\nText.\n# Section 2\nMore text.";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Section);
        assert_eq!(blocks[0].content, "# Section 1\nText.\n");
        assert_eq!(blocks[1].kind, BlockKind::Section);
        assert_eq!(blocks[1].content, "# Section 2\nMore text.");
    }

    #[test]
    fn test_split_markdown_hierarchy() {
        let content = "# Root\n## Sub\n### SubSub\n# Root 2";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 2);
        // First block contains Root, Sub, SubSub
        assert_eq!(blocks[0].content, "# Root\n## Sub\n### SubSub\n");
        // Second block contains Root 2
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
        let blocks = split(content, Language::Nix).unwrap();
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
        let blocks = split(content, Language::Nix).unwrap();
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
        let blocks = split(content, Language::Nix).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_nix_comment_only_file_is_comment_block() {
        let content = "# comment only\n# still comment\n";
        let blocks = split(content, Language::Nix).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Comment);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_just_paragraphs() {
        assert_paragraph_split(Language::Just);
    }

    #[test]
    fn test_split_rust_simple() {
        let content = "fn foo() {}\n\nstruct Bar;";
        let blocks = split(content, Language::Rust).unwrap();
        // Tree-sitter splitting is complex but should return items
        assert!(!blocks.is_empty());
    }

    #[test]
    fn test_split_go_simple_maps_import_struct_and_function() {
        let content = "package main\n\nimport \"fmt\"\n\ntype Worker struct{}\n\nfunc run() {\n    fmt.Println(\"ok\")\n}\n";
        let blocks = split(content, Language::Go).unwrap();
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Struct));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Function));
    }

    #[test]
    fn test_split_cpp_simple_maps_import_class_and_function() {
        let content = "#include <vector>\n\nclass Worker {\npublic:\n    int value = 1;\n};\n\nint run() {\n    return 1;\n}\n";
        let blocks = split(content, Language::Cpp).unwrap();
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Import));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Class));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Function));
    }

    #[test]
    fn test_block_hashes_match_content_rust() {
        let content = "use std::fmt;\n\nfn foo() {}\n";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(!blocks.is_empty());
        assert_block_hashes_match(&blocks);
        assert!(!blocks.iter().any(|block| block.kind == BlockKind::Gap));
    }

    #[test]
    fn test_block_hashes_match_content_markdown() {
        let content = "# Title\nParagraph text.\n";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block_hashes_match(&blocks);
        assert_eq!(blocks[0].content, content);
    }

    #[test]
    fn test_split_rust_impl_methods() {
        let content = "struct Foo;\n\nimpl Foo {\n    fn read_heavy(&self) {}\n    const MAX: usize = 1;\n}\n";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Impl));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Method));
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Const));
    }

    #[test]
    fn test_split_rust_top_level_static_maps_to_static_kind() {
        let content = "static MAX: usize = 1;\n";
        let blocks = split(content, Language::Rust).unwrap();
        assert!(blocks.iter().any(|block| block.kind == BlockKind::Static));
    }

    #[test]
    fn test_markdown_discards_whitespace_only_preamble() {
        let content = "\n\n# Title\nBody";
        let blocks = split(content, Language::Markdown).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "# Title\nBody");
    }
}
