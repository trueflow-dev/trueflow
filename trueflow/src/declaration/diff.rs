use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use anyhow::{Result, ensure};
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
    ensure!(
        pair.base.is_some() || pair.head.is_some(),
        "snapshot pair {} is missing both base and head endpoints",
        pair.id.as_str()
    );
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

    let matching = if path_allows_matching && languages_match {
        match_declarations(base, head)
    } else {
        DeclarationMatching::default()
    };
    let reserved = &matching.reserved;

    let mut base_matches = vec![None; base.len()];
    let mut head_matches = vec![None; head.len()];
    for (match_index, reserved_match) in reserved.iter().enumerate() {
        base_matches[reserved_match.base] = Some(match_index);
        head_matches[reserved_match.head] = Some(match_index);
    }

    if path_allows_matching && languages_match && matching.has_ambiguity {
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

#[derive(Default)]
struct DeclarationMatching {
    reserved: Vec<ReservedMatch>,
    has_ambiguity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ParentGroup {
    Root,
    Matched(usize),
}

#[derive(Debug, Clone, Copy)]
enum MatchSide {
    Base,
    Head,
}

#[derive(Debug, Clone, Copy)]
enum MatchingPhase {
    ExactKey,
    ExactProjection,
    UniqueCompatible,
    UniqueNameElided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DeclarationGroup<'a> {
    ExactKey(&'a str),
    ExactProjection {
        kind: super::DeclarationKind,
        name: &'a str,
        parent: ParentGroup,
        projection_hash: &'a str,
    },
    Compatible {
        kind: super::DeclarationKind,
        name: &'a str,
        parent: ParentGroup,
    },
    NameElided {
        kind: super::DeclarationKind,
        parent: ParentGroup,
        discriminator: &'a str,
    },
}

struct ParentCorrespondence<'a> {
    base: HashMap<&'a str, usize>,
    head: HashMap<&'a str, usize>,
}

impl<'a> ParentCorrespondence<'a> {
    fn new() -> Self {
        Self {
            base: HashMap::new(),
            head: HashMap::new(),
        }
    }

    fn group(&self, declaration: &DeclarationNode, side: MatchSide) -> Option<ParentGroup> {
        let Some(parent) = &declaration.parent_part else {
            return Some(ParentGroup::Root);
        };
        let matches = match side {
            MatchSide::Base => &self.base,
            MatchSide::Head => &self.head,
        };
        matches
            .get(parent.as_str())
            .copied()
            .map(ParentGroup::Matched)
    }

    fn insert(&mut self, match_index: usize, base: &'a DeclarationNode, head: &'a DeclarationNode) {
        self.base.insert(base.id.as_str(), match_index);
        self.head.insert(head.id.as_str(), match_index);
    }
}

fn match_declarations(base: &[DeclarationNode], head: &[DeclarationNode]) -> DeclarationMatching {
    let base_discriminators = base
        .iter()
        .map(name_elided_discriminator)
        .collect::<Vec<_>>();
    let head_discriminators = head
        .iter()
        .map(name_elided_discriminator)
        .collect::<Vec<_>>();
    let mut matching = DeclarationMatching::default();
    let mut base_used = vec![false; base.len()];
    let mut head_used = vec![false; head.len()];
    let mut parents = ParentCorrespondence::new();
    let mut base_children = remaining_children(base);
    let mut head_children = remaining_children(head);

    reserve_phase(
        MatchingPhase::ExactKey,
        MatchingEvidence::ExactKey,
        base,
        head,
        &base_discriminators,
        &head_discriminators,
        &mut base_used,
        &mut head_used,
        &mut matching.reserved,
        &mut parents,
        &mut base_children,
        &mut head_children,
    );

    loop {
        let matched_before = matching.reserved.len();
        for (phase, evidence) in [
            (
                MatchingPhase::ExactProjection,
                MatchingEvidence::ExactProjection,
            ),
            (
                MatchingPhase::UniqueCompatible,
                MatchingEvidence::UniqueCompatible,
            ),
            (
                MatchingPhase::UniqueNameElided,
                MatchingEvidence::UniqueNameElided,
            ),
        ] {
            reserve_phase(
                phase,
                evidence,
                base,
                head,
                &base_discriminators,
                &head_discriminators,
                &mut base_used,
                &mut head_used,
                &mut matching.reserved,
                &mut parents,
                &mut base_children,
                &mut head_children,
            );
        }

        if matching.reserved.len() == matched_before
            || !matching.reserved[matched_before..].iter().any(|matched| {
                base_children
                    .get(base[matched.base].id.as_str())
                    .copied()
                    .unwrap_or_default()
                    > 0
                    && head_children
                        .get(head[matched.head].id.as_str())
                        .copied()
                        .unwrap_or_default()
                        > 0
            })
        {
            break;
        }
    }

    matching.has_ambiguity = has_ambiguous_candidates(
        base,
        head,
        &base_discriminators,
        &head_discriminators,
        &base_used,
        &head_used,
        &parents,
    );
    matching
}

#[allow(clippy::too_many_arguments)]
fn reserve_phase<'a>(
    phase: MatchingPhase,
    evidence: MatchingEvidence,
    base: &'a [DeclarationNode],
    head: &'a [DeclarationNode],
    base_discriminators: &'a [String],
    head_discriminators: &'a [String],
    base_used: &mut [bool],
    head_used: &mut [bool],
    reserved: &mut Vec<ReservedMatch>,
    parents: &mut ParentCorrespondence<'a>,
    base_children: &mut HashMap<&'a str, usize>,
    head_children: &mut HashMap<&'a str, usize>,
) {
    let base_groups =
        declaration_groups(base, base_discriminators, MatchSide::Base, phase, parents);
    let head_groups =
        declaration_groups(head, head_discriminators, MatchSide::Head, phase, parents);
    reserve_unique(
        base,
        head,
        &base_groups,
        &head_groups,
        base_used,
        head_used,
        reserved,
        parents,
        base_children,
        head_children,
        evidence,
    );
}

fn declaration_groups<'a>(
    declarations: &'a [DeclarationNode],
    discriminators: &'a [String],
    side: MatchSide,
    phase: MatchingPhase,
    parents: &ParentCorrespondence<'_>,
) -> Vec<Option<DeclarationGroup<'a>>> {
    declarations
        .iter()
        .zip(discriminators)
        .map(|(declaration, discriminator)| match phase {
            MatchingPhase::ExactKey => Some(DeclarationGroup::ExactKey(declaration.key.as_str())),
            MatchingPhase::ExactProjection => {
                parents
                    .group(declaration, side)
                    .map(|parent| DeclarationGroup::ExactProjection {
                        kind: declaration.kind,
                        name: &declaration.name,
                        parent,
                        projection_hash: declaration.projection_hash.as_str(),
                    })
            }
            MatchingPhase::UniqueCompatible => {
                parents
                    .group(declaration, side)
                    .map(|parent| DeclarationGroup::Compatible {
                        kind: declaration.kind,
                        name: &declaration.name,
                        parent,
                    })
            }
            MatchingPhase::UniqueNameElided => {
                parents
                    .group(declaration, side)
                    .map(|parent| DeclarationGroup::NameElided {
                        kind: declaration.kind,
                        parent,
                        discriminator,
                    })
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
struct GroupCount {
    index: usize,
    count: usize,
}

#[allow(clippy::too_many_arguments)]
fn reserve_unique<'a, K: Copy + Eq + Hash>(
    base: &'a [DeclarationNode],
    head: &'a [DeclarationNode],
    base_groups: &[Option<K>],
    head_groups: &[Option<K>],
    base_used: &mut [bool],
    head_used: &mut [bool],
    reserved: &mut Vec<ReservedMatch>,
    parents: &mut ParentCorrespondence<'a>,
    base_children: &mut HashMap<&'a str, usize>,
    head_children: &mut HashMap<&'a str, usize>,
    evidence: MatchingEvidence,
) {
    let base_counts = group_counts(base_groups, base_used);
    let head_counts = group_counts(head_groups, head_used);

    // Base source order makes reservation deterministic without resolving ambiguity by order.
    for (base_index, group) in base_groups.iter().enumerate() {
        if base_used[base_index] {
            continue;
        }
        let Some(group) = group else {
            continue;
        };
        let Some(base_count) = base_counts.get(group) else {
            continue;
        };
        let Some(head_count) = head_counts.get(group) else {
            continue;
        };
        if base_count.count != 1 || head_count.count != 1 {
            continue;
        }

        let head_index = head_count.index;
        base_used[base_index] = true;
        head_used[head_index] = true;
        decrement_parent_count(&base[base_index], base_children);
        decrement_parent_count(&head[head_index], head_children);
        let match_index = reserved.len();
        parents.insert(match_index, &base[base_index], &head[head_index]);
        reserved.push(ReservedMatch {
            base: base_index,
            head: head_index,
            evidence,
        });
    }
}

fn group_counts<K: Copy + Eq + Hash>(
    groups: &[Option<K>],
    used: &[bool],
) -> HashMap<K, GroupCount> {
    let mut counts = HashMap::new();
    for (index, group) in groups.iter().enumerate() {
        if used[index] {
            continue;
        }
        if let Some(group) = group {
            counts
                .entry(*group)
                .and_modify(|count: &mut GroupCount| count.count += 1)
                .or_insert(GroupCount { index, count: 1 });
        }
    }
    counts
}

fn remaining_children(declarations: &[DeclarationNode]) -> HashMap<&str, usize> {
    let mut children = HashMap::new();
    for declaration in declarations {
        if let Some(parent) = &declaration.parent_part {
            *children.entry(parent.as_str()).or_default() += 1;
        }
    }
    children
}

fn decrement_parent_count(declaration: &DeclarationNode, children: &mut HashMap<&str, usize>) {
    if let Some(parent) = &declaration.parent_part
        && let Some(count) = children.get_mut(parent.as_str())
    {
        *count -= 1;
    }
}

fn name_elided_discriminator(declaration: &DeclarationNode) -> String {
    fn push_usize(output: &mut String, mut value: usize) {
        if value == 0 {
            output.push('0');
            return;
        }

        let mut digits = [0_u8; std::mem::size_of::<usize>() * 3];
        let mut index = digits.len();
        while value != 0 {
            index -= 1;
            digits[index] = b"0123456789"[value % 10];
            value /= 10;
        }
        for digit in &digits[index..] {
            output.push(char::from(*digit));
        }
    }

    let mut discriminator = String::with_capacity(declaration.projection_text.len());
    let mut name_elided = false;
    for component in &declaration.components {
        if component.role == SourceComponentRole::Documentation {
            continue;
        }
        discriminator.push_str(component.role.protocol_tag());
        discriminator.push(':');
        if !name_elided && let Some(position) = component.text.find(&declaration.name) {
            name_elided = true;
            let normalized_len = component.text.len() - declaration.name.len() + "<name>".len();
            push_usize(&mut discriminator, normalized_len);
            discriminator.push(':');
            discriminator.push_str(&component.text[..position]);
            discriminator.push_str("<name>");
            discriminator.push_str(&component.text[position + declaration.name.len()..]);
        } else {
            push_usize(&mut discriminator, component.text.len());
            discriminator.push(':');
            discriminator.push_str(&component.text);
        }
    }
    discriminator
}

#[allow(clippy::too_many_arguments)]
fn has_ambiguous_candidates(
    base: &[DeclarationNode],
    head: &[DeclarationNode],
    base_discriminators: &[String],
    head_discriminators: &[String],
    base_used: &[bool],
    head_used: &[bool],
    parents: &ParentCorrespondence<'_>,
) -> bool {
    [
        MatchingPhase::UniqueCompatible,
        MatchingPhase::UniqueNameElided,
    ]
    .into_iter()
    .any(|phase| {
        let base_groups =
            declaration_groups(base, base_discriminators, MatchSide::Base, phase, parents);
        let head_groups =
            declaration_groups(head, head_discriminators, MatchSide::Head, phase, parents);
        let head_keys = head_groups
            .iter()
            .enumerate()
            .filter_map(|(index, group)| (!head_used[index]).then_some(*group).flatten())
            .collect::<HashSet<_>>();
        base_groups.iter().enumerate().any(|(index, group)| {
            !base_used[index] && group.is_some_and(|group| head_keys.contains(&group))
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
