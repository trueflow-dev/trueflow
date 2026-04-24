use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::store::{Identity, Record, ReviewCheck, ReviewDatabase, ReviewTargetRef, Verdict};
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct CoverageBuildOptions {
    pub workdir_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BindingRelation {
    Exact,
    PathScoped,
    HashOnly,
    TargetNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum CoverageDiagnostic {
    AmbiguousRecord {
        record_id: String,
        candidate_nodes: Vec<TreeNodeId>,
    },
    UnresolvedRecord {
        record_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageScope {
    Direct,
    Effective,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictRequirement {
    Any,
    ApprovedOnly,
}

#[derive(Debug, Clone)]
pub struct CoveragePolicy {
    pub required_checks: Vec<ReviewCheck>,
    pub min_distinct_identities: usize,
    pub verdict_requirement: VerdictRequirement,
    pub scope: CoverageScope,
}

impl CoveragePolicy {
    pub fn single_review() -> Self {
        Self {
            required_checks: vec![ReviewCheck::review()],
            min_distinct_identities: 1,
            verdict_requirement: VerdictRequirement::ApprovedOnly,
            scope: CoverageScope::Direct,
        }
    }

    pub fn two_person_review() -> Self {
        Self {
            required_checks: vec![ReviewCheck::review()],
            min_distinct_identities: 2,
            verdict_requirement: VerdictRequirement::ApprovedOnly,
            scope: CoverageScope::Direct,
        }
    }

    pub fn with_scope(mut self, scope: CoverageScope) -> Self {
        self.scope = scope;
        self
    }

    fn allows_verdict(&self, verdict: Option<&Verdict>) -> bool {
        match self.verdict_requirement {
            VerdictRequirement::Any => true,
            VerdictRequirement::ApprovedOnly => verdict == Some(&Verdict::Approved),
        }
    }
}

pub struct CoverageIndex<'a> {
    tree: &'a Tree,
    database: &'a ReviewDatabase,
    workdir_prefix: Option<String>,
    diagnostics: Vec<CoverageDiagnostic>,
    node_facts: HashMap<TreeNodeId, NodeCoverageFacts>,
    record_bindings: HashMap<String, RecordBinding>,
}

impl<'a> CoverageIndex<'a> {
    pub fn build(
        tree: &'a Tree,
        database: &'a ReviewDatabase,
        options: &CoverageBuildOptions,
    ) -> anyhow::Result<Self> {
        let lookups = TreeCoverageLookups::from_tree(tree);
        let mut coverage = Self {
            tree,
            database,
            workdir_prefix: options.workdir_prefix.clone(),
            diagnostics: Vec::new(),
            node_facts: HashMap::new(),
            record_bindings: HashMap::new(),
        };

        for (record_index, record) in database.records().iter().enumerate() {
            match lookups.bind_record(record, options.workdir_prefix.as_deref()) {
                Ok(binding) => {
                    coverage
                        .node_facts
                        .entry(binding.node_id)
                        .or_default()
                        .linked_record_indices
                        .push(record_index);
                    coverage
                        .node_facts
                        .entry(binding.node_id)
                        .or_default()
                        .direct_record_indices
                        .push(record_index);
                    coverage.record_bindings.insert(record.id.clone(), binding);
                }
                Err(CoverageDiagnostic::AmbiguousRecord {
                    record_id,
                    candidate_nodes,
                }) => {
                    for node_id in &candidate_nodes {
                        coverage
                            .node_facts
                            .entry(*node_id)
                            .or_default()
                            .linked_record_indices
                            .push(record_index);
                    }
                    coverage
                        .diagnostics
                        .push(CoverageDiagnostic::AmbiguousRecord {
                            record_id,
                            candidate_nodes,
                        });
                }
                Err(diagnostic) => coverage.diagnostics.push(diagnostic),
            }
        }

        for facts in coverage.node_facts.values_mut() {
            facts.finalize(database.records());
        }

        Ok(coverage)
    }

    pub fn diagnostics(&self) -> &[CoverageDiagnostic] {
        &self.diagnostics
    }

    pub fn binding_relation_for_record(&self, record_id: &str) -> Option<BindingRelation> {
        self.record_bindings
            .get(record_id)
            .map(|binding| binding.relation.clone())
    }

    pub fn node(&'a self, node_id: TreeNodeId) -> NodeCoverage<'a> {
        NodeCoverage {
            index: self,
            node_id,
        }
    }

    pub fn block(&'a self, path: &RepoPath, block: &crate::block::Block) -> BlockCoverage<'a> {
        let resolved_node_id = self.tree.find_block_node(path.as_str(), block);
        let container_node_id = resolved_node_id
            .or_else(|| self.smallest_covering_block_node(path, block))
            .or_else(|| self.tree.find_by_path(path.as_str()));

        let (direct_record_indices, linked_record_indices) = if let Some(node_id) = resolved_node_id
        {
            (
                self.direct_record_indices_for_node(node_id),
                self.linked_record_indices_for_node(node_id),
            )
        } else {
            let direct = self.matching_record_indices_for_block(path, block);
            (direct.clone(), direct)
        };

        BlockCoverage {
            index: self,
            resolved_node_id,
            container_node_id,
            path: path.clone(),
            block_hash: block.hash.clone(),
            block_start_line: block.start_line,
            direct_record_indices,
            linked_record_indices,
        }
    }

    pub fn subtree(&'a self, node_id: TreeNodeId) -> SubtreeCoverage<'a> {
        SubtreeCoverage {
            index: self,
            root_id: node_id,
        }
    }

    pub fn is_container_block_node(&self, node_id: TreeNodeId) -> bool {
        self.tree.is_container_block(node_id)
    }

    fn direct_record_indices_for_node(&self, node_id: TreeNodeId) -> Vec<usize> {
        self.node_facts
            .get(&node_id)
            .map(|facts| facts.direct_record_indices.clone())
            .unwrap_or_default()
    }

    fn linked_record_indices_for_node(&self, node_id: TreeNodeId) -> Vec<usize> {
        self.node_facts
            .get(&node_id)
            .map(|facts| facts.linked_record_indices.clone())
            .unwrap_or_default()
    }

    fn matching_record_indices_for_block(
        &self,
        path: &RepoPath,
        block: &crate::block::Block,
    ) -> Vec<usize> {
        let path_candidates = collect_path_candidates(path, self.workdir_prefix.as_deref());
        let Ok(start_line) = u32::try_from(block.start_line) else {
            return Vec::new();
        };

        let matched = self
            .database
            .records()
            .iter()
            .enumerate()
            .filter_map(|(record_index, record)| {
                record_match_relation_for_block(record, &block.hash, start_line, &path_candidates)
                    .map(|_| record_index)
            })
            .collect::<Vec<_>>();
        sort_record_indices(matched, self.database.records())
    }

    fn smallest_covering_block_node(
        &self,
        path: &RepoPath,
        block: &crate::block::Block,
    ) -> Option<TreeNodeId> {
        self.tree
            .nodes()
            .iter()
            .filter(|node| matches!(node.kind, TreeNodeKind::Block) && node.path == *path)
            .filter_map(|node| {
                let candidate = node.block.as_ref()?;
                let contains = candidate.start_line <= block.start_line
                    && candidate.end_line >= block.end_line;
                if contains {
                    Some((
                        node.id,
                        candidate.end_line.saturating_sub(candidate.start_line),
                    ))
                } else {
                    None
                }
            })
            .min_by_key(|(_, span)| *span)
            .map(|(node_id, _)| node_id)
    }

    fn binding_relation_for_record_index(&self, record_index: usize) -> Option<BindingRelation> {
        let record_id = &self.database.records()[record_index].id;
        self.binding_relation_for_record(record_id)
    }
}

pub struct NodeCoverage<'a> {
    index: &'a CoverageIndex<'a>,
    node_id: TreeNodeId,
}

impl<'a> NodeCoverage<'a> {
    pub fn linked_records(&self) -> Vec<&'a Record> {
        self.linked_record_indices_for_node(self.node_id)
            .into_iter()
            .map(|index| &self.index.database.records()[index])
            .collect()
    }

    pub fn direct_records(&self) -> Vec<&'a Record> {
        self.record_indices_for_node(self.node_id)
            .into_iter()
            .map(|index| &self.index.database.records()[index])
            .collect()
    }

    pub fn effective_records(&self) -> Vec<&'a Record> {
        let mut record_indices = Vec::new();
        for ancestor_id in self.index.tree.ancestors(self.node_id) {
            record_indices.extend(self.record_indices_for_node(ancestor_id));
        }
        sort_record_indices(record_indices, self.index.database.records())
            .into_iter()
            .map(|index| &self.index.database.records()[index])
            .collect()
    }

    pub fn direct_latest_verdict_for(&self, check: &ReviewCheck) -> Option<&'a Verdict> {
        let record_index = preferred_record_index_for_check(
            self.index.database.records(),
            &self.record_indices_for_node(self.node_id),
            check,
            |record_index| self.index.binding_relation_for_record_index(record_index),
        )?;
        Some(&self.index.database.records()[record_index].verdict)
    }

    pub fn effective_latest_verdict_for(&self, check: &ReviewCheck) -> Option<&'a Verdict> {
        for ancestor_id in self.index.tree.ancestors(self.node_id) {
            let record_indices = self.record_indices_for_node(ancestor_id);
            let Some(record_index) = preferred_record_index_for_check(
                self.index.database.records(),
                &record_indices,
                check,
                |record_index| self.index.binding_relation_for_record_index(record_index),
            ) else {
                continue;
            };
            return Some(&self.index.database.records()[record_index].verdict);
        }
        None
    }

    pub fn direct_distinct_identity_count(&self, check: &ReviewCheck) -> usize {
        self.index
            .node_facts
            .get(&self.node_id)
            .and_then(|facts| facts.direct_identities_by_check.get(check))
            .map_or(0, HashSet::len)
    }

    pub fn effective_distinct_identity_count(&self, check: &ReviewCheck) -> usize {
        let mut identities = HashSet::new();
        for ancestor_id in self.index.tree.ancestors(self.node_id) {
            let Some(facts) = self.index.node_facts.get(&ancestor_id) else {
                continue;
            };
            let Some(check_identities) = facts.direct_identities_by_check.get(check) else {
                continue;
            };
            identities.extend(check_identities.iter().cloned());
        }
        identities.len()
    }

    pub fn is_well_reviewed(&self, policy: &CoveragePolicy) -> bool {
        policy.required_checks.iter().all(|check| {
            let (verdict, identity_count) = match policy.scope {
                CoverageScope::Direct => (
                    self.direct_latest_verdict_for(check),
                    self.direct_distinct_identity_count(check),
                ),
                CoverageScope::Effective => (
                    self.effective_latest_verdict_for(check),
                    self.effective_distinct_identity_count(check),
                ),
            };

            policy.allows_verdict(verdict) && identity_count >= policy.min_distinct_identities
        })
    }

    fn record_indices_for_node(&self, node_id: TreeNodeId) -> Vec<usize> {
        self.index.direct_record_indices_for_node(node_id)
    }

    fn linked_record_indices_for_node(&self, node_id: TreeNodeId) -> Vec<usize> {
        self.index.linked_record_indices_for_node(node_id)
    }
}

pub struct BlockCoverage<'a> {
    index: &'a CoverageIndex<'a>,
    resolved_node_id: Option<TreeNodeId>,
    container_node_id: Option<TreeNodeId>,
    path: RepoPath,
    block_hash: crate::store::TreeHash,
    block_start_line: usize,
    direct_record_indices: Vec<usize>,
    linked_record_indices: Vec<usize>,
}

impl<'a> BlockCoverage<'a> {
    pub fn resolved_node_id(&self) -> Option<TreeNodeId> {
        self.resolved_node_id
    }

    pub fn direct_records(&self) -> Vec<&'a Record> {
        if let Some(node_id) = self.resolved_node_id {
            return self.index.node(node_id).direct_records();
        }
        self.direct_record_indices
            .iter()
            .map(|index| &self.index.database.records()[*index])
            .collect()
    }

    pub fn linked_records(&self) -> Vec<&'a Record> {
        self.linked_record_indices
            .iter()
            .map(|index| &self.index.database.records()[*index])
            .collect()
    }

    pub fn effective_records(&self) -> Vec<&'a Record> {
        if let Some(node_id) = self.resolved_node_id {
            return self.index.node(node_id).effective_records();
        }

        let mut record_indices = self.direct_record_indices.clone();
        if let Some(container_node_id) = self.container_node_id {
            for ancestor_id in self.index.tree.ancestors(container_node_id) {
                record_indices.extend(self.index.direct_record_indices_for_node(ancestor_id));
            }
        }

        unique_sorted_record_indices(record_indices, self.index.database.records())
            .into_iter()
            .map(|index| &self.index.database.records()[index])
            .collect()
    }

    pub fn direct_latest_verdict_for(&self, check: &ReviewCheck) -> Option<&'a Verdict> {
        if let Some(node_id) = self.resolved_node_id {
            return self.index.node(node_id).direct_latest_verdict_for(check);
        }

        let path_candidates =
            collect_path_candidates(&self.path, self.index.workdir_prefix.as_deref());
        let start_line = u32::try_from(self.block_start_line).ok()?;
        let record_index = preferred_record_index_for_check(
            self.index.database.records(),
            &self.direct_record_indices,
            check,
            |record_index| {
                record_match_relation_for_block(
                    &self.index.database.records()[record_index],
                    &self.block_hash,
                    start_line,
                    &path_candidates,
                )
            },
        )?;
        Some(&self.index.database.records()[record_index].verdict)
    }

    pub fn effective_latest_verdict_for(&self, check: &ReviewCheck) -> Option<&'a Verdict> {
        if let Some(verdict) = self.direct_latest_verdict_for(check) {
            return Some(verdict);
        }
        let container_node_id = self.container_node_id?;
        self.index
            .node(container_node_id)
            .effective_latest_verdict_for(check)
    }

    pub fn direct_distinct_identity_count(&self, check: &ReviewCheck) -> usize {
        self.direct_records()
            .into_iter()
            .filter(|record| &record.check == check)
            .map(|record| identity_key(&record.identity))
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn effective_distinct_identity_count(&self, check: &ReviewCheck) -> usize {
        self.effective_records()
            .into_iter()
            .filter(|record| &record.check == check)
            .map(|record| identity_key(&record.identity))
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn is_well_reviewed(&self, policy: &CoveragePolicy) -> bool {
        policy.required_checks.iter().all(|check| {
            let (verdict, identity_count) = match policy.scope {
                CoverageScope::Direct => (
                    self.direct_latest_verdict_for(check),
                    self.direct_distinct_identity_count(check),
                ),
                CoverageScope::Effective => (
                    self.effective_latest_verdict_for(check),
                    self.effective_distinct_identity_count(check),
                ),
            };

            policy.allows_verdict(verdict) && identity_count >= policy.min_distinct_identities
        })
    }
}

pub struct SubtreeCoverage<'a> {
    index: &'a CoverageIndex<'a>,
    root_id: TreeNodeId,
}

impl<'a> SubtreeCoverage<'a> {
    pub fn descendant_block_nodes(&self) -> Vec<TreeNodeId> {
        let mut descendants = Vec::new();
        let mut stack = self
            .index
            .tree
            .node(self.root_id)
            .children
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>();

        while let Some(node_id) = stack.pop() {
            let node = self.index.tree.node(node_id);
            for child_id in node.children.iter().rev() {
                stack.push(*child_id);
            }
            if matches!(node.kind, TreeNodeKind::Block) {
                descendants.push(node_id);
            }
        }

        descendants
    }

    pub fn all_descendant_blocks_well_reviewed(&self, policy: &CoveragePolicy) -> bool {
        self.descendant_block_nodes()
            .into_iter()
            .all(|node_id| self.index.node(node_id).is_well_reviewed(policy))
    }
}

#[derive(Debug, Clone)]
struct RecordBinding {
    node_id: TreeNodeId,
    relation: BindingRelation,
}

#[derive(Debug, Default)]
struct NodeCoverageFacts {
    linked_record_indices: Vec<usize>,
    direct_record_indices: Vec<usize>,
    direct_identities_by_check: HashMap<ReviewCheck, HashSet<String>>,
}

impl NodeCoverageFacts {
    fn finalize(&mut self, records: &[Record]) {
        self.linked_record_indices =
            sort_record_indices(self.linked_record_indices.clone(), records);
        self.direct_record_indices =
            sort_record_indices(self.direct_record_indices.clone(), records);

        for record_index in &self.direct_record_indices {
            let record = &records[*record_index];
            self.direct_identities_by_check
                .entry(record.check.clone())
                .or_default()
                .insert(identity_key(&record.identity));
        }
    }
}

#[derive(Debug, Default)]
struct TreeCoverageLookups {
    block_exact: HashMap<(RepoPath, crate::store::TreeHash, u32), Vec<TreeNodeId>>,
    block_by_path_hash: HashMap<(RepoPath, crate::store::TreeHash), Vec<TreeNodeId>>,
    block_by_hash: HashMap<crate::store::TreeHash, Vec<TreeNodeId>>,
    file_by_path_hash: HashMap<(RepoPath, crate::store::TreeHash), Vec<TreeNodeId>>,
    file_by_hash: HashMap<crate::store::TreeHash, Vec<TreeNodeId>>,
    tree_by_path_hash: HashMap<(RepoPath, crate::store::TreeHash), Vec<TreeNodeId>>,
    tree_by_hash: HashMap<crate::store::TreeHash, Vec<TreeNodeId>>,
}

impl TreeCoverageLookups {
    fn from_tree(tree: &Tree) -> Self {
        let mut lookups = Self::default();

        for node in tree.nodes() {
            match node.kind {
                TreeNodeKind::Block => {
                    let Some(block) = node.block.as_ref() else {
                        continue;
                    };
                    push_lookup(
                        &mut lookups.block_exact,
                        (
                            node.path.clone(),
                            node.hash.clone(),
                            u32::try_from(block.start_line).unwrap_or(u32::MAX),
                        ),
                        node.id,
                    );
                    push_lookup(
                        &mut lookups.block_by_path_hash,
                        (node.path.clone(), node.hash.clone()),
                        node.id,
                    );
                    push_lookup(&mut lookups.block_by_hash, node.hash.clone(), node.id);
                }
                TreeNodeKind::File => {
                    push_lookup(
                        &mut lookups.file_by_path_hash,
                        (node.path.clone(), node.hash.clone()),
                        node.id,
                    );
                    push_lookup(&mut lookups.file_by_hash, node.hash.clone(), node.id);
                }
                TreeNodeKind::Root | TreeNodeKind::Directory => {
                    push_lookup(
                        &mut lookups.tree_by_path_hash,
                        (node.path.clone(), node.hash.clone()),
                        node.id,
                    );
                    push_lookup(&mut lookups.tree_by_hash, node.hash.clone(), node.id);
                }
            }
        }

        lookups
    }

    fn bind_record(
        &self,
        record: &Record,
        workdir_prefix: Option<&str>,
    ) -> Result<RecordBinding, CoverageDiagnostic> {
        match &record.target {
            ReviewTargetRef::Block { hash } => self.bind_block_record(record, hash, workdir_prefix),
            ReviewTargetRef::File { hash } => self.bind_path_target_record(
                record,
                hash,
                workdir_prefix,
                &self.file_by_path_hash,
                &self.file_by_hash,
            ),
            ReviewTargetRef::Tree { hash } => self.bind_path_target_record(
                record,
                hash,
                workdir_prefix,
                &self.tree_by_path_hash,
                &self.tree_by_hash,
            ),
        }
    }

    fn bind_block_record(
        &self,
        record: &Record,
        hash: &crate::store::TreeHash,
        workdir_prefix: Option<&str>,
    ) -> Result<RecordBinding, CoverageDiagnostic> {
        if let (Some(path_hint), Some(line_hint)) = (&record.path_hint, record.line_hint) {
            let exact_candidates = collect_path_candidates(path_hint, workdir_prefix)
                .into_iter()
                .flat_map(|path| {
                    self.block_exact
                        .get(&(path, hash.clone(), line_hint))
                        .into_iter()
                        .flatten()
                        .copied()
                })
                .collect::<Vec<_>>();
            match unique_node_candidates(exact_candidates).as_slice() {
                [node_id] => {
                    return Ok(RecordBinding {
                        node_id: *node_id,
                        relation: BindingRelation::Exact,
                    });
                }
                [] => {}
                candidates => return Err(ambiguous_record(record, candidates.to_vec())),
            }
        }

        if let Some(path_hint) = &record.path_hint {
            let path_scoped_candidates = collect_path_candidates(path_hint, workdir_prefix)
                .into_iter()
                .flat_map(|path| {
                    self.block_by_path_hash
                        .get(&(path, hash.clone()))
                        .into_iter()
                        .flatten()
                        .copied()
                })
                .collect::<Vec<_>>();
            match unique_node_candidates(path_scoped_candidates).as_slice() {
                [node_id] => {
                    return Ok(RecordBinding {
                        node_id: *node_id,
                        relation: BindingRelation::PathScoped,
                    });
                }
                [] => {}
                candidates => return Err(ambiguous_record(record, candidates.to_vec())),
            }
        }

        match self
            .block_by_hash
            .get(hash)
            .cloned()
            .map(unique_node_candidates)
            .unwrap_or_default()
            .as_slice()
        {
            [node_id] => Ok(RecordBinding {
                node_id: *node_id,
                relation: BindingRelation::HashOnly,
            }),
            [] => Err(unresolved_record(record)),
            candidates => Err(ambiguous_record(record, candidates.to_vec())),
        }
    }

    fn bind_path_target_record(
        &self,
        record: &Record,
        hash: &crate::store::TreeHash,
        workdir_prefix: Option<&str>,
        by_path_hash: &HashMap<(RepoPath, crate::store::TreeHash), Vec<TreeNodeId>>,
        by_hash: &HashMap<crate::store::TreeHash, Vec<TreeNodeId>>,
    ) -> Result<RecordBinding, CoverageDiagnostic> {
        if let Some(path_hint) = &record.path_hint {
            let candidates = collect_path_candidates(path_hint, workdir_prefix)
                .into_iter()
                .flat_map(|path| {
                    by_path_hash
                        .get(&(path, hash.clone()))
                        .into_iter()
                        .flatten()
                        .copied()
                })
                .collect::<Vec<_>>();
            match unique_node_candidates(candidates).as_slice() {
                [node_id] => {
                    return Ok(RecordBinding {
                        node_id: *node_id,
                        relation: BindingRelation::TargetNode,
                    });
                }
                [] => {}
                candidates => return Err(ambiguous_record(record, candidates.to_vec())),
            }
        }

        match by_hash
            .get(hash)
            .cloned()
            .map(unique_node_candidates)
            .unwrap_or_default()
            .as_slice()
        {
            [node_id] => Ok(RecordBinding {
                node_id: *node_id,
                relation: BindingRelation::HashOnly,
            }),
            [] => Err(unresolved_record(record)),
            candidates => Err(ambiguous_record(record, candidates.to_vec())),
        }
    }
}

fn push_lookup<K>(entries: &mut HashMap<K, Vec<TreeNodeId>>, key: K, node_id: TreeNodeId)
where
    K: Eq + std::hash::Hash,
{
    entries.entry(key).or_default().push(node_id);
}

fn sort_record_indices(mut record_indices: Vec<usize>, records: &[Record]) -> Vec<usize> {
    record_indices.sort_by_key(|record_index| {
        let record = &records[*record_index];
        (record.timestamp, *record_index)
    });
    record_indices
}

fn unique_sorted_record_indices(record_indices: Vec<usize>, records: &[Record]) -> Vec<usize> {
    let mut sorted = sort_record_indices(record_indices, records);
    sorted.dedup();
    sorted
}

fn collect_path_candidates(path_hint: &RepoPath, workdir_prefix: Option<&str>) -> Vec<RepoPath> {
    if path_hint.is_root() {
        return vec![RepoPath::root()];
    }

    let mut candidates = Vec::new();
    for candidate in path_utils::repo_path_candidates(path_hint.as_str(), workdir_prefix, None)
        .into_iter()
        .chain(path_utils::tree_path_candidates_for_repo_path(
            path_hint.as_str(),
            workdir_prefix,
        ))
    {
        let Ok(candidate) = RepoPath::new(candidate) else {
            continue;
        };
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    if candidates.is_empty() {
        candidates.push(path_hint.clone());
    }
    candidates
}

fn unique_node_candidates(node_ids: Vec<TreeNodeId>) -> Vec<TreeNodeId> {
    let mut unique = Vec::new();
    for node_id in node_ids {
        if !unique.contains(&node_id) {
            unique.push(node_id);
        }
    }
    unique
}

fn preferred_record_index_for_check<F>(
    records: &[Record],
    record_indices: &[usize],
    check: &ReviewCheck,
    relation_for_record: F,
) -> Option<usize>
where
    F: Fn(usize) -> Option<BindingRelation>,
{
    let mut best: Option<(u8, i64, usize)> = None;

    for record_index in record_indices {
        let record = &records[*record_index];
        if &record.check != check {
            continue;
        }

        let relation = relation_for_record(*record_index);
        let candidate = (
            binding_relation_priority(relation.as_ref()),
            record.timestamp,
            *record_index,
        );

        match best {
            Some((best_priority, best_timestamp, best_index)) => {
                if candidate.0 < best_priority
                    || (candidate.0 == best_priority
                        && (candidate.1 > best_timestamp
                            || (candidate.1 == best_timestamp && candidate.2 > best_index)))
                {
                    best = Some(candidate);
                }
            }
            None => best = Some(candidate),
        }
    }

    best.map(|(_, _, record_index)| record_index)
}

fn binding_relation_priority(relation: Option<&BindingRelation>) -> u8 {
    match relation {
        Some(BindingRelation::Exact) => 0,
        Some(BindingRelation::PathScoped) | Some(BindingRelation::TargetNode) => 1,
        Some(BindingRelation::HashOnly) => 2,
        None => 3,
    }
}

fn record_match_relation_for_block(
    record: &Record,
    hash: &crate::store::TreeHash,
    start_line: u32,
    candidate_paths: &[RepoPath],
) -> Option<BindingRelation> {
    let ReviewTargetRef::Block { hash: record_hash } = &record.target else {
        return None;
    };
    if record_hash != hash {
        return None;
    }

    match (&record.path_hint, record.line_hint) {
        (Some(path_hint), Some(line_hint))
            if line_hint == start_line && candidate_paths.iter().any(|path| path == path_hint) =>
        {
            Some(BindingRelation::Exact)
        }
        (Some(path_hint), None) if candidate_paths.iter().any(|path| path == path_hint) => {
            Some(BindingRelation::PathScoped)
        }
        (None, _) => Some(BindingRelation::HashOnly),
        _ => None,
    }
}

fn ambiguous_record(record: &Record, candidate_nodes: Vec<TreeNodeId>) -> CoverageDiagnostic {
    CoverageDiagnostic::AmbiguousRecord {
        record_id: record.id.clone(),
        candidate_nodes,
    }
}

fn unresolved_record(record: &Record) -> CoverageDiagnostic {
    CoverageDiagnostic::UnresolvedRecord {
        record_id: record.id.clone(),
    }
}

fn identity_key(identity: &Identity) -> String {
    match identity {
        Identity::Email { email } => email.trim().to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind};
    use crate::hashing::{BytesHash, TreeHash};
    use crate::repo_path::RepoPath;
    use crate::store::{
        BlockState, CommitId, Identity, Record, RepoRef, ReviewTargetRef, VcsSystem,
    };
    use crate::tree::TreeBuilder;

    #[test]
    fn block_coverage_distinguishes_direct_and_inherited_reviews() {
        let (tree, file_id, function_id, _, _) = build_function_tree();
        let file_hash = tree.node(file_id).hash.clone();
        let function_hash = tree.node(function_id).hash.clone();
        let records = vec![
            file_record(
                "1",
                file_hash,
                "review",
                Verdict::Approved,
                "alice@example.com",
                1,
            ),
            block_record(
                "2",
                function_hash,
                "src/lib.rs",
                1,
                "security",
                Verdict::Approved,
                "bob@example.com",
                2,
            ),
        ];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();
        let security = ReviewCheck::new("security").unwrap();

        assert_eq!(
            coverage
                .node(function_id)
                .direct_latest_verdict_for(&ReviewCheck::review()),
            None
        );
        assert_eq!(
            coverage
                .node(function_id)
                .effective_latest_verdict_for(&ReviewCheck::review()),
            Some(&Verdict::Approved)
        );
        assert_eq!(
            coverage
                .node(function_id)
                .direct_distinct_identity_count(&ReviewCheck::review()),
            0
        );
        assert_eq!(
            coverage
                .node(function_id)
                .effective_distinct_identity_count(&ReviewCheck::review()),
            1
        );
        assert_eq!(
            coverage
                .node(function_id)
                .direct_latest_verdict_for(&security),
            Some(&Verdict::Approved)
        );
    }

    #[test]
    fn single_review_policy_defaults_to_direct_approved_scope() {
        let policy = CoveragePolicy::single_review();

        assert_eq!(policy.scope, CoverageScope::Direct);
        assert_eq!(policy.verdict_requirement, VerdictRequirement::ApprovedOnly);
        assert_eq!(policy.min_distinct_identities, 1);
    }

    #[test]
    fn effective_scope_policy_counts_inherited_reviews() {
        let (tree, file_id, function_id, _, _) = build_function_tree();
        let file_hash = tree.node(file_id).hash.clone();
        let records = vec![file_record(
            "1",
            file_hash,
            "review",
            Verdict::Approved,
            "alice@example.com",
            1,
        )];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert!(
            !coverage
                .node(function_id)
                .is_well_reviewed(&CoveragePolicy::single_review())
        );
        assert!(coverage.node(function_id).is_well_reviewed(
            &CoveragePolicy::single_review().with_scope(CoverageScope::Effective)
        ));
    }

    #[test]
    fn exact_block_binding_supports_two_person_review_policy() {
        let (tree, _, function_id, _, _) = build_function_tree();
        let function_hash = tree.node(function_id).hash.clone();
        let records = vec![
            block_record(
                "1",
                function_hash.clone(),
                "src/lib.rs",
                1,
                "review",
                Verdict::Approved,
                "alice@example.com",
                1,
            ),
            block_record(
                "2",
                function_hash,
                "src/lib.rs",
                1,
                "review",
                Verdict::Approved,
                "bob@example.com",
                2,
            ),
        ];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(
            coverage
                .node(function_id)
                .direct_distinct_identity_count(&ReviewCheck::review()),
            2
        );
        assert!(
            coverage
                .node(function_id)
                .is_well_reviewed(&CoveragePolicy::two_person_review())
        );
    }

    #[test]
    fn hash_only_block_record_is_reported_as_ambiguous() {
        let (tree, first_id, second_id, first_hash, second_hash) = build_duplicate_hash_tree();
        assert_eq!(first_hash, second_hash);

        let records = vec![hash_only_block_record(
            "1",
            first_hash,
            "review",
            Verdict::Approved,
            "alice@example.com",
            1,
        )];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(coverage.diagnostics().len(), 1);
        assert_eq!(coverage.binding_relation_for_record("1"), None);
        assert_eq!(coverage.node(first_id).linked_records().len(), 1);
        assert_eq!(coverage.node(second_id).linked_records().len(), 1);
        assert_eq!(
            coverage
                .node(first_id)
                .direct_latest_verdict_for(&ReviewCheck::review()),
            None
        );
        assert_eq!(
            coverage
                .node(second_id)
                .direct_latest_verdict_for(&ReviewCheck::review()),
            None
        );
    }

    #[test]
    fn subtree_queries_descendant_block_review_status() {
        let (tree, container_id, first_method_id, second_method_id) = build_container_tree();
        let first_hash = tree.node(first_method_id).hash.clone();
        let second_hash = tree.node(second_method_id).hash.clone();
        let records = vec![
            block_record(
                "1",
                first_hash,
                "src/lib.rs",
                2,
                "review",
                Verdict::Approved,
                "alice@example.com",
                1,
            ),
            block_record(
                "2",
                second_hash,
                "src/lib.rs",
                4,
                "review",
                Verdict::Approved,
                "bob@example.com",
                2,
            ),
        ];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(
            coverage
                .subtree(container_id)
                .descendant_block_nodes()
                .len(),
            2
        );
        assert!(
            coverage
                .subtree(container_id)
                .all_descendant_blocks_well_reviewed(&CoveragePolicy::single_review())
        );
    }

    fn build_function_tree() -> (Tree, TreeNodeId, TreeNodeId, TreeHash, TreeHash) {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file_id = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let function = Block::new("fn helper() {}\n".to_string(), BlockKind::Function, 1, 2);
        let function_hash = function.hash.clone();
        let function_id = builder.add_block(
            file_id,
            "fn helper".to_string(),
            "src/lib.rs".to_string(),
            function,
            Language::Rust,
        );
        let tree = builder.finalize();
        let file_hash = tree.node(file_id).hash.clone();
        (tree, file_id, function_id, file_hash, function_hash)
    }

    fn build_duplicate_hash_tree() -> (Tree, TreeNodeId, TreeNodeId, TreeHash, TreeHash) {
        let shared_content = "fn dup() {}\n".to_string();
        let first = Block::new(shared_content.clone(), BlockKind::Function, 1, 2);
        let second = Block::new(shared_content, BlockKind::Function, 10, 11);

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
        let first_hash = first.hash.clone();
        let second_hash = second.hash.clone();
        let first_id = builder.add_block(
            file,
            "dup-1".to_string(),
            "src/lib.rs".to_string(),
            first,
            Language::Rust,
        );
        let second_id = builder.add_block(
            file,
            "dup-2".to_string(),
            "src/lib.rs".to_string(),
            second,
            Language::Rust,
        );

        let tree = builder.finalize();
        (tree, first_id, second_id, first_hash, second_hash)
    }

    fn build_container_tree() -> (Tree, TreeNodeId, TreeNodeId, TreeNodeId) {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            BytesHash::new("bytes").to_string(),
            Language::Rust,
        );
        let container = Block::new(
            "impl Worker {\n    fn start(&self) {}\n\n    fn stop(&self) {}\n}\n".to_string(),
            BlockKind::Impl,
            1,
            6,
        );
        let first_method = Block::new("fn start(&self) {}\n".to_string(), BlockKind::Method, 2, 3);
        let second_method = Block::new("fn stop(&self) {}\n".to_string(), BlockKind::Method, 4, 5);

        let container_id = builder.add_block(
            file,
            "impl Worker".to_string(),
            "src/lib.rs".to_string(),
            container,
            Language::Rust,
        );
        let first_method_id = builder.add_block(
            container_id,
            "fn start".to_string(),
            "src/lib.rs".to_string(),
            first_method,
            Language::Rust,
        );
        let second_method_id = builder.add_block(
            container_id,
            "fn stop".to_string(),
            "src/lib.rs".to_string(),
            second_method,
            Language::Rust,
        );

        (
            builder.finalize(),
            container_id,
            first_method_id,
            second_method_id,
        )
    }

    fn file_record(
        id: &str,
        hash: TreeHash,
        check: &str,
        verdict: Verdict,
        email: &str,
        timestamp: i64,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: crate::store::CURRENT_VERSION,
            target: ReviewTargetRef::File { hash },
            check: ReviewCheck::new(check).unwrap(),
            verdict,
            identity: Identity::Email {
                email: email.to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new("0123456789abcdef").unwrap(),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: Some(RepoPath::new("src/lib.rs").unwrap()),
            line_hint: None,
            note: None,
            comment_scope: None,
            comment_context: None,
            comment_anchor: None,
            tags: None,
            attestations: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn block_record(
        id: &str,
        hash: TreeHash,
        path: &str,
        start_line: u32,
        check: &str,
        verdict: Verdict,
        email: &str,
        timestamp: i64,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: crate::store::CURRENT_VERSION,
            target: ReviewTargetRef::Block { hash },
            check: ReviewCheck::new(check).unwrap(),
            verdict,
            identity: Identity::Email {
                email: email.to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new("0123456789abcdef").unwrap(),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: Some(RepoPath::new(path).unwrap()),
            line_hint: Some(start_line),
            note: None,
            comment_scope: None,
            comment_context: None,
            comment_anchor: None,
            tags: None,
            attestations: None,
        }
    }

    fn hash_only_block_record(
        id: &str,
        hash: TreeHash,
        check: &str,
        verdict: Verdict,
        email: &str,
        timestamp: i64,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: crate::store::CURRENT_VERSION,
            target: ReviewTargetRef::Block { hash },
            check: ReviewCheck::new(check).unwrap(),
            verdict,
            identity: Identity::Email {
                email: email.to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new("0123456789abcdef").unwrap(),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: None,
            line_hint: None,
            note: None,
            comment_scope: None,
            comment_context: None,
            comment_anchor: None,
            tags: None,
            attestations: None,
        }
    }
}
