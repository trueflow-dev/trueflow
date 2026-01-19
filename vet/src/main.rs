use clap::{Parser, Subcommand};
use anyhow::Result;

mod commands;
mod hashing;
mod store;
mod diff_logic;




#[derive(Parser)]
#[command(name = "vet")]
#[command(about = "Semantic code review for the agent era", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
    /// Sync reviews with remote (fetch & push vet-db branch)
    Sync,
    /// CI gate check
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Diff { json } => commands::diff::run(*json),
        Commands::Mark { fingerprint, verdict, check, note, path, line } => {
            commands::mark::run(fingerprint, verdict, check, note.as_deref(), path.as_deref(), *line)
        },
        Commands::Sync => commands::sync::run(),
        Commands::Check => commands::check::run(),
    }
}
