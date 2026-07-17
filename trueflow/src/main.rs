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
            comment_scope_start,
            comment_scope_end,
            comment_context,
            comment_anchor_json,
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
                comment_scope: comment_scope_start.zip(*comment_scope_end).map(
                    |(start_line, end_line)| trueflow::store::CommentScope {
                        start_line,
                        end_line,
                    },
                ),
                comment_context: comment_context.clone(),
                comment_anchor: comment_anchor_json
                    .as_ref()
                    .map(|value| serde_json::from_str::<trueflow::store::CommentAnchor>(value))
                    .transpose()?,
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
            pr,
            dry_run,
            open,
            submit,
            target,
            include_approved,
            only,
            exclude,
        } => commands::feedback::run(
            &context,
            commands::feedback::FeedbackParams {
                format: *format,
                since: since.as_ref(),
                pr: pr.as_ref(),
                dry_run: *dry_run,
                open: *open,
                submit: *submit,
                targets: target,
                include_approved: *include_approved,
                only,
                exclude,
            },
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
            mode,
            trust_lsp_workspace,
            all,
            target,
            since,
            only,
            exclude,
        } => commands::tui::run(
            &context,
            commands::tui::TuiRunRequest {
                mode: *mode,
                trust_lsp_workspace: *trust_lsp_workspace,
                all: *all,
                target,
                since: since.as_deref(),
                only,
                exclude,
            },
        ),
    }
}
