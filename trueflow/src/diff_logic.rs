use crate::hashing::compute_fingerprint;
use crate::store::{FileStore, Record, ReviewStore, Verdict};
use anyhow::{Context, Result};
use git2::{DiffOptions, Repository};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

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
    let mut review_state: HashMap<String, Verdict> = HashMap::new();
    let mut reviews_by_fp: HashMap<String, Vec<Record>> = HashMap::new();

    // Sort by timestamp asc so we replay history
    let mut sorted_history = history.clone();
    sorted_history.sort_by_key(|r| r.timestamp);

    for record in sorted_history {
        if record.check == "review" {
            // Last write wins for simple status
            review_state.insert(record.fingerprint.clone(), record.verdict.clone());
        }
        // Collect all reviews for this fingerprint
        reviews_by_fp
            .entry(record.fingerprint.clone())
            .or_default()
            .push(record);
    }

    // 2. Compute Diff
    let repo = Repository::discover(".")?;

    // Target: diff main..HEAD
    let head_commit = repo.head()?.peel_to_commit()?;
    let head_tree = head_commit.tree()?;

    let main_branch = repo
        .find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .context("Could not find main or master branch")?;
    let main_commit = main_branch.get().peel_to_commit()?;

    let base_tree = match repo.merge_base(head_commit.id(), main_commit.id()) {
        Ok(oid) => repo.find_commit(oid)?.tree()?,
        Err(_) => main_commit.tree()?,
    };

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(3); // Standard 3 lines context

    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut diff_opts))?;

    let mut unreviewed_changes = Vec::new();

    // Structure to hold build-in-progress hunk
    struct ChangeBuilder {
        lines: Vec<String>,
        new_start: u32,
        file_path: String,
    }

    let changes_found: Rc<RefCell<Vec<ChangeBuilder>>> = Rc::new(RefCell::new(Vec::new()));

    // Create clones for closures
    let changes_found_hunk = changes_found.clone();
    let changes_found_line = changes_found.clone();

    diff.foreach(
        &mut |_delta, _progress| {
            // File callback
            true
        },
        None, // binary callback
        Some(&mut |delta, hunk| {
            // New hunk starting
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());

            changes_found_hunk.borrow_mut().push(ChangeBuilder {
                lines: Vec::new(),
                new_start: hunk.new_start(),
                file_path: path,
            });
            true
        }),
        Some(&mut |_delta, _hunk, line| {
            // Line callback
            let mut changes = changes_found_line.borrow_mut();
            if let Some(builder) = changes.last_mut() {
                let origin = line.origin();
                let content = String::from_utf8_lossy(line.content());
                // Prefix with origin char (+, -, space)
                let prefix = match origin {
                    '+' | '-' | ' ' => origin,
                    _ => ' ', // Context often comes as space, sometimes other things?
                };
                builder.lines.push(format!("{}{}", prefix, content));
            }
            true
        }),
    )?;

    // Process gathered hunks
    for builder in changes_found.borrow().iter() {
        let (diff_content, new_content, context, hash_body) = parse_hunk_lines(&builder.lines);

        let fp = compute_fingerprint(&hash_body, &context);
        let fp_str = fp.as_string();

        // Check status
        let verdict = review_state.get(&fp_str);
        let status = verdict.map(|v| v.as_str()).unwrap_or("unreviewed");

        // Get all reviews for this hunk
        let reviews = reviews_by_fp.get(&fp_str).cloned().unwrap_or_default();

        if verdict != Some(&Verdict::Approved) {
            unreviewed_changes.push(Change {
                fingerprint: fp_str,
                file: builder.file_path.clone(),
                line: builder.new_start,
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
