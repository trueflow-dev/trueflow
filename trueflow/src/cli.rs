use clap::{ArgGroup, Parser, Subcommand};

use crate::block::BlockKind;
use crate::build_info;
use crate::commands::review::ReviewTarget;
use crate::logging::LoggingMode;

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
        #[arg(long, default_value = "approved")]
        verdict: String,

        /// Check type: review, security, style, etc.
        #[arg(long, default_value = "review")]
        check: String,

        /// Optional note
        #[arg(long)]
        note: Option<String>,

        /// Path hint for debugging/UI
        #[arg(long)]
        path: Option<String>,

        /// Line number hint
        #[arg(long)]
        line: Option<u32>,

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

        /// Review targets such as `dirty`, `main`, `file:src/lib.rs`, `dir:src`, or `rev:abc1234..def5678`
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
        #[arg(long, default_value = "xml")]
        format: String,

        /// Only include records since this point ("all", "last", relative durations like "1h", unix ts, or RFC3339)
        #[arg(long)]
        since: Option<String>,

        /// Feedback targets such as `dirty`, `main`, `file:src/lib.rs`, `dir:src`, or `rev:abc1234..def5678`
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

        /// Review targets such as `dirty`, `main`, `file:src/lib.rs`, `dir:src`, or `rev:abc1234..def5678`
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
    use crate::build_metadata::UNKNOWN_BUILD_TIMESTAMP;
    use crate::commands::review::{ReviewTarget, RevisionSpec};
    use crate::repo_path::RepoPath;
    use chrono::DateTime;
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
    fn long_help_includes_build_metadata_footer() {
        let mut command = Cli::command();
        let mut help = Vec::new();
        command
            .write_long_help(&mut help)
            .unwrap_or_else(|error| panic!("failed to render help output: {error}"));
        let help = String::from_utf8(help)
            .unwrap_or_else(|error| panic!("help output was not utf8: {error}"));

        assert!(help.contains(build_info::HELP_FOOTER));
    }

    #[test]
    fn build_timestamp_is_unknown_or_rfc3339() {
        let build_timestamp = env!("TRUEFLOW_BUILD_TIMESTAMP");
        if build_timestamp == UNKNOWN_BUILD_TIMESTAMP {
            return;
        }

        DateTime::parse_from_rfc3339(build_timestamp)
            .unwrap_or_else(|error| panic!("build timestamp was not RFC3339: {error}"));
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
                        ReviewTarget::Revision(RevisionSpec::new("abc1234").unwrap()),
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
                target,
                include_approved,
                only,
                exclude,
            } => {
                assert_eq!(format, "json");
                assert_eq!(since.as_deref(), Some("last"));
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
            Commands::Feedback { target, .. } => {
                assert_eq!(
                    target,
                    vec![
                        ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
                        ReviewTarget::Dir(RepoPath::new("src").unwrap()),
                        ReviewTarget::Revision(RevisionSpec::new("abc1234").unwrap()),
                    ]
                );
            }
            _ => panic!("expected feedback command"),
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
    fn review_command_parses_since_with_target() {
        let cli = Cli::parse_from([
            "trueflow", "review", "--since", "abc1234", "--target", "dir:src",
        ]);

        match cli.command {
            Commands::Review { target, since, .. } => {
                assert_eq!(since.as_deref(), Some("abc1234"));
                assert_eq!(
                    target,
                    vec![ReviewTarget::Dir(RepoPath::new("src").unwrap())]
                );
            }
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn tui_command_parses_since_with_target() {
        let cli = Cli::parse_from([
            "trueflow", "tui", "--since", "abc1234", "--target", "dir:src",
        ]);

        match cli.command {
            Commands::Tui { target, since, .. } => {
                assert_eq!(since.as_deref(), Some("abc1234"));
                assert_eq!(
                    target,
                    vec![ReviewTarget::Dir(RepoPath::new("src").unwrap())]
                );
            }
            _ => panic!("expected tui command"),
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
