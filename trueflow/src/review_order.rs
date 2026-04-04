use crate::repo_path::RepoPath;
use crate::tree::{Tree, TreeNode, TreeNodeId};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewGroup {
    Test,
    Library,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewBand {
    Data,
    Const,
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCursor {
    pub file_path: RepoPath,
    pub kind_rank: u8,
    pub start_line: usize,
    pub node_id: TreeNodeId,
}

#[derive(Debug, Clone, Copy)]
pub enum ReviewAnchor<'a> {
    Block(TreeNodeId),
    Subtree(&'a HashSet<TreeNodeId>),
}

#[derive(Debug, Clone)]
pub struct ReviewOrder {
    ordered: Vec<ReviewCursor>,
    index_by_node: HashMap<TreeNodeId, usize>,
}

impl ReviewOrder {
    pub fn from_tree(tree: &Tree, unreviewed_block_nodes: &HashSet<TreeNodeId>) -> Self {
        let mut ordered = Vec::new();
        let mut items: Vec<_> = unreviewed_block_nodes
            .iter()
            .copied()
            .filter_map(|node_id| {
                let node = tree.node(node_id);
                let block = node.block.as_ref()?;
                let file_path = if node.path.is_root() {
                    match RepoPath::new(&node.name) {
                        Ok(path) => path,
                        Err(error) => {
                            panic!("tree node name should form a valid repo path: {error}")
                        }
                    }
                } else {
                    node.path.clone()
                };
                let cursor = ReviewCursor {
                    file_path,
                    kind_rank: block.kind.default_review_priority(),
                    start_line: block.start_line,
                    node_id,
                };
                Some((cursor, node))
            })
            .collect();

        items.sort_by(|(a_cursor, a_node), (b_cursor, b_node)| {
            let a_group = review_group(&a_cursor.file_path, a_node);
            let b_group = review_group(&b_cursor.file_path, b_node);
            let a_band = review_band_from_kind_rank(a_cursor.kind_rank);
            let b_band = review_band_from_kind_rank(b_cursor.kind_rank);
            (
                review_group_rank(a_group),
                &a_cursor.file_path,
                review_band_rank(a_band),
                a_cursor.kind_rank,
                a_cursor.start_line,
            )
                .cmp(&(
                    review_group_rank(b_group),
                    &b_cursor.file_path,
                    review_band_rank(b_band),
                    b_cursor.kind_rank,
                    b_cursor.start_line,
                ))
        });

        let mut index_by_node = HashMap::with_capacity(items.len());
        for (index, (cursor, _)) in items.into_iter().enumerate() {
            index_by_node.insert(cursor.node_id, index);
            ordered.push(cursor);
        }

        Self {
            ordered,
            index_by_node,
        }
    }

    pub fn first_reviewable_block(&self) -> Option<TreeNodeId> {
        self.ordered.first().map(|cursor| cursor.node_id)
    }

    pub fn next_remaining_after(
        &self,
        anchor: ReviewAnchor<'_>,
        remaining: &HashSet<TreeNodeId>,
    ) -> Option<TreeNodeId> {
        match anchor {
            ReviewAnchor::Block(current) => {
                let index = *self.index_by_node.get(&current)?;
                self.ordered
                    .iter()
                    .skip(index + 1)
                    .find(|cursor| remaining.contains(&cursor.node_id))
                    .map(|cursor| cursor.node_id)
            }
            ReviewAnchor::Subtree(subtree_blocks) => {
                let start_index = subtree_blocks
                    .iter()
                    .filter_map(|node_id| self.index_by_node.get(node_id).copied())
                    .min()?;

                self.ordered
                    .iter()
                    .skip(start_index + 1)
                    .find(|cursor| {
                        remaining.contains(&cursor.node_id)
                            && !subtree_blocks.contains(&cursor.node_id)
                    })
                    .map(|cursor| cursor.node_id)
            }
        }
    }

    #[cfg(test)]
    fn ordered_ids(&self) -> Vec<TreeNodeId> {
        self.ordered.iter().map(|cursor| cursor.node_id).collect()
    }

    #[cfg(test)]
    fn index_for(&self, node_id: TreeNodeId) -> Option<usize> {
        self.index_by_node.get(&node_id).copied()
    }
}

fn review_band_from_kind_rank(kind_rank: u8) -> ReviewBand {
    match kind_rank {
        0 => ReviewBand::Data,
        20 => ReviewBand::Const,
        _ => ReviewBand::Code,
    }
}

fn review_band_rank(band: ReviewBand) -> u8 {
    match band {
        ReviewBand::Data => 0,
        ReviewBand::Const => 1,
        ReviewBand::Code => 2,
    }
}

fn review_group(path: &RepoPath, node: &TreeNode) -> ReviewGroup {
    if is_test_block(path, node) {
        ReviewGroup::Test
    } else if is_library_path(path) {
        ReviewGroup::Library
    } else {
        ReviewGroup::Main
    }
}

fn review_group_rank(group: ReviewGroup) -> u8 {
    match group {
        ReviewGroup::Test => 0,
        ReviewGroup::Library => 1,
        ReviewGroup::Main => 2,
    }
}

fn is_library_path(path: &RepoPath) -> bool {
    path.as_str() == "src/lib.rs"
        || (path.as_str().starts_with("src/")
            && !path.as_str().starts_with("src/main.rs")
            && !path.as_str().starts_with("src/bin/"))
        || path.as_str().starts_with("Sources/")
}

fn is_test_block(path: &RepoPath, node: &TreeNode) -> bool {
    if is_test_path(path) {
        return true;
    }

    if let Some(block) = node.block.as_ref() {
        return block.tags.iter().any(|tag| tag == "test");
    }

    false
}

fn is_test_path(path: &RepoPath) -> bool {
    let path = Path::new(path.as_str());
    if path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .any(|component| component.eq_ignore_ascii_case("tests"))
    {
        return true;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = file_name.to_ascii_lowercase();

    lower.starts_with("test_")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.js")
        || lower.ends_with("_test.ts")
        || lower.ends_with("_test.swift")
        || lower.ends_with("tests.swift")
        || lower.ends_with("test.swift")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind};
    use crate::tree::TreeBuilder;

    fn test_block(kind: BlockKind, start: usize, tags: &[&str]) -> Block {
        let mut block = Block::new(format!("line {start}"), kind, start, start + 1);
        block.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
        block
    }

    #[test]
    fn review_order_prioritizes_test_then_library_then_main() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();

        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let tests = builder.add_dir(root, "tests".to_string(), "tests".to_string());

        let test_file = builder.add_file(
            tests,
            "unit.rs".to_string(),
            "tests/unit.rs".to_string(),
            "file-test".to_string(),
            Language::Rust,
        );
        let lib_file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-lib".to_string(),
            Language::Rust,
        );
        let main_file = builder.add_file(
            src,
            "main.rs".to_string(),
            "src/main.rs".to_string(),
            "file-main".to_string(),
            Language::Rust,
        );

        let test_block_id = builder.add_block(
            test_file,
            "test".to_string(),
            "tests/unit.rs".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Rust,
        );
        let lib_block_id = builder.add_block(
            lib_file,
            "lib".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Rust,
        );
        let main_block_id = builder.add_block(
            main_file,
            "main".to_string(),
            "src/main.rs".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Rust,
        );

        let tree = builder.finalize();
        let unreviewed = HashSet::from([main_block_id, test_block_id, lib_block_id]);
        let order = ReviewOrder::from_tree(&tree, &unreviewed);

        assert_eq!(
            order.ordered_ids(),
            vec![test_block_id, lib_block_id, main_block_id]
        );
    }

    #[test]
    fn review_order_uses_kind_priority_before_line_number_within_file() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-lib".to_string(),
            Language::Rust,
        );

        let function_id = builder.add_block(
            file,
            "function".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Rust,
        );
        let struct_id = builder.add_block(
            file,
            "struct".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Struct, 40, &[]),
            Language::Rust,
        );

        let tree = builder.finalize();
        let unreviewed = HashSet::from([function_id, struct_id]);
        let order = ReviewOrder::from_tree(&tree, &unreviewed);

        assert_eq!(order.ordered_ids(), vec![struct_id, function_id]);
    }

    #[test]
    fn review_order_prioritizes_swiftpm_tests_then_sources_then_manifest() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();

        let sources = builder.add_dir(root, "Sources".to_string(), "Sources".to_string());
        let app = builder.add_dir(sources, "App".to_string(), "Sources/App".to_string());
        let tests = builder.add_dir(root, "Tests".to_string(), "Tests".to_string());
        let app_tests =
            builder.add_dir(tests, "AppTests".to_string(), "Tests/AppTests".to_string());

        let source_file = builder.add_file(
            app,
            "Core.swift".to_string(),
            "Sources/App/Core.swift".to_string(),
            "file-source".to_string(),
            Language::Swift,
        );
        let test_file = builder.add_file(
            app_tests,
            "CoreTests.swift".to_string(),
            "Tests/AppTests/CoreTests.swift".to_string(),
            "file-test".to_string(),
            Language::Swift,
        );
        let manifest_file = builder.add_file(
            root,
            "Package.swift".to_string(),
            "Package.swift".to_string(),
            "file-package".to_string(),
            Language::Swift,
        );

        let source_block_id = builder.add_block(
            source_file,
            "source".to_string(),
            "Sources/App/Core.swift".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Swift,
        );
        let test_block_id = builder.add_block(
            test_file,
            "test".to_string(),
            "Tests/AppTests/CoreTests.swift".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Swift,
        );
        let manifest_block_id = builder.add_block(
            manifest_file,
            "manifest".to_string(),
            "Package.swift".to_string(),
            test_block(BlockKind::Const, 1, &[]),
            Language::Swift,
        );

        let tree = builder.finalize();
        let unreviewed = HashSet::from([manifest_block_id, source_block_id, test_block_id]);
        let order = ReviewOrder::from_tree(&tree, &unreviewed);

        assert_eq!(
            order.ordered_ids(),
            vec![test_block_id, source_block_id, manifest_block_id]
        );
    }

    #[test]
    fn review_order_treats_swiftpm_tests_directory_as_test_even_without_suffix() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();

        let sources = builder.add_dir(root, "Sources".to_string(), "Sources".to_string());
        let app = builder.add_dir(sources, "App".to_string(), "Sources/App".to_string());
        let tests = builder.add_dir(root, "Tests".to_string(), "Tests".to_string());
        let support = builder.add_dir(tests, "Support".to_string(), "Tests/Support".to_string());

        let source_file = builder.add_file(
            app,
            "Core.swift".to_string(),
            "Sources/App/Core.swift".to_string(),
            "file-source".to_string(),
            Language::Swift,
        );
        let helper_file = builder.add_file(
            support,
            "Fixtures.swift".to_string(),
            "Tests/Support/Fixtures.swift".to_string(),
            "file-helper".to_string(),
            Language::Swift,
        );

        let source_block_id = builder.add_block(
            source_file,
            "source".to_string(),
            "Sources/App/Core.swift".to_string(),
            test_block(BlockKind::Function, 1, &[]),
            Language::Swift,
        );
        let helper_block_id = builder.add_block(
            helper_file,
            "helper".to_string(),
            "Tests/Support/Fixtures.swift".to_string(),
            test_block(BlockKind::Const, 1, &[]),
            Language::Swift,
        );

        let tree = builder.finalize();
        let unreviewed = HashSet::from([source_block_id, helper_block_id]);
        let order = ReviewOrder::from_tree(&tree, &unreviewed);

        assert_eq!(order.ordered_ids(), vec![helper_block_id, source_block_id]);
    }

    #[test]
    fn review_order_index_matches_sorted_positions() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-lib".to_string(),
            Language::Rust,
        );

        let first = builder.add_block(
            file,
            "first".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Struct, 1, &[]),
            Language::Rust,
        );
        let second = builder.add_block(
            file,
            "second".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 10, &[]),
            Language::Rust,
        );

        let tree = builder.finalize();
        let unreviewed = HashSet::from([first, second]);
        let order = ReviewOrder::from_tree(&tree, &unreviewed);

        assert_eq!(order.ordered_ids(), vec![first, second]);
        assert_eq!(order.index_for(first), Some(0));
        assert_eq!(order.index_for(second), Some(1));
    }

    #[test]
    fn next_remaining_after_uses_typed_anchor_for_block_and_subtree() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-lib".to_string(),
            Language::Rust,
        );

        let impl_block = builder.add_block(
            file,
            "impl".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Impl, 1, &[]),
            Language::Rust,
        );
        let method = builder.add_block(
            impl_block,
            "method".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Method, 3, &[]),
            Language::Rust,
        );
        let tail = builder.add_block(
            file,
            "tail".to_string(),
            "src/lib.rs".to_string(),
            test_block(BlockKind::Function, 20, &[]),
            Language::Rust,
        );

        let tree = builder.finalize();
        let remaining = HashSet::from([impl_block, method, tail]);
        let order = ReviewOrder::from_tree(&tree, &remaining);
        let subtree = HashSet::from([impl_block, method]);

        assert_eq!(
            order.next_remaining_after(ReviewAnchor::Block(impl_block), &remaining),
            Some(method)
        );
        assert_eq!(
            order.next_remaining_after(ReviewAnchor::Subtree(&subtree), &remaining),
            Some(tail)
        );
    }
}
