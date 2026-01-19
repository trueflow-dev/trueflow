use anyhow::Result;
use crate::store::{GitRefStore, Record, ReviewStore};
use uuid::Uuid;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(
    fingerprint: &str, 
    verdict: &str, 
    check: &str, 
    note: Option<&str>,
    path_hint: Option<&str>,
    line_hint: Option<u32>
) -> Result<()> {
    let store = GitRefStore::new()?;
    
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    
    // TODO: Get actual author from git config.
    // For now, using a placeholder or environment variable would be better.
    let author = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    let record = Record {
        id: Uuid::new_v4().to_string(),
        fingerprint: fingerprint.to_string(),
        check: check.to_string(),
        verdict: verdict.to_string(),
        author,
        timestamp: now,
        path_hint: path_hint.map(|s| s.to_string()),
        line_hint,
        note: note.map(|s| s.to_string()),
        tags: None,
    };
    
    store.append(record)?;
    println!("Recorded verdict '{}' for {}", verdict, fingerprint);
    Ok(())
}
