use anyhow::{Context, Result};
use git2::{Repository};
use serde::{Deserialize, Serialize};


const DB_BRANCH: &str = "refs/heads/vet-db";
const DB_FILE: &str = "reviews.jsonl";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Record {
    pub id: String,
    pub fingerprint: String,
    pub check: String,
    pub verdict: String,
    pub author: String,
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

pub struct GitRefStore {
    repo: Repository,
}

impl GitRefStore {
    pub fn new() -> Result<Self> {
        // Attempt to discover repo from current directory
        let repo = Repository::discover(".").context("Failed to discover git repository")?;
        Ok(Self { repo })
    }
}

impl ReviewStore for GitRefStore {
    fn read_history(&self) -> Result<Vec<Record>> {
        let branch = match self.repo.find_reference(DB_BRANCH) {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()), // Branch doesn't exist yet
        };

        let commit = branch.peel_to_commit().context("vet-db ref is not a commit")?;
        let tree = commit.tree().context("Failed to get tree")?;

        let entry = match tree.get_name(DB_FILE) {
            Some(e) => e,
            None => return Ok(Vec::new()), // File doesn't exist in tree
        };

        let object = entry.to_object(&self.repo).context("Failed to get object")?;
        let blob = object.as_blob().context("Not a blob")?;
        
        let content = std::str::from_utf8(blob.content()).context("Invalid UTF-8 in DB")?;
        
        let mut records = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() { continue; }
            let record: Record = serde_json::from_str(line).context("Failed to parse record")?;
            records.push(record);
        }
        
        Ok(records)
    }

    fn append(&self, record: Record) -> Result<()> {
        // serialize record
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');

        // Get current state
        let (parent_commit, current_content) = match self.repo.find_reference(DB_BRANCH) {
            Ok(r) => {
                let commit = r.peel_to_commit()?;
                let tree = commit.tree()?;
                let content = match tree.get_name(DB_FILE) {
                    Some(entry) => {
                        let obj = entry.to_object(&self.repo)?;
                        let blob = obj.as_blob().context("Not a blob")?;
                        String::from_utf8(blob.content().to_vec())?
                    },
                    None => String::new(),
                };
                (Some(commit), content)
            },
            Err(_) => (None, String::new()),
        };

        let new_content = current_content + &line;
        let blob_oid = self.repo.blob(new_content.as_bytes())?;

        // Create tree
        let mut tree_builder = self.repo.treebuilder(None)?;
        tree_builder.insert(DB_FILE, blob_oid, 0o100644)?;
        let tree_oid = tree_builder.write()?;
        let tree = self.repo.find_tree(tree_oid)?;

        // Create commit
        let sig = self.repo.signature()?; 
        let parents = if let Some(ref c) = parent_commit {
            vec![c]
        } else {
            vec![]
        };

        self.repo.commit(
            Some(DB_BRANCH),
            &sig,
            &sig,
            "Update reviews.jsonl",
            &tree,
            &parents,
        )?;

        Ok(())
    }
}
