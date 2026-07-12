use crate::block::{Block, ByteSpan};
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::store::{Identity, Record, ReviewCheck, ReviewDatabase, ReviewTargetRef, Verdict};
use crate::sub_splitter::{self, SubSplitSemantics};
use crate::tree::{Tree, TreeNode, TreeNodeId, TreeNodeKind};
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
        candidates: Vec<CoverageCandidate>,
    },
    UnresolvedRecord {
        record_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoverageCandidate {
    Block {
        path: RepoPath,
        hash: crate::store::TreeHash,
        start_line: usize,
        end_line: usize,
        start_byte: usize,
        end_byte: usize,
    },
    TreeNode {
        node_id: TreeNodeId,
        path: RepoPath,
        hash: crate::store::TreeHash,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CoverageUnitId {
    Node(TreeNodeId),
    Generated(CoverageBlockLocator),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CoverageBlockLocator {
    path: RepoPath,
    hash: crate::store::TreeHash,
    byte_span: ByteSpan,
}

impl CoverageBlockLocator {
    fn new(path: &RepoPath, block: &Block) -> Self {
        Self {
            path: path.clone(),
            hash: block.hash.clone(),
            byte_span: block.byte_span(),
        }
    }
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
    diagnostics: Vec<CoverageDiagnostic>,
    unit_facts: HashMap<CoverageUnitId, CoverageFacts>,
    block_units: HashMap<CoverageBlockLocator, CoverageUnitId>,
    record_bindings: HashMap<String, RecordBinding>,
}

impl<'a> CoverageIndex<'a> {
    pub fn build(
        tree: &'a Tree,
        database: &'a ReviewDatabase,
        options: &CoverageBuildOptions,
    ) -> anyhow::Result<Self> {
        let lookups = CoverageLookups::from_tree(tree);
        let mut coverage = Self {
            tree,
            database,
            diagnostics: Vec::new(),
            unit_facts: HashMap::new(),
            block_units: lookups.block_units.clone(),
            record_bindings: HashMap::new(),
        };

        for (record_index, record) in database.records().iter().enumerate() {
            match lookups.bind_record(record, options.workdir_prefix.as_deref()) {
                Ok(binding) => {
                    let facts = coverage
                        .unit_facts
                        .entry(binding.unit_id.clone())
                        .or_default();
                    facts.linked_record_indices.push(record_index);
                    facts.direct_record_indices.push(record_index);
                    coverage.record_bindings.insert(record.id.clone(), binding);
                }
                Err(RecordBindingFailure::Ambiguous {
                    record_id,
                    candidates,
                }) => {
                    for unit_id in &candidates {
                        coverage
                            .unit_facts
                            .entry(unit_id.clone())
                            .or_default()
                            .linked_record_indices
                            .push(record_index);
                    }
                    coverage
                        .diagnostics
                        .push(lookups.ambiguous_diagnostic(record_id, candidates));
                }
                Err(RecordBindingFailure::Unresolved { record_id }) => {
                    coverage
                        .diagnostics
                        .push(CoverageDiagnostic::UnresolvedRecord { record_id });
                }
            }
        }

        for facts in coverage.unit_facts.values_mut() {
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

    pub fn block(&'a self, path: &RepoPath, block: &Block) -> BlockCoverage<'a> {
        let resolved_node_id = self.tree.find_block_node(path.as_str(), block);
        let container_node_id = resolved_node_id
            .or_else(|| self.smallest_covering_block_node(path, block))
            .or_else(|| self.tree.find_by_path(path.as_str()));
        let locator = CoverageBlockLocator::new(path, block);
        let facts = self
            .block_units
            .get(&locator)
            .and_then(|unit_id| self.unit_facts.get(unit_id));

        BlockCoverage {
            index: self,
            resolved_node_id,
            container_node_id,
            direct_record_indices: facts
                .map(|facts| facts.direct_record_indices.clone())
                .unwrap_or_default(),
            linked_record_indices: facts
                .map(|facts| facts.linked_record_indices.clone())
                .unwrap_or_default(),
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
        self.direct_record_indices_for_unit(&CoverageUnitId::Node(node_id))
    }

    fn linked_record_indices_for_node(&self, node_id: TreeNodeId) -> Vec<usize> {
        self.linked_record_indices_for_unit(&CoverageUnitId::Node(node_id))
    }

    fn direct_record_indices_for_unit(&self, unit_id: &CoverageUnitId) -> Vec<usize> {
        self.unit_facts
            .get(unit_id)
            .map(|facts| facts.direct_record_indices.clone())
            .unwrap_or_default()
    }

    fn linked_record_indices_for_unit(&self, unit_id: &CoverageUnitId) -> Vec<usize> {
        self.unit_facts
            .get(unit_id)
            .map(|facts| facts.linked_record_indices.clone())
            .unwrap_or_default()
    }

    fn smallest_covering_block_node(&self, path: &RepoPath, block: &Block) -> Option<TreeNodeId> {
        self.tree
            .nodes()
            .iter()
            .filter(|node| matches!(node.kind, TreeNodeKind::Block) && node.path == *path)
            .filter_map(|node| {
                let candidate = node.block.as_ref()?;
                let byte_span = candidate.byte_span();
                if byte_span.contains(&block.byte_span()) {
                    Some((
                        node.id,
                        byte_span.len(),
                        byte_span.start_byte,
                        byte_span.end_byte,
                    ))
                } else {
                    None
                }
            })
            .min_by_key(|(_, len, start, end)| (*len, *start, *end))
            .map(|(node_id, _, _, _)| node_id)
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
            .unit_facts
            .get(&CoverageUnitId::Node(self.node_id))
            .and_then(|facts| facts.direct_identities_by_check.get(check))
            .map_or(0, HashSet::len)
    }

    pub fn effective_distinct_identity_count(&self, check: &ReviewCheck) -> usize {
        let mut identities = HashSet::new();
        for ancestor_id in self.index.tree.ancestors(self.node_id) {
            let Some(facts) = self
                .index
                .unit_facts
                .get(&CoverageUnitId::Node(ancestor_id))
            else {
                continue;
            };
            let Some(check_identities) = facts.direct_identities_by_check.get(check) else {
                continue;
            };
            identities.extend(check_identities.iter().cloned());
        }
        identities.len()
    }

    fn direct_distinct_identity_count_matching(
        &self,
        check: &ReviewCheck,
        verdict_requirement: VerdictRequirement,
    ) -> usize {
        distinct_identity_count_matching(
            self.index
                .direct_record_indices_for_node(self.node_id)
                .into_iter()
                .map(|index| &self.index.database.records()[index]),
            check,
            verdict_requirement,
        )
    }

    fn effective_distinct_identity_count_matching(
        &self,
        check: &ReviewCheck,
        verdict_requirement: VerdictRequirement,
    ) -> usize {
        distinct_identity_count_matching(
            self.effective_records().into_iter(),
            check,
            verdict_requirement,
        )
    }

    pub fn is_well_reviewed(&self, policy: &CoveragePolicy) -> bool {
        policy.required_checks.iter().all(|check| {
            let (verdict, identity_count) = match policy.scope {
                CoverageScope::Direct => (
                    self.direct_latest_verdict_for(check),
                    self.direct_distinct_identity_count_matching(check, policy.verdict_requirement),
                ),
                CoverageScope::Effective => (
                    self.effective_latest_verdict_for(check),
                    self.effective_distinct_identity_count_matching(
                        check,
                        policy.verdict_requirement,
                    ),
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
    direct_record_indices: Vec<usize>,
    linked_record_indices: Vec<usize>,
}

impl<'a> BlockCoverage<'a> {
    pub fn resolved_node_id(&self) -> Option<TreeNodeId> {
        self.resolved_node_id
    }

    pub fn direct_records(&self) -> Vec<&'a Record> {
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
        let mut record_indices = self.direct_record_indices.clone();
        if let Some(node_id) = self.resolved_node_id.or(self.container_node_id) {
            for ancestor_id in self.index.tree.ancestors(node_id) {
                record_indices.extend(self.index.direct_record_indices_for_node(ancestor_id));
            }
        }

        unique_sorted_record_indices(record_indices, self.index.database.records())
            .into_iter()
            .map(|index| &self.index.database.records()[index])
            .collect()
    }

    pub fn direct_latest_verdict_for(&self, check: &ReviewCheck) -> Option<&'a Verdict> {
        let record_index = preferred_record_index_for_check(
            self.index.database.records(),
            &self.direct_record_indices,
            check,
            |record_index| self.index.binding_relation_for_record_index(record_index),
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

    fn direct_distinct_identity_count_matching(
        &self,
        check: &ReviewCheck,
        verdict_requirement: VerdictRequirement,
    ) -> usize {
        distinct_identity_count_matching(
            self.direct_records().into_iter(),
            check,
            verdict_requirement,
        )
    }

    fn effective_distinct_identity_count_matching(
        &self,
        check: &ReviewCheck,
        verdict_requirement: VerdictRequirement,
    ) -> usize {
        distinct_identity_count_matching(
            self.effective_records().into_iter(),
            check,
            verdict_requirement,
        )
    }

    pub fn is_well_reviewed(&self, policy: &CoveragePolicy) -> bool {
        policy.required_checks.iter().all(|check| {
            let (verdict, identity_count) = match policy.scope {
                CoverageScope::Direct => (
                    self.direct_latest_verdict_for(check),
                    self.direct_distinct_identity_count_matching(check, policy.verdict_requirement),
                ),
                CoverageScope::Effective => (
                    self.effective_latest_verdict_for(check),
                    self.effective_distinct_identity_count_matching(
                        check,
                        policy.verdict_requirement,
                    ),
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
    unit_id: CoverageUnitId,
    relation: BindingRelation,
}

#[derive(Debug)]
enum RecordBindingFailure {
    Ambiguous {
        record_id: String,
        candidates: Vec<CoverageUnitId>,
    },
    Unresolved {
        record_id: String,
    },
}

#[derive(Debug, Default)]
struct CoverageFacts {
    linked_record_indices: Vec<usize>,
    direct_record_indices: Vec<usize>,
    direct_identities_by_check: HashMap<ReviewCheck, HashSet<String>>,
}

impl CoverageFacts {
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
struct CoverageLookups {
    block_units: HashMap<CoverageBlockLocator, CoverageUnitId>,
    block_exact: HashMap<(RepoPath, crate::store::TreeHash, u32), Vec<CoverageUnitId>>,
    block_by_path_hash: HashMap<(RepoPath, crate::store::TreeHash), Vec<CoverageUnitId>>,
    block_by_hash: HashMap<crate::store::TreeHash, Vec<CoverageUnitId>>,
    file_by_path_hash: HashMap<(RepoPath, crate::store::TreeHash), Vec<CoverageUnitId>>,
    file_by_hash: HashMap<crate::store::TreeHash, Vec<CoverageUnitId>>,
    tree_by_path_hash: HashMap<(RepoPath, crate::store::TreeHash), Vec<CoverageUnitId>>,
    tree_by_hash: HashMap<crate::store::TreeHash, Vec<CoverageUnitId>>,
    candidate_descriptions: HashMap<CoverageUnitId, CoverageCandidate>,
}

impl CoverageLookups {
    fn from_tree(tree: &Tree) -> Self {
        let mut lookups = Self::default();

        for node in tree.nodes() {
            match node.kind {
                TreeNodeKind::Block => lookups.register_tree_block(node),
                TreeNodeKind::File => {
                    let unit_id = lookups.register_tree_node(node);
                    push_lookup(
                        &mut lookups.file_by_path_hash,
                        (node.path.clone(), node.hash.clone()),
                        unit_id.clone(),
                    );
                    push_lookup(&mut lookups.file_by_hash, node.hash.clone(), unit_id);
                }
                TreeNodeKind::Root | TreeNodeKind::Directory => {
                    let unit_id = lookups.register_tree_node(node);
                    push_lookup(
                        &mut lookups.tree_by_path_hash,
                        (node.path.clone(), node.hash.clone()),
                        unit_id.clone(),
                    );
                    push_lookup(&mut lookups.tree_by_hash, node.hash.clone(), unit_id);
                }
            }
        }

        for node in tree.nodes() {
            let Some(block) = node.block.as_ref() else {
                continue;
            };
            let Some(language) = node.language else {
                continue;
            };
            let Ok(split) = sub_splitter::split_result(block, language) else {
                continue;
            };
            if split.semantics != SubSplitSemantics::ReviewUnits {
                continue;
            }
            for generated_block in split.blocks {
                lookups.register_generated_block(node.path.clone(), &generated_block);
            }
        }

        lookups
    }

    fn register_tree_node(&mut self, node: &TreeNode) -> CoverageUnitId {
        let unit_id = CoverageUnitId::Node(node.id);
        self.candidate_descriptions
            .entry(unit_id.clone())
            .or_insert_with(|| CoverageCandidate::TreeNode {
                node_id: node.id,
                path: node.path.clone(),
                hash: node.hash.clone(),
            });
        unit_id
    }

    fn register_tree_block(&mut self, node: &TreeNode) {
        let Some(block) = node.block.as_ref() else {
            return;
        };
        self.register_block_candidate(node.path.clone(), block, Some(node.id));
    }

    fn register_generated_block(&mut self, path: RepoPath, block: &Block) {
        self.register_block_candidate(path, block, None);
    }

    fn register_block_candidate(
        &mut self,
        path: RepoPath,
        block: &Block,
        tree_node_id: Option<TreeNodeId>,
    ) {
        let locator = CoverageBlockLocator::new(&path, block);
        if self.block_units.contains_key(&locator) {
            return;
        }

        let unit_id = match tree_node_id {
            Some(node_id) => CoverageUnitId::Node(node_id),
            None => CoverageUnitId::Generated(locator.clone()),
        };
        self.block_units.insert(locator, unit_id.clone());
        self.candidate_descriptions.insert(
            unit_id.clone(),
            CoverageCandidate::Block {
                path: path.clone(),
                hash: block.hash.clone(),
                start_line: block.start_line,
                end_line: block.end_line,
                start_byte: block.start_byte,
                end_byte: block.end_byte,
            },
        );

        if let Ok(start_line) = u32::try_from(block.start_line) {
            push_lookup(
                &mut self.block_exact,
                (path.clone(), block.hash.clone(), start_line),
                unit_id.clone(),
            );
        }
        push_lookup(
            &mut self.block_by_path_hash,
            (path, block.hash.clone()),
            unit_id.clone(),
        );
        push_lookup(&mut self.block_by_hash, block.hash.clone(), unit_id);
    }

    fn bind_record(
        &self,
        record: &Record,
        workdir_prefix: Option<&str>,
    ) -> Result<RecordBinding, RecordBindingFailure> {
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
    ) -> Result<RecordBinding, RecordBindingFailure> {
        if let (Some(path_hint), Some(line_hint)) = (&record.path_hint, record.line_hint) {
            let candidates = unique_unit_candidates(
                collect_path_candidates(path_hint, workdir_prefix)
                    .into_iter()
                    .flat_map(|path| {
                        self.block_exact
                            .get(&(path, hash.clone(), line_hint))
                            .into_iter()
                            .flatten()
                            .cloned()
                    })
                    .collect(),
            );
            match candidates.as_slice() {
                [unit_id] => {
                    return Ok(RecordBinding {
                        unit_id: unit_id.clone(),
                        relation: BindingRelation::Exact,
                    });
                }
                [] => {}
                _ => return Err(self.ambiguous_failure(record, candidates)),
            }
        }

        if let Some(path_hint) = &record.path_hint {
            let candidates = unique_unit_candidates(
                collect_path_candidates(path_hint, workdir_prefix)
                    .into_iter()
                    .flat_map(|path| {
                        self.block_by_path_hash
                            .get(&(path, hash.clone()))
                            .into_iter()
                            .flatten()
                            .cloned()
                    })
                    .collect(),
            );
            match candidates.as_slice() {
                [unit_id] => {
                    return Ok(RecordBinding {
                        unit_id: unit_id.clone(),
                        relation: BindingRelation::PathScoped,
                    });
                }
                [] => {}
                _ => return Err(self.ambiguous_failure(record, candidates)),
            }
        }

        let candidates =
            unique_unit_candidates(self.block_by_hash.get(hash).cloned().unwrap_or_default());
        match candidates.as_slice() {
            [unit_id] => Ok(RecordBinding {
                unit_id: unit_id.clone(),
                relation: BindingRelation::HashOnly,
            }),
            [] => Err(RecordBindingFailure::Unresolved {
                record_id: record.id.clone(),
            }),
            _ => Err(self.ambiguous_failure(record, candidates)),
        }
    }

    fn bind_path_target_record(
        &self,
        record: &Record,
        hash: &crate::store::TreeHash,
        workdir_prefix: Option<&str>,
        by_path_hash: &HashMap<(RepoPath, crate::store::TreeHash), Vec<CoverageUnitId>>,
        by_hash: &HashMap<crate::store::TreeHash, Vec<CoverageUnitId>>,
    ) -> Result<RecordBinding, RecordBindingFailure> {
        if let Some(path_hint) = &record.path_hint {
            let candidates = unique_unit_candidates(
                collect_path_candidates(path_hint, workdir_prefix)
                    .into_iter()
                    .flat_map(|path| {
                        by_path_hash
                            .get(&(path, hash.clone()))
                            .into_iter()
                            .flatten()
                            .cloned()
                    })
                    .collect(),
            );
            match candidates.as_slice() {
                [unit_id] => {
                    return Ok(RecordBinding {
                        unit_id: unit_id.clone(),
                        relation: BindingRelation::TargetNode,
                    });
                }
                [] => {}
                _ => return Err(self.ambiguous_failure(record, candidates)),
            }
        }

        let candidates = unique_unit_candidates(by_hash.get(hash).cloned().unwrap_or_default());
        match candidates.as_slice() {
            [unit_id] => Ok(RecordBinding {
                unit_id: unit_id.clone(),
                relation: BindingRelation::HashOnly,
            }),
            [] => Err(RecordBindingFailure::Unresolved {
                record_id: record.id.clone(),
            }),
            _ => Err(self.ambiguous_failure(record, candidates)),
        }
    }

    fn ambiguous_failure(
        &self,
        record: &Record,
        candidates: Vec<CoverageUnitId>,
    ) -> RecordBindingFailure {
        RecordBindingFailure::Ambiguous {
            record_id: record.id.clone(),
            candidates,
        }
    }

    fn ambiguous_diagnostic(
        &self,
        record_id: String,
        candidate_unit_ids: Vec<CoverageUnitId>,
    ) -> CoverageDiagnostic {
        let candidates = candidate_unit_ids
            .into_iter()
            .map(|unit_id| {
                self.candidate_descriptions
                    .get(&unit_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        panic!("coverage candidate missing description for {unit_id:?}")
                    })
            })
            .collect();
        CoverageDiagnostic::AmbiguousRecord {
            record_id,
            candidates,
        }
    }
}

fn push_lookup<K>(entries: &mut HashMap<K, Vec<CoverageUnitId>>, key: K, unit_id: CoverageUnitId)
where
    K: Eq + std::hash::Hash,
{
    entries.entry(key).or_default().push(unit_id);
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

fn unique_unit_candidates(unit_ids: Vec<CoverageUnitId>) -> Vec<CoverageUnitId> {
    let mut seen = HashSet::new();
    unit_ids
        .into_iter()
        .filter(|unit_id| seen.insert(unit_id.clone()))
        .collect()
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
    let mut best: Option<(i64, usize)> = None;

    for record_index in record_indices {
        let record = &records[*record_index];
        if &record.check != check {
            continue;
        }

        if relation_for_record(*record_index).is_none() {
            continue;
        }

        let candidate = (record.timestamp, *record_index);

        match best {
            Some((best_timestamp, best_index)) => {
                if candidate.0 > best_timestamp
                    || (candidate.0 == best_timestamp && candidate.1 > best_index)
                {
                    best = Some(candidate);
                }
            }
            None => best = Some(candidate),
        }
    }

    best.map(|(_, record_index)| record_index)
}

fn identity_key(identity: &Identity) -> String {
    match identity {
        Identity::Email { email } => email.trim().to_ascii_lowercase(),
    }
}

fn distinct_identity_count_matching<'a>(
    records: impl Iterator<Item = &'a Record>,
    check: &ReviewCheck,
    verdict_requirement: VerdictRequirement,
) -> usize {
    let mut latest_by_identity = HashMap::new();
    for (position, record) in records.enumerate() {
        if &record.check != check {
            continue;
        }

        latest_by_identity
            .entry(identity_key(&record.identity))
            .and_modify(|(best_position, best_record): &mut (usize, &Record)| {
                if (record.timestamp, position) > (best_record.timestamp, *best_position) {
                    *best_position = position;
                    *best_record = record;
                }
            })
            .or_insert((position, record));
    }

    latest_by_identity
        .values()
        .filter(|(_, record)| record_matches_review_requirement(record, check, verdict_requirement))
        .count()
}

fn record_matches_review_requirement(
    record: &Record,
    check: &ReviewCheck,
    verdict_requirement: VerdictRequirement,
) -> bool {
    &record.check == check
        && match verdict_requirement {
            VerdictRequirement::Any => true,
            VerdictRequirement::ApprovedOnly => record.verdict == Verdict::Approved,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
    use crate::hashing::{BytesHash, TreeHash};
    use crate::repo_path::RepoPath;
    use crate::store::{
        BlockState, CommitId, Identity, Record, RepoRef, ReviewTargetRef, VcsSystem,
    };
    use crate::tree::TreeBuilder;

    fn test_block_at(
        content: String,
        kind: BlockKind,
        start_line: usize,
        end_line: usize,
        start_byte: usize,
    ) -> Block {
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

    fn test_block(content: String, kind: BlockKind, start_line: usize, end_line: usize) -> Block {
        let start_byte = start_line;
        test_block_at(content, kind, start_line, end_line, start_byte)
    }

    #[test]
    fn block_coverage_distinguishes_direct_and_inherited_reviews() {
        let (tree, file_id, function_id, _, _) = build_function_tree();
        let file_hash = tree.node(file_id).hash.clone();
        let function_hash = tree.node(function_id).hash.clone();
        let records = vec![
            approved_file_record("1", file_hash, "src/lib.rs", "alice@example.com", 1),
            approved_block_record("2", function_hash, "src/lib.rs", 1, "bob@example.com", 2)
                .with_check("security"),
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
        let records = vec![approved_file_record(
            "1",
            file_hash,
            "src/lib.rs",
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
    fn path_scoped_file_approval_does_not_cover_same_hash_sibling_file() {
        let (tree, approved_file_id, approved_block_id, sibling_block_id) =
            build_same_hash_file_tree();
        let file_hash = tree.node(approved_file_id).hash.clone();
        let records = vec![approved_file_record(
            "1",
            file_hash,
            "src/a.rs",
            "alice@example.com",
            1,
        )];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(
            coverage
                .node(approved_block_id)
                .effective_latest_verdict_for(&ReviewCheck::review()),
            Some(&Verdict::Approved)
        );
        assert_eq!(
            coverage
                .node(sibling_block_id)
                .effective_latest_verdict_for(&ReviewCheck::review()),
            None
        );
    }

    #[test]
    fn exact_block_binding_supports_two_person_review_policy() {
        let (tree, _, function_id, _, _) = build_function_tree();
        let function_hash = tree.node(function_id).hash.clone();
        let records = vec![
            approved_block_record(
                "1",
                function_hash.clone(),
                "src/lib.rs",
                1,
                "alice@example.com",
                1,
            ),
            approved_block_record("2", function_hash, "src/lib.rs", 1, "bob@example.com", 2),
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
    fn newer_hash_only_block_record_overrides_older_exact_record() {
        let (tree, _, function_id, _, function_hash) = build_function_tree();
        let mut rejected =
            approved_hash_only_block_record("2", function_hash.clone(), "alice@example.com", 2);
        rejected.verdict = Verdict::Rejected;
        let records = vec![
            approved_block_record("1", function_hash, "src/lib.rs", 1, "alice@example.com", 1),
            rejected,
        ];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(
            coverage
                .node(function_id)
                .direct_latest_verdict_for(&ReviewCheck::review()),
            Some(&Verdict::Rejected)
        );
    }

    #[test]
    fn approved_only_two_person_policy_ignores_non_approving_identities() {
        let (tree, _, function_id, _, _) = build_function_tree();
        let function_hash = tree.node(function_id).hash.clone();
        let mut rejected = approved_block_record(
            "1",
            function_hash.clone(),
            "src/lib.rs",
            1,
            "alice@example.com",
            1,
        );
        rejected.verdict = Verdict::Rejected;
        let records = vec![
            rejected,
            approved_block_record("2", function_hash, "src/lib.rs", 1, "bob@example.com", 2),
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
            !coverage
                .node(function_id)
                .is_well_reviewed(&CoveragePolicy::two_person_review())
        );
    }

    #[test]
    fn two_person_policy_ignores_withdrawn_approval() {
        let (tree, _, function_id, _, _) = build_function_tree();
        let function_hash = tree.node(function_id).hash.clone();
        let mut alice_rejected = approved_block_record(
            "2",
            function_hash.clone(),
            "src/lib.rs",
            1,
            "alice@example.com",
            2,
        );
        alice_rejected.verdict = Verdict::Rejected;
        let records = vec![
            approved_block_record(
                "1",
                function_hash.clone(),
                "src/lib.rs",
                1,
                "alice@example.com",
                1,
            ),
            alice_rejected,
            approved_block_record("3", function_hash, "src/lib.rs", 1, "bob@example.com", 3),
        ];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();
        let function_coverage = coverage.node(function_id);

        assert_eq!(
            function_coverage.direct_latest_verdict_for(&ReviewCheck::review()),
            Some(&Verdict::Approved)
        );
        assert!(!function_coverage.is_well_reviewed(&CoveragePolicy::two_person_review()));
    }

    #[test]
    fn hash_only_block_record_is_reported_as_ambiguous() {
        let (tree, first_id, second_id, first_hash, second_hash) = build_duplicate_hash_tree();
        assert_eq!(first_hash, second_hash);

        let records = vec![approved_hash_only_block_record(
            "1",
            first_hash,
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
    fn generated_review_unit_binding_keeps_same_line_byte_distinct_candidates() {
        let mut source = String::from("Twin. Twin. Tail.\n");
        for line in 0..50 {
            source.push_str(&format!("Tail {line}.\n"));
        }
        let parent = Block::from_file_range(
            &source,
            BlockKind::Paragraph,
            ByteSpan::new(0, source.len()),
        )
        .unwrap_or_else(|error| panic!("markdown parent should be source-backed: {error}"));
        let split = crate::sub_splitter::split_result(&parent, Language::Markdown)
            .unwrap_or_else(|error| panic!("markdown parent should split: {error}"));
        assert_eq!(
            split.semantics,
            crate::sub_splitter::SubSplitSemantics::ReviewUnits
        );
        let twins = split
            .blocks
            .into_iter()
            .filter(|block| block.content == "Twin. ")
            .collect::<Vec<_>>();
        assert_eq!(twins.len(), 2);
        assert_eq!(twins[0].hash, twins[1].hash);
        assert_eq!(twins[0].start_line, twins[1].start_line);
        assert_ne!(twins[0].byte_span(), twins[1].byte_span());
        assert!(!twins[0].byte_span().overlaps(&twins[1].byte_span()));

        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "README.md".to_string(),
            "README.md".to_string(),
            "file-hash".to_string(),
            Language::Markdown,
        );
        builder.add_block(
            file,
            "paragraph".to_string(),
            "README.md".to_string(),
            parent,
            Language::Markdown,
        );
        let tree = builder.finalize();
        let path = RepoPath::new("README.md").unwrap();

        let records = vec![
            approved_hash_only_block_record("hash-only", twins[0].hash.clone(), "a@example.com", 1),
            TestRecord::approved(
                "path-only",
                ReviewTargetRef::Block {
                    hash: twins[0].hash.clone(),
                },
                "b@example.com",
                2,
            )
            .with_path_hint("README.md"),
            approved_block_record(
                "persisted-exact",
                twins[0].hash.clone(),
                "README.md",
                u32::try_from(twins[0].start_line).unwrap(),
                "c@example.com",
                3,
            ),
        ];
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        for record_id in ["hash-only", "path-only", "persisted-exact"] {
            assert_eq!(coverage.binding_relation_for_record(record_id), None);
            let candidates = coverage
                .diagnostics()
                .iter()
                .find_map(|diagnostic| match diagnostic {
                    CoverageDiagnostic::AmbiguousRecord {
                        record_id: diagnostic_record_id,
                        candidates,
                    } if diagnostic_record_id == record_id => Some(candidates),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{record_id} should be ambiguous"));
            assert_eq!(candidates.len(), 2);
            let byte_spans = candidates
                .iter()
                .map(|candidate| match candidate {
                    CoverageCandidate::Block {
                        path: candidate_path,
                        hash,
                        start_line,
                        end_line,
                        start_byte,
                        end_byte,
                    } => {
                        assert_eq!(candidate_path, &path);
                        assert_eq!(hash, &twins[0].hash);
                        assert_eq!(*start_line, twins[0].start_line);
                        assert_eq!(*end_line, twins[0].end_line);
                        ByteSpan::new(*start_byte, *end_byte)
                    }
                    CoverageCandidate::TreeNode { .. } => {
                        panic!("generated ambiguity should describe block candidates")
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(byte_spans, vec![twins[0].byte_span(), twins[1].byte_span()]);
        }
        for twin in &twins {
            let block_coverage = coverage.block(&path, twin);
            assert!(block_coverage.direct_records().is_empty());
            assert_eq!(block_coverage.linked_records().len(), 3);
            assert_eq!(
                block_coverage.direct_latest_verdict_for(&ReviewCheck::review()),
                None
            );
        }
    }

    #[test]
    fn subtree_queries_descendant_block_review_status() {
        let (tree, container_id, first_method_id, second_method_id) = build_container_tree();
        let first_hash = tree.node(first_method_id).hash.clone();
        let second_hash = tree.node(second_method_id).hash.clone();
        let records = vec![
            approved_block_record("1", first_hash, "src/lib.rs", 2, "alice@example.com", 1),
            approved_block_record("2", second_hash, "src/lib.rs", 4, "bob@example.com", 2),
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

    #[test]
    fn block_coverage_resolves_identical_same_line_blocks_by_byte_span() {
        let content = "const _: () = ();";
        let first = test_block_at(content.to_string(), BlockKind::Const, 0, 1, 0);
        let second = test_block_at(
            content.to_string(),
            BlockKind::Const,
            0,
            1,
            content.len() + 1,
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
        builder.add_block(
            file,
            "first".to_string(),
            "src/lib.rs".to_string(),
            first,
            Language::Rust,
        );
        let second_id = builder.add_block(
            file,
            "second".to_string(),
            "src/lib.rs".to_string(),
            second.clone(),
            Language::Rust,
        );
        let tree = builder.finalize();
        let database = ReviewDatabase::from_records(Vec::new());
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(
            coverage
                .block(&RepoPath::new("src/lib.rs").unwrap(), &second)
                .resolved_node_id(),
            Some(second_id)
        );
    }

    #[test]
    fn smallest_covering_block_uses_byte_containment_on_same_line() {
        let source = "class Outer { class A {} class B {} }";
        let outer =
            Block::from_file_range(source, BlockKind::Class, ByteSpan::new(0, source.len()))
                .unwrap_or_else(|error| panic!("outer source range should be valid: {error}"));
        let sibling_start = source
            .find("class B {}")
            .unwrap_or_else(|| panic!("expected same-line sibling"));
        let sibling = Block::from_file_range(
            source,
            BlockKind::Class,
            ByteSpan::new(sibling_start, sibling_start + "class B {}".len()),
        )
        .unwrap_or_else(|error| panic!("sibling source range should be valid: {error}"));
        let probe_start = source
            .find("class A {}")
            .unwrap_or_else(|| panic!("expected probe source"));
        let probe = test_block_at("class C {}".to_string(), BlockKind::Code, 0, 1, probe_start);

        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "Main.java".to_string(),
            "src/Main.java".to_string(),
            "file-hash".to_string(),
            Language::Java,
        );
        let outer_id = builder.add_block(
            file,
            "outer".to_string(),
            "src/Main.java".to_string(),
            outer,
            Language::Java,
        );
        builder.add_block(
            file,
            "sibling".to_string(),
            "src/Main.java".to_string(),
            sibling,
            Language::Java,
        );
        let tree = builder.finalize();
        let database = ReviewDatabase::from_records(Vec::new());
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();

        assert_eq!(
            coverage
                .block(&RepoPath::new("src/Main.java").unwrap(), &probe)
                .container_node_id,
            Some(outer_id)
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
        let function = test_block("fn helper() {}\n".to_string(), BlockKind::Function, 1, 2);
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

    fn build_same_hash_file_tree() -> (Tree, TreeNodeId, TreeNodeId, TreeNodeId) {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file_hash = BytesHash::new("same-bytes").to_string();
        let first_file = builder.add_file(
            src,
            "a.rs".to_string(),
            "src/a.rs".to_string(),
            file_hash.clone(),
            Language::Rust,
        );
        let second_file = builder.add_file(
            src,
            "b.rs".to_string(),
            "src/b.rs".to_string(),
            file_hash,
            Language::Rust,
        );
        let first_block = builder.add_block(
            first_file,
            "fn duplicate".to_string(),
            "src/a.rs".to_string(),
            test_block("fn duplicate() {}\n".to_string(), BlockKind::Function, 1, 2),
            Language::Rust,
        );
        let second_block = builder.add_block(
            second_file,
            "fn duplicate".to_string(),
            "src/b.rs".to_string(),
            test_block("fn duplicate() {}\n".to_string(), BlockKind::Function, 1, 2),
            Language::Rust,
        );

        (builder.finalize(), first_file, first_block, second_block)
    }

    fn build_duplicate_hash_tree() -> (Tree, TreeNodeId, TreeNodeId, TreeHash, TreeHash) {
        let shared_content = "fn dup() {}\n".to_string();
        let first = test_block(shared_content.clone(), BlockKind::Function, 1, 2);
        let second = test_block(shared_content, BlockKind::Function, 10, 11);

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
        let container = test_block(
            "impl Worker {\n    fn start(&self) {}\n\n    fn stop(&self) {}\n}\n".to_string(),
            BlockKind::Impl,
            1,
            6,
        );
        let first_method = test_block("fn start(&self) {}\n".to_string(), BlockKind::Method, 2, 3);
        let second_method = test_block("fn stop(&self) {}\n".to_string(), BlockKind::Method, 4, 5);

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

    fn approved_file_record(
        id: &str,
        hash: TreeHash,
        path: &str,
        email: &str,
        timestamp: i64,
    ) -> Record {
        TestRecord::approved(id, ReviewTargetRef::File { hash }, email, timestamp)
            .with_path_hint(path)
    }

    fn approved_block_record(
        id: &str,
        hash: TreeHash,
        path: &str,
        start_line: u32,
        email: &str,
        timestamp: i64,
    ) -> Record {
        TestRecord::approved(id, ReviewTargetRef::Block { hash }, email, timestamp)
            .with_path_hint(path)
            .with_line_hint(start_line)
    }

    fn approved_hash_only_block_record(
        id: &str,
        hash: TreeHash,
        email: &str,
        timestamp: i64,
    ) -> Record {
        TestRecord::approved(id, ReviewTargetRef::Block { hash }, email, timestamp)
    }

    struct TestRecord;

    impl TestRecord {
        fn approved(id: &str, target: ReviewTargetRef, email: &str, timestamp: i64) -> Record {
            let mut record = Record::new(
                target,
                ReviewCheck::review(),
                Verdict::Approved,
                Identity::Email {
                    email: email.to_string(),
                },
                RepoRef::Vcs {
                    system: VcsSystem::Git,
                    revision: CommitId::new("0123456789abcdef").unwrap(),
                },
                BlockState::Committed,
            );
            record.id = id.to_string();
            record.timestamp = timestamp;
            record
        }
    }

    trait TestRecordExt {
        fn with_check(self, check: &str) -> Self;
        fn with_path_hint(self, path: &str) -> Self;
        fn with_line_hint(self, line: u32) -> Self;
    }

    impl TestRecordExt for Record {
        fn with_check(mut self, check: &str) -> Self {
            self.check = ReviewCheck::new(check).unwrap();
            self
        }

        fn with_path_hint(mut self, path: &str) -> Self {
            self.path_hint = Some(RepoPath::new(path).unwrap());
            self
        }

        fn with_line_hint(mut self, line: u32) -> Self {
            self.line_hint = Some(line);
            self
        }
    }
}
