use std::collections::HashSet;
use std::fs;

use anyhow::{Context, Result};
use serde_json::json;
use trueflow::declaration::{DeclarationKey, DeclarationProjectionHash};
use trueflow::hashing::BytesHash;
use trueflow::repo_path::RepoPath;
use trueflow::store::{
    BlockState, CommentAnchor, CommitId, DeclarationAnchorRange, DeclarationCommentAnchor,
    DeclarationRecordLocator, FileStore, Identity, Record, RepoRef, ReviewCheck, ReviewStore,
    ReviewTargetRef, ReviewedDeclarationSnapshot, SourceCommentAnchor, VcsSystem, Verdict,
    CURRENT_VERSION,
};
use trueflow_test_support::temp_test_dir;

const PROJECTION_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_PROJECTION_HASH: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const VALID_SOURCE: &str = "prefix\n/// café\npub fn run() {}\n";
const REVIEW_REVISION: &str = "0123456789abcdef";

fn projection_hash(value: &str) -> DeclarationProjectionHash {
    DeclarationProjectionHash::new(value)
}

fn reviewed_snapshot(source: &str) -> ReviewedDeclarationSnapshot {
    ReviewedDeclarationSnapshot {
        snapshot_id: "snapshot-head".to_string(),
        content_hash: BytesHash::from_bytes(source.as_bytes()),
    }
}

fn anchor_range(start_byte: usize, end_byte: usize, exact_text: &str) -> DeclarationAnchorRange {
    DeclarationAnchorRange {
        start_byte,
        end_byte,
        exact_text: exact_text.to_string(),
    }
}

fn declaration_locator(
    path: &str,
    key: &str,
    source_ordinal: usize,
) -> Result<DeclarationRecordLocator> {
    Ok(DeclarationRecordLocator {
        path: RepoPath::new(path)?,
        declaration_key: DeclarationKey::new(key),
        source_ordinal,
        source_span: 7..32,
        reviewed_snapshot: reviewed_snapshot(VALID_SOURCE),
        projection_hash: projection_hash(PROJECTION_HASH),
    })
}

fn declaration_anchor() -> DeclarationCommentAnchor {
    DeclarationCommentAnchor {
        reviewed_snapshot: reviewed_snapshot(VALID_SOURCE),
        projection_hash: projection_hash(PROJECTION_HASH),
        source_len_bytes: VALID_SOURCE.len(),
        ranges: vec![
            anchor_range(7, 16, "/// café"),
            anchor_range(17, 29, "pub fn run()"),
        ],
    }
}

fn declaration_record() -> Result<Record> {
    Ok(Record {
        id: "declaration-record".to_string(),
        version: 5,
        target: ReviewTargetRef::Declaration {
            hash: projection_hash(PROJECTION_HASH),
        },
        check: ReviewCheck::declaration(),
        verdict: Verdict::Approved,
        identity: Identity::Email {
            email: "dev@example.com".to_string(),
        },
        repo_ref: RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: CommitId::new(REVIEW_REVISION)?,
        },
        block_state: BlockState::Committed,
        timestamp: 1_700_000_000,
        path_hint: None,
        line_hint: None,
        note: None,
        comment_scope: None,
        comment_context: None,
        comment_anchor: Some(CommentAnchor::Declaration(declaration_anchor())),
        declaration_locator: Some(declaration_locator("src/lib.rs", "crate::run()", 0)?),
        tags: None,
        attestations: None,
    })
}

fn legacy_record(version: u32) -> Result<Record> {
    Ok(Record {
        id: "fixture".to_string(),
        version,
        target: ReviewTargetRef::Block {
            hash: trueflow::store::TreeHash::parse(PROJECTION_HASH)?,
        },
        check: ReviewCheck::review(),
        verdict: Verdict::Comment,
        identity: Identity::Email {
            email: "dev@example.com".to_string(),
        },
        repo_ref: RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: CommitId::new(REVIEW_REVISION)?,
        },
        block_state: BlockState::Committed,
        timestamp: 1_700,
        path_hint: Some(RepoPath::new("src/lib.rs")?),
        line_hint: Some(7),
        note: Some("note".to_string()),
        comment_scope: Some(trueflow::store::CommentScope {
            start_line: 2,
            end_line: 4,
        }),
        comment_context: Some("ctx".to_string()),
        comment_anchor: Some(CommentAnchor::Source(SourceCommentAnchor {
            revision: CommitId::new(REVIEW_REVISION)?,
            path: RepoPath::new("src/lib.rs")?,
            start_line: 2,
            end_line: 4,
        })),
        declaration_locator: None,
        tags: Some(vec!["signed".to_string(), "fixture".to_string()]),
        attestations: None,
    })
}

fn locator_mut(record: &mut Record) -> Result<&mut DeclarationRecordLocator> {
    record
        .declaration_locator
        .as_mut()
        .context("test declaration record must have a locator")
}

fn declaration_anchor_mut(record: &mut Record) -> Result<&mut DeclarationCommentAnchor> {
    match record.comment_anchor.as_mut() {
        Some(CommentAnchor::Declaration(anchor)) => Ok(anchor),
        other => {
            anyhow::bail!("test declaration record must have a declaration anchor, got {other:?}")
        }
    }
}

fn assert_invalid(records: Vec<(&str, Record)>) {
    for (case, record) in records {
        assert!(record.validate().is_err(), "{case} must be rejected");
    }
}

#[test]
fn declaration_check_has_stable_wire_value() -> Result<()> {
    let check = ReviewCheck::declaration();

    assert_eq!(check.as_str(), "declaration");
    assert_eq!(serde_json::to_string(&check)?, r#""declaration""#);
    Ok(())
}

#[test]
fn legacy_v2_v3_v4_signing_payloads_remain_byte_identical() -> Result<()> {
    let cases = [
        (
            2,
            r#"{"block_state":"committed","check":"review","id":"fixture","identity":{"email":"dev@example.com","type":"email"},"line_hint":7,"note":"note","path_hint":"src/lib.rs","repo_ref":{"revision":"0123456789abcdef","system":"git","type":"vcs"},"tags":["signed","fixture"],"target":{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"block"},"timestamp":1700,"verdict":"comment","version":2}"#,
        ),
        (
            3,
            r#"{"block_state":"committed","check":"review","comment_context":"ctx","comment_scope":{"end_line":4,"start_line":2},"id":"fixture","identity":{"email":"dev@example.com","type":"email"},"line_hint":7,"note":"note","path_hint":"src/lib.rs","repo_ref":{"revision":"0123456789abcdef","system":"git","type":"vcs"},"tags":["signed","fixture"],"target":{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"block"},"timestamp":1700,"verdict":"comment","version":3}"#,
        ),
        (
            4,
            r#"{"block_state":"committed","check":"review","comment_anchor":{"end_line":4,"path":"src/lib.rs","revision":"0123456789abcdef","start_line":2,"type":"source"},"comment_context":"ctx","comment_scope":{"end_line":4,"start_line":2},"id":"fixture","identity":{"email":"dev@example.com","type":"email"},"line_hint":7,"note":"note","path_hint":"src/lib.rs","repo_ref":{"revision":"0123456789abcdef","system":"git","type":"vcs"},"tags":["signed","fixture"],"target":{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"block"},"timestamp":1700,"verdict":"comment","version":4}"#,
        ),
    ];

    for (version, expected) in cases {
        assert_eq!(
            legacy_record(version)?.signing_payload()?,
            expected,
            "V{version} signing bytes changed"
        );
    }
    Ok(())
}

#[test]
fn v5_signing_payload_binds_locator_and_declaration_anchor() -> Result<()> {
    let baseline = declaration_record()?;
    let baseline_payload = baseline.signing_payload()?;

    let mut moved_locator = baseline.clone();
    locator_mut(&mut moved_locator)?.path = RepoPath::new("src/moved.rs")?;

    let mut changed_ordinal = baseline.clone();
    locator_mut(&mut changed_ordinal)?.source_ordinal = 1;

    let mut changed_anchor = baseline;
    declaration_anchor_mut(&mut changed_anchor)?.ranges[0] = anchor_range(7, 14, "/// caf");

    for (case, changed) in [
        ("locator path", moved_locator),
        ("source ordinal", changed_ordinal),
        ("exact anchor range", changed_anchor),
    ] {
        assert_ne!(
            changed.signing_payload()?,
            baseline_payload,
            "changing the signed {case} must change V5 signing bytes"
        );
    }
    Ok(())
}

#[test]
fn signing_dispatch_rejects_every_unsupported_record_version() -> Result<()> {
    assert_eq!(
        CURRENT_VERSION, 5,
        "these fixtures define the V5 wire contract"
    );

    for version in [0, 1, CURRENT_VERSION + 1, u32::MAX] {
        let mut record = legacy_record(4)?;
        record.version = version;
        assert!(
            record.signing_payload().is_err(),
            "unsupported version {version} must never borrow another version's signing shape"
        );
    }
    Ok(())
}

#[test]
fn legacy_versions_reject_declaration_only_fields_before_signing() -> Result<()> {
    for version in 2..=4 {
        let mut declaration_target = declaration_record()?;
        declaration_target.version = version;
        declaration_target.declaration_locator = None;
        declaration_target.comment_anchor = None;

        let mut declaration_locator = legacy_record(version)?;
        declaration_locator.declaration_locator = declaration_record()?.declaration_locator;

        let mut declaration_anchor = legacy_record(version)?;
        declaration_anchor.comment_anchor = declaration_record()?.comment_anchor;

        for (case, record) in [
            ("declaration target", declaration_target),
            ("declaration locator", declaration_locator),
            ("declaration anchor", declaration_anchor),
        ] {
            assert!(
                record.validate().is_err(),
                "V{version} {case} must be invalid"
            );
            assert!(
                record.signing_payload().is_err(),
                "V{version} {case} must not acquire legacy signing bytes"
            );
        }
    }
    Ok(())
}

#[test]
fn v5_declaration_target_requires_a_complete_locator_and_declaration_check() -> Result<()> {
    let mut missing_locator = declaration_record()?;
    missing_locator.declaration_locator = None;
    missing_locator.comment_anchor = None;

    let mut root_path = declaration_record()?;
    locator_mut(&mut root_path)?.path = RepoPath::root();

    let mut empty_key = declaration_record()?;
    locator_mut(&mut empty_key)?.declaration_key = DeclarationKey::new("");

    let mut empty_span = declaration_record()?;
    locator_mut(&mut empty_span)?.source_span = 7..7;

    let mut reversed_span = declaration_record()?;
    locator_mut(&mut reversed_span)?.source_span = std::ops::Range { start: 32, end: 7 };

    let mut empty_snapshot_id = declaration_record()?;
    locator_mut(&mut empty_snapshot_id)?
        .reviewed_snapshot
        .snapshot_id
        .clear();

    let mut empty_content_hash = declaration_record()?;
    locator_mut(&mut empty_content_hash)?
        .reviewed_snapshot
        .content_hash = BytesHash::new("");

    let mut wrong_check = declaration_record()?;
    wrong_check.check = ReviewCheck::review();

    let mut declaration_check_on_block = legacy_record(4)?;
    declaration_check_on_block.version = 5;
    declaration_check_on_block.check = ReviewCheck::declaration();
    declaration_check_on_block.comment_anchor = None;

    assert_invalid(vec![
        ("missing locator", missing_locator),
        ("repository-root locator path", root_path),
        ("empty declaration key", empty_key),
        ("empty source span", empty_span),
        ("reversed source span", reversed_span),
        ("empty reviewed snapshot id", empty_snapshot_id),
        ("empty reviewed content hash", empty_content_hash),
        ("declaration target with ordinary review check", wrong_check),
        (
            "declaration check with block target",
            declaration_check_on_block,
        ),
    ]);
    Ok(())
}

#[test]
fn declaration_locator_fields_are_required_during_deserialization() -> Result<()> {
    let serialized = serde_json::to_value(declaration_record()?)?;

    for field in [
        "path",
        "declaration_key",
        "source_ordinal",
        "source_span",
        "reviewed_snapshot",
        "projection_hash",
    ] {
        let mut missing = serialized.clone();
        missing["declaration_locator"]
            .as_object_mut()
            .context("serialized locator must be an object")?
            .remove(field);
        assert!(
            serde_json::from_value::<Record>(missing).is_err(),
            "missing locator field {field} must fail closed"
        );
    }
    Ok(())
}

#[test]
fn declaration_target_locator_and_anchor_must_name_one_projection_snapshot() -> Result<()> {
    let mut target_hash = declaration_record()?;
    target_hash.target = ReviewTargetRef::Declaration {
        hash: projection_hash(OTHER_PROJECTION_HASH),
    };

    let mut locator_hash = declaration_record()?;
    locator_mut(&mut locator_hash)?.projection_hash = projection_hash(OTHER_PROJECTION_HASH);

    let mut anchor_hash = declaration_record()?;
    declaration_anchor_mut(&mut anchor_hash)?.projection_hash =
        projection_hash(OTHER_PROJECTION_HASH);

    let mut anchor_snapshot_id = declaration_record()?;
    declaration_anchor_mut(&mut anchor_snapshot_id)?
        .reviewed_snapshot
        .snapshot_id = "snapshot-base".to_string();

    let mut anchor_content_hash = declaration_record()?;
    declaration_anchor_mut(&mut anchor_content_hash)?
        .reviewed_snapshot
        .content_hash = BytesHash::from_bytes(b"different source");

    assert_invalid(vec![
        ("target/locator projection hash mismatch", target_hash),
        ("locator/target projection hash mismatch", locator_hash),
        ("anchor/locator projection hash mismatch", anchor_hash),
        ("anchor/locator snapshot id mismatch", anchor_snapshot_id),
        ("anchor/locator content hash mismatch", anchor_content_hash),
    ]);
    Ok(())
}

#[test]
fn declaration_anchor_rejects_empty_unordered_overlapping_or_out_of_bounds_ranges() -> Result<()> {
    let invalid_ranges = [
        ("no ranges", vec![]),
        ("empty range", vec![anchor_range(7, 7, "")]),
        ("reversed range", vec![anchor_range(16, 7, "")]),
        (
            "unordered ranges",
            vec![
                anchor_range(17, 29, "pub fn run()"),
                anchor_range(7, 16, "/// café"),
            ],
        ),
        (
            "overlapping ranges",
            vec![
                anchor_range(7, 18, "abcdefghijk"),
                anchor_range(17, 29, "pub fn run()"),
            ],
        ),
        ("past snapshot", vec![anchor_range(31, 34, "abc")]),
        ("before declaration span", vec![anchor_range(3, 6, "fix")]),
        ("wrong exact-text width", vec![anchor_range(7, 16, "short")]),
    ];

    for (case, ranges) in invalid_ranges {
        let mut record = declaration_record()?;
        declaration_anchor_mut(&mut record)?.ranges = ranges;
        assert!(record.validate().is_err(), "{case} must be rejected");
    }
    Ok(())
}

#[test]
fn declaration_anchor_proves_utf8_boundaries_exact_slices_and_snapshot_hash_against_source(
) -> Result<()> {
    declaration_anchor().validate_against_source(VALID_SOURCE)?;

    let mut split_code_point = declaration_anchor();
    split_code_point.ranges = vec![anchor_range(15, 17, "é")];

    let mut wrong_exact_slice = declaration_anchor();
    wrong_exact_slice.ranges[0].exact_text = "/// cafe!".to_string();

    let mut wrong_source_length = declaration_anchor();
    wrong_source_length.source_len_bytes -= 1;

    let mut wrong_content_hash = declaration_anchor();
    wrong_content_hash.reviewed_snapshot.content_hash = BytesHash::from_bytes(b"other");

    for (case, anchor) in [
        ("range splitting a UTF-8 code point", split_code_point),
        ("same-width but different exact source", wrong_exact_slice),
        ("different source byte length", wrong_source_length),
        ("different reviewed content hash", wrong_content_hash),
    ] {
        assert!(
            anchor.validate_against_source(VALID_SOURCE).is_err(),
            "{case} must be rejected"
        );
    }
    Ok(())
}

#[test]
fn malformed_declaration_records_fail_during_deserialization() -> Result<()> {
    let valid = serde_json::to_value(declaration_record()?)?;

    let mut unsupported_version = valid.clone();
    unsupported_version["version"] = json!(CURRENT_VERSION + 1);

    let mut hash_mismatch = valid.clone();
    hash_mismatch["declaration_locator"]["projection_hash"] = json!(OTHER_PROJECTION_HASH);

    let mut missing_ranges = valid;
    missing_ranges["comment_anchor"]["ranges"] = json!([]);

    for (case, value) in [
        ("unsupported version", unsupported_version),
        ("projection hash mismatch", hash_mismatch),
        ("empty declaration anchor", missing_ranges),
    ] {
        assert!(
            serde_json::from_value::<Record>(value).is_err(),
            "{case} must fail at the deserialization boundary"
        );
    }
    Ok(())
}

#[test]
fn v2_v3_v4_declaration_signals_fail_during_deserialization() -> Result<()> {
    let declaration = serde_json::to_value(declaration_record()?)?;
    let signals = [
        (
            "declaration target",
            "target",
            declaration["target"].clone(),
        ),
        ("declaration check", "check", declaration["check"].clone()),
        (
            "non-null declaration locator",
            "declaration_locator",
            declaration["declaration_locator"].clone(),
        ),
        (
            "declaration comment anchor",
            "comment_anchor",
            declaration["comment_anchor"].clone(),
        ),
    ];

    for version in 2..=4 {
        let mut complete_declaration = declaration.clone();
        complete_declaration["version"] = json!(version);
        assert!(
            serde_json::from_value::<Record>(complete_declaration).is_err(),
            "a complete declaration record must not deserialize as V{version}"
        );

        for (case, field, signal) in &signals {
            let mut ordinary = serde_json::to_value(legacy_record(version)?)?;
            ordinary[*field] = signal.clone();

            assert!(
                serde_json::from_value::<Record>(ordinary).is_err(),
                "V{version} record with {case} must fail at the deserialization boundary"
            );
        }
    }
    Ok(())
}

#[test]
fn explicit_null_declaration_fields_respect_the_v4_v5_runtime_boundary() -> Result<()> {
    let mut declaration_target = serde_json::to_value(declaration_record()?)?;
    declaration_target["declaration_locator"] = serde_json::Value::Null;
    assert!(
        serde_json::from_value::<Record>(declaration_target).is_err(),
        "a declaration target with an explicitly null locator must fail runtime validation"
    );

    for version in [4, 5] {
        let mut ordinary = serde_json::to_value(legacy_record(version)?)?;
        ordinary["declaration_locator"] = serde_json::Value::Null;
        ordinary["comment_anchor"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<Record>(ordinary).is_ok(),
            "an ordinary V{version} record must accept explicitly null optional declaration fields"
        );
    }
    Ok(())
}

#[test]
fn diff_target_with_any_declaration_signal_fails_closed_during_history_load() -> Result<()> {
    let declaration = serde_json::to_value(declaration_record()?)?;
    let signals = [
        ("declaration check", "check", json!("declaration")),
        (
            "declaration locator",
            "declaration_locator",
            declaration["declaration_locator"].clone(),
        ),
        (
            "declaration anchor",
            "comment_anchor",
            declaration["comment_anchor"].clone(),
        ),
    ];
    let mut silently_skipped = Vec::new();

    for (case, field, signal) in signals {
        let root = temp_test_dir("diff_target_declaration_signal");
        let store = FileStore::for_root(&root)?;
        let mut raw = serde_json::to_value(legacy_record(5)?)?;
        raw["target"] = json!({ "kind": "diff", "hash": PROJECTION_HASH });
        raw[field] = signal;
        fs::write(
            store.db_path(),
            format!("{}\n", serde_json::to_string(&raw)?),
        )?;

        if let Ok(records) = store.read_history() {
            silently_skipped.push(format!("{case} ({} records returned)", records.len()));
        }
    }

    assert!(
        silently_skipped.is_empty(),
        "diff targets carrying declaration signals were silently accepted by the legacy-diff skip: {}",
        silently_skipped.join(", ")
    );
    Ok(())
}

#[test]
fn load_fails_closed_when_history_contains_a_malformed_declaration_record() -> Result<()> {
    let root = temp_test_dir("declaration_record_invalid_load");
    let store = FileStore::for_root(&root)?;
    let mut malformed = serde_json::to_value(declaration_record()?)?;
    malformed["declaration_locator"]["projection_hash"] = json!(OTHER_PROJECTION_HASH);
    fs::write(
        store.db_path(),
        format!("{}\n", serde_json::to_string(&malformed)?),
    )?;

    assert!(
        store.read_history().is_err(),
        "load must not skip a malformed signed declaration record and continue with partial history"
    );
    Ok(())
}

#[test]
fn append_rejects_an_invalid_declaration_record_without_writing_it() -> Result<()> {
    let root = temp_test_dir("declaration_record_invalid_append");
    let store = FileStore::for_root(&root)?;
    let mut invalid = declaration_record()?;
    locator_mut(&mut invalid)?.projection_hash = projection_hash(OTHER_PROJECTION_HASH);

    assert!(store.append(&invalid).is_err());
    assert!(
        !store.db_path().exists(),
        "validation must happen before opening or mutating the history"
    );
    Ok(())
}

#[test]
fn valid_declaration_record_round_trips_through_the_store() -> Result<()> {
    let root = temp_test_dir("declaration_record_round_trip");
    let store = FileStore::for_root(&root)?;
    let record = declaration_record()?;

    store.append(&record)?;
    let loaded = store.read_history()?;

    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].declaration_locator, record.declaration_locator);
    assert_eq!(loaded[0].comment_anchor, record.comment_anchor);
    assert_eq!(loaded[0].signing_payload()?, record.signing_payload()?);
    Ok(())
}

#[test]
fn equal_projection_hashes_at_distinct_locators_remain_distinct_in_history() -> Result<()> {
    let root = temp_test_dir("declaration_record_duplicate_hashes");
    let store = FileStore::for_root(&root)?;
    let first = declaration_record()?;
    let mut second = declaration_record()?;
    second.id = "declaration-record-2".to_string();
    second.timestamp += 1;
    second.declaration_locator = Some(declaration_locator(
        "src/other.rs",
        "crate::other::run()",
        3,
    )?);
    second.comment_anchor = None;

    store.append(&first)?;
    store.append(&second)?;
    let loaded = store.read_history()?;
    let locators = loaded
        .iter()
        .map(|record| {
            record
                .declaration_locator
                .clone()
                .context("loaded declaration record must retain its signed locator")
        })
        .collect::<Result<HashSet<_>>>()?;

    assert_eq!(loaded.len(), 2);
    assert_eq!(locators.len(), 2);
    assert!(locators.contains(
        first
            .declaration_locator
            .as_ref()
            .context("first locator")?
    ));
    assert!(locators.contains(
        second
            .declaration_locator
            .as_ref()
            .context("second locator")?
    ));
    Ok(())
}
