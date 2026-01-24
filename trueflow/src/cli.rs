use clap::{Parser, Subcommand};

use crate::logging::LoggingMode;

#[derive(Parser)]
#[command(name = "trueflow")]
#[command(about = "Semantic review for the agent era", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(
        long,
        value_enum,
        default_value_t = LoggingMode::Stderr,
        hide = true
    )]
    pub logging_mode: LoggingMode,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show unreviewed hunks (semantic diff)
    Diff {
        /// Output format (default is text, use --json for machine parsing)
        #[arg(long)]
        json: bool,
    },
    /// Mark a hunk with a verdict
    Mark {
        /// Content-based fingerprint of the hunk
        #[arg(long)]
        fingerprint: String,

        /// Verdict: approved, rejected, question, comment
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
    },
    /// Sync reviews with remote (fetch & push trueflow-db branch)
    Sync,
    /// CI gate check
    Check,
    /// Scan the directory and build the Merkle tree (Audit mode)
    Scan {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Interactive review of unreviewed blocks
    Review {
        /// Output format (default is text, use --json for machine parsing)
        #[arg(long)]
        json: bool,

        /// Review everything (Audit mode), ignoring git status
        #[arg(long)]
        all: bool,

        /// Exclude block types (e.g. "gap", "comment", "whitespace")
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Export feedback for LLM/Agent consumption
    Feedback {
        /// Output format (xml or json)
        #[arg(long, default_value = "xml")]
        format: String,

        /// Include approved blocks (for few-shot examples)
        #[arg(long)]
        include_approved: bool,

        /// Exclude block types
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Inspect a block (and optionally split it)
    Inspect {
        /// Block fingerprint (hash)
        #[arg(long)]
        fingerprint: String,

        /// Split into sub-blocks
        #[arg(long)]
        split: bool,
    },
    /// Launch the TUI
    Tui,
}
