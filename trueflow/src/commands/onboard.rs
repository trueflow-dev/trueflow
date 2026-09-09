use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;

use crate::ai::{
    AiAvailability, AiEnvironment, CommandAiReviewPlanProvider, resolve_ai_availability,
};
use crate::commands::{review, tui};
use crate::config::{AiMode, TrueflowConfig};
use crate::context::TrueflowContext;
use crate::onboarding::{ReviewSpecification, candidate_page, generation_prompt};
use crate::repo_path::RepoPath;
use crate::scanner::ScanCacheMode;

#[derive(Debug, Args)]
pub struct OnboardArgs {
    /// Open a portable JSON review specification instead of generating one
    #[arg(long, value_name = "FILE", conflicts_with = "candidates")]
    pub plan: Option<PathBuf>,

    /// Print the generated/loaded specification without entering the TUI
    #[arg(long, conflicts_with = "candidates")]
    pub json: bool,

    /// Query unreviewed source targets as JSON (no LLM invocation)
    #[arg(long)]
    pub candidates: bool,

    /// Restrict candidate queries to a repository-relative file or directory
    #[arg(long, requires = "candidates")]
    pub path: Option<RepoPath>,

    /// Candidate query offset, in stable path/source order
    #[arg(long, requires = "candidates")]
    pub offset: Option<usize>,

    /// Candidate page size (1..=64; default 64)
    #[arg(long, requires = "candidates", value_parser = clap::value_parser!(u8).range(1..=64))]
    pub limit: Option<u8>,
}

pub fn run(context: &TrueflowContext, args: &OnboardArgs) -> Result<()> {
    // Resolve a user-supplied plan before switching from a subtree to the repository root.
    let plan_path = args.plan.as_ref().map(std::path::absolute).transpose()?;
    if let Some(root) = crate::vcs::git_root_from_workdir()? {
        std::env::set_current_dir(root)
            .context("failed to enter repository root for onboarding")?;
    }
    let config = crate::config::load()?;
    if !args.candidates && !args.json {
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "onboarding review needs an interactive terminal; use --json to generate a specification without the TUI"
        );
    }
    let loaded_plan = plan_path
        .as_ref()
        .map(|path| {
            let mut text = String::new();
            std::fs::File::open(path)
                .with_context(|| format!("failed to open onboarding plan {}", path.display()))?
                .take(256 * 1024 + 1)
                .read_to_string(&mut text)?;
            ReviewSpecification::from_json(&text)
        })
        .transpose()?;
    let collected = collect(&config, args.candidates)?;
    if args.candidates {
        let page = candidate_page(
            &collected,
            args.path.as_ref(),
            args.offset.unwrap_or(0),
            usize::from(args.limit.unwrap_or(64)),
        )?;
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }
    for diagnostic in &collected.summary.diagnostics {
        eprintln!("warning: {}", diagnostic.display_message());
    }
    if collected.unreviewed_block_nodes.is_empty() && loaded_plan.is_none() {
        if args.json {
            bail!("no eligible unreviewed blocks; no onboarding specification generated");
        }
        eprintln!("No eligible unreviewed blocks remain in this repository scope.");
        return Ok(());
    }
    let (specification, collected) = match loaded_plan {
        Some(plan) => (plan, collected),
        None => {
            let mut ai = config.ai.clone();
            // An explicit onboard invocation requests generation even when normal TUI AI is off.
            ai.mode = AiMode::ReviewPlan;
            let availability = resolve_ai_availability(&ai, &AiEnvironment::detect_current());
            let AiAvailability::Ready { provider, model } = availability else {
                bail!(
                    "onboarding requires an available LLM CLI: {availability:?}. Install/authenticate codex, or configure [ai].provider and [ai].model"
                );
            };
            let generator = CommandAiReviewPlanProvider::new(provider, model.clone())?;
            let prompt = generation_prompt(&collected, &std::env::current_exe()?)?;
            eprintln!(
                "Generating a focused onboarding review with {} ({model}); targeting under 15 minutes, at most 64 cards...",
                provider.label()
            );
            let answer = generator.generate(&prompt)?;
            let specification = ReviewSpecification::from_json(&answer)?;
            // Generation can take time. Recheck both source and approvals, and use one fresh
            // captured tree for all cards; never render new content under old target identities.
            drop(collected);
            let fresh = collect(&config, true)?;
            (specification, fresh)
        }
    };
    let prepared = specification.prepare(collected)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&specification)?);
        return Ok(());
    }
    if prepared.cards.is_empty() {
        eprintln!("No eligible unreviewed cards remain in this onboarding pass.");
        return Ok(());
    }
    eprintln!(
        "Starting {} cards (estimated {} minutes; actual pace varies).",
        prepared.cards.len(),
        specification.estimated_seconds().div_ceil(60)
    );
    tui::run_onboarding(context, &config, prepared)
}

pub(crate) fn collect(config: &TrueflowConfig, read_only: bool) -> Result<review::CollectedReview> {
    let mut scan_options = config.scan.resolve_options();
    if read_only {
        scan_options.cache_mode = match scan_options.cache_mode {
            ScanCacheMode::ReadOnly | ScanCacheMode::ReadWrite => ScanCacheMode::ReadOnly,
            ScanCacheMode::Disabled | ScanCacheMode::WriteOnly => ScanCacheMode::Disabled,
        };
    }
    let query = review::resolve_review_request(
        review::ReviewRequest::AllFiles,
        config.review.resolve_filters(&[], &[]),
        scan_options,
    )?;
    review::collect_review(&query)
}
