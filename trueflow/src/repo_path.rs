use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let raw = value.as_ref();
        let normalized = normalize_repo_path_string(raw);

        if is_absolute_repo_path(raw, &normalized) {
            return Err(anyhow!("repo path must be relative: {raw}"));
        }

        if normalized.is_empty() {
            return Ok(Self::root());
        }

        for segment in normalized.split('/') {
            if segment.is_empty() {
                return Err(anyhow!("repo path contains empty segment: {raw}"));
            }
            if matches!(segment, "." | "..") {
                return Err(anyhow!(
                    "repo path contains invalid segment {segment:?}: {raw}"
                ));
            }
        }

        Ok(Self(normalized))
    }

    pub fn from_relative_path(path: &Path) -> Result<Self> {
        Self::new(path.to_string_lossy().as_ref())
    }

    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    pub fn join(&self, child: &str) -> Result<Self> {
        if child.is_empty() {
            return Err(anyhow!("repo path child must not be empty"));
        }

        let joined = if self.is_root() {
            child.to_string()
        } else {
            format!("{}/{}", self.0, child)
        };
        Self::new(joined)
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for RepoPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for RepoPath {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<RepoPath> for String {
    fn from(value: RepoPath) -> Self {
        value.0
    }
}

impl TryFrom<String> for RepoPath {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for RepoPath {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

pub(crate) fn normalize_repo_path_string(path: &str) -> String {
    let normalized = path.trim_start_matches("./").replace('\\', "/");
    if normalized == "." {
        String::new()
    } else {
        normalized
    }
}

fn is_absolute_repo_path(raw: &str, normalized: &str) -> bool {
    Path::new(raw).is_absolute()
        || normalized.starts_with('/')
        || has_windows_drive_prefix(normalized)
}

fn has_windows_drive_prefix(value: &str) -> bool {
    value.len() >= 2
        && value.as_bytes()[1] == b':'
        && value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::RepoPath;

    #[test]
    fn repo_path_normalizes_relative_separators_and_root() {
        assert_eq!(
            RepoPath::new("./src\\lib.rs").unwrap().as_str(),
            "src/lib.rs"
        );
        assert_eq!(RepoPath::new(".").unwrap().as_str(), "");
        assert!(RepoPath::root().is_root());
    }

    #[test]
    fn repo_path_rejects_absolute_and_parent_segments() {
        assert!(RepoPath::new("/tmp/file.rs").is_err());
        assert!(RepoPath::new("../file.rs").is_err());
        assert!(RepoPath::new("src/../file.rs").is_err());
        assert!(RepoPath::new("src//file.rs").is_err());
    }
}
