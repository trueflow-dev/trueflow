use crate::commands::review::{ReviewRequest, ReviewTarget, RevisionRangeSpec, RevisionSpec};
use crate::vcs::CommitInfo;
use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOption {
    pub label: String,
    pub scope: ReviewScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewScope {
    All,
    MainDiff,
    Commit { id: String, summary: String },
    RevisionRange { start: String, end: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffQuery {
    MainDiff {
        path: String,
    },
    Revision {
        revision: String,
        path: String,
    },
    RevisionRange {
        start: String,
        end: String,
        path: String,
    },
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
        ReviewScope::RevisionRange { start, end } => DiffQuery::RevisionRange {
            start: start.clone(),
            end: end.clone(),
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
            ReviewScope::RevisionRange { start, end } => {
                format!(
                    "revisions {}..{}",
                    short_commit_id(start),
                    short_commit_id(end)
                )
            }
        }
    }

    pub fn to_review_request(&self) -> Result<ReviewRequest> {
        match self {
            ReviewScope::All => Ok(ReviewRequest::AllFiles),
            ReviewScope::MainDiff => Ok(ReviewRequest::Targets(vec![ReviewTarget::MainDiff])),
            ReviewScope::Commit { id, .. } => {
                Ok(ReviewRequest::Targets(vec![ReviewTarget::Revision(
                    RevisionSpec::new(id.clone())?,
                )]))
            }
            ReviewScope::RevisionRange { start, end } => {
                Ok(ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(
                    RevisionRangeSpec::new(start.clone(), end.clone())?,
                )]))
            }
        }
    }
}

pub fn default_scope_options(commits: &[CommitInfo]) -> Vec<ScopeOption> {
    let mut options = vec![
        ScopeOption {
            label: "All files".to_string(),
            scope: ReviewScope::All,
        },
        ScopeOption {
            label: "Diff vs main".to_string(),
            scope: ReviewScope::MainDiff,
        },
    ];

    for commit in commits {
        options.push(commit_scope_option(commit));
    }

    options
}

fn commit_scope_option(commit: &CommitInfo) -> ScopeOption {
    let short_id = short_commit_id(&commit.id);
    let summary = truncate_text(&commit.summary, 60);
    let label = if summary.is_empty() {
        format!("Commit {short_id}")
    } else {
        format!("Commit {short_id} {summary}")
    };

    ScopeOption {
        label,
        scope: ReviewScope::Commit {
            id: commit.id.clone(),
            summary: commit.summary.clone(),
        },
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
    fn diff_query_for_revision_range_scope_uses_revision_range() {
        let query = diff_query_for_scope(
            &ReviewScope::RevisionRange {
                start: "abc1234".to_string(),
                end: "def5678".to_string(),
            },
            "src/lib.rs",
        );
        assert_eq!(
            query,
            DiffQuery::RevisionRange {
                start: "abc1234".to_string(),
                end: "def5678".to_string(),
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
    fn revision_range_scope_label_uses_short_ids() {
        let scope = ReviewScope::RevisionRange {
            start: "1234567890abcdef".to_string(),
            end: "abcdef1234567890".to_string(),
        };
        assert_eq!(scope.label(), "revisions 1234567..abcdef1");
    }

    #[test]
    fn all_scope_maps_to_all_review_request() {
        let request = ReviewScope::All
            .to_review_request()
            .unwrap_or_else(|error| panic!("expected all review request: {error}"));
        assert_eq!(request, ReviewRequest::AllFiles);
    }

    #[test]
    fn main_diff_scope_maps_to_expected_review_request() {
        let request = ReviewScope::MainDiff
            .to_review_request()
            .unwrap_or_else(|error| panic!("expected main diff review request: {error}"));
        assert_eq!(
            request,
            ReviewRequest::Targets(vec![ReviewTarget::MainDiff])
        );
    }

    #[test]
    fn revision_range_scope_maps_to_expected_review_request() {
        let request = ReviewScope::RevisionRange {
            start: "abc1234".to_string(),
            end: "def5678".to_string(),
        }
        .to_review_request()
        .unwrap_or_else(|error| panic!("expected revision range review request: {error}"));
        assert_eq!(
            request,
            ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(
                RevisionRangeSpec::new("abc1234", "def5678").unwrap(),
            )])
        );
    }

    #[test]
    fn default_scope_options_include_base_scopes_and_commits() {
        let commits = vec![
            CommitInfo {
                id: "abcdef123456".to_string(),
                summary: "first summary".to_string(),
            },
            CommitInfo {
                id: "1234567890ab".to_string(),
                summary: "second summary".to_string(),
            },
        ];

        let options = default_scope_options(&commits);
        assert_eq!(options.len(), 4);
        assert_eq!(options[0].label, "All files");
        assert_eq!(options[1].label, "Diff vs main");
        assert!(options[2].label.starts_with("Commit abcdef1 first summary"));
        assert!(
            options[3]
                .label
                .starts_with("Commit 1234567 second summary")
        );
    }

    #[test]
    fn default_scope_options_truncate_long_commit_summary() {
        let commits = vec![CommitInfo {
            id: "abcdef123456".to_string(),
            summary:
                "this is a very long commit summary that should be truncated in selector labels"
                    .to_string(),
        }];

        let options = default_scope_options(&commits);
        assert!(options[2].label.ends_with("..."));
    }
}
