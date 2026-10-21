use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::repo_path::RepoPath;
use crate::store::{
    DeclarationRecordLocator, Record, ReviewCheck, ReviewTargetRef, ReviewedDeclarationSnapshot,
    Verdict,
};

use super::diff::{DeclarationChangeKind, DeclarationDiffUnit, DeclarationMatch};
use super::review::DeclarationReviewDiffBatch;
use super::snapshot::{PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot};
use super::{DeclarationId, DeclarationKey, DeclarationNode, DeclarationProjectionHash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageBindingKind {
    ExactLocator,
    SamePathKeyHash,
    ProvenRenameMatch,
    UniqueKeyHash,
    UniqueHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationCoverageBinding {
    record_index: usize,
    kind: CoverageBindingKind,
    verdict: Verdict,
}

impl DeclarationCoverageBinding {
    pub fn record_index(&self) -> usize {
        self.record_index
    }

    pub fn kind(&self) -> CoverageBindingKind {
        self.kind
    }

    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OccurrenceKey {
    snapshot_pair_id: SnapshotPairId,
    snapshot_id: SnapshotId,
    declaration_id: DeclarationId,
}

#[derive(Debug, Clone)]
struct CandidateOccurrence {
    key: OccurrenceKey,
    change: ReviewedChange,
    locator: DeclarationRecordLocator,
    rename_sources: Vec<DeclarationRecordLocator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateMatch {
    Unique(usize),
    Ambiguous,
}

/// Which change a declaration review covers. A record never binds an occurrence of the other
/// kind: approving a surface when it was added or changed says nothing about removing it, and
/// approving its removal says nothing about a later re-addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReviewedChange {
    Surface,
    Deletion,
}

impl ReviewedChange {
    fn of_record(record: &Record) -> Self {
        match record.declaration_deletion {
            Some(_) => Self::Deletion,
            None => Self::Surface,
        }
    }

    fn of_unit(unit: &DeclarationDiffUnit) -> Self {
        match unit.change_kind {
            DeclarationChangeKind::Added | DeclarationChangeKind::Changed => Self::Surface,
            DeclarationChangeKind::Deleted => Self::Deletion,
        }
    }
}

/// One surface at one path, by semantic key and projection: the `SamePathKeyHash` tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SurfaceKey<'a> {
    path: &'a RepoPath,
    declaration_key: &'a DeclarationKey,
    projection_hash: &'a DeclarationProjectionHash,
}

impl<'a> SurfaceKey<'a> {
    fn of_locator(locator: &'a DeclarationRecordLocator) -> Self {
        Self {
            path: &locator.path,
            declaration_key: &locator.declaration_key,
            projection_hash: &locator.projection_hash,
        }
    }
}

/// Revision-independent identity of a declaration review: the surface coverage binds at its
/// `SamePathKeyHash` tier plus the reviewed change kind coverage requires to match. Feedback
/// export keys latest verdicts on it, so a later approval resolves an earlier comment exactly
/// when the TUI treats it as reviewed, whichever snapshot either record was captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeclarationSurfaceIdentity<'a> {
    change: ReviewedChange,
    surface: SurfaceKey<'a>,
}

impl<'a> DeclarationSurfaceIdentity<'a> {
    /// `None` for records without a declaration locator.
    pub fn of_record(record: &'a Record) -> Option<Self> {
        record.declaration_locator.as_ref().map(|locator| Self {
            change: ReviewedChange::of_record(record),
            surface: SurfaceKey::of_locator(locator),
        })
    }
}

struct CandidateLookup<'a> {
    exact: HashMap<&'a DeclarationRecordLocator, CandidateMatch>,
    same_path: HashMap<SurfaceKey<'a>, CandidateMatch>,
    rename: HashMap<SurfaceKey<'a>, CandidateMatch>,
    key_hash: HashMap<(&'a DeclarationKey, &'a DeclarationProjectionHash), CandidateMatch>,
    hash: HashMap<&'a DeclarationProjectionHash, CandidateMatch>,
}

impl<'a> CandidateLookup<'a> {
    fn new(candidates: &'a [CandidateOccurrence]) -> Self {
        let mut lookup = Self {
            exact: HashMap::with_capacity(candidates.len()),
            same_path: HashMap::with_capacity(candidates.len()),
            rename: HashMap::new(),
            key_hash: HashMap::with_capacity(candidates.len()),
            hash: HashMap::with_capacity(candidates.len()),
        };
        for (index, candidate) in candidates.iter().enumerate() {
            let locator = &candidate.locator;
            insert_match(&mut lookup.exact, locator, index);
            insert_match(
                &mut lookup.same_path,
                SurfaceKey::of_locator(locator),
                index,
            );
            for source in &candidate.rename_sources {
                insert_match(&mut lookup.rename, SurfaceKey::of_locator(source), index);
            }
            insert_match(
                &mut lookup.key_hash,
                (&locator.declaration_key, &locator.projection_hash),
                index,
            );
            insert_match(&mut lookup.hash, &locator.projection_hash, index);
        }
        lookup
    }

    /// Records are validated first, so the locator's projection hash is the target hash.
    fn find(
        &self,
        locator: &DeclarationRecordLocator,
    ) -> Option<(CoverageBindingKind, CandidateMatch)> {
        let surface = SurfaceKey::of_locator(locator);
        let hash = &locator.projection_hash;
        // An ambiguous stronger tier is still a match: never fall through it.
        self.exact
            .get(locator)
            .map(|matched| (CoverageBindingKind::ExactLocator, *matched))
            .or_else(|| {
                self.same_path
                    .get(&surface)
                    .map(|matched| (CoverageBindingKind::SamePathKeyHash, *matched))
            })
            .or_else(|| {
                self.rename
                    .get(&surface)
                    .map(|matched| (CoverageBindingKind::ProvenRenameMatch, *matched))
            })
            .or_else(|| {
                self.key_hash
                    .get(&(&locator.declaration_key, hash))
                    .map(|matched| (CoverageBindingKind::UniqueKeyHash, *matched))
            })
            .or_else(|| {
                self.hash
                    .get(hash)
                    .map(|matched| (CoverageBindingKind::UniqueHash, *matched))
            })
    }
}

fn insert_match<K: Eq + std::hash::Hash>(
    index: &mut HashMap<K, CandidateMatch>,
    key: K,
    candidate_index: usize,
) {
    index
        .entry(key)
        .and_modify(|matched| {
            // Distinct rename locators can share a relaxed key for one occurrence.
            if *matched != CandidateMatch::Unique(candidate_index) {
                *matched = CandidateMatch::Ambiguous;
            }
        })
        .or_insert(CandidateMatch::Unique(candidate_index));
}

#[derive(Debug, Clone, Default)]
pub struct DeclarationCoverageIndex {
    bindings: HashMap<OccurrenceKey, DeclarationCoverageBinding>,
    public_keys: HashMap<(SnapshotPairId, DeclarationId), Vec<OccurrenceKey>>,
}

impl DeclarationCoverageIndex {
    pub fn build(batches: &[DeclarationReviewDiffBatch], records: &[Record]) -> Result<Self> {
        let candidates = collect_candidates(batches)?;
        let lookup = CandidateLookup::new(&candidates);
        let mut bindings = HashMap::<OccurrenceKey, DeclarationCoverageBinding>::new();

        for (record_index, record) in records.iter().enumerate() {
            if !matches!(record.target, ReviewTargetRef::Declaration { .. }) {
                continue;
            }
            record
                .validate()
                .with_context(|| format!("invalid declaration coverage record {}", record.id))?;
            if record.check.as_str() != ReviewCheck::declaration().as_str() {
                bail!(
                    "declaration coverage record {} does not use the declaration review check",
                    record.id
                );
            }
            let locator = record.declaration_locator.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration coverage record {} has no declaration locator",
                    record.id
                )
            })?;

            let Some((kind, CandidateMatch::Unique(candidate_index))) = lookup.find(locator) else {
                continue;
            };
            let candidate = &candidates[candidate_index];
            // The record still identifies its strongest occurrence, so it never falls through
            // to a weaker tier; it just covers nothing when that occurrence is the other change.
            if candidate.change != ReviewedChange::of_record(record) {
                continue;
            }
            let replacement = DeclarationCoverageBinding {
                record_index,
                kind,
                verdict: record.verdict.clone(),
            };
            let should_replace = bindings.get(&candidate.key).is_none_or(|current| {
                let current_record = &records[current.record_index];
                record.timestamp > current_record.timestamp
                    || (record.timestamp == current_record.timestamp
                        && record_index > current.record_index)
            });
            if should_replace {
                bindings.insert(candidate.key.clone(), replacement);
            }
        }

        let mut public_keys = HashMap::<(SnapshotPairId, DeclarationId), Vec<OccurrenceKey>>::new();
        for candidate in &candidates {
            public_keys
                .entry((
                    candidate.key.snapshot_pair_id.clone(),
                    candidate.key.declaration_id.clone(),
                ))
                .or_default()
                .push(candidate.key.clone());
        }

        Ok(Self {
            bindings,
            public_keys,
        })
    }

    pub fn binding_for(
        &self,
        snapshot_pair_id: &SnapshotPairId,
        declaration_id: &DeclarationId,
    ) -> Option<&DeclarationCoverageBinding> {
        let occurrences = self
            .public_keys
            .get(&(snapshot_pair_id.clone(), declaration_id.clone()))?;
        if occurrences.len() != 1 {
            return None;
        }
        self.bindings.get(&occurrences[0])
    }
}

fn collect_candidates(batches: &[DeclarationReviewDiffBatch]) -> Result<Vec<CandidateOccurrence>> {
    let mut candidates = Vec::<CandidateOccurrence>::new();
    let mut candidate_indices = HashMap::<OccurrenceKey, usize>::new();

    for (batch_index, batch) in batches.iter().enumerate() {
        for unit in &batch.diff().units {
            let pair = resolve_unit_pair(batch_index, batch.snapshot_pairs(), unit)?;
            let (snapshot, declaration) = unit_display_endpoint(unit, pair)?;
            insert_candidate(
                &mut candidates,
                &mut candidate_indices,
                candidate_occurrence(pair, ReviewedChange::of_unit(unit), snapshot, declaration)?,
            )?;
        }

        for declaration_match in &batch.diff().matches {
            let pair = resolve_match_pair(batch_index, batch.snapshot_pairs(), declaration_match)?;
            let head = pair.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration match for pair {} has no head snapshot",
                    pair.id.as_str()
                )
            })?;
            let mut candidate =
                candidate_occurrence(pair, ReviewedChange::Surface, head, &declaration_match.head)?;
            if is_conservative_file_rename(pair, declaration_match) {
                let base = pair.base.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "conservative rename match for pair {} has no base snapshot",
                        pair.id.as_str()
                    )
                })?;
                candidate
                    .rename_sources
                    .push(locator_for(base, &declaration_match.base)?);
            }
            insert_candidate(&mut candidates, &mut candidate_indices, candidate)?;
        }
    }

    Ok(candidates)
}

fn insert_candidate(
    candidates: &mut Vec<CandidateOccurrence>,
    indices: &mut HashMap<OccurrenceKey, usize>,
    candidate: CandidateOccurrence,
) -> Result<()> {
    if let Some(index) = indices.get(&candidate.key).copied() {
        let existing = &mut candidates[index];
        if existing.change != candidate.change || existing.locator != candidate.locator {
            bail!(
                "declaration occurrence {} for pair {} has conflicting projections",
                candidate.key.declaration_id.as_str(),
                candidate.key.snapshot_pair_id.as_str()
            );
        }
        for source in candidate.rename_sources {
            if !existing.rename_sources.contains(&source) {
                existing.rename_sources.push(source);
            }
        }
        return Ok(());
    }

    let index = candidates.len();
    indices.insert(candidate.key.clone(), index);
    candidates.push(candidate);
    Ok(())
}

fn candidate_occurrence(
    pair: &SnapshotPair,
    change: ReviewedChange,
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
) -> Result<CandidateOccurrence> {
    Ok(CandidateOccurrence {
        key: OccurrenceKey {
            snapshot_pair_id: pair.id.clone(),
            snapshot_id: snapshot.id.clone(),
            declaration_id: declaration.id.clone(),
        },
        change,
        locator: locator_for(snapshot, declaration)?,
        rename_sources: Vec::new(),
    })
}

fn locator_for(
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
) -> Result<DeclarationRecordLocator> {
    Ok(DeclarationRecordLocator {
        path: RepoPath::from_relative_path(&snapshot.path)?,
        declaration_key: declaration.key.clone(),
        source_ordinal: declaration.source_ordinal,
        source_span: declaration.source_span.clone(),
        reviewed_snapshot: ReviewedDeclarationSnapshot {
            snapshot_id: snapshot.id.as_str().to_owned(),
            content_hash: snapshot.bytes_hash().clone(),
        },
        projection_hash: declaration.projection_hash.clone(),
    })
}

fn resolve_unit_pair<'a>(
    batch_index: usize,
    pairs: &'a [SnapshotPair],
    unit: &DeclarationDiffUnit,
) -> Result<&'a SnapshotPair> {
    resolve_exact_pair(
        batch_index,
        pairs,
        &unit.snapshot_pair_id,
        |pair| match unit.change_kind {
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
        },
    )
}

fn resolve_match_pair<'a>(
    batch_index: usize,
    pairs: &'a [SnapshotPair],
    declaration_match: &DeclarationMatch,
) -> Result<&'a SnapshotPair> {
    resolve_exact_pair(
        batch_index,
        pairs,
        &declaration_match.snapshot_pair_id,
        |pair| {
            pair.base.as_ref().map(|snapshot| &snapshot.id)
                == Some(&declaration_match.base_snapshot_id)
                && pair.head.as_ref().map(|snapshot| &snapshot.id)
                    == Some(&declaration_match.head_snapshot_id)
        },
    )
}

fn resolve_exact_pair<'a>(
    batch_index: usize,
    pairs: &'a [SnapshotPair],
    pair_id: &SnapshotPairId,
    endpoints_match: impl Fn(&SnapshotPair) -> bool,
) -> Result<&'a SnapshotPair> {
    let mut matches = pairs
        .iter()
        .filter(|pair| pair.id == *pair_id && endpoints_match(pair));
    let Some(pair) = matches.next() else {
        bail!(
            "declaration coverage input in batch {batch_index} references missing exact snapshot pair {}",
            pair_id.as_str()
        );
    };
    if matches.next().is_some() {
        bail!(
            "declaration coverage input in batch {batch_index} ambiguously references snapshot pair {}",
            pair_id.as_str()
        );
    }
    Ok(pair)
}

fn unit_display_endpoint<'a>(
    unit: &'a DeclarationDiffUnit,
    pair: &'a SnapshotPair,
) -> Result<(&'a SourceSnapshot, &'a DeclarationNode)> {
    match unit.change_kind {
        DeclarationChangeKind::Added | DeclarationChangeKind::Changed => Ok((
            pair.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!("declaration unit for pair {} has no head", pair.id.as_str())
            })?,
            unit.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration unit for pair {} has no head projection",
                    pair.id.as_str()
                )
            })?,
        )),
        DeclarationChangeKind::Deleted => Ok((
            pair.base.as_ref().ok_or_else(|| {
                anyhow::anyhow!("declaration unit for pair {} has no base", pair.id.as_str())
            })?,
            unit.base.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration unit for pair {} has no base projection",
                    pair.id.as_str()
                )
            })?,
        )),
    }
}

fn is_conservative_file_rename(pair: &SnapshotPair, declaration_match: &DeclarationMatch) -> bool {
    let (Some(base), Some(head)) = (&pair.base, &pair.head) else {
        return false;
    };
    pair.path_evidence == PathPairEvidence::ExplicitRename
        && declaration_match.path_evidence == PathPairEvidence::ExplicitRename
        && base.path != head.path
        && declaration_match.base.key == declaration_match.head.key
        && declaration_match.base.name == declaration_match.head.name
        && declaration_match.base.kind == declaration_match.head.kind
        && declaration_match.base.projection_hash == declaration_match.head.projection_hash
}
