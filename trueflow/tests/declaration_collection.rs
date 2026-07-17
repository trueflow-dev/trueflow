use std::path::Path;

use anyhow::Result;
use trueflow::analysis::Language;
use trueflow::declaration::diff::{DiffDiagnostic, diff_declarations};
use trueflow::declaration::review::{
    DeclarationReviewDiffBatch, DeclarationReviewQuery, DeclarationReviewStatus,
    collect_declaration_review,
};
use trueflow::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};

fn added_rust_pair(pair_id: &str, snapshot_id: &str, path: &str, source: &str) -> SnapshotPair {
    SnapshotPair::new(
        SnapshotPairId::new(pair_id),
        None,
        Some(SourceSnapshot::new(
            SnapshotId::new(snapshot_id),
            Path::new(path),
            Language::Rust,
            source,
        )),
        PathPairEvidence::Unmatched,
    )
}

fn describes_missing_endpoints(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    (message.contains("endpoint")
        && (message.contains("missing")
            || message.contains("neither")
            || message.contains("without")))
        || (message.contains("base") && message.contains("head") && message.contains("missing"))
}

fn explicitly_diagnoses_endpointless_pair(diagnostic: &DiffDiagnostic, pair_id: &str) -> bool {
    diagnostic.snapshot_pair_id.as_str() == pair_id
        && describes_missing_endpoints(&diagnostic.message)
}

fn is_ordinary_collection_status(status: &DeclarationReviewStatus) -> bool {
    matches!(
        status,
        DeclarationReviewStatus::Ready
            | DeclarationReviewStatus::NoSurfaceChanges
            | DeclarationReviewStatus::UnsupportedLanguage { .. }
            | DeclarationReviewStatus::FullyReviewed
    )
}

#[test]
fn endpointless_snapshot_pair_is_rejected_or_explicitly_diagnosed() -> Result<()> {
    const PAIR_ID: &str = "endpointless";
    let pair = SnapshotPair::new(
        SnapshotPairId::new(PAIR_ID),
        None,
        None,
        PathPairEvidence::Unmatched,
    );

    match diff_declarations(&[pair]) {
        Err(error) => assert!(
            describes_missing_endpoints(&format!("{error:#}")),
            "rejection must identify the missing snapshot endpoints: {error:#}"
        ),
        Ok(diff) => assert!(
            diff.diagnostics
                .iter()
                .any(|diagnostic| explicitly_diagnoses_endpointless_pair(diagnostic, PAIR_ID)),
            "an endpointless pair must not look like a valid empty diff: {diff:#?}"
        ),
    }

    Ok(())
}

#[test]
fn malformed_source_preserves_successful_units_and_reports_partial_collection() -> Result<()> {
    const MALFORMED_PAIR_ID: &str = "malformed-rust";
    let pairs = vec![
        added_rust_pair(
            "valid-rust",
            "valid-rust-head",
            "src/valid.rs",
            "pub fn retained(value: u8) -> u8 { value }\n",
        ),
        added_rust_pair(
            MALFORMED_PAIR_ID,
            "malformed-rust-head",
            "src/malformed.rs",
            "pub fn broken(",
        ),
    ];
    let diff = diff_declarations(&pairs)?;
    let collection = collect_declaration_review(&DeclarationReviewQuery::new(vec![
        DeclarationReviewDiffBatch::new(pairs, diff),
    ]))?;

    let collected_names = collection
        .items
        .iter()
        .map(|item| item.declaration.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        collected_names,
        ["retained"],
        "a malformed source must not discard independently projected review units"
    );
    assert!(
        collection.diagnostics.iter().any(|diagnostic| {
            diagnostic.snapshot_pair_id.as_str() == MALFORMED_PAIR_ID
                && diagnostic
                    .message
                    .to_ascii_lowercase()
                    .contains("syntax error")
        }),
        "the malformed source must retain its projection diagnostic: {:#?}",
        collection.diagnostics
    );
    assert!(
        !is_ordinary_collection_status(&collection.status),
        "a collection with a failed projection must expose a partial/failure status, got {:?}",
        collection.status
    );

    Ok(())
}
