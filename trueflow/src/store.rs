use anyhow::Result;
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::warn;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::vcs;

const TRUEFLOW_DIR: &str = ".trueflow";
const DB_FILE: &str = "reviews.jsonl";
pub const CURRENT_VERSION: u32 = 1;

fn default_version() -> u32 {
    0 // Legacy records
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(tag = "type")]
#[schemars(deny_unknown_fields)]
pub enum Identity {
    #[serde(rename = "email")]
    Email {
        #[schemars(email)]
        email: String,
    },
    // Future: OIDC, DID, etc.
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum RepoRef {
    Vcs {
        system: VcsSystem,
        #[schemars(regex(pattern = "^[0-9a-f]{7,40}$"))]
        revision: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(deny_unknown_fields)]
pub enum BlockState {
    Committed,
    Uncommitted,
    Unknown,
}

pub use crate::hashing::ContentHash;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
pub struct DiffFingerprint(String);

impl DiffFingerprint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DiffFingerprint {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for DiffFingerprint {
    fn from(value: &str) -> Self {
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
    Diff,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[schemars(deny_unknown_fields)]
pub enum ReviewTargetRef {
    Block { hash: ContentHash },
    File { hash: ContentHash },
    Tree { hash: ContentHash },
    Diff { fingerprint: DiffFingerprint },
}

impl ReviewTargetRef {
    pub fn lookup_key(&self) -> &str {
        match self {
            ReviewTargetRef::Block { hash }
            | ReviewTargetRef::File { hash }
            | ReviewTargetRef::Tree { hash } => hash.as_str(),
            ReviewTargetRef::Diff { fingerprint } => fingerprint.as_str(),
        }
    }
}

impl ReviewTargetKind {
    pub fn into_target(self, value: impl Into<String>) -> ReviewTargetRef {
        let value = value.into();
        match self {
            ReviewTargetKind::Block => ReviewTargetRef::Block {
                hash: ContentHash::new(value),
            },
            ReviewTargetKind::File => ReviewTargetRef::File {
                hash: ContentHash::new(value),
            },
            ReviewTargetKind::Tree => ReviewTargetRef::Tree {
                hash: ContentHash::new(value),
            },
            ReviewTargetKind::Diff => ReviewTargetRef::Diff {
                fingerprint: DiffFingerprint::new(value),
            },
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
    // Schema version
    #[serde(default = "default_version")]
    #[schemars(range(min = 0))]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ReviewTargetRef>,
    pub fingerprint: String,
    #[schemars(length(min = 1))]
    pub check: String,
    pub verdict: Verdict,

    pub identity: Identity,

    pub repo_ref: RepoRef,
    pub block_state: BlockState,

    #[schemars(range(min = 0))]
    pub timestamp: i64,
    pub path_hint: Option<String>,
    pub line_hint: Option<u32>,
    pub note: Option<String>,
    #[schemars(inner(length(min = 1)))]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attestations: Option<Vec<Attestation>>,
}

impl Record {
    pub fn lookup_key(&self) -> &str {
        self.target
            .as_ref()
            .map_or(self.fingerprint.as_str(), ReviewTargetRef::lookup_key)
    }

    pub fn signing_payload(&self) -> Result<String> {
        let mut payload = self.clone();
        payload.attestations = None;
        Ok(serde_jcs::to_string(&payload)?)
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
            _ => Err(anyhow::anyhow!("Unknown verdict: {value}")),
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

pub trait ReviewStore {
    fn read_history(&self) -> Result<Vec<Record>>;
    fn append(&self, record: Record) -> Result<()>;
}

pub fn latest_verdicts(records: &[Record], check_filter: Option<&str>) -> HashMap<String, Verdict> {
    let mut latest_by_target_key: HashMap<String, (i64, Verdict)> = HashMap::new();

    for record in records {
        if check_filter.is_some_and(|check| record.check != check) {
            continue;
        }

        let key = record.lookup_key();
        match latest_by_target_key.get_mut(key) {
            Some((timestamp, verdict)) => {
                // Keep semantics compatible with stable-sort behavior:
                // for equal timestamps, later entries in input order win.
                if record.timestamp >= *timestamp {
                    *timestamp = record.timestamp;
                    *verdict = record.verdict.clone();
                }
            }
            None => {
                latest_by_target_key
                    .insert(key.to_string(), (record.timestamp, record.verdict.clone()));
            }
        }
    }

    latest_by_target_key
        .into_iter()
        .map(|(key, (_, verdict))| (key, verdict))
        .collect()
}

pub fn latest_review_verdicts(records: &[Record]) -> HashMap<String, Verdict> {
    latest_verdicts(records, Some("review"))
}

pub fn approved_hashes_from_verdicts(verdicts: &HashMap<String, Verdict>) -> HashSet<String> {
    verdicts
        .iter()
        .filter_map(|(hash, verdict)| {
            if verdict == &Verdict::Approved {
                Some(hash.clone())
            } else {
                None
            }
        })
        .collect()
}

pub struct FileStore {
    root_path: PathBuf,
}

fn ensure_trueflow_dir(root: &Path) -> Result<()> {
    let trueflow_dir = root.join(TRUEFLOW_DIR);
    if !trueflow_dir.exists() {
        fs::create_dir(&trueflow_dir)?;
    }
    Ok(())
}

impl FileStore {
    pub fn new() -> Result<Self> {
        if let Ok(Some(root)) = vcs::git_root_from_workdir() {
            ensure_trueflow_dir(&root)?;
            return Ok(Self { root_path: root });
        }

        let start_dir = std::env::current_dir()?;
        for dir in start_dir.ancestors() {
            if dir.join(TRUEFLOW_DIR).exists() {
                return Ok(Self {
                    root_path: dir.to_path_buf(),
                });
            }
        }

        ensure_trueflow_dir(&start_dir)?;
        Ok(Self {
            root_path: start_dir,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.root_path.join(TRUEFLOW_DIR).join(DB_FILE)
    }

    pub fn trueflow_dir(&self) -> PathBuf {
        self.root_path.join(TRUEFLOW_DIR)
    }
}

impl ReviewStore for FileStore {
    fn read_history(&self) -> Result<Vec<Record>> {
        let db_path = self.db_path();

        if !db_path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(db_path)?;
        file.lock_shared()?; // Shared lock for reading

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

        // Lock releases when file is dropped
        Ok(records)
    }

    fn append(&self, record: Record) -> Result<()> {
        let db_path = self.db_path();

        let mut file = OpenOptions::new().create(true).append(true).open(db_path)?;
        file.lock_exclusive()?; // Exclusive lock for appending

        let mut line = serde_json::to_string(&record)?;
        line.push('\n');

        file.write_all(line.as_bytes())?;

        // Lock releases when file is dropped
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        id: &str,
        fingerprint: &str,
        check: &str,
        verdict: Verdict,
        timestamp: i64,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: CURRENT_VERSION,
            target: None,
            fingerprint: fingerprint.to_string(),
            check: check.to_string(),
            verdict,
            identity: Identity::Email {
                email: "dev@example.com".to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: "0123456789abcdef".to_string(),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: Some("src/lib.rs".to_string()),
            line_hint: Some(1),
            note: None,
            tags: None,
            attestations: None,
        }
    }

    #[test]
    fn latest_review_verdicts_prefers_highest_timestamp() {
        let records = vec![
            record("1", "fp", "review", Verdict::Rejected, 1),
            record("2", "fp", "review", Verdict::Approved, 2),
            record("3", "fp", "review", Verdict::Comment, 0),
        ];

        let latest = latest_review_verdicts(&records);
        assert_eq!(latest.get("fp"), Some(&Verdict::Approved));
    }

    #[test]
    fn verdict_from_str_maps_question_to_comment() {
        let parsed = "question".parse::<Verdict>();
        assert!(
            matches!(parsed, Ok(Verdict::Comment)),
            "question should parse as comment, got {parsed:?}"
        );
    }

    #[test]
    fn verdict_deserialize_accepts_question_alias_as_comment() {
        let parsed = serde_json::from_str::<Verdict>("\"question\"");
        assert!(
            matches!(parsed, Ok(Verdict::Comment)),
            "question alias should deserialize as comment, got {parsed:?}"
        );
    }

    #[test]
    fn latest_review_verdicts_uses_last_entry_for_equal_timestamp() {
        let records = vec![
            record("1", "fp", "review", Verdict::Rejected, 5),
            record("2", "fp", "review", Verdict::Approved, 5),
        ];

        let latest = latest_review_verdicts(&records);
        assert_eq!(latest.get("fp"), Some(&Verdict::Approved));
    }

    #[test]
    fn latest_review_verdicts_ignores_non_review_checks() {
        let records = vec![
            record("1", "fp", "security", Verdict::Rejected, 10),
            record("2", "fp", "review", Verdict::Approved, 1),
        ];

        let latest = latest_review_verdicts(&records);
        assert_eq!(latest.get("fp"), Some(&Verdict::Approved));
    }

    #[test]
    fn latest_verdicts_without_check_filter_uses_latest_timestamp() {
        let records = vec![
            record("1", "fp", "review", Verdict::Approved, 1),
            record("2", "fp", "security", Verdict::Rejected, 2),
        ];

        let latest = latest_verdicts(&records, None);
        assert_eq!(latest.get("fp"), Some(&Verdict::Rejected));
    }

    #[test]
    fn record_lookup_key_prefers_typed_target_when_present() {
        let record = Record {
            id: "typed".to_string(),
            version: CURRENT_VERSION,
            target: Some(ReviewTargetRef::Diff {
                fingerprint: DiffFingerprint::new("diff-fp"),
            }),
            fingerprint: "legacy-block-fp".to_string(),
            check: "review".to_string(),
            verdict: Verdict::Approved,
            identity: Identity::Email {
                email: "dev@example.com".to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: "0123456789abcdef".to_string(),
            },
            block_state: BlockState::Committed,
            timestamp: 1,
            path_hint: Some("src/lib.rs".to_string()),
            line_hint: Some(1),
            note: None,
            tags: None,
            attestations: None,
        };

        assert_eq!(record.lookup_key(), "diff-fp");
    }

    #[test]
    fn latest_review_verdicts_uses_typed_target_key() {
        let mut legacy = record("1", "legacy-key", "review", Verdict::Rejected, 1);
        legacy.target = Some(ReviewTargetRef::Block {
            hash: ContentHash::new("typed-key"),
        });

        let mut typed_later = record("2", "legacy-key", "review", Verdict::Approved, 2);
        typed_later.target = Some(ReviewTargetRef::Block {
            hash: ContentHash::new("typed-key"),
        });

        let latest = latest_review_verdicts(&[legacy, typed_later]);
        assert_eq!(latest.get("typed-key"), Some(&Verdict::Approved));
        assert!(!latest.contains_key("legacy-key"));
    }
}
