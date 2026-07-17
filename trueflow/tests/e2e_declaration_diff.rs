use std::path::Path;

use anyhow::{Context, Result};
use trueflow::analysis::Language;
use trueflow::declaration::DeclarationKind;
use trueflow::declaration::diff::{
    DeclarationChangeKind, DeclarationDiff, DeclarationDiffUnit, DiffDiagnosticKind,
    MatchingEvidence, diff_declarations,
};
use trueflow::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};

fn snapshot(id: &str, path: &str, source: &str) -> SourceSnapshot {
    SourceSnapshot::new(SnapshotId::new(id), Path::new(path), Language::Rust, source)
}

fn same_path_pair(
    pair_id: &str,
    path: &str,
    base_source: Option<&str>,
    head_source: Option<&str>,
) -> SnapshotPair {
    SnapshotPair::new(
        SnapshotPairId::new(pair_id),
        base_source.map(|source| snapshot(&format!("{pair_id}-base"), path, source)),
        head_source.map(|source| snapshot(&format!("{pair_id}-head"), path, source)),
        if base_source.is_some() && head_source.is_some() {
            PathPairEvidence::SamePath
        } else {
            PathPairEvidence::Unmatched
        },
    )
}

fn single_unit(diff: &DeclarationDiff) -> Result<&DeclarationDiffUnit> {
    diff.units
        .first()
        .filter(|_| diff.units.len() == 1)
        .with_context(|| {
            format!(
                "expected exactly one declaration diff unit, got {:?}",
                diff.units
            )
        })
}

#[test]
fn snapshot_content_identity_is_path_independent_and_byte_exact() {
    let original = snapshot("original", "src/original.rs", "pub fn café() {}\n");
    let moved = snapshot("moved", "src/moved.rs", "pub fn café() {}\n");
    let missing_final_newline = snapshot("changed", "src/original.rs", "pub fn café() {}");

    assert_eq!(
        original.bytes_hash(),
        moved.bytes_hash(),
        "snapshot content identity must not include path or snapshot ID"
    );
    assert_ne!(
        original.bytes_hash(),
        missing_final_newline.bytes_hash(),
        "even a trailing-byte change must produce a different snapshot content identity"
    );
}

#[test]
fn body_only_edit_matches_but_creates_no_review_unit() -> Result<()> {
    const BASE: &str =
        "/// Returns the next value.\npub fn next(value: u32) -> u32 {\n    value + 1\n}\n";
    const HEAD: &str = "/// Returns the next value.\npub fn next(value: u32) -> u32 {\n    value.saturating_add(1)\n}\n";

    let diff = diff_declarations(&[same_path_pair(
        "body-only",
        "src/counter.rs",
        Some(BASE),
        Some(HEAD),
    )])?;

    assert!(
        diff.units.is_empty(),
        "an executable-body-only edit must not create declaration review targets"
    );
    assert_eq!(
        diff.matches.len(),
        1,
        "the unchanged surface must still match"
    );
    assert_eq!(diff.matches[0].evidence, MatchingEvidence::ExactKey);
    assert!(diff.diagnostics.is_empty());

    Ok(())
}

struct SurfaceEditCase {
    id: &'static str,
    base: &'static str,
    head: &'static str,
    expected_kind: DeclarationKind,
    expected_base_projection: &'static str,
    expected_head_projection: &'static str,
}

#[test]
fn documentation_signature_field_and_variant_edits_are_exact_changed_units() -> Result<()> {
    let cases = [
        SurfaceEditCase {
            id: "documentation",
            base: "/// Parses one value.\npub fn parse(input: &str) -> usize { input.len() }\n",
            head: "/// Parses a value.\npub fn parse(input: &str) -> usize { input.len() }\n",
            expected_kind: DeclarationKind::Function,
            expected_base_projection: "/// Parses one value.\npub fn parse(input: &str) -> usize",
            expected_head_projection: "/// Parses a value.\npub fn parse(input: &str) -> usize",
        },
        SurfaceEditCase {
            id: "signature",
            base: "pub fn parse(input: &str) -> usize { input.len() }\n",
            head: "pub fn parse(input: &[u8]) -> usize { input.len() }\n",
            expected_kind: DeclarationKind::Function,
            expected_base_projection: "pub fn parse(input: &str) -> usize",
            expected_head_projection: "pub fn parse(input: &[u8]) -> usize",
        },
        SurfaceEditCase {
            id: "field",
            base: "pub struct Packet {\n    pub id: u32,\n    payload: Vec<u8>,\n}\n",
            head: "pub struct Packet {\n    pub id: u64,\n    payload: Vec<u8>,\n}\n",
            expected_kind: DeclarationKind::Struct,
            expected_base_projection: "pub struct Packet {\n    pub id: u32,\n    payload: Vec<u8>,\n}",
            expected_head_projection: "pub struct Packet {\n    pub id: u64,\n    payload: Vec<u8>,\n}",
        },
        SurfaceEditCase {
            id: "variant",
            base: "pub enum State {\n    Ready,\n    Failed(u16),\n}\n",
            head: "pub enum State {\n    Ready,\n    Failed { code: u16 },\n}\n",
            expected_kind: DeclarationKind::Enum,
            expected_base_projection: "pub enum State {\n    Ready,\n    Failed(u16),\n}",
            expected_head_projection: "pub enum State {\n    Ready,\n    Failed { code: u16 },\n}",
        },
    ];

    for case in cases {
        let diff = diff_declarations(&[same_path_pair(
            case.id,
            "src/surface.rs",
            Some(case.base),
            Some(case.head),
        )])?;
        let unit = single_unit(&diff)?;
        let base = unit
            .base
            .as_ref()
            .context("changed unit must retain its base projection")?;
        let head = unit
            .head
            .as_ref()
            .context("changed unit must retain its head projection")?;

        assert_eq!(
            unit.change_kind,
            DeclarationChangeKind::Changed,
            "{}",
            case.id
        );
        assert_eq!(base.kind, case.expected_kind, "{} base kind", case.id);
        assert_eq!(head.kind, case.expected_kind, "{} head kind", case.id);
        assert_eq!(
            base.projection_text, case.expected_base_projection,
            "{} base",
            case.id
        );
        assert_eq!(
            head.projection_text, case.expected_head_projection,
            "{} head",
            case.id
        );
        assert_ne!(
            base.projection_hash, head.projection_hash,
            "{} must reopen",
            case.id
        );
        assert!(
            diff.diagnostics.is_empty(),
            "{}: {:?}",
            case.id,
            diff.diagnostics
        );
    }

    Ok(())
}

#[test]
fn additions_and_deletions_retain_the_only_available_exact_projection() -> Result<()> {
    const ADDED: &str = "pub fn created() -> u32 { 1 }\n";
    const DELETED: &str = "pub struct Removed {\n    pub reason: String,\n}\n";
    let diff = diff_declarations(&[
        same_path_pair("addition", "src/created.rs", None, Some(ADDED)),
        same_path_pair("deletion", "src/removed.rs", Some(DELETED), None),
    ])?;

    assert_eq!(diff.units.len(), 2);
    let added = diff
        .units
        .iter()
        .find(|unit| unit.change_kind == DeclarationChangeKind::Added)
        .context("missing added declaration unit")?;
    assert!(added.base.is_none());
    assert_eq!(
        added.head.as_ref().context("added head")?.projection_text,
        "pub fn created() -> u32"
    );

    let deleted = diff
        .units
        .iter()
        .find(|unit| unit.change_kind == DeclarationChangeKind::Deleted)
        .context("missing deleted declaration unit")?;
    assert!(deleted.head.is_none());
    assert_eq!(
        deleted
            .base
            .as_ref()
            .context("deleted base")?
            .projection_text,
        "pub struct Removed {\n    pub reason: String,\n}"
    );

    Ok(())
}

#[test]
fn same_name_overloads_match_by_signature_without_cross_pairing() -> Result<()> {
    const BASE: &str = "impl Codec {\n    /// Integer v1.\n    fn convert(&self, value: i32) -> i32 { value }\n    /// Text v1.\n    fn convert(&self, value: &str) -> usize { value.len() }\n}\n";
    const HEAD: &str = "impl Codec {\n    /// Text v2.\n    fn convert(&self, value: &str) -> usize { value.bytes().count() }\n    /// Integer v2.\n    fn convert(&self, value: i32) -> i32 { value.saturating_add(0) }\n}\n";
    let diff = diff_declarations(&[same_path_pair(
        "overloads",
        "src/codec.rs",
        Some(BASE),
        Some(HEAD),
    )])?;

    assert_eq!(diff.units.len(), 2);
    assert!(
        diff.units
            .iter()
            .all(|unit| unit.change_kind == DeclarationChangeKind::Changed)
    );

    let integer = diff
        .units
        .iter()
        .find(|unit| {
            unit.base
                .as_ref()
                .is_some_and(|base| base.projection_text.contains("value: i32"))
        })
        .context("missing integer overload")?;
    assert_eq!(
        integer
            .head
            .as_ref()
            .context("integer head")?
            .projection_text,
        "/// Integer v2.\nfn convert(&self, value: i32) -> i32"
    );

    let text = diff
        .units
        .iter()
        .find(|unit| {
            unit.base
                .as_ref()
                .is_some_and(|base| base.projection_text.contains("value: &str"))
        })
        .context("missing text overload")?;
    assert_eq!(
        text.head.as_ref().context("text head")?.projection_text,
        "/// Text v2.\nfn convert(&self, value: &str) -> usize"
    );
    assert!(diff.diagnostics.is_empty());

    Ok(())
}

#[test]
fn ambiguous_duplicate_declarations_are_not_arbitrarily_paired() -> Result<()> {
    const BASE: &str = "fn duplicate() -> u8 { 1 }\n\n\n\nfn duplicate() -> u8 { 2 }\n";
    const HEAD: &str = "\n\nfn duplicate() -> u8 { 3 }\n";
    let diff = diff_declarations(&[same_path_pair(
        "ambiguous-duplicates",
        "src/duplicates.rs",
        Some(BASE),
        Some(HEAD),
    )])?;

    assert!(
        diff.matches.is_empty(),
        "an ambiguous candidate must not be guessed"
    );
    assert_eq!(
        diff.units
            .iter()
            .filter(|unit| unit.change_kind == DeclarationChangeKind::Deleted)
            .count(),
        2
    );
    assert_eq!(
        diff.units
            .iter()
            .filter(|unit| unit.change_kind == DeclarationChangeKind::Added)
            .count(),
        1
    );
    assert!(diff.diagnostics.iter().any(|diagnostic| {
        diagnostic.snapshot_pair_id == SnapshotPairId::new("ambiguous-duplicates")
            && diagnostic.kind == DiffDiagnosticKind::AmbiguousDeclarationMatch
    }));

    Ok(())
}

#[test]
fn thousands_of_duplicate_declarations_remain_ambiguous_without_arbitrary_matches() -> Result<()> {
    const DECLARATIONS_PER_ENDPOINT: usize = 2_000;
    let base = "fn duplicate() -> u8 { 1 }\n".repeat(DECLARATIONS_PER_ENDPOINT);
    let head = "fn duplicate() -> u8 { 2 }\n".repeat(DECLARATIONS_PER_ENDPOINT);

    let diff = diff_declarations(&[same_path_pair(
        "thousands-of-ambiguous-duplicates",
        "src/duplicates.rs",
        Some(&base),
        Some(&head),
    )])?;

    assert!(
        diff.matches.is_empty(),
        "ambiguous duplicates must not be paired according to source order or another arbitrary tie-breaker"
    );
    assert_eq!(
        diff.units
            .iter()
            .filter(|unit| unit.change_kind == DeclarationChangeKind::Deleted)
            .count(),
        DECLARATIONS_PER_ENDPOINT,
        "every unmatched base declaration must remain a deletion"
    );
    assert_eq!(
        diff.units
            .iter()
            .filter(|unit| unit.change_kind == DeclarationChangeKind::Added)
            .count(),
        DECLARATIONS_PER_ENDPOINT,
        "every unmatched head declaration must remain an addition"
    );
    assert_eq!(
        diff.diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.snapshot_pair_id
                    == SnapshotPairId::new("thousands-of-ambiguous-duplicates")
                    && diagnostic.kind == DiffDiagnosticKind::AmbiguousDeclarationMatch
            })
            .count(),
        1,
        "the ambiguous declaration set must remain diagnosed"
    );
    assert!(
        diff.diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind != DiffDiagnosticKind::ProjectionDiagnostic),
        "the generated duplicate declarations must all remain syntactically parseable"
    );

    Ok(())
}

#[test]
fn pure_file_rename_requires_explicit_path_pair_evidence() -> Result<()> {
    const SOURCE: &str = "/// Stable API.\npub fn retained(value: u32) -> u32 { value }\n";
    let without_evidence = SnapshotPair::new(
        SnapshotPairId::new("rename-unproven"),
        Some(snapshot("rename-unproven-base", "src/old.rs", SOURCE)),
        Some(snapshot("rename-unproven-head", "src/new.rs", SOURCE)),
        PathPairEvidence::Unmatched,
    );
    let unproven = diff_declarations(&[without_evidence])?;
    assert_eq!(
        unproven.units.len(),
        2,
        "equal declarations at unrelated paths must remain a deletion and an addition"
    );
    assert!(unproven.matches.is_empty());

    let with_evidence = SnapshotPair::new(
        SnapshotPairId::new("rename-proven"),
        Some(snapshot("rename-proven-base", "src/old.rs", SOURCE)),
        Some(snapshot("rename-proven-head", "src/new.rs", SOURCE)),
        PathPairEvidence::ExplicitRename,
    );
    let proven = diff_declarations(&[with_evidence])?;
    assert!(
        proven.units.is_empty(),
        "a proven pure rename with an unchanged projection has no review target"
    );
    assert_eq!(proven.matches.len(), 1);
    assert_eq!(proven.matches[0].evidence, MatchingEvidence::ExactKey);
    assert_eq!(
        proven.matches[0].path_evidence,
        PathPairEvidence::ExplicitRename
    );

    Ok(())
}

#[test]
fn declaration_name_change_reopens_a_uniquely_matched_declaration() -> Result<()> {
    const BASE: &str = "pub fn old_name(value: u32) -> u32 { value }\n";
    const HEAD: &str = "pub fn new_name(value: u32) -> u32 { value }\n";
    let diff = diff_declarations(&[same_path_pair(
        "declaration-rename",
        "src/api.rs",
        Some(BASE),
        Some(HEAD),
    )])?;
    let unit = single_unit(&diff)?;

    assert_eq!(unit.change_kind, DeclarationChangeKind::Changed);
    assert_eq!(
        unit.matching_evidence,
        Some(MatchingEvidence::UniqueNameElided)
    );
    assert_eq!(
        unit.base.as_ref().context("rename base")?.projection_text,
        "pub fn old_name(value: u32) -> u32"
    );
    assert_eq!(
        unit.head.as_ref().context("rename head")?.projection_text,
        "pub fn new_name(value: u32) -> u32"
    );
    assert_ne!(
        unit.base.as_ref().context("rename base")?.projection_hash,
        unit.head.as_ref().context("rename head")?.projection_hash
    );

    Ok(())
}

#[test]
fn equal_hash_units_from_distinct_snapshot_pairs_are_never_deduplicated() -> Result<()> {
    const SOURCE: &str = "pub fn repeated() -> u8 { 1 }\n";
    let diff = diff_declarations(&[
        same_path_pair("commit-a", "src/repeated.rs", None, Some(SOURCE)),
        same_path_pair("commit-b", "src/repeated.rs", None, Some(SOURCE)),
    ])?;

    assert_eq!(diff.units.len(), 2);
    assert_eq!(diff.units[0].change_kind, DeclarationChangeKind::Added);
    assert_eq!(diff.units[1].change_kind, DeclarationChangeKind::Added);
    assert_eq!(
        diff.units[0]
            .head
            .as_ref()
            .context("first head")?
            .projection_hash,
        diff.units[1]
            .head
            .as_ref()
            .context("second head")?
            .projection_hash,
        "the fixture must exercise equal approval hashes"
    );
    let mut pair_ids = diff
        .units
        .iter()
        .map(|unit| unit.snapshot_pair_id.as_str())
        .collect::<Vec<_>>();
    pair_ids.sort_unstable();
    assert_eq!(pair_ids, ["commit-a", "commit-b"]);

    Ok(())
}
