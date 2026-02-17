use crate::tree::{Tree, TreeNodeId};
use anyhow::Result;
use std::collections::HashSet;

pub struct ReviewNavigator {
    pub tree: Tree,
    pub visible_nodes: HashSet<TreeNodeId>,
    current: TreeNodeId,
}

impl ReviewNavigator {
    pub fn new(tree: Tree, unreviewed_blocks: HashSet<TreeNodeId>) -> Result<Self> {
        let mut visible_nodes = HashSet::new();
        for block_id in unreviewed_blocks {
            visible_nodes.insert(block_id);
            for ancestor in tree.ancestors(block_id) {
                visible_nodes.insert(ancestor);
            }
        }

        let root = tree.root();
        visible_nodes.insert(root);

        Ok(Self {
            tree,
            visible_nodes,
            current: root,
        })
    }

    pub fn current_id(&self) -> TreeNodeId {
        self.current
    }

    pub fn set_current(&mut self, id: TreeNodeId) {
        if self.visible_nodes.contains(&id) {
            self.current = id;
        }
    }

    pub fn jump_root(&mut self) {
        self.current = self.tree.root();
    }

    pub fn descend(&mut self) {
        if let Some(child) = self
            .tree
            .node(self.current)
            .children
            .iter()
            .copied()
            .find(|child| self.visible_nodes.contains(child))
        {
            self.current = child;
        }
    }

    pub fn ascend(&mut self) {
        if let Some(parent) = self.tree.parent(self.current)
            && self.visible_nodes.contains(&parent)
        {
            self.current = parent;
        }
    }

    pub fn move_next(&mut self) {
        if let Some(next) = self.sibling_at_offset(self.current, 1) {
            self.current = next;
        }
    }

    pub fn move_prev(&mut self) {
        if let Some(prev) = self.sibling_at_offset(self.current, -1) {
            self.current = prev;
        }
    }

    fn sibling_at_offset(&self, node_id: TreeNodeId, offset: isize) -> Option<TreeNodeId> {
        let parent = self.tree.parent(node_id)?;
        let siblings: Vec<TreeNodeId> = self
            .tree
            .node(parent)
            .children
            .iter()
            .copied()
            .filter(|child| self.visible_nodes.contains(child))
            .collect();
        let index = siblings
            .iter()
            .position(|&id| id == node_id)?
            .checked_add_signed(offset)?;
        siblings.get(index).copied()
    }
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

    struct TreeFixture {
        tree: Tree,
        root: TreeNodeId,
        src: TreeNodeId,
        docs: TreeNodeId,
        lib_file: TreeNodeId,
        readme_file: TreeNodeId,
        block_a: TreeNodeId,
        block_b: TreeNodeId,
        block_docs: TreeNodeId,
    }

    fn build_fixture() -> TreeFixture {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let docs = builder.add_dir(root, "docs".to_string(), "docs".to_string());
        let lib_file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "lib-hash".to_string(),
            Language::Rust,
        );
        let readme_file = builder.add_file(
            docs,
            "readme.md".to_string(),
            "docs/readme.md".to_string(),
            "readme-hash".to_string(),
            Language::Markdown,
        );
        let block_a = builder.add_block(
            lib_file,
            "a".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 1, 3),
            Language::Rust,
        );
        let block_b = builder.add_block(
            lib_file,
            "b".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 10, 12),
            Language::Rust,
        );
        let block_docs = builder.add_block(
            readme_file,
            "p".to_string(),
            "docs/readme.md".to_string(),
            test_block(BlockKind::Paragraph, 1, 2),
            Language::Markdown,
        );

        TreeFixture {
            tree: builder.finalize(),
            root,
            src,
            docs,
            lib_file,
            readme_file,
            block_a,
            block_b,
            block_docs,
        }
    }

    #[test]
    fn new_marks_root_and_block_ancestors_visible() {
        let fixture = build_fixture();
        let unreviewed = HashSet::from([fixture.block_b]);
        let navigator = ReviewNavigator::new(fixture.tree, unreviewed).unwrap_or_else(|error| {
            panic!("failed to create navigator: {error}");
        });

        assert!(navigator.visible_nodes.contains(&fixture.root));
        assert!(navigator.visible_nodes.contains(&fixture.src));
        assert!(navigator.visible_nodes.contains(&fixture.lib_file));
        assert!(navigator.visible_nodes.contains(&fixture.block_b));
        assert!(!navigator.visible_nodes.contains(&fixture.block_a));
        assert!(!navigator.visible_nodes.contains(&fixture.docs));
        assert!(!navigator.visible_nodes.contains(&fixture.readme_file));
        assert!(!navigator.visible_nodes.contains(&fixture.block_docs));
        assert_eq!(navigator.current_id(), fixture.root);
    }

    #[test]
    fn descend_and_ascend_follow_visible_path() {
        let fixture = build_fixture();
        let unreviewed = HashSet::from([fixture.block_a]);
        let mut navigator =
            ReviewNavigator::new(fixture.tree, unreviewed).unwrap_or_else(|error| {
                panic!("failed to create navigator: {error}");
            });

        navigator.descend();
        assert_eq!(navigator.current_id(), fixture.src);

        navigator.descend();
        assert_eq!(navigator.current_id(), fixture.lib_file);

        navigator.descend();
        assert_eq!(navigator.current_id(), fixture.block_a);

        navigator.ascend();
        assert_eq!(navigator.current_id(), fixture.lib_file);

        navigator.ascend();
        assert_eq!(navigator.current_id(), fixture.src);
    }

    #[test]
    fn move_next_and_prev_use_visible_sibling_order() {
        let fixture = build_fixture();
        let unreviewed = HashSet::from([fixture.block_a, fixture.block_b]);
        let mut navigator =
            ReviewNavigator::new(fixture.tree, unreviewed).unwrap_or_else(|error| {
                panic!("failed to create navigator: {error}");
            });
        navigator.set_current(fixture.block_a);

        navigator.move_next();
        assert_eq!(navigator.current_id(), fixture.block_b);

        navigator.move_next();
        assert_eq!(navigator.current_id(), fixture.block_b);

        navigator.move_prev();
        assert_eq!(navigator.current_id(), fixture.block_a);
    }

    #[test]
    fn set_current_ignores_invisible_nodes() {
        let fixture = build_fixture();
        let unreviewed = HashSet::from([fixture.block_a]);
        let mut navigator =
            ReviewNavigator::new(fixture.tree, unreviewed).unwrap_or_else(|error| {
                panic!("failed to create navigator: {error}");
            });

        navigator.set_current(fixture.block_docs);
        assert_eq!(navigator.current_id(), fixture.root);
    }
}
