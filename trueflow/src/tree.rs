use crate::analysis::Language;
use crate::block::{Block, ByteSpan, FileState};
use crate::hashing::{TreeHash, hash_str};
use crate::repo_path::RepoPath;
use crate::store::{ApprovedTargets, ReviewedBlockChange};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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

    /// Children order by `"{prefix}:{name}"`. The prefixes differ in their
    /// first byte, so comparing `(prefix, name)` gives the same order without
    /// building either string.
    fn sort_key(self, name: &str) -> (&'static str, &str) {
        (self.entry_prefix(), name)
    }

    fn can_contain(self, child: TreeNodeKind) -> bool {
        match self {
            TreeNodeKind::Root | TreeNodeKind::Directory => {
                matches!(child, TreeNodeKind::Directory | TreeNodeKind::File)
            }
            TreeNodeKind::File | TreeNodeKind::Block => matches!(child, TreeNodeKind::Block),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub id: TreeNodeId,
    pub parent: Option<TreeNodeId>,
    pub kind: TreeNodeKind,
    pub name: String,
    pub path: RepoPath,
    pub hash: TreeHash,
    pub children: Vec<TreeNodeId>,
    pub block: Option<Block>,
    pub language: Option<Language>,
    /// What reviewing the node covers. A diff review's base-only block, and every block
    /// expanded from one, shows a deletion; every other node shows content.
    pub change: ReviewedBlockChange,
}

#[derive(Debug, Clone)]
enum SpanBlockNodes {
    One(TreeNodeId),
    Many(Vec<TreeNodeId>),
}

impl SpanBlockNodes {
    fn insert(&mut self, nodes: &[TreeNode], candidate_id: TreeNodeId) -> bool {
        let candidate_hash = &nodes[candidate_id.0].hash;
        match self {
            Self::One(existing_id) => {
                if &nodes[existing_id.0].hash == candidate_hash {
                    return false;
                }
                *self = Self::Many(vec![*existing_id, candidate_id]);
                true
            }
            Self::Many(existing) => {
                if existing
                    .iter()
                    .any(|id| &nodes[id.0].hash == candidate_hash)
                {
                    return false;
                }
                existing.push(candidate_id);
                true
            }
        }
    }

    fn find(&self, nodes: &[TreeNode], hash: &TreeHash) -> Option<TreeNodeId> {
        match self {
            Self::One(id) => (&nodes[id.0].hash == hash).then_some(*id),
            Self::Many(ids) => ids.iter().copied().find(|id| &nodes[id.0].hash == hash),
        }
    }
}

pub struct IndexedTreeFile {
    pub path: RepoPath,
    pub language: Language,
    pub block_nodes: Vec<TreeNodeId>,
}

pub struct Tree {
    nodes: Vec<TreeNode>,
    root: TreeNodeId,
    files_by_path: HashMap<RepoPath, TreeNodeId>,
    dirs_by_path: HashMap<RepoPath, TreeNodeId>,
    block_nodes_by_path_span: HashMap<RepoPath, HashMap<ByteSpan, SpanBlockNodes>>,
}

impl Tree {
    pub fn root(&self) -> TreeNodeId {
        self.root
    }

    pub fn node(&self, id: TreeNodeId) -> &TreeNode {
        &self.nodes[id.0]
    }

    pub fn nodes(&self) -> &[TreeNode] {
        &self.nodes
    }

    pub fn view_json(&self) -> Value {
        let mut values: HashMap<TreeNodeId, Value> = HashMap::new();
        let mut stack = vec![(self.root, false)];

        while let Some((id, visited)) = stack.pop() {
            if visited {
                let node = self.node(id);
                let children = node
                    .children
                    .iter()
                    .filter_map(|child| values.remove(child))
                    .collect::<Vec<_>>();
                let mut value = json!({
                    "type": node.kind.label(),
                    "name": node.name,
                    "path": node.path,
                    "hash": node.hash,
                    "children": children,
                });
                if let Some(block) = node.block.as_ref() {
                    value["start_line"] = json!(block.start_line);
                    value["end_line"] = json!(block.end_line);
                    value["start_byte"] = json!(block.start_byte);
                    value["end_byte"] = json!(block.end_byte);
                }
                values.insert(id, value);
                continue;
            }

            stack.push((id, true));
            let children = self.node(id).children.clone();
            stack.extend(children.into_iter().rev().map(|child| (child, false)));
        }

        values.remove(&self.root).unwrap_or_else(|| json!({}))
    }

    pub fn find_file_by_path(&self, path: &str) -> Option<TreeNodeId> {
        if let Some(node_id) = self.files_by_path.get(path) {
            return Some(*node_id);
        }
        let normalized = RepoPath::new(path).ok()?;
        self.files_by_path.get(&normalized).copied()
    }

    pub fn find_dir_by_path(&self, path: &str) -> Option<TreeNodeId> {
        if let Some(node_id) = self.dirs_by_path.get(path) {
            return Some(*node_id);
        }
        let normalized = RepoPath::new(path).ok()?;
        self.dirs_by_path.get(&normalized).copied()
    }

    pub fn find_by_path(&self, path: &str) -> Option<TreeNodeId> {
        self.find_file_by_path(path)
            .or_else(|| self.find_dir_by_path(path))
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

    pub fn find_block_node(&self, path: impl AsRef<str>, block: &Block) -> Option<TreeNodeId> {
        let raw = path.as_ref();
        let by_span = match self.block_nodes_by_path_span.get(raw) {
            Some(by_span) => by_span,
            None => {
                let normalized = RepoPath::new(raw).ok()?;
                self.block_nodes_by_path_span.get(&normalized)?
            }
        };
        by_span
            .get(&block.byte_span())?
            .find(&self.nodes, &block.hash)
    }
    fn block_has_child_blocks(&self, id: TreeNodeId) -> bool {
        self.node(id)
            .children
            .iter()
            .any(|child| matches!(self.node(*child).kind, TreeNodeKind::Block))
    }

    pub fn is_container_block(&self, id: TreeNodeId) -> bool {
        matches!(self.node(id).kind, TreeNodeKind::Block) && self.block_has_child_blocks(id)
    }

    pub fn insert_block_children(
        &mut self,
        parent: TreeNodeId,
        blocks: Vec<Block>,
    ) -> Vec<TreeNodeId> {
        if blocks.is_empty() {
            return Vec::new();
        }

        let parent_node = self.node(parent).clone();
        assert!(
            matches!(parent_node.kind, TreeNodeKind::File | TreeNodeKind::Block),
            "tree node kind {:?} cannot receive dynamic block children",
            parent_node.kind
        );
        let language = parent_node.language.unwrap_or(Language::Unknown);
        let path = parent_node.path;

        blocks
            .into_iter()
            .map(|block| self.insert_block_child(parent, &path, language, block))
            .collect()
    }

    fn insert_block_child(
        &mut self,
        parent: TreeNodeId,
        path: &RepoPath,
        language: Language,
        block: Block,
    ) -> TreeNodeId {
        let parent_kind = self
            .nodes
            .get(parent.0)
            .unwrap_or_else(|| panic!("tree parent id {parent:?} must exist"))
            .kind;
        assert!(
            parent_kind.can_contain(TreeNodeKind::Block),
            "tree node kind {parent_kind:?} cannot receive block children"
        );

        let hash = block.hash.clone();
        let byte_span = block.byte_span();
        // A block expanded from a deleted block is deleted too.
        let change = self.nodes[parent.0].change;
        let id = TreeNodeId(self.nodes.len());
        self.nodes.push(TreeNode {
            id,
            parent: Some(parent),
            kind: TreeNodeKind::Block,
            name: block_label(&block),
            path: path.clone(),
            hash,
            children: Vec::new(),
            block: Some(block),
            language: Some(language),
            change,
        });
        self.nodes[parent.0].children.push(id);

        insert_span_block_node(
            &mut self.block_nodes_by_path_span,
            &self.nodes,
            path,
            byte_span,
            id,
        );

        id
    }

    pub fn is_node_covered(
        &self,
        id: TreeNodeId,
        approved_targets: &ApprovedTargets,
        workdir_prefix: Option<&str>,
    ) -> bool {
        let mut current = Some(id);
        while let Some(node_id) = current {
            let node = self.node(node_id);
            match node.kind {
                TreeNodeKind::Root | TreeNodeKind::Directory => {
                    if approved_targets.contains_tree(&node.hash, &node.path, workdir_prefix) {
                        return true;
                    }
                }
                TreeNodeKind::File => {
                    if approved_targets.contains_file(&node.hash, &node.path, workdir_prefix) {
                        return true;
                    }
                }
                TreeNodeKind::Block => {
                    let Some(block) = node.block.as_ref() else {
                        current = node.parent;
                        continue;
                    };
                    if approved_targets.contains_block(
                        &node.hash,
                        &node.path,
                        block.start_line,
                        workdir_prefix,
                    ) {
                        return true;
                    }
                }
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
    files_by_path: HashMap<RepoPath, TreeNodeId>,
    dirs_by_path: HashMap<RepoPath, TreeNodeId>,
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
            change: ReviewedBlockChange::Content,
        };
        let mut dirs_by_path = HashMap::new();
        dirs_by_path.insert(RepoPath::root(), root);
        Self {
            nodes: vec![root_node],
            root,
            children_by_id: HashMap::new(),
            files_by_path: HashMap::new(),
            dirs_by_path,
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
        self.add_block_showing(
            parent,
            name,
            path,
            block,
            language,
            ReviewedBlockChange::Content,
        )
    }

    /// Adds a block whose review covers `change`: its content, or its deletion.
    pub fn add_block_showing<P>(
        &mut self,
        parent: TreeNodeId,
        name: String,
        path: P,
        block: Block,
        language: Language,
        change: ReviewedBlockChange,
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
            node.change = change;
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
        let parent_kind = self
            .nodes
            .get(parent.0)
            .unwrap_or_else(|| panic!("tree parent id {parent:?} must exist"))
            .kind;
        assert!(
            parent_kind.can_contain(kind),
            "tree node kind {kind:?} cannot be added under {parent_kind:?}"
        );

        let id = TreeNodeId(self.nodes.len());
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
            change: ReviewedBlockChange::Content,
        };
        self.nodes.push(node);
        match kind {
            TreeNodeKind::File => {
                let previous = self.files_by_path.insert(index_path.clone(), id);
                assert!(
                    previous.is_none(),
                    "tree file path must be unique: {index_path}"
                );
            }
            TreeNodeKind::Directory => {
                let previous = self.dirs_by_path.insert(index_path.clone(), id);
                assert!(
                    previous.is_none(),
                    "tree directory path must be unique: {index_path}"
                );
            }
            TreeNodeKind::Root | TreeNodeKind::Block => {}
        }
        self.children_by_id.entry(parent).or_default().push(id);
        id
    }

    pub fn finalize(mut self) -> Tree {
        let root_children = self.children_by_id.remove(&self.root).unwrap_or_default();
        self.attach_children(self.root, root_children);
        self.compute_hashes(self.root);
        let block_nodes_by_path_span = build_block_lookup_indexes(&self.nodes);
        Tree {
            nodes: self.nodes,
            root: self.root,
            files_by_path: self.files_by_path,
            dirs_by_path: self.dirs_by_path,
            block_nodes_by_path_span,
        }
    }

    fn attach_children(&mut self, id: TreeNodeId, children: Vec<TreeNodeId>) {
        let mut stack = vec![(id, children)];
        while let Some((current, mut children)) = stack.pop() {
            let kind = self.nodes[current.0].kind;
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
            for &child in children.iter().rev() {
                if let Some(grand_children) = self.children_by_id.remove(&child) {
                    stack.push((child, grand_children));
                }
            }
            if let Some(node) = self.nodes.get_mut(current.0) {
                node.children = children;
            }
        }
    }

    fn compute_hashes(&mut self, id: TreeNodeId) {
        let mut stack = vec![(id, false)];
        let mut concatenated = String::new();
        while let Some((current, visited)) = stack.pop() {
            let kind = self.nodes[current.0].kind;
            if matches!(kind, TreeNodeKind::Block | TreeNodeKind::File) {
                continue;
            }
            if !visited {
                stack.push((current, true));
                let children = &self.nodes[current.0].children;
                for &child in children.iter().rev() {
                    if !matches!(
                        self.nodes[child.0].kind,
                        TreeNodeKind::Block | TreeNodeKind::File
                    ) {
                        stack.push((child, false));
                    }
                }
                continue;
            }

            concatenated.clear();
            for &child in &self.nodes[current.0].children {
                let node = &self.nodes[child.0];
                if !node.kind.is_hash_entry() {
                    continue;
                }
                concatenated.push_str(node.kind.entry_prefix());
                concatenated.push(':');
                concatenated.push_str(&node.name);
                concatenated.push(':');
                concatenated.push_str(node.hash.as_str());
                concatenated.push('|');
            }
            self.nodes[current.0].hash = TreeHash::new(hash_str(&concatenated));
        }
    }
}

fn insert_span_block_node(
    by_path_span: &mut HashMap<RepoPath, HashMap<ByteSpan, SpanBlockNodes>>,
    nodes: &[TreeNode],
    path: &RepoPath,
    byte_span: ByteSpan,
    id: TreeNodeId,
) {
    let by_span = match by_path_span.get_mut(path.as_str()) {
        Some(by_span) => by_span,
        None => by_path_span.entry(path.clone()).or_default(),
    };
    let inserted = match by_span.get_mut(&byte_span) {
        Some(existing) => existing.insert(nodes, id),
        None => {
            by_span.insert(byte_span, SpanBlockNodes::One(id));
            true
        }
    };
    assert!(
        inserted,
        "duplicate block lookup key: path={path}, hash={}, start_byte={}, end_byte={}",
        nodes[id.0].hash, byte_span.start_byte, byte_span.end_byte,
    );
}

fn build_block_lookup_indexes(
    nodes: &[TreeNode],
) -> HashMap<RepoPath, HashMap<ByteSpan, SpanBlockNodes>> {
    let mut by_path_span = HashMap::new();

    for node in nodes {
        if !matches!(node.kind, TreeNodeKind::Block) {
            continue;
        }

        let Some(block) = node.block.as_ref() else {
            continue;
        };
        insert_span_block_node(
            &mut by_path_span,
            nodes,
            &node.path,
            block.byte_span(),
            node.id,
        );
    }

    by_path_span
}

fn block_label(block: &Block) -> String {
    let start = block.start_line + 1;
    let end = block.end_line.max(start);
    format!("{}:L{}-L{}", block.kind.as_str(), start, end)
}

pub fn build_tree_from_files(files: &[FileState]) -> Tree {
    build_tree_from_files_owned(files.to_vec()).0
}

pub fn build_tree_from_files_owned(files: Vec<FileState>) -> (Tree, Vec<IndexedTreeFile>) {
    let mut builder = TreeBuilder::new();
    let root = builder.root();
    let mut directories: HashMap<RepoPath, TreeNodeId> = HashMap::new();
    directories.insert(RepoPath::root(), root);
    let mut indexed_files = Vec::with_capacity(files.len());
    let mut container_stack: Vec<(TreeNodeId, ByteSpan)> = Vec::new();

    for file in files {
        let raw_path = file.path.as_str();
        let mut parent = root;
        let mut segment_start = 0;

        for (slash_idx, _) in raw_path.match_indices('/') {
            let dir_prefix = &raw_path[..slash_idx];
            let part = &raw_path[segment_start..slash_idx];
            let dir_id = match directories.get(dir_prefix) {
                Some(&dir_id) => dir_id,
                None => {
                    let dir_path = RepoPath::new(dir_prefix).unwrap_or_else(|error| {
                        panic!("valid tree path segment expected: {error}")
                    });
                    let id = builder.add_dir(parent, part.to_string(), dir_path.clone());
                    directories.insert(dir_path, id);
                    id
                }
            };
            parent = dir_id;
            segment_start = slash_idx + 1;
        }

        let file_name = raw_path[segment_start..].to_string();
        let file_id = builder.add_file(
            parent,
            file_name,
            file.path.clone(),
            file.tree_hash,
            file.language,
        );
        container_stack.clear();
        let mut block_nodes = Vec::with_capacity(file.blocks.len());
        for block in file.blocks {
            let byte_span = block.byte_span();
            while let Some((_, container_span)) = container_stack.last() {
                if byte_span.start_byte >= container_span.end_byte
                    || !container_span.properly_contains(&byte_span)
                {
                    container_stack.pop();
                } else {
                    break;
                }
            }

            let block_parent = container_stack.last().map_or(file_id, |(id, _)| *id);
            let kind = block.kind;
            let name = block_label(&block);
            let node_id =
                builder.add_block(block_parent, name, file.path.clone(), block, file.language);
            block_nodes.push(node_id);

            if kind.can_contain_review_children() {
                container_stack.push((node_id, byte_span));
            }
        }
        indexed_files.push(IndexedTreeFile {
            path: file.path,
            language: file.language,
            block_nodes,
        });
    }

    (builder.finalize(), indexed_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockKind, ByteSpan, LineSpan};
    use crate::hashing::BytesHash;

    fn test_block(content: String, kind: BlockKind, start_line: usize, end_line: usize) -> Block {
        let start_byte = start_line;
        let end_byte = start_byte
            .checked_add(content.len())
            .unwrap_or_else(|| panic!("test block byte end overflow"));
        Block::new(
            content,
            kind,
            LineSpan::new(start_line, end_line),
            ByteSpan::new(start_byte, end_byte),
        )
    }

    #[test]
    fn find_block_node_distinguishes_identical_hashes_on_same_line_by_byte_span() {
        let shared_content = "fn dup() {}".to_string();
        let first = Block::new(
            shared_content.clone(),
            BlockKind::Function,
            LineSpan::new(0, 1),
            ByteSpan::new(0, "fn dup() {}".len()),
        );
        let second = Block::new(
            shared_content,
            BlockKind::Function,
            LineSpan::new(0, 1),
            ByteSpan::new("fn dup() {}".len() + 1, 2 * "fn dup() {}".len() + 1),
        );
        assert_eq!(first.hash, second.hash);
        assert_eq!(first.line_span(), second.line_span());
        assert_ne!(first.byte_span(), second.byte_span());

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
    fn dynamic_insert_keeps_identical_same_line_blocks_distinct_by_byte_span() {
        let content = "fn dup() {}";
        let first = Block::new(
            content.to_string(),
            BlockKind::Function,
            LineSpan::new(0, 1),
            ByteSpan::new(0, content.len()),
        );
        let second = Block::new(
            content.to_string(),
            BlockKind::Function,
            LineSpan::new(0, 1),
            ByteSpan::new(content.len() + 1, 2 * content.len() + 1),
        );
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let mut tree = builder.finalize();

        let ids = tree.insert_block_children(file, vec![first.clone(), second.clone()]);
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(tree.find_block_node("src/lib.rs", &first), Some(ids[0]));
        assert_eq!(tree.find_block_node("src/lib.rs", &second), Some(ids[1]));
    }

    #[test]
    fn build_tree_nests_section_children_that_share_parent_start_line() {
        let source = "# Guide\nIntro.\n\n## Install\nSteps.\n";
        let parent =
            Block::from_file_range(source, BlockKind::Section, ByteSpan::new(0, source.len()))
                .unwrap_or_else(|error| panic!("parent source range should be valid: {error}"));
        let child_start = "# Guide\nIntro.\n\n".len();
        let intro =
            Block::from_file_range(source, BlockKind::Section, ByteSpan::new(0, child_start))
                .unwrap_or_else(|error| panic!("intro source range should be valid: {error}"));
        let child = Block::from_file_range(
            source,
            BlockKind::Section,
            ByteSpan::new(child_start, source.len()),
        )
        .unwrap_or_else(|error| panic!("child source range should be valid: {error}"));
        let files = vec![FileState::new(
            RepoPath::new("README.md").unwrap(),
            Language::Markdown,
            BytesHash::new("bytes-hash"),
            vec![intro.clone(), child.clone(), parent.clone()],
        )];

        let tree = build_tree_from_files(&files);
        let parent_id = tree
            .find_block_node(RepoPath::new("README.md").unwrap(), &parent)
            .unwrap_or_else(|| panic!("expected parent section node"));
        let intro_id = tree
            .find_block_node(RepoPath::new("README.md").unwrap(), &intro)
            .unwrap_or_else(|| panic!("expected intro section node"));
        let child_id = tree
            .find_block_node(RepoPath::new("README.md").unwrap(), &child)
            .unwrap_or_else(|| panic!("expected child section node"));

        assert_eq!(tree.node(intro_id).parent, Some(parent_id));
        assert_eq!(tree.node(child_id).parent, Some(parent_id));
    }

    #[test]
    fn build_tree_nests_swift_type_members_under_container_block() {
        let source = "struct Worker {\n    func start() {}\n\n    func stop() {}\n}\n";
        let container =
            Block::from_file_range(source, BlockKind::Struct, ByteSpan::new(0, source.len()))
                .unwrap_or_else(|error| panic!("container source range should be valid: {error}"));
        let first_start = source
            .find("func start() {}\n")
            .unwrap_or_else(|| panic!("expected first method"));
        let second_start = source
            .find("func stop() {}\n")
            .unwrap_or_else(|| panic!("expected second method"));
        let first_method = Block::from_file_range(
            source,
            BlockKind::Method,
            ByteSpan::new(first_start, first_start + "func start() {}\n".len()),
        )
        .unwrap_or_else(|error| panic!("first method range should be valid: {error}"));
        let second_method = Block::from_file_range(
            source,
            BlockKind::Method,
            ByteSpan::new(second_start, second_start + "func stop() {}\n".len()),
        )
        .unwrap_or_else(|error| panic!("second method range should be valid: {error}"));

        let files = vec![FileState::new(
            RepoPath::new("Sources/App/Core.swift").unwrap(),
            Language::Swift,
            BytesHash::new("bytes-hash"),
            vec![
                container.clone(),
                first_method.clone(),
                second_method.clone(),
            ],
        )];

        let tree = build_tree_from_files(&files);
        let container_id = tree
            .find_block_node("Sources/App/Core.swift", &container)
            .unwrap_or_else(|| panic!("expected Swift container block"));
        let first_method_id = tree
            .find_block_node("Sources/App/Core.swift", &first_method)
            .unwrap_or_else(|| panic!("expected first Swift member block"));
        let second_method_id = tree
            .find_block_node("Sources/App/Core.swift", &second_method)
            .unwrap_or_else(|| panic!("expected second Swift member block"));

        assert_eq!(tree.parent(first_method_id), Some(container_id));
        assert_eq!(tree.parent(second_method_id), Some(container_id));
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

    #[test]
    #[should_panic(expected = "tree file path must be unique")]
    fn builder_rejects_duplicate_indexed_paths() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let dir = builder.add_dir(root, "src".to_string(), RepoPath::new("src").unwrap());
        builder.add_file(
            dir,
            "lib.rs".to_string(),
            RepoPath::new("src/lib.rs").unwrap(),
            "hash-a",
            Language::Rust,
        );
        builder.add_file(
            dir,
            "lib.rs".to_string(),
            RepoPath::new("src/lib.rs").unwrap(),
            "hash-b",
            Language::Rust,
        );
    }

    #[test]
    #[should_panic(expected = "tree node kind Block cannot be added under Directory")]
    fn builder_rejects_invalid_parent_child_relationships() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let dir = builder.add_dir(root, "src".to_string(), RepoPath::new("src").unwrap());
        builder.add_block(
            dir,
            "function:L1-L2".to_string(),
            RepoPath::new("src/lib.rs").unwrap(),
            test_block("fn f() {}".to_string(), BlockKind::Function, 0, 1),
            Language::Rust,
        );
    }

    #[test]
    #[should_panic(expected = "duplicate block lookup key")]
    fn build_tree_rejects_duplicate_block_lookup_keys() {
        let block = test_block("fn dup() {}".to_string(), BlockKind::Function, 1, 2);
        let files = vec![FileState::new(
            RepoPath::new("src/lib.rs").unwrap(),
            Language::Rust,
            BytesHash::new("bytes-hash"),
            vec![block.clone(), block],
        )];

        let _tree = build_tree_from_files(&files);
    }
}
