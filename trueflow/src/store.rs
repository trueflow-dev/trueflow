use anyhow::Result;
use fs2::FileExt;
use log::warn;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    Question,
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
            Verdict::Question => "question",
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
            "question" => Ok(Verdict::Question),
            "comment" => Ok(Verdict::Comment),
            _ => Err(anyhow::anyhow!("Unknown verdict: {}", value)),
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
                Err(err) => warn!("Skipping malformed record: {}", err),
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
