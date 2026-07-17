use std::{
    path::Path,
    sync::{LazyLock, Mutex},
};

use anyhow::{Context, Result, anyhow, bail};
use trueflow::commands::review::{
    ResolvedReviewQuery, ReviewRequest, ReviewTarget, RevisionExpr, RevisionRangeExpr,
    resolve_review_request,
};
use trueflow::commands::tui::declaration::prepare_declaration_launch;
use trueflow::config::BlockFilters;
use trueflow::declaration::capture::{CaptureBatch, capture_declaration_sources};
use trueflow::declaration::diff::{DeclarationChangeKind, diff_declarations};
use trueflow::declaration::review::{
    DeclarationReviewDiffBatch, DeclarationReviewQuery, DeclarationReviewStatus,
    collect_declaration_review,
};
use trueflow::declaration::snapshot::{PathPairEvidence, SnapshotPair};
use trueflow::scanner::ScanOptions;
use trueflow::store::CommitId;
use trueflow::targets::{ReviewDiffSelection, ReviewDiffTarget};
use trueflow_test_support::{TestRepo, run_git_output};

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const UNCHANGED_DECLARATIONS: &str = r#"pub fn retained_alpha() {
    println!("alpha");
}

pub fn retained_beta() {
    println!("beta");
}

pub fn retained_gamma() {
    println!("gamma");
}
"#;

const MODIFIED_ORIGINAL: &str = r#"pub fn retained_alpha() {
    println!("modified alpha");
}

pub fn retained_beta() {
    println!("beta");
}

pub fn retained_gamma() {
    println!("gamma");
}
"#;

fn commit_id(repo: &TestRepo, revision: &str) -> Result<CommitId> {
    CommitId::new(run_git_output(&repo.path, &["rev-parse", revision])?)
}

fn resolve_in_repo(repo: &TestRepo, request: ReviewRequest) -> Result<ResolvedReviewQuery> {
    let _guard = CWD_LOCK
        .lock()
        .map_err(|error| anyhow!("current-directory lock poisoned: {error}"))?;
    let original = std::env::current_dir().context("failed to capture test process cwd")?;
    std::env::set_current_dir(&repo.path)
        .with_context(|| format!("failed to enter fixture repository {}", repo.path.display()))?;

    let query = resolve_review_request(request, BlockFilters::default(), ScanOptions::default());
    let restore = std::env::set_current_dir(&original).with_context(|| {
        format!(
            "failed to restore test process cwd to {}",
            original.display()
        )
    });

    match (query, restore) {
        (Ok(query), Ok(())) => Ok(query),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(query_error), Err(restore_error)) => Err(anyhow!(
            "query resolution failed: {query_error:#}; additionally, {restore_error:#}"
        )),
    }
}

fn one_batch(captures: &[CaptureBatch]) -> Result<&CaptureBatch> {
    let [capture] = captures else {
        bail!("expected exactly one capture batch, got {}", captures.len());
    };
    Ok(capture)
}

fn one_pair(capture: &CaptureBatch) -> Result<&SnapshotPair> {
    let [pair] = capture.pairs.as_slice() else {
        bail!(
            "expected exactly one snapshot pair, got {:?}",
            capture.pairs
        );
    };
    Ok(pair)
}

#[test]
fn staged_pure_git_mv_is_one_explicit_rename_without_add_delete_review_units() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_staged_pure_git_mv")?;
    repo.git(&["config", "diff.renames", "true"])?;
    repo.write("src/old.rs", UNCHANGED_DECLARATIONS)?;
    repo.commit_all("add rename source")?;

    repo.git(&["mv", "src/old.rs", "src/new.rs"])?;
    let detected = run_git_output(
        &repo.path,
        &[
            "diff",
            "--cached",
            "--name-status",
            "--find-renames=100%",
            "--",
            "src/old.rs",
            "src/new.rs",
        ],
    )?;
    assert_eq!(
        detected, "R100\tsrc/old.rs\tsrc/new.rs\n",
        "the fixture must be a staged, exact Git rename before declaration capture"
    );

    let query = resolve_in_repo(
        &repo,
        ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree]),
    )?;
    let captures = capture_declaration_sources(&repo.path, &query)?;
    let capture = one_batch(&captures)?;
    let pair = one_pair(capture)?;
    let base = pair.base.as_ref().context("rename base snapshot")?;
    let head = pair.head.as_ref().context("rename head snapshot")?;
    assert_eq!(base.path, Path::new("src/old.rs"));
    assert_eq!(head.path, Path::new("src/new.rs"));
    assert_eq!(base.source(), UNCHANGED_DECLARATIONS);
    assert_eq!(head.source(), UNCHANGED_DECLARATIONS);
    assert_eq!(pair.path_evidence, PathPairEvidence::ExplicitRename);

    let diff = diff_declarations(&capture.pairs)?;
    assert!(
        diff.units.is_empty(),
        "a pure rename must not become added/deleted declaration units: {:?}",
        diff.units
    );
    let review = collect_declaration_review(&DeclarationReviewQuery::new(vec![
        DeclarationReviewDiffBatch::new(capture.pairs.clone(), diff),
    ]))?;
    assert!(review.items.is_empty());
    assert_eq!(review.status, DeclarationReviewStatus::NoSurfaceChanges);

    Ok(())
}

#[test]
fn detected_copy_retains_original_and_reviews_copy_as_an_addition() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_detected_copy")?;
    repo.git(&["config", "diff.renames", "copies"])?;
    repo.write("src/original.rs", UNCHANGED_DECLARATIONS)?;
    repo.commit_all("add copy source")?;
    let base = commit_id(&repo, "HEAD")?;

    repo.write("src/original.rs", MODIFIED_ORIGINAL)?;
    std::fs::copy(
        repo.path.join("src/original.rs"),
        repo.path.join("src/copy.rs"),
    )?;
    repo.commit_all("copy source and modify retained original")?;
    let head = commit_id(&repo, "HEAD")?;

    let detected = run_git_output(
        &repo.path,
        &[
            "diff",
            "--name-status",
            "--find-copies=50%",
            base.as_str(),
            head.as_str(),
            "--",
            "src/original.rs",
            "src/copy.rs",
        ],
    )?;
    let statuses = detected.lines().collect::<Vec<_>>();
    assert_eq!(
        statuses.len(),
        2,
        "unexpected copy fixture status: {detected:?}"
    );
    assert!(
        statuses.iter().any(|status| {
            let fields = status.split('\t').collect::<Vec<_>>();
            fields.len() == 3
                && fields[0].starts_with('C')
                && fields[1] == "src/original.rs"
                && fields[2] == "src/copy.rs"
        }),
        "Git must detect the fixture addition as a copy: {detected:?}"
    );
    assert!(
        statuses.contains(&"M\tsrc/original.rs"),
        "the detected copy source must remain present as a modified file: {detected:?}"
    );
    assert_eq!(
        run_git_output(
            &repo.path,
            &["show", &format!("{}:src/original.rs", head.as_str())],
        )?,
        MODIFIED_ORIGINAL,
        "the copy fixture must retain the original at the head commit"
    );
    assert_eq!(
        run_git_output(
            &repo.path,
            &["show", &format!("{}:src/copy.rs", head.as_str())],
        )?,
        MODIFIED_ORIGINAL,
        "the detected copy must contain the retained source's head bytes"
    );

    let query = resolve_in_repo(
        &repo,
        ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(RevisionRangeExpr::new(
            base.as_str(),
            head.as_str(),
        )?)]),
    )?;
    let captures = capture_declaration_sources(&repo.path, &query)?;
    let capture = one_batch(&captures)?;
    let diff = diff_declarations(&capture.pairs)?;
    assert_eq!(
        diff.units.len(),
        3,
        "each declaration in a retained-source copy must be an added review unit: {:?}",
        diff.units
    );
    assert!(
        diff.units
            .iter()
            .all(|unit| unit.change_kind == DeclarationChangeKind::Added),
        "a retained-source copy must not be matched as a rename: {:?}",
        diff.units
    );

    let pair = capture
        .pairs
        .iter()
        .find(|pair| {
            pair.head
                .as_ref()
                .is_some_and(|snapshot| snapshot.path == Path::new("src/copy.rs"))
        })
        .context("copy snapshot pair")?;
    assert!(pair.base.is_none(), "a copy addition has no base endpoint");
    let copy = pair.head.as_ref().context("copy head snapshot")?;
    assert_eq!(copy.path, Path::new("src/copy.rs"));
    assert_eq!(copy.source(), MODIFIED_ORIGINAL);
    assert_eq!(pair.path_evidence, PathPairEvidence::SamePath);

    let review = collect_declaration_review(&DeclarationReviewQuery::new(vec![
        DeclarationReviewDiffBatch::new(capture.pairs.clone(), diff),
    ]))?;
    assert_eq!(review.status, DeclarationReviewStatus::Ready);
    assert_eq!(review.items.len(), 3);
    assert!(
        review
            .items
            .iter()
            .all(|item| item.display_path.as_str() == "src/copy.rs")
    );

    Ok(())
}

#[test]
fn equivalent_revision_and_range_targets_keep_ordered_capture_provenance_identity() -> Result<()> {
    let repo = TestRepo::new("declaration_capture_equivalent_target_identity")?;
    const BASE: &str = "pub fn selected(value: u8) -> u8 { value }\n";
    const HEAD: &str = "pub fn selected(value: u16) -> u16 { value }\n";
    repo.write("src/lib.rs", BASE)?;
    repo.commit_all("base declaration")?;
    let parent = commit_id(&repo, "HEAD")?;
    repo.write("src/lib.rs", HEAD)?;
    repo.commit_all("change declaration")?;
    let head = commit_id(&repo, "HEAD")?;

    let query = resolve_in_repo(
        &repo,
        ReviewRequest::Targets(vec![
            ReviewTarget::Revision(RevisionExpr::new("HEAD")?),
            ReviewTarget::RevisionRange(RevisionRangeExpr::new("HEAD^", "HEAD")?),
        ]),
    )?;
    let ReviewDiffSelection::Targets(targets) = &query.diff_selection else {
        bail!("expected two immutable declaration targets");
    };
    let [
        ReviewDiffTarget::Revision(revision),
        ReviewDiffTarget::RevisionRange(range),
    ] = targets.as_slice()
    else {
        bail!("query did not retain Revision then RevisionRange target order: {targets:?}");
    };
    assert_eq!(revision, &head);
    assert_eq!(range.start, parent);
    assert_eq!(range.end, head);

    let captures = capture_declaration_sources(&repo.path, &query)?;
    let [revision_capture, range_capture] = captures.as_slice() else {
        bail!(
            "expected two ordered capture batches, got {}",
            captures.len()
        );
    };
    assert_eq!(
        revision_capture.provenance, range_capture.provenance,
        "the fixture targets must resolve to semantically equivalent Git endpoints"
    );
    let revision_pair = one_pair(revision_capture)?;
    let range_pair = one_pair(range_capture)?;
    assert_eq!(
        revision_pair
            .base
            .as_ref()
            .context("revision base")?
            .source(),
        BASE
    );
    assert_eq!(
        range_pair.base.as_ref().context("range base")?.source(),
        BASE
    );
    assert_eq!(
        revision_pair
            .head
            .as_ref()
            .context("revision head")?
            .source(),
        HEAD
    );
    assert_eq!(
        range_pair.head.as_ref().context("range head")?.source(),
        HEAD
    );

    let launch = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let [revision_target, range_target] = launch.targets() else {
        bail!(
            "expected one changed declaration from each ordered batch, got {:?}",
            launch.targets()
        );
    };
    assert_eq!(revision_target.snapshot_pair_id, revision_pair.id);
    assert_eq!(range_target.snapshot_pair_id, range_pair.id);
    assert_ne!(
        revision_target.snapshot_pair_id,
        range_target.snapshot_pair_id
    );
    assert_eq!(
        revision_target.snapshot.id,
        revision_pair
            .head
            .as_ref()
            .context("revision display head")?
            .id
    );
    assert_eq!(
        range_target.snapshot.id,
        range_pair.head.as_ref().context("range display head")?.id
    );
    assert_ne!(revision_target.snapshot.id, range_target.snapshot.id);
    assert_eq!(revision_target.declaration.name, "selected");
    assert_eq!(range_target.declaration.name, "selected");

    Ok(())
}
