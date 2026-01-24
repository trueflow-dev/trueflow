use anyhow::Result;
use clap::Parser;
use log::info;

mod analysis;
mod block;
mod block_splitter;
mod cli;
mod commands;
mod context;
mod diff_logic;
mod hashing;
mod logging;
mod optimizer;
mod scanner;
mod store;
pub mod sub_splitter;

use crate::cli::{Cli, Commands};
use crate::context::TrueflowContext;

fn main() -> Result<()> {
    let cli = Cli::parse();
    logging::init_logging(cli.logging_mode)?;
    let context = TrueflowContext::new(cli);
    info!("trueflow starting");
    info!("logging mode: {:?}", context.invocation.logging_mode);
    info!("args: {:?}", std::env::args().collect::<Vec<_>>());
    info!("command parsed");
    if let Ok(dir) = context.trueflow_dir() {
        info!("trueflow dir: {}", dir.display());
    }

    match &context.invocation.command {
        Commands::Diff { json } => commands::diff::run(&context, *json),
        Commands::Mark {
            fingerprint,
            verdict,
            check,
            note,
            path,
            line,
        } => commands::mark::run(
            &context,
            fingerprint,
            verdict,
            check,
            note.as_deref(),
            path.as_deref(),
            *line,
        ),
        Commands::Sync => commands::sync::run(&context),
        Commands::Check => commands::check::run(&context),
        Commands::Scan { json } => commands::scan::run(&context, *json),
        Commands::Review { json, all, exclude } => {
            commands::review::run(&context, *json, *all, exclude.clone())
        }
        Commands::Feedback {
            format,
            include_approved,
            exclude,
        } => commands::feedback::run(&context, format, *include_approved, exclude.clone()),
        Commands::Inspect { fingerprint, split } => {
            commands::inspect::run(&context, fingerprint, *split)
        }
        Commands::Tui => commands::tui::run(&context),
    }
}
