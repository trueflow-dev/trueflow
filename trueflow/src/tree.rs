use crate::analysis::Language;
use crate::block::{Block, BlockKind, FileState};
use crate::hashing::{TreeHash, hash_str};
use crate::repo_path::RepoPath;
use crate::store::ApprovedTargets;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct TreeNodeId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TreeNodeKind {
    Root,
    Directory,
    File,
    Block,
}

impl TreeNodeKind {
    fn label(self) -> &'static str {
        match self {
            TreeNodeKind::Root => "root",
            TreeNodeKind::Directory => "directory",
            TreeNodeKind::File => "file",
            TreeNodeKind::Block => "block",
        }
    }

    fn entry_prefix(self) -> &'static str {
        match self {
            TreeNodeKind::Root => "root",
            TreeNodeKind::Directory => "dir",
            TreeNodeKind::File => "file",
            TreeNodeKind::Block => "block",
        }
    }

    fn should_sort_children(self) -> bool {
        matches!(self, TreeNodeKind::Root | TreeNodeKind::Directory)
    }

    fn is_hash_entry(self) -> bool {
        matches!(self, TreeNodeKind::Directory | TreeNodeKind::File)
    }

    fn sort_key(self, name: &str) -> String {
        format!("{}:{}", self.entry_prefix(), name)
    }
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    #[allow(dead_code)]
    pub id: TreeNodeId,
    pub parent: Option<TreeNodeId>,
    pub kind: TreeNodeKind,
    pub name: String,
    pub path: RepoPath,
    pub hash: TreeHash,
    pub children: Vec<TreeNodeId>,
    pub block: Option<Block>,
    pub language: Option<Language>,
}

pub struct Tree {
    nodes: Vec<TreeNode>,
    root: TreeNodeId,
    nodes_by_path: HashMap<RepoPath, TreeNodeId>,
    block_nodes_by_path_hash_start:
        HashMap<RepoPath, HashMap<TreeHash, HashMap<usize, TreeNodeId>>>,
    #[allow(dead_code)]
    file_paths: HashSet<RepoPath>,
}

impl Tree {
    #[allow(dead_code)]
    pub fn root(&self) -> TreeNodeId {
        self.root
    }

    pub fn node(&self, id: TreeNodeId) -> &TreeNode {
        &self.nodes[id.0]
    }

    #[allow(dead_code)]
    pub fn nodes(&self) -> &[TreeNode] {
        &self.nodes
    }

    pub fn view_json(&self) -> Value {
        self.view_json_from(self.root)
    }

    pub fn view_json_from(&self, id: TreeNodeId) -> Value {
        let node = self.node(id);
        let children = node
            .children
            .iter()
            .map(|child| self.view_json_from(*child))
            .collect::<Vec<_>>();
        json!({
            "type": node.kind.label(),
            "name": node.name,
            "path": node.path,
            "hash": node.hash,
            "children": children,
        })
    }

    pub fn find_by_path(&self, path: &str) -> Option<TreeNodeId> {
        let path = RepoPath::new(path).ok()?;
        self.nodes_by_path.get(&path).copied()
    }

    pub fn parent(&self, id: TreeNodeId) -> Option<TreeNodeId> {
        self.node(id).parent
    }

    pub fn ancestors(&self, id: TreeNodeId) -> Vec<TreeNodeId> {
        let mut current = Some(id);
        let mut ancestors = Vec::new();
        while let Some(node_id) = current {
            ancestors.push(node_id);
            current = self.node(node_id).parent;
        }
        ancestors
    }

    #[allow(dead_code)]
    pub fn file_nodes(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.kind, TreeNodeKind::File))
    }

    pub fn find_block_node(&self, path: impl AsRef<str>, block: &Block) -> Option<TreeNodeId> {
        let path = RepoPath::new(path.as_ref()).ok()?;
        self.block_nodes_by_path_hash_start
            .get(&path)?
            .get(&block.hash)?
            .get(&block.start_line)
            .copied()
    }

    #[allow(dead_code)]
    pub fn file_paths(&self) -> impl Iterator<Item = &RepoPath> {
        self.file_paths.iter()
    }

    pub fn is_node_covered(&self, id: TreeNodeId, approved_targets: &ApprovedTargets) -> bool {
        let mut current = Some(id);
        while let Some(node_id) = current {
            let node = self.node(node_id);
            let target = match node.kind {
                TreeNodeKind::Root | TreeNodeKind::Directory => {
                    crate::store::ReviewTargetRef::Tree {
                        hash: node.hash.clone(),
                    }
                }
                TreeNodeKind::File => crate::store::ReviewTargetRef::File {
                    hash: node.hash.clone(),
                },
                TreeNodeKind::Block => crate::store::ReviewTargetRef::Block {
                    hash: node.hash.clone(),
                },
            };
            if approved_targets.contains_target(&target) {
                return true;
            }
            current = node.parent;
        }
        false
    }
}

pub struct TreeBuilder {
    nodes: Vec<TreeNode>,
    root: TreeNodeId,
    children_by_id: HashMap<TreeNodeId, Vec<TreeNodeId>>,
    nodes_by_path: HashMap<RepoPath, TreeNodeId>,
    file_paths: HashSet<RepoPath>,
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeBuilder {
    pub fn new() -> Self {
        let root = TreeNodeId(0);
        let root_node = TreeNode {
            id: root,
            parent: None,
            kind: TreeNodeKind::Root,
            name: "Root".to_string(),
            path: RepoPath::root(),
            hash: TreeHash::default(),
            children: Vec::new(),
            block: None,
            language: None,
        };
        let mut nodes_by_path = HashMap::new();
        nodes_by_path.insert(RepoPath::root(), root);
        Self {
            nodes: vec![root_node],
            root,
            children_by_id: HashMap::new(),
            nodes_by_path,
            file_paths: HashSet::new(),
        }
    }

    pub fn root(&self) -> TreeNodeId {
        self.root
    }

    pub fn add_dir<P>(&mut self, parent: TreeNodeId, name: String, path: P) -> TreeNodeId
    where
        P: TryInto<RepoPath>,
        P::Error: std::fmt::Debug,
    {
        self.add_node(parent, TreeNodeKind::Directory, name, path)
    }

    pub fn add_file<P>(
        &mut self,
        parent: TreeNodeId,
        name: String,
        path: P,
        hash: impl Into<TreeHash>,
        language: Language,
    ) -> TreeNodeId
    where
        P: TryInto<RepoPath>,
        P::Error: std::fmt::Debug,
    {
        let id = self.add_node(parent, TreeNodeKind::File, name, path);
        if let Some(node) = self.nodes.get_mut(id.0) {
            node.hash = hash.into();
            node.language = Some(language);
        }
        id
    }

    pub fn add_block<P>(
        &mut self,
        parent: TreeNodeId,
        name: String,
        path: P,
        block: Block,
        language: Language,
    ) -> TreeNodeId
    where
        P: TryInto<RepoPath>,
        P::Error: std::fmt::Debug,
    {
        let hash = block.hash.clone();
        let id = self.add_node(parent, TreeNodeKind::Block, name, path);
        if let Some(node) = self.nodes.get_mut(id.0) {
            node.hash = hash;
            node.block = Some(block);
            node.language = Some(language);
        }
        id
    }

    fn add_node<P>(
        &mut self,
        parent: TreeNodeId,
        kind: TreeNodeKind,
        name: String,
        path: P,
    ) -> TreeNodeId
    where
        P: TryInto<RepoPath>,
        P::Error: std::fmt::Debug,
    {
        let path = match path.try_into() {
            Ok(path) => path,
            Err(error) => panic!("tree node path should be valid repo path: {error:?}"),
        };
        let id = TreeNodeId(self.nodes.len());
        let is_hash_entry = kind.is_hash_entry();
        let is_file = matches!(kind, TreeNodeKind::File);
        let index_path = path.clone();
        let node = TreeNode {
            id,
            parent: Some(parent),
            kind,
            name,
            path,
            hash: TreeHash::default(),
            children: Vec::new(),
            block: None,
            language: None,
        };
        self.nodes.push(node);
        if is_hash_entry {
            self.nodes_by_path.insert(index_path.clone(), id);
        }
        if is_file {
            self.file_paths.insert(index_path);
        }
        self.children_by_id.entry(parent).or_default().push(id);
        id
    }

    pub fn finalize(mut self) -> Tree {
        let root_children = self
            .children_by_id
            .get(&self.root)
            .cloned()
            .unwrap_or_default();
        self.attach_children(self.root, root_children);
        self.compute_hashes(self.root);
        let block_nodes_by_path_hash_start = build_block_lookup_indexes(&self.nodes);
        Tree {
            nodes: self.nodes,
            root: self.root,
            nodes_by_path: self.nodes_by_path,
            block_nodes_by_path_hash_start,
            file_paths: self.file_paths,
        }
    }

    fn attach_children(&mut self, id: TreeNodeId, mut children: Vec<TreeNodeId>) {
        let kind = self.nodes[id.0].kind;
        if kind.should_sort_children() {
            children.sort_by(|a, b| {
                let a_node = &self.nodes[a.0];
                let b_node = &self.nodes[b.0];
                a_node
                    .kind
                    .sort_key(&a_node.name)
                    .cmp(&b_node.kind.sort_key(&b_node.name))
            });
        }
        if let Some(node) = self.nodes.get_mut(id.0) {
            node.children = children.clone();
        }
        for child in children {
            let grand_children = self.children_by_id.get(&child).cloned().unwrap_or_default();
            self.attach_children(child, grand_children);
        }
    }

    fn compute_hashes(&mut self, id: TreeNodeId) {
        let children = self.nodes[id.0].children.clone();
        for child in &children {
            self.compute_hashes(*child);
        }

        let kind = self.nodes[id.0].kind;
        if matches!(kind, TreeNodeKind::Block | TreeNodeKind::File) {
            return;
        }

        let mut entries: Vec<(String, TreeHash)> = children
            .iter()
            .filter_map(|child| {
                let node = &self.nodes[child.0];
                if !node.kind.is_hash_entry() {
                    return None;
                }
                let entry_name = format!("{}:{}", node.kind.entry_prefix(), node.name);
                Some((entry_name, node.hash.clone()))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut concatenated = String::new();
        for (name, hash) in entries {
            concatenated.push_str(&name);
            concatenated.push(':');
            concatenated.push_str(hash.as_str());
            concatenated.push('|');
        }
        self.nodes[id.0].hash = TreeHash::new(hash_str(&concatenated));
    }
}

fn build_block_lookup_indexes(
    nodes: &[TreeNode],
) -> HashMap<RepoPath, HashMap<TreeHash, HashMap<usize, TreeNodeId>>> {
    let mut by_path_hash_start: HashMap<RepoPath, HashMap<TreeHash, HashMap<usize, TreeNodeId>>> =
        HashMap::new();

    for node in nodes {
        if !matches!(node.kind, TreeNodeKind::Block) {
            continue;
        }

        let Some(block) = node.block.as_ref() else {
            continue;
        };

        by_path_hash_start
            .entry(node.path.clone())
            .or_default()
            .entry(node.hash.clone())
            .or_default()
            .insert(block.start_line, node.id);
    }

    by_path_hash_start
}

fn block_label(block: &Block) -> String {
    let start = block.start_line + 1;
    let end = block.end_line.max(start);
    format!("{}:L{}-L{}", block.kind.as_str(), start, end)
}

pub fn build_tree_from_files(files: &[FileState]) -> Tree {
    let mut builder = TreeBuilder::new();
    let root = builder.root();
    let mut directories: BTreeMap<RepoPath, TreeNodeId> = BTreeMap::new();
    directories.insert(RepoPath::root(), root);

    for file in files {
        let parts: Vec<&str> = file.path.as_str().split('/').collect();
        let mut current_path = RepoPath::root();
        let mut parent = root;

        for (index, part) in parts.iter().enumerate() {
            let is_file = index == parts.len().saturating_sub(1);
            let next_path = match current_path.join(part) {
                Ok(path) => path,
                Err(error) => panic!("valid tree path segment expected: {error}"),
            };

            if is_file {
                let file_id = builder.add_file(
                    parent,
                    part.to_string(),
                    next_path.clone(),
                    file.tree_hash.clone(),
                    file.language,
                );
                let mut impl_stack: Vec<(TreeNodeId, usize, usize)> = Vec::new();
                for block in file.blocks.clone() {
                    while let Some((_, _, end_line)) = impl_stack.last()
                        && block.start_line > *end_line
                    {
                        impl_stack.pop();
                    }

                    let parent = impl_stack
                        .iter()
                        .rev()
                        .find(|(_, start, end)| {
                            block.start_line >= *start && block.end_line <= *end
                        })
                        .map(|(id, _, _)| *id)
                        .unwrap_or(file_id);

                    let start_line = block.start_line;
                    let end_line = block.end_line;
                    let kind = block.kind;
                    let name = block_label(&block);
                    let node_id =
                        builder.add_block(parent, name, next_path.clone(), block, file.language);

                    if matches!(kind, BlockKind::Impl | BlockKind::Interface) {
                        impl_stack.push((node_id, start_line, end_line));
                    }
                }
            } else {
                let dir_id = directories.entry(next_path.clone()).or_insert_with(|| {
                    builder.add_dir(parent, part.to_string(), next_path.clone())
                });
                parent = *dir_id;
                current_path = next_path;
            }
        }
    }

    builder.finalize()
}

pub fn build_tree_from_path(root: &str) -> anyhow::Result<Tree> {
    let scan_result =
        crate::scanner::scan_directory(root, &crate::scanner::ScanOptions::default())?;
    Ok(build_tree_from_files(&scan_result.files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::BytesHash;

    #[test]
    fn find_block_node_distinguishes_duplicate_hashes_by_start_line() {
        let shared_content = "fn dup() {}".to_string();
        let first = Block::new(shared_content.clone(), BlockKind::Function, 1, 2);
        let second = Block::new(shared_content, BlockKind::Function, 10, 11);
        assert_eq!(first.hash, second.hash);
        assert_ne!(first.start_line, second.start_line);

        let files = vec![FileState::new(
            RepoPath::new("src/lib.rs").unwrap(),
            Language::Rust,
            BytesHash::new("bytes-hash"),
            vec![first.clone(), second.clone()],
        )];

        let tree = build_tree_from_files(&files);
        let first_id = tree.find_block_node(RepoPath::new("src/lib.rs").unwrap(), &first);
        let second_id = tree.find_block_node(RepoPath::new("src/lib.rs").unwrap(), &second);
        assert!(first_id.is_some());
        assert!(second_id.is_some());
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn test_directory_hash_uses_sorted_children() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let dir = builder.add_dir(root, "src".to_string(), RepoPath::new("src").unwrap());
        builder.add_file(
            dir,
            "b.rs".to_string(),
            RepoPath::new("src/b.rs").unwrap(),
            "hash-b",
            Language::Unknown,
        );
        builder.add_file(
            dir,
            "a.rs".to_string(),
            RepoPath::new("src/a.rs").unwrap(),
            "hash-a",
            Language::Unknown,
        );

        let tree = builder.finalize();
        let dir_node = tree.node(tree.find_by_path("src").unwrap());
        let hash_first = dir_node.hash.clone();

        let mut builder_alt = TreeBuilder::new();
        let root_alt = builder_alt.root();
        let dir_alt =
            builder_alt.add_dir(root_alt, "src".to_string(), RepoPath::new("src").unwrap());
        builder_alt.add_file(
            dir_alt,
            "a.rs".to_string(),
            RepoPath::new("src/a.rs").unwrap(),
            "hash-a",
            Language::Unknown,
        );
        builder_alt.add_file(
            dir_alt,
            "b.rs".to_string(),
            RepoPath::new("src/b.rs").unwrap(),
            "hash-b",
            Language::Unknown,
        );
        let tree_alt = builder_alt.finalize();
        let dir_alt_node = tree_alt.node(tree_alt.find_by_path("src").unwrap());
        let hash_second = dir_alt_node.hash.clone();

        assert_eq!(hash_first, hash_second);
    }
}
