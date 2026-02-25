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
    let mut impl_parts = Vec::new();
    let mut current = None;

    for ancestor_id in ancestors {
        let ancestor = tree.node(ancestor_id);
        match ancestor.kind {
            TreeNodeKind::File => {
                if !ancestor.path.is_empty() {
                    file_path = Some(ancestor.path.clone());
                }
            }
            TreeNodeKind::Block => {
                let Some(block) = ancestor.block.as_ref() else {
                    continue;
                };
                let label = block_signature(block);
                if label.is_empty() {
                    continue;
                }
                if matches!(block.kind, BlockKind::Impl | BlockKind::Interface) {
                    impl_parts.push(label.clone());
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
    parts.extend(impl_parts);
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

fn block_signature(block: &crate::block::Block) -> String {
    let mut non_empty_lines = block
        .content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let line = if should_skip_attribute_lines(block.kind) {
        non_empty_lines
            .find(|line| !line.starts_with("#["))
            .or_else(|| non_empty_lines.next())
    } else {
        non_empty_lines.next()
    };
    let Some(line) = line else {
        return block.kind.as_str().to_string();
    };
    let mut text = line.trim_end_matches('{').trim().to_string();

    if matches!(
        block.kind,
        BlockKind::Function | BlockKind::Method | BlockKind::FunctionSignature
    ) && let Some(idx) = find_argument_list_start(&text)
    {
        text.truncate(idx);
    }

    truncate_text(text.trim(), 72)
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

fn truncate_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if max_chars == 0 || trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let cutoff = max_chars.saturating_sub(3).max(1);
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= cutoff {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::Block;
    use crate::tree::TreeBuilder;
    use std::collections::HashSet;

    fn block(content: &str, kind: BlockKind, start: usize, end: usize) -> Block {
        Block::new(content.to_string(), kind, start, end)
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
            block("impl Foo {", BlockKind::Impl, 1, 40),
            Language::Rust,
        );
        let method = builder.add_block(
            impl_block,
            "method".to_string(),
            "src/lib.rs".to_string(),
            block("fn bar(&self, x: i32) -> i32 {", BlockKind::Method, 3, 10),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, method);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> impl Foo -> fn bar".to_string())
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
            block("impl Foo {", BlockKind::Impl, 1, 40),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, impl_block);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> impl Foo".to_string())
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
                6,
            ),
            Language::Rust,
        );
        let tree = builder.finalize();

        let breadcrumb = block_breadcrumb(&tree, struct_block);
        assert_eq!(
            breadcrumb,
            Some("File (src/lib.rs) -> struct Config".to_string())
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
            block("fn f() {}", BlockKind::Function, 1, 2),
            Language::Rust,
        );
        let struct_block = builder.add_block(
            file,
            "struct".to_string(),
            "src/lib.rs".to_string(),
            block("struct S {}", BlockKind::Struct, 5, 8),
            Language::Rust,
        );
        let enum_block = builder.add_block(
            file,
            "enum".to_string(),
            "src/lib.rs".to_string(),
            block("enum E {}", BlockKind::Enum, 10, 14),
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
