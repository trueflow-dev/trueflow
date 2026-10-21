#![cfg(feature = "tui-test-support")]

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::KeyCode;
use trueflow::analysis::Language;
use trueflow::cli::TuiReviewMode;
use trueflow::commands::review::ResolvedReviewQuery;
use trueflow::commands::tui::declaration::{
    DeclarationAppRuntime, DeclarationPane, DeclarationRecordAppender, DeclarationReviewActionKind,
    DeclarationReviewStatus as SurfaceStatus, GraphSelection, Relationship,
    RelationshipDestination, RelationshipGroup, RelationshipState, prepare_declaration_launch,
};
use trueflow::commands::tui::{TuiRuntimeKind, resolve_tui_launch};
use trueflow::config::{BlockFilters, TrueflowConfig};
use trueflow::declaration::review::DeclarationReviewStatus as CollectionStatus;
use trueflow::repo_path::RepoPath;
use trueflow::review_scope::ScopePreset;
use trueflow::scanner::ScanOptions;
use trueflow::store::{
    BlockState, CURRENT_VERSION, CommentAnchor, CommitId, Identity, Record, RepoRef, ReviewCheck,
    ReviewTargetRef, VcsSystem, Verdict,
};
use trueflow::targets::{
    CommitRange, ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection,
};
use trueflow::vcs::ChangedPath;
use trueflow_test_support::{TestRepo, run_git_output};

const BODY_SENTINEL: &str = "EXECUTABLE BODY SENTINEL MUST NEVER RENDER";
const A_BASE: &str = r#"/// Converts the input.
pub fn alpha(value: u8) -> u8 {
    value
}

pub fn stable() -> u8 {
    0
}
"#;
const A_HEAD: &str = r#"/// Converts the input.
pub fn alpha(value: u16) -> u16 {
    let hidden = "EXECUTABLE BODY SENTINEL MUST NEVER RENDER";
    value
}

pub fn stable() -> u8 {
    1
}
"#;
const Z_BASE: &str = r#"/// Parses text.
pub fn beta(value: &str) -> usize {
    value.len()
}
"#;
const Z_HEAD: &str = r#"/// Parses bytes.
pub fn beta(value: &[u8]) -> usize {
    let hidden = "EXECUTABLE BODY SENTINEL MUST NEVER RENDER";
    value.len()
}
"#;

#[derive(Debug, Default)]
struct RecordingAppender {
    records: Vec<Record>,
}

impl DeclarationRecordAppender for RecordingAppender {
    fn append(&mut self, record: &Record) -> Result<()> {
        record.validate()?;
        self.records.push(record.clone());
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FailingAppender {
    attempts: usize,
}

impl DeclarationRecordAppender for FailingAppender {
    fn append(&mut self, _record: &Record) -> Result<()> {
        self.attempts += 1;
        bail!("injected append failure")
    }
}

fn identity() -> Identity {
    Identity::Email {
        email: "reviewer@example.com".to_owned(),
    }
}

fn dirty_query(paths: &[&str]) -> Result<ResolvedReviewQuery> {
    let changed = paths
        .iter()
        .map(|path| RepoPath::new(*path).map(ChangedPath::identity))
        .collect::<Result<HashSet<_>>>()?;
    Ok(ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Workdir,
        path_selection: ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: Vec::new(),
            changed: Some(changed),
        },
        diff_selection: ReviewDiffSelection::None,
    })
}

fn two_file_dirty_repo(name: &str) -> Result<(TestRepo, ResolvedReviewQuery)> {
    let repo = TestRepo::new(name)?;
    repo.write("src/a.rs", A_BASE)?;
    repo.write("src/z.rs", Z_BASE)?;
    repo.commit_all("base declaration surfaces")?;
    repo.write("src/a.rs", A_HEAD)?;
    repo.write("src/z.rs", Z_HEAD)?;
    let query = dirty_query(&["src/a.rs", "src/z.rs"])?;
    Ok((repo, query))
}

fn head_revision(repo: &TestRepo) -> Result<CommitId> {
    CommitId::new(run_git_output(&repo.path, &["rev-parse", "HEAD"])?)
}

fn only_record(mut records: Vec<Record>) -> Result<Record> {
    ensure!(
        records.len() == 1,
        "expected one appended record, got {records:#?}"
    );
    Ok(records.remove(0))
}

#[test]
fn prepared_dirty_launch_uses_exact_sources_changed_surfaces_and_global_order() -> Result<()> {
    let (repo, query) = two_file_dirty_repo("declaration_tui_prepared_launch")?;
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;

    assert_eq!(prepared.status(), &CollectionStatus::Ready);
    assert_eq!(
        prepared
            .documents()
            .iter()
            .map(|document| (document.path.as_str(), document.exact_source.as_str()))
            .collect::<Vec<_>>(),
        [("src/a.rs", A_HEAD), ("src/z.rs", Z_HEAD)],
        "documents must retain the exact dirty bytes captured for each file"
    );
    assert_eq!(
        prepared
            .targets()
            .iter()
            .map(|target| (
                target.display_path.as_str(),
                target.declaration.name.as_str()
            ))
            .collect::<Vec<_>>(),
        [("src/a.rs", "alpha"), ("src/z.rs", "beta")],
        "body-only stable must be absent and canonical order must span files by path/source"
    );
    assert_eq!(
        prepared.canonical_order(),
        prepared
            .targets()
            .iter()
            .map(|target| target.declaration.id.clone())
            .collect::<Vec<_>>()
            .as_slice()
    );

    let runtime =
        DeclarationAppRuntime::new(prepared, identity(), RecordingAppender::default(), 120, 20)?;
    let visible = runtime.visible_text();
    assert!(visible.contains("pub fn alpha(value: u16) -> u16"));
    assert!(
        !visible.contains("stable"),
        "body-only edits became review targets"
    );
    assert!(
        !visible.contains(BODY_SENTINEL),
        "executable body text leaked into the review surface"
    );
    Ok(())
}

#[test]
fn in_review_relationship_navigation_crosses_prepared_targets_and_back_restores_view() -> Result<()>
{
    let (repo, query) = two_file_dirty_repo("declaration_tui_relationship_navigation")?;
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let source = prepared
        .targets()
        .iter()
        .find(|target| target.declaration.name == "alpha")
        .context("missing prepared alpha target")?
        .declaration
        .id
        .clone();
    let destination = prepared
        .targets()
        .iter()
        .find(|target| target.declaration.name == "beta")
        .context("missing prepared beta target")?
        .declaration
        .id
        .clone();

    let mut runtime =
        DeclarationAppRuntime::new(prepared, identity(), RecordingAppender::default(), 120, 20)?;
    runtime.apply_relationship_state(
        source.as_str(),
        RelationshipState::Ready(vec![RelationshipGroup {
            label: "Calls".to_owned(),
            relationships: vec![Relationship {
                id: "alpha.calls.beta".to_owned(),
                label: "beta".to_owned(),
                destination: RelationshipDestination::InReview {
                    declaration_id: destination.as_str().to_owned(),
                },
            }],
        }]),
    )?;

    let controller = runtime
        .controller_mut()
        .context("prepared runtime has no declaration controller")?;
    controller.handle_key(KeyCode::Tab)?;
    controller.handle_key(KeyCode::Down)?;
    let before_activation = controller.state_snapshot();
    ensure!(
        before_activation.active_declaration == source.as_str()
            && before_activation.active_pane == DeclarationPane::Relationships
            && before_activation.relationships.selection
                == Some(GraphSelection::Relationship("alpha.calls.beta".to_owned())),
        "fixture did not select alpha's beta relationship: {before_activation:#?}"
    );

    controller.handle_key(KeyCode::Enter)?;
    let after_activation = controller.state_snapshot();
    let relationship_request_target = runtime
        .selected_target()
        .map(|target| target.declaration.id.as_str().to_owned());
    runtime
        .controller_mut()
        .context("navigation lost the declaration controller")?
        .handle_key(KeyCode::Backspace)?;
    let after_back = runtime
        .controller()
        .context("back navigation lost the declaration controller")?
        .state_snapshot();

    assert_eq!(
        after_activation.active_declaration,
        destination.as_str(),
        "activating an in-review relationship must open its prepared destination target"
    );
    assert_eq!(
        relationship_request_target.as_deref(),
        Some(destination.as_str()),
        "relationship requests and live updates must target the inspected beta, not queued alpha"
    );
    assert_eq!(
        after_back, before_activation,
        "Backspace must restore the prior target, pane, and row selection"
    );
    Ok(())
}

#[test]
fn approving_relationship_destination_preserves_pending_queue_and_exact_binding() -> Result<()> {
    let (repo, query) = two_file_dirty_repo("declaration_tui_relationship_approval")?;
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let alpha = prepared
        .targets()
        .iter()
        .find(|target| target.declaration.name == "alpha")
        .context("missing prepared alpha target")?
        .clone();
    let beta = prepared
        .targets()
        .iter()
        .find(|target| target.declaration.name == "beta")
        .context("missing prepared beta target")?
        .clone();
    let mut runtime =
        DeclarationAppRuntime::new(prepared, identity(), RecordingAppender::default(), 120, 20)?;
    runtime.apply_relationship_state(
        alpha.declaration.id.as_str(),
        RelationshipState::Ready(vec![RelationshipGroup {
            label: "Calls".to_owned(),
            relationships: vec![Relationship {
                id: "alpha.calls.beta".to_owned(),
                label: "beta".to_owned(),
                destination: RelationshipDestination::InReview {
                    declaration_id: beta.declaration.id.as_str().to_owned(),
                },
            }],
        }]),
    )?;
    let controller = runtime
        .controller_mut()
        .context("prepared runtime has no declaration controller")?;
    for key in [KeyCode::Tab, KeyCode::Down, KeyCode::Enter, KeyCode::Tab] {
        controller.handle_key(key)?;
    }
    let inspected = controller.state_snapshot();
    assert_eq!(inspected.active_declaration, beta.declaration.id.as_str());
    assert_eq!(inspected.active_pane, DeclarationPane::Outline);
    controller.handle_key(KeyCode::Char('a'))?;
    let mut actions = controller.take_actions();
    ensure!(actions.len() == 1, "approving beta must produce one action");
    let action = actions.remove(0);
    let mut persisted = Vec::new();
    runtime.submit_action_with_persistence(&action, |record| {
        record.validate()?;
        persisted.push(record.clone());
        Ok(())
    })?;
    let record = only_record(persisted)?;
    assert_eq!(
        record.target,
        ReviewTargetRef::Declaration {
            hash: beta.declaration.projection_hash.clone(),
        }
    );
    assert_eq!(record.verdict, Verdict::Approved);
    let locator = record
        .declaration_locator
        .as_ref()
        .context("beta approval locator")?;
    assert_eq!(locator.path, beta.display_path);
    assert_eq!(locator.declaration_key, beta.declaration.key);
    assert_eq!(locator.source_ordinal, beta.declaration.source_ordinal);
    assert_eq!(locator.source_span, beta.declaration.source_span);
    assert_eq!(locator.projection_hash, beta.declaration.projection_hash);
    assert_eq!(
        locator.reviewed_snapshot.snapshot_id,
        beta.snapshot.id.as_str()
    );
    assert_eq!(
        &locator.reviewed_snapshot.content_hash,
        beta.snapshot.bytes_hash()
    );
    let anchor = match record.comment_anchor.as_ref() {
        Some(CommentAnchor::Declaration(anchor)) => anchor,
        other => bail!("beta approval must retain its declaration anchor, got {other:?}"),
    };
    assert_eq!(anchor.reviewed_snapshot, locator.reviewed_snapshot);
    assert_eq!(anchor.projection_hash, beta.declaration.projection_hash);
    assert_eq!(anchor.source_len_bytes, beta.snapshot.source().len());
    assert_eq!(anchor.ranges.len(), beta.declaration.components.len());
    for (actual, component) in anchor.ranges.iter().zip(&beta.declaration.components) {
        assert_eq!(actual.start_byte..actual.end_byte, component.source_range);
        assert_eq!(actual.exact_text, component.text);
    }
    assert_eq!(
        runtime.queue_current().map(|target| &target.declaration.id),
        Some(&alpha.declaration.id),
        "approving an inspected destination must leave the original queue item pending"
    );
    assert_eq!(
        runtime
            .controller()
            .context("alpha must remain reviewable")?
            .state_snapshot()
            .active_declaration,
        alpha.declaration.id.as_str(),
        "successful approval must return to pending alpha"
    );
    let reloaded = prepare_declaration_launch(&repo.path, &query, vec![record])?;
    assert_eq!(
        reloaded
            .targets()
            .iter()
            .map(|target| target.declaration.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha"],
        "persisted beta approval must not mark alpha reviewed"
    );
    runtime.submit(DeclarationReviewActionKind::Approve, None)?;
    assert!(
        runtime.is_finished(),
        "later canonical traversal must skip already-reviewed beta"
    );
    assert!(runtime.queue_current().is_none());
    let alpha_record = only_record(runtime.into_appender().records)?;
    assert_eq!(
        alpha_record
            .declaration_locator
            .context("alpha approval locator")?
            .declaration_key,
        alpha.declaration.key
    );
    Ok(())
}

#[test]
fn failed_relationship_approval_preserves_inspection_and_leaves_destination_pending() -> Result<()>
{
    let (repo, query) = two_file_dirty_repo("declaration_tui_relationship_append_failure")?;
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let alpha = prepared.targets()[0].declaration.id.clone();
    let beta = prepared.targets()[1].declaration.id.clone();
    let mut runtime =
        DeclarationAppRuntime::new(prepared, identity(), RecordingAppender::default(), 120, 20)?;
    runtime.apply_relationship_state(
        alpha.as_str(),
        RelationshipState::Ready(vec![RelationshipGroup {
            label: "Calls".to_owned(),
            relationships: vec![Relationship {
                id: "alpha.calls.beta".to_owned(),
                label: "beta".to_owned(),
                destination: RelationshipDestination::InReview {
                    declaration_id: beta.as_str().to_owned(),
                },
            }],
        }]),
    )?;
    let controller = runtime.controller_mut().context("missing controller")?;
    for key in [
        KeyCode::Tab,
        KeyCode::Down,
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::Char('a'),
    ] {
        controller.handle_key(key)?;
    }
    let action = controller
        .take_actions()
        .pop()
        .context("missing approval")?;
    let before = controller.state_snapshot();
    let before_text = runtime.visible_text();
    let mut append_attempted = false;
    let result = runtime.submit_action_with_persistence(&action, |_| {
        append_attempted = true;
        bail!("injected append failure")
    });
    assert!(
        append_attempted,
        "the inspected action must reach persistence"
    );
    assert!(result.is_err());
    assert_eq!(
        runtime
            .controller()
            .context("lost controller")?
            .state_snapshot(),
        before,
        "failed persistence must retain the inspected target and navigation"
    );
    assert_eq!(runtime.visible_text(), before_text);
    assert_eq!(
        runtime.queue_current().map(|target| &target.declaration.id),
        Some(&alpha)
    );
    runtime.skip_current()?;
    assert_eq!(
        runtime.queue_current().map(|target| &target.declaration.id),
        Some(&beta),
        "skipping queued alpha must not skip the unsuccessfully persisted beta"
    );
    runtime.submit(DeclarationReviewActionKind::Approve, None)?;
    assert!(runtime.is_finished());
    Ok(())
}

#[test]
fn approval_persists_exact_v5_binding_and_only_advances_after_append() -> Result<()> {
    let (repo, query) = two_file_dirty_repo("declaration_tui_approval_runtime")?;
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let approved = prepared
        .targets()
        .first()
        .context("missing alpha target")?
        .clone();
    let successor = prepared
        .targets()
        .get(1)
        .context("missing beta target")?
        .declaration
        .id
        .clone();
    let expected_revision = head_revision(&repo)?;

    let mut runtime =
        DeclarationAppRuntime::new(prepared, identity(), RecordingAppender::default(), 120, 20)?;
    assert_eq!(
        runtime.queue_current().map(|target| &target.declaration.id),
        Some(&approved.declaration.id)
    );
    runtime.submit(DeclarationReviewActionKind::Approve, None)?;
    assert_eq!(
        runtime.queue_current().map(|target| &target.declaration.id),
        Some(&successor),
        "a successful append must advance to the canonical successor"
    );
    let record = only_record(runtime.into_appender().records)?;

    assert_eq!(record.version, CURRENT_VERSION);
    assert_eq!(
        record.target,
        ReviewTargetRef::Declaration {
            hash: approved.declaration.projection_hash.clone(),
        }
    );
    assert_eq!(record.check, ReviewCheck::declaration());
    assert_eq!(record.verdict, Verdict::Approved);
    assert!(matches!(
        &record.identity,
        Identity::Email { email } if email == "reviewer@example.com"
    ));
    assert_eq!(
        record.repo_ref,
        RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: expected_revision,
        }
    );
    assert_eq!(record.block_state, BlockState::Uncommitted);
    let locator = record
        .declaration_locator
        .as_ref()
        .context("approval locator")?;
    assert_eq!(locator.path, approved.display_path);
    assert_eq!(locator.declaration_key, approved.declaration.key);
    assert_eq!(locator.source_ordinal, approved.declaration.source_ordinal);
    assert_eq!(locator.source_span, approved.declaration.source_span);
    assert_eq!(
        locator.reviewed_snapshot.snapshot_id,
        approved.snapshot.id.as_str()
    );
    assert_eq!(
        &locator.reviewed_snapshot.content_hash,
        approved.snapshot.bytes_hash()
    );
    assert_eq!(
        locator.projection_hash,
        approved.declaration.projection_hash
    );

    let anchor = match record.comment_anchor.as_ref() {
        Some(CommentAnchor::Declaration(anchor)) => anchor,
        other => bail!("approval must retain an exact declaration anchor, got {other:?}"),
    };
    assert_eq!(anchor.reviewed_snapshot, locator.reviewed_snapshot);
    assert_eq!(anchor.projection_hash, locator.projection_hash);
    assert_eq!(anchor.source_len_bytes, approved.snapshot.source().len());
    assert_eq!(anchor.ranges.len(), approved.declaration.components.len());
    for (actual, component) in anchor.ranges.iter().zip(&approved.declaration.components) {
        assert_eq!(actual.start_byte..actual.end_byte, component.source_range);
        assert_eq!(actual.exact_text, component.text);
        assert_eq!(
            approved
                .snapshot
                .source()
                .get(actual.start_byte..actual.end_byte),
            Some(actual.exact_text.as_str())
        );
        assert!(!actual.exact_text.contains(BODY_SENTINEL));
    }
    record.validate()?;

    let reloaded = prepare_declaration_launch(&repo.path, &query, vec![record])?;
    assert_eq!(
        reloaded
            .targets()
            .iter()
            .map(|target| target.declaration.name.as_str())
            .collect::<Vec<_>>(),
        ["beta"],
        "reload must hide only the approved declaration"
    );

    let failing_prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    let original = failing_prepared
        .targets()
        .first()
        .context("missing failing target")?
        .declaration
        .id
        .clone();
    let mut failing = DeclarationAppRuntime::new(
        failing_prepared,
        identity(),
        FailingAppender::default(),
        120,
        20,
    )?;
    let Err(error) = failing.submit(DeclarationReviewActionKind::Approve, None) else {
        anyhow::bail!("injected append failure must surface");
    };
    assert!(format!("{error:#}").contains("injected append failure"));
    assert_eq!(
        failing.queue_current().map(|target| &target.declaration.id),
        Some(&original)
    );
    assert_eq!(failing.into_appender().attempts, 1);
    Ok(())
}

#[test]
fn comment_and_rejection_persist_then_remain_reviewable_with_status() -> Result<()> {
    let (repo, query) = two_file_dirty_repo("declaration_tui_nonapproval_status")?;
    let cases = [
        (
            DeclarationReviewActionKind::Comment,
            Verdict::Comment,
            "needs an example",
            SurfaceStatus::Commented,
            "[commented]",
        ),
        (
            DeclarationReviewActionKind::Reject,
            Verdict::Rejected,
            "breaking contract",
            SurfaceStatus::Rejected,
            "[rejected]",
        ),
    ];

    for (action, verdict, note, surface_status, visible_marker) in cases {
        let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
        let first_id = prepared
            .targets()
            .first()
            .context("missing first target")?
            .declaration
            .id
            .clone();
        let second_id = prepared
            .targets()
            .get(1)
            .context("missing successor")?
            .declaration
            .id
            .clone();
        let mut runtime = DeclarationAppRuntime::new(
            prepared,
            identity(),
            RecordingAppender::default(),
            120,
            20,
        )?;
        runtime.submit(action, Some(note.to_owned()))?;
        assert_eq!(
            runtime.queue_current().map(|target| &target.declaration.id),
            Some(&second_id),
            "successful {verdict:?} append must advance within the current session"
        );
        let record = only_record(runtime.into_appender().records)?;
        assert_eq!(record.verdict, verdict);
        assert_eq!(record.note.as_deref(), Some(note));
        record.validate()?;

        let reloaded = prepare_declaration_launch(&repo.path, &query, vec![record])?;
        let current = reloaded
            .targets()
            .first()
            .context("non-approved target disappeared")?;
        assert_eq!(current.declaration.id, first_id);
        assert_eq!(current.latest_verdict.as_ref(), Some(&verdict));
        let owner_row = reloaded
            .documents()
            .iter()
            .flat_map(|document| &document.outline_rows)
            .find(|row| row.id == first_id.as_str())
            .context("missing reloaded owner row")?;
        assert_eq!(owner_row.status, surface_status);

        let reloaded_runtime = DeclarationAppRuntime::new(
            reloaded,
            identity(),
            RecordingAppender::default(),
            120,
            20,
        )?;
        assert!(
            reloaded_runtime.visible_text().contains(visible_marker),
            "persisted {verdict:?} status was not visible after reload"
        );
    }
    Ok(())
}

#[test]
fn empty_dirty_launches_preserve_explicit_reason_and_finish_without_a_controller() -> Result<()> {
    let body_repo = TestRepo::new("declaration_tui_body_only_empty")?;
    body_repo.write(
        "src/lib.rs",
        "pub fn total(values: &[u64]) -> u64 { values.iter().sum() }\n",
    )?;
    body_repo.commit_all("base body")?;
    body_repo.write(
        "src/lib.rs",
        "pub fn total(values: &[u64]) -> u64 { values.iter().copied().sum() }\n",
    )?;
    let body_query = dirty_query(&["src/lib.rs"])?;
    let body = prepare_declaration_launch(&body_repo.path, &body_query, Vec::new())?;
    assert_eq!(body.status(), &CollectionStatus::NoSurfaceChanges);
    let body_runtime =
        DeclarationAppRuntime::new(body, identity(), RecordingAppender::default(), 100, 12)?;
    assert_eq!(body_runtime.status(), &CollectionStatus::NoSurfaceChanges);
    assert!(body_runtime.queue_current().is_none());
    assert!(body_runtime.is_finished());
    assert!(
        body_runtime
            .visible_text()
            .contains("No declaration surface changes")
    );

    let unsupported_repo = TestRepo::new("declaration_tui_unsupported_empty")?;
    unsupported_repo.write("src/Only.java", "public final class Only {}\n")?;
    unsupported_repo.commit_all("base unsupported source")?;
    unsupported_repo.write(
        "src/Only.java",
        "public final class Only { public int id(); }\n",
    )?;
    let unsupported_query = dirty_query(&["src/Only.java"])?;
    let unsupported =
        prepare_declaration_launch(&unsupported_repo.path, &unsupported_query, Vec::new())?;
    assert_eq!(
        unsupported.status(),
        &CollectionStatus::UnsupportedLanguage {
            languages: vec![Language::Java],
        }
    );
    let unsupported_runtime = DeclarationAppRuntime::new(
        unsupported,
        identity(),
        RecordingAppender::default(),
        100,
        12,
    )?;
    assert_eq!(
        unsupported_runtime.status(),
        &CollectionStatus::UnsupportedLanguage {
            languages: vec![Language::Java],
        }
    );
    assert!(unsupported_runtime.queue_current().is_none());
    assert!(unsupported_runtime.is_finished());
    let visible = unsupported_runtime.visible_text();
    assert!(visible.contains("Unsupported language"));
    assert!(visible.contains("Java"));
    Ok(())
}

#[test]
fn shell_function_declarations_are_reviewable_without_aborting_dirty_launch() -> Result<()> {
    const BASE: &str = "#!/usr/bin/env bash\n\ndeploy() {\n    echo old\n}\n";
    const HEAD: &str = "#!/usr/bin/env bash\n\ndeploy() {\n    echo new\n}\n\n# Reports service status.\nfunction healthcheck {\n    printf '%s\\n' healthy\n}\n";

    let repo = TestRepo::new("declaration_tui_shell_functions")?;
    repo.write("scripts/release.sh", BASE)?;
    repo.commit_all("base shell declarations")?;
    repo.write("scripts/release.sh", HEAD)?;

    let query = dirty_query(&["scripts/release.sh"])?;
    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;

    assert_eq!(prepared.status(), &CollectionStatus::Ready);
    assert_eq!(
        prepared
            .targets()
            .iter()
            .map(|target| (
                target.declaration.kind,
                target.declaration.name.as_str(),
                target.declaration.projection_text.as_str(),
            ))
            .collect::<Vec<_>>(),
        [(
            trueflow::declaration::DeclarationKind::Function,
            "healthcheck",
            "# Reports service status.\nfunction healthcheck",
        )],
        "the added function must be reviewable while deploy's body-only edit stays out of the declaration review"
    );
    Ok(())
}

#[test]
fn overlapping_diff_targets_review_an_identical_declaration_once() -> Result<()> {
    let repo = TestRepo::new("declaration_tui_overlapping_targets")?;
    repo.write("src/a.rs", A_BASE)?;
    repo.commit_all("base declaration surfaces")?;
    let parent = head_revision(&repo)?;
    repo.write("src/a.rs", A_HEAD)?;
    repo.commit_all("change alpha")?;
    let head = head_revision(&repo)?;
    let query = ResolvedReviewQuery {
        filters: BlockFilters::default(),
        scan_options: ScanOptions::default(),
        content_source: ReviewContentSource::Revision(head.clone()),
        path_selection: ReviewPathSelection::All,
        diff_selection: ReviewDiffSelection::Targets(vec![
            ReviewDiffTarget::Revision(head.clone()),
            ReviewDiffTarget::RevisionRange(CommitRange {
                start: parent,
                end: head,
            }),
        ]),
    };

    let prepared = prepare_declaration_launch(&repo.path, &query, Vec::new())?;
    assert_eq!(
        prepared
            .targets()
            .iter()
            .map(|target| target.declaration.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha"],
        "both targets capture the same alpha surface; it must be one review item"
    );
    let mut runtime =
        DeclarationAppRuntime::new(prepared, identity(), RecordingAppender::default(), 120, 20)?;
    runtime.submit(DeclarationReviewActionKind::Approve, None)?;
    assert!(
        runtime.is_finished(),
        "approving the merged occurrence must finish the review"
    );
    Ok(())
}

#[test]
fn resolved_declaration_mode_selects_the_declaration_runtime_route() -> Result<()> {
    let config: TrueflowConfig = toml::from_str("[tui]\nmode = \"blocks\"\n")?;
    let declarations = resolve_tui_launch(
        &config,
        Some(TuiReviewMode::Declarations),
        false,
        ScopePreset::All,
        &[],
        &[],
    )?;
    let blocks = resolve_tui_launch(
        &config,
        Some(TuiReviewMode::Blocks),
        false,
        ScopePreset::All,
        &[],
        &[],
    )?;

    assert_eq!(declarations.runtime_kind(), TuiRuntimeKind::Declarations);
    assert_eq!(blocks.runtime_kind(), TuiRuntimeKind::Blocks);
    Ok(())
}
