use anyhow::{Result, anyhow};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::warn;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::str::FromStr;

use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::vcs;

const TRUEFLOW_DIR: &str = ".trueflow";
const DB_FILE: &str = "reviews.jsonl";
pub const CURRENT_VERSION: u32 = 2;

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
pub struct RepoRevision(String);

impl RepoRevision {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if !(7..=40).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(anyhow!(
                "repo revision must be a 7-40 character hex string: {value}"
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum RepoRef {
    Vcs {
        system: VcsSystem,
        revision: RepoRevision,
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
    #[schemars(inner(length(min = 1)))]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attestations: Option<Vec<Attestation>>,
}

#[derive(Serialize)]
struct SignableRecord<'a> {
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

impl Record {
    pub fn signing_payload(&self) -> Result<String> {
        Ok(serde_jcs::to_string(&SignableRecord {
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

#[derive(Debug, Clone, Default)]
pub struct ApprovedTargets {
    block_hashes: HashSet<TreeHash>,
    path_scoped_block_targets: HashSet<PathScopedBlockTarget>,
    exact_block_targets: HashSet<ExactBlockTarget>,
    file_hashes: HashSet<TreeHash>,
    tree_hashes: HashSet<TreeHash>,
}

impl ApprovedTargets {
    pub fn contains_target(&self, target: &ReviewTargetRef) -> bool {
        match target {
            ReviewTargetRef::Block { hash } => self.block_hashes.contains(hash),
            ReviewTargetRef::File { hash } => self.file_hashes.contains(hash),
            ReviewTargetRef::Tree { hash } => self.tree_hashes.contains(hash),
        }
    }

    pub fn contains_block(
        &self,
        hash: &TreeHash,
        path: &RepoPath,
        start_line: usize,
        workdir_prefix: Option<&str>,
    ) -> bool {
        let candidates = block_path_candidates(path, workdir_prefix);
        if let Ok(start_line) = u32::try_from(start_line) {
            for candidate in &candidates {
                if self.exact_block_targets.contains(&ExactBlockTarget {
                    hash: hash.clone(),
                    path: candidate.clone(),
                    start_line,
                }) {
                    return true;
                }
            }
        }

        for candidate in &candidates {
            if self
                .path_scoped_block_targets
                .contains(&PathScopedBlockTarget {
                    hash: hash.clone(),
                    path: candidate.clone(),
                })
            {
                return true;
            }
        }

        self.block_hashes.contains(hash)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewIndex {
    latest_verdicts: HashMap<ReviewTargetRef, Verdict>,
    block_hash_verdicts: HashMap<TreeHash, Verdict>,
    path_scoped_block_verdicts: HashMap<PathScopedBlockTarget, Verdict>,
    exact_block_verdicts: HashMap<ExactBlockTarget, Verdict>,
}

impl ReviewIndex {
    pub fn from_records(records: &[Record], check_filter: Option<&ReviewCheck>) -> Self {
        let mut latest_by_target: HashMap<ReviewTargetRef, (i64, Verdict)> = HashMap::new();
        let mut block_hash_verdicts: HashMap<TreeHash, (i64, Verdict)> = HashMap::new();
        let mut path_scoped_block_verdicts: HashMap<PathScopedBlockTarget, (i64, Verdict)> =
            HashMap::new();
        let mut exact_block_verdicts: HashMap<ExactBlockTarget, (i64, Verdict)> = HashMap::new();

        for record in records {
            if check_filter.is_some_and(|check| &record.check != check) {
                continue;
            }

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

            if let ReviewTargetRef::Block { hash } = &record.target {
                match (&record.path_hint, record.line_hint) {
                    (Some(path), Some(start_line)) => update_latest_verdict(
                        &mut exact_block_verdicts,
                        ExactBlockTarget {
                            hash: hash.clone(),
                            path: path.clone(),
                            start_line,
                        },
                        record.timestamp,
                        record.verdict.clone(),
                    ),
                    (Some(path), None) => update_latest_verdict(
                        &mut path_scoped_block_verdicts,
                        PathScopedBlockTarget {
                            hash: hash.clone(),
                            path: path.clone(),
                        },
                        record.timestamp,
                        record.verdict.clone(),
                    ),
                    (None, _) => update_latest_verdict(
                        &mut block_hash_verdicts,
                        hash.clone(),
                        record.timestamp,
                        record.verdict.clone(),
                    ),
                }
            }
        }

        Self {
            latest_verdicts: latest_by_target
                .into_iter()
                .map(|(target, (_, verdict))| (target, verdict))
                .collect(),
            block_hash_verdicts: block_hash_verdicts
                .into_iter()
                .map(|(key, (_, verdict))| (key, verdict))
                .collect(),
            path_scoped_block_verdicts: path_scoped_block_verdicts
                .into_iter()
                .map(|(key, (_, verdict))| (key, verdict))
                .collect(),
            exact_block_verdicts: exact_block_verdicts
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
        let candidates = block_path_candidates(path, workdir_prefix);
        if let Ok(start_line) = u32::try_from(start_line) {
            for candidate in &candidates {
                if let Some(verdict) = self.exact_block_verdicts.get(&ExactBlockTarget {
                    hash: hash.clone(),
                    path: candidate.clone(),
                    start_line,
                }) {
                    return Some(verdict);
                }
            }
        }

        for candidate in &candidates {
            if let Some(verdict) = self.path_scoped_block_verdicts.get(&PathScopedBlockTarget {
                hash: hash.clone(),
                path: candidate.clone(),
            }) {
                return Some(verdict);
            }
        }

        self.block_hash_verdicts.get(hash)
    }
    pub fn approved_targets(&self) -> ApprovedTargets {
        let mut approved = ApprovedTargets::default();
        for (target, verdict) in &self.latest_verdicts {
            if verdict != &Verdict::Approved {
                continue;
            }
            match target {
                ReviewTargetRef::Block { .. } => {}
                ReviewTargetRef::File { hash } => {
                    approved.file_hashes.insert(hash.clone());
                }
                ReviewTargetRef::Tree { hash } => {
                    approved.tree_hashes.insert(hash.clone());
                }
            }
        }

        for (hash, verdict) in &self.block_hash_verdicts {
            if verdict == &Verdict::Approved {
                approved.block_hashes.insert(hash.clone());
            }
        }
        for (target, verdict) in &self.path_scoped_block_verdicts {
            if verdict == &Verdict::Approved {
                approved.path_scoped_block_targets.insert(target.clone());
            }
        }
        for (target, verdict) in &self.exact_block_verdicts {
            if verdict == &Verdict::Approved {
                approved.exact_block_targets.insert(target.clone());
            }
        }

        approved
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PathScopedBlockTarget {
    hash: TreeHash,
    path: RepoPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExactBlockTarget {
    hash: TreeHash,
    path: RepoPath,
    start_line: u32,
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

fn block_path_candidates(path: &RepoPath, workdir_prefix: Option<&str>) -> Vec<RepoPath> {
    let mut candidates = Vec::new();
    for candidate in path_utils::candidate_repo_paths_for_hint(path.as_str(), workdir_prefix, None)
    {
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

#[cfg(test)]
pub fn parse_records_jsonl(content: &str) -> Vec<Record> {
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<Record>(line) {
            Ok(record) => Some(record),
            Err(err) => {
                warn!("Skipping malformed record: {err}");
                None
            }
        })
        .collect()
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
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&self.db_path)?;
        file.lock_shared()?;

        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(&line) {
                Ok(record) => records.push(record),
                Err(err) => warn!("Skipping malformed record: {err}"),
            }
        }
        Ok(records)
    }

    fn append(&self, record: &Record) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.db_path)?;
        file.lock_exclusive()?;

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
                revision: RepoRevision::new("0123456789abcdef").unwrap(),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: Some(RepoPath::new("src/lib.rs").unwrap()),
            line_hint: Some(1),
            note: None,
            tags: None,
            attestations: None,
        }
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
    fn parse_records_jsonl_skips_legacy_diff_target_records() {
        let content = "{\"id\":\"legacy-diff\",\"version\":2,\"target\":{\"kind\":\"diff\",\"fingerprint\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"},\"check\":\"review\",\"verdict\":\"approved\",\"identity\":{\"type\":\"email\",\"email\":\"dev@example.com\"},\"repo_ref\":{\"type\":\"vcs\",\"system\":\"git\",\"revision\":\"0123456789abcdef\"},\"block_state\":\"committed\",\"timestamp\":1,\"path_hint\":null,\"line_hint\":null,\"note\":null,\"tags\":null}\n";

        assert!(parse_records_jsonl(content).is_empty());
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
