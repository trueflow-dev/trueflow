use crate::analysis::Language;
use crate::block::{Block, BlockKind, FileState};
use crate::hashing::hash_str;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct TreeNodeId(usize);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TreeNodeKind {
    Root,
    Directory,
    File,
    Block,
}

impl TreeNodeKind {
    fn label(&self) -> &'static str {
        match self {
            TreeNodeKind::Root => "root",
            TreeNodeKind::Directory => "directory",
            TreeNodeKind::File => "file",
            TreeNodeKind::Block => "block",
        }
    }

    fn entry_prefix(&self) -> &'static str {
        match self {
            TreeNodeKind::Root => "root",
            TreeNodeKind::Directory => "dir",
            TreeNodeKind::File => "file",
            TreeNodeKind::Block => "block",
        }
    }

    fn should_sort_children(&self) -> bool {
        matches!(self, TreeNodeKind::Root | TreeNodeKind::Directory)
    }

    fn is_hash_entry(&self) -> bool {
        matches!(self, TreeNodeKind::Directory | TreeNodeKind::File)
    }

    fn sort_key(&self, name: &str) -> String {
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
    pub path: String,
    pub hash: String,
    pub children: Vec<TreeNodeId>,
    pub block: Option<Block>,
    pub language: Option<Language>,
}

pub struct Tree {
    nodes: Vec<TreeNode>,
    root: TreeNodeId,
    nodes_by_path: HashMap<String, TreeNodeId>,
    #[allow(dead_code)]
    file_paths: HashSet<String>,
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
        self.nodes_by_path.get(path).copied()
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

    pub fn node_by_path_and_hash(&self, path: &str, hash: &str) -> Option<TreeNodeId> {
        let file_id = self.find_by_path(path)?;
        let file_node = self.node(file_id);
        if matches!(file_node.kind, TreeNodeKind::File) && file_node.hash == hash {
            return Some(file_id);
        }
        let mut stack = file_node.children.clone();
        while let Some(node_id) = stack.pop() {
            let node = self.node(node_id);
            if matches!(node.kind, TreeNodeKind::Block) && node.hash == hash {
                return Some(node_id);
            }
            stack.extend(node.children.iter().copied());
        }
        None
    }

    pub fn find_block_node(&self, path: &str, block: &Block) -> Option<TreeNodeId> {
        let file_id = self.find_by_path(path)?;
        let file_node = self.node(file_id);
        
        let mut stack = file_node.children.clone();
        while let Some(node_id) = stack.pop() {
            let node = self.node(node_id);
            if matches!(node.kind, TreeNodeKind::Block) 
                && node.hash == block.hash 
                && node.block.as_ref().is_some_and(|b| b.start_line == block.start_line)
            {
                return Some(node_id);
            }
            stack.extend(node.children.iter().copied());
        }
        None
    }

    #[allow(dead_code)]
    pub fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.file_paths.iter().map(|path| path.as_str())
    }

    pub fn is_node_covered(&self, id: TreeNodeId, approved_hashes: &HashSet<String>) -> bool {
        self.ancestors(id)
            .iter()
            .any(|node_id| approved_hashes.contains(&self.node(*node_id).hash))
    }
}

pub struct TreeBuilder {
    nodes: Vec<TreeNode>,
    root: TreeNodeId,
    children_by_id: HashMap<TreeNodeId, Vec<TreeNodeId>>,
    nodes_by_path: HashMap<String, TreeNodeId>,
    file_paths: HashSet<String>,
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
            path: String::new(),
            hash: String::new(),
            children: Vec::new(),
            block: None,
            language: None,
        };
        let mut nodes_by_path = HashMap::new();
        nodes_by_path.insert(String::new(), root);
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

    pub fn add_dir(&mut self, parent: TreeNodeId, name: String, path: String) -> TreeNodeId {
        self.add_node(parent, TreeNodeKind::Directory, name, path)
    }

    pub fn add_file(
        &mut self,
        parent: TreeNodeId,
        name: String,
        path: String,
        hash: String,
        language: Language,
    ) -> TreeNodeId {
        let id = self.add_node(parent, TreeNodeKind::File, name, path);
        if let Some(node) = self.nodes.get_mut(id.0) {
            node.hash = hash;
            node.language = Some(language);
        }
        id
    }

    pub fn add_block(
        &mut self,
        parent: TreeNodeId,
        name: String,
        path: String,
        block: Block,
        language: Language,
    ) -> TreeNodeId {
        let hash = block.hash.clone();
        let id = self.add_node(parent, TreeNodeKind::Block, name, path);
        if let Some(node) = self.nodes.get_mut(id.0) {
            node.hash = hash;
            node.block = Some(block);
            node.language = Some(language);
        }
        id
    }

    fn add_node(
        &mut self,
        parent: TreeNodeId,
        kind: TreeNodeKind,
        name: String,
        path: String,
    ) -> TreeNodeId {
        let id = TreeNodeId(self.nodes.len());
        let node = TreeNode {
            id,
            parent: Some(parent),
            kind: kind.clone(),
            name: name.clone(),
            path: path.clone(),
            hash: String::new(),
            children: Vec::new(),
            block: None,
            language: None,
        };
        self.nodes.push(node);
        if matches!(kind, TreeNodeKind::Directory | TreeNodeKind::File) {
            self.nodes_by_path.insert(path.clone(), id);
        }
        if matches!(kind, TreeNodeKind::File) {
            self.file_paths.insert(path);
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
        Tree {
            nodes: self.nodes,
            root: self.root,
            nodes_by_path: self.nodes_by_path,
            file_paths: self.file_paths,
        }
    }

    fn attach_children(&mut self, id: TreeNodeId, mut children: Vec<TreeNodeId>) {
        let kind = self.nodes[id.0].kind.clone();
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

        let kind = self.nodes[id.0].kind.clone();
        if matches!(kind, TreeNodeKind::Block | TreeNodeKind::File) {
            return;
        }

        let mut entries: Vec<(String, String)> = children
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
            concatenated.push_str(&hash);
            concatenated.push('|');
        }
        self.nodes[id.0].hash = hash_str(&concatenated);
    }
}

fn block_label(block: &Block) -> String {
    let start = block.start_line + 1;
    let end = block.end_line.max(start);
    format!("{}:L{}-L{}", block.kind.as_str(), start, end)
}

pub fn build_tree_from_files(files: &[FileState]) -> Tree {
    let mut builder = TreeBuilder::new();
    let root = builder.root();
    let mut directories: BTreeMap<String, TreeNodeId> = BTreeMap::new();
    directories.insert(String::new(), root);

    for file in files {
        let parts: Vec<&str> = file.path.split('/').collect();
        let mut current_path = String::new();
        let mut parent = root;

        for (index, part) in parts.iter().enumerate() {
            let is_file = index == parts.len().saturating_sub(1);
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(part);

            if is_file {
                let file_id = builder.add_file(
                    parent,
                    part.to_string(),
                    current_path.clone(),
                    file.file_hash.clone(),
                    file.language.clone(),
                );
                let mut blocks = file.blocks.clone();
                blocks.sort_by_key(|block| (block.start_line, block.end_line));
                let mut impl_stack: Vec<(TreeNodeId, usize, usize)> = Vec::new();
                for block in blocks {
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
                    let kind = block.kind.clone();
                    let name = block_label(&block);
                    let node_id = builder.add_block(
                        parent,
                        name,
                        current_path.clone(),
                        block,
                        file.language.clone(),
                    );

                    if matches!(kind, BlockKind::Impl | BlockKind::Interface) {
                        impl_stack.push((node_id, start_line, end_line));
                    }
                }
            } else {
                let dir_id = directories.entry(current_path.clone()).or_insert_with(|| {
                    builder.add_dir(parent, part.to_string(), current_path.clone())
                });
                parent = *dir_id;
            }
        }
    }

    builder.finalize()
}

pub fn build_tree_from_path(root: &str) -> anyhow::Result<Tree> {
    let files = crate::scanner::scan_directory(root)?;
    Ok(build_tree_from_files(&files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directory_hash_uses_sorted_children() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let dir = builder.add_dir(root, "src".to_string(), "src".to_string());
        builder.add_file(
            dir,
            "b.rs".to_string(),
            "src/b.rs".to_string(),
            "hash-b".to_string(),
            Language::Unknown,
        );
        builder.add_file(
            dir,
            "a.rs".to_string(),
            "src/a.rs".to_string(),
            "hash-a".to_string(),
            Language::Unknown,
        );

        let tree = builder.finalize();
        let dir_node = tree.node(tree.find_by_path("src").expect("dir"));
        let hash_first = dir_node.hash.clone();

        let mut builder_alt = TreeBuilder::new();
        let root_alt = builder_alt.root();
        let dir_alt = builder_alt.add_dir(root_alt, "src".to_string(), "src".to_string());
        builder_alt.add_file(
            dir_alt,
            "a.rs".to_string(),
            "src/a.rs".to_string(),
            "hash-a".to_string(),
            Language::Unknown,
        );
        builder_alt.add_file(
            dir_alt,
            "b.rs".to_string(),
            "src/b.rs".to_string(),
            "hash-b".to_string(),
            Language::Unknown,
        );
        let tree_alt = builder_alt.finalize();
        let dir_alt_node = tree_alt.node(tree_alt.find_by_path("src").expect("dir"));
        let hash_second = dir_alt_node.hash.clone();

        assert_eq!(hash_first, hash_second);
    }
}
