use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;

use crate::ai::{
    AiAvailability, AiEnvironment, CommandAiReviewPlanProvider, resolve_ai_availability,
};
use crate::commands::{onboard, tui};
use crate::config::AiMode;
use crate::context::TrueflowContext;
use crate::focus::{FocusSpecification, ReviewDuration};
use crate::focus_evidence::{FocusInventory, FocusScope, LimitationSummary};
use crate::repo_path::RepoPath;

#[derive(Debug, Args)]
pub struct FocusArgs {
    /// Requested reading estimate, e.g. 5m or 10m (interactive prompt if omitted)
    #[arg(long)]
    pub duration: Option<ReviewDuration>,
    /// Load and revalidate a portable focus specification without an LLM
    #[arg(long, value_name = "FILE", conflicts_with_all = ["candidates", "path"])]
    pub plan: Option<PathBuf>,
    /// Print a validated specification without opening the TUI
    #[arg(long, conflicts_with = "candidates")]
    pub json: bool,
    /// Inspect current candidates and grounded evidence without invoking an LLM
    #[arg(long)]
    pub candidates: bool,
    /// Restrict discovery to a repository-relative file or directory
    #[arg(long)]
    pub path: Option<RepoPath>,
    #[arg(long, requires = "candidates", default_value_t = 0)]
    pub offset: usize,
    /// Candidate page size (default 64)
    #[arg(long, requires = "candidates", default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=256))]
    pub limit: u16,
    /// Total generation deadline in seconds, across all selection passes
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
    pub timeout: u64,
}

pub fn run(context: &TrueflowContext, args: &FocusArgs) -> Result<()> {
    let plan_path = args.plan.as_ref().map(std::path::absolute).transpose()?;
    let loaded = plan_path
        .as_ref()
        .map(|path| {
            let mut text = String::new();
            std::fs::File::open(path)
                .with_context(|| format!("failed to open focus plan {}", path.display()))?
                .take(crate::focus::MAX_SPECIFICATION_BYTES as u64 + 1)
                .read_to_string(&mut text)?;
            FocusSpecification::from_json(&text)
        })
        .transpose()?;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !args.candidates && !args.json {
        ensure!(
            interactive,
            "focus review needs an interactive terminal; use --json with --duration or --plan"
        );
    }
    let duration = match (&loaded, args.duration, args.candidates) {
        (Some(plan), requested, _) => {
            ensure!(
                requested.is_none_or(|duration| duration == plan.duration()),
                "--duration disagrees with the saved plan duration; generate a new plan"
            );
            Some(plan.duration())
        }
        (None, requested, true) => requested,
        (None, Some(duration), false) => Some(duration),
        (None, None, false) => {
            ensure!(
                interactive,
                "non-interactive focus generation requires --duration, e.g. --duration 5m"
            );
            eprint!("Review duration (5m, 10m, 15m, 30m, or custom up to 60m): ");
            std::io::stderr().flush()?;
            let mut input = String::new();
            std::io::stdin().lock().take(64).read_line(&mut input)?;
            Some(input.trim().parse()?)
        }
    };
    if let Some(root) = crate::vcs::git_root_from_workdir()? {
        std::env::set_current_dir(root).context("failed to enter repository root for focus")?;
    }
    let config = crate::config::load()?;
    let (collected, database) = onboard::collect_with_database(&config, true)?;
    let scope = match (&loaded, &args.path) {
        (Some(plan), _) => plan.target_scope(),
        (None, Some(path)) => FocusScope::Under(path.clone()),
        (None, None) => FocusScope::Repository,
    };
    let referenced_evidence = loaded
        .as_ref()
        .map(FocusSpecification::referenced_evidence_ids)
        .unwrap_or_default();
    let inventory = FocusInventory::capture_with_database_and_referenced_evidence(
        &collected,
        &scope,
        &database,
        &referenced_evidence,
    )?;
    if args.candidates {
        let page: Vec<_> = inventory
            .candidates
            .iter()
            .skip(args.offset)
            .take(usize::from(args.limit))
            .collect();
        let end = args.offset.saturating_add(page.len());
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": 1,
                "snapshot_hash": inventory.snapshot_hash,
                "total_candidates": inventory.candidates.len(),
                "offset": args.offset,
                "next_offset": (end < inventory.candidates.len()).then_some(end),
                "diagnostics": inventory.diagnostics,
                "candidates": page,
            }))?
        );
        return Ok(());
    }
    if !inventory.diagnostics.is_empty() {
        eprintln!(
            "warning: evidence limitations: {}",
            LimitationSummary::of(&inventory.diagnostics)
        );
    }
    let duration = duration.context("missing focus duration")?;
    let oversized = inventory
        .candidates
        .iter()
        .filter(|candidate| candidate.estimated_seconds > duration.seconds())
        .count();
    if oversized > 0 {
        eprintln!(
            "{oversized} complete blocks exceed the requested reading estimate; inspect --candidates or choose a longer duration."
        );
    }
    let (specification, collected, inventory) = match loaded {
        Some(plan) => (plan, collected, inventory),
        None => {
            // An empty scope or budget is the answer; don't ask for an LLM first.
            crate::focus::eligible_candidates(&inventory, duration)?;
            let mut ai = config.ai.clone();
            ai.mode = AiMode::ReviewPlan;
            let availability = resolve_ai_availability(&ai, &AiEnvironment::detect_current());
            let AiAvailability::Ready { provider, model } = availability else {
                bail!(
                    "focus requires an available LLM CLI: {availability:?}; authenticate codex or configure [ai].provider and [ai].model"
                );
            };
            let generator = CommandAiReviewPlanProvider::new(provider, model.clone())?;
            eprintln!(
                "Selecting a grounded {}s focus review with {} ({model}); reading estimate, not a time guarantee...",
                duration.seconds(),
                provider.label()
            );
            let deadline = Instant::now() + Duration::from_secs(args.timeout);
            let specification = crate::focus::generate(&inventory, duration, |prompt| {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                    .context("focus generation timed out; no partial plan generated")?;
                generator.generate_from_evidence(prompt, remaining)
            })?;
            drop(collected);
            drop(database);
            let (fresh, fresh_database) = onboard::collect_with_database(&config, true)?;
            let fresh_inventory =
                FocusInventory::capture_with_database(&fresh, &scope, &fresh_database)?;
            ensure!(
                fresh_inventory.snapshot_hash == inventory.snapshot_hash,
                "source, review state or evidence changed during focus generation; regenerate the plan"
            );
            (specification, fresh, fresh_inventory)
        }
    };
    let prepared = specification.prepare(collected, &inventory)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&specification)?);
        return Ok(());
    }
    if prepared.cards.is_empty() {
        eprintln!("No eligible unreviewed cards remain in this focus plan.");
        return Ok(());
    }
    eprintln!(
        "Starting {} focus cards (estimated {}s of requested {}s; actual pace varies). F1 shows evidence and inspection questions.",
        prepared.cards.len(),
        specification.estimated_seconds(),
        duration.seconds()
    );
    tui::run_onboarding(context, &config, prepared)
}
