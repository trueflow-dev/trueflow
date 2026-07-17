use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use trueflow::analysis::Language;
use trueflow::block::BlockKind;
use trueflow::cli::{Cli, Commands, TuiReviewMode};
use trueflow::commands::tui::{
    TuiLaunchPayload, build_pull_request_launch_queue, resolve_tui_launch,
};
use trueflow::config::TrueflowConfig;
use trueflow::declaration::Visibility;
use trueflow::declaration::diff::{DeclarationDiff, diff_declarations};
use trueflow::declaration::review::{
    DeclarationReviewDiffBatch, DeclarationReviewQuery, DeclarationReviewStatus,
    collect_declaration_review,
};
use trueflow::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};
use trueflow::github::PullRequestCommit;
use trueflow::review_scope::ScopePreset;
use trueflow::store::CommitId;

#[derive(Debug)]
struct ParsedTuiArgs {
    mode: Option<TuiReviewMode>,
    trust_lsp_workspace: bool,
    only: Vec<BlockKind>,
    exclude: Vec<BlockKind>,
}

fn parse_tui(args: &[&str]) -> Result<ParsedTuiArgs> {
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        Commands::Tui {
            mode,
            trust_lsp_workspace,
            only,
            exclude,
            ..
        } => Ok(ParsedTuiArgs {
            mode,
            trust_lsp_workspace,
            only,
            exclude,
        }),
        _ => anyhow::bail!("expected tui command"),
    }
}

fn parse_config(source: &str) -> Result<TrueflowConfig> {
    toml::from_str(source).context("failed to parse test configuration")
}

fn resolve_launch(config: &TrueflowConfig, args: &ParsedTuiArgs) -> Result<TuiLaunchPayload> {
    resolve_tui_launch(
        config,
        args.mode,
        args.trust_lsp_workspace,
        ScopePreset::All,
        &args.only,
        &args.exclude,
    )
}

fn snapshot(id: &str, path: &str, language: Language, source: &str) -> SourceSnapshot {
    SourceSnapshot::new(SnapshotId::new(id), Path::new(path), language, source)
}

fn added_pair(
    pair_id: &str,
    snapshot_id: &str,
    path: &str,
    language: Language,
    source: &str,
) -> SnapshotPair {
    SnapshotPair::new(
        SnapshotPairId::new(pair_id),
        None,
        Some(snapshot(snapshot_id, path, language, source)),
        PathPairEvidence::Unmatched,
    )
}

fn changed_pair(
    pair_id: &str,
    base_id: &str,
    head_id: &str,
    path: &str,
    language: Language,
    base: &str,
    head: &str,
) -> SnapshotPair {
    SnapshotPair::new(
        SnapshotPairId::new(pair_id),
        Some(snapshot(base_id, path, language, base)),
        Some(snapshot(head_id, path, language, head)),
        PathPairEvidence::SamePath,
    )
}

fn diff_batch(pairs: Vec<SnapshotPair>) -> Result<DeclarationReviewDiffBatch> {
    let diff = diff_declarations(&pairs)?;
    Ok(DeclarationReviewDiffBatch::new(pairs, diff))
}

fn declaration_launch(config_source: &str) -> Result<TuiLaunchPayload> {
    let config = parse_config(config_source)?;
    let args = parse_tui(&["trueflow", "tui"])?;
    resolve_launch(&config, &args)
}

#[test]
fn cli_parses_declaration_mode_and_omission_preserves_configured_blocks() -> Result<()> {
    let declarations = parse_tui(&[
        "trueflow",
        "tui",
        "--mode",
        "declarations",
        "--trust-lsp-workspace",
    ])?;
    assert_eq!(declarations.mode, Some(TuiReviewMode::Declarations));
    assert!(declarations.trust_lsp_workspace);

    let omitted = parse_tui(&["trueflow", "tui"])?;
    assert_eq!(
        omitted.mode, None,
        "clap must preserve the absence of an override"
    );

    let blocks_config = parse_config("[tui]\nmode = \"blocks\"\n")?;
    let launch = resolve_launch(&blocks_config, &omitted)?;
    assert_eq!(launch.mode, TuiReviewMode::Blocks);
    Ok(())
}

#[test]
fn configured_declarations_are_selected_and_cli_mode_overrides_them() -> Result<()> {
    let config = parse_config("[tui]\nmode = \"declarations\"\n")?;

    let configured = resolve_launch(&config, &parse_tui(&["trueflow", "tui"])?)?;
    assert_eq!(configured.mode, TuiReviewMode::Declarations);

    let overridden = resolve_launch(
        &config,
        &parse_tui(&["trueflow", "tui", "--mode", "blocks"])?,
    )?;
    assert_eq!(overridden.mode, TuiReviewMode::Blocks);
    Ok(())
}

#[test]
fn declaration_mode_rejects_cli_block_filters_with_actionable_errors() -> Result<()> {
    let config = parse_config("[tui]\nmode = \"blocks\"\n")?;
    for (flag, value) in [("--only", "function"), ("--exclude", "comment")] {
        let parsed = parse_tui(&["trueflow", "tui", "--mode", "declarations", flag, value])?;
        let Err(error) = resolve_launch(&config, &parsed) else {
            anyhow::bail!("declaration mode must reject block-only CLI filter {flag}");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains(flag) && message.contains("declaration"),
            "error for {flag} must name both the invalid flag and declaration mode: {message}"
        );
    }
    Ok(())
}

#[test]
fn configured_block_filters_do_not_remove_private_declarations() -> Result<()> {
    let config = parse_config(
        "[tui]\nmode = \"declarations\"\n\n[review]\nonly = [\"struct\"]\nexclude = [\"function\"]\n",
    )?;
    let launch = resolve_launch(&config, &parse_tui(&["trueflow", "tui"])?)?;
    let batch = diff_batch(vec![added_pair(
        "worktree",
        "head-private",
        "src/private.rs",
        Language::Rust,
        "fn hidden() {}\n\npub struct Visible;\n",
    )])?;

    let collection = collect_declaration_review(&launch.declaration_query(vec![batch])?)?;
    assert_eq!(collection.status, DeclarationReviewStatus::Ready);
    let declarations = collection
        .items
        .iter()
        .map(|item| (item.declaration.name.as_str(), &item.declaration.visibility))
        .collect::<Vec<_>>();
    assert_eq!(
        declarations,
        [
            ("hidden", &Visibility::Private),
            ("Visible", &Visibility::Public),
        ]
    );
    Ok(())
}

#[test]
fn body_only_diff_reports_no_surface_changes() -> Result<()> {
    let launch = declaration_launch("[tui]\nmode = \"declarations\"\n")?;
    let batch = diff_batch(vec![changed_pair(
        "dirty",
        "base-body",
        "head-body",
        "src/lib.rs",
        Language::Rust,
        "pub fn total(values: &[u64]) -> u64 { values.iter().sum() }\n",
        "pub fn total(values: &[u64]) -> u64 { values.iter().copied().sum() }\n",
    )])?;

    let collection = collect_declaration_review(&launch.declaration_query(vec![batch])?)?;
    assert!(collection.items.is_empty());
    assert_eq!(collection.status, DeclarationReviewStatus::NoSurfaceChanges);
    Ok(())
}

#[test]
fn unsupported_language_and_fully_reviewed_are_distinct_empty_states() -> Result<()> {
    let launch = declaration_launch("[tui]\nmode = \"declarations\"\n")?;
    let unsupported = diff_batch(vec![added_pair(
        "worktree",
        "head-unknown",
        "src/example.unknown",
        Language::Unknown,
        "declare widget\n",
    )])?;
    let unsupported_collection =
        collect_declaration_review(&launch.declaration_query(vec![unsupported])?)?;
    assert!(unsupported_collection.items.is_empty());
    assert!(matches!(
        unsupported_collection.status,
        DeclarationReviewStatus::UnsupportedLanguage { .. }
    ));

    let supported = diff_batch(vec![added_pair(
        "worktree",
        "head-reviewed",
        "src/reviewed.rs",
        Language::Rust,
        "pub fn reviewed(value: u8) -> u8 { value }\n",
    )])?;
    let initial = collect_declaration_review(&launch.declaration_query(vec![supported.clone()])?)?;
    assert_eq!(initial.status, DeclarationReviewStatus::Ready);
    let reviewed_ids = initial
        .items
        .iter()
        .map(|item| item.declaration.id.clone())
        .collect::<Vec<_>>();
    let reviewed_query = launch
        .declaration_query(vec![supported])?
        .with_reviewed_target_ids(reviewed_ids);
    let fully_reviewed = collect_declaration_review(&reviewed_query)?;
    assert!(fully_reviewed.items.is_empty());
    assert_eq!(
        fully_reviewed.status,
        DeclarationReviewStatus::FullyReviewed
    );
    Ok(())
}

fn declaration_for_unit(
    unit: &mut trueflow::declaration::diff::DeclarationDiffUnit,
) -> &mut trueflow::declaration::DeclarationNode {
    match (&mut unit.head, &mut unit.base) {
        (Some(head), _) => head,
        (None, Some(base)) => base,
        (None, None) => panic!("diff review unit must retain a declaration side"),
    }
}

fn force_kind_and_ordinal_ties(diff: &mut DeclarationDiff) {
    for unit in &mut diff.units {
        let declaration = declaration_for_unit(unit);
        if matches!(
            declaration.name.as_str(),
            "kind_function" | "kind_struct" | "ordinal_early" | "ordinal_late"
        ) {
            declaration.source_span = 0..1;
            declaration.source_ordinal = match declaration.name.as_str() {
                "ordinal_early" => 1,
                "ordinal_late" => 9,
                _ => 5,
            };
        }
    }
}

#[test]
fn canonical_order_is_stable_by_pair_path_source_kind_and_ordinal() -> Result<()> {
    let mut first_pairs = vec![
        added_pair(
            "commit-one",
            "commit-one-z",
            "z.rs",
            Language::Rust,
            "fn source_first() {}\nfn source_second() {}\n",
        ),
        added_pair(
            "commit-one",
            "commit-one-a",
            "a.rs",
            Language::Rust,
            "fn kind_function() {}\nstruct kind_struct;\nfn ordinal_late() {}\nfn ordinal_early() {}\n",
        ),
    ];
    let second_pairs = vec![added_pair(
        "commit-two",
        "commit-two-b",
        "b.rs",
        Language::Rust,
        "fn final_commit() {}\n",
    )];

    let mut first_diff = diff_declarations(&first_pairs)?;
    force_kind_and_ordinal_ties(&mut first_diff);
    let second_diff = diff_declarations(&second_pairs)?;
    let forward_query = DeclarationReviewQuery::new(vec![
        DeclarationReviewDiffBatch::new(first_pairs.clone(), first_diff.clone()),
        DeclarationReviewDiffBatch::new(second_pairs.clone(), second_diff.clone()),
    ]);
    let forward = collect_declaration_review(&forward_query)?;

    first_pairs.reverse();
    first_diff.units.reverse();
    let reverse_query = DeclarationReviewQuery::new(vec![
        DeclarationReviewDiffBatch::new(first_pairs, first_diff),
        DeclarationReviewDiffBatch::new(second_pairs, second_diff),
    ]);
    let reverse = collect_declaration_review(&reverse_query)?;

    let identity = |collection: &trueflow::declaration::review::CollectedDeclarationReview| {
        collection
            .items
            .iter()
            .map(|item| {
                (
                    item.snapshot_pair_id.as_str().to_string(),
                    item.display_path.to_string(),
                    item.declaration.name.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let ordered = identity(&forward);
    assert_eq!(
        ordered,
        identity(&reverse),
        "input permutations must not alter review order"
    );

    let first_commit_end = ordered
        .iter()
        .position(|(pair, _, _)| pair == "commit-two")
        .context("missing second comparison pair")?;
    assert!(
        ordered[..first_commit_end]
            .iter()
            .all(|(pair, _, _)| pair == "commit-one")
    );
    assert!(
        ordered[..first_commit_end]
            .windows(2)
            .all(|window| window[0].1 <= window[1].1)
    );

    let source_first = ordered
        .iter()
        .position(|(_, _, name)| name == "source_first")
        .context("missing source_first")?;
    let source_second = ordered
        .iter()
        .position(|(_, _, name)| name == "source_second")
        .context("missing source_second")?;
    assert!(source_first < source_second);

    let ordinal_early = ordered
        .iter()
        .position(|(_, _, name)| name == "ordinal_early")
        .context("missing ordinal_early")?;
    let ordinal_late = ordered
        .iter()
        .position(|(_, _, name)| name == "ordinal_late")
        .context("missing ordinal_late")?;
    assert!(ordinal_early < ordinal_late);
    Ok(())
}

#[test]
fn repeated_and_pr_launches_preserve_mode_trust_and_commit_order() -> Result<()> {
    let config = parse_config("[tui]\nmode = \"declarations\"\n")?;
    let args = parse_tui(&["trueflow", "tui", "--trust-lsp-workspace"])?;
    let direct = resolve_launch(&config, &args)?;

    let repeated = direct.for_scope(ScopePreset::MainDiff);
    assert_eq!(repeated.mode, TuiReviewMode::Declarations);
    assert!(repeated.trust_lsp_workspace);

    let commits = vec![
        PullRequestCommit {
            sha: CommitId::new("1111111")?,
            summary: "first".to_string(),
        },
        PullRequestCommit {
            sha: CommitId::new("2222222")?,
            summary: "second".to_string(),
        },
        PullRequestCommit {
            sha: CommitId::new("3333333")?,
            summary: "third".to_string(),
        },
    ];
    let queued = build_pull_request_launch_queue(&direct, &commits)?;
    assert!(queued.iter().all(|launch| {
        launch.mode == TuiReviewMode::Declarations && launch.trust_lsp_workspace
    }));
    let queued_ids = queued
        .iter()
        .map(|launch| match &launch.scope {
            ScopePreset::Commit { id, .. } => Ok(id.as_str()),
            other => anyhow::bail!("expected commit launch, got {other:?}"),
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(queued_ids, ["1111111", "2222222", "3333333"]);
    Ok(())
}
