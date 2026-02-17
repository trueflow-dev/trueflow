use crate::commands::review::{ReviewOptions, ReviewTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewScope {
    All,
    MainDiff,
    Commit { id: String, summary: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffQuery {
    MainDiff { path: String },
    Revision { revision: String, path: String },
}

pub fn diff_query_for_scope(scope: &ReviewScope, path: &str) -> DiffQuery {
    match scope {
        ReviewScope::All | ReviewScope::MainDiff => DiffQuery::MainDiff {
            path: path.to_string(),
        },
        ReviewScope::Commit { id, .. } => DiffQuery::Revision {
            revision: id.clone(),
            path: path.to_string(),
        },
    }
}

impl ReviewScope {
    pub fn label(&self) -> String {
        match self {
            ReviewScope::All => "entire review".to_string(),
            ReviewScope::MainDiff => "diff vs main".to_string(),
            ReviewScope::Commit { id, summary } => {
                let short_id = short_commit_id(id);
                let summary = truncate_text(summary, 32);
                if summary.is_empty() {
                    format!("commit {short_id}")
                } else {
                    format!("commit {short_id} {summary}")
                }
            }
        }
    }

    pub fn to_review_options(&self) -> ReviewOptions {
        match self {
            ReviewScope::All => ReviewOptions {
                all: true,
                targets: vec![ReviewTarget::All],
                only: Vec::new(),
                exclude: Vec::new(),
            },
            ReviewScope::MainDiff => ReviewOptions {
                all: false,
                targets: vec![ReviewTarget::MainDiff],
                only: Vec::new(),
                exclude: Vec::new(),
            },
            ReviewScope::Commit { id, .. } => ReviewOptions {
                all: false,
                targets: vec![ReviewTarget::Revision(id.clone())],
                only: Vec::new(),
                exclude: Vec::new(),
            },
        }
    }
}

fn short_commit_id(id: &str) -> String {
    id.chars().take(7).collect()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if max_chars == 0 || trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let cutoff = max_chars.saturating_sub(3).max(1);
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= cutoff {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_query_for_main_scope_uses_main_diff() {
        let query = diff_query_for_scope(&ReviewScope::MainDiff, "src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_for_commit_scope_uses_revision() {
        let query = diff_query_for_scope(
            &ReviewScope::Commit {
                id: "abc1234".to_string(),
                summary: "message".to_string(),
            },
            "src/lib.rs",
        );
        assert_eq!(
            query,
            DiffQuery::Revision {
                revision: "abc1234".to_string(),
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn commit_scope_label_uses_short_id_and_truncated_summary() {
        let scope = ReviewScope::Commit {
            id: "1234567890abcdef".to_string(),
            summary: "this is a long summary that should be truncated for display".to_string(),
        };
        assert!(scope.label().starts_with("commit 1234567"));
        assert!(scope.label().ends_with("..."));
    }

    #[test]
    fn main_diff_scope_maps_to_expected_review_options() {
        let options = ReviewScope::MainDiff.to_review_options();
        assert!(!options.all);
        assert_eq!(options.targets, vec![ReviewTarget::MainDiff]);
        assert!(options.only.is_empty());
        assert!(options.exclude.is_empty());
    }
}
