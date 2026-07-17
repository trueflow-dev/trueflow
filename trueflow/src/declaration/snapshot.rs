use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::analysis::Language;
use crate::hashing::BytesHash;

/// Stable identity assigned to one exact source capture.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable identity for one independently diffed base/head pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotPairId(String);

impl SnapshotPairId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Evidence that permits declarations at the two paths to be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathPairEvidence {
    /// Both snapshots describe the same path.
    SamePath,
    /// The source resolver proved that the head path is a rename of the base path.
    ExplicitRename,
    /// No relationship between two endpoint paths has been established.
    Unmatched,
}

/// An immutable, byte-exact source generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub id: SnapshotId,
    pub path: PathBuf,
    pub language: Language,
    source: Arc<str>,
    bytes_hash: BytesHash,
}

impl SourceSnapshot {
    pub fn new(
        id: SnapshotId,
        path: &Path,
        language: Language,
        source: impl AsRef<str>,
    ) -> Self {
        let source: Arc<str> = Arc::from(source.as_ref());
        let bytes_hash = BytesHash::from_bytes(source.as_bytes());
        Self {
            id,
            path: path.to_path_buf(),
            language,
            source,
            bytes_hash,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn bytes_hash(&self) -> &BytesHash {
        &self.bytes_hash
    }
}

/// One independently resolved source comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPair {
    pub id: SnapshotPairId,
    pub base: Option<SourceSnapshot>,
    pub head: Option<SourceSnapshot>,
    pub path_evidence: PathPairEvidence,
}

impl SnapshotPair {
    pub fn new(
        id: SnapshotPairId,
        base: Option<SourceSnapshot>,
        head: Option<SourceSnapshot>,
        path_evidence: PathPairEvidence,
    ) -> Self {
        Self {
            id,
            base,
            head,
            path_evidence,
        }
    }
}
