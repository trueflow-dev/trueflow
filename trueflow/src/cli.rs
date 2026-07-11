use clap::{ArgGroup, Parser, Subcommand};

use crate::block::BlockKind;
use crate::build_info;
use crate::commands::feedback::FeedbackFormat;
use crate::commands::review::ReviewTarget;
use crate::feedback_since::FeedbackSinceExpr;
use crate::github::PullRequestRef;
use crate::logging::LoggingMode;
use crate::repo_path::RepoPath;
use crate::store::{ReviewCheck, Verdict};

#[derive(Parser)]
#[command(name = "trueflow")]
#[command(version = build_info::VERSION, long_version = build_info::LONG_VERSION)]
#[command(about = "Semantic review for the agent era", long_about = None)]
#[command(after_help = build_info::HELP_FOOTER)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable verbose debug logging
    #[arg(long)]
    pub debug: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = LoggingMode::File,
        hide = true
    )]
    pub logging_mode: LoggingMode,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Mark a review target with a verdict
    Mark {
        /// Current CLI field name for the review-target identifier
        /// (typically a content-addressed semantic review hash)
        #[arg(long)]
        fingerprint: String,

        /// Verdict: approved, rejected, comment
        #[arg(long, default_value_t = Verdict::Approved)]
        verdict: Verdict,

        /// Check type: review, security, style, etc.
        #[arg(long, default_value_t = ReviewCheck::review())]
        check: ReviewCheck,

        /// Optional note
        #[arg(long)]
        note: Option<String>,

        /// Path hint for debugging/UI
        #[arg(long)]
        path: Option<RepoPath>,

        /// Line number hint
        #[arg(long)]
        line: Option<u32>,

        /// Internal scoped comment line span start (0-indexed, inclusive)
        #[arg(long, hide = true)]
        comment_scope_start: Option<u32>,

        /// Internal scoped comment line span end (0-indexed, exclusive)
        #[arg(long, hide = true)]
        comment_scope_end: Option<u32>,

        /// Internal scoped comment context
        #[arg(long, hide = true)]
        comment_context: Option<String>,

        /// Internal serialized comment anchor JSON
        #[arg(long, hide = true)]
        comment_anchor_json: Option<String>,

        /// Suppress output for UI usage
        #[arg(long)]
        quiet: bool,
    },
    /// CI gate check
    Check,
    /// Scan the directory and build the Merkle tree (Audit mode)
    Scan {
        /// Output JSON
        #[arg(long)]
        json: bool,

        /// Output the full Merkle tree
        #[arg(long, requires = "json")]
        tree: bool,
    },
    /// Interactive review of unreviewed blocks
    Review {
        /// Output format (default is text, use --json for machine parsing)
        #[arg(long)]
        json: bool,

        /// Review everything (Audit mode), ignoring git status
        #[arg(long)]
        all: bool,

        /// Review targets such as `dirty`, `main`, `file:src/lib.rs`, `dir:src`, `rev:abc1234..def5678`, or `pr:11`
        #[arg(long, value_name = "TARGET")]
        target: Vec<ReviewTarget>,

        /// Review committed changes since this commit (equivalent to `rev:COMMIT..HEAD`)
        #[arg(long, value_name = "COMMIT", conflicts_with = "all")]
        since: Option<String>,

        /// Only include block types (e.g. "function", "struct")
        #[arg(long)]
        only: Vec<BlockKind>,

        /// Exclude block types (e.g. "gap", "comment", "whitespace")
        #[arg(long)]
        exclude: Vec<BlockKind>,
    },
    /// Export feedback for LLM/Agent consumption
    Feedback {
        /// Output format (xml or json)
        #[arg(long, value_enum, default_value_t = FeedbackFormat::Xml)]
        format: FeedbackFormat,

        /// Only include records since this point ("all", "last", relative durations like "1h", unix ts, or RFC3339)
        #[arg(long)]
        since: Option<FeedbackSinceExpr>,

        /// Post feedback to a pull request such as `pr:11` or `https://github.com/owner/repo/pull/11`
        #[arg(long, value_name = "PR", conflicts_with = "target")]
        pr: Option<PullRequestRef>,

        /// Print the pull request review plan without posting it. Requires `--pr`.
        #[arg(long, requires = "pr")]
        dry_run: bool,

        /// Open the posted or submitted review in a browser. Requires `--pr` and conflicts with `--dry-run`.
        #[arg(long, requires = "pr", conflicts_with = "dry_run")]
        open: bool,

        /// Submit the current trueflow-owned pending review as a COMMENT review. Requires `--pr`.
        #[arg(long, requires = "pr")]
        submit: bool,

        /// Feedback export targets such as `dirty`, `main`, `file:src/lib.rs`, `dir:src`, or `rev:abc1234..def5678`; use `--pr` for pull request posting
        #[arg(long, value_name = "TARGET")]
        target: Vec<ReviewTarget>,

        /// Include approved blocks (for few-shot examples)
        #[arg(long)]
        include_approved: bool,

        /// Only include block types
        #[arg(long)]
        only: Vec<BlockKind>,

        /// Exclude block types
        #[arg(long)]
        exclude: Vec<BlockKind>,
    },
    /// Inspect a block review target (and optionally split it)
    Inspect {
        /// Current CLI field name for the block identifier
        /// (content-addressed block hash)
        #[arg(long)]
        fingerprint: String,

        /// Split into sub-blocks
        #[arg(long)]
        split: bool,

        /// Include review coverage details
        #[arg(long)]
        coverage: bool,
    },
    /// Verify record attestations
    #[command(group(
        ArgGroup::new("verify_selection")
            .required(true)
            .multiple(false)
            .args(["all", "id"])
    ))]
    Verify {
        /// Verify all records
        #[arg(long)]
        all: bool,

        /// Verify a specific record id
        #[arg(long)]
        id: Option<String>,
    },
    /// Launch the TUI
    Tui {
        /// Review everything (Audit mode), ignoring git status
        #[arg(long)]
        all: bool,

        /// Review targets such as `dirty`, `main`, `file:src/lib.rs`, `dir:src`, `rev:abc1234..def5678`, or `pr:11`
        #[arg(long, value_name = "TARGET")]
        target: Vec<ReviewTarget>,

        /// Review committed changes since this commit (equivalent to `rev:COMMIT..HEAD`)
        #[arg(long, value_name = "COMMIT", conflicts_with = "all")]
        since: Option<String>,

        /// Only include block types (e.g. "function", "struct")
        #[arg(long)]
        only: Vec<BlockKind>,

        /// Exclude block types (e.g. "gap", "comment", "whitespace")
        #[arg(long)]
        exclude: Vec<BlockKind>,
    },
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands};
    use crate::block::BlockKind;
    use crate::build_info;
    use crate::commands::feedback::FeedbackFormat;
    use crate::commands::review::{ReviewTarget, RevisionExpr};
    use crate::feedback_since::FeedbackSinceExpr;
    use crate::github::PullRequestRef;
    use crate::repo_path::RepoPath;
    use crate::store::CommentAnchor;
    use crate::store::{ReviewCheck, Verdict};
    use clap::{CommandFactory, Parser};

    #[test]
    fn long_help_omits_removed_diff_and_sync_commands() {
        let mut command = Cli::command();
        let mut help = Vec::new();
        command
            .write_long_help(&mut help)
            .unwrap_or_else(|error| panic!("failed to render help output: {error}"));
        let help = String::from_utf8(help)
            .unwrap_or_else(|error| panic!("help output was not utf8: {error}"));
        assert!(!help.contains("\n  diff"));
        assert!(!help.contains("\n  sync"));
    }

    #[test]
    fn long_help_includes_version_only_footer() {
        let mut command = Cli::command();
        let mut help = Vec::new();
        command
            .write_long_help(&mut help)
            .unwrap_or_else(|error| panic!("failed to render help output: {error}"));
        let help = String::from_utf8(help)
            .unwrap_or_else(|error| panic!("help output was not utf8: {error}"));

        assert!(help.contains(build_info::HELP_FOOTER));
        assert!(!help.contains("Commit:"));
        assert!(!help.contains("Built:"));
    }

    #[test]
    fn version_flag_reports_only_the_package_version() {
        let version = Cli::command().render_version().to_string();
        assert_eq!(version, format!("trueflow {}\n", build_info::VERSION));
    }

    #[test]
    fn help_documents_pull_request_review_and_feedback_targets() {
        let mut command = Cli::command();

        let review_help = subcommand_long_help(&mut command, "review");
        let tui_help = subcommand_long_help(&mut command, "tui");
        let feedback_help = subcommand_long_help(&mut command, "feedback");

        assert!(
            review_help.contains("rev:abc1234..def5678`, or `pr:11"),
            "review target help should document PR targets:\n{review_help}"
        );
        assert!(
            tui_help.contains("rev:abc1234..def5678`, or `pr:11"),
            "TUI target help should document PR targets:\n{tui_help}"
        );
        assert!(
            feedback_help.contains("use `--pr` for pull request posting"),
            "feedback target help should point PR posting to --pr:\n{feedback_help}"
        );
        assert!(
            feedback_help.contains("Post feedback to a pull request such as `pr:11`"),
            "feedback --pr help should document pull request posting:\n{feedback_help}"
        );
    }

    fn subcommand_long_help(command: &mut clap::Command, name: &str) -> String {
        let subcommand = command
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("expected {name} subcommand"));
        let mut help = Vec::new();
        subcommand
            .write_long_help(&mut help)
            .unwrap_or_else(|error| panic!("failed to render {name} help output: {error}"));
        String::from_utf8(help)
            .unwrap_or_else(|error| panic!("{name} help output was not utf8: {error}"))
    }

    #[test]
    fn diff_command_is_rejected() {
        let err = match Cli::try_parse_from(["trueflow", "diff", "--json"]) {
            Ok(_) => panic!("expected clap to reject removed diff command"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("unrecognized subcommand") && rendered.contains("diff"),
            "unexpected clap error: {rendered}"
        );
    }

    #[test]
    fn sync_command_is_rejected() {
        let err = match Cli::try_parse_from(["trueflow", "sync"]) {
            Ok(_) => panic!("expected clap to reject removed sync command"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("unrecognized subcommand") && rendered.contains("sync"),
            "unexpected clap error: {rendered}"
        );
    }

    #[test]
    fn tui_command_parses_targets_and_filters() {
        let cli = Cli::parse_from([
            "trueflow",
            "tui",
            "--target",
            "file:src/lib.rs",
            "--target",
            "rev:abc1234",
            "--only",
            "function",
            "--exclude",
            "comment",
        ]);

        match cli.command {
            Commands::Tui {
                all,
                target,
                since,
                only,
                exclude,
            } => {
                assert!(!all);
                assert_eq!(
                    target,
                    vec![
                        ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
                        ReviewTarget::Revision(RevisionExpr::new("abc1234").unwrap()),
                    ]
                );
                assert!(since.is_none());
                assert_eq!(only, vec![BlockKind::Function]);
                assert_eq!(exclude, vec![BlockKind::Comment]);
            }
            _ => panic!("expected tui command"),
        }
    }

    #[test]
    fn tui_command_defaults_to_empty_overrides() {
        let cli = Cli::parse_from(["trueflow", "tui"]);

        match cli.command {
            Commands::Tui {
                all,
                target,
                since,
                only,
                exclude,
            } => {
                assert!(!all);
                assert!(target.is_empty());
                assert!(since.is_none());
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected tui command"),
        }
    }

    #[test]
    fn feedback_command_parses_since_override() {
        let cli = Cli::parse_from([
            "trueflow", "feedback", "--format", "json", "--since", "last",
        ]);

        match cli.command {
            Commands::Feedback {
                format,
                since,
                pr,
                dry_run,
                open,
                submit,
                target,
                include_approved,
                only,
                exclude,
            } => {
                assert_eq!(format, FeedbackFormat::Json);
                assert_eq!(since, Some(FeedbackSinceExpr::new("last").unwrap()));
                assert!(pr.is_none());
                assert!(!dry_run);
                assert!(!open);
                assert!(!submit);
                assert!(target.is_empty());
                assert!(!include_approved);
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected feedback command"),
        }
    }

    #[test]
    fn feedback_command_parses_explicit_targets() {
        let cli = Cli::parse_from([
            "trueflow",
            "feedback",
            "--target",
            "file:src/lib.rs",
            "--target",
            "dir:src",
            "--target",
            "rev:abc1234",
        ]);

        match cli.command {
            Commands::Feedback {
                pr,
                dry_run,
                open,
                target,
                ..
            } => {
                assert!(pr.is_none());
                assert!(!dry_run);
                assert!(!open);
                assert_eq!(
                    target,
                    vec![
                        ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
                        ReviewTarget::Dir(RepoPath::new("src").unwrap()),
                        ReviewTarget::Revision(RevisionExpr::new("abc1234").unwrap()),
                    ]
                );
            }
            _ => panic!("expected feedback command"),
        }
    }

    #[test]
    fn feedback_command_parses_pull_request_mode() {
        let cli = Cli::parse_from(["trueflow", "feedback", "--pr", "pr:11", "--dry-run"]);

        match cli.command {
            Commands::Feedback {
                format,
                since,
                pr,
                dry_run,
                open,
                submit,
                target,
                include_approved,
                only,
                exclude,
            } => {
                assert_eq!(format, FeedbackFormat::Xml);
                assert!(since.is_none());
                assert_eq!(pr, Some(PullRequestRef::Number { number: 11 }));
                assert!(dry_run);
                assert!(!open);
                assert!(!submit);
                assert!(target.is_empty());
                assert!(!include_approved);
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected feedback command"),
        }
    }

    #[test]
    fn feedback_command_parses_submit_pull_request_mode() {
        let cli = Cli::parse_from(["trueflow", "feedback", "--pr", "pr:11", "--submit"]);

        match cli.command {
            Commands::Feedback {
                pr,
                dry_run,
                open,
                submit,
                ..
            } => {
                assert_eq!(pr, Some(PullRequestRef::Number { number: 11 }));
                assert!(!dry_run);
                assert!(!open);
                assert!(submit);
            }
            _ => panic!("expected feedback command"),
        }
    }

    #[test]
    fn feedback_command_rejects_target_when_pr_is_present() {
        let err = match Cli::try_parse_from([
            "trueflow", "feedback", "--pr", "pr:11", "--target", "dir:src",
        ]) {
            Ok(_) => panic!("expected clap to reject mixed feedback target modes"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("cannot be used with '--target <TARGET>'"));
    }

    #[test]
    fn feedback_command_parses_open_pull_request_mode() {
        let cli = Cli::parse_from(["trueflow", "feedback", "--pr", "pr:11", "--open"]);

        match cli.command {
            Commands::Feedback {
                pr,
                dry_run,
                open,
                submit,
                ..
            } => {
                assert_eq!(pr, Some(PullRequestRef::Number { number: 11 }));
                assert!(!dry_run);
                assert!(open);
                assert!(!submit);
            }
            _ => panic!("expected feedback command"),
        }
    }

    #[test]
    fn tui_command_parses_pull_request_target() {
        let cli = Cli::parse_from(["trueflow", "tui", "--target", "pr:11"]);

        match cli.command {
            Commands::Tui {
                all,
                target,
                since,
                only,
                exclude,
            } => {
                assert!(!all);
                assert_eq!(
                    target,
                    vec![ReviewTarget::PullRequest(PullRequestRef::Number {
                        number: 11
                    })]
                );
                assert!(since.is_none());
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected tui command"),
        }
    }

    #[test]
    fn review_command_parses_explicit_dirty_and_main_targets() {
        let cli = Cli::parse_from([
            "trueflow", "review", "--target", "dirty", "--target", "main",
        ]);

        match cli.command {
            Commands::Review {
                target,
                since,
                only,
                exclude,
                ..
            } => {
                assert_eq!(
                    target,
                    vec![ReviewTarget::DirtyWorktree, ReviewTarget::MainDiff]
                );
                assert!(since.is_none());
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn review_command_parses_since_commit() {
        let cli = Cli::parse_from(["trueflow", "review", "--since", "abc1234"]);

        match cli.command {
            Commands::Review {
                all,
                target,
                since,
                only,
                exclude,
                ..
            } => {
                assert!(!all);
                assert!(target.is_empty());
                assert_eq!(since.as_deref(), Some("abc1234"));
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn tui_command_parses_since_commit() {
        let cli = Cli::parse_from(["trueflow", "tui", "--since", "abc1234"]);

        match cli.command {
            Commands::Tui {
                all,
                target,
                since,
                only,
                exclude,
            } => {
                assert!(!all);
                assert!(target.is_empty());
                assert_eq!(since.as_deref(), Some("abc1234"));
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected tui command"),
        }
    }


    #[test]
    fn review_command_accepts_whitespace_exclude_alias() {
        let cli = Cli::parse_from(["trueflow", "review", "--exclude", "whitespace"]);

        match cli.command {
            Commands::Review { exclude, .. } => {
                assert_eq!(exclude, vec![BlockKind::Gap]);
            }
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn review_command_rejects_unknown_block_kind() {
        let err = match Cli::try_parse_from(["trueflow", "review", "--only", "not-a-kind"]) {
            Ok(_) => panic!("expected clap to reject unknown block kind"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("Unknown block kind: not-a-kind"),
            "unexpected clap error: {rendered}"
        );
    }

    #[test]
    fn inspect_command_parses_coverage_flag() {
        let cli = Cli::parse_from([
            "trueflow",
            "inspect",
            "--fingerprint",
            "abc1234",
            "--coverage",
        ]);
        match cli.command {
            Commands::Inspect {
                fingerprint,
                split,
                coverage,
            } => {
                assert_eq!(fingerprint, "abc1234");
                assert!(!split);
                assert!(coverage);
            }
            _ => panic!("expected inspect command"),
        }
    }

    #[test]
    fn mark_command_parses_typed_fields() {
        let cli = Cli::parse_from([
            "trueflow",
            "mark",
            "--fingerprint",
            "abc1234",
            "--verdict",
            "comment",
            "--check",
            "security",
            "--path",
            "src/lib.rs",
            "--line",
            "42",
        ]);

        match cli.command {
            Commands::Mark {
                fingerprint,
                verdict,
                check,
                path,
                line,
                ..
            } => {
                assert_eq!(fingerprint, "abc1234");
                assert_eq!(verdict, Verdict::Comment);
                assert_eq!(check, ReviewCheck::new("security").unwrap());
                assert_eq!(path, Some(RepoPath::new("src/lib.rs").unwrap()));
                assert_eq!(line, Some(42));
            }
            _ => panic!("expected mark command"),
        }
    }

    #[test]
    fn mark_command_rejects_invalid_repo_path() {
        let err = match Cli::try_parse_from([
            "trueflow",
            "mark",
            "--fingerprint",
            "abc1234",
            "--path",
            "../src/lib.rs",
        ]) {
            Ok(_) => panic!("expected clap to reject invalid repo path"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("repo path contains invalid segment")
        );
    }

    #[test]
    fn mark_command_parses_hidden_comment_anchor_json() {
        let comment_anchor =
            serde_json::to_string(&CommentAnchor::Source(crate::store::SourceCommentAnchor {
                revision: crate::store::CommitId::new("1111111111111111111111111111111111111111")
                    .unwrap(),
                path: RepoPath::new("src/lib.rs").unwrap(),
                start_line: 1,
                end_line: 3,
            }))
            .unwrap();
        let cli = Cli::parse_from([
            "trueflow",
            "mark",
            "--fingerprint",
            "abc1234",
            "--comment-anchor-json",
            &comment_anchor,
        ]);

        match cli.command {
            Commands::Mark {
                comment_anchor_json,
                ..
            } => {
                assert_eq!(comment_anchor_json, Some(comment_anchor));
            }
            _ => panic!("expected mark command"),
        }
    }

    #[test]
    fn feedback_command_rejects_unknown_format() {
        let err = match Cli::try_parse_from(["trueflow", "feedback", "--format", "yaml"]) {
            Ok(_) => panic!("expected clap to reject unknown feedback format"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("possible values"));
    }

    #[test]
    fn feedback_command_rejects_invalid_since_expression() {
        let err = match Cli::try_parse_from(["trueflow", "feedback", "--since", "someday"]) {
            Ok(_) => panic!("expected clap to reject invalid feedback since value"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("Invalid feedback since value 'someday'")
        );
    }

    #[test]
    fn scan_command_rejects_tree_without_json() {
        let err = match Cli::try_parse_from(["trueflow", "scan", "--tree"]) {
            Ok(_) => panic!("expected clap to reject --tree without --json"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("--json") && rendered.contains("--tree"),
            "unexpected clap error: {rendered}"
        );
    }

    #[test]
    fn verify_command_requires_selection() {
        let err = match Cli::try_parse_from(["trueflow", "verify"]) {
            Ok(_) => panic!("expected clap to require a verify selection"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("--all") && rendered.contains("--id"),
            "unexpected clap error: {rendered}"
        );
    }

    #[test]
    fn verify_command_rejects_all_and_id_together() {
        let err = match Cli::try_parse_from(["trueflow", "verify", "--all", "--id", "abc"]) {
            Ok(_) => panic!("expected clap to reject --all with --id"),
            Err(err) => err,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("--all") && rendered.contains("--id"),
            "unexpected clap error: {rendered}"
        );
    }
}
