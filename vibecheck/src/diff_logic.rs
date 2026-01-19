use anyhow::{Context, Result};
use git2::{DiffOptions, Repository};
use serde::Serialize;
use std::collections::HashMap;
use std::cell::RefCell;
use std::rc::Rc;
use crate::store::{GitRefStore, ReviewStore};
use crate::hashing::compute_fingerprint;

#[derive(Serialize)]
pub struct Change {
    pub fingerprint: String,
    pub file: String,
    pub line: u32,
    pub diff_content: String, // The +/- diff
    pub new_content: String,  // The clean new content (for editing/preview)
    pub context: String,
    pub status: String, 
}

pub fn get_unreviewed_changes() -> Result<Vec<Change>> {
    // 1. Load DB
    let store = GitRefStore::new()?;
    let history = store.read_history()?;
    
    // Build lookup map: (fingerprint, check) -> verdict
    let mut review_state: HashMap<String, String> = HashMap::new();
    
    // Sort by timestamp asc so we replay history
    let mut sorted_history = history.clone();
    sorted_history.sort_by_key(|r| r.timestamp);
    
    for record in sorted_history {
        if record.check == "review" {
            review_state.insert(record.fingerprint, record.verdict);
        }
    }

    // 2. Compute Diff
    let repo = Repository::discover(".")?;
    
    // Target: diff main..HEAD
    let head = repo.head()?.peel_to_tree()?;
    
    let main_branch = repo.find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .context("Could not find main or master branch")?;
    let main_tree = main_branch.get().peel_to_tree()?;
    
    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(3); // Standard 3 lines context
    
    let diff = repo.diff_tree_to_tree(Some(&main_tree), Some(&head), Some(&mut diff_opts))?;
    
    let mut unreviewed_changes = Vec::new();

    // Structure to hold build-in-progress hunk
    struct ChangeBuilder {
        header: String,
        lines: Vec<String>,
        old_start: u32,
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
            let path = delta.new_file().path().unwrap().to_string_lossy().to_string();
            changes_found_hunk.borrow_mut().push(ChangeBuilder {
                header: String::from_utf8_lossy(hunk.header()).to_string(),
                lines: Vec::new(),
                old_start: hunk.old_start(),
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
        // Reconstruct body and context
        let mut diff_content = String::new();
        let mut new_content = String::new();
        let mut context = String::new();
        
        // For hashing, we still use the diff body (lines starting with + or -)
        let mut hash_body = String::new();

        for line in &builder.lines {
            if line.starts_with(' ') {
                context.push_str(line);
            } else if line.starts_with('+') {
                diff_content.push_str(line);
                hash_body.push_str(line);
                // Extract content without '+'
                new_content.push_str(&line[1..]);
            } else if line.starts_with('-') {
                diff_content.push_str(line);
                hash_body.push_str(line);
            }
        }
        
        let fp = compute_fingerprint(&hash_body, &context);
        let fp_str = fp.as_string();
        
        // Check status
        let verdict = review_state.get(&fp_str).map(|s| s.as_str()).unwrap_or("unreviewed");
        
        if verdict != "approved" {
            unreviewed_changes.push(Change {
                fingerprint: fp_str,
                file: builder.file_path.clone(),
                line: builder.new_start,
                diff_content,
                new_content,
                context: context,
                status: verdict.to_string(),
            });
        }
    }
    
    Ok(unreviewed_changes)
}
