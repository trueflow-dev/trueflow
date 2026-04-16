use anyhow::Result;
use clap::Parser;
use tracing::info;
use trueflow::cli::{Cli, Commands};
use trueflow::commands;
use trueflow::context::TrueflowContext;
use trueflow::logging;

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
                target_kind: None,
                verdict: verdict.clone(),
                check: check.clone(),
                note: note.clone(),
                path: path.clone(),
                line: *line,
            },
        ),
        Commands::Check => commands::check::run(&context),
        Commands::Scan { json, tree } => commands::scan::run(
            &context,
            commands::scan::ScanOutputMode::from_flags(*json, *tree),
        ),
        Commands::Review {
            json,
            all,
            target,
            since,
            only,
            exclude,
        } => commands::review::run(
            &context,
            *json,
            *all,
            target,
            since.as_deref(),
            only,
            exclude,
        ),
        Commands::Feedback {
            format,
            since,
            target,
            include_approved,
            only,
            exclude,
        } => commands::feedback::run(
            &context,
            *format,
            since.as_ref(),
            target,
            *include_approved,
            only,
            exclude,
        ),
        Commands::Inspect {
            fingerprint,
            split,
            coverage,
        } => commands::inspect::run(&context, fingerprint, *split, *coverage),
        Commands::Verify { all, id } => commands::verify::run(
            commands::verify::VerifySelection::from_clap_args(*all, id.as_deref()),
        ),
        Commands::Tui {
            all,
            target,
            since,
            only,
            exclude,
        } => commands::tui::run(&context, *all, target, since.as_deref(), only, exclude),
    }
}
