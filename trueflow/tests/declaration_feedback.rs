use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use trueflow::analysis::Language;
use trueflow::commands::feedback::{
    build_pull_request_feedback_plan, feedback_entries_to_json_values, feedback_entries_to_xml,
};
use trueflow::commands::review::ResolvedReviewQuery;
use trueflow::config::BlockFilters;
use trueflow::declaration::capture::{capture_declaration_sources, CaptureBatch};
use trueflow::declaration::diff::diff_declarations;
use trueflow::declaration::snapshot::{SnapshotId, SourceSnapshot};
use trueflow::declaration::{
    project_source, DeclarationNode, DeclarationProjectionHash, SourceComponentRole,
};
use trueflow::feedback_export::{
    collect_feedback_entries, resolve_declaration_feedback, DeclarationFeedbackSource,
    FeedbackContextResolver, FeedbackEntry, FeedbackEntryKind, FeedbackQuery, FeedbackSinceFilter,
    ResolvedFeedbackContext,
};
use trueflow::github::{
    GitHubCommentSide, PullRequestCommit, PullRequestMetadata, ResolvedPullRequestRef,
};
use trueflow::hashing::BytesHash;
use trueflow::repo_path::RepoPath;
use trueflow::scanner::ScanOptions;
use trueflow::store::{
    BlockState, CommentAnchor, CommitId, DeclarationAnchorRange, DeclarationCommentAnchor,
    DeclarationRecordLocator, Identity, Record, RepoRef, ReviewCheck, ReviewIndex, ReviewTargetRef,
    ReviewedDeclarationSnapshot, VcsSystem, Verdict,
};
use trueflow::targets::{
    ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection,
};
use trueflow::vcs::ChangedPath;
use trueflow_test_support::{run_git_output, FeedbackScenario, TestRepo};

const PATH: &str = "src/lib.rs";
const BODY_SENTINEL: &str = "EXECUTABLE BODY SENTINEL MUST NEVER EXPORT";
const DECLARATION_SOURCE: &str = r#"/// Converts one value.
pub fn convert(value: u8) -> u8 {
    let hidden = "EXECUTABLE BODY SENTINEL MUST NEVER EXPORT";
    value
}
"#;

type TestResult = Result<()>;

#[derive(Clone, Copy)]
enum AnchorSelection {
    Signature,
    DocumentationAndSignature,
}

struct PullRequestFixture {
    repo: TestRepo,
    metadata: PullRequestMetadata,
    revisions: Vec<CommitId>,
}

fn commit_id(repo: &TestRepo, revision: &str) -> Result<CommitId> {
    CommitId::new(run_git_output(&repo.path, &["rev-parse", revision])?)
}

fn revision_query(revision: CommitId) -> ResolvedReviewQuery {
    ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Revision(revision.clone()),
        path_selection: ReviewPathSelection::All,
        diff_selection: ReviewDiffSelection::Targets(vec![ReviewDiffTarget::Revision(revision)]),
    }
}

fn captured_declaration(
    repo: &TestRepo,
    revision: &CommitId,
) -> Result<(SourceSnapshot, DeclarationNode)> {
    let batches = capture_declaration_sources(&repo.path, &revision_query(revision.clone()))?;
    let batch = match batches.as_slice() {
        [batch] => batch,
        other => bail!("expected one captured revision batch, got {}", other.len()),
    };
    declaration_from_batch(batch, "convert")
}

fn declaration_from_batch(
    batch: &CaptureBatch,
    name: &str,
) -> Result<(SourceSnapshot, DeclarationNode)> {
    let diff = diff_declarations(&batch.pairs)?;
    let declaration = diff
        .units
        .iter()
        .filter_map(|unit| unit.head.as_ref())
        .find(|declaration| declaration.name == name)
        .with_context(|| format!("missing projected declaration {name}"))?
        .clone();
    let snapshot_id = diff
        .units
        .iter()
        .find(|unit| {
            unit.head
                .as_ref()
                .is_some_and(|head| head.id == declaration.id)
        })
        .and_then(|unit| unit.head_snapshot_id.as_ref())
        .context("projected declaration must retain its exact head snapshot")?;
    let snapshot = batch
        .pairs
        .iter()
        .filter_map(|pair| pair.head.as_ref())
        .find(|snapshot| &snapshot.id == snapshot_id)
        .context("captured batch must contain the declaration source snapshot")?
        .clone();
    Ok((snapshot, declaration))
}

fn projected_snapshot(
    id: &str,
    path: &str,
    source: &str,
) -> Result<(SourceSnapshot, DeclarationNode)> {
    let snapshot =
        SourceSnapshot::new(SnapshotId::new(id), Path::new(path), Language::Rust, source);
    let declaration = project_source(Path::new(path), Language::Rust, source)?
        .declarations()
        .iter()
        .find(|declaration| declaration.name == "convert")
        .context("fixture must project convert")?
        .clone();
    Ok((snapshot, declaration))
}

fn reviewed_snapshot(snapshot: &SourceSnapshot) -> ReviewedDeclarationSnapshot {
    ReviewedDeclarationSnapshot {
        snapshot_id: snapshot.id.as_str().to_string(),
        content_hash: snapshot.bytes_hash().clone(),
    }
}

fn selected_ranges(
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
    selection: AnchorSelection,
) -> Result<Vec<DeclarationAnchorRange>> {
    let roles: &[SourceComponentRole] = match selection {
        AnchorSelection::Signature => &[SourceComponentRole::Signature],
        AnchorSelection::DocumentationAndSignature => &[
            SourceComponentRole::Documentation,
            SourceComponentRole::Signature,
        ],
    };
    roles
        .iter()
        .map(|role| {
            let component = declaration
                .components
                .iter()
                .find(|component| component.role == *role)
                .with_context(|| format!("fixture declaration must have a {role:?} component"))?;
            let exact_text = snapshot
                .source()
                .get(component.source_range.clone())
                .context("component range must be an exact source slice")?;
            Ok(DeclarationAnchorRange {
                start_byte: component.source_range.start,
                end_byte: component.source_range.end,
                exact_text: exact_text.to_string(),
            })
        })
        .collect()
}

fn declaration_record(
    id: &str,
    revision: &CommitId,
    snapshot: &SourceSnapshot,
    declaration: &DeclarationNode,
    selection: AnchorSelection,
    note: &str,
) -> Result<Record> {
    let reviewed_snapshot = reviewed_snapshot(snapshot);
    let mut record = Record::new(
        ReviewTargetRef::Declaration {
            hash: declaration.projection_hash.clone(),
        },
        ReviewCheck::declaration(),
        Verdict::Comment,
        Identity::Email {
            email: "reviewer@example.com".to_string(),
        },
        RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: revision.clone(),
        },
        BlockState::Committed,
    );
    record.id = id.to_string();
    record.timestamp = 1_700_000_000;
    record.note = Some(note.to_string());
    record.comment_context = Some("relationship: used by crate::caller".to_string());
    record.declaration_locator = Some(DeclarationRecordLocator {
        path: RepoPath::new(
            snapshot
                .path
                .to_str()
                .context("fixture snapshot path must be UTF-8")?,
        )?,
        declaration_key: declaration.key.clone(),
        source_ordinal: declaration.source_ordinal,
        source_span: declaration.source_span.clone(),
        reviewed_snapshot: reviewed_snapshot.clone(),
        projection_hash: declaration.projection_hash.clone(),
    });
    record.comment_anchor = Some(CommentAnchor::Declaration(DeclarationCommentAnchor {
        reviewed_snapshot,
        projection_hash: declaration.projection_hash.clone(),
        source_len_bytes: snapshot.source().len(),
        ranges: selected_ranges(snapshot, declaration, selection)?,
    }));
    record.validate()?;
    Ok(record)
}

fn declaration_anchor_mut(record: &mut Record) -> Result<&mut DeclarationCommentAnchor> {
    match record.comment_anchor.as_mut() {
        Some(CommentAnchor::Declaration(anchor)) => Ok(anchor),
        other => bail!("expected declaration anchor, got {other:?}"),
    }
}

fn locator_mut(record: &mut Record) -> Result<&mut DeclarationRecordLocator> {
    record
        .declaration_locator
        .as_mut()
        .context("expected declaration locator")
}

fn declaration_json_entry<'a>(entries: &'a [Value], id: &str) -> Result<&'a Value> {
    entries
        .iter()
        .find(|entry| {
            entry["reviews"]
                .as_array()
                .is_some_and(|reviews| reviews.iter().any(|review| review["id"] == id))
        })
        .with_context(|| format!("missing exported entry for {id}"))
}

#[test]
fn declaration_comment_json_and_xml_retain_exact_semantic_surface_without_body() -> TestResult {
    let (snapshot, declaration) = projected_snapshot("source:exported", PATH, DECLARATION_SOURCE)?;
    let revision = CommitId::new("0123456789abcdef")?;
    let record = declaration_record(
        "declaration-comment",
        &revision,
        &snapshot,
        &declaration,
        AnchorSelection::DocumentationAndSignature,
        "please clarify the public contract",
    )?;
    let expected_ranges = match record.comment_anchor.as_ref() {
        Some(CommentAnchor::Declaration(anchor)) => serde_json::to_value(&anchor.ranges)?,
        _ => bail!("fixture record lost its declaration anchor"),
    };
    let resolved = resolve_declaration_feedback(
        &record,
        &[DeclarationFeedbackSource::from_snapshot(snapshot)?],
    )?
    .context("exact declaration feedback must resolve before export")?;
    let entry = FeedbackEntry {
        kind: FeedbackEntryKind::Declaration,
        file_path: PATH.to_string(),
        block: None,
        declaration: Some(resolved),
        reviews: vec![record],
        latest_verdict: "comment".to_string(),
    };

    let entries = feedback_entries_to_json_values(std::slice::from_ref(&entry));
    let entry_json = declaration_json_entry(&entries, "declaration-comment")?;
    assert_eq!(
        entry_json["target"],
        json!({
            "kind": "declaration",
            "semantic_key": declaration.key.as_str(),
        })
    );
    assert_eq!(entry_json["declaration"]["path"], PATH);
    assert_eq!(entry_json["declaration"]["ranges"], expected_ranges);
    assert_eq!(
        entry_json["declaration"]["projection_text"],
        declaration.projection_text
    );
    assert_eq!(
        entry_json["declaration"]["context"],
        "relationship: used by crate::caller"
    );
    let json_text = serde_json::to_string(entry_json)?;
    assert!(!json_text.contains(BODY_SENTINEL));

    let xml = feedback_entries_to_xml(std::slice::from_ref(&entry))?;
    assert!(xml.contains("target_kind=\"declaration\""));
    assert!(xml.contains(&format!("semantic_key=\"{}\"", declaration.key.as_str())));
    assert!(xml.contains("path=\"src/lib.rs\""));
    for range in expected_ranges
        .as_array()
        .context("expected serialized declaration ranges")?
    {
        assert!(xml.contains(&format!(
            "start_byte=\"{}\" end_byte=\"{}\"",
            range["start_byte"], range["end_byte"]
        )));
        assert!(xml.contains(
            range["exact_text"]
                .as_str()
                .context("range exact_text must be a string")?
        ));
    }
    assert!(xml.contains(&declaration.projection_text));
    assert!(xml.contains("relationship: used by crate::caller"));
    assert!(!xml.contains(BODY_SENTINEL));
    Ok(())
}

#[test]
fn declaration_resolution_requires_exact_snapshot_hash_path_and_valid_anchor() -> TestResult {
    let (snapshot, declaration) = projected_snapshot("source:captured", PATH, DECLARATION_SOURCE)?;
    let revision = CommitId::new("0123456789abcdef")?;
    let record = declaration_record(
        "exact-resolution",
        &revision,
        &snapshot,
        &declaration,
        AnchorSelection::Signature,
        "exactly anchored note",
    )?;
    let source = DeclarationFeedbackSource::from_snapshot(snapshot.clone())?;
    let resolved = resolve_declaration_feedback(&record, std::slice::from_ref(&source))?
        .context("an exact declaration locator and anchor must resolve")?;
    assert_eq!(resolved.path, RepoPath::new(PATH)?);
    assert_eq!(resolved.semantic_key, declaration.key);
    assert_eq!(resolved.projection_text, declaration.projection_text);
    assert_eq!(
        resolved.context.as_deref(),
        Some("relationship: used by crate::caller")
    );

    let mut wrong_snapshot = record.clone();
    locator_mut(&mut wrong_snapshot)?
        .reviewed_snapshot
        .snapshot_id = "source:not-captured".to_string();
    declaration_anchor_mut(&mut wrong_snapshot)?
        .reviewed_snapshot
        .snapshot_id = "source:not-captured".to_string();

    let other_hash = DeclarationProjectionHash::new(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let mut wrong_hash = record.clone();
    wrong_hash.target = ReviewTargetRef::Declaration {
        hash: other_hash.clone(),
    };
    locator_mut(&mut wrong_hash)?.projection_hash = other_hash.clone();
    declaration_anchor_mut(&mut wrong_hash)?.projection_hash = other_hash;

    let mut wrong_path = record.clone();
    locator_mut(&mut wrong_path)?.path = RepoPath::new("src/current_name.rs")?;

    for (case, candidate) in [
        ("snapshot identity", wrong_snapshot),
        ("projection hash", wrong_hash),
        ("persisted path", wrong_path),
    ] {
        assert!(
            resolve_declaration_feedback(&candidate, std::slice::from_ref(&source))?.is_none(),
            "resolver accepted the wrong {case}"
        );
    }

    let mut out_of_bounds = record;
    let source_len = snapshot.source().len();
    declaration_anchor_mut(&mut out_of_bounds)?.ranges = vec![DeclarationAnchorRange {
        start_byte: source_len,
        end_byte: source_len + 1,
        exact_text: "x".to_string(),
    }];
    assert!(
        resolve_declaration_feedback(&out_of_bounds, &[source]).is_err(),
        "an out-of-bounds persisted anchor must fail closed"
    );
    Ok(())
}

#[test]
fn declaration_resolution_does_not_fall_back_to_ambiguous_equal_projections() -> TestResult {
    let (captured, declaration) = projected_snapshot("source:captured", PATH, DECLARATION_SOURCE)?;
    let revision = CommitId::new("0123456789abcdef")?;
    let mut record = declaration_record(
        "ambiguous-projection",
        &revision,
        &captured,
        &declaration,
        AnchorSelection::Signature,
        "do not bind by name",
    )?;
    locator_mut(&mut record)?.reviewed_snapshot.snapshot_id = "source:missing".to_string();
    declaration_anchor_mut(&mut record)?
        .reviewed_snapshot
        .snapshot_id = "source:missing".to_string();

    let (first, first_declaration) = projected_snapshot("source:first", PATH, DECLARATION_SOURCE)?;
    let (second, second_declaration) =
        projected_snapshot("source:second", PATH, DECLARATION_SOURCE)?;
    assert_eq!(first_declaration.key, second_declaration.key);
    assert_eq!(
        first_declaration.projection_hash, second_declaration.projection_hash,
        "fixture must contain equal projections"
    );
    let candidates = [
        DeclarationFeedbackSource::from_snapshot(first)?,
        DeclarationFeedbackSource::from_snapshot(second)?,
    ];

    assert!(
        resolve_declaration_feedback(&record, &candidates)?.is_none(),
        "equal current projections must not replace the missing captured source identity"
    );
    Ok(())
}

struct CountingOrdinaryResolver {
    calls: Cell<usize>,
}

impl FeedbackContextResolver for CountingOrdinaryResolver {
    fn resolve_context(&mut self, _record: &Record) -> Result<ResolvedFeedbackContext> {
        self.calls.set(self.calls.get() + 1);
        bail!("the ordinary block resolver must not receive declaration records")
    }
}

#[test]
fn ordinary_feedback_export_retains_unresolved_declaration_while_review_index_skips_it(
) -> TestResult {
    let (snapshot, declaration) =
        projected_snapshot("source:ordinary-skip", PATH, DECLARATION_SOURCE)?;
    let record = declaration_record(
        "declaration-only",
        &CommitId::new("0123456789abcdef")?,
        &snapshot,
        &declaration,
        AnchorSelection::Signature,
        "dedicated path only",
    )?;
    let mut resolver = CountingOrdinaryResolver {
        calls: Cell::new(0),
    };
    let entries = collect_feedback_entries(
        std::slice::from_ref(&record),
        &FeedbackSinceFilter::All,
        &FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: true,
        },
        &mut resolver,
    )?;
    let [entry] = entries.as_slice() else {
        bail!(
            "expected one unresolved declaration feedback entry, got {}",
            entries.len()
        );
    };
    let resolution_error = match &entry.kind {
        FeedbackEntryKind::DeclarationResolutionFailed { reason } => reason,
        other => bail!("expected a declaration resolution failure, got {other:?}"),
    };
    assert!(!resolution_error.trim().is_empty());
    assert!(entry.declaration.is_none());
    assert!(entry.block.is_none());
    assert_eq!(entry.reviews.len(), 1);
    assert_eq!(
        serde_json::to_value(&entry.reviews[0])?,
        serde_json::to_value(&record)?,
        "the unresolved entry must retain the original review record"
    );

    let rendered = feedback_entries_to_json_values(&entries);
    let [rendered] = rendered.as_slice() else {
        bail!(
            "expected one rendered feedback entry, got {}",
            rendered.len()
        );
    };
    assert_eq!(rendered["target"]["kind"], "declaration");
    assert!(
        rendered["resolution_error"]
            .as_str()
            .is_some_and(|reason| !reason.trim().is_empty()),
        "rendered unresolved declarations must explain the resolution failure"
    );
    assert_eq!(resolver.calls.get(), 0);

    let colliding_block = ReviewTargetRef::Block {
        hash: trueflow::store::TreeHash::parse(declaration.projection_hash.as_str())?,
    };
    let approved = ReviewIndex::from_records(&[record], None).approved_targets();
    assert!(!approved.contains_target(&colliding_block));
    assert!(!approved.contains_target(&ReviewTargetRef::Declaration {
        hash: declaration.projection_hash,
    }));
    Ok(())
}

#[test]
fn mixed_block_and_declaration_history_exports_each_through_its_own_shape() -> TestResult {
    let scenario = FeedbackScenario::new("mixed_declaration_feedback")?;
    let source = "pub fn convert(value: u8) -> u8 { value }\n";
    scenario.write(PATH, source)?;
    let revision = CommitId::new(scenario.commit_all("add mixed feedback source")?)?;
    let block = scenario.review_block_in_process(PATH, "comment")?;
    let (snapshot, declaration) = captured_declaration(scenario.repo(), &revision)?;
    let declaration_record = declaration_record(
        "mixed-declaration",
        &revision,
        &snapshot,
        &declaration,
        AnchorSelection::Signature,
        "declaration note",
    )?;
    scenario.write_reviews(&[block.clone(), declaration_record])?;

    let entries = scenario.feedback_json_in_process(&["--since", "all"])?;
    assert_eq!(entries.len(), 2);
    let block_entry = declaration_json_entry(&entries, &block.id)?;
    assert_eq!(block_entry["target"]["kind"], "block");
    assert_eq!(
        block_entry["block"]["content"],
        source.trim_end_matches('\n')
    );
    assert!(block_entry.get("declaration").is_none());

    let declaration_entry = declaration_json_entry(&entries, "mixed-declaration")?;
    assert_eq!(declaration_entry["target"]["kind"], "declaration");
    assert_eq!(
        declaration_entry["target"]["semantic_key"],
        declaration.key.as_str()
    );
    assert!(declaration_entry.get("block").is_none());
    Ok(())
}

#[test]
fn declaration_feedback_uses_newest_timestamp_when_older_comment_is_stored_last() -> TestResult {
    const STALE_NOTE: &str = "stale declaration comment must not export";

    let scenario = FeedbackScenario::new("declaration_feedback_latest_timestamp")?;
    scenario.write(PATH, DECLARATION_SOURCE)?;
    let revision = CommitId::new(scenario.commit_all("add declaration feedback source")?)?;
    let (snapshot, declaration) = captured_declaration(scenario.repo(), &revision)?;

    let mut older_comment = declaration_record(
        "older-comment",
        &revision,
        &snapshot,
        &declaration,
        AnchorSelection::Signature,
        STALE_NOTE,
    )?;
    older_comment.timestamp = 1_700_000_000;
    older_comment.validate()?;

    let mut newer_approval = older_comment.clone();
    newer_approval.id = "newer-approval".to_string();
    newer_approval.timestamp = 1_700_000_001;
    newer_approval.verdict = Verdict::Approved;
    newer_approval.note = None;
    newer_approval.validate()?;

    // The file order intentionally opposes time order: the stale comment is physically last.
    scenario.write_reviews(&[newer_approval, older_comment])?;

    let entries = scenario.feedback_json_in_process(&["--since", "all"])?;
    assert!(
        entries.is_empty(),
        "the newer approval must suppress feedback for its exact declaration locator; \
         stale note {STALE_NOTE:?} was exported in {entries:#?}"
    );
    Ok(())
}

#[test]
fn uncommitted_declaration_feedback_resolves_against_captured_worktree_snapshot() -> TestResult {
    const INITIAL_SOURCE: &str =
        "/// Converts one wide value.\npub fn convert(value: u16) -> u16 { value }\n";
    const RECORD_ID: &str = "uncommitted-declaration";
    const NOTE: &str = "the dirty declaration contract needs revision";
    const EXPECTED_PROJECTION: &str = "/// Converts one value.\npub fn convert(value: u8) -> u8";

    let scenario = FeedbackScenario::new("uncommitted_declaration_feedback")?;
    scenario.write(PATH, INITIAL_SOURCE)?;
    let revision = CommitId::new(scenario.commit_all("add initial declaration")?)?;
    scenario.write(PATH, DECLARATION_SOURCE)?;

    let changed = HashSet::from([ChangedPath::identity(RepoPath::new(PATH)?)]);
    let query = ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Workdir,
        path_selection: ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: Vec::new(),
            changed: Some(changed),
        },
        diff_selection: ReviewDiffSelection::None,
    };
    let batches = capture_declaration_sources(&scenario.repo().path, &query)?;
    let batch = match batches.as_slice() {
        [batch] => batch,
        other => bail!("expected one dirty capture batch, got {}", other.len()),
    };
    let (snapshot, declaration) = declaration_from_batch(batch, "convert")?;
    assert_eq!(
        snapshot.source(),
        DECLARATION_SOURCE,
        "the record fixture must bind to the dirty worktree bytes"
    );

    let mut record = declaration_record(
        RECORD_ID,
        &revision,
        &snapshot,
        &declaration,
        AnchorSelection::DocumentationAndSignature,
        NOTE,
    )?;
    record.block_state = BlockState::Uncommitted;
    record.validate()?;
    let expected_ranges = match record.comment_anchor.as_ref() {
        Some(CommentAnchor::Declaration(anchor)) => serde_json::to_value(&anchor.ranges)?,
        _ => bail!("fixture record lost its declaration anchor"),
    };
    scenario.write_reviews(std::slice::from_ref(&record))?;

    let entries = scenario.feedback_json_in_process(&["--since", "all"])?;
    assert!(
        entries.iter().any(|entry| {
            entry["reviews"]
                .as_array()
                .is_some_and(|reviews| reviews.iter().any(|review| review["id"] == RECORD_ID))
        }),
        "uncommitted declaration feedback must resolve against its captured worktree snapshot; \
         exported entries: {entries:#?}"
    );

    let entry = declaration_json_entry(&entries, RECORD_ID)?;
    assert_eq!(
        entry["target"],
        json!({
            "kind": "declaration",
            "semantic_key": declaration.key.as_str(),
        })
    );
    assert_eq!(entry["declaration"]["path"], PATH);
    assert_eq!(entry["declaration"]["ranges"], expected_ranges);
    assert_eq!(entry["declaration"]["projection_text"], EXPECTED_PROJECTION);
    assert_eq!(entry["reviews"][0]["note"], NOTE);
    assert!(
        !serde_json::to_string(entry)?.contains(BODY_SENTINEL),
        "declaration feedback must not export executable body text"
    );
    Ok(())
}

fn pull_request_fixture(name: &str, versions: &[&str]) -> Result<PullRequestFixture> {
    let [base, feature_versions @ ..] = versions else {
        bail!("pull request fixture requires a base version")
    };
    let repo = TestRepo::new(name)?;
    repo.write(PATH, base)?;
    repo.commit_all("base")?;
    repo.git(&["branch", "-M", "main"])?;
    let base_sha = commit_id(&repo, "HEAD")?;
    repo.git(&["switch", "-c", "feature/declaration-feedback"])?;

    let mut revisions = Vec::new();
    for (index, contents) in feature_versions.iter().enumerate() {
        repo.write(PATH, contents)?;
        repo.commit_all(&format!("feature version {index}"))?;
        revisions.push(commit_id(&repo, "HEAD")?);
    }
    let head_sha = revisions
        .last()
        .cloned()
        .context("pull request fixture requires at least one feature revision")?;
    let metadata = PullRequestMetadata {
        pr: ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "trueflow".to_string(),
            repo: "trueflow".to_string(),
            number: 7,
        },
        title: "Declaration feedback".to_string(),
        base_ref: "main".to_string(),
        base_sha,
        head_ref: "feature/declaration-feedback".to_string(),
        head_sha,
        commits: revisions
            .iter()
            .enumerate()
            .map(|(index, sha)| PullRequestCommit {
                sha: sha.clone(),
                summary: format!("feature version {index}"),
            })
            .collect(),
    };
    Ok(PullRequestFixture {
        repo,
        metadata,
        revisions,
    })
}

fn record_at_source(
    fixture: &PullRequestFixture,
    revision_index: usize,
    source: &str,
    selection: AnchorSelection,
    id: &str,
    note: &str,
) -> Result<Record> {
    let revision = fixture
        .revisions
        .get(revision_index)
        .context("fixture revision index must exist")?;
    let (snapshot, declaration) = projected_snapshot(&format!("source:{id}"), PATH, source)?;
    declaration_record(id, revision, &snapshot, &declaration, selection, note)
}

#[test]
fn github_declaration_mapper_inlines_one_exact_added_or_context_anchor() -> TestResult {
    let cases = [
        (
            "declaration_feedback_github_added",
            "",
            DECLARATION_SOURCE,
            2,
            "added declaration note",
        ),
        (
            "declaration_feedback_github_context",
            "/// Converts one value.\npub fn convert(value: u8) -> u8 {\n    value\n}\n",
            "/// Converts one value.\npub fn convert(value: u8) -> u8 {\n    value.saturating_add(1)\n}\n",
            2,
            "context declaration note",
        ),
    ];

    for (name, base, head, expected_line, note) in cases {
        let fixture = pull_request_fixture(name, &[base, head])?;
        let record = record_at_source(&fixture, 0, head, AnchorSelection::Signature, name, note)?;
        let repo = gix::discover(&fixture.repo.path)?;
        let plan = build_pull_request_feedback_plan(
            &repo,
            &fixture.metadata,
            std::slice::from_ref(&record),
            &HashSet::new(),
        )?;

        assert_eq!(plan.staged_record_ids, vec![name.to_string()]);
        assert_eq!(plan.draft.comments.len(), 1, "{name}");
        let comment = &plan.draft.comments[0];
        assert_eq!(comment.path, RepoPath::new(PATH)?, "{name}");
        assert_eq!(comment.line, expected_line, "{name}");
        assert_eq!(comment.side, GitHubCommentSide::Right, "{name}");
        assert_eq!(comment.start_line, None, "{name}");
        assert_eq!(comment.start_side, None, "{name}");
        assert_eq!(comment.body, note, "{name}");
        assert!(
            !plan.draft.body.contains(note),
            "{name} was duplicated as general feedback"
        );
    }
    Ok(())
}

#[test]
fn github_declaration_mapper_uses_general_feedback_without_inventing_coordinates() -> TestResult {
    let multi = pull_request_fixture(
        "declaration_feedback_github_multi_range",
        &["", DECLARATION_SOURCE],
    )?;
    let multi_record = record_at_source(
        &multi,
        0,
        DECLARATION_SOURCE,
        AnchorSelection::DocumentationAndSignature,
        "multi-range",
        "multi-range declaration note",
    )?;

    let rewritten_source =
        "/// Converts one value.\npub fn convert(value: u16) -> u16 {\n    value\n}\n";
    let rewritten = pull_request_fixture(
        "declaration_feedback_github_unmappable",
        &["", DECLARATION_SOURCE, rewritten_source],
    )?;
    let rewritten_record = record_at_source(
        &rewritten,
        0,
        DECLARATION_SOURCE,
        AnchorSelection::Signature,
        "rewritten-anchor",
        "unmappable declaration note",
    )?;

    let deleted = pull_request_fixture(
        "declaration_feedback_github_deleted",
        &["", DECLARATION_SOURCE, ""],
    )?;
    let deleted_record = record_at_source(
        &deleted,
        0,
        DECLARATION_SOURCE,
        AnchorSelection::Signature,
        "deleted-anchor",
        "deleted declaration note",
    )?;

    for (case, fixture, record, note) in [
        (
            "multi-range",
            multi,
            multi_record,
            "multi-range declaration note",
        ),
        (
            "unmappable",
            rewritten,
            rewritten_record,
            "unmappable declaration note",
        ),
        (
            "deleted",
            deleted,
            deleted_record,
            "deleted declaration note",
        ),
    ] {
        let repo = gix::discover(&fixture.repo.path)?;
        let plan = build_pull_request_feedback_plan(
            &repo,
            &fixture.metadata,
            std::slice::from_ref(&record),
            &HashSet::new(),
        )?;

        assert!(
            plan.draft.comments.is_empty(),
            "{case} fabricated an inline coordinate"
        );
        assert_eq!(plan.staged_record_ids, vec![record.id.clone()], "{case}");
        assert!(
            plan.draft.body.contains(note),
            "{case} feedback was dropped"
        );
        let semantic_key = record
            .declaration_locator
            .as_ref()
            .context("declaration record must retain its locator")?
            .declaration_key
            .as_str();
        assert!(
            plan.draft.body.contains(semantic_key),
            "{case} general feedback lost declaration identity"
        );
        assert!(!plan.draft.body.contains("src/lib.rs:0"), "{case}");
        assert!(!plan.draft.body.contains("line 0"), "{case}");
    }
    Ok(())
}

#[test]
fn exact_resolution_rejects_same_snapshot_id_with_wrong_source_hash() -> TestResult {
    let (snapshot, declaration) =
        projected_snapshot("source:hash-bound", PATH, DECLARATION_SOURCE)?;
    let record = declaration_record(
        "wrong-source-hash",
        &CommitId::new("0123456789abcdef")?,
        &snapshot,
        &declaration,
        AnchorSelection::Signature,
        "hash-bound note",
    )?;
    let changed_source = DECLARATION_SOURCE.replace("value\n}", "value.saturating_add(1)\n}");
    let impostor =
        SourceSnapshot::new(snapshot.id, Path::new(PATH), Language::Rust, changed_source);
    assert_ne!(
        impostor.bytes_hash(),
        &BytesHash::from_bytes(DECLARATION_SOURCE.as_bytes())
    );

    assert!(
        resolve_declaration_feedback(
            &record,
            &[DeclarationFeedbackSource::from_snapshot(impostor)?],
        )?
        .is_none(),
        "snapshot ID equality must not bypass the persisted content hash"
    );
    Ok(())
}
