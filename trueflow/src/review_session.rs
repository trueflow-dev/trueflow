use crate::block::BlockKind;
use crate::review_order::ReviewOrder;
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
use std::collections::HashSet;

pub fn action_block_ids(
    tree: &Tree,
    visible_nodes: &HashSet<TreeNodeId>,
    node_id: TreeNodeId,
) -> Vec<TreeNodeId> {
    let node = tree.node(node_id);
    match node.kind {
        TreeNodeKind::Block => {
            if is_impl_or_interface_block(tree, node_id) {
                block_ids_in_visible_subtree(tree, visible_nodes, node_id)
            } else {
                vec![node_id]
            }
        }
        _ => block_ids_in_visible_subtree(tree, visible_nodes, node_id),
    }
}

pub fn next_review_target(
    tree: &Tree,
    visible_nodes: &HashSet<TreeNodeId>,
    review_order: &ReviewOrder,
    remaining_reviewable: &HashSet<TreeNodeId>,
    node_id: TreeNodeId,
) -> Option<TreeNodeId> {
    let node = tree.node(node_id);
    match node.kind {
        TreeNodeKind::Block => {
            if is_impl_or_interface_block(tree, node_id) {
                let subtree_blocks = block_ids_in_visible_subtree(tree, visible_nodes, node_id)
                    .into_iter()
                    .collect::<HashSet<_>>();
                review_order.next_after_subtree(&subtree_blocks, remaining_reviewable)
            } else {
                review_order.next_after_blocks(node_id, remaining_reviewable)
            }
        }
        _ => {
            let subtree_blocks = block_ids_in_visible_subtree(tree, visible_nodes, node_id)
                .into_iter()
                .collect::<HashSet<_>>();
            review_order.next_after_subtree(&subtree_blocks, remaining_reviewable)
        }
    }
}

pub fn prune_visible_nodes(
    tree: &Tree,
    visible_nodes: &HashSet<TreeNodeId>,
) -> HashSet<TreeNodeId> {
    let mut pruned = HashSet::new();
    for node_id in visible_nodes
        .iter()
        .copied()
        .filter(|id| matches!(tree.node(*id).kind, TreeNodeKind::Block))
    {
        for ancestor in tree.ancestors(node_id) {
            pruned.insert(ancestor);
        }
    }

    pruned.insert(tree.root());
    pruned
}

fn block_ids_in_visible_subtree(
    tree: &Tree,
    visible_nodes: &HashSet<TreeNodeId>,
    root: TreeNodeId,
) -> Vec<TreeNodeId> {
    let mut stack = vec![root];
    let mut blocks = Vec::new();
    while let Some(node_id) = stack.pop() {
        if !visible_nodes.contains(&node_id) {
            continue;
        }
        let node = tree.node(node_id);
        if matches!(node.kind, TreeNodeKind::Block) {
            blocks.push(node_id);
        }
        for child in &node.children {
            stack.push(*child);
        }
    }
    blocks
}

fn is_impl_or_interface_block(tree: &Tree, node_id: TreeNodeId) -> bool {
    tree.node(node_id)
        .block
        .as_ref()
        .is_some_and(|block| matches!(block.kind, BlockKind::Impl | BlockKind::Interface))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind};
    use crate::tree::TreeBuilder;

    fn test_block(kind: BlockKind, start: usize, end: usize) -> Block {
        Block::new(format!("{kind:?}-{start}"), kind, start, end)
    }

    fn visible_nodes_from_blocks(tree: &Tree, block_ids: &[TreeNodeId]) -> HashSet<TreeNodeId> {
        let mut visible = HashSet::new();
        for block_id in block_ids {
            for ancestor in tree.ancestors(*block_id) {
                visible.insert(ancestor);
            }
        }
        visible.insert(tree.root());
        visible
    }

    #[test]
    fn action_block_ids_for_non_impl_block_returns_only_current() {
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
            test_block(BlockKind::Function, 10, 20),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = visible_nodes_from_blocks(&tree, &[function]);

        let selected = action_block_ids(&tree, &visible, function);
        assert_eq!(selected, vec![function]);
    }

    #[test]
    fn action_block_ids_for_impl_block_includes_descendants() {
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
            test_block(BlockKind::Impl, 1, 50),
            Language::Rust,
        );
        let method_a = builder.add_block(
            impl_block,
            "method_a".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 3, 10),
            Language::Rust,
        );
        let method_b = builder.add_block(
            impl_block,
            "method_b".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 20, 28),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = visible_nodes_from_blocks(&tree, &[impl_block, method_a, method_b]);

        let selected = action_block_ids(&tree, &visible, impl_block)
            .into_iter()
            .collect::<HashSet<_>>();
        let expected = HashSet::from([impl_block, method_a, method_b]);
        assert_eq!(selected, expected);
    }

    #[test]
    fn action_block_ids_for_file_includes_all_visible_descendant_blocks() {
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
        let first = builder.add_block(
            file,
            "first".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 1, 3),
            Language::Rust,
        );
        let second = builder.add_block(
            file,
            "second".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 10, 12),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = visible_nodes_from_blocks(&tree, &[first, second]);

        let selected = action_block_ids(&tree, &visible, file)
            .into_iter()
            .collect::<HashSet<_>>();
        let expected = HashSet::from([first, second]);
        assert_eq!(selected, expected);
    }

    #[test]
    fn next_review_target_for_non_impl_block_uses_linear_order() {
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
        let first = builder.add_block(
            file,
            "first".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 1, 3),
            Language::Rust,
        );
        let second = builder.add_block(
            file,
            "second".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 10, 12),
            Language::Rust,
        );
        let third = builder.add_block(
            file,
            "third".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 20, 22),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = visible_nodes_from_blocks(&tree, &[first, second, third]);
        let remaining = HashSet::from([first, second, third]);
        let order = ReviewOrder::from_tree(&tree, &remaining);

        let next = next_review_target(&tree, &visible, &order, &remaining, second);
        assert_eq!(next, Some(third));
    }

    #[test]
    fn next_review_target_for_impl_block_skips_impl_subtree() {
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
            test_block(BlockKind::Impl, 1, 80),
            Language::Rust,
        );
        let method = builder.add_block(
            impl_block,
            "method".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 3, 9),
            Language::Rust,
        );
        let tail = builder.add_block(
            file,
            "tail".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 100, 110),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = visible_nodes_from_blocks(&tree, &[impl_block, method, tail]);
        let remaining = HashSet::from([impl_block, method, tail]);
        let order = ReviewOrder::from_tree(&tree, &remaining);

        let next = next_review_target(&tree, &visible, &order, &remaining, impl_block);
        assert_eq!(next, Some(tail));
    }

    #[test]
    fn prune_visible_nodes_keeps_only_root_and_block_ancestors() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let docs = builder.add_dir(root, "docs".to_string(), "docs".to_string());
        let src_file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "src-hash".to_string(),
            Language::Rust,
        );
        let docs_file = builder.add_file(
            docs,
            "readme.md".to_string(),
            "docs/readme.md".to_string(),
            "docs-hash".to_string(),
            Language::Markdown,
        );
        let reviewed_block = builder.add_block(
            src_file,
            "f".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 1, 3),
            Language::Rust,
        );
        let _doc_block = builder.add_block(
            docs_file,
            "p".to_string(),
            "docs/readme.md".to_string(),
            test_block(BlockKind::Paragraph, 1, 2),
            Language::Markdown,
        );
        let tree = builder.finalize();
        let mut visible = visible_nodes_from_blocks(&tree, &[reviewed_block]);
        visible.insert(docs_file);
        visible.insert(docs);

        let pruned = prune_visible_nodes(&tree, &visible);
        let expected = visible_nodes_from_blocks(&tree, &[reviewed_block]);
        assert_eq!(pruned, expected);
    }
}
