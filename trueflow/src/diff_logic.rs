use crate::hashing::compute_fingerprint;
use crate::store::{
    FileStore, Record, ReviewStore, Verdict, approved_hashes_from_verdicts, latest_review_verdicts,
};
use crate::tree;
use crate::vcs;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize)]
pub struct Change {
    pub fingerprint: String,
    pub file: String,
    pub line: u32,
    pub diff_content: String, // The +/- diff
    pub new_content: String,  // The clean new content (for editing/preview)
    pub context: String,
    pub status: String,
    pub reviews: Vec<Record>,
}

pub fn get_unreviewed_changes() -> Result<Vec<Change>> {
    // 1. Load DB
    let store = FileStore::new()?;
    let history = store.read_history()?;

    // Build lookup map: (fingerprint, check) -> verdict
    // We also store the full history for the fingerprint to enable queries
    let review_state = latest_review_verdicts(&history);
    let mut reviews_by_fp: HashMap<String, Vec<Record>> = HashMap::new();

    for record in history {
        reviews_by_fp
            .entry(record.fingerprint.clone())
            .or_default()
            .push(record);
    }

    let approved_hashes = approved_hashes_from_verdicts(&review_state);
    let tree = tree::build_tree_from_path(".")?;

    // 2. Compute Diff
    let diff_hunks = vcs::diff_main_to_head()?;

    let mut unreviewed_changes = Vec::new();

    for hunk in diff_hunks {
        let (diff_content, new_content, context, hash_body) = parse_hunk_lines(&hunk.lines);

        let fp = compute_fingerprint(&hash_body, &context);
        let fp_str = fp.as_string();

        // Check status
        let verdict = review_state.get(&fp_str);
        let status = verdict.map(|v| v.as_str()).unwrap_or("unreviewed");

        // Get all reviews for this hunk
        let reviews = reviews_by_fp.get(&fp_str).cloned().unwrap_or_default();

        if tree
            .find_by_path(&hunk.file_path)
            .is_some_and(|node_id| tree.is_node_covered(node_id, &approved_hashes))
        {
            continue;
        }

        if verdict != Some(&Verdict::Approved) {
            unreviewed_changes.push(Change {
                fingerprint: fp_str,
                file: hunk.file_path.clone(),
                line: hunk.new_start,
                diff_content,
                new_content,
                context,
                status: status.to_string(),
                reviews,
            });
        }
    }

    Ok(unreviewed_changes)
}

fn parse_hunk_lines(lines: &[String]) -> (String, String, String, String) {
    let mut diff_content = String::new();
    let mut new_content = String::new();
    let mut context = String::new();
    let mut hash_body = String::new();

    for line in lines {
        if let Some(stripped) = line.strip_prefix(' ') {
            context.push_str(line);
            new_content.push_str(stripped);
        } else if let Some(stripped) = line.strip_prefix('+') {
            diff_content.push_str(line);
            hash_body.push_str(line);
            new_content.push_str(stripped);
        } else if line.starts_with('-') {
            diff_content.push_str(line);
            hash_body.push_str(line);
        }
    }

    (diff_content, new_content, context, hash_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_hunk() {
        let lines = vec![
            " context 1\n".to_string(),
            "-old\n".to_string(),
            "+new\n".to_string(),
            " context 2\n".to_string(),
        ];

        let (diff, new, ctx, hash) = parse_hunk_lines(&lines);

        assert_eq!(diff, "-old\n+new\n");
        assert_eq!(new, "context 1\nnew\ncontext 2\n");
        assert_eq!(ctx, " context 1\n context 2\n");
        assert_eq!(hash, "-old\n+new\n");
    }

    #[test]
    fn test_hunk_only_additions() {
        let lines = vec!["+add1\n".to_string(), "+add2\n".to_string()];

        let (diff, new, ctx, hash) = parse_hunk_lines(&lines);

        assert_eq!(diff, "+add1\n+add2\n");
        assert_eq!(new, "add1\nadd2\n");
        assert_eq!(ctx, "");
        assert_eq!(hash, "+add1\n+add2\n");
    }

    #[test]
    fn test_hunk_mixed_context() {
        // Ensures context is just concatenated, ignoring position relative to edits
        let lines = vec![
            " pre\n".to_string(),
            "-del\n".to_string(),
            " mid\n".to_string(),
            "+add\n".to_string(),
        ];

        let (_, _, ctx, _) = parse_hunk_lines(&lines);
        assert_eq!(ctx, " pre\n mid\n");
    }
}
