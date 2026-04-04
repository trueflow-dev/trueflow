use crate::hashing::compute_fingerprint;
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::store::{
    DiffFingerprint, FileStore, Record, ReviewCheck, ReviewStore, ReviewTargetRef, Verdict,
};
use crate::tree;
use crate::vcs;
use anyhow::Result;
use serde::Serialize;

#[derive(Serialize)]
pub struct Change {
    pub fingerprint: String,
    pub file: RepoPath,
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
    let database = store.load_database()?;
    let review_index = database.latest_index(Some(&ReviewCheck::review()));
    let reviews_by_target = database.records_by_target_since(None);
    let approved_targets = review_index.approved_targets();
    let tree = tree::build_tree_from_path(".")?;
    let workdir_prefix = workdir_prefix_from_git_root();

    // 2. Compute Diff
    let file_diffs = vcs::diff_main_to_head_files()?;

    let mut unreviewed_changes = Vec::new();

    for file_diff in file_diffs {
        let vcs::FileDiff::Text { path, hunks } = file_diff else {
            continue;
        };

        if path_is_covered_by_approved_node(
            &tree,
            &approved_targets,
            &path,
            workdir_prefix.as_deref(),
        ) {
            continue;
        }

        for hunk in hunks {
            let (diff_content, new_content, context, hash_body) = parse_hunk_lines(&hunk.lines);

            let fp = DiffFingerprint::from_computed(
                compute_fingerprint(&hash_body, &context).as_string(),
            );
            let fp_str = fp.as_str().to_string();
            let diff_target = ReviewTargetRef::Diff {
                fingerprint: fp.clone(),
            };

            let verdict = review_index.verdict_for(&diff_target);
            let status = verdict.map(|v| v.as_str()).unwrap_or("unreviewed");

            let reviews = reviews_by_target
                .get(&diff_target)
                .cloned()
                .unwrap_or_default();

            if verdict != Some(&Verdict::Approved) {
                unreviewed_changes.push(Change {
                    fingerprint: fp_str,
                    file: path.clone(),
                    line: hunk.new_start,
                    diff_content,
                    new_content,
                    context,
                    status: status.to_string(),
                    reviews,
                });
            }
        }
    }

    Ok(unreviewed_changes)
}

fn path_is_covered_by_approved_node(
    tree: &tree::Tree,
    approved_targets: &crate::store::ApprovedTargets,
    repo_relative_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> bool {
    let candidates =
        path_utils::tree_path_candidates_for_repo_path(repo_relative_path.as_str(), workdir_prefix);
    for candidate in candidates {
        if tree
            .find_by_path(candidate.as_str())
            .is_some_and(|node_id| tree.is_node_covered(node_id, approved_targets, workdir_prefix))
        {
            tracing::debug!(
                repo_relative_path = %repo_relative_path,
                tree_path = %candidate,
                "diff path coverage matched approved node"
            );
            return true;
        }
    }
    tracing::debug!(
        repo_relative_path = %repo_relative_path,
        workdir_prefix = ?workdir_prefix,
        "diff path coverage found no approved node"
    );
    false
}

fn workdir_prefix_from_git_root() -> Option<String> {
    let repo_root = vcs::git_root_from_workdir().ok().flatten()?;
    path_utils::current_workdir_prefix_for_repo_root(&repo_root)
}

fn parse_hunk_lines(lines: &[vcs::DiffHunkLine]) -> (String, String, String, String) {
    let mut diff_content = String::new();
    let mut new_content = String::new();
    let mut context = String::new();
    let mut hash_body = String::new();

    for line in lines {
        match line.kind {
            vcs::DiffLineKind::Context => {
                context.push_str(&line.as_unified_line());
                new_content.push_str(&line.text);
            }
            vcs::DiffLineKind::Added => {
                let unified = line.as_unified_line();
                diff_content.push_str(&unified);
                hash_body.push_str(&unified);
                new_content.push_str(&line.text);
            }
            vcs::DiffLineKind::Removed => {
                let unified = line.as_unified_line();
                diff_content.push_str(&unified);
                hash_body.push_str(&unified);
            }
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
            vcs::DiffHunkLine::context("context 1\n"),
            vcs::DiffHunkLine::removed("old\n"),
            vcs::DiffHunkLine::added("new\n"),
            vcs::DiffHunkLine::context("context 2\n"),
        ];

        let (diff, new, ctx, hash) = parse_hunk_lines(&lines);

        assert_eq!(diff, "-old\n+new\n");
        assert_eq!(new, "context 1\nnew\ncontext 2\n");
        assert_eq!(ctx, " context 1\n context 2\n");
        assert_eq!(hash, "-old\n+new\n");
    }

    #[test]
    fn test_hunk_only_additions() {
        let lines = vec![
            vcs::DiffHunkLine::added("add1\n"),
            vcs::DiffHunkLine::added("add2\n"),
        ];

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
            vcs::DiffHunkLine::context("pre\n"),
            vcs::DiffHunkLine::removed("del\n"),
            vcs::DiffHunkLine::context("mid\n"),
            vcs::DiffHunkLine::added("add\n"),
        ];

        let (_, _, ctx, _) = parse_hunk_lines(&lines);
        assert_eq!(ctx, " pre\n mid\n");
    }
}
