use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::commands::mark::{StructuredMarkRequest, build_structured_record};
use trueflow::declaration::coverage::{
    CoverageBindingKind, DeclarationCoverageBinding, DeclarationCoverageIndex,
};
use trueflow::declaration::diff::{DeclarationDiff, diff_declarations};
use trueflow::declaration::review::{
    DeclarationReviewDiffBatch, DeclarationReviewQuery, collect_declaration_review,
};
use trueflow::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};
use trueflow::declaration::{
    DeclarationId, DeclarationKey, DeclarationNode, DeclarationProjectionHash,
};
use trueflow::hashing::BytesHash;
use trueflow::repo_path::RepoPath;
use trueflow::store::{
    BlockState, CommentAnchor, CommitId, DeclarationAnchorRange, DeclarationCommentAnchor,
    DeclarationRecordLocator, Identity, Record, RepoRef, ReviewCheck, ReviewIndex, ReviewTargetRef,
    ReviewedDeclarationSnapshot, VcsSystem, Verdict,
};

const REVISION: &str = "0123456789abcdef";

fn snapshot(id: &str, path: &str, source: &str) -> SourceSnapshot {
    SourceSnapshot::new(SnapshotId::new(id), Path::new(path), Language::Rust, source)
}

fn pair(
    id: &str,
    base: Option<SourceSnapshot>,
    head: Option<SourceSnapshot>,
    path_evidence: PathPairEvidence,
) -> SnapshotPair {
    SnapshotPair::new(SnapshotPairId::new(id), base, head, path_evidence)
}

fn batch(pairs: Vec<SnapshotPair>) -> Result<(DeclarationReviewDiffBatch, DeclarationDiff)> {
    let diff = diff_declarations(&pairs)?;
    Ok((DeclarationReviewDiffBatch::new(pairs, diff.clone()), diff))
}

fn reviewed_snapshot(snapshot: &SourceSnapshot) -> ReviewedDeclarationSnapshot {
    ReviewedDeclarationSnapshot {
        snapshot_id: snapshot.id.as_str().to_string(),
        content_hash: snapshot.bytes_hash().clone(),
    }
}

fn locator(
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
) -> Result<DeclarationRecordLocator> {
    Ok(DeclarationRecordLocator {
        path: RepoPath::new(
            snapshot
                .path
                .to_str()
                .context("test snapshot path must be UTF-8")?,
        )?,
        declaration_key: declaration.key.clone(),
        source_ordinal: declaration.source_ordinal,
        source_span: declaration.source_span.clone(),
        reviewed_snapshot: reviewed_snapshot(snapshot),
        projection_hash: declaration.projection_hash.clone(),
    })
}

fn record(
    id: &str,
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
    verdict: Verdict,
    timestamp: i64,
) -> Result<Record> {
    let is_comment = verdict == Verdict::Comment;
    let mut record = Record::new(
        ReviewTargetRef::Declaration {
            hash: declaration.projection_hash.clone(),
        },
        ReviewCheck::declaration(),
        verdict,
        Identity::Email {
            email: "reviewer@example.com".to_string(),
        },
        RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: CommitId::new(REVISION)?,
        },
        BlockState::Committed,
    );
    record.id = id.to_string();
    record.timestamp = timestamp;
    record.declaration_locator = Some(locator(snapshot, declaration)?);
    if is_comment {
        record.note = Some("needs another look".to_string());
    }
    Ok(record)
}

fn added_target(
    pair_id: &str,
    snapshot_id: &str,
    path: &str,
    source: &str,
    name: &str,
) -> Result<(DeclarationReviewDiffBatch, SourceSnapshot, DeclarationNode)> {
    let head = snapshot(snapshot_id, path, source);
    let (batch, diff) = batch(vec![pair(
        pair_id,
        None,
        Some(head.clone()),
        PathPairEvidence::Unmatched,
    )])?;
    let declaration = diff
        .units
        .iter()
        .filter_map(|unit| unit.head.as_ref())
        .find(|declaration| declaration.name == name)
        .with_context(|| format!("missing projected declaration {name}"))?
        .clone();
    Ok((batch, head, declaration))
}

fn binding<'a>(
    index: &'a DeclarationCoverageIndex,
    pair_id: &str,
    declaration_id: &DeclarationId,
) -> Option<&'a DeclarationCoverageBinding> {
    index.binding_for(&SnapshotPairId::new(pair_id), declaration_id)
}

fn assert_binding(
    index: &DeclarationCoverageIndex,
    pair_id: &str,
    declaration_id: &DeclarationId,
    record_index: usize,
    kind: CoverageBindingKind,
    verdict: &Verdict,
) {
    let binding = binding(index, pair_id, declaration_id)
        .unwrap_or_else(|| panic!("missing binding for {pair_id}:{}", declaration_id.as_str()));
    assert_eq!(binding.record_index(), record_index);
    assert_eq!(binding.kind(), kind);
    assert_eq!(binding.verdict(), verdict);
}

#[test]
fn exact_locators_bind_duplicate_equal_hashes_before_verdict_selection() -> Result<()> {
    let source = "pub fn duplicate() {}\n";
    let first = snapshot("first-head", "src/first.rs", source);
    let second = snapshot("second-head", "src/second.rs", source);
    let pairs = vec![
        pair(
            "first-pair",
            None,
            Some(first.clone()),
            PathPairEvidence::Unmatched,
        ),
        pair(
            "second-pair",
            None,
            Some(second.clone()),
            PathPairEvidence::Unmatched,
        ),
    ];
    let (batch, diff) = batch(pairs)?;
    let first_declaration = diff.units[0].head.as_ref().context("first declaration")?;
    let second_declaration = diff.units[1].head.as_ref().context("second declaration")?;
    assert_eq!(
        first_declaration.projection_hash, second_declaration.projection_hash,
        "fixture must exercise equal content identities"
    );

    let records = vec![
        record(
            "first-approval",
            &first,
            first_declaration,
            Verdict::Approved,
            10,
        )?,
        record(
            "second-rejection",
            &second,
            second_declaration,
            Verdict::Rejected,
            20,
        )?,
    ];
    let index = DeclarationCoverageIndex::build(&[batch], &records)?;

    assert_binding(
        &index,
        "first-pair",
        &first_declaration.id,
        0,
        CoverageBindingKind::ExactLocator,
        &Verdict::Approved,
    );
    assert_binding(
        &index,
        "second-pair",
        &second_declaration.id,
        1,
        CoverageBindingKind::ExactLocator,
        &Verdict::Rejected,
    );
    Ok(())
}

#[test]
fn ambiguous_key_or_hash_fallbacks_leave_every_duplicate_uncovered() -> Result<()> {
    let source = "pub fn duplicate() {}\n";
    let first = snapshot("first-head", "src/first.rs", source);
    let second = snapshot("second-head", "src/second.rs", source);
    let pairs = vec![
        pair("first-pair", None, Some(first), PathPairEvidence::Unmatched),
        pair(
            "second-pair",
            None,
            Some(second),
            PathPairEvidence::Unmatched,
        ),
    ];
    let (batch, diff) = batch(pairs)?;
    let first_declaration = diff.units[0].head.as_ref().context("first declaration")?;
    let second_declaration = diff.units[1].head.as_ref().context("second declaration")?;

    let obsolete = snapshot("obsolete", "old/location.rs", source);
    let ambiguous_key = record(
        "ambiguous-key",
        &obsolete,
        first_declaration,
        Verdict::Approved,
        10,
    )?;
    let mut ambiguous_hash = ambiguous_key.clone();
    ambiguous_hash.id = "ambiguous-hash".to_string();
    ambiguous_hash.timestamp = 11;
    ambiguous_hash
        .declaration_locator
        .as_mut()
        .context("declaration locator")?
        .declaration_key = DeclarationKey::new("non-matching-key");

    for candidate in [ambiguous_key, ambiguous_hash] {
        let index = DeclarationCoverageIndex::build(std::slice::from_ref(&batch), &[candidate])?;
        assert!(binding(&index, "first-pair", &first_declaration.id).is_none());
        assert!(binding(&index, "second-pair", &second_declaration.id).is_none());
    }
    Ok(())
}

#[test]
fn same_path_unique_key_hash_fallback_survives_locator_drift() -> Result<()> {
    let (batch, head, declaration) = added_target(
        "current-pair",
        "current-head",
        "src/lib.rs",
        "\n\npub fn stable(value: u8) -> u8 { value }\n",
        "stable",
    )?;
    let mut approval = record("old-approval", &head, &declaration, Verdict::Approved, 10)?;
    let locator = approval
        .declaration_locator
        .as_mut()
        .context("declaration locator")?;
    locator.reviewed_snapshot.snapshot_id = "older-snapshot".to_string();
    locator.source_ordinal += 7;
    locator.source_span = 1..2;

    let index = DeclarationCoverageIndex::build(&[batch], &[approval])?;
    assert_binding(
        &index,
        "current-pair",
        &declaration.id,
        0,
        CoverageBindingKind::SamePathKeyHash,
        &Verdict::Approved,
    );
    Ok(())
}

#[test]
fn proven_file_rename_and_declaration_match_carry_approval() -> Result<()> {
    let source = "pub fn stable(value: u8) -> u8 { value }\n";
    let base = snapshot("base", "src/old.rs", source);
    let head = snapshot("head", "src/new.rs", source);
    let (batch, diff) = batch(vec![pair(
        "rename-pair",
        Some(base.clone()),
        Some(head),
        PathPairEvidence::ExplicitRename,
    )])?;
    let declaration_match = diff.matches.first().context("proven declaration match")?;
    let approval = record(
        "base-approval",
        &base,
        &declaration_match.base,
        Verdict::Approved,
        10,
    )?;

    let index = DeclarationCoverageIndex::build(&[batch], &[approval])?;
    assert_binding(
        &index,
        "rename-pair",
        &declaration_match.head.id,
        0,
        CoverageBindingKind::ProvenRenameMatch,
        &Verdict::Approved,
    );
    Ok(())
}

#[test]
fn unproven_move_and_declaration_rename_do_not_transfer_coverage() -> Result<()> {
    let unchanged_source = "pub fn stable(value: u8) -> u8 { value }\n";
    let old_path = snapshot("move-base", "src/old.rs", unchanged_source);
    let new_path = snapshot("move-head", "src/new.rs", unchanged_source);
    let (unproven_batch, unproven_diff) = batch(vec![pair(
        "unproven-move",
        Some(old_path.clone()),
        Some(new_path),
        PathPairEvidence::Unmatched,
    )])?;
    let old_declaration = unproven_diff
        .units
        .iter()
        .filter_map(|unit| unit.base.as_ref())
        .find(|declaration| declaration.name == "stable")
        .context("deleted declaration")?;
    let moved_declaration = unproven_diff
        .units
        .iter()
        .filter_map(|unit| unit.head.as_ref())
        .find(|declaration| declaration.name == "stable")
        .context("added declaration")?;
    let old_approval = record(
        "move-base-approval",
        &old_path,
        old_declaration,
        Verdict::Approved,
        10,
    )?;
    let unproven = DeclarationCoverageIndex::build(&[unproven_batch], &[old_approval])?;
    assert!(binding(&unproven, "unproven-move", &moved_declaration.id).is_none());

    let rename_base = snapshot(
        "rename-base",
        "src/lib.rs",
        "pub fn before(value: u8) -> u8 { value }\n",
    );
    let rename_head = snapshot(
        "rename-head",
        "src/lib.rs",
        "pub fn after(value: u8) -> u8 { value }\n",
    );
    let (rename_batch, rename_diff) = batch(vec![pair(
        "declaration-rename",
        Some(rename_base.clone()),
        Some(rename_head),
        PathPairEvidence::SamePath,
    )])?;
    let before = rename_diff
        .matches
        .iter()
        .find(|candidate| candidate.base.name == "before")
        .map(|candidate| &candidate.base)
        .or_else(|| {
            rename_diff
                .units
                .iter()
                .filter_map(|unit| unit.base.as_ref())
                .find(|declaration| declaration.name == "before")
        })
        .context("base side of declaration rename")?;
    let after = rename_diff
        .matches
        .iter()
        .find(|candidate| candidate.head.name == "after")
        .map(|candidate| &candidate.head)
        .or_else(|| {
            rename_diff
                .units
                .iter()
                .filter_map(|unit| unit.head.as_ref())
                .find(|declaration| declaration.name == "after")
        })
        .context("head side of declaration rename")?;
    assert_ne!(before.projection_hash, after.projection_hash);
    let before_approval = record(
        "before-approval",
        &rename_base,
        before,
        Verdict::Approved,
        10,
    )?;
    let renamed = DeclarationCoverageIndex::build(&[rename_batch], &[before_approval])?;
    assert!(binding(&renamed, "declaration-rename", &after.id).is_none());
    Ok(())
}

#[test]
fn path_independent_key_hash_and_hash_only_fallbacks_require_global_uniqueness() -> Result<()> {
    let source = "pub fn portable(value: u8) -> u8 { value }\n";
    let (unique_batch, unique_head, unique_declaration) = added_target(
        "unique-pair",
        "unique-head",
        "src/new.rs",
        source,
        "portable",
    )?;
    let obsolete = snapshot("obsolete", "src/old.rs", source);
    let key_hash_record = record(
        "key-hash",
        &obsolete,
        &unique_declaration,
        Verdict::Approved,
        10,
    )?;
    let key_hash_index =
        DeclarationCoverageIndex::build(std::slice::from_ref(&unique_batch), &[key_hash_record])?;
    assert_binding(
        &key_hash_index,
        "unique-pair",
        &unique_declaration.id,
        0,
        CoverageBindingKind::UniqueKeyHash,
        &Verdict::Approved,
    );

    let mut hash_only_record = record(
        "hash-only",
        &unique_head,
        &unique_declaration,
        Verdict::Comment,
        11,
    )?;
    let hash_only_locator = hash_only_record
        .declaration_locator
        .as_mut()
        .context("declaration locator")?;
    hash_only_locator.path = RepoPath::new("unrelated/path.rs")?;
    hash_only_locator.declaration_key = DeclarationKey::new("unrelated-key");
    hash_only_locator.reviewed_snapshot.snapshot_id = "unrelated-snapshot".to_string();
    let hash_only_index =
        DeclarationCoverageIndex::build(&[unique_batch], std::slice::from_ref(&hash_only_record))?;
    assert_binding(
        &hash_only_index,
        "unique-pair",
        &unique_declaration.id,
        0,
        CoverageBindingKind::UniqueHash,
        &Verdict::Comment,
    );

    let duplicate = snapshot("duplicate-head", "src/duplicate.rs", source);
    let (ambiguous_batch, ambiguous_diff) = batch(vec![
        pair(
            "unique-pair",
            None,
            Some(unique_head),
            PathPairEvidence::Unmatched,
        ),
        pair(
            "duplicate-pair",
            None,
            Some(duplicate),
            PathPairEvidence::Unmatched,
        ),
    ])?;
    let ambiguous_index = DeclarationCoverageIndex::build(&[ambiguous_batch], &[hash_only_record])?;
    for unit in &ambiguous_diff.units {
        let declaration = unit.head.as_ref().context("added declaration")?;
        assert!(
            binding(
                &ambiguous_index,
                unit.snapshot_pair_id.as_str(),
                &declaration.id
            )
            .is_none(),
            "hash-only coverage must not choose between equal candidates"
        );
    }
    Ok(())
}

#[test]
fn latest_verdict_is_chosen_after_binding_and_timestamp_ties_use_append_position() -> Result<()> {
    let source = "pub fn duplicate() {}\n";
    let first = snapshot("first-head", "src/first.rs", source);
    let second = snapshot("second-head", "src/second.rs", source);
    let (batch, diff) = batch(vec![
        pair(
            "first-pair",
            None,
            Some(first.clone()),
            PathPairEvidence::Unmatched,
        ),
        pair(
            "second-pair",
            None,
            Some(second.clone()),
            PathPairEvidence::Unmatched,
        ),
    ])?;
    let first_declaration = diff.units[0].head.as_ref().context("first declaration")?;
    let second_declaration = diff.units[1].head.as_ref().context("second declaration")?;
    let records = vec![
        record(
            "first-old",
            &first,
            first_declaration,
            Verdict::Rejected,
            50,
        )?,
        record(
            "second-newer-by-time",
            &second,
            second_declaration,
            Verdict::Comment,
            100,
        )?,
        record(
            "first-tie-earlier-append",
            &first,
            first_declaration,
            Verdict::Comment,
            200,
        )?,
        record(
            "first-tie-later-append",
            &first,
            first_declaration,
            Verdict::Approved,
            200,
        )?,
    ];

    let index = DeclarationCoverageIndex::build(&[batch], &records)?;
    assert_binding(
        &index,
        "first-pair",
        &first_declaration.id,
        3,
        CoverageBindingKind::ExactLocator,
        &Verdict::Approved,
    );
    assert_binding(
        &index,
        "second-pair",
        &second_declaration.id,
        1,
        CoverageBindingKind::ExactLocator,
        &Verdict::Comment,
    );
    Ok(())
}

#[test]
fn collection_hides_approved_but_keeps_comment_and_rejected_verdicts_visible() -> Result<()> {
    let head = snapshot(
        "collection-head",
        "src/lib.rs",
        "pub fn approved() {}\npub fn commented() {}\npub fn rejected() {}\n",
    );
    let (batch, diff) = batch(vec![pair(
        "collection-pair",
        None,
        Some(head.clone()),
        PathPairEvidence::Unmatched,
    )])?;
    let find = |name: &str| {
        diff.units
            .iter()
            .filter_map(|unit| unit.head.as_ref())
            .find(|declaration| declaration.name == name)
            .cloned()
            .with_context(|| format!("missing {name}"))
    };
    let approved = find("approved")?;
    let commented = find("commented")?;
    let rejected = find("rejected")?;
    let records = vec![
        record("approved", &head, &approved, Verdict::Approved, 10)?,
        record("commented", &head, &commented, Verdict::Comment, 10)?,
        record("rejected", &head, &rejected, Verdict::Rejected, 10)?,
    ];

    let query = DeclarationReviewQuery::new(vec![batch]).with_records(records);
    let collection = collect_declaration_review(&query)?;
    let visible = collection
        .items
        .iter()
        .map(|item| (item.declaration.name.as_str(), item.latest_verdict.as_ref()))
        .collect::<Vec<_>>();

    assert_eq!(
        visible,
        [
            ("commented", Some(&Verdict::Comment)),
            ("rejected", Some(&Verdict::Rejected)),
        ]
    );
    Ok(())
}

#[test]
fn ordinary_review_index_ignores_declaration_records() -> Result<()> {
    let source = "pub fn isolated() {}\n";
    let (_, head, declaration) = added_target(
        "isolated-pair",
        "isolated-head",
        "src/lib.rs",
        source,
        "isolated",
    )?;
    let approval = record(
        "declaration-approval",
        &head,
        &declaration,
        Verdict::Approved,
        10,
    )?;
    let ordinary = ReviewIndex::from_records(&[approval], None).approved_targets();
    let colliding_block = ReviewTargetRef::Block {
        hash: trueflow::store::TreeHash::parse(declaration.projection_hash.as_str())?,
    };

    assert!(!ordinary.contains_target(&colliding_block));
    assert!(!ordinary.contains_target(&ReviewTargetRef::Declaration {
        hash: declaration.projection_hash,
    }));
    Ok(())
}

fn declaration_anchor(
    snapshot: &SourceSnapshot,
    projection_hash: &DeclarationProjectionHash,
    source: &str,
) -> DeclarationCommentAnchor {
    let exact_text = "pub fn structured(value: u8) -> u8";
    DeclarationCommentAnchor {
        reviewed_snapshot: reviewed_snapshot(snapshot),
        projection_hash: projection_hash.clone(),
        source_len_bytes: source.len(),
        ranges: vec![DeclarationAnchorRange {
            start_byte: 0,
            end_byte: exact_text.len(),
            exact_text: exact_text.to_string(),
        }],
    }
}

#[test]
fn structured_declaration_builder_preserves_resolved_provenance_locator_and_anchor() -> Result<()> {
    let source = "pub fn structured(value: u8) -> u8 { value }\n";
    let (_, head, declaration) = added_target(
        "structured-pair",
        "captured-worktree",
        "src/generated.rs",
        source,
        "structured",
    )?;
    let locator = locator(&head, &declaration)?;
    let anchor = declaration_anchor(&head, &declaration.projection_hash, source);
    let repo_ref = RepoRef::Vcs {
        system: VcsSystem::Git,
        revision: CommitId::new("fedcba9876543210")?,
    };
    let request = StructuredMarkRequest {
        target: ReviewTargetRef::Declaration {
            hash: declaration.projection_hash,
        },
        check: ReviewCheck::declaration(),
        verdict: Verdict::Comment,
        identity: Identity::Email {
            email: "reviewer@example.com".to_string(),
        },
        repo_ref: repo_ref.clone(),
        block_state: BlockState::Uncommitted,
        note: Some("captured declaration comment".to_string()),
        comment_context: Some("relationship data is advisory".to_string()),
        comment_anchor: Some(CommentAnchor::Declaration(anchor.clone())),
        declaration_locator: Some(locator.clone()),
    };

    let built = build_structured_record(request)?;
    assert_eq!(built.repo_ref, repo_ref);
    assert_eq!(built.block_state, BlockState::Uncommitted);
    assert_eq!(built.declaration_locator, Some(locator));
    assert_eq!(
        built.comment_anchor,
        Some(CommentAnchor::Declaration(anchor))
    );
    assert_eq!(built.path_hint, None);
    assert_eq!(built.line_hint, None);
    Ok(())
}

#[test]
fn structured_declaration_builder_rejects_resolved_field_mismatches() -> Result<()> {
    let source = "pub fn structured(value: u8) -> u8 { value }\n";
    let (_, head, declaration) = added_target(
        "structured-pair",
        "captured-worktree",
        "src/generated.rs",
        source,
        "structured",
    )?;
    let locator = locator(&head, &declaration)?;
    let anchor = declaration_anchor(&head, &declaration.projection_hash, source);
    let request = StructuredMarkRequest {
        target: ReviewTargetRef::Declaration {
            hash: declaration.projection_hash,
        },
        check: ReviewCheck::declaration(),
        verdict: Verdict::Approved,
        identity: Identity::Email {
            email: "reviewer@example.com".to_string(),
        },
        repo_ref: RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: CommitId::new("fedcba9876543210")?,
        },
        block_state: BlockState::Committed,
        note: None,
        comment_context: None,
        comment_anchor: Some(CommentAnchor::Declaration(anchor)),
        declaration_locator: Some(locator),
    };

    let mut wrong_locator_hash = request.clone();
    wrong_locator_hash
        .declaration_locator
        .as_mut()
        .context("declaration locator")?
        .projection_hash = DeclarationProjectionHash::new(
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );

    let mut wrong_anchor_snapshot = request;
    match wrong_anchor_snapshot.comment_anchor.as_mut() {
        Some(CommentAnchor::Declaration(anchor)) => {
            anchor.reviewed_snapshot.snapshot_id = "different-snapshot".to_string();
        }
        other => anyhow::bail!("expected declaration anchor, got {other:?}"),
    }

    for (case, invalid) in [
        ("target/locator hash mismatch", wrong_locator_hash),
        ("locator/anchor snapshot mismatch", wrong_anchor_snapshot),
    ] {
        assert!(
            build_structured_record(invalid).is_err(),
            "structured builder must reject {case} before append"
        );
    }
    Ok(())
}

#[test]
fn declaration_fixture_hash_is_not_a_worktree_block_target() -> Result<()> {
    let arbitrary_projection = DeclarationProjectionHash::new(
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    );
    let snapshot = ReviewedDeclarationSnapshot {
        snapshot_id: "detached-snapshot".to_string(),
        content_hash: BytesHash::from_bytes(b"pub fn detached();\n"),
    };
    let locator = DeclarationRecordLocator {
        path: RepoPath::new("detached/generated.rs")?,
        declaration_key: DeclarationKey::new("detached::generated"),
        source_ordinal: 0,
        source_span: 0..18,
        reviewed_snapshot: snapshot,
        projection_hash: arbitrary_projection.clone(),
    };
    let request = StructuredMarkRequest {
        target: ReviewTargetRef::Declaration {
            hash: arbitrary_projection,
        },
        check: ReviewCheck::declaration(),
        verdict: Verdict::Approved,
        identity: Identity::Email {
            email: "reviewer@example.com".to_string(),
        },
        repo_ref: RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: CommitId::new("fedcba9876543210")?,
        },
        block_state: BlockState::Committed,
        note: None,
        comment_context: None,
        comment_anchor: None,
        declaration_locator: Some(locator.clone()),
    };

    let built = build_structured_record(request)?;
    assert_eq!(built.declaration_locator, Some(locator));
    Ok(())
}
