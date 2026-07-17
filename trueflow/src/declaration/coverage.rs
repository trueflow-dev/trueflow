use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::repo_path::RepoPath;
use crate::store::{
    DeclarationRecordLocator, Record, ReviewCheck, ReviewTargetRef, ReviewedDeclarationSnapshot,
    Verdict,
};

use super::diff::{DeclarationChangeKind, DeclarationMatch, DeclarationDiffUnit};
use super::review::DeclarationReviewDiffBatch;
use super::snapshot::{PathPairEvidence, SnapshotPair, SnapshotPairId, SnapshotId, SourceSnapshot};
use super::{DeclarationId, DeclarationNode};

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
    locator: DeclarationRecordLocator,
    rename_sources: Vec<DeclarationRecordLocator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateMatch {
    None,
    Unique(usize),
    Ambiguous,
}

#[derive(Debug, Clone, Default)]
pub struct DeclarationCoverageIndex {
    bindings: HashMap<OccurrenceKey, DeclarationCoverageBinding>,
    public_keys: HashMap<(SnapshotPairId, DeclarationId), Vec<OccurrenceKey>>,
}

impl DeclarationCoverageIndex {
    pub fn build(batches: &[DeclarationReviewDiffBatch], records: &[Record]) -> Result<Self> {
        let candidates = collect_candidates(batches)?;
        let mut bindings = HashMap::<OccurrenceKey, DeclarationCoverageBinding>::new();

        for (record_index, record) in records.iter().enumerate() {
            let ReviewTargetRef::Declaration { hash } = &record.target else {
                continue;
            };
            record.validate()?;
            if record.check.as_str() != ReviewCheck::declaration().as_str() {
                continue;
            }
            let Some(locator) = record.declaration_locator.as_ref() else {
                continue;
            };

            let tiers = [
                (
                    CoverageBindingKind::ExactLocator,
                    find_candidates(&candidates, |candidate| candidate.locator == *locator),
                ),
                (
                    CoverageBindingKind::SamePathKeyHash,
                    find_candidates(&candidates, |candidate| {
                        candidate.locator.path == locator.path
                            && candidate.locator.declaration_key == locator.declaration_key
                            && candidate.locator.projection_hash == *hash
                    }),
                ),
                (
                    CoverageBindingKind::ProvenRenameMatch,
                    find_candidates(&candidates, |candidate| {
                        candidate.rename_sources.iter().any(|source| {
                            source.path == locator.path
                                && source.declaration_key == locator.declaration_key
                                && source.projection_hash == *hash
                        })
                    }),
                ),
                (
                    CoverageBindingKind::UniqueKeyHash,
                    find_candidates(&candidates, |candidate| {
                        candidate.locator.declaration_key == locator.declaration_key
                            && candidate.locator.projection_hash == *hash
                    }),
                ),
                (
                    CoverageBindingKind::UniqueHash,
                    find_candidates(&candidates, |candidate| {
                        candidate.locator.projection_hash == *hash
                    }),
                ),
            ];

            let selected = tiers
                .into_iter()
                .find(|(_, candidate_match)| *candidate_match != CandidateMatch::None);
            let Some((kind, CandidateMatch::Unique(candidate_index))) = selected else {
                continue;
            };
            let candidate = &candidates[candidate_index];
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

fn find_candidates(
    candidates: &[CandidateOccurrence],
    predicate: impl Fn(&CandidateOccurrence) -> bool,
) -> CandidateMatch {
    let mut matched = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if !predicate(candidate) {
            continue;
        }
        if matched.is_some() {
            return CandidateMatch::Ambiguous;
        }
        matched = Some(index);
    }
    matched.map_or(CandidateMatch::None, CandidateMatch::Unique)
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
                candidate_occurrence(pair, snapshot, declaration)?,
            )?;
        }

        for declaration_match in &batch.diff().matches {
            let pair = resolve_match_pair(
                batch_index,
                batch.snapshot_pairs(),
                declaration_match,
            )?;
            let head = pair.head.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration match for pair {} has no head snapshot",
                    pair.id.as_str()
                )
            })?;
            let mut candidate = candidate_occurrence(pair, head, &declaration_match.head)?;
            if is_conservative_file_rename(pair, declaration_match) {
                let base = pair.base.as_ref().expect("rename match has a base snapshot");
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
        if existing.locator != candidate.locator {
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
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
) -> Result<CandidateOccurrence> {
    Ok(CandidateOccurrence {
        key: OccurrenceKey {
            snapshot_pair_id: pair.id.clone(),
            snapshot_id: snapshot.id.clone(),
            declaration_id: declaration.id.clone(),
        },
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
    resolve_exact_pair(batch_index, pairs, &unit.snapshot_pair_id, |pair| {
        match unit.change_kind {
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
        }
    })
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
