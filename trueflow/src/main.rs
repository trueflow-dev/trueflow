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
            deleted_in,
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
                deletion: deleted_in
                    .as_deref()
                    .map(|revision| {
                        trueflow::vcs::resolve_commit_id_from_workdir(revision)
                            .map(|head_revision| trueflow::store::BlockDeletion { head_revision })
                    })
                    .transpose()?,
                provenance: None,
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
            pass,
            budget,
        } => commands::review::run(
            &context,
            commands::review::ReviewRunRequest {
                json: *json,
                all: *all,
                target,
                since: since.as_deref(),
                only,
                exclude,
                pass: *pass,
                budget: *budget,
            },
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
            path,
            line,
        } => commands::inspect::run(
            &context,
            fingerprint,
            commands::inspect::LocationFilter {
                path: path.as_ref(),
                line: *line,
            },
            *split,
            *coverage,
        ),
        Commands::Verify { all, id } => commands::verify::run(
            commands::verify::VerifySelection::from_clap_args(*all, id.as_deref()),
        ),
        Commands::Onboard(args) => commands::onboard::run(&context, args),
        Commands::Focus(args) => commands::focus::run(&context, args),
        Commands::Tui {
            mode,
            trust_lsp_workspace,
            all,
            target,
            since,
            only,
            exclude,
            pass,
            budget,
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
                pass: *pass,
                budget: *budget,
            },
        ),
    }
}
