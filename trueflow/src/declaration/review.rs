use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};

use crate::analysis::Language;
use crate::repo_path::RepoPath;
use crate::store::{Record, Verdict};

use super::coverage::DeclarationCoverageIndex;
use super::diff::{
    DeclarationChangeKind, DeclarationDiff, DeclarationDiffUnit, DiffDiagnostic, DiffDiagnosticKind,
};
use super::snapshot::{SnapshotPair, SnapshotPairId, SourceSnapshot};
use super::{Capability, DeclarationId, DeclarationKind, DeclarationNode, capabilities_for};

/// One already-captured comparison and its declaration diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationReviewDiffBatch {
    snapshot_pairs: Vec<SnapshotPair>,
    diff: DeclarationDiff,
}

impl DeclarationReviewDiffBatch {
    pub fn new(snapshot_pairs: Vec<SnapshotPair>, diff: DeclarationDiff) -> Self {
        Self {
            snapshot_pairs,
            diff,
        }
    }

    pub fn snapshot_pairs(&self) -> &[SnapshotPair] {
        &self.snapshot_pairs
    }

    pub fn diff(&self) -> &DeclarationDiff {
        &self.diff
    }
}

/// Inputs needed to collect declaration review targets without re-reading source files.
#[derive(Debug, Clone)]
pub struct DeclarationReviewQuery {
    batches: Vec<DeclarationReviewDiffBatch>,
    reviewed_target_ids: HashSet<DeclarationId>,
    records: Vec<Record>,
}

impl DeclarationReviewQuery {
    pub fn new(batches: Vec<DeclarationReviewDiffBatch>) -> Self {
        Self {
            batches,
            reviewed_target_ids: HashSet::new(),
            records: Vec::new(),
        }
    }

    pub fn with_reviewed_target_ids(mut self, reviewed_target_ids: Vec<DeclarationId>) -> Self {
        self.reviewed_target_ids = reviewed_target_ids.into_iter().collect();
        self
    }

    pub fn with_records(mut self, records: Vec<Record>) -> Self {
        self.records = records;
        self
    }

    pub fn batches(&self) -> &[DeclarationReviewDiffBatch] {
        &self.batches
    }

    pub fn reviewed_target_ids(&self) -> &HashSet<DeclarationId> {
        &self.reviewed_target_ids
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedDeclarationItem {
    pub snapshot_pair_id: SnapshotPairId,
    pub display_path: RepoPath,
    pub snapshot: SourceSnapshot,
    pub declaration: DeclarationNode,
    pub diff_unit: DeclarationDiffUnit,
    pub latest_verdict: Option<Verdict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationReviewStatus {
    Ready,
    NoSurfaceChanges,
    UnsupportedLanguage { languages: Vec<Language> },
    Partial { diagnostic_count: usize },
    FullyReviewed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedDeclarationReview {
    pub status: DeclarationReviewStatus,
    pub items: Vec<CollectedDeclarationItem>,
    pub diagnostics: Vec<DiffDiagnostic>,
    pub canonical_order: Vec<DeclarationId>,
}

struct OrderedItem {
    batch_ordinal: usize,
    item: CollectedDeclarationItem,
}

/// Collect canonical declaration review items from immutable snapshot/diff batches.
///
/// Collection deliberately does not accept block filters: every projected visibility is
/// reviewable. A diff unit must identify exactly one snapshot pair by its pair and endpoint
/// snapshot IDs, so repeated comparison labels and byte-identical captures remain distinct.
pub fn collect_declaration_review(
    query: &DeclarationReviewQuery,
) -> Result<CollectedDeclarationReview> {
    let mut ordered_items = Vec::new();
    let mut diagnostics = Vec::new();
    let mut unsupported_languages = Vec::new();
    let mut reviewable_unit_count = 0usize;
    let coverage = DeclarationCoverageIndex::build(&query.batches, &query.records)?;

    for (batch_ordinal, batch) in query.batches.iter().enumerate() {
        diagnostics.extend(batch.diff.diagnostics.iter().cloned());
        collect_unsupported_languages(batch, &mut unsupported_languages);

        for unit in &batch.diff.units {
            let pair = resolve_snapshot_pair(batch_ordinal, &batch.snapshot_pairs, unit)?;
            let (snapshot, declaration) = display_side(batch_ordinal, unit, pair)?;

            if !unit.review_target {
                continue;
            }
            reviewable_unit_count += 1;

            if query.reviewed_target_ids.contains(&declaration.id) {
                continue;
            }

            let latest_verdict = coverage
                .binding_for(&unit.snapshot_pair_id, &declaration.id)
                .map(|binding| binding.verdict().clone());
            if latest_verdict == Some(Verdict::Approved) {
                continue;
            }

            ordered_items.push(OrderedItem {
                batch_ordinal,
                item: CollectedDeclarationItem {
                    snapshot_pair_id: unit.snapshot_pair_id.clone(),
                    display_path: RepoPath::from_relative_path(&snapshot.path).with_context(
                        || {
                            format!(
                                "snapshot path {} is not a valid repository-relative display path",
                                snapshot.path.display()
                            )
                        },
                    )?,
                    snapshot: snapshot.clone(),
                    declaration: declaration.clone(),
                    diff_unit: unit.clone(),
                    latest_verdict,
                },
            });
        }
    }

    ordered_items.sort_by(|left, right| {
        left.batch_ordinal
            .cmp(&right.batch_ordinal)
            .then_with(|| left.item.display_path.cmp(&right.item.display_path))
            .then_with(|| {
                left.item
                    .declaration
                    .source_span
                    .start
                    .cmp(&right.item.declaration.source_span.start)
            })
            .then_with(|| {
                declaration_kind_priority(left.item.declaration.kind)
                    .cmp(&declaration_kind_priority(right.item.declaration.kind))
            })
            .then_with(|| {
                left.item
                    .declaration
                    .source_ordinal
                    .cmp(&right.item.declaration.source_ordinal)
            })
            .then_with(|| left.item.declaration.id.cmp(&right.item.declaration.id))
            .then_with(|| left.item.snapshot.id.cmp(&right.item.snapshot.id))
    });

    unsupported_languages.sort_unstable_by_key(|language| language_sort_key(*language));
    unsupported_languages.dedup();

    let items = ordered_items
        .into_iter()
        .map(|ordered| ordered.item)
        .collect::<Vec<_>>();
    let canonical_order = items
        .iter()
        .map(|item| item.declaration.id.clone())
        .collect();
    let status = if !unsupported_languages.is_empty() {
        DeclarationReviewStatus::UnsupportedLanguage {
            languages: unsupported_languages,
        }
    } else if !diagnostics.is_empty() {
        DeclarationReviewStatus::Partial {
            diagnostic_count: diagnostics.len(),
        }
    } else if !items.is_empty() {
        DeclarationReviewStatus::Ready
    } else if reviewable_unit_count > 0 {
        DeclarationReviewStatus::FullyReviewed
    } else {
        DeclarationReviewStatus::NoSurfaceChanges
    };

    Ok(CollectedDeclarationReview {
        status,
        items,
        diagnostics,
        canonical_order,
    })
}

fn resolve_snapshot_pair<'a>(
    batch_ordinal: usize,
    snapshot_pairs: &'a [SnapshotPair],
    unit: &DeclarationDiffUnit,
) -> Result<&'a SnapshotPair> {
    let mut resolved = None;
    let mut resolved_count = 0usize;
    let mut pair_id_count = 0usize;

    for pair in snapshot_pairs {
        if pair.id != unit.snapshot_pair_id {
            continue;
        }
        pair_id_count += 1;
        let endpoints_match = match unit.change_kind {
            DeclarationChangeKind::Added => {
                pair.head.as_ref().map(|snapshot| &snapshot.id) == unit.head_snapshot_id.as_ref()
            }
            DeclarationChangeKind::Changed => {
                pair.base.as_ref().map(|snapshot| &snapshot.id) == unit.base_snapshot_id.as_ref()
                    && pair.head.as_ref().map(|snapshot| &snapshot.id)
                        == unit.head_snapshot_id.as_ref()
            }
            DeclarationChangeKind::Deleted => {
                pair.base.as_ref().map(|snapshot| &snapshot.id) == unit.base_snapshot_id.as_ref()
            }
        };
        if endpoints_match {
            resolved_count += 1;
            resolved = Some(pair);
        }
    }

    match (resolved_count, resolved) {
        (1, Some(pair)) => Ok(pair),
        (0, _) if pair_id_count == 0 => bail!(
            "declaration diff unit in batch {batch_ordinal} references missing snapshot pair {}",
            unit.snapshot_pair_id.as_str()
        ),
        (0, _) => bail!(
            "declaration diff unit for pair {} in batch {batch_ordinal} does not match any supplied pair's endpoint snapshot IDs",
            unit.snapshot_pair_id.as_str()
        ),
        (count, _) => bail!(
            "declaration diff unit for pair {} in batch {batch_ordinal} resolves to {count} supplied snapshot pairs; expected exactly one",
            unit.snapshot_pair_id.as_str()
        ),
    }
}

fn display_side<'a>(
    batch_ordinal: usize,
    unit: &'a DeclarationDiffUnit,
    pair: &'a SnapshotPair,
) -> Result<(&'a SourceSnapshot, &'a DeclarationNode)> {
    match unit.change_kind {
        DeclarationChangeKind::Added => {
            ensure!(
                unit.base.is_none(),
                "added declaration diff unit for pair {} in batch {batch_ordinal} unexpectedly retains a base declaration",
                unit.snapshot_pair_id.as_str()
            );
            let snapshot = pair.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "added declaration diff unit for pair {} in batch {batch_ordinal} has no head snapshot",
                    unit.snapshot_pair_id.as_str()
                )
            })?;
            let declaration = unit.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "added declaration diff unit for pair {} in batch {batch_ordinal} has no head declaration",
                    unit.snapshot_pair_id.as_str()
                )
            })?;
            Ok((snapshot, declaration))
        }
        DeclarationChangeKind::Changed => {
            ensure!(
                unit.base.is_some(),
                "changed declaration diff unit for pair {} in batch {batch_ordinal} has no base declaration",
                unit.snapshot_pair_id.as_str()
            );
            let snapshot = pair.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "changed declaration diff unit for pair {} in batch {batch_ordinal} has no head snapshot",
                    unit.snapshot_pair_id.as_str()
                )
            })?;
            let declaration = unit.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "changed declaration diff unit for pair {} in batch {batch_ordinal} has no head declaration",
                    unit.snapshot_pair_id.as_str()
                )
            })?;
            Ok((snapshot, declaration))
        }
        DeclarationChangeKind::Deleted => {
            ensure!(
                unit.head.is_none(),
                "deleted declaration diff unit for pair {} in batch {batch_ordinal} unexpectedly retains a head declaration",
                unit.snapshot_pair_id.as_str()
            );
            let snapshot = pair.base.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "deleted declaration diff unit for pair {} in batch {batch_ordinal} has no base snapshot",
                    unit.snapshot_pair_id.as_str()
                )
            })?;
            let declaration = unit.base.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "deleted declaration diff unit for pair {} in batch {batch_ordinal} has no base declaration",
                    unit.snapshot_pair_id.as_str()
                )
            })?;
            Ok((snapshot, declaration))
        }
    }
}

fn collect_unsupported_languages(
    batch: &DeclarationReviewDiffBatch,
    unsupported_languages: &mut Vec<Language>,
) {
    let has_no_projector_diagnostic = batch.diff.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiffDiagnosticKind::ProjectionDiagnostic
            && diagnostic.message.contains("no declaration projector")
    });

    for snapshot in batch
        .snapshot_pairs
        .iter()
        .flat_map(|pair| pair.base.iter().chain(pair.head.iter()))
    {
        let inventory = capabilities_for(snapshot.language).inventory;
        let explicitly_unsupported = matches!(&inventory, Capability::Unsupported { .. });
        let projector_unavailable =
            has_no_projector_diagnostic && !matches!(&inventory, Capability::Complete);
        if explicitly_unsupported || projector_unavailable {
            unsupported_languages.push(snapshot.language);
        }
    }
}

const fn declaration_kind_priority(kind: DeclarationKind) -> u8 {
    match kind {
        DeclarationKind::Function => 0,
        DeclarationKind::Method => 1,
        DeclarationKind::Struct => 2,
        DeclarationKind::Enum => 3,
        DeclarationKind::Trait => 4,
        DeclarationKind::Interface => 5,
        DeclarationKind::Class => 6,
        DeclarationKind::TypeAlias => 7,
        DeclarationKind::AssociatedType => 8,
        DeclarationKind::Constant => 9,
        DeclarationKind::Static => 10,
        DeclarationKind::Module => 11,
        DeclarationKind::Constructor => 12,
        DeclarationKind::Destructor => 13,
        DeclarationKind::Operator => 14,
        DeclarationKind::Property => 15,
        DeclarationKind::Macro => 16,
    }
}

const fn language_sort_key(language: Language) -> &'static str {
    match language {
        Language::Rust => "Rust",
        Language::Swift => "Swift",
        Language::Elisp => "Elisp",
        Language::JavaScript => "JavaScript",
        Language::TypeScript => "TypeScript",
        Language::Java => "Java",
        Language::Kotlin => "Kotlin",
        Language::CSharp => "CSharp",
        Language::Python => "Python",
        Language::Ruby => "Ruby",
        Language::Php => "Php",
        Language::Go => "Go",
        Language::C => "C",
        Language::Cpp => "Cpp",
        Language::Zig => "Zig",
        Language::Lua => "Lua",
        Language::Dart => "Dart",
        Language::Scala => "Scala",
        Language::Haskell => "Haskell",
        Language::OCaml => "OCaml",
        Language::Elixir => "Elixir",
        Language::Clojure => "Clojure",
        Language::Sql => "Sql",
        Language::Yaml => "Yaml",
        Language::Json => "Json",
        Language::Html => "Html",
        Language::Css => "Css",
        Language::Shell => "Shell",
        Language::Markdown => "Markdown",
        Language::Toml => "Toml",
        Language::Nix => "Nix",
        Language::Just => "Just",
        Language::Text => "Text",
        Language::Unknown => "Unknown",
    }
}
