use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::snapshot::{PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId};
use super::{DeclarationNode, SourceComponentRole, project_source};

/// Why two declaration projections were considered to represent the same declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MatchingEvidence {
    /// The conservative declaration keys were mutually unique.
    ExactKey,
    /// A projection was mutually unique within a duplicate key/name group.
    ExactProjection,
    /// Exactly one same-name declaration of the compatible kind remained on each side.
    UniqueCompatible,
    /// Exactly one compatible declaration remained after eliding its declared name.
    UniqueNameElided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeclarationChangeKind {
    Added,
    Deleted,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffDiagnosticKind {
    AmbiguousDeclarationMatch,
    ProjectionDiagnostic,
    PathEvidenceMismatch,
    IncompatibleSnapshotLanguages,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffDiagnostic {
    pub snapshot_pair_id: SnapshotPairId,
    pub kind: DiffDiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationMatch {
    pub snapshot_pair_id: SnapshotPairId,
    pub base_snapshot_id: SnapshotId,
    pub head_snapshot_id: SnapshotId,
    pub base: DeclarationNode,
    pub head: DeclarationNode,
    pub evidence: MatchingEvidence,
    pub path_evidence: PathPairEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationDiffUnit {
    pub snapshot_pair_id: SnapshotPairId,
    pub base_snapshot_id: Option<SnapshotId>,
    pub head_snapshot_id: Option<SnapshotId>,
    pub base: Option<DeclarationNode>,
    pub head: Option<DeclarationNode>,
    pub change_kind: DeclarationChangeKind,
    pub review_target: bool,
    pub matching_evidence: Option<MatchingEvidence>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationDiff {
    pub units: Vec<DeclarationDiffUnit>,
    pub matches: Vec<DeclarationMatch>,
    pub diagnostics: Vec<DiffDiagnostic>,
}

#[derive(Debug, Clone, Copy)]
struct ReservedMatch {
    base: usize,
    head: usize,
    evidence: MatchingEvidence,
}

/// Project and diff each snapshot pair independently, in the order supplied.
///
/// Matching never uses executable body text and never crosses pair boundaries.
/// A matched declaration whose projection is unchanged remains in `matches`, but
/// does not become a review unit.
pub fn diff_declarations(snapshot_pairs: &[SnapshotPair]) -> Result<DeclarationDiff> {
    let mut diff = DeclarationDiff::default();
    for pair in snapshot_pairs {
        diff_pair(pair, &mut diff)?;
    }
    Ok(diff)
}

fn diff_pair(pair: &SnapshotPair, diff: &mut DeclarationDiff) -> Result<()> {
    let base_facts = pair
        .base
        .as_ref()
        .map(|snapshot| project_source(&snapshot.path, snapshot.language, snapshot.source()))
        .transpose()?;
    let head_facts = pair
        .head
        .as_ref()
        .map(|snapshot| project_source(&snapshot.path, snapshot.language, snapshot.source()))
        .transpose()?;

    for diagnostic in base_facts
        .iter()
        .flat_map(|facts| facts.diagnostics.iter())
        .chain(head_facts.iter().flat_map(|facts| facts.diagnostics.iter()))
    {
        diff.diagnostics.push(DiffDiagnostic {
            snapshot_pair_id: pair.id.clone(),
            kind: DiffDiagnosticKind::ProjectionDiagnostic,
            message: diagnostic.message.clone(),
        });
    }

    let base = base_facts
        .as_ref()
        .map_or(&[][..], |facts| facts.declarations());
    let head = head_facts
        .as_ref()
        .map_or(&[][..], |facts| facts.declarations());

    let path_allows_matching = matching_paths_are_proven(pair, diff);
    let languages_match = match (&pair.base, &pair.head) {
        (Some(base_snapshot), Some(head_snapshot)) => {
            if base_snapshot.language != head_snapshot.language {
                diff.diagnostics.push(DiffDiagnostic {
                    snapshot_pair_id: pair.id.clone(),
                    kind: DiffDiagnosticKind::IncompatibleSnapshotLanguages,
                    message: format!(
                        "cannot match declaration projections for {:?} and {:?}",
                        base_snapshot.language, head_snapshot.language
                    ),
                });
                false
            } else {
                true
            }
        }
        _ => true,
    };

    let reserved = if path_allows_matching && languages_match {
        match_declarations(base, head)
    } else {
        Vec::new()
    };

    let mut base_matches = vec![None; base.len()];
    let mut head_matches = vec![None; head.len()];
    for (match_index, reserved_match) in reserved.iter().enumerate() {
        base_matches[reserved_match.base] = Some(match_index);
        head_matches[reserved_match.head] = Some(match_index);
    }

    if path_allows_matching
        && languages_match
        && has_ambiguous_candidates(base, head, &base_matches, &head_matches, &reserved)
    {
        diff.diagnostics.push(DiffDiagnostic {
            snapshot_pair_id: pair.id.clone(),
            kind: DiffDiagnosticKind::AmbiguousDeclarationMatch,
            message: "declaration candidates remained ambiguous after conservative matching"
                .to_owned(),
        });
    }

    let base_snapshot_id = pair.base.as_ref().map(|snapshot| snapshot.id.clone());
    let head_snapshot_id = pair.head.as_ref().map(|snapshot| snapshot.id.clone());

    // Head order is canonical for additions and changed declarations.
    for (head_index, head_declaration) in head.iter().enumerate() {
        if let Some(match_index) = head_matches[head_index] {
            let matched = reserved.get(match_index).ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration diff for pair {} references missing reserved match {match_index}",
                    pair.id.as_str()
                )
            })?;
            let base_declaration = base.get(matched.base).ok_or_else(|| {
                anyhow::anyhow!(
                    "declaration diff for pair {} references missing base declaration {}",
                    pair.id.as_str(),
                    matched.base
                )
            })?;
            let (Some(matched_base_snapshot_id), Some(matched_head_snapshot_id)) =
                (&base_snapshot_id, &head_snapshot_id)
            else {
                anyhow::bail!(
                    "matched declaration for pair {} requires both base and head snapshots",
                    pair.id.as_str()
                );
            };
            diff.matches.push(DeclarationMatch {
                snapshot_pair_id: pair.id.clone(),
                base_snapshot_id: matched_base_snapshot_id.clone(),
                head_snapshot_id: matched_head_snapshot_id.clone(),
                base: declaration_for_diff(base_declaration),
                head: declaration_for_diff(head_declaration),
                evidence: matched.evidence,
                path_evidence: pair.path_evidence,
            });
            if base_declaration.projection_hash != head_declaration.projection_hash {
                diff.units.push(DeclarationDiffUnit {
                    snapshot_pair_id: pair.id.clone(),
                    base_snapshot_id: base_snapshot_id.clone(),
                    head_snapshot_id: head_snapshot_id.clone(),
                    base: Some(declaration_for_diff(base_declaration)),
                    head: Some(declaration_for_diff(head_declaration)),
                    change_kind: DeclarationChangeKind::Changed,
                    review_target: true,
                    matching_evidence: Some(matched.evidence),
                });
            }
        } else {
            diff.units.push(DeclarationDiffUnit {
                snapshot_pair_id: pair.id.clone(),
                base_snapshot_id: None,
                head_snapshot_id: head_snapshot_id.clone(),
                base: None,
                head: Some(declaration_for_diff(head_declaration)),
                change_kind: DeclarationChangeKind::Added,
                review_target: true,
                matching_evidence: None,
            });
        }
    }

    // Deletions retain base source order and follow the head-ordered units.
    for (base_index, base_declaration) in base.iter().enumerate() {
        if base_matches[base_index].is_none() {
            diff.units.push(DeclarationDiffUnit {
                snapshot_pair_id: pair.id.clone(),
                base_snapshot_id: base_snapshot_id.clone(),
                head_snapshot_id: None,
                base: Some(declaration_for_diff(base_declaration)),
                head: None,
                change_kind: DeclarationChangeKind::Deleted,
                review_target: true,
                matching_evidence: None,
            });
        }
    }

    Ok(())
}

fn matching_paths_are_proven(pair: &SnapshotPair, diff: &mut DeclarationDiff) -> bool {
    let (Some(base), Some(head)) = (&pair.base, &pair.head) else {
        return false;
    };
    let same_path = base.path == head.path;
    match pair.path_evidence {
        PathPairEvidence::ExplicitRename => true,
        PathPairEvidence::SamePath if same_path => true,
        PathPairEvidence::SamePath | PathPairEvidence::Unmatched => {
            if pair.path_evidence == PathPairEvidence::SamePath {
                diff.diagnostics.push(DiffDiagnostic {
                    snapshot_pair_id: pair.id.clone(),
                    kind: DiffDiagnosticKind::PathEvidenceMismatch,
                    message: format!(
                        "same-path evidence does not match endpoint paths {} and {}",
                        base.path.display(),
                        head.path.display()
                    ),
                });
            }
            false
        }
    }
}

fn match_declarations(base: &[DeclarationNode], head: &[DeclarationNode]) -> Vec<ReservedMatch> {
    let mut reserved = Vec::new();
    let mut base_used = vec![false; base.len()];
    let mut head_used = vec![false; head.len()];

    reserve_unique(
        base,
        head,
        &mut base_used,
        &mut head_used,
        &mut reserved,
        MatchingEvidence::ExactKey,
        |left, right, _| left.key == right.key,
    );
    loop {
        let matched_before = reserved.len();
        reserve_unique(
            base,
            head,
            &mut base_used,
            &mut head_used,
            &mut reserved,
            MatchingEvidence::ExactProjection,
            |left, right, prior| {
                same_named_group(left, right, base, head, prior)
                    && left.projection_hash == right.projection_hash
            },
        );
        reserve_unique(
            base,
            head,
            &mut base_used,
            &mut head_used,
            &mut reserved,
            MatchingEvidence::UniqueCompatible,
            |left, right, prior| same_named_group(left, right, base, head, prior),
        );
        reserve_unique(
            base,
            head,
            &mut base_used,
            &mut head_used,
            &mut reserved,
            MatchingEvidence::UniqueNameElided,
            |left, right, prior| {
                left.kind == right.kind
                    && parents_compatible(left, right, base, head, prior)
                    && name_elided_discriminator(left) == name_elided_discriminator(right)
            },
        );
        if reserved.len() == matched_before {
            break;
        }
    }

    reserved
}

fn reserve_unique<F>(
    base: &[DeclarationNode],
    head: &[DeclarationNode],
    base_used: &mut [bool],
    head_used: &mut [bool],
    reserved: &mut Vec<ReservedMatch>,
    evidence: MatchingEvidence,
    compatible: F,
) where
    F: Fn(&DeclarationNode, &DeclarationNode, &[ReservedMatch]) -> bool,
{
    let mut candidates = Vec::new();
    let mut base_counts = HashMap::<usize, usize>::new();
    let mut head_counts = HashMap::<usize, usize>::new();

    for (base_index, base_declaration) in base.iter().enumerate() {
        if base_used[base_index] {
            continue;
        }
        for (head_index, head_declaration) in head.iter().enumerate() {
            if !head_used[head_index] && compatible(base_declaration, head_declaration, reserved) {
                candidates.push((base_index, head_index));
                *base_counts.entry(base_index).or_default() += 1;
                *head_counts.entry(head_index).or_default() += 1;
            }
        }
    }

    for (base_index, head_index) in candidates {
        if base_counts.get(&base_index) == Some(&1) && head_counts.get(&head_index) == Some(&1) {
            base_used[base_index] = true;
            head_used[head_index] = true;
            reserved.push(ReservedMatch {
                base: base_index,
                head: head_index,
                evidence,
            });
        }
    }
}

fn same_named_group(
    left: &DeclarationNode,
    right: &DeclarationNode,
    base: &[DeclarationNode],
    head: &[DeclarationNode],
    prior: &[ReservedMatch],
) -> bool {
    left.kind == right.kind
        && left.name == right.name
        && parents_compatible(left, right, base, head, prior)
}

fn parents_compatible(
    left: &DeclarationNode,
    right: &DeclarationNode,
    base: &[DeclarationNode],
    head: &[DeclarationNode],
    prior: &[ReservedMatch],
) -> bool {
    match (&left.parent_part, &right.parent_part) {
        (None, None) => true,
        (Some(left_parent), Some(right_parent)) => prior.iter().any(|matched| {
            base[matched.base].id == *left_parent && head[matched.head].id == *right_parent
        }),
        _ => false,
    }
}

fn name_elided_discriminator(declaration: &DeclarationNode) -> String {
    let mut discriminator = String::new();
    let mut name_elided = false;
    for component in &declaration.components {
        if component.role == SourceComponentRole::Documentation {
            continue;
        }
        let text = if name_elided {
            component.text.clone()
        } else if let Some(position) = component.text.find(&declaration.name) {
            name_elided = true;
            let mut normalized = component.text.clone();
            normalized.replace_range(position..position + declaration.name.len(), "<name>");
            normalized
        } else {
            component.text.clone()
        };
        discriminator.push_str(component.role.protocol_tag());
        discriminator.push(':');
        discriminator.push_str(&text.len().to_string());
        discriminator.push(':');
        discriminator.push_str(&text);
    }
    discriminator
}

fn has_ambiguous_candidates(
    base: &[DeclarationNode],
    head: &[DeclarationNode],
    base_matches: &[Option<usize>],
    head_matches: &[Option<usize>],
    prior: &[ReservedMatch],
) -> bool {
    base.iter().enumerate().any(|(base_index, left)| {
        base_matches[base_index].is_none()
            && head.iter().enumerate().any(|(head_index, right)| {
                head_matches[head_index].is_none()
                    && left.kind == right.kind
                    && parents_compatible(left, right, base, head, prior)
                    && (left.name == right.name
                        || name_elided_discriminator(left) == name_elided_discriminator(right))
            })
    })
}

fn declaration_for_diff(declaration: &DeclarationNode) -> DeclarationNode {
    let mut declaration = declaration.clone();
    let mut projection_text = String::new();
    let mut trim_following_indent = false;
    for component in &declaration.components {
        if component.role == SourceComponentRole::Layout {
            if let Some(newline) = component.text.rfind('\n') {
                projection_text.push_str(&component.text[..=newline]);
                trim_following_indent = true;
            } else if projection_text.ends_with('\n')
                && component
                    .text
                    .bytes()
                    .all(|byte| matches!(byte, b' ' | b'\t'))
            {
                trim_following_indent = true;
            } else {
                projection_text.push_str(&component.text);
                trim_following_indent = false;
            }
        } else if trim_following_indent {
            projection_text.push_str(component.text.trim_start_matches([' ', '\t']));
            trim_following_indent = false;
        } else {
            projection_text.push_str(&component.text);
        }
    }
    declaration.projection_text = projection_text;
    declaration
}
