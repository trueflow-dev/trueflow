use crate::commands::review::{ReviewRequest, ReviewTarget, RevisionExpr, RevisionRangeExpr};
use crate::repo_path::RepoPath;
use crate::vcs::CommitInfo;
use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOption {
    pub label: String,
    pub scope: ReviewScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliSemanticReviewScope {
    All,
    DirtyWorktree,
    MainDiff,
    File(RepoPath),
    Dir(RepoPath),
    Revision(RevisionExpr),
    RevisionRange(RevisionRangeExpr),
    MultiTarget(Vec<ReviewTarget>),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiffSelection {
    MainDiff,
    Revision { revision: String },
    RevisionRange { start: String, end: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewContentSelection {
    EntireReview,
    DiffOnly(ReviewDiffSelection),
}

impl CliSemanticReviewScope {
    pub fn from_cli(all: bool, targets: &[ReviewTarget]) -> Result<Self> {
        if all {
            if !targets.is_empty() {
                return Err(anyhow!(
                    "Explicit review targets cannot be combined with --all"
                ));
            }
            return Ok(Self::All);
        }

        Ok(match targets {
            [] => Self::DirtyWorktree,
            [ReviewTarget::DirtyWorktree] => Self::DirtyWorktree,
            [ReviewTarget::MainDiff] => Self::MainDiff,
            [ReviewTarget::File(path)] => Self::File(path.clone()),
            [ReviewTarget::Dir(path)] => Self::Dir(path.clone()),
            [ReviewTarget::Revision(revision)] => Self::Revision(revision.clone()),
            [ReviewTarget::RevisionRange(range)] => Self::RevisionRange(range.clone()),
            _ => Self::MultiTarget(targets.to_vec()),
        })
    }

    pub fn review_request(&self) -> ReviewRequest {
        match self {
            Self::All => ReviewRequest::AllFiles,
            Self::DirtyWorktree => ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree]),
            Self::MainDiff => ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
            Self::File(path) => ReviewRequest::Targets(vec![ReviewTarget::File(path.clone())]),
            Self::Dir(path) => ReviewRequest::Targets(vec![ReviewTarget::Dir(path.clone())]),
            Self::Revision(revision) => {
                ReviewRequest::Targets(vec![ReviewTarget::Revision(revision.clone())])
            }
            Self::RevisionRange(range) => {
                ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(range.clone())])
            }
            Self::MultiTarget(targets) => ReviewRequest::Targets(targets.clone()),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::All => "all files (CLI)".to_string(),
            Self::DirtyWorktree => "dirty worktree".to_string(),
            Self::MainDiff => "diff vs main".to_string(),
            Self::File(path) => format!("file {path}"),
            Self::Dir(path) => format!("dir:{path}"),
            Self::Revision(revision) => format!("revision {revision}"),
            Self::RevisionRange(range) => {
                format!("revisions {}..{}", range.start, range.end)
            }
            Self::MultiTarget(targets) => format!("{} targets", targets.len()),
        }
    }

    pub fn tui_scope(&self) -> ReviewScope {
        match self {
            Self::All => ReviewScope::All,
            Self::Revision(revision) => ReviewScope::Commit {
                id: revision.as_str().to_string(),
                summary: String::new(),
            },
            Self::RevisionRange(range) => ReviewScope::RevisionRange {
                start: range.start.as_str().to_string(),
                end: range.end.as_str().to_string(),
            },
            // Workdir-scoped explicit path selections still render through the
            // main-diff TUI view; the CLI scope carries the path restriction.
            Self::DirtyWorktree
            | Self::MainDiff
            | Self::File(_)
            | Self::Dir(_)
            | Self::MultiTarget(_) => ReviewScope::MainDiff,
        }
    }
}

impl ReviewScope {
    pub fn diff_selection(&self) -> ReviewDiffSelection {
        match self {
            ReviewScope::All | ReviewScope::MainDiff => ReviewDiffSelection::MainDiff,
            ReviewScope::Commit { id, .. } => ReviewDiffSelection::Revision {
                revision: id.clone(),
            },
            ReviewScope::RevisionRange { start, end } => ReviewDiffSelection::RevisionRange {
                start: start.clone(),
                end: end.clone(),
            },
        }
    }

    pub fn content_selection(&self) -> ReviewContentSelection {
        match self {
            ReviewScope::All => ReviewContentSelection::EntireReview,
            ReviewScope::MainDiff
            | ReviewScope::Commit { .. }
            | ReviewScope::RevisionRange { .. } => {
                ReviewContentSelection::DiffOnly(self.diff_selection())
            }
        }
    }

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
        self.content_selection().to_review_request()
    }
}

impl ReviewDiffSelection {
    pub fn query_for_path(&self, path: &str) -> DiffQuery {
        match self {
            ReviewDiffSelection::MainDiff => DiffQuery::MainDiff {
                path: path.to_string(),
            },
            ReviewDiffSelection::Revision { revision } => DiffQuery::Revision {
                revision: revision.clone(),
                path: path.to_string(),
            },
            ReviewDiffSelection::RevisionRange { start, end } => DiffQuery::RevisionRange {
                start: start.clone(),
                end: end.clone(),
                path: path.to_string(),
            },
        }
    }
}

impl ReviewContentSelection {
    pub fn to_review_request(&self) -> Result<ReviewRequest> {
        match self {
            ReviewContentSelection::EntireReview => Ok(ReviewRequest::AllFiles),
            ReviewContentSelection::DiffOnly(diff_selection) => Ok(ReviewRequest::Targets(vec![
                diff_selection.to_review_target()?,
            ])),
        }
    }
}

impl ReviewDiffSelection {
    fn to_review_target(&self) -> Result<ReviewTarget> {
        match self {
            ReviewDiffSelection::MainDiff => Ok(ReviewTarget::MainDiff),
            ReviewDiffSelection::Revision { revision } => {
                Ok(ReviewTarget::Revision(RevisionExpr::new(revision.clone())?))
            }
            ReviewDiffSelection::RevisionRange { start, end } => Ok(ReviewTarget::RevisionRange(
                RevisionRangeExpr::new(start.clone(), end.clone())?,
            )),
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
    let short_id = short_commit_id(commit.id.as_str());
    let summary = truncate_text(&commit.summary, 60);
    let label = if summary.is_empty() {
        format!("Commit {short_id}")
    } else {
        format!("Commit {short_id} {summary}")
    };

    ScopeOption {
        label,
        scope: ReviewScope::Commit {
            id: commit.id.to_string(),
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
    use crate::store::CommitId;

    #[test]
    fn cli_semantic_review_scope_defaults_to_dirty_worktree() {
        let scope = CliSemanticReviewScope::from_cli(false, &[])
            .unwrap_or_else(|error| panic!("expected default cli scope: {error}"));
        assert_eq!(scope, CliSemanticReviewScope::DirtyWorktree);
        assert_eq!(
            scope.review_request(),
            ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree])
        );
        assert_eq!(scope.label(), "dirty worktree");
        assert_eq!(scope.tui_scope(), ReviewScope::MainDiff);
    }

    #[test]
    fn cli_semantic_review_scope_preserves_single_file_target() {
        let scope = CliSemanticReviewScope::from_cli(
            false,
            &[ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap())],
        )
        .unwrap_or_else(|error| panic!("expected file cli scope: {error}"));
        assert_eq!(
            scope,
            CliSemanticReviewScope::File(RepoPath::new("src/lib.rs").unwrap())
        );
        assert_eq!(scope.label(), "file src/lib.rs");
        assert_eq!(scope.tui_scope(), ReviewScope::MainDiff);
    }

    #[test]
    fn cli_semantic_review_scope_preserves_single_dir_target() {
        let scope = CliSemanticReviewScope::from_cli(
            false,
            &[ReviewTarget::Dir(RepoPath::new("src/nested").unwrap())],
        )
        .unwrap_or_else(|error| panic!("expected dir cli scope: {error}"));
        assert_eq!(
            scope,
            CliSemanticReviewScope::Dir(RepoPath::new("src/nested").unwrap())
        );
        assert_eq!(scope.label(), "dir:src/nested");
        assert_eq!(scope.tui_scope(), ReviewScope::MainDiff);
    }

    #[test]
    fn cli_semantic_review_scope_preserves_revision_range_target() {
        let scope = CliSemanticReviewScope::from_cli(
            false,
            &[ReviewTarget::RevisionRange(
                RevisionRangeExpr::new("abc1234", "def5678").unwrap(),
            )],
        )
        .unwrap_or_else(|error| panic!("expected revision range cli scope: {error}"));
        assert_eq!(
            scope,
            CliSemanticReviewScope::RevisionRange(
                RevisionRangeExpr::new("abc1234", "def5678").unwrap(),
            )
        );
        assert_eq!(scope.label(), "revisions abc1234..def5678");
        assert_eq!(
            scope.tui_scope(),
            ReviewScope::RevisionRange {
                start: "abc1234".to_string(),
                end: "def5678".to_string(),
            }
        );
    }

    #[test]
    fn cli_semantic_review_scope_tracks_multiple_targets_without_downgrading_request() {
        let targets = vec![
            ReviewTarget::MainDiff,
            ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
        ];
        let scope = CliSemanticReviewScope::from_cli(false, &targets)
            .unwrap_or_else(|error| panic!("expected multi-target cli scope: {error}"));
        assert_eq!(scope, CliSemanticReviewScope::MultiTarget(targets.clone()));
        assert_eq!(scope.review_request(), ReviewRequest::Targets(targets));
        assert_eq!(scope.label(), "2 targets");
    }

    #[test]
    fn cli_semantic_review_scope_rejects_all_with_explicit_targets() {
        let error = CliSemanticReviewScope::from_cli(true, &[ReviewTarget::MainDiff]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Explicit review targets cannot be combined with --all")
        );
    }

    #[test]
    fn all_scope_uses_entire_review_content_and_main_diff_selection() {
        let scope = ReviewScope::All;
        assert_eq!(
            scope.content_selection(),
            ReviewContentSelection::EntireReview
        );
        assert_eq!(scope.diff_selection(), ReviewDiffSelection::MainDiff);
    }

    #[test]
    fn main_diff_scope_uses_diff_only_content_and_main_diff_selection() {
        let scope = ReviewScope::MainDiff;
        assert_eq!(
            scope.content_selection(),
            ReviewContentSelection::DiffOnly(ReviewDiffSelection::MainDiff)
        );
        assert_eq!(scope.diff_selection(), ReviewDiffSelection::MainDiff);
    }

    #[test]
    fn diff_query_for_main_scope_uses_main_diff() {
        let query = ReviewScope::MainDiff
            .diff_selection()
            .query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn commit_scope_uses_diff_only_content_and_revision_selection() {
        let scope = ReviewScope::Commit {
            id: "abc1234".to_string(),
            summary: "message".to_string(),
        };
        assert_eq!(
            scope.content_selection(),
            ReviewContentSelection::DiffOnly(ReviewDiffSelection::Revision {
                revision: "abc1234".to_string(),
            })
        );
        assert_eq!(
            scope.diff_selection(),
            ReviewDiffSelection::Revision {
                revision: "abc1234".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_for_commit_scope_uses_revision() {
        let query = ReviewScope::Commit {
            id: "abc1234".to_string(),
            summary: "message".to_string(),
        }
        .diff_selection()
        .query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::Revision {
                revision: "abc1234".to_string(),
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn revision_range_scope_uses_diff_only_content_and_revision_range_selection() {
        let scope = ReviewScope::RevisionRange {
            start: "abc1234".to_string(),
            end: "def5678".to_string(),
        };
        assert_eq!(
            scope.content_selection(),
            ReviewContentSelection::DiffOnly(ReviewDiffSelection::RevisionRange {
                start: "abc1234".to_string(),
                end: "def5678".to_string(),
            })
        );
        assert_eq!(
            scope.diff_selection(),
            ReviewDiffSelection::RevisionRange {
                start: "abc1234".to_string(),
                end: "def5678".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_for_revision_range_scope_uses_revision_range() {
        let query = ReviewScope::RevisionRange {
            start: "abc1234".to_string(),
            end: "def5678".to_string(),
        }
        .diff_selection()
        .query_for_path("src/lib.rs");
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
                RevisionRangeExpr::new("abc1234", "def5678").unwrap(),
            )])
        );
    }

    #[test]
    fn default_scope_options_include_base_scopes_and_commits() {
        let commits = vec![
            CommitInfo {
                id: CommitId::new("abcdef123456").unwrap(),
                summary: "first summary".to_string(),
            },
            CommitInfo {
                id: CommitId::new("1234567890ab").unwrap(),
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
            id: CommitId::new("abcdef123456").unwrap(),
            summary:
                "this is a very long commit summary that should be truncated in selector labels"
                    .to_string(),
        }];

        let options = default_scope_options(&commits);
        assert!(options[2].label.ends_with("..."));
    }
}
