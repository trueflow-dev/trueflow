use clap::{Parser, Subcommand};

use crate::block::BlockKind;
use crate::logging::LoggingMode;

#[derive(Parser)]
#[command(name = "trueflow")]
#[command(about = "Semantic review for the agent era", long_about = None)]
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
    /// Show unreviewed blocks (semantic diff view)
    Diff {
        /// Output format (default is text, use --json for machine parsing)
        #[arg(long)]
        json: bool,
    },
    /// Mark a review target with a verdict
    Mark {
        /// Current CLI field name for the review-target identifier
        /// (content-addressed block hash; distinct from diff fingerprints)
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
    /// Sync reviews with remote (fetch & push configured storage branch)
    Sync,
    /// CI gate check
    Check,
    /// Scan the directory and build the Merkle tree (Audit mode)
    Scan {
        /// Output JSON
        #[arg(long)]
        json: bool,

        /// Output the full Merkle tree
        #[arg(long)]
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

        /// Review targets (file:`<path>`, rev:`<sha>`, rev:`<start>..<end>`)
        #[arg(long, value_name = "TARGET")]
        target: Vec<String>,

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

        /// Only include records since this point ("all", "last", unix ts, or RFC3339)
        #[arg(long)]
        since: Option<String>,

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
    },
    /// Verify record attestations
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

        /// Review targets (file:`<path>`, rev:`<sha>`, rev:`<start>..<end>`)
        #[arg(long, value_name = "TARGET")]
        target: Vec<String>,

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
    use clap::{CommandFactory, Parser};

    #[test]
    fn sync_help_mentions_configured_storage_branch() {
        let mut command = Cli::command();
        let mut help = Vec::new();
        command
            .write_long_help(&mut help)
            .unwrap_or_else(|error| panic!("failed to render help output: {error}"));
        let help = String::from_utf8(help)
            .unwrap_or_else(|error| panic!("help output was not utf8: {error}"));
        assert!(help.contains("configured storage branch"));
        assert!(!help.contains("trueflow-db branch"));
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
                only,
                exclude,
            } => {
                assert!(!all);
                assert_eq!(
                    target,
                    vec!["file:src/lib.rs".to_string(), "rev:abc1234".to_string()]
                );
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
                only,
                exclude,
            } => {
                assert!(!all);
                assert!(target.is_empty());
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
                include_approved,
                only,
                exclude,
            } => {
                assert_eq!(format, "json");
                assert_eq!(since.as_deref(), Some("last"));
                assert!(!include_approved);
                assert!(only.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected feedback command"),
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
}
