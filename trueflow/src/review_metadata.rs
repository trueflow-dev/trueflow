use crate::block::BlockKind;
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
use std::collections::HashMap;

pub fn block_breadcrumb(tree: &Tree, node_id: TreeNodeId) -> Option<String> {
    if !matches!(tree.node(node_id).kind, TreeNodeKind::Block) {
        return None;
    }

    let mut ancestors = tree.ancestors(node_id);
    ancestors.reverse();

    let mut parts = Vec::new();
    let mut file_path = None;
    let mut container_parts = Vec::new();
    let mut current = None;

    for ancestor_id in ancestors {
        let ancestor = tree.node(ancestor_id);
        match ancestor.kind {
            TreeNodeKind::File => {
                if !ancestor.path.is_root() {
                    file_path = Some(ancestor.path.clone());
                }
            }
            TreeNodeKind::Block => {
                let Some(block) = ancestor.block.as_ref() else {
                    continue;
                };
                let label = block_breadcrumb_label(block);
                if label.is_empty() {
                    continue;
                }
                if tree.is_container_block(ancestor_id) {
                    container_parts.push(label.clone());
                }
                if ancestor.id == node_id {
                    current = Some(label);
                }
            }
            _ => {}
        }
    }

    if let Some(path) = file_path {
        parts.push(format!("File ({path})"));
    }
    parts.extend(container_parts);
    if let Some(current) = current
        && parts.last().is_none_or(|part| part != &current)
    {
        parts.push(current);
    }

    if parts.len() > 1 {
        Some(parts.join(" -> "))
    } else {
        None
    }
}

pub fn sorted_visible_block_kind_counts(
    tree: &Tree,
    visible_nodes: &std::collections::HashSet<TreeNodeId>,
) -> Vec<(BlockKind, usize)> {
    let mut counts = HashMap::new();
    for id in visible_nodes {
        let node = tree.node(*id);
        if node.kind != TreeNodeKind::Block {
            continue;
        }
        let Some(block) = &node.block else {
            continue;
        };
        *counts.entry(block.kind).or_insert(0) += 1;
    }

    let mut kind_counts = counts.into_iter().collect::<Vec<_>>();
    kind_counts.sort_by(|a, b| {
        let parent_a = parent_kind_label(a.0);
        let parent_b = parent_kind_label(b.0);
        if parent_a != parent_b {
            parent_a.cmp(parent_b)
        } else {
            b.0.as_str().cmp(a.0.as_str())
        }
    });
    kind_counts
}

pub fn parent_kind_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Function
        | BlockKind::Method
        | BlockKind::FunctionSignature
        | BlockKind::CodeParagraph => "Code Logic",
        BlockKind::Struct
        | BlockKind::Enum
        | BlockKind::Class
        | BlockKind::Impl
        | BlockKind::Macro
        | BlockKind::Const
        | BlockKind::Static
        | BlockKind::Type => "Definitions",
        BlockKind::Module
        | BlockKind::Modules
        | BlockKind::Import
        | BlockKind::Imports
        | BlockKind::Export
        | BlockKind::Preamble => "Module Structure",
        BlockKind::Comment
        | BlockKind::TextBlock
        | BlockKind::Paragraph
        | BlockKind::ListItem
        | BlockKind::Header
        | BlockKind::Quote
        | BlockKind::Section => "Documentation",
        _ => "Other",
    }
}

fn block_breadcrumb_label(block: &crate::block::Block) -> String {
    let kind_label = humanize_block_kind(block.kind);
    match block_identifier(block) {
        Some(identifier) => format!("{kind_label} ({identifier})"),
        None => kind_label,
    }
}

fn block_identifier(block: &crate::block::Block) -> Option<String> {
    let line = first_meaningful_line(block)?;
    match block.kind {
        BlockKind::Function | BlockKind::Method | BlockKind::FunctionSignature => {
            extract_callable_name(line)
        }
        BlockKind::Struct => extract_named_declaration(line, &["struct"]),
        BlockKind::Enum => extract_named_declaration(line, &["enum"]),
        BlockKind::Class => extract_named_declaration(line, &["class"]),
        BlockKind::Interface => extract_named_declaration(line, &["interface", "protocol"]),
        BlockKind::Type => extract_named_declaration(line, &["type", "typealias"]),
        BlockKind::Module => extract_named_declaration(line, &["module", "mod", "namespace"]),
        BlockKind::Const => extract_named_declaration(line, &["const"]),
        BlockKind::Static => extract_named_declaration(line, &["static"]),
        BlockKind::Variable => extract_named_declaration(line, &["let", "var", "val", "const"]),
        BlockKind::Macro => extract_macro_name(line),
        BlockKind::Impl => extract_impl_target(line),
        _ => None,
    }
}

pub(crate) fn semantic_block_identifier(block: &crate::block::Block) -> Option<String> {
    block_identifier(block)
}

fn first_meaningful_line(block: &crate::block::Block) -> Option<&str> {
    let mut non_empty_lines = block
        .content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if should_skip_attribute_lines(block.kind) {
        non_empty_lines
            .find(|line| !line.starts_with("#["))
            .or_else(|| non_empty_lines.next())
    } else {
        non_empty_lines.next()
    }
}

fn humanize_block_kind(kind: BlockKind) -> String {
    let raw = kind.as_str();
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_was_lower_or_digit = false;

    for ch in raw.chars() {
        if matches!(ch, '_' | '-' | ' ') {
            if !current.is_empty() {
                words.push(current);
                current = String::new();
            }
            previous_was_lower_or_digit = false;
            continue;
        }

        if ch.is_uppercase() && previous_was_lower_or_digit && !current.is_empty() {
            words.push(current);
            current = String::new();
        }

        current.push(ch);
        previous_was_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
        .into_iter()
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            let mut label = first.to_uppercase().collect::<String>();
            label.push_str(&chars.as_str().to_ascii_lowercase());
            label
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn should_skip_attribute_lines(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Struct
            | BlockKind::Enum
            | BlockKind::Class
            | BlockKind::Function
            | BlockKind::Method
            | BlockKind::FunctionSignature
            | BlockKind::Const
            | BlockKind::Static
            | BlockKind::Type
            | BlockKind::Impl
            | BlockKind::Interface
    )
}

fn extract_callable_name(text: &str) -> Option<String> {
    let before_arguments = &text[..find_argument_list_start(text)?];
    trailing_identifier(before_arguments)
}

fn extract_named_declaration(text: &str, keywords: &[&str]) -> Option<String> {
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        if keywords
            .iter()
            .any(|keyword| token.eq_ignore_ascii_case(keyword))
        {
            let mut cursor = index + 1;
            while let Some(next) = tokens.get(cursor) {
                if is_inline_rust_attribute_token(next) {
                    cursor += 1;
                    while tokens
                        .get(cursor.saturating_sub(1))
                        .is_some_and(|token| !token.contains(']'))
                    {
                        cursor += 1;
                    }
                    continue;
                }
                if let Some(identifier) = leading_identifier(next) {
                    return Some(identifier);
                }
                return None;
            }
        }
    }
    None
}

fn is_inline_rust_attribute_token(token: &str) -> bool {
    token.starts_with("#[")
}

fn extract_macro_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("macro_rules!") {
        return leading_identifier(rest);
    }
    extract_named_declaration(trimmed, &["macro"])
}

fn extract_impl_target(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let impl_index = trimmed.find("impl")?;
    let mut rest = trimmed[impl_index + "impl".len()..].trim_start();
    rest = strip_leading_generic_group(rest);
    if let Some((_, after_for)) = rest.rsplit_once(" for ") {
        rest = after_for.trim();
    }
    let target = rest.split(" where ").next().unwrap_or(rest).trim();
    trailing_identifier(target)
}

fn strip_leading_generic_group(text: &str) -> &str {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('<') {
        return trimmed;
    }
    let Some(end) = balanced_group_end(trimmed, '<', '>') else {
        return trimmed;
    };
    trimmed[end + 1..].trim_start()
}

fn balanced_group_end(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn leading_identifier(text: &str) -> Option<String> {
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if is_identifier_start(ch) {
            start = Some(index);
            break;
        }
    }
    let start = start?;
    let mut end = text.len();
    for (offset, ch) in text[start..].char_indices() {
        if !is_identifier_char(ch) {
            end = start + offset;
            break;
        }
    }
    Some(text[start..end].to_string())
}

fn trailing_identifier(text: &str) -> Option<String> {
    let trimmed = strip_trailing_generic_groups(text.trim_end_matches('{').trim());
    let mut end = None;

    for (index, ch) in trimmed.char_indices().rev() {
        if end.is_none() {
            if is_identifier_char(ch) {
                end = Some(index + ch.len_utf8());
            }
            continue;
        }

        if !is_identifier_char(ch) {
            let start = index + ch.len_utf8();
            let candidate = &trimmed[start..end?];
            return is_identifier_start(candidate.chars().next()?).then(|| candidate.to_string());
        }
    }

    let end = end?;
    let candidate = &trimmed[..end];
    is_identifier_start(candidate.chars().next()?).then(|| candidate.to_string())
}

fn strip_trailing_generic_groups(text: &str) -> &str {
    let mut trimmed = text.trim_end();
    loop {
        if !trimmed.ends_with('>') {
            return trimmed;
        }
        let Some(start) = trailing_balanced_group_start(trimmed, '<', '>') else {
            return trimmed;
        };
        trimmed = trimmed[..start].trim_end();
    }
}

fn trailing_balanced_group_start(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut saw_close = false;
    for (index, ch) in text.char_indices().rev() {
        if ch == close {
            depth += 1;
            saw_close = true;
        } else if ch == open {
            depth = depth.saturating_sub(1);
            if saw_close && depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_identifier_char(ch: char) -> bool {
    is_identifier_start(ch) || ch.is_ascii_digit()
}

fn find_argument_list_start(text: &str) -> Option<usize> {
    let mut depth = 0;
    for (index, c) in text.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            '(' if depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, ByteSpan, LineSpan};
    use crate::tree::TreeBuilder;
    use std::collections::HashSet;

    fn block(content: &str, kind: BlockKind, start: usize) -> Block {
        let line_span = LineSpan::new(start, start + content.lines().count());
        let byte_span = ByteSpan::new(0, content.len());
        Block::new(content.to_string(), kind, line_span, byte_span)
    }

    #[test]
    fn breadcrumb_includes_file_impl_and_current_block() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let impl_block = builder.add_block(
            file,
            "impl".to_string(),
            "src/lib.rs".to_string(),
            block("impl Foo {", BlockKind::Impl, 1),
            Language::Rust,
        );
        let method = builder.add_block(
            impl_block,
            "method".to_string(),
            "src/lib.rs".to_string(),
            block("fn bar(&self, x: i32) -> i32 {", BlockKind::Method, 3),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, method);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> Impl (Foo) -> Method (bar)".to_string())
        );
    }

    #[test]
    fn breadcrumb_for_impl_block_does_not_duplicate_current_label() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let impl_block = builder.add_block(
            file,
            "impl".to_string(),
            "src/lib.rs".to_string(),
            block("impl Foo {", BlockKind::Impl, 1),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, impl_block);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> Impl (Foo)".to_string())
        );
    }

    #[test]
    fn breadcrumb_struct_prefers_type_name_over_attribute_line() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let struct_block = builder.add_block(
            file,
            "struct".to_string(),
            "src/lib.rs".to_string(),
            block(
                "#[derive(Debug, Clone)]\nstruct Config {\n    name: String,\n}\n",
                BlockKind::Struct,
                1,
            ),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, struct_block);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> Struct (Config)".to_string())
        );
    }

    #[test]
    fn semantic_block_identifier_skips_inline_rust_attribute_after_declaration_keyword() {
        let block = block(
            "struct #[derive(Debug, Clone)] Config {\n    name: String,\n}\n",
            BlockKind::Struct,
            1,
        );

        assert_eq!(
            semantic_block_identifier(&block),
            Some("Config".to_string())
        );
    }

    #[test]
    fn breadcrumb_uses_fixed_semantic_label_for_comment_children() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let function = builder.add_block(
            file,
            "function".to_string(),
            "src/lib.rs".to_string(),
            block("fn build_state() {", BlockKind::Function, 1),
            Language::Rust,
        );
        let comment = builder.add_block(
            function,
            "comment".to_string(),
            "src/lib.rs".to_string(),
            block(
                "// this breadcrumb should not include comment text",
                BlockKind::Comment,
                2,
            ),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, comment);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> Function (build_state) -> Comment".to_string())
        );
    }

    #[test]
    fn breadcrumb_uses_fixed_semantic_label_for_paragraph_blocks() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let docs = builder.add_dir(root, "docs".to_string(), "docs".to_string());
        let file = builder.add_file(
            docs,
            "guide.md".to_string(),
            "docs/guide.md".to_string(),
            "file-hash".to_string(),
            Language::Markdown,
        );
        let paragraph = builder.add_block(
            file,
            "paragraph".to_string(),
            "docs/guide.md".to_string(),
            block(
                "This paragraph should not be copied into breadcrumbs.",
                BlockKind::Paragraph,
                1,
            ),
            Language::Markdown,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, paragraph);
        assert_eq!(
            breadcrumb,
            Some("File (docs/guide.md) -> Paragraph".to_string())
        );
    }

    #[test]
    fn breadcrumb_returns_none_for_non_block_nodes() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let tree = builder.finalize();

        assert!(block_breadcrumb(&tree, root).is_none());
        assert!(block_breadcrumb(&tree, src).is_none());
    }

    #[test]
    fn visible_block_kind_counts_are_sorted_by_group_then_kind() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let function = builder.add_block(
            file,
            "function".to_string(),
            "src/lib.rs".to_string(),
            block("fn f() {}", BlockKind::Function, 1),
            Language::Rust,
        );
        let struct_block = builder.add_block(
            file,
            "struct".to_string(),
            "src/lib.rs".to_string(),
            block("struct S {}", BlockKind::Struct, 5),
            Language::Rust,
        );
        let enum_block = builder.add_block(
            file,
            "enum".to_string(),
            "src/lib.rs".to_string(),
            block("enum E {}", BlockKind::Enum, 10),
            Language::Rust,
        );
        let tree = builder.finalize();

        let visible = HashSet::from([root, src, file, function, struct_block, enum_block]);
        let counts = sorted_visible_block_kind_counts(&tree, &visible);

        assert_eq!(counts.len(), 3);
        assert_eq!(counts[0], (BlockKind::Function, 1));
        assert_eq!(counts[1], (BlockKind::Struct, 1));
        assert_eq!(counts[2], (BlockKind::Enum, 1));
    }
}
