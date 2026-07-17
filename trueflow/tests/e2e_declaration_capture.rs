use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use trueflow::analysis::Language;
use trueflow::commands::review::ResolvedReviewQuery;
use trueflow::config::BlockFilters;
use trueflow::declaration::capture::{
    CaptureBatch, CaptureEndpointProvenance, capture_declaration_sources,
    capture_declaration_sources_with_hook,
};
use trueflow::declaration::diff::diff_declarations;
use trueflow::declaration::snapshot::{PathPairEvidence, SnapshotPair};
use trueflow::repo_path::RepoPath;
use trueflow::scanner::ScanOptions;
use trueflow::store::{BlockState, CommitId, RepoRef, VcsSystem};
use trueflow::targets::{
    CommitRange, ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection,
};
use trueflow::vcs::ChangedPath;
use trueflow_test_support::{TestRepo, run_git_output};

const CAPTURE_DRIFT_ERROR: &str = "worktree changed during declaration capture; retry";

fn commit_id(repo: &TestRepo, revision: &str) -> Result<CommitId> {
    CommitId::new(run_git_output(&repo.path, &["rev-parse", revision])?)
}

fn query(
    content_source: ReviewContentSource,
    path_selection: ReviewPathSelection,
    diff_selection: ReviewDiffSelection,
) -> ResolvedReviewQuery {
    ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source,
        path_selection,
        diff_selection,
    }
}

fn worktree_query(path_selection: ReviewPathSelection) -> ResolvedReviewQuery {
    query(
        ReviewContentSource::Workdir,
        path_selection,
        ReviewDiffSelection::None,
    )
}

fn revision_query(revision: CommitId) -> ResolvedReviewQuery {
    query(
        ReviewContentSource::Revision(revision.clone()),
        ReviewPathSelection::All,
        ReviewDiffSelection::Targets(vec![ReviewDiffTarget::Revision(revision)]),
    )
}

fn range_query(start: CommitId, end: CommitId) -> ResolvedReviewQuery {
    query(
        ReviewContentSource::Revision(end.clone()),
        ReviewPathSelection::All,
        ReviewDiffSelection::Targets(vec![ReviewDiffTarget::RevisionRange(CommitRange {
            start,
            end,
        })]),
    )
}

fn dirty_query(paths: &[&str]) -> Result<ResolvedReviewQuery> {
    let changed = paths
        .iter()
        .map(|path| RepoPath::new(*path).map(ChangedPath::identity))
        .collect::<Result<HashSet<_>>>()?;
    Ok(worktree_query(ReviewPathSelection::Scoped {
        files: HashSet::new(),
        dirs: Vec::new(),
        changed: Some(changed),
    }))
}

fn one_batch(batches: &[CaptureBatch]) -> Result<&CaptureBatch> {
    match batches {
        [batch] => Ok(batch),
        _ => bail!("expected exactly one capture batch, got {}", batches.len()),
    }
}

fn one_pair(batch: &CaptureBatch) -> Result<&SnapshotPair> {
    match batch.pairs.as_slice() {
        [pair] => Ok(pair),
        pairs => bail!("expected exactly one snapshot pair, got {}", pairs.len()),
    }
}

fn pair_for_display_path<'a>(batch: &'a CaptureBatch, path: &str) -> Result<&'a SnapshotPair> {
    batch
        .pairs
        .iter()
        .find(|pair| {
            pair.head
                .as_ref()
                .map_or_else(|| pair.base.as_ref(), |_| pair.head.as_ref())
                .is_some_and(|snapshot| snapshot.path == Path::new(path))
        })
        .with_context(|| format!("missing captured pair displayed at {path}"))
}

fn assert_endpoint(endpoint: &CaptureEndpointProvenance, revision: &CommitId, state: &BlockState) {
    assert_eq!(
        endpoint.repo_ref,
        RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: revision.clone(),
        }
    );
    assert_eq!(&endpoint.block_state, state);
}

fn head_inventory(batch: &CaptureBatch) -> Vec<(String, Language, String)> {
    let mut inventory = batch
        .pairs
        .iter()
        .filter_map(|pair| pair.head.as_ref())
        .map(|snapshot| {
            (
                snapshot.path.to_string_lossy().into_owned(),
                snapshot.language,
                snapshot.source().to_string(),
            )
        })
        .collect::<Vec<_>>();
    inventory.sort_by(|left, right| left.0.cmp(&right.0));
    inventory
}

#[test]
fn all_file_and_dir_worktree_scopes_capture_only_selected_supported_sources_byte_exactly()
-> Result<()> {
    let repo = TestRepo::new("declaration_capture_worktree_scopes")?;
    let sources = [
        (
            "native/api.c",
            Language::C,
            "int café(void) {\r\n    return 1;\r\n}",
        ),
        (
            "native/api.cpp",
            Language::Cpp,
            "int widget() { return 2; }\n",
        ),
        (
            "src/exact.rs",
            Language::Rust,
            "pub fn café() -> &'static str {\r\n    \"🦀\"\r\n}",
        ),
        (
            "src/model.py",
            Language::Python,
            "def café() -> str:\n    return \"☕\"\n",
        ),
        (
            "src/nested/model.go",
            Language::Go,
            "package nested\n\nfunc Café() string { return \"coffee\" }\n",
        ),
        (
            "web/widget.ts",
            Language::TypeScript,
            "export function café(): string { return \"coffee\"; }\n",
        ),
    ];
    for (path, _, source) in sources {
        repo.write(path, source)?;
    }
    repo.commit_all("supported sources")?;
    let head = commit_id(&repo, "HEAD")?;

    let all_capture =
        capture_declaration_sources(&repo.path, &worktree_query(ReviewPathSelection::All))?;
    let all_batch = one_batch(&all_capture)?;
    assert!(
        all_batch.diagnostics.is_empty(),
        "supported sources must capture without diagnostics"
    );
    assert!(all_batch.provenance.base.is_none());
    assert_endpoint(&all_batch.provenance.head, &head, &BlockState::Committed);

    let mut expected = sources
        .into_iter()
        .map(|(path, language, source)| (path.to_string(), language, source.to_string()))
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(head_inventory(all_batch), expected);

    let invalid_path = repo.path.join("unselected/broken.rs");
    std::fs::create_dir_all(invalid_path.parent().context("broken source parent")?)?;
    std::fs::write(&invalid_path, b"pub fn broken() {\xff}\n")?;

    let file_capture = capture_declaration_sources(
        &repo.path,
        &worktree_query(ReviewPathSelection::Scoped {
            files: HashSet::from([RepoPath::new("src/exact.rs")?]),
            dirs: Vec::new(),
            changed: None,
        }),
    )?;
    assert_eq!(
        head_inventory(one_batch(&file_capture)?),
        [(
            "src/exact.rs".to_string(),
            Language::Rust,
            "pub fn café() -> &'static str {\r\n    \"🦀\"\r\n}".to_string(),
        )]
    );

    let dir_capture = capture_declaration_sources(
        &repo.path,
        &worktree_query(ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("src/nested")?],
            changed: None,
        }),
    )?;
    assert_eq!(
        head_inventory(one_batch(&dir_capture)?),
        [(
            "src/nested/model.go".to_string(),
            Language::Go,
            "package nested\n\nfunc Café() string { return \"coffee\" }\n".to_string(),
        )]
    );

    Ok(())
}

#[test]
fn main_diff_uses_the_existing_mainline_precedence_and_merge_base() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_main_merge_base")?;
    const COMMON: &str = "pub fn value() -> u8 { 1 }\n";
    const MAIN_ONLY: &str = "pub fn value() -> u8 { 2 }\n";
    const FEATURE: &str = "pub fn value() -> u8 { 3 }\n";

    repo.write("src/lib.rs", COMMON)?;
    repo.commit_all("common ancestor")?;
    repo.git(&["branch", "-M", "main"])?;
    let common = commit_id(&repo, "HEAD")?;
    repo.git(&["switch", "-c", "feature"])?;
    repo.git(&["switch", "main"])?;
    repo.write("src/lib.rs", MAIN_ONLY)?;
    repo.commit_all("advance main")?;
    repo.git(&["switch", "feature"])?;
    repo.write("src/lib.rs", FEATURE)?;
    repo.commit_all("advance feature")?;
    let feature = commit_id(&repo, "HEAD")?;

    let capture = capture_declaration_sources(
        &repo.path,
        &query(
            ReviewContentSource::Workdir,
            ReviewPathSelection::All,
            ReviewDiffSelection::Targets(vec![ReviewDiffTarget::MainDiff]),
        ),
    )?;
    let batch = one_batch(&capture)?;
    let pair = one_pair(batch)?;
    assert_eq!(pair.base.as_ref().context("main base")?.source(), COMMON);
    assert_eq!(
        pair.head.as_ref().context("feature head")?.source(),
        FEATURE
    );
    assert_eq!(pair.path_evidence, PathPairEvidence::SamePath);
    assert_endpoint(
        batch
            .provenance
            .base
            .as_ref()
            .context("main provenance base")?,
        &common,
        &BlockState::Committed,
    );
    assert_endpoint(&batch.provenance.head, &feature, &BlockState::Committed);
    assert!(batch.diagnostics.is_empty());

    Ok(())
}

#[test]
fn root_revision_pairs_the_commit_against_the_empty_tree() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_root_revision")?;
    const ROOT_SOURCE: &str = "pub struct Root {\n    pub id: u64,\n}\n";
    repo.write("src/root.rs", ROOT_SOURCE)?;
    repo.commit_all("root")?;
    let root = commit_id(&repo, "HEAD")?;

    let capture = capture_declaration_sources(&repo.path, &revision_query(root.clone()))?;
    let batch = one_batch(&capture)?;
    let pair = one_pair(batch)?;
    assert!(pair.base.is_none(), "the empty tree has no source snapshot");
    let head = pair.head.as_ref().context("root commit head snapshot")?;
    assert_eq!(head.path, Path::new("src/root.rs"));
    assert_eq!(head.source(), ROOT_SOURCE);
    assert!(
        batch.provenance.base.is_none(),
        "empty-tree provenance must not invent a commit"
    );
    assert_endpoint(&batch.provenance.head, &root, &BlockState::Committed);

    Ok(())
}

#[test]
fn revision_range_preserves_the_resolved_start_and_end_instead_of_using_head() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_revision_range")?;
    const START_SOURCE: &str = "pub fn selected() -> u8 { 1 }\n";
    const END_SOURCE: &str = "pub fn selected() -> u8 { 2 }\n";
    const LATER_SOURCE: &str = "pub fn selected() -> u8 { 3 }\n";

    repo.write("src/lib.rs", START_SOURCE)?;
    repo.commit_all("range start")?;
    let start = commit_id(&repo, "HEAD")?;
    repo.write("src/lib.rs", END_SOURCE)?;
    repo.commit_all("range end")?;
    let end = commit_id(&repo, "HEAD")?;
    repo.write("src/lib.rs", LATER_SOURCE)?;
    repo.commit_all("later head")?;

    let capture =
        capture_declaration_sources(&repo.path, &range_query(start.clone(), end.clone()))?;
    let batch = one_batch(&capture)?;
    let pair = one_pair(batch)?;
    assert_eq!(
        pair.base.as_ref().context("range base")?.source(),
        START_SOURCE
    );
    assert_eq!(
        pair.head.as_ref().context("range head")?.source(),
        END_SOURCE
    );
    assert_endpoint(
        batch
            .provenance
            .base
            .as_ref()
            .context("range provenance base")?,
        &start,
        &BlockState::Committed,
    );
    assert_endpoint(&batch.provenance.head, &end, &BlockState::Committed);

    Ok(())
}

#[test]
fn added_deleted_and_explicitly_renamed_files_keep_their_correct_endpoint_paths_and_bytes()
-> Result<()> {
    let repo = TestRepo::new("declaration_capture_change_endpoints")?;
    repo.git(&["config", "diff.renames", "true"])?;
    const DELETED: &str = "pub fn removed() -> u8 { 1 }\n";
    const RENAMED: &str = "pub fn retained_alpha() { println!(\"alpha\"); }\npub fn retained_beta() { println!(\"beta\"); }\npub fn retained_gamma() { println!(\"gamma\"); }\n";
    const ADDED: &str = "pub fn created() -> u8 { 2 }\n";

    repo.write("src/deleted.rs", DELETED)?;
    repo.write("src/old_name.rs", RENAMED)?;
    repo.commit_all("base files")?;
    let base = commit_id(&repo, "HEAD")?;

    std::fs::remove_file(repo.path.join("src/deleted.rs"))?;
    repo.git(&["mv", "src/old_name.rs", "src/new_name.rs"])?;
    repo.write("src/added.rs", ADDED)?;
    repo.commit_all("add delete rename")?;
    let head = commit_id(&repo, "HEAD")?;

    let capture = capture_declaration_sources(&repo.path, &range_query(base, head))?;
    let batch = one_batch(&capture)?;
    assert_eq!(batch.pairs.len(), 3);

    let added = pair_for_display_path(batch, "src/added.rs")?;
    assert!(added.base.is_none());
    let added_head = added.head.as_ref().context("added head")?;
    assert_eq!(added_head.path, Path::new("src/added.rs"));
    assert_eq!(added_head.source(), ADDED);

    let deleted = pair_for_display_path(batch, "src/deleted.rs")?;
    assert!(deleted.head.is_none());
    let deleted_base = deleted.base.as_ref().context("deleted base")?;
    assert_eq!(deleted_base.path, Path::new("src/deleted.rs"));
    assert_eq!(deleted_base.source(), DELETED);

    let renamed = pair_for_display_path(batch, "src/new_name.rs")?;
    let renamed_base = renamed.base.as_ref().context("rename base")?;
    let renamed_head = renamed.head.as_ref().context("rename head")?;
    assert_eq!(renamed_base.path, Path::new("src/old_name.rs"));
    assert_eq!(renamed_head.path, Path::new("src/new_name.rs"));
    assert_eq!(renamed_base.source(), RENAMED);
    assert_eq!(renamed_head.source(), RENAMED);
    assert_eq!(renamed.path_evidence, PathPairEvidence::ExplicitRename);

    Ok(())
}

#[test]
fn multiple_revision_targets_remain_distinct_and_in_request_order() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_ordered_revisions")?;
    repo.write("src/first.rs", "pub fn first() {}\n")?;
    repo.commit_all("first")?;
    let first = commit_id(&repo, "HEAD")?;
    repo.write("src/second.rs", "pub fn second() {}\n")?;
    repo.commit_all("second")?;
    let second = commit_id(&repo, "HEAD")?;

    let ordered_query = query(
        ReviewContentSource::Revision(second.clone()),
        ReviewPathSelection::All,
        ReviewDiffSelection::Targets(vec![
            ReviewDiffTarget::Revision(first.clone()),
            ReviewDiffTarget::Revision(second.clone()),
        ]),
    );
    let batches = capture_declaration_sources(&repo.path, &ordered_query)?;
    assert_eq!(batches.len(), 2);
    assert_endpoint(&batches[0].provenance.head, &first, &BlockState::Committed);
    assert_endpoint(&batches[1].provenance.head, &second, &BlockState::Committed);
    assert_eq!(
        one_pair(&batches[0])?
            .head
            .as_ref()
            .context("first root head")?
            .path,
        Path::new("src/first.rs")
    );
    assert_eq!(
        one_pair(&batches[1])?
            .head
            .as_ref()
            .context("second commit head")?
            .path,
        Path::new("src/second.rs")
    );
    assert_ne!(one_pair(&batches[0])?.id, one_pair(&batches[1])?.id);

    Ok(())
}

#[test]
fn dirty_capture_returns_every_selected_file_from_one_validated_generation() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_dirty_generation")?;
    repo.write("src/a.rs", "pub fn a() -> u8 { 0 }\n")?;
    repo.write("src/b.rs", "pub fn b() -> u8 { 0 }\n")?;
    repo.commit_all("base")?;
    let head = commit_id(&repo, "HEAD")?;

    const A_GENERATION: &str = "pub fn a() -> u8 { 11 }\n";
    const B_GENERATION: &str = "pub fn b() -> u8 { 22 }\n";
    repo.write("src/a.rs", A_GENERATION)?;
    repo.write("src/b.rs", B_GENERATION)?;

    let capture =
        capture_declaration_sources(&repo.path, &dirty_query(&["src/a.rs", "src/b.rs"])?)?;
    let batch = one_batch(&capture)?;
    assert_eq!(batch.pairs.len(), 2);
    assert_eq!(
        pair_for_display_path(batch, "src/a.rs")?
            .base
            .as_ref()
            .context("a base")?
            .source(),
        "pub fn a() -> u8 { 0 }\n"
    );
    assert_eq!(
        pair_for_display_path(batch, "src/a.rs")?
            .head
            .as_ref()
            .context("a head")?
            .source(),
        A_GENERATION
    );
    assert_eq!(
        pair_for_display_path(batch, "src/b.rs")?
            .base
            .as_ref()
            .context("b base")?
            .source(),
        "pub fn b() -> u8 { 0 }\n"
    );
    assert_eq!(
        pair_for_display_path(batch, "src/b.rs")?
            .head
            .as_ref()
            .context("b head")?
            .source(),
        B_GENERATION
    );
    assert_endpoint(
        batch
            .provenance
            .base
            .as_ref()
            .context("dirty base provenance")?,
        &head,
        &BlockState::Committed,
    );
    assert_endpoint(&batch.provenance.head, &head, &BlockState::Uncommitted);

    Ok(())
}

#[test]
fn dirty_capture_fails_closed_when_a_selected_file_changes_before_finalize() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_file_drift")?;
    repo.write("src/lib.rs", "pub fn value() -> u8 { 0 }\n")?;
    repo.commit_all("base")?;
    repo.write("src/lib.rs", "pub fn value() -> u8 { 1 }\n")?;
    let query = dirty_query(&["src/lib.rs"])?;

    let result = capture_declaration_sources_with_hook(&repo.path, &query, || {
        repo.write("src/lib.rs", "pub fn value() -> u8 { 2 }\n")
    });
    let error = match result {
        Ok(_) => bail!("capture accepted source bytes from a generation that changed"),
        Err(error) => error,
    };
    assert_eq!(format!("{error:#}"), CAPTURE_DRIFT_ERROR);

    Ok(())
}

#[test]
fn dirty_capture_fails_closed_when_head_changes_before_finalize() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_head_drift")?;
    repo.write("src/lib.rs", "pub fn value() -> u8 { 0 }\n")?;
    repo.commit_all("base")?;
    repo.write("src/lib.rs", "pub fn value() -> u8 { 1 }\n")?;
    let query = dirty_query(&["src/lib.rs"])?;

    let result = capture_declaration_sources_with_hook(&repo.path, &query, || {
        repo.git(&["commit", "--allow-empty", "-m", "move HEAD during capture"])
    });
    let error = match result {
        Ok(_) => bail!("capture accepted source bytes paired with a later HEAD"),
        Err(error) => error,
    };
    assert_eq!(format!("{error:#}"), CAPTURE_DRIFT_ERROR);

    Ok(())
}

#[test]
fn body_only_diff_from_a_captured_pair_yields_zero_declaration_review_units() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_body_only")?;
    const BASE: &str =
        "/// Returns the value.\npub fn value(input: u8) -> u8 {\n    input + 1\n}\n";
    const HEAD: &str =
        "/// Returns the value.\npub fn value(input: u8) -> u8 {\n    input.saturating_add(1)\n}\n";
    repo.write("src/lib.rs", BASE)?;
    repo.commit_all("base body")?;
    let base = commit_id(&repo, "HEAD")?;
    repo.write("src/lib.rs", HEAD)?;
    repo.commit_all("head body")?;
    let head = commit_id(&repo, "HEAD")?;

    let capture = capture_declaration_sources(&repo.path, &range_query(base, head))?;
    let batch = one_batch(&capture)?;
    let pair = one_pair(batch)?;
    assert_eq!(pair.base.as_ref().context("body base")?.source(), BASE);
    assert_eq!(pair.head.as_ref().context("body head")?.source(), HEAD);

    let diff = diff_declarations(&batch.pairs)?;
    assert!(
        diff.units.is_empty(),
        "body-only edits must not create declaration review units"
    );
    assert_eq!(
        diff.matches.len(),
        1,
        "the unchanged declaration surface must still match"
    );

    Ok(())
}
