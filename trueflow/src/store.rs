use anyhow::{Result, anyhow};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::warn;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::str::FromStr;

use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::vcs;

const TRUEFLOW_DIR: &str = ".trueflow";
const DB_FILE: &str = "reviews.jsonl";
pub const CURRENT_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(tag = "type")]
#[schemars(deny_unknown_fields)]
pub enum Identity {
    #[serde(rename = "email")]
    Email {
        #[schemars(email)]
        email: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum Verdict {
    Approved,
    Rejected,
    #[serde(alias = "question")]
    Comment,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum VcsSystem {
    Git,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct CommitId(String);

impl CommitId {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if !(7..=40).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(anyhow!(
                "commit id must be a 7-40 character hex string: {value}"
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CommitId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum RepoRef {
    Vcs {
        system: VcsSystem,
        revision: CommitId,
    },
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum BlockState {
    Committed,
    Uncommitted,
    Unknown,
}

pub use crate::hashing::TreeHash;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ReviewCheck(String);

impl ReviewCheck {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(anyhow!("review check cannot be empty"));
        }
        Ok(Self(value.to_string()))
    }

    pub fn review() -> Self {
        Self("review".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReviewCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReviewCheck {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(deny_unknown_fields)]
pub enum ReviewTargetKind {
    Block,
    File,
    Tree,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[schemars(deny_unknown_fields)]
pub enum ReviewTargetRef {
    Block { hash: TreeHash },
    File { hash: TreeHash },
    Tree { hash: TreeHash },
}

impl ReviewTargetRef {
    pub fn lookup_key(&self) -> &str {
        match self {
            ReviewTargetRef::Block { hash }
            | ReviewTargetRef::File { hash }
            | ReviewTargetRef::Tree { hash } => hash.as_str(),
        }
    }
}

impl ReviewTargetKind {
    pub fn parse_target(self, raw: &str) -> Result<ReviewTargetRef> {
        match self {
            ReviewTargetKind::Block => Ok(ReviewTargetRef::Block {
                hash: TreeHash::parse(raw)?,
            }),
            ReviewTargetKind::File => Ok(ReviewTargetRef::File {
                hash: TreeHash::parse(raw)?,
            }),
            ReviewTargetKind::Tree => Ok(ReviewTargetRef::Tree {
                hash: TreeHash::parse(raw)?,
            }),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[schemars(deny_unknown_fields)]
pub enum AttestationKind {
    Pgp,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[schemars(deny_unknown_fields)]
pub enum Canonicalization {
    JcsV1,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Attestation {
    pub kind: AttestationKind,
    pub canonicalization: Canonicalization,
    pub signature: String,
    pub public_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CommentScope {
    #[schemars(range(min = 0))]
    pub start_line: u32,
    #[schemars(range(min = 0))]
    pub end_line: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(deny_unknown_fields)]
pub enum CommentAnchorDiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SourceCommentAnchor {
    pub revision: CommitId,
    pub path: RepoPath,
    #[schemars(range(min = 0))]
    pub start_line: u32,
    #[schemars(range(min = 0))]
    pub end_line: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DiffCommentAnchorRow {
    pub kind: CommentAnchorDiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DiffCommentAnchor {
    pub revision: CommitId,
    pub path: RepoPath,
    pub rows: Vec<DiffCommentAnchorRow>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(deny_unknown_fields)]
pub enum CommentAnchor {
    Source(SourceCommentAnchor),
    Diff(DiffCommentAnchor),
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    #[schemars(range(min = 0))]
    pub version: u32,
    pub target: ReviewTargetRef,
    pub check: ReviewCheck,
    pub verdict: Verdict,
    pub identity: Identity,
    pub repo_ref: RepoRef,
    pub block_state: BlockState,
    #[schemars(range(min = 0))]
    pub timestamp: i64,
    pub path_hint: Option<RepoPath>,
    pub line_hint: Option<u32>,
    pub note: Option<String>,
    pub comment_scope: Option<CommentScope>,
    pub comment_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_anchor: Option<CommentAnchor>,
    #[schemars(inner(length(min = 1)))]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attestations: Option<Vec<Attestation>>,
}

#[derive(Serialize)]
struct SignableRecordV2<'a> {
    id: &'a str,
    version: u32,
    target: &'a ReviewTargetRef,
    check: &'a ReviewCheck,
    verdict: &'a Verdict,
    identity: &'a Identity,
    repo_ref: &'a RepoRef,
    block_state: &'a BlockState,
    timestamp: i64,
    path_hint: &'a Option<RepoPath>,
    line_hint: &'a Option<u32>,
    note: &'a Option<String>,
    tags: &'a Option<Vec<String>>,
}

#[derive(Serialize)]
struct SignableRecordV3<'a> {
    id: &'a str,
    version: u32,
    target: &'a ReviewTargetRef,
    check: &'a ReviewCheck,
    verdict: &'a Verdict,
    identity: &'a Identity,
    repo_ref: &'a RepoRef,
    block_state: &'a BlockState,
    timestamp: i64,
    path_hint: &'a Option<RepoPath>,
    line_hint: &'a Option<u32>,
    note: &'a Option<String>,
    comment_scope: &'a Option<CommentScope>,
    comment_context: &'a Option<String>,
    tags: &'a Option<Vec<String>>,
}

#[derive(Serialize)]
struct SignableRecordV4<'a> {
    id: &'a str,
    version: u32,
    target: &'a ReviewTargetRef,
    check: &'a ReviewCheck,
    verdict: &'a Verdict,
    identity: &'a Identity,
    repo_ref: &'a RepoRef,
    block_state: &'a BlockState,
    timestamp: i64,
    path_hint: &'a Option<RepoPath>,
    line_hint: &'a Option<u32>,
    note: &'a Option<String>,
    comment_scope: &'a Option<CommentScope>,
    comment_context: &'a Option<String>,
    comment_anchor: &'a Option<CommentAnchor>,
    tags: &'a Option<Vec<String>>,
}

impl Record {
    pub fn new(
        target: ReviewTargetRef,
        check: ReviewCheck,
        verdict: Verdict,
        identity: Identity,
        repo_ref: RepoRef,
        block_state: BlockState,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            version: CURRENT_VERSION,
            target,
            check,
            verdict,
            identity,
            repo_ref,
            block_state,
            timestamp: chrono::Utc::now().timestamp(),
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

    pub fn signing_payload(&self) -> Result<String> {
        if self.version >= 4 {
            return Ok(serde_jcs::to_string(&SignableRecordV4 {
                id: &self.id,
                version: self.version,
                target: &self.target,
                check: &self.check,
                verdict: &self.verdict,
                identity: &self.identity,
                repo_ref: &self.repo_ref,
                block_state: &self.block_state,
                timestamp: self.timestamp,
                path_hint: &self.path_hint,
                line_hint: &self.line_hint,
                note: &self.note,
                comment_scope: &self.comment_scope,
                comment_context: &self.comment_context,
                comment_anchor: &self.comment_anchor,
                tags: &self.tags,
            })?);
        }

        if self.version >= 3 {
            return Ok(serde_jcs::to_string(&SignableRecordV3 {
                id: &self.id,
                version: self.version,
                target: &self.target,
                check: &self.check,
                verdict: &self.verdict,
                identity: &self.identity,
                repo_ref: &self.repo_ref,
                block_state: &self.block_state,
                timestamp: self.timestamp,
                path_hint: &self.path_hint,
                line_hint: &self.line_hint,
                note: &self.note,
                comment_scope: &self.comment_scope,
                comment_context: &self.comment_context,
                tags: &self.tags,
            })?);
        }

        Ok(serde_jcs::to_string(&SignableRecordV2 {
            id: &self.id,
            version: self.version,
            target: &self.target,
            check: &self.check,
            verdict: &self.verdict,
            identity: &self.identity,
            repo_ref: &self.repo_ref,
            block_state: &self.block_state,
            timestamp: self.timestamp,
            path_hint: &self.path_hint,
            line_hint: &self.line_hint,
            note: &self.note,
            tags: &self.tags,
        })?)
    }
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Approved => "approved",
            Verdict::Rejected => "rejected",
            Verdict::Comment => "comment",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Verdict {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "approved" => Ok(Verdict::Approved),
            "rejected" => Ok(Verdict::Rejected),
            "question" => Ok(Verdict::Comment),
            "comment" => Ok(Verdict::Comment),
            _ => Err(anyhow!("Unknown verdict: {value}")),
        }
    }
}

impl VcsSystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            VcsSystem::Git => "git",
        }
    }
}

impl fmt::Display for VcsSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BlockState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockState::Committed => "committed",
            BlockState::Uncommitted => "uncommitted",
            BlockState::Unknown => "unknown",
        }
    }
}

impl From<crate::vcs::BlockStateResult> for BlockState {
    fn from(result: crate::vcs::BlockStateResult) -> Self {
        match result {
            crate::vcs::BlockStateResult::Committed => BlockState::Committed,
            crate::vcs::BlockStateResult::Uncommitted => BlockState::Uncommitted,
            crate::vcs::BlockStateResult::Unknown => BlockState::Unknown,
        }
    }
}

impl fmt::Display for BlockState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BlockLocator {
    Hash(TreeHash),
    Path {
        hash: TreeHash,
        path: RepoPath,
    },
    Exact {
        hash: TreeHash,
        path: RepoPath,
        start_line: u32,
    },
}

impl BlockLocator {
    fn from_record(hash: &TreeHash, path_hint: Option<&RepoPath>, line_hint: Option<u32>) -> Self {
        match (path_hint, line_hint) {
            (Some(path), Some(start_line)) => Self::Exact {
                hash: hash.clone(),
                path: path.clone(),
                start_line,
            },
            (Some(path), None) => Self::Path {
                hash: hash.clone(),
                path: path.clone(),
            },
            (None, _) => Self::Hash(hash.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PathTargetLocator {
    Hash(TreeHash),
    Path { hash: TreeHash, path: RepoPath },
}

impl PathTargetLocator {
    fn from_record(hash: &TreeHash, path_hint: Option<&RepoPath>) -> Self {
        match path_hint {
            Some(path) => Self::Path {
                hash: hash.clone(),
                path: path.clone(),
            },
            None => Self::Hash(hash.clone()),
        }
    }

    fn hash(&self) -> &TreeHash {
        match self {
            Self::Hash(hash) | Self::Path { hash, .. } => hash,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApprovedTargets {
    block_targets: HashSet<BlockLocator>,
    file_targets: HashSet<PathTargetLocator>,
    tree_targets: HashSet<PathTargetLocator>,
}

impl ApprovedTargets {
    pub fn contains_target(&self, target: &ReviewTargetRef) -> bool {
        match target {
            ReviewTargetRef::Block { hash } => self
                .block_targets
                .contains(&BlockLocator::Hash(hash.clone())),
            ReviewTargetRef::File { hash } => {
                self.file_targets.iter().any(|target| target.hash() == hash)
            }
            ReviewTargetRef::Tree { hash } => {
                self.tree_targets.iter().any(|target| target.hash() == hash)
            }
        }
    }

    pub fn contains_file(
        &self,
        hash: &TreeHash,
        path: &RepoPath,
        workdir_prefix: Option<&str>,
    ) -> bool {
        contains_path_locator(&self.file_targets, hash, path, workdir_prefix)
    }

    pub fn contains_tree(
        &self,
        hash: &TreeHash,
        path: &RepoPath,
        workdir_prefix: Option<&str>,
    ) -> bool {
        contains_path_locator(&self.tree_targets, hash, path, workdir_prefix)
    }

    pub fn contains_block(
        &self,
        hash: &TreeHash,
        path: &RepoPath,
        start_line: usize,
        workdir_prefix: Option<&str>,
    ) -> bool {
        contains_block_locator(&self.block_targets, hash, path, start_line, workdir_prefix)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewIndex {
    #[cfg(test)]
    latest_verdicts: HashMap<ReviewTargetRef, Verdict>,
    block_verdicts: HashMap<BlockLocator, Verdict>,
    file_verdicts: HashMap<PathTargetLocator, Verdict>,
    tree_verdicts: HashMap<PathTargetLocator, Verdict>,
}

impl ReviewIndex {
    pub fn from_records(records: &[Record], check_filter: Option<&ReviewCheck>) -> Self {
        #[cfg(test)]
        let mut latest_by_target: HashMap<ReviewTargetRef, (i64, Verdict)> = HashMap::new();
        let mut block_verdicts: HashMap<BlockLocator, (i64, Verdict)> = HashMap::new();
        let mut file_verdicts: HashMap<PathTargetLocator, (i64, Verdict)> = HashMap::new();
        let mut tree_verdicts: HashMap<PathTargetLocator, (i64, Verdict)> = HashMap::new();

        for record in records {
            if check_filter.is_some_and(|check| &record.check != check) {
                continue;
            }

            #[cfg(test)]
            {
                match latest_by_target.get_mut(&record.target) {
                    Some((timestamp, verdict)) => {
                        if record.timestamp >= *timestamp {
                            *timestamp = record.timestamp;
                            *verdict = record.verdict.clone();
                        }
                    }
                    None => {
                        latest_by_target.insert(
                            record.target.clone(),
                            (record.timestamp, record.verdict.clone()),
                        );
                    }
                }
            }

            match &record.target {
                ReviewTargetRef::Block { hash } => {
                    update_latest_verdict(
                        &mut block_verdicts,
                        BlockLocator::from_record(
                            hash,
                            record.path_hint.as_ref(),
                            record.line_hint,
                        ),
                        record.timestamp,
                        record.verdict.clone(),
                    );
                }
                ReviewTargetRef::File { hash } => {
                    update_latest_verdict(
                        &mut file_verdicts,
                        PathTargetLocator::from_record(hash, record.path_hint.as_ref()),
                        record.timestamp,
                        record.verdict.clone(),
                    );
                }
                ReviewTargetRef::Tree { hash } => {
                    update_latest_verdict(
                        &mut tree_verdicts,
                        PathTargetLocator::from_record(hash, record.path_hint.as_ref()),
                        record.timestamp,
                        record.verdict.clone(),
                    );
                }
            }
        }

        Self {
            #[cfg(test)]
            latest_verdicts: latest_by_target
                .into_iter()
                .map(|(target, (_, verdict))| (target, verdict))
                .collect(),
            block_verdicts: block_verdicts
                .into_iter()
                .map(|(key, (_, verdict))| (key, verdict))
                .collect(),
            file_verdicts: file_verdicts
                .into_iter()
                .map(|(key, (_, verdict))| (key, verdict))
                .collect(),
            tree_verdicts: tree_verdicts
                .into_iter()
                .map(|(key, (_, verdict))| (key, verdict))
                .collect(),
        }
    }

    #[cfg(test)]
    pub fn verdict_for(&self, target: &ReviewTargetRef) -> Option<&Verdict> {
        self.latest_verdicts.get(target)
    }

    #[cfg(test)]
    pub fn block_verdict_for(
        &self,
        hash: &TreeHash,
        path: &RepoPath,
        start_line: usize,
        workdir_prefix: Option<&str>,
    ) -> Option<&Verdict> {
        find_block_verdict(&self.block_verdicts, hash, path, start_line, workdir_prefix)
    }

    pub fn approved_targets(&self) -> ApprovedTargets {
        let mut approved = ApprovedTargets::default();
        for (locator, verdict) in &self.file_verdicts {
            if verdict != &Verdict::Approved {
                continue;
            }
            approved.file_targets.insert(locator.clone());
        }

        for (locator, verdict) in &self.tree_verdicts {
            if verdict != &Verdict::Approved {
                continue;
            }
            approved.tree_targets.insert(locator.clone());
        }

        for (locator, verdict) in &self.block_verdicts {
            if verdict == &Verdict::Approved {
                approved.block_targets.insert(locator.clone());
            }
        }

        approved
    }
}

fn update_latest_verdict<K: Eq + std::hash::Hash>(
    entries: &mut HashMap<K, (i64, Verdict)>,
    key: K,
    timestamp: i64,
    verdict: Verdict,
) {
    match entries.get_mut(&key) {
        Some((existing_timestamp, existing_verdict)) => {
            if timestamp >= *existing_timestamp {
                *existing_timestamp = timestamp;
                *existing_verdict = verdict;
            }
        }
        None => {
            entries.insert(key, (timestamp, verdict));
        }
    }
}

fn contains_path_locator(
    targets: &HashSet<PathTargetLocator>,
    hash: &TreeHash,
    path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> bool {
    for path in block_path_candidates(path, workdir_prefix) {
        if targets.contains(&PathTargetLocator::Path {
            hash: hash.clone(),
            path,
        }) {
            return true;
        }
    }
    targets.contains(&PathTargetLocator::Hash(hash.clone()))
}

fn contains_block_locator(
    targets: &HashSet<BlockLocator>,
    hash: &TreeHash,
    path: &RepoPath,
    start_line: usize,
    workdir_prefix: Option<&str>,
) -> bool {
    let paths = block_path_candidates(path, workdir_prefix);
    if let Ok(start_line) = u32::try_from(start_line) {
        for path in &paths {
            if targets.contains(&BlockLocator::Exact {
                hash: hash.clone(),
                path: path.clone(),
                start_line,
            }) {
                return true;
            }
        }
    }

    for path in paths {
        if targets.contains(&BlockLocator::Path {
            hash: hash.clone(),
            path,
        }) {
            return true;
        }
    }
    targets.contains(&BlockLocator::Hash(hash.clone()))
}

#[cfg(test)]
fn find_block_verdict<'a>(
    entries: &'a HashMap<BlockLocator, Verdict>,
    hash: &TreeHash,
    path: &RepoPath,
    start_line: usize,
    workdir_prefix: Option<&str>,
) -> Option<&'a Verdict> {
    let paths = block_path_candidates(path, workdir_prefix);
    if let Ok(start_line) = u32::try_from(start_line) {
        for path in &paths {
            let candidate = BlockLocator::Exact {
                hash: hash.clone(),
                path: path.clone(),
                start_line,
            };
            if let Some(verdict) = entries.get(&candidate) {
                return Some(verdict);
            }
        }
    }

    for path in paths {
        let candidate = BlockLocator::Path {
            hash: hash.clone(),
            path,
        };
        if let Some(verdict) = entries.get(&candidate) {
            return Some(verdict);
        }
    }
    entries.get(&BlockLocator::Hash(hash.clone()))
}

fn block_path_candidates(path: &RepoPath, workdir_prefix: Option<&str>) -> Vec<RepoPath> {
    let mut candidates = Vec::with_capacity(2);
    for candidate in path_utils::repo_path_candidates(path.as_str(), workdir_prefix, None) {
        let Ok(candidate) = RepoPath::new(candidate) else {
            continue;
        };
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

#[derive(Debug, Clone, Default)]
pub struct ReviewDatabase {
    records: Vec<Record>,
}

impl ReviewDatabase {
    pub fn from_records(records: Vec<Record>) -> Self {
        Self { records }
    }

    pub fn load(store: &impl ReviewStore) -> Result<Self> {
        Ok(Self {
            records: store.read_history()?,
        })
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn max_timestamp(&self) -> Option<i64> {
        self.records.iter().map(|record| record.timestamp).max()
    }

    pub fn latest_index(&self, check_filter: Option<&ReviewCheck>) -> ReviewIndex {
        ReviewIndex::from_records(&self.records, check_filter)
    }
}

#[cfg(test)]
pub fn merge_record_histories<I, J>(left: I, right: J) -> Vec<Record>
where
    I: IntoIterator<Item = Record>,
    J: IntoIterator<Item = Record>,
{
    let mut all_records = Vec::new();
    let mut seen_ids = HashSet::new();

    for record in left.into_iter().chain(right) {
        if seen_ids.insert(record.id.clone()) {
            all_records.push(record);
        }
    }

    all_records.sort_by_key(|record| record.timestamp);
    all_records
}

#[derive(Debug, Clone, Default)]
struct JsonlParseReport {
    records: Vec<Record>,
    skipped_legacy_diff_target_records: usize,
    skipped_malformed_records: usize,
}

fn parse_record_line(line: &str) -> Option<Result<Record, anyhow::Error>> {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => return Some(Err(err.into())),
    };

    let is_legacy_diff_target = value
        .get("target")
        .and_then(|target| target.get("kind"))
        .and_then(serde_json::Value::as_str)
        == Some("diff");
    if is_legacy_diff_target {
        return None;
    }

    Some(serde_json::from_value(value).map_err(Into::into))
}

fn parse_records_jsonl_report_impl(content: &str) -> JsonlParseReport {
    let mut report = JsonlParseReport::default();

    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        match parse_record_line(line) {
            Some(Ok(record)) => report.records.push(record),
            Some(Err(err)) => {
                report.skipped_malformed_records += 1;
                warn!("Skipping malformed record: {err}");
            }
            None => {
                report.skipped_legacy_diff_target_records += 1;
            }
        }
    }

    report
}

#[cfg(test)]
fn parse_records_jsonl_report(content: &str) -> JsonlParseReport {
    parse_records_jsonl_report_impl(content)
}

#[cfg(test)]
pub fn parse_records_jsonl(content: &str) -> Vec<Record> {
    parse_records_jsonl_report_impl(content).records
}

pub trait ReviewStore {
    fn read_history(&self) -> Result<Vec<Record>>;
    fn append(&self, record: &Record) -> Result<()>;

    fn load_database(&self) -> Result<ReviewDatabase>
    where
        Self: Sized,
    {
        ReviewDatabase::load(self)
    }
}

#[derive(Debug, Clone)]
pub struct StoreLocation {
    root_path: PathBuf,
}

impl StoreLocation {
    pub fn discover() -> Result<Self> {
        if let Ok(Some(root)) = vcs::git_root_from_workdir() {
            let location = Self { root_path: root };
            location.ensure_trueflow_dir()?;
            return Ok(location);
        }

        let start_dir = std::env::current_dir()?;
        for dir in start_dir.ancestors() {
            if dir.join(TRUEFLOW_DIR).exists() {
                return Ok(Self {
                    root_path: dir.to_path_buf(),
                });
            }
        }

        let location = Self {
            root_path: start_dir,
        };
        location.ensure_trueflow_dir()?;
        Ok(location)
    }

    pub fn db_path(&self) -> PathBuf {
        self.trueflow_dir().join(DB_FILE)
    }

    pub fn trueflow_dir(&self) -> PathBuf {
        self.root_path.join(TRUEFLOW_DIR)
    }

    fn ensure_trueflow_dir(&self) -> Result<()> {
        let trueflow_dir = self.trueflow_dir();
        if !trueflow_dir.exists() {
            fs::create_dir(&trueflow_dir)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct JsonlStoreBackend {
    db_path: PathBuf,
}

impl JsonlStoreBackend {
    fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn read_history(&self) -> Result<Vec<Record>> {
        let mut file = match fs::File::open(&self.db_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        file.lock_shared()?;

        let mut content = String::new();
        file.read_to_string(&mut content)?;

        let report = parse_records_jsonl_report_impl(&content);
        if report.skipped_legacy_diff_target_records > 0 {
            warn!(
                "Skipped {} legacy diff-target review records for compatibility",
                report.skipped_legacy_diff_target_records
            );
        }
        Ok(report.records)
    }

    fn append(&self, record: &Record) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.db_path)?;
        file.lock_exclusive()?;

        let file_len = file.metadata()?.len();
        if file_len > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last_byte = [0];
            file.read_exact(&mut last_byte)?;
            if last_byte[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }

        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }
}

pub struct FileStore {
    location: StoreLocation,
    backend: JsonlStoreBackend,
}

impl FileStore {
    pub fn new() -> Result<Self> {
        let location = StoreLocation::discover()?;
        Self::from_location(location)
    }

    pub fn for_root(root_path: impl Into<PathBuf>) -> Result<Self> {
        let location = StoreLocation {
            root_path: root_path.into(),
        };
        location.ensure_trueflow_dir()?;
        Self::from_location(location)
    }

    fn from_location(location: StoreLocation) -> Result<Self> {
        let backend = JsonlStoreBackend::new(location.db_path());
        Ok(Self { location, backend })
    }

    pub fn db_path(&self) -> PathBuf {
        self.location.db_path()
    }

    pub fn trueflow_dir(&self) -> PathBuf {
        self.location.trueflow_dir()
    }
}

impl ReviewStore for FileStore {
    fn read_history(&self) -> Result<Vec<Record>> {
        self.backend.read_history()
    }

    fn append(&self, record: &Record) -> Result<()> {
        self.backend.append(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        id: &str,
        target: ReviewTargetRef,
        check: &str,
        verdict: Verdict,
        timestamp: i64,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: CURRENT_VERSION,
            target,
            check: ReviewCheck::new(check).unwrap(),
            verdict,
            identity: Identity::Email {
                email: "dev@example.com".to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new("0123456789abcdef").unwrap(),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: Some(RepoPath::new("src/lib.rs").unwrap()),
            line_hint: Some(1),
            note: None,
            comment_scope: None,
            comment_context: None,
            comment_anchor: None,
            tags: None,
            attestations: None,
        }
    }

    #[test]
    fn record_new_generates_identity_fields_and_empty_optional_metadata() {
        let before = chrono::Utc::now().timestamp();
        let target = ReviewTargetRef::Block {
            hash: TreeHash::from_content("fn demo() {}\n"),
        };
        let record = Record::new(
            target.clone(),
            ReviewCheck::review(),
            Verdict::Approved,
            Identity::Email {
                email: "dev@example.com".to_string(),
            },
            RepoRef::Unknown,
            BlockState::Unknown,
        );
        let after = chrono::Utc::now().timestamp();

        assert_eq!(record.version, CURRENT_VERSION);
        assert_eq!(record.target, target);
        assert_eq!(record.check, ReviewCheck::review());
        assert_eq!(record.verdict, Verdict::Approved);
        assert!(uuid::Uuid::parse_str(&record.id).is_ok());
        assert!((before..=after).contains(&record.timestamp));
        assert_eq!(record.path_hint, None);
        assert_eq!(record.line_hint, None);
        assert_eq!(record.note, None);
        assert_eq!(record.comment_scope, None);
        assert_eq!(record.comment_context, None);
        assert_eq!(record.comment_anchor, None);
        assert_eq!(record.tags, None);
        assert!(record.attestations.is_none());
    }

    #[test]
    fn review_target_kind_parses_typed_hash_targets() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let block = ReviewTargetKind::Block.parse_target(hash).unwrap();
        let file = ReviewTargetKind::File.parse_target(hash).unwrap();
        let tree = ReviewTargetKind::Tree.parse_target(hash).unwrap();

        assert_eq!(
            block,
            ReviewTargetRef::Block {
                hash: TreeHash::parse(hash).unwrap()
            }
        );
        assert_eq!(
            file,
            ReviewTargetRef::File {
                hash: TreeHash::parse(hash).unwrap()
            }
        );
        assert_eq!(
            tree,
            ReviewTargetRef::Tree {
                hash: TreeHash::parse(hash).unwrap()
            }
        );
    }

    #[test]
    fn signing_payload_omits_scoped_comment_fields_for_legacy_versions() {
        let mut legacy = record(
            "legacy",
            ReviewTargetRef::Block {
                hash: TreeHash::parse(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .unwrap(),
            },
            "review",
            Verdict::Comment,
            1,
        );
        legacy.version = 2;
        legacy.comment_scope = Some(CommentScope {
            start_line: 3,
            end_line: 7,
        });
        legacy.comment_context = Some("scoped".to_string());

        let payload = legacy
            .signing_payload()
            .unwrap_or_else(|error| panic!("legacy signing payload: {error}"));

        assert!(!payload.contains("comment_scope"));
        assert!(!payload.contains("comment_context"));
    }

    #[test]
    fn parse_records_jsonl_reports_skipped_legacy_diff_target_records() {
        let content = concat!(
            "{\"id\":\"legacy-diff\",\"version\":2,\"target\":{\"kind\":\"diff\",\"fingerprint\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"},\"check\":\"review\",\"verdict\":\"approved\",\"identity\":{\"type\":\"email\",\"email\":\"dev@example.com\"},\"repo_ref\":{\"type\":\"vcs\",\"system\":\"git\",\"revision\":\"0123456789abcdef\"},\"block_state\":\"committed\",\"timestamp\":1,\"path_hint\":null,\"line_hint\":null,\"note\":null,\"tags\":null}\n",
            "{\"id\":\"typed\",\"version\":2,\"target\":{\"kind\":\"block\",\"hash\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"},\"check\":\"review\",\"verdict\":\"approved\",\"identity\":{\"type\":\"email\",\"email\":\"dev@example.com\"},\"repo_ref\":{\"type\":\"vcs\",\"system\":\"git\",\"revision\":\"0123456789abcdef\"},\"block_state\":\"committed\",\"timestamp\":2,\"path_hint\":null,\"line_hint\":null,\"note\":null,\"tags\":null}\n"
        );

        let report = parse_records_jsonl_report(content);
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.skipped_legacy_diff_target_records, 1);
    }

    #[test]
    fn review_index_prefers_highest_timestamp_for_typed_target() {
        let target = ReviewTargetRef::Block {
            hash: TreeHash::new("typed-key"),
        };
        let records = vec![
            record("1", target.clone(), "review", Verdict::Rejected, 1),
            record("2", target.clone(), "review", Verdict::Approved, 2),
            record("3", target.clone(), "review", Verdict::Comment, 0),
        ];

        let index = ReviewIndex::from_records(&records, Some(&ReviewCheck::review()));
        assert_eq!(index.verdict_for(&target), Some(&Verdict::Approved));
    }

    #[test]
    fn review_index_uses_last_entry_for_equal_timestamp() {
        let target = ReviewTargetRef::Block {
            hash: TreeHash::new("typed-key"),
        };
        let records = vec![
            record("1", target.clone(), "review", Verdict::Rejected, 5),
            record("2", target.clone(), "review", Verdict::Approved, 5),
        ];

        let index = ReviewIndex::from_records(&records, Some(&ReviewCheck::review()));
        assert_eq!(index.verdict_for(&target), Some(&Verdict::Approved));
    }

    #[test]
    fn review_index_ignores_non_matching_checks() {
        let target = ReviewTargetRef::Block {
            hash: TreeHash::new("typed-key"),
        };
        let records = vec![
            record("1", target.clone(), "security", Verdict::Rejected, 10),
            record("2", target.clone(), "review", Verdict::Approved, 1),
        ];

        let review_index = ReviewIndex::from_records(&records, Some(&ReviewCheck::review()));
        let any_index = ReviewIndex::from_records(&records, None);
        assert_eq!(review_index.verdict_for(&target), Some(&Verdict::Approved));
        assert_eq!(any_index.verdict_for(&target), Some(&Verdict::Rejected));
    }

    #[test]
    fn review_index_uses_exact_block_location_before_hash_fallback() {
        let hash = TreeHash::new("typed-key");
        let target = ReviewTargetRef::Block { hash: hash.clone() };
        let mut precise = record("1", target.clone(), "review", Verdict::Approved, 2);
        precise.path_hint = Some(RepoPath::new("src/lib.rs").unwrap());
        precise.line_hint = Some(10);

        let mut coarse = record("2", target, "review", Verdict::Rejected, 1);
        coarse.path_hint = None;
        coarse.line_hint = None;

        let index = ReviewIndex::from_records(&[precise, coarse], Some(&ReviewCheck::review()));

        assert_eq!(
            index.block_verdict_for(&hash, &RepoPath::new("src/lib.rs").unwrap(), 10, None),
            Some(&Verdict::Approved)
        );
        assert_eq!(
            index.block_verdict_for(&hash, &RepoPath::new("src/lib.rs").unwrap(), 11, None),
            Some(&Verdict::Rejected)
        );
    }

    #[test]
    fn approved_targets_track_typed_target_kinds_separately() {
        let approved = vec![
            record(
                "1",
                ReviewTargetRef::Block {
                    hash: TreeHash::new("block-hash"),
                },
                "review",
                Verdict::Approved,
                1,
            ),
            record(
                "2",
                ReviewTargetRef::File {
                    hash: TreeHash::new("file-hash"),
                },
                "review",
                Verdict::Approved,
                2,
            ),
            record(
                "3",
                ReviewTargetRef::Tree {
                    hash: TreeHash::new("tree-hash"),
                },
                "review",
                Verdict::Approved,
                3,
            ),
        ];

        let index = ReviewIndex::from_records(&approved, Some(&ReviewCheck::review()));
        let approved_targets = index.approved_targets();

        assert!(approved_targets.contains_block(
            &TreeHash::new("block-hash"),
            &RepoPath::new("src/lib.rs").unwrap(),
            1,
            None,
        ));
        assert!(approved_targets.contains_target(&ReviewTargetRef::File {
            hash: TreeHash::new("file-hash")
        }));
        assert!(approved_targets.contains_target(&ReviewTargetRef::Tree {
            hash: TreeHash::new("tree-hash")
        }));
    }

    #[test]
    fn approved_targets_keep_path_scoped_file_hashes_distinct() {
        let hash = TreeHash::new("shared-file-hash");
        let mut approved = record(
            "1",
            ReviewTargetRef::File { hash: hash.clone() },
            "review",
            Verdict::Approved,
            1,
        );
        approved.path_hint = Some(RepoPath::new("src/a.rs").unwrap());

        let index = ReviewIndex::from_records(&[approved], Some(&ReviewCheck::review()));
        let approved_targets = index.approved_targets();

        assert!(approved_targets.contains_file(&hash, &RepoPath::new("src/a.rs").unwrap(), None));
        assert!(!approved_targets.contains_file(&hash, &RepoPath::new("src/b.rs").unwrap(), None));
    }

    #[test]
    fn merge_record_histories_dedupes_by_id_and_sorts_by_timestamp() {
        let target = ReviewTargetRef::Block {
            hash: TreeHash::new("typed-key"),
        };
        let merged = merge_record_histories(
            vec![record(
                "dup",
                target.clone(),
                "review",
                Verdict::Approved,
                2,
            )],
            vec![
                record("dup", target.clone(), "review", Verdict::Rejected, 1),
                record("unique", target, "review", Verdict::Comment, 0),
            ],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "unique");
        assert_eq!(merged[1].id, "dup");
    }
}
