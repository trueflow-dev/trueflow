use anyhow::Result;
use clap::Parser;
use log::info;

mod analysis;
mod block;
mod block_splitter;
mod cli;
mod commands;
mod complexity;
mod config;
mod context;
mod diff_logic;
mod hashing;
mod logging;
mod optimizer;
mod policy;
mod scanner;
mod store;
pub mod sub_splitter;
mod text_split;
mod tree;
mod vcs;

use crate::cli::{Cli, Commands};
use crate::context::TrueflowContext;

fn main() -> Result<()> {
    let cli = Cli::parse();
    logging::init_logging(cli.logging_mode, cli.debug)?;
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
            quiet: _,
        } => commands::mark::run(
            &context,
            commands::mark::MarkParams {
                fingerprint: fingerprint.clone(),
                verdict: verdict.parse()?,
                check: check.clone(),
                note: note.clone(),
                path: path.clone(),
                line: *line,
            },
        ),
        Commands::Sync => commands::sync::run(&context),
        Commands::Check => commands::check::run(&context),
        Commands::Scan { json, tree } => commands::scan::run(&context, *json, *tree),
        Commands::Review {
            json,
            all,
            target,
            only,
            exclude,
        } => commands::review::run(
            &context,
            *json,
            *all,
            target.clone(),
            only.clone(),
            exclude.clone(),
        ),
        Commands::Feedback {
            format,
            include_approved,
            only,
            exclude,
        } => commands::feedback::run(
            &context,
            format,
            *include_approved,
            only.clone(),
            exclude.clone(),
        ),
        Commands::Inspect { fingerprint, split } => {
            commands::inspect::run(&context, fingerprint, *split)
        }
        Commands::Verify { all, id } => commands::verify::run(*all, id.clone()),
        Commands::Tui => commands::tui::run(&context),
    }
}
