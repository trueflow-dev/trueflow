use crate::review_navigator::ReviewNavigator;
use crate::review_order::{ReviewAnchor, ReviewOrder};
use crate::store::Verdict;
use crate::tree::{TreeNodeId, TreeNodeKind};
use std::collections::HashSet;

pub fn action_block_ids(navigator: &ReviewNavigator, node_id: TreeNodeId) -> Vec<TreeNodeId> {
    let node = navigator.tree.node(node_id);
    match node.kind {
        TreeNodeKind::Block => {
            if navigator.tree.is_container_block(node_id) {
                navigator.visible_descendant_block_ids(node_id)
            } else {
                vec![node_id]
            }
        }
        _ => navigator.visible_descendant_block_ids(node_id),
    }
}

/// What a review tree holds of the content its nodes hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTreeContent {
    /// Every block of the scanned files, so a node's hash identifies exactly
    /// the blocks under it.
    FullScan,
    /// Only the blocks a diff changed, while file and directory nodes still
    /// hash whole files, so their hashes also identify blocks the tree omits.
    Diff,
}

/// How a review action is written to the review store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRecordShape {
    /// One record of the acted-on node itself.
    Node,
    /// One block record per listed block: the blocks the session removes.
    Blocks(Vec<TreeNodeId>),
}

/// How to record `verdict` on `node_id` so that an approval never covers
/// code the session did not select. An approval of a file, directory, root
/// or container block records the node itself only when its hash identifies
/// nothing but blocks the session shows: in a full scan, with every block
/// under it (and the node itself, when it is a block) pending in the
/// session. Blocks a filter left out, unchanged blocks of a diff and blocks
/// approved earlier all fail that test, so the approval instead records
/// each block the session removes. Comments and rejections cover nothing,
/// so they keep one record of the node, whose note is about the node.
pub fn action_record_shape(
    navigator: &ReviewNavigator,
    node_id: TreeNodeId,
    verdict: &Verdict,
    content: ReviewTreeContent,
) -> ActionRecordShape {
    match verdict {
        Verdict::Comment | Verdict::Rejected => ActionRecordShape::Node,
        Verdict::Approved => {
            let block_ids = action_block_ids(navigator, node_id);
            let node_is_exactly_its_shown_blocks = block_ids == [node_id]
                || match content {
                    ReviewTreeContent::FullScan => every_block_under_is_shown(navigator, node_id),
                    ReviewTreeContent::Diff => false,
                };
            if node_is_exactly_its_shown_blocks {
                ActionRecordShape::Node
            } else {
                ActionRecordShape::Blocks(block_ids)
            }
        }
    }
}

fn every_block_under_is_shown(navigator: &ReviewNavigator, node_id: TreeNodeId) -> bool {
    let mut stack = vec![node_id];
    while let Some(id) = stack.pop() {
        let node = navigator.tree.node(id);
        if matches!(node.kind, TreeNodeKind::Block) && !navigator.is_visible_block(id) {
            return false;
        }
        stack.extend(node.children.iter().copied());
    }
    true
}

pub fn next_review_target(
    navigator: &ReviewNavigator,
    review_order: &ReviewOrder,
    remaining_reviewable: &HashSet<TreeNodeId>,
    node_id: TreeNodeId,
) -> Option<TreeNodeId> {
    let node = navigator.tree.node(node_id);
    match node.kind {
        TreeNodeKind::Block => {
            if navigator.tree.is_container_block(node_id) {
                let subtree_blocks = navigator.visible_descendant_block_id_set(node_id);
                review_order.next_remaining_after(
                    ReviewAnchor::Subtree(&subtree_blocks),
                    remaining_reviewable,
                )
            } else {
                review_order
                    .next_remaining_after(ReviewAnchor::Block(node_id), remaining_reviewable)
            }
        }
        _ => {
            let subtree_blocks = navigator.visible_descendant_block_id_set(node_id);
            review_order
                .next_remaining_after(ReviewAnchor::Subtree(&subtree_blocks), remaining_reviewable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
    use crate::tree::{Tree, TreeBuilder};

    fn test_block(kind: BlockKind, start: usize) -> Block {
        let content = format!("{kind:?}-{start}");
        let line_span = LineSpan::new(start, start + content.lines().count());
        let byte_span = ByteSpan::new(0, content.len());
        Block::new(content, kind, line_span, byte_span)
    }

    fn navigator_from_blocks(tree: Tree, block_ids: &[TreeNodeId]) -> ReviewNavigator {
        let unreviewed = block_ids.iter().copied().collect::<HashSet<_>>();
        ReviewNavigator::new(tree, unreviewed)
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"))
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
            test_block(BlockKind::Function, 10),
            Language::Rust,
        );
        let tree = builder.finalize();
        let navigator = navigator_from_blocks(tree, &[function]);

        let selected = action_block_ids(&navigator, function);
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
            test_block(BlockKind::Impl, 1),
            Language::Rust,
        );
        let method_a = builder.add_block(
            impl_block,
            "method_a".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 3),
            Language::Rust,
        );
        let method_b = builder.add_block(
            impl_block,
            "method_b".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 20),
            Language::Rust,
        );
        let tree = builder.finalize();
        let navigator = navigator_from_blocks(tree, &[impl_block, method_a, method_b]);

        let selected = action_block_ids(&navigator, impl_block)
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
            test_block(BlockKind::Function, 1),
            Language::Rust,
        );
        let second = builder.add_block(
            file,
            "second".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 10),
            Language::Rust,
        );
        let tree = builder.finalize();
        let navigator = navigator_from_blocks(tree, &[first, second]);

        let selected = action_block_ids(&navigator, file)
            .into_iter()
            .collect::<HashSet<_>>();
        let expected = HashSet::from([first, second]);
        assert_eq!(selected, expected);
    }

    #[test]
    fn file_approval_records_the_file_only_when_its_whole_scan_is_shown() {
        let build = || {
            let mut builder = TreeBuilder::new();
            let root = builder.root();
            let file = builder.add_file(
                root,
                "lib.rs".to_string(),
                "lib.rs".to_string(),
                "file-hash".to_string(),
                Language::Rust,
            );
            let first = builder.add_block(
                file,
                "first".to_string(),
                "lib.rs".to_string(),
                test_block(BlockKind::Function, 1),
                Language::Rust,
            );
            let second = builder.add_block(
                file,
                "second".to_string(),
                "lib.rs".to_string(),
                test_block(BlockKind::Struct, 10),
                Language::Rust,
            );
            (builder.finalize(), root, file, first, second)
        };
        let (tree, root, file, first, second) = build();
        let all_shown = navigator_from_blocks(tree, &[first, second]);
        let first_shown = navigator_from_blocks(build().0, &[first]);
        let approved = Verdict::Approved;

        assert_eq!(
            action_record_shape(&all_shown, file, &approved, ReviewTreeContent::FullScan),
            ActionRecordShape::Node
        );
        assert_eq!(
            action_record_shape(&all_shown, file, &approved, ReviewTreeContent::Diff),
            ActionRecordShape::Blocks(vec![first, second])
        );
        assert_eq!(
            action_record_shape(&first_shown, file, &approved, ReviewTreeContent::FullScan),
            ActionRecordShape::Blocks(vec![first])
        );
        assert_eq!(
            action_record_shape(&first_shown, root, &approved, ReviewTreeContent::FullScan),
            ActionRecordShape::Blocks(vec![first])
        );
        assert_eq!(
            action_record_shape(&first_shown, first, &approved, ReviewTreeContent::Diff),
            ActionRecordShape::Node
        );
        for verdict in [Verdict::Comment, Verdict::Rejected] {
            assert_eq!(
                action_record_shape(&first_shown, file, &verdict, ReviewTreeContent::Diff),
                ActionRecordShape::Node
            );
        }
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
            test_block(BlockKind::Function, 1),
            Language::Rust,
        );
        let second = builder.add_block(
            file,
            "second".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 10),
            Language::Rust,
        );
        let third = builder.add_block(
            file,
            "third".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 20),
            Language::Rust,
        );
        let tree = builder.finalize();
        let remaining = HashSet::from([first, second, third]);
        let order = ReviewOrder::from_tree(&tree, &remaining);
        let navigator = navigator_from_blocks(tree, &[first, second, third]);

        let next = next_review_target(&navigator, &order, &remaining, second);
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
            test_block(BlockKind::Impl, 1),
            Language::Rust,
        );
        let method = builder.add_block(
            impl_block,
            "method".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 3),
            Language::Rust,
        );
        let tail = builder.add_block(
            file,
            "tail".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 100),
            Language::Rust,
        );
        let tree = builder.finalize();
        let remaining = HashSet::from([impl_block, method, tail]);
        let order = ReviewOrder::from_tree(&tree, &remaining);
        let navigator = navigator_from_blocks(tree, &[impl_block, method, tail]);

        let next = next_review_target(&navigator, &order, &remaining, impl_block);
        assert_eq!(next, Some(tail));
    }
}
