use anyhow::Result;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::str::FromStr;

const TRUEFLOW_DIR: &str = ".trueflow";
const DB_FILE: &str = "reviews.jsonl";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Identity {
    #[serde(rename = "email")]
    Email {
        email: String,
        // Optional PGP/Sig
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    // Future: OIDC, DID, etc.
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approved,
    Rejected,
    Question,
    Comment,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Record {
    pub id: String,
    pub fingerprint: String,
    pub check: String,
    pub verdict: Verdict,

    pub identity: Identity,

    pub timestamp: i64,
    pub path_hint: Option<String>,
    pub line_hint: Option<u32>,
    pub note: Option<String>,
    pub tags: Option<Vec<String>>,
}

pub trait ReviewStore {
    fn read_history(&self) -> Result<Vec<Record>>;
    fn append(&self, record: Record) -> Result<()>;
}

pub struct FileStore {
    root_path: PathBuf,
}

impl FileStore {
    pub fn new() -> Result<Self> {
        let start_dir = std::env::current_dir()?;

        // Prefer git root when available
        if let Ok(repo) = Repository::discover(&start_dir)
            && let Some(workdir) = repo.workdir()
        {
            let root = workdir.to_path_buf();
            let trueflow_dir = root.join(TRUEFLOW_DIR);
            if !trueflow_dir.exists() {
                fs::create_dir(&trueflow_dir)?;
            }
            return Ok(Self { root_path: root });
        }

        let mut current = Some(start_dir.as_path());
        while let Some(dir) = current {
            let trueflow_dir = dir.join(TRUEFLOW_DIR);
            if trueflow_dir.exists() {
                return Ok(Self {
                    root_path: dir.to_path_buf(),
                });
            }
            current = dir.parent();
        }

        // Fallback to creating in current directory
        let root = start_dir;
        let trueflow_dir = root.join(TRUEFLOW_DIR);
        if !trueflow_dir.exists() {
            fs::create_dir(&trueflow_dir)?;
        }

        Ok(Self { root_path: root })
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
        let reader = BufReader::new(file);
        let mut records = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(&line) {
                Ok(record) => records.push(record),
                Err(err) => eprintln!("Skipping malformed record: {}", err),
            }
        }

        Ok(records)
    }

    fn append(&self, record: Record) -> Result<()> {
        let db_path = self.db_path();

        let mut file = OpenOptions::new().create(true).append(true).open(db_path)?;

        let mut line = serde_json::to_string(&record)?;
        line.push('\n');

        file.write_all(line.as_bytes())?;

        Ok(())
    }
}
