use crate::block::BlockKind;
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::feedback_export::{
    FeedbackCursor, FeedbackEntry, FeedbackQuery, FeedbackSinceFilter, RepoFeedbackContextResolver,
    collect_feedback_entries, feedback_cursor_path, resolve_allowed_revisions,
    resolve_since_filter, write_feedback_cursor,
};
use crate::feedback_since::{FeedbackSinceExpr, ResolvedFeedbackSince as ParsedFeedbackSince};
use crate::github::{
    GhGitHubClient, GitHubClient, GitHubCommentSide, GitHubInlineComment, GitHubReviewDraft,
    PostedPullRequestReview, PreparedPullRequestReview, PullRequestMetadata, PullRequestRef,
    PullRequestReviewState, ResolvedPullRequestRef, prepare_pull_request_review,
};
use crate::github_delivery::{GITHUB_DELIVERY_LEDGER_FILE, GitHubDeliveryLedger};
use crate::store::{
    CommentAnchor, CommitId, DiffCommentAnchor, FileStore, Record, RepoRef, ReviewStore,
    SourceCommentAnchor,
};
use crate::targets::{
    ResolvedTargets, ReviewContentSource, ReviewPathSelection, ReviewTarget,
    extract_pull_request_target, resolve_targets_with, workdir_prefix_from_git_root,
};
use crate::vcs;
use anyhow::{Result, anyhow};
use clap::ValueEnum;
use serde::ser::{SerializeSeq, Serializer as _};
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const TRUEFLOW_PENDING_REVIEW_MARKER: &str = "<!-- trueflow:pending-review -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FeedbackFormat {
    Xml,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct FeedbackParams<'a> {
    pub format: FeedbackFormat,
    pub since: Option<&'a FeedbackSinceExpr>,
    pub pr: Option<&'a PullRequestRef>,
    pub dry_run: bool,
    pub open: bool,
    pub submit: bool,
    pub targets: &'a [ReviewTarget],
    pub include_approved: bool,
    pub only: &'a [BlockKind],
    pub exclude: &'a [BlockKind],
}

#[derive(Debug, Clone, Copy)]
pub struct FeedbackCollectionParams<'a> {
    pub since: Option<&'a FeedbackSinceExpr>,
    pub targets: &'a [ReviewTarget],
    pub include_approved: bool,
    pub only: &'a [BlockKind],
    pub exclude: &'a [BlockKind],
}

#[derive(Debug, Clone, Copy)]
struct FeedbackRecordFilterParams<'a> {
    since: Option<&'a FeedbackSinceExpr>,
    include_approved: bool,
    only: &'a [BlockKind],
    exclude: &'a [BlockKind],
}

#[cfg(test)]
impl FeedbackRecordFilterParams<'_> {
    fn unfiltered() -> Self {
        Self {
            since: None,
            include_approved: true,
            only: &[],
            exclude: &[],
        }
    }
}

struct FeedbackCommandResult {
    entries: Vec<FeedbackEntry>,
    cursor_update: Option<FeedbackCursorUpdate>,
}

struct FeedbackCursorUpdate {
    path: PathBuf,
    cursor: crate::feedback_export::FeedbackCursor,
}

pub fn run(_context: &TrueflowContext, params: FeedbackParams<'_>) -> Result<()> {
    let FeedbackParams {
        format,
        since,
        pr,
        dry_run,
        open,
        submit,
        targets,
        include_approved,
        only,
        exclude,
    } = params;

    if let Some(pr) = pr {
        return run_pull_request_feedback(
            pr,
            FeedbackRecordFilterParams {
                since,
                include_approved,
                only,
                exclude,
            },
            dry_run,
            open,
            submit,
        );
    }

    let result = collect_local_feedback(FeedbackCollectionParams {
        since,
        targets,
        include_approved,
        only,
        exclude,
    })?;
    render_feedback(format, result.entries)?;
    write_feedback_cursor_update(result.cursor_update)?;

    Ok(())
}

pub fn collect_feedback_json_values(
    params: FeedbackCollectionParams<'_>,
) -> Result<Vec<serde_json::Value>> {
    let result = collect_local_feedback(params)?;
    let values = feedback_entries_to_json_values(&result.entries);
    write_feedback_cursor_update(result.cursor_update)?;
    Ok(values)
}

fn collect_local_feedback(params: FeedbackCollectionParams<'_>) -> Result<FeedbackCommandResult> {
    let FeedbackCollectionParams {
        since,
        targets,
        include_approved,
        only,
        exclude,
    } = params;

    validate_feedback_command_args(targets, None)?;
    let config = load_config()?;
    let filters = config.feedback.filters.resolve_filters(only, exclude);
    let scan_options = config.scan.resolve_options();
    let effective_since = since.unwrap_or(&config.feedback.default_since);
    let resolved_targets = resolve_local_feedback_targets(targets)?;

    let store = crate::store::FileStore::new()?;
    let database = store.load_database()?;
    let since_mode = effective_since.resolve()?;
    let since_filter = resolve_since_filter(&store, since_mode)?;
    let explicit_selection = resolved_targets.explicit_selection();
    let changed_selection = feedback_changed_selection(targets, &resolved_targets);
    let allowed_revisions = resolve_allowed_revisions(&resolved_targets.diff_selection)?;
    let query = FeedbackQuery {
        filters,
        explicit_selection,
        changed_selection,
        allowed_revisions,
        include_approved,
    };
    let workdir_prefix = workdir_prefix_from_git_root();
    let mut resolver = RepoFeedbackContextResolver::new(
        &resolved_targets.content_source,
        &scan_options,
        workdir_prefix.as_deref(),
    )?;
    let entries =
        collect_feedback_entries(database.records(), &since_filter, &query, &mut resolver)?;

    let cursor_update = if matches!(since_mode, ParsedFeedbackSince::Last) {
        build_feedback_cursor_after_entries_export(&since_filter, &entries).map(|cursor| {
            FeedbackCursorUpdate {
                path: feedback_cursor_path(&store),
                cursor,
            }
        })
    } else {
        None
    };

    Ok(FeedbackCommandResult {
        entries,
        cursor_update,
    })
}

fn build_feedback_cursor_after_entries_export(
    since_filter: &FeedbackSinceFilter,
    entries: &[FeedbackEntry],
) -> Option<FeedbackCursor> {
    let exported_cursor = build_feedback_cursor_from_entries(entries);
    let FeedbackSinceFilter::Cursor(previous) = since_filter else {
        return exported_cursor;
    };

    let Some(exported_cursor) = exported_cursor else {
        return Some(previous.clone());
    };

    if exported_cursor.timestamp < previous.timestamp {
        return Some(previous.clone());
    }

    if exported_cursor.timestamp > previous.timestamp {
        return Some(exported_cursor);
    }

    let mut record_ids_at_timestamp = previous.record_ids_at_timestamp.clone();
    record_ids_at_timestamp.extend(exported_cursor.record_ids_at_timestamp);
    record_ids_at_timestamp.sort();
    record_ids_at_timestamp.dedup();

    Some(FeedbackCursor {
        timestamp: previous.timestamp,
        record_ids_at_timestamp,
    })
}

fn build_feedback_cursor_from_entries(entries: &[FeedbackEntry]) -> Option<FeedbackCursor> {
    let mut timestamp = None;
    let mut record_ids_at_timestamp = Vec::new();

    for record in entries.iter().flat_map(|entry| entry.reviews.iter()) {
        match timestamp {
            None => {
                timestamp = Some(record.timestamp);
                record_ids_at_timestamp.push(record.id.clone());
            }
            Some(current) if record.timestamp > current => {
                timestamp = Some(record.timestamp);
                record_ids_at_timestamp.clear();
                record_ids_at_timestamp.push(record.id.clone());
            }
            Some(current) if record.timestamp == current => {
                record_ids_at_timestamp.push(record.id.clone());
            }
            Some(_) => {}
        }
    }

    let timestamp = timestamp?;
    record_ids_at_timestamp.sort();
    record_ids_at_timestamp.dedup();

    Some(FeedbackCursor {
        timestamp,
        record_ids_at_timestamp,
    })
}

fn write_feedback_cursor_update(update: Option<FeedbackCursorUpdate>) -> Result<()> {
    if let Some(update) = update {
        write_feedback_cursor(update.path.as_path(), &update.cursor)?;
    }
    Ok(())
}

fn validate_feedback_command_args(
    targets: &[ReviewTarget],
    pr: Option<&PullRequestRef>,
) -> Result<()> {
    if pr.is_some() {
        return Ok(());
    }

    if let Some(_pull_request) = extract_pull_request_target(targets)? {
        return Err(anyhow!(
            "Pull request targets are not supported by `feedback --target`; use `feedback --pr ...` instead"
        ));
    }

    Ok(())
}

fn resolve_local_feedback_targets(targets: &[ReviewTarget]) -> Result<ResolvedTargets> {
    resolve_local_feedback_targets_with(
        targets,
        |revision| vcs::resolve_commit_id_from_workdir(revision.as_str()),
        vcs::dirty_files_from_workdir,
        vcs::files_changed_main_to_head,
    )
}

fn resolve_local_feedback_targets_with<ResolveFn, DirtyFn, MainFn>(
    targets: &[ReviewTarget],
    resolve_revision: ResolveFn,
    dirty_files: DirtyFn,
    main_diff_files: MainFn,
) -> Result<ResolvedTargets>
where
    ResolveFn: Fn(&crate::targets::RevisionExpr) -> Result<CommitId>,
    DirtyFn: Fn() -> Result<HashSet<crate::repo_path::RepoPath>>,
    MainFn: Fn() -> Result<HashSet<crate::repo_path::RepoPath>>,
{
    resolve_targets_with(
        targets,
        resolve_revision,
        dirty_files,
        main_diff_files,
        |_revision| Ok(HashSet::new()),
        |_start, _end| Ok(HashSet::new()),
    )
}

fn feedback_changed_selection(
    targets: &[ReviewTarget],
    resolved_targets: &ResolvedTargets,
) -> Option<ReviewPathSelection> {
    targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::DirtyWorktree | ReviewTarget::MainDiff))
        .then(|| resolved_targets.changed_selection())
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestFeedbackPlan {
    draft: GitHubReviewDraft,
    staged_record_ids: Vec<String>,
    staged_comments: Vec<StagedPullRequestComment>,
    skipped: Vec<SkippedPullRequestRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StagedPullRequestComment {
    record_id: String,
    comment: GitHubInlineComment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedPullRequestRecord {
    record_id: String,
    reason: PullRequestFeedbackSkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestFeedbackSkipReason {
    MissingCommentAnchor,
    MissingPullRequestCommit,
    RangeDeletedByLaterCommit,
    AmbiguousLineTranslation,
    NotPresentInPrHeadDiff,
    MixedDiffRowsUnsupported,
    PathRemappingUnsupported,
}

impl std::fmt::Display for PullRequestFeedbackSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::MissingCommentAnchor => "missing comment anchor",
            Self::MissingPullRequestCommit => {
                "anchor revision is not in the pull request commit set"
            }
            Self::RangeDeletedByLaterCommit => "anchored range was deleted by a later commit",
            Self::AmbiguousLineTranslation => {
                "anchored range could not be translated unambiguously"
            }
            Self::NotPresentInPrHeadDiff => {
                "anchored range is not present on the pull request head diff"
            }
            Self::MixedDiffRowsUnsupported => {
                "diff anchor rows cannot be represented on a single GitHub diff side"
            }
            Self::PathRemappingUnsupported => {
                "anchor path moved across the pull request in an unsupported or ambiguous way"
            }
        };
        f.write_str(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestFeedbackOutcome {
    plan: PullRequestFeedbackPlan,
    delivery: Option<PullRequestFeedbackDelivery>,
    review_url: Option<String>,
    submission: Option<PullRequestFeedbackSubmission>,
}

#[derive(Debug, Clone, Copy)]
struct PullRequestFeedbackRunOptions<'a> {
    filters: FeedbackRecordFilterParams<'a>,
    dry_run: bool,
    open: bool,
    submit: bool,
}

#[cfg(test)]
impl PullRequestFeedbackRunOptions<'_> {
    fn unfiltered(dry_run: bool, open: bool, submit: bool) -> Self {
        Self {
            filters: FeedbackRecordFilterParams::unfiltered(),
            dry_run,
            open,
            submit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestFeedbackDelivery {
    CreatePendingReview,
    AppendToPendingReview { review: PostedPullRequestReview },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestFeedbackSubmission {
    NoPendingReview,
    Target { review: PostedPullRequestReview },
    Submitted { review: PostedPullRequestReview },
}

fn run_pull_request_feedback(
    pr: &PullRequestRef,
    filters: FeedbackRecordFilterParams<'_>,
    dry_run: bool,
    open: bool,
    submit: bool,
) -> Result<()> {
    let repo_root = vcs::git_root_from_workdir()?
        .ok_or_else(|| anyhow!("git repository required for pull request feedback"))?;
    let client = GhGitHubClient;
    let prepared = prepare_pull_request_review(&repo_root, pr, &client)?;
    let outcome = run_prepared_pull_request_feedback_with_filters(
        &repo_root,
        &prepared,
        &client,
        PullRequestFeedbackRunOptions {
            filters,
            dry_run,
            open,
            submit,
        },
        open_url_in_browser,
    )?;
    print_pull_request_feedback_outcome(&prepared.metadata.pr, &outcome, dry_run);
    Ok(())
}

#[cfg(test)]
fn run_prepared_pull_request_feedback<C, O>(
    repo_root: &Path,
    prepared: &PreparedPullRequestReview,
    client: &C,
    dry_run: bool,
    open: bool,
    submit: bool,
    open_url: O,
) -> Result<PullRequestFeedbackOutcome>
where
    C: GitHubClient,
    O: FnMut(&str) -> Result<()>,
{
    run_prepared_pull_request_feedback_with_filters(
        repo_root,
        prepared,
        client,
        PullRequestFeedbackRunOptions::unfiltered(dry_run, open, submit),
        open_url,
    )
}

fn run_prepared_pull_request_feedback_with_filters<C, O>(
    repo_root: &Path,
    prepared: &PreparedPullRequestReview,
    client: &C,
    options: PullRequestFeedbackRunOptions<'_>,
    mut open_url: O,
) -> Result<PullRequestFeedbackOutcome>
where
    C: GitHubClient,
    O: FnMut(&str) -> Result<()>,
{
    let repo = gix::discover(repo_root)?;
    let store = FileStore::for_root(repo_root)?;
    let ledger_path = store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE);
    let mut ledger = GitHubDeliveryLedger::load(&ledger_path)?;
    let ledger_changed = ledger.sync_pending_reviews(&prepared.metadata.pr, |review_id| {
        client.pull_request_review_status(&prepared.metadata.pr, review_id)
    })?;
    if ledger_changed {
        ledger.save(&ledger_path)?;
    }

    if options.submit {
        return run_prepared_pull_request_feedback_submission(
            &mut ledger,
            ledger_path.as_path(),
            &prepared.metadata,
            client,
            options.dry_run,
            options.open,
            open_url,
        );
    }

    let database = store.load_database()?;
    let records = filter_pull_request_feedback_records(
        &store,
        repo_root,
        &prepared.metadata,
        database.records(),
        options.filters,
    )?;
    let excluded_ids =
        ledger.excluded_record_ids_for_head(&prepared.metadata.pr, &prepared.metadata.head_sha);
    let plan =
        build_pull_request_feedback_plan(&repo, &prepared.metadata, &records, &excluded_ids)?;

    let delivery = if plan.staged_record_ids.is_empty() {
        None
    } else {
        Some(select_pull_request_feedback_delivery(
            &ledger,
            &prepared.metadata,
            client,
        )?)
    };

    if options.dry_run || plan.staged_record_ids.is_empty() {
        return Ok(PullRequestFeedbackOutcome {
            plan,
            delivery,
            review_url: None,
            submission: None,
        });
    }

    let review_url = match delivery
        .clone()
        .unwrap_or(PullRequestFeedbackDelivery::CreatePendingReview)
    {
        PullRequestFeedbackDelivery::CreatePendingReview => {
            let review = client.create_pending_pull_request_review(
                &prepared.metadata.pr,
                &prepared.metadata.head_sha,
                &plan.draft,
            )?;
            let review_url = review.html_url.clone();
            ledger.record_pending_review(
                &prepared.metadata.pr,
                review,
                &prepared.metadata.head_sha,
                plan.staged_record_ids.clone(),
            );
            review_url
        }
        PullRequestFeedbackDelivery::AppendToPendingReview { review } => {
            for staged in &plan.staged_comments {
                client.add_comment_to_pending_pull_request_review(
                    &prepared.metadata.pr,
                    &review,
                    &staged.comment,
                )?;
                ledger.record_pending_review(
                    &prepared.metadata.pr,
                    review.clone(),
                    &prepared.metadata.head_sha,
                    vec![staged.record_id.clone()],
                );
                ledger.save(&ledger_path)?;
            }
            review.html_url
        }
    };
    ledger.save(&ledger_path)?;

    if options.open
        && let Err(error) = open_url(&review_url)
    {
        eprintln!("warning: failed to open pending review URL {review_url}: {error:#}");
    }

    Ok(PullRequestFeedbackOutcome {
        plan,
        delivery,
        review_url: Some(review_url),
        submission: None,
    })
}

fn filter_pull_request_feedback_records(
    store: &FileStore,
    repo_root: &Path,
    metadata: &PullRequestMetadata,
    records: &[Record],
    filters: FeedbackRecordFilterParams<'_>,
) -> Result<Vec<Record>> {
    let config = load_config()?;
    let block_filters = config
        .feedback
        .filters
        .resolve_filters(filters.only, filters.exclude);
    let scan_options = config.scan.resolve_options();
    let effective_since = filters.since.unwrap_or(&config.feedback.default_since);
    let since_filter = resolve_since_filter(store, effective_since.resolve()?)?;
    let allowed_revisions = Some(
        metadata
            .commits
            .iter()
            .map(|commit| commit.sha.as_str().to_string())
            .collect::<HashSet<_>>(),
    );
    let query = FeedbackQuery {
        filters: block_filters,
        explicit_selection: None,
        changed_selection: None,
        allowed_revisions,
        include_approved: filters.include_approved,
    };
    let content_source = ReviewContentSource::Revision(metadata.head_sha.clone());
    let mut resolver = RepoFeedbackContextResolver::new_for_repo_root(
        &content_source,
        &scan_options,
        None,
        repo_root,
    )?;
    let entries = collect_feedback_entries(records, &since_filter, &query, &mut resolver)?;

    Ok(entries
        .into_iter()
        .flat_map(|entry| entry.reviews)
        .collect())
}

fn run_prepared_pull_request_feedback_submission<C, O>(
    ledger: &mut GitHubDeliveryLedger,
    ledger_path: &Path,
    metadata: &PullRequestMetadata,
    client: &C,
    dry_run: bool,
    open: bool,
    mut open_url: O,
) -> Result<PullRequestFeedbackOutcome>
where
    C: GitHubClient,
    O: FnMut(&str) -> Result<()>,
{
    let pr = &metadata.pr;
    let Some(review) = find_trueflow_pending_review(ledger, pr, &metadata.head_sha, client)? else {
        return Ok(PullRequestFeedbackOutcome {
            plan: empty_pull_request_feedback_plan(),
            delivery: None,
            review_url: None,
            submission: Some(PullRequestFeedbackSubmission::NoPendingReview),
        });
    };

    if dry_run {
        return Ok(PullRequestFeedbackOutcome {
            plan: empty_pull_request_feedback_plan(),
            delivery: None,
            review_url: None,
            submission: Some(PullRequestFeedbackSubmission::Target { review }),
        });
    }

    let submitted = client.submit_pending_pull_request_review(pr, review.id)?;
    ensure_submitted_pull_request_review_state(&submitted)?;
    let review_url = submitted.html_url.clone();
    ledger.record_submitted_review(pr, review.id);
    ledger.save(ledger_path)?;

    if open && let Err(error) = open_url(&review_url) {
        eprintln!("warning: failed to open submitted review URL {review_url}: {error:#}");
    }

    Ok(PullRequestFeedbackOutcome {
        plan: empty_pull_request_feedback_plan(),
        delivery: None,
        review_url: Some(review_url),
        submission: Some(PullRequestFeedbackSubmission::Submitted { review: submitted }),
    })
}

fn ensure_submitted_pull_request_review_state(review: &PostedPullRequestReview) -> Result<()> {
    if review.state.is_terminal() {
        return Ok(());
    }

    Err(anyhow!(
        "GitHub returned non-terminal review state {:?} after submitting review {}; leaving local pending review state unchanged",
        review.state,
        review.id
    ))
}

fn empty_pull_request_feedback_plan() -> PullRequestFeedbackPlan {
    PullRequestFeedbackPlan {
        draft: GitHubReviewDraft {
            body: String::new(),
            comments: Vec::new(),
        },
        staged_record_ids: Vec::new(),
        staged_comments: Vec::new(),
        skipped: Vec::new(),
    }
}

fn select_pull_request_feedback_delivery<C>(
    ledger: &GitHubDeliveryLedger,
    metadata: &PullRequestMetadata,
    client: &C,
) -> Result<PullRequestFeedbackDelivery>
where
    C: GitHubClient,
{
    if let Some(review) =
        find_trueflow_pending_review(ledger, &metadata.pr, &metadata.head_sha, client)?
    {
        return Ok(PullRequestFeedbackDelivery::AppendToPendingReview { review });
    }

    Ok(PullRequestFeedbackDelivery::CreatePendingReview)
}

fn find_trueflow_pending_review<C>(
    ledger: &GitHubDeliveryLedger,
    pr: &ResolvedPullRequestRef,
    head_sha: &CommitId,
    client: &C,
) -> Result<Option<PostedPullRequestReview>>
where
    C: GitHubClient,
{
    for pending in ledger.pending_reviews(pr).into_iter().rev() {
        if pending.head_sha != *head_sha {
            continue;
        }
        let Some(review) = client.pull_request_review_status(pr, pending.review_id)? else {
            continue;
        };
        if review.state == PullRequestReviewState::Pending && review_body_is_trueflow_owned(&review)
        {
            return Ok(Some(review));
        }
    }

    Ok(None)
}

fn review_body_is_trueflow_owned(review: &PostedPullRequestReview) -> bool {
    review.body.contains(TRUEFLOW_PENDING_REVIEW_MARKER)
}

fn build_pull_request_feedback_plan(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    records: &[Record],
    excluded_ids: &HashSet<String>,
) -> Result<PullRequestFeedbackPlan> {
    let mut staged_comments = Vec::new();
    let mut skipped = Vec::new();

    for record in records {
        if excluded_ids.contains(&record.id) {
            continue;
        }
        let Some(note) = record.note.as_ref().filter(|note| !note.trim().is_empty()) else {
            continue;
        };
        let Some(record_revision) = record_revision(record) else {
            continue;
        };
        if !pull_request_contains_revision(metadata, record_revision) {
            continue;
        }

        match map_record_to_github_comment(repo, metadata, record, note)? {
            Ok(comment) => staged_comments.push(StagedPullRequestComment {
                record_id: record.id.clone(),
                comment,
            }),
            Err(reason) => skipped.push(SkippedPullRequestRecord {
                record_id: record.id.clone(),
                reason,
            }),
        }
    }

    staged_comments.sort_by(|left, right| {
        left.comment
            .path
            .cmp(&right.comment.path)
            .then(left.comment.line.cmp(&right.comment.line))
            .then(left.comment.body.cmp(&right.comment.body))
            .then(left.record_id.cmp(&right.record_id))
    });
    let comments = staged_comments
        .iter()
        .map(|staged| staged.comment.clone())
        .collect::<Vec<_>>();
    let mut staged_record_ids = staged_comments
        .iter()
        .map(|staged| staged.record_id.clone())
        .collect::<Vec<_>>();
    staged_record_ids.sort();
    staged_record_ids.dedup();

    Ok(PullRequestFeedbackPlan {
        draft: GitHubReviewDraft {
            body: build_pull_request_review_body(metadata, comments.len(), skipped.len()),
            comments,
        },
        staged_record_ids,
        staged_comments,
        skipped,
    })
}

fn map_record_to_github_comment(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    record: &Record,
    note: &str,
) -> Result<std::result::Result<GitHubInlineComment, PullRequestFeedbackSkipReason>> {
    let Some(anchor) = &record.comment_anchor else {
        return Ok(Err(PullRequestFeedbackSkipReason::MissingCommentAnchor));
    };

    match anchor {
        CommentAnchor::Source(anchor) => {
            map_source_anchor_to_github_comment(repo, metadata, anchor, note)
        }
        CommentAnchor::Diff(anchor) => {
            map_diff_anchor_to_github_comment(repo, metadata, anchor, note)
        }
    }
}

fn map_source_anchor_to_github_comment(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    anchor: &SourceCommentAnchor,
    note: &str,
) -> Result<std::result::Result<GitHubInlineComment, PullRequestFeedbackSkipReason>> {
    if !pull_request_contains_revision(metadata, &anchor.revision) {
        return Ok(Err(PullRequestFeedbackSkipReason::MissingPullRequestCommit));
    }

    let translated = match translate_source_anchor_to_head(repo, metadata, anchor)? {
        Ok(translated) => translated,
        Err(reason) => return Ok(Err(reason)),
    };
    let first_line = translated.first_line;
    let last_line = translated.last_line;
    if !head_diff_contains_right_side_range(
        repo,
        metadata,
        &translated.path,
        first_line,
        last_line,
    )? {
        return Ok(Err(PullRequestFeedbackSkipReason::NotPresentInPrHeadDiff));
    }

    Ok(Ok(GitHubInlineComment {
        path: translated.path,
        line: last_line,
        side: GitHubCommentSide::Right,
        start_line: (first_line != last_line).then_some(first_line),
        start_side: (first_line != last_line).then_some(GitHubCommentSide::Right),
        body: note.to_string(),
    }))
}

fn map_diff_anchor_to_github_comment(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    anchor: &DiffCommentAnchor,
    note: &str,
) -> Result<std::result::Result<GitHubInlineComment, PullRequestFeedbackSkipReason>> {
    let right_lines = match diff_anchor_lines_for_side(anchor, GitHubCommentSide::Right) {
        Ok(lines) => lines,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Some(mapped_lines) = right_lines {
        let source_anchor = SourceCommentAnchor {
            revision: anchor.revision.clone(),
            path: anchor.path.clone(),
            start_line: mapped_lines[0].saturating_sub(1),
            end_line: *mapped_lines.last().unwrap_or(&mapped_lines[0]),
        };
        return map_source_anchor_to_github_comment(repo, metadata, &source_anchor, note);
    }

    let left_lines = match diff_anchor_lines_for_side(anchor, GitHubCommentSide::Left) {
        Ok(lines) => lines,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Some(mapped_lines) = left_lines {
        let translated =
            match translate_left_diff_anchor_to_base(repo, metadata, anchor, &mapped_lines)? {
                Ok(translated) => translated,
                Err(reason) => return Ok(Err(reason)),
            };
        let first_line = translated.first_line;
        let last_line = translated.last_line;
        if !head_diff_contains_left_side_range(
            repo,
            metadata,
            &translated.path,
            first_line,
            last_line,
        )? {
            return Ok(Err(PullRequestFeedbackSkipReason::NotPresentInPrHeadDiff));
        }
        return Ok(Ok(GitHubInlineComment {
            path: translated.path,
            line: last_line,
            side: GitHubCommentSide::Left,
            start_line: (first_line != last_line).then_some(first_line),
            start_side: (first_line != last_line).then_some(GitHubCommentSide::Left),
            body: note.to_string(),
        }));
    }

    Ok(Err(PullRequestFeedbackSkipReason::MixedDiffRowsUnsupported))
}

fn diff_anchor_lines_for_side(
    anchor: &DiffCommentAnchor,
    side: GitHubCommentSide,
) -> Result<Option<Vec<u32>>, PullRequestFeedbackSkipReason> {
    let lines = anchor
        .rows
        .iter()
        .map(|row| match side {
            GitHubCommentSide::Left => row.old_line,
            GitHubCommentSide::Right => row.new_line,
        })
        .collect::<Option<Vec<_>>>();
    let Some(lines) = lines else {
        return Ok(None);
    };
    if lines.is_empty() {
        return Ok(None);
    }
    if !is_contiguous(&lines) {
        return Err(PullRequestFeedbackSkipReason::AmbiguousLineTranslation);
    }
    Ok(Some(lines))
}

fn translate_left_diff_anchor_to_base(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    anchor: &DiffCommentAnchor,
    lines: &[u32],
) -> Result<std::result::Result<TranslatedSourceAnchor, PullRequestFeedbackSkipReason>> {
    let Some(anchor_commit_index) = metadata
        .commits
        .iter()
        .position(|commit| commit.sha == anchor.revision)
    else {
        return Ok(Err(PullRequestFeedbackSkipReason::MissingPullRequestCommit));
    };
    let parent_revision = if anchor_commit_index == 0 {
        &metadata.base_sha
    } else {
        &metadata.commits[anchor_commit_index - 1].sha
    };
    if !path_exists_in_revision(repo, parent_revision, &anchor.path)? {
        return Ok(Err(
            PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit,
        ));
    }

    let mut path = anchor.path.clone();
    let mut mapped_lines = lines.to_vec();
    for commit_index in (0..anchor_commit_index).rev() {
        let current = &metadata.commits[commit_index].sha;
        let previous = if commit_index == 0 {
            &metadata.base_sha
        } else {
            &metadata.commits[commit_index - 1].sha
        };
        if !path_exists_in_revision(repo, previous, &path)? {
            let Some(source_path) = pure_rename_source(repo, previous, current, &path)? else {
                return Ok(Err(PullRequestFeedbackSkipReason::PathRemappingUnsupported));
            };
            path = source_path;
            continue;
        }
        let hunks =
            vcs::diff_hunks_for_file_in_range(repo, previous.as_str(), current.as_str(), &path)?;
        if hunks.is_empty() {
            continue;
        }
        let Some(previous_lines) = mapped_lines
            .into_iter()
            .map(|line| translate_new_line_to_old_line_strict(line, &hunks))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(Err(
                PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit,
            ));
        };
        mapped_lines = previous_lines;
        if !is_contiguous(&mapped_lines) {
            return Ok(Err(PullRequestFeedbackSkipReason::AmbiguousLineTranslation));
        }
    }

    Ok(mapped_lines
        .first()
        .zip(mapped_lines.last())
        .map(|(first, last)| TranslatedSourceAnchor {
            path,
            first_line: *first,
            last_line: *last,
        })
        .ok_or(PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranslatedSourceAnchor {
    path: crate::repo_path::RepoPath,
    first_line: u32,
    last_line: u32,
}

fn translate_source_anchor_to_head(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    anchor: &SourceCommentAnchor,
) -> Result<std::result::Result<TranslatedSourceAnchor, PullRequestFeedbackSkipReason>> {
    if anchor.end_line <= anchor.start_line {
        return Ok(Err(
            PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit,
        ));
    }
    if !path_exists_in_revision(repo, &anchor.revision, &anchor.path)? {
        return Ok(Err(
            PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit,
        ));
    }

    let mut path = anchor.path.clone();
    let mut mapped_lines =
        (anchor.start_line.saturating_add(1)..=anchor.end_line).collect::<Vec<_>>();
    let Some(start_index) = metadata
        .commits
        .iter()
        .position(|commit| commit.sha == anchor.revision)
    else {
        return Ok(Err(PullRequestFeedbackSkipReason::MissingPullRequestCommit));
    };

    for pair in metadata.commits[start_index..].windows(2) {
        let current = &pair[0].sha;
        let next = &pair[1].sha;
        if !path_exists_in_revision(repo, next, &path)? {
            let Some(renamed_path) = pure_rename_target(repo, current, next, &path)? else {
                return Ok(Err(PullRequestFeedbackSkipReason::PathRemappingUnsupported));
            };
            path = renamed_path;
            continue;
        }
        let hunks =
            vcs::diff_hunks_for_file_in_range(repo, current.as_str(), next.as_str(), &path)?;
        if hunks.is_empty() {
            continue;
        }
        let Some(next_lines) = mapped_lines
            .into_iter()
            .map(|line| translate_old_line_to_new_line_strict(line, &hunks))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(Err(
                PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit,
            ));
        };
        mapped_lines = next_lines;
        if !is_contiguous(&mapped_lines) {
            return Ok(Err(PullRequestFeedbackSkipReason::AmbiguousLineTranslation));
        }
    }

    Ok(mapped_lines
        .first()
        .zip(mapped_lines.last())
        .map(|(first, last)| TranslatedSourceAnchor {
            path,
            first_line: *first,
            last_line: *last,
        })
        .ok_or(PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit))
}

fn translate_old_line_to_new_line_strict(
    old_line: u32,
    hunks: &[crate::vcs::DiffHunk],
) -> Option<u32> {
    let mut old_cursor = 1u32;
    let mut new_cursor = 1u32;

    for hunk in hunks {
        while old_cursor < hunk.old_start {
            if old_line == old_cursor {
                return Some(new_cursor);
            }
            old_cursor = old_cursor.saturating_add(1);
            new_cursor = new_cursor.saturating_add(1);
        }

        for line in &hunk.lines {
            match line.kind {
                crate::vcs::DiffLineKind::Context => {
                    if old_line == old_cursor {
                        return Some(new_cursor);
                    }
                    old_cursor = old_cursor.saturating_add(1);
                    new_cursor = new_cursor.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Removed => {
                    if old_line == old_cursor {
                        return None;
                    }
                    old_cursor = old_cursor.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Added => {
                    new_cursor = new_cursor.saturating_add(1);
                }
            }
        }
    }

    let delta = i64::from(new_cursor) - i64::from(old_cursor);
    let mapped = i64::from(old_line) + delta;
    u32::try_from(mapped.max(1).min(i64::from(u32::MAX))).ok()
}

fn translate_new_line_to_old_line_strict(
    new_line: u32,
    hunks: &[crate::vcs::DiffHunk],
) -> Option<u32> {
    let mut old_cursor = 1u32;
    let mut new_cursor = 1u32;

    for hunk in hunks {
        while new_cursor < hunk.new_start {
            if new_line == new_cursor {
                return Some(old_cursor);
            }
            old_cursor = old_cursor.saturating_add(1);
            new_cursor = new_cursor.saturating_add(1);
        }

        for line in &hunk.lines {
            match line.kind {
                crate::vcs::DiffLineKind::Context => {
                    if new_line == new_cursor {
                        return Some(old_cursor);
                    }
                    old_cursor = old_cursor.saturating_add(1);
                    new_cursor = new_cursor.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Removed => {
                    old_cursor = old_cursor.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Added => {
                    if new_line == new_cursor {
                        return None;
                    }
                    new_cursor = new_cursor.saturating_add(1);
                }
            }
        }
    }

    let delta = i64::from(old_cursor) - i64::from(new_cursor);
    let mapped = i64::from(new_line) + delta;
    u32::try_from(mapped.max(1).min(i64::from(u32::MAX))).ok()
}

fn head_diff_contains_right_side_range(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    path: &crate::repo_path::RepoPath,
    first_line: u32,
    last_line: u32,
) -> Result<bool> {
    let hunks = vcs::diff_hunks_for_file_in_range(
        repo,
        metadata.base_sha.as_str(),
        metadata.head_sha.as_str(),
        path,
    )?;
    if hunks.is_empty() {
        return Ok(false);
    }
    Ok(diff_contains_visible_range_for_side(
        &hunks,
        GitHubCommentSide::Right,
        first_line,
        last_line,
    ))
}

fn head_diff_contains_left_side_range(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    path: &crate::repo_path::RepoPath,
    first_line: u32,
    last_line: u32,
) -> Result<bool> {
    let hunks = vcs::diff_hunks_for_file_in_range(
        repo,
        metadata.base_sha.as_str(),
        metadata.head_sha.as_str(),
        path,
    )?;
    if hunks.is_empty() {
        return Ok(false);
    }
    Ok(diff_contains_visible_range_for_side(
        &hunks,
        GitHubCommentSide::Left,
        first_line,
        last_line,
    ))
}

fn diff_contains_visible_range_for_side(
    hunks: &[crate::vcs::DiffHunk],
    side: GitHubCommentSide,
    first_line: u32,
    last_line: u32,
) -> bool {
    if first_line > last_line {
        return false;
    }

    let mut next_required = first_line;
    for hunk in hunks {
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        for line in &hunk.lines {
            let visible_line = match line.kind {
                crate::vcs::DiffLineKind::Context => Some(match side {
                    GitHubCommentSide::Left => old_line,
                    GitHubCommentSide::Right => new_line,
                }),
                crate::vcs::DiffLineKind::Added => {
                    (side == GitHubCommentSide::Right).then_some(new_line)
                }
                crate::vcs::DiffLineKind::Removed => {
                    (side == GitHubCommentSide::Left).then_some(old_line)
                }
            };

            if let Some(visible_line) = visible_line {
                if visible_line > next_required {
                    return false;
                }
                if visible_line == next_required {
                    if next_required == last_line {
                        return true;
                    }
                    next_required = next_required.saturating_add(1);
                }
            }

            match line.kind {
                crate::vcs::DiffLineKind::Context => {
                    old_line = old_line.saturating_add(1);
                    new_line = new_line.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Added => {
                    new_line = new_line.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Removed => {
                    old_line = old_line.saturating_add(1);
                }
            }
        }
    }

    false
}

fn pure_rename_target(
    repo: &gix::Repository,
    start: &CommitId,
    end: &CommitId,
    path: &crate::repo_path::RepoPath,
) -> Result<Option<crate::repo_path::RepoPath>> {
    pure_rename_path(repo, start, end, path, PureRenameDirection::Target)
}

fn pure_rename_source(
    repo: &gix::Repository,
    start: &CommitId,
    end: &CommitId,
    path: &crate::repo_path::RepoPath,
) -> Result<Option<crate::repo_path::RepoPath>> {
    pure_rename_path(repo, start, end, path, PureRenameDirection::Source)
}

#[derive(Clone, Copy)]
enum PureRenameDirection {
    Source,
    Target,
}

impl PureRenameDirection {
    fn remapped_path<'a>(
        self,
        old_path: &'a str,
        new_path: &'a str,
        query_path: &crate::repo_path::RepoPath,
    ) -> Option<&'a str> {
        match self {
            Self::Source if new_path == query_path.as_str() => Some(old_path),
            Self::Target if old_path == query_path.as_str() => Some(new_path),
            Self::Source | Self::Target => None,
        }
    }
}

fn pure_rename_path(
    repo: &gix::Repository,
    start: &CommitId,
    end: &CommitId,
    path: &crate::repo_path::RepoPath,
    direction: PureRenameDirection,
) -> Result<Option<crate::repo_path::RepoPath>> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("rename remapping requires a non-bare git repository"))?;
    let output = Command::new("git")
        .args([
            "diff",
            "--name-status",
            "--find-renames=100%",
            "--diff-filter=R",
            start.as_str(),
            end.as_str(),
            "--",
        ])
        .current_dir(workdir)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "git diff --name-status failed while remapping rename history: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut matches = Vec::new();
    for line in stdout.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        let [status, old_path, new_path] = fields.as_slice() else {
            continue;
        };
        if *status == "R100"
            && let Some(remapped_path) = direction.remapped_path(old_path, new_path, path)
        {
            matches.push(crate::repo_path::RepoPath::new(remapped_path)?);
        }
    }

    match matches.as_slice() {
        [] => Ok(None),
        [source] => Ok(Some(source.clone())),
        _ => Ok(None),
    }
}

fn path_exists_in_revision(
    repo: &gix::Repository,
    revision: &CommitId,
    path: &crate::repo_path::RepoPath,
) -> Result<bool> {
    let object = repo.rev_parse_single(revision.as_str())?;
    let commit = object.object()?.peel_to_commit()?;
    let tree = commit.tree()?;
    Ok(tree
        .lookup_entry_by_path(Path::new(path.as_str()))?
        .is_some())
}

fn pull_request_contains_revision(metadata: &PullRequestMetadata, revision: &CommitId) -> bool {
    metadata
        .commits
        .iter()
        .any(|commit| &commit.sha == revision)
}

fn record_revision(record: &Record) -> Option<&CommitId> {
    match &record.repo_ref {
        RepoRef::Vcs { revision, .. } => Some(revision),
        RepoRef::Unknown => None,
    }
}

fn build_pull_request_review_body(
    metadata: &PullRequestMetadata,
    staged_comments: usize,
    skipped_comments: usize,
) -> String {
    format!(
        "{TRUEFLOW_PENDING_REVIEW_MARKER}\nGenerated by trueflow for PR #{} at head {}.\nInline comments staged: {}.\nSkipped locally: {}.",
        metadata.pr.number, metadata.head_sha, staged_comments, skipped_comments
    )
}

fn print_pull_request_feedback_outcome(
    pr: &ResolvedPullRequestRef,
    outcome: &PullRequestFeedbackOutcome,
    dry_run: bool,
) {
    for line in pull_request_feedback_outcome_lines(pr, outcome, dry_run) {
        println!("{line}");
    }
}

fn pull_request_feedback_outcome_lines(
    pr: &ResolvedPullRequestRef,
    outcome: &PullRequestFeedbackOutcome,
    dry_run: bool,
) -> Vec<String> {
    if let Some(submission) = &outcome.submission {
        return pull_request_feedback_submission_outcome_lines(pr, submission, dry_run);
    }

    let action = if dry_run {
        "Planned"
    } else if outcome.review_url.is_some() {
        "Staged"
    } else {
        "No-op"
    };
    let mut lines = vec![format!(
        "{action} {} inline comment(s) for PR {} (skipped {}).",
        outcome.plan.draft.comments.len(),
        pr.number,
        outcome.plan.skipped.len()
    )];

    match (&outcome.delivery, dry_run, outcome.review_url.as_ref()) {
        (Some(PullRequestFeedbackDelivery::CreatePendingReview), true, _) => {
            lines.push("Would create a new trueflow pending review.".to_string());
        }
        (Some(PullRequestFeedbackDelivery::CreatePendingReview), false, Some(url)) => {
            lines.push(format!("Created pending review: {url}"));
        }
        (Some(PullRequestFeedbackDelivery::AppendToPendingReview { review }), true, _) => {
            lines.push(format!(
                "Would append to trueflow pending review {}: {}",
                review.id, review.html_url
            ));
        }
        (Some(PullRequestFeedbackDelivery::AppendToPendingReview { review }), false, Some(url)) => {
            lines.push(format!(
                "Appended to trueflow pending review {}: {url}",
                review.id
            ));
        }
        _ => {}
    }

    for skipped in &outcome.plan.skipped {
        lines.push(format!(
            "Skipped record {}: {}",
            skipped.record_id, skipped.reason
        ));
    }
    lines
}

fn pull_request_feedback_submission_outcome_lines(
    pr: &ResolvedPullRequestRef,
    submission: &PullRequestFeedbackSubmission,
    dry_run: bool,
) -> Vec<String> {
    match (submission, dry_run) {
        (PullRequestFeedbackSubmission::NoPendingReview, _) => vec![format!(
            "No trueflow-owned pending review found for PR {}.",
            pr.number
        )],
        (PullRequestFeedbackSubmission::Target { review }, true) => vec![format!(
            "Would submit trueflow pending review {} as COMMENT: {}",
            review.id, review.html_url
        )],
        (PullRequestFeedbackSubmission::Submitted { review }, false) => vec![format!(
            "Submitted trueflow pending review {} as COMMENT: {}",
            review.id, review.html_url
        )],
        (PullRequestFeedbackSubmission::Target { review }, false) => vec![format!(
            "Selected trueflow pending review {} for submission: {}",
            review.id, review.html_url
        )],
        (PullRequestFeedbackSubmission::Submitted { review }, true) => vec![format!(
            "Would have submitted trueflow pending review {} as COMMENT: {}",
            review.id, review.html_url
        )],
    }
}

fn open_url_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.arg("/C").arg("start");
        command
    };

    let status = command.arg(url).status()?;
    if !status.success() {
        return Err(anyhow!("browser opener exited with status {status}"));
    }
    Ok(())
}

fn is_contiguous(lines: &[u32]) -> bool {
    lines
        .windows(2)
        .all(|pair| pair[1] == pair[0].saturating_add(1))
}

fn render_feedback(format: FeedbackFormat, entries: Vec<FeedbackEntry>) -> Result<()> {
    match format {
        FeedbackFormat::Json => {
            print_feedback_json(&entries)?;
        }
        FeedbackFormat::Xml => {
            println!("<trueflow_feedback>");

            let mut current_file_path: Option<String> = None;
            for entry in entries {
                let FeedbackEntry {
                    file_path,
                    block,
                    reviews,
                    ..
                } = entry;
                if current_file_path.as_deref() != Some(file_path.as_str()) {
                    if current_file_path.is_some() {
                        println!("  </file>");
                    }
                    println!("  <file path=\"{}\">", escape_xml(&file_path));
                    current_file_path = Some(file_path);
                }

                print_block_xml(&block, &reviews);
            }

            if current_file_path.is_some() {
                println!("  </file>");
            }

            println!("</trueflow_feedback>");
        }
    }

    Ok(())
}

fn print_feedback_json(entries: &[FeedbackEntry]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    {
        let mut serializer = serde_json::Serializer::pretty(&mut stdout);
        let mut sequence = serializer.serialize_seq(Some(entries.len()))?;
        for entry in entries {
            sequence.serialize_element(&feedback_entry_to_json_value(entry))?;
        }
        sequence.end()?;
    }
    writeln!(stdout)?;
    Ok(())
}

fn feedback_entries_to_json_values(entries: &[FeedbackEntry]) -> Vec<serde_json::Value> {
    entries.iter().map(feedback_entry_to_json_value).collect()
}

fn feedback_entry_to_json_value(entry: &FeedbackEntry) -> serde_json::Value {
    serde_json::json!({
        "file": entry.file_path,
        "block": entry.block,
        "reviews": entry.reviews,
        "latest_verdict": entry.latest_verdict,
    })
}

fn print_block_xml(block: &crate::block::Block, reviews: &[crate::store::Record]) {
    println!(
        "    <block start_line=\"{}\" end_line=\"{}\" kind=\"{}\" hash=\"{}\">",
        block.start_line,
        block.end_line,
        escape_xml(block.kind.as_str()),
        block.hash
    );

    println!("      <context><![CDATA[");
    if block.content.contains("]]>") {
        println!("{}", block.content.replace("]]>", "]]]]><![CDATA[>"));
    } else {
        println!("{}", block.content);
    }
    println!("]]></context>");

    println!("      <reviews>");
    for review in reviews {
        let author = match &review.identity {
            crate::store::Identity::Email { email, .. } => email,
        };
        println!(
            "        <review verdict=\"{}\" author=\"{}\">",
            escape_xml(review.verdict.as_str()),
            escape_xml(author)
        );
        if let Some(note) = &review.note {
            println!("          <comment>{}</comment>", escape_xml(note));
        }
        println!("        </review>");
    }
    println!("      </reviews>");
    println!("    </block>");
}

fn escape_xml(s: &str) -> Cow<'_, str> {
    let Some(first_escape) = s.find(['&', '<', '>', '"', '\'']) else {
        return Cow::Borrowed(s);
    };

    let mut escaped = String::with_capacity(s.len() + 8);
    escaped.push_str(&s[..first_escape]);
    for ch in s[first_escape..].chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }

    Cow::Owned(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{GitRemote, PostedPullRequestReview, PullRequestReviewState};
    use crate::hashing::TreeHash;
    use crate::store::{BlockState, Identity, ReviewCheck, ReviewTargetRef, VcsSystem};
    use crate::test_git::{run_git, run_git_stdout, temp_git_repo};
    use std::cell::RefCell;
    use std::fs;

    #[test]
    fn feedback_format_exposes_xml_and_json_variants() {
        assert_eq!(
            FeedbackFormat::value_variants(),
            &[FeedbackFormat::Xml, FeedbackFormat::Json]
        );
    }

    #[test]
    fn escape_xml_escapes_only_when_needed() {
        assert_eq!(escape_xml("plain text").as_ref(), "plain text");
        assert_eq!(
            escape_xml("a&b<c>d\"e'f").as_ref(),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn validate_feedback_command_args_rejects_pull_request_target_in_target_flag() {
        let err = validate_feedback_command_args(
            &[ReviewTarget::PullRequest(PullRequestRef::Number {
                number: 11,
            })],
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("use `feedback --pr ...` instead"));
    }

    #[test]
    fn feedback_changed_selection_keeps_main_target_changed_paths() -> Result<()> {
        let changed = crate::repo_path::RepoPath::new("src/changed.rs")?;
        let unchanged = crate::repo_path::RepoPath::new("src/unchanged.rs")?;
        let resolved_targets = ResolvedTargets::new(
            crate::targets::ReviewContentSource::Workdir,
            crate::targets::ReviewDiffSelection::Targets(vec![
                crate::targets::ReviewDiffTarget::MainDiff,
            ]),
            HashSet::new(),
            Vec::new(),
            HashSet::from([changed.clone()]),
        );

        let selection = feedback_changed_selection(&[ReviewTarget::MainDiff], &resolved_targets)
            .ok_or_else(|| anyhow!("expected main target to keep changed selection"))?;

        assert!(selection.includes(&changed));
        assert!(!selection.includes(&unchanged));
        Ok(())
    }

    #[test]
    fn feedback_changed_selection_keeps_revision_ranges_record_centric() -> Result<()> {
        let changed = crate::repo_path::RepoPath::new("src/changed.rs")?;
        let resolved_targets = ResolvedTargets::new(
            crate::targets::ReviewContentSource::Revision(CommitId::new("bbbbbbb")?),
            crate::targets::ReviewDiffSelection::Targets(vec![
                crate::targets::ReviewDiffTarget::RevisionRange(crate::targets::CommitRange {
                    start: CommitId::new("aaaaaaa")?,
                    end: CommitId::new("bbbbbbb")?,
                }),
            ]),
            HashSet::new(),
            Vec::new(),
            HashSet::from([changed]),
        );

        assert!(
            feedback_changed_selection(
                &[ReviewTarget::RevisionRange(
                    crate::commands::review::RevisionRangeExpr::new("aaaaaaa", "bbbbbbb")?
                )],
                &resolved_targets,
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn resolve_local_feedback_targets_skips_revision_range_changed_paths() -> Result<()> {
        let start = CommitId::new("aaaaaaa")?;
        let end = CommitId::new("bbbbbbb")?;
        let resolved_revision_exprs = RefCell::new(Vec::new());
        let target =
            ReviewTarget::RevisionRange(crate::targets::RevisionRangeExpr::new("start", "end")?);
        let targets = [target];

        let resolved_targets = resolve_local_feedback_targets_with(
            &targets,
            |revision| {
                resolved_revision_exprs
                    .borrow_mut()
                    .push(revision.as_str().to_string());
                match revision.as_str() {
                    "start" => Ok(start.clone()),
                    "end" => Ok(end.clone()),
                    other => Err(anyhow!("unexpected revision expression {other}")),
                }
            },
            || -> Result<HashSet<crate::repo_path::RepoPath>> {
                Err(anyhow!("dirty files should not be resolved"))
            },
            || -> Result<HashSet<crate::repo_path::RepoPath>> {
                Err(anyhow!("main diff files should not be resolved"))
            },
        )?;

        assert_eq!(resolved_revision_exprs.into_inner(), vec!["start", "end"]);
        assert_eq!(
            resolved_targets.content_source,
            crate::targets::ReviewContentSource::Revision(end.clone())
        );
        assert_eq!(
            resolved_targets.diff_selection,
            crate::targets::ReviewDiffSelection::Targets(vec![
                crate::targets::ReviewDiffTarget::RevisionRange(crate::targets::CommitRange {
                    start,
                    end,
                })
            ])
        );
        assert!(feedback_changed_selection(&targets, &resolved_targets).is_none());
        Ok(())
    }

    #[test]
    fn build_feedback_cursor_after_entries_export_merges_last_cursor() -> Result<()> {
        let revision = CommitId::new("abcdef1")?;
        let mut older = review_record("older", &revision, None, None);
        older.timestamp = 9;
        let mut duplicate_previous = review_record("previous", &revision, None, None);
        duplicate_previous.timestamp = 10;
        let mut current = review_record("current", &revision, None, None);
        current.timestamp = 10;
        let entry = FeedbackEntry {
            file_path: "src/lib.rs".to_string(),
            block: crate::block::Block::new(
                "fn value() {}\n".to_string(),
                BlockKind::Function,
                0,
                1,
            ),
            reviews: vec![older, duplicate_previous, current],
            latest_verdict: "comment".to_string(),
        };
        let previous = FeedbackCursor {
            timestamp: 10,
            record_ids_at_timestamp: vec!["previous".to_string()],
        };

        let cursor = build_feedback_cursor_after_entries_export(
            &FeedbackSinceFilter::Cursor(previous),
            &[entry],
        )
        .ok_or_else(|| anyhow!("expected cursor"))?;

        assert_eq!(cursor.timestamp, 10);
        assert_eq!(
            cursor.record_ids_at_timestamp,
            vec!["current".to_string(), "previous".to_string()]
        );
        Ok(())
    }

    #[test]
    fn build_feedback_cursor_after_entries_export_keeps_last_cursor_when_empty() -> Result<()> {
        let previous = FeedbackCursor {
            timestamp: 10,
            record_ids_at_timestamp: vec!["previous".to_string()],
        };

        let cursor =
            build_feedback_cursor_after_entries_export(&FeedbackSinceFilter::Cursor(previous), &[])
                .ok_or_else(|| anyhow!("last cursor should be preserved"))?;

        assert_eq!(cursor.timestamp, 10);
        assert_eq!(cursor.record_ids_at_timestamp, vec!["previous".to_string()]);
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_maps_head_source_anchor_to_inline_comment() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_plan_source")?;
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "source-note",
            &metadata.head_sha,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: metadata.head_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                start_line: 0,
                end_line: 1,
            })),
            Some("nit: rename value"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.staged_record_ids, vec!["source-note".to_string()]);
        assert_eq!(plan.draft.comments.len(), 1);
        let comment = &plan.draft.comments[0];
        assert_eq!(comment.path, crate::repo_path::RepoPath::new("src/lib.rs")?);
        assert_eq!(comment.line, 1);
        assert_eq!(comment.side, GitHubCommentSide::Right);
        assert_eq!(comment.start_line, None);
        assert_eq!(comment.body, "nit: rename value");
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_remaps_source_anchor_across_pure_rename() -> Result<()> {
        let repo_root = temp_git_repo("feedback_plan_single_rename");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, "pub fn value() -> u32 { 1 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = commit_id_at_head(&repo_root)?;

        fs::write(&file_path, "pub fn value() -> u32 { 2 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update value"]);
        let anchor_sha = commit_id_at_head(&repo_root)?;

        let renamed_path = repo_root.join("src/main.rs");
        fs::rename(&file_path, &renamed_path)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Rename lib to main"]);
        let head_sha = commit_id_at_head(&repo_root)?;
        let metadata = pull_request_metadata(
            base_sha,
            head_sha,
            vec![
                (anchor_sha.clone(), "Update value"),
                (commit_id_at_head(&repo_root)?, "Rename lib to main"),
            ],
        );
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "rename-source",
            &anchor_sha,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: anchor_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                start_line: 0,
                end_line: 1,
            })),
            Some("rename note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.draft.comments.len(), 1);
        let comment = &plan.draft.comments[0];
        assert_eq!(
            comment.path,
            crate::repo_path::RepoPath::new("src/main.rs")?
        );
        assert_eq!(comment.line, 1);
        assert_eq!(comment.side, GitHubCommentSide::Right);
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_remaps_diff_anchor_across_pure_rename() -> Result<()> {
        let repo_root = temp_git_repo("feedback_plan_diff_rename");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, "pub fn value() -> u32 { 1 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = commit_id_at_head(&repo_root)?;

        fs::write(&file_path, "pub fn value() -> u32 { 2 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update value"]);
        let anchor_sha = commit_id_at_head(&repo_root)?;

        let renamed_path = repo_root.join("src/main.rs");
        fs::rename(&file_path, &renamed_path)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Rename lib to main"]);
        let head_sha = commit_id_at_head(&repo_root)?;
        let metadata = pull_request_metadata(
            base_sha,
            head_sha.clone(),
            vec![
                (anchor_sha.clone(), "Update value"),
                (head_sha, "Rename lib to main"),
            ],
        );
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "rename-diff",
            &anchor_sha,
            Some(CommentAnchor::Diff(DiffCommentAnchor {
                revision: anchor_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                rows: vec![crate::store::DiffCommentAnchorRow {
                    kind: crate::store::CommentAnchorDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                }],
            })),
            Some("diff rename note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.draft.comments.len(), 1);
        assert_eq!(
            plan.draft.comments[0].path,
            crate::repo_path::RepoPath::new("src/main.rs")?
        );
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_remaps_source_anchor_across_multiple_renames() -> Result<()>
    {
        let repo_root = temp_git_repo("feedback_plan_multiple_renames");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, "pub fn value() -> u32 { 1 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = commit_id_at_head(&repo_root)?;

        fs::write(&file_path, "pub fn value() -> u32 { 2 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update value"]);
        let anchor_sha = commit_id_at_head(&repo_root)?;

        let main_path = repo_root.join("src/main.rs");
        fs::rename(&file_path, &main_path)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Rename lib to main"]);
        let first_rename_sha = commit_id_at_head(&repo_root)?;

        let app_path = repo_root.join("src/app.rs");
        fs::rename(&main_path, &app_path)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Rename main to app"]);
        let head_sha = commit_id_at_head(&repo_root)?;
        let metadata = pull_request_metadata(
            base_sha,
            head_sha.clone(),
            vec![
                (anchor_sha.clone(), "Update value"),
                (first_rename_sha, "Rename lib to main"),
                (head_sha, "Rename main to app"),
            ],
        );
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "multi-rename-source",
            &anchor_sha,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: anchor_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                start_line: 0,
                end_line: 1,
            })),
            Some("multi rename note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.draft.comments.len(), 1);
        assert_eq!(
            plan.draft.comments[0].path,
            crate::repo_path::RepoPath::new("src/app.rs")?
        );
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_remaps_renamed_source_anchor_then_translates_lines()
    -> Result<()> {
        let repo_root = temp_git_repo("feedback_plan_rename_then_lines");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, "fn keep() {\n    old();\n}\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = commit_id_at_head(&repo_root)?;

        fs::write(&file_path, "fn keep() {\n    new();\n}\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update call"]);
        let anchor_sha = commit_id_at_head(&repo_root)?;

        let renamed_path = repo_root.join("src/main.rs");
        fs::rename(&file_path, &renamed_path)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Rename lib to main"]);
        let rename_sha = commit_id_at_head(&repo_root)?;

        fs::write(&renamed_path, "fn keep() {\n    setup();\n    new();\n}\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Insert setup"]);
        let head_sha = commit_id_at_head(&repo_root)?;
        let metadata = pull_request_metadata(
            base_sha,
            head_sha.clone(),
            vec![
                (anchor_sha.clone(), "Update call"),
                (rename_sha, "Rename lib to main"),
                (head_sha, "Insert setup"),
            ],
        );
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "rename-line-source",
            &anchor_sha,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: anchor_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                start_line: 1,
                end_line: 2,
            })),
            Some("line note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.draft.comments.len(), 1);
        let comment = &plan.draft.comments[0];
        assert_eq!(
            comment.path,
            crate::repo_path::RepoPath::new("src/main.rs")?
        );
        assert_eq!(comment.line, 3);
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_skips_non_pure_rename_history() -> Result<()> {
        let repo_root = temp_git_repo("feedback_plan_non_pure_rename");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, "fn keep() {\n    old();\n}\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = commit_id_at_head(&repo_root)?;

        fs::write(&file_path, "fn keep() {\n    new();\n}\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update call"]);
        let anchor_sha = commit_id_at_head(&repo_root)?;

        let renamed_path = repo_root.join("src/main.rs");
        fs::rename(&file_path, &renamed_path)?;
        fs::write(&renamed_path, "completely different\ncontent\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Rewrite while renaming"]);
        let head_sha = commit_id_at_head(&repo_root)?;
        let metadata = pull_request_metadata(
            base_sha,
            head_sha.clone(),
            vec![
                (anchor_sha.clone(), "Update call"),
                (head_sha, "Rewrite while renaming"),
            ],
        );
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "non-pure-rename-source",
            &anchor_sha,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: anchor_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                start_line: 1,
                end_line: 2,
            })),
            Some("skip note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert!(plan.draft.comments.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(
            plan.skipped[0].reason,
            PullRequestFeedbackSkipReason::PathRemappingUnsupported
        );
        Ok(())
    }

    #[test]
    fn pull_request_feedback_reuses_trueflow_owned_pending_review() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_reuse_pending")?;
        let repo = gix::discover(&repo_root)?;
        let store = FileStore::for_root(&repo_root)?;
        let old_record = review_record(
            "old-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("old note"),
        );
        let new_record = review_record(
            "new-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("new note"),
        );
        store.append(&old_record)?;
        store.append(&new_record)?;

        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.head_sha,
            vec!["old-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![pending]);
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            false,
            |_| Ok(()),
        )?;

        assert!(matches!(
            outcome.delivery,
            Some(PullRequestFeedbackDelivery::AppendToPendingReview { .. })
        ));
        assert!(client.created.borrow().is_empty());
        let appended = client.appended.borrow();
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].0, 17);
        assert_eq!(appended[0].1.body, "new note");
        assert_eq!(
            outcome.plan.staged_record_ids,
            vec!["new-record".to_string()]
        );
        assert!(
            build_pull_request_feedback_plan(
                &repo,
                &metadata,
                &[old_record, new_record],
                &GitHubDeliveryLedger::load(
                    &store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE)
                )?
                .excluded_record_ids(&metadata.pr),
            )?
            .staged_record_ids
            .is_empty()
        );

        Ok(())
    }

    #[test]
    fn pull_request_feedback_filters_records_by_since() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_pr_since")?;
        let store = FileStore::for_root(&repo_root)?;
        let mut old_record = review_record(
            "old-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("old note"),
        );
        old_record.timestamp = 1;
        let mut new_record = review_record(
            "new-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("new note"),
        );
        new_record.timestamp = 2;
        store.append(&old_record)?;
        store.append(&new_record)?;

        let since = FeedbackSinceExpr::new("2")?;
        let client = FeedbackTestGitHubClient::new(Vec::new());
        let prepared = prepared_review(metadata);
        let outcome = run_prepared_pull_request_feedback_with_filters(
            &repo_root,
            &prepared,
            &client,
            PullRequestFeedbackRunOptions {
                filters: FeedbackRecordFilterParams {
                    since: Some(&since),
                    include_approved: true,
                    only: &[],
                    exclude: &[],
                },
                dry_run: true,
                open: false,
                submit: false,
            },
            |_| Ok(()),
        )?;

        assert_eq!(
            outcome.plan.staged_record_ids,
            vec!["new-record".to_string()]
        );
        assert_eq!(outcome.plan.draft.comments.len(), 1);
        assert_eq!(outcome.plan.draft.comments[0].body, "new note");
        Ok(())
    }

    #[test]
    fn pull_request_feedback_honors_block_kind_filters() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_pr_block_filter")?;
        let store = FileStore::for_root(&repo_root)?;
        store.append(&review_record(
            "function-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("function note"),
        ))?;

        let client = FeedbackTestGitHubClient::new(Vec::new());
        let prepared = prepared_review(metadata);
        let outcome = run_prepared_pull_request_feedback_with_filters(
            &repo_root,
            &prepared,
            &client,
            PullRequestFeedbackRunOptions {
                filters: FeedbackRecordFilterParams {
                    since: None,
                    include_approved: true,
                    only: &[],
                    exclude: &[BlockKind::Code],
                },
                dry_run: true,
                open: false,
                submit: false,
            },
            |_| Ok(()),
        )?;

        assert!(outcome.plan.staged_record_ids.is_empty());
        assert!(outcome.plan.draft.comments.is_empty());
        Ok(())
    }

    #[test]
    fn pull_request_feedback_suppresses_records_approved_later() -> Result<()> {
        let (repo_root, metadata) =
            single_commit_pull_request_fixture("feedback_pr_later_approved")?;
        let store = FileStore::for_root(&repo_root)?;
        let mut comment = review_record(
            "same-target",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("stale note"),
        );
        comment.timestamp = 1;
        let mut approval = review_record("same-target", &metadata.head_sha, None, None);
        approval.verdict = crate::store::Verdict::Approved;
        approval.timestamp = 2;
        approval.path_hint = Some(crate::repo_path::RepoPath::new("src/lib.rs")?);
        approval.line_hint = Some(0);
        store.append(&comment)?;
        store.append(&approval)?;

        let client = FeedbackTestGitHubClient::new(Vec::new());
        let prepared = prepared_review(metadata);
        let outcome = run_prepared_pull_request_feedback_with_filters(
            &repo_root,
            &prepared,
            &client,
            PullRequestFeedbackRunOptions {
                filters: FeedbackRecordFilterParams {
                    since: None,
                    include_approved: false,
                    only: &[],
                    exclude: &[],
                },
                dry_run: true,
                open: false,
                submit: false,
            },
            |_| Ok(()),
        )?;

        assert!(outcome.plan.staged_record_ids.is_empty());
        assert!(outcome.plan.draft.comments.is_empty());
        Ok(())
    }

    #[test]
    fn pull_request_feedback_records_successful_appends_before_later_append_failure() -> Result<()>
    {
        let (repo_root, metadata) =
            single_commit_pull_request_fixture("feedback_partial_append_failure")?;
        let store = FileStore::for_root(&repo_root)?;
        store.append(&review_record(
            "first-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("first note"),
        ))?;
        store.append(&review_record(
            "second-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("second note"),
        ))?;

        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.head_sha,
            vec!["old-record".to_string()],
        );
        let ledger_path = store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE);
        ledger.save(&ledger_path)?;

        let client = FeedbackTestGitHubClient::new(vec![pending]).with_append_failure_at(2);
        let prepared = prepared_review(metadata.clone());
        let error = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            false,
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected append failure"));
        let appended = client.appended.borrow();
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].1.body, "first note");

        let ledger = GitHubDeliveryLedger::load(&ledger_path)?;
        let excluded = ledger.excluded_record_ids(&metadata.pr);
        assert!(excluded.contains("old-record"));
        assert!(excluded.contains("first-record"));
        assert!(!excluded.contains("second-record"));

        Ok(())
    }

    #[test]
    fn pull_request_feedback_creates_fresh_review_when_marker_mismatches() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_marker_mismatch")?;
        let store = FileStore::for_root(&repo_root)?;
        store.append(&review_record(
            "old-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("old note"),
        ))?;
        store.append(&review_record(
            "new-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("new note"),
        ))?;

        let edited = posted_review(17, PullRequestReviewState::Pending, "human edited body");
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            edited.clone(),
            &metadata.head_sha,
            vec!["old-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![edited]);
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            false,
            |_| Ok(()),
        )?;

        assert_eq!(
            outcome.delivery,
            Some(PullRequestFeedbackDelivery::CreatePendingReview)
        );
        assert_eq!(client.created.borrow().len(), 1);
        assert!(client.appended.borrow().is_empty());
        let ledger =
            GitHubDeliveryLedger::load(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;
        let excluded = ledger.excluded_record_ids(&metadata.pr);
        assert!(excluded.contains("old-record"));
        assert!(excluded.contains("new-record"));

        Ok(())
    }

    #[test]
    fn pull_request_feedback_ignores_pending_review_from_stale_head() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_stale_head")?;
        let store = FileStore::for_root(&repo_root)?;
        store.append(&review_record(
            "old-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("old note"),
        ))?;
        store.append(&review_record(
            "new-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("new note"),
        ))?;

        let stale_pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            stale_pending.clone(),
            &metadata.base_sha,
            vec!["old-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![stale_pending]);
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            false,
            |_| Ok(()),
        )?;

        assert_eq!(
            outcome.delivery,
            Some(PullRequestFeedbackDelivery::CreatePendingReview)
        );
        assert!(client.appended.borrow().is_empty());
        let created = client.created.borrow();
        assert_eq!(created.len(), 1);
        let comment_bodies = created[0]
            .comments
            .iter()
            .map(|comment| comment.body.as_str())
            .collect::<Vec<_>>();
        assert_eq!(comment_bodies, vec!["new note", "old note"]);

        Ok(())
    }

    #[test]
    fn pull_request_feedback_dry_run_reports_append_or_create_destination() -> Result<()> {
        let (repo_root, metadata) =
            single_commit_pull_request_fixture("feedback_dry_run_destination")?;
        let store = FileStore::for_root(&repo_root)?;
        store.append(&review_record(
            "new-record",
            &metadata.head_sha,
            Some(source_anchor(&metadata, 0, 1)?),
            Some("new note"),
        ))?;

        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.head_sha,
            Vec::new(),
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let prepared = prepared_review(metadata.clone());
        let append_outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &FeedbackTestGitHubClient::new(vec![pending]),
            true,
            false,
            false,
            |_| Ok(()),
        )?;
        let append_lines = pull_request_feedback_outcome_lines(&metadata.pr, &append_outcome, true);
        assert!(
            append_lines
                .iter()
                .any(|line| line.contains("Would append"))
        );

        std::fs::write(
            store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE),
            "{\"version\":1,\"pull_requests\":[]}",
        )?;
        let create_outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &FeedbackTestGitHubClient::new(Vec::new()),
            true,
            false,
            false,
            |_| Ok(()),
        )?;
        let create_lines = pull_request_feedback_outcome_lines(&metadata.pr, &create_outcome, true);
        assert!(
            create_lines
                .iter()
                .any(|line| line.contains("Would create"))
        );

        Ok(())
    }

    #[test]
    fn pull_request_feedback_submit_transitions_staged_ids_to_delivered() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_submit_pending")?;
        let store = FileStore::for_root(&repo_root)?;
        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.head_sha,
            vec!["staged-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![pending]);
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            true,
            |_| Ok(()),
        )?;

        assert!(matches!(
            outcome.submission,
            Some(PullRequestFeedbackSubmission::Submitted { .. })
        ));
        assert_eq!(client.submitted.borrow().as_slice(), &[17]);
        assert!(client.created.borrow().is_empty());
        assert!(client.appended.borrow().is_empty());
        let ledger =
            GitHubDeliveryLedger::load(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;
        assert!(ledger.pending_reviews(&metadata.pr).is_empty());
        let excluded = ledger.excluded_record_ids(&metadata.pr);
        assert!(excluded.contains("staged-record"));

        Ok(())
    }

    #[test]
    fn pull_request_feedback_submit_keeps_pending_when_response_state_is_unknown() -> Result<()> {
        let (repo_root, metadata) =
            single_commit_pull_request_fixture("feedback_submit_unknown_state")?;
        let store = FileStore::for_root(&repo_root)?;
        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.head_sha,
            vec!["staged-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![pending])
            .with_submit_state(PullRequestReviewState::Unknown);
        let prepared = prepared_review(metadata.clone());
        let error = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            true,
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("GitHub returned non-terminal review state")
        );
        assert_eq!(client.submitted.borrow().as_slice(), &[17]);
        let ledger =
            GitHubDeliveryLedger::load(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;
        assert_eq!(ledger.pending_reviews(&metadata.pr).len(), 1);
        let excluded = ledger.excluded_record_ids(&metadata.pr);
        assert!(excluded.contains("staged-record"));

        Ok(())
    }

    #[test]
    fn pull_request_feedback_submit_reports_no_pending_review() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_submit_none")?;
        let client = FeedbackTestGitHubClient::new(Vec::new());
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            true,
            |_| Ok(()),
        )?;

        assert_eq!(
            outcome.submission,
            Some(PullRequestFeedbackSubmission::NoPendingReview)
        );
        assert!(client.submitted.borrow().is_empty());
        let lines = pull_request_feedback_outcome_lines(&metadata.pr, &outcome, false);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("No trueflow-owned pending review"))
        );

        Ok(())
    }

    #[test]
    fn pull_request_feedback_submit_ignores_stale_ledger_review() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_submit_stale")?;
        let store = FileStore::for_root(&repo_root)?;
        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending,
            &metadata.head_sha,
            vec!["staged-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(Vec::new());
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            true,
            |_| Ok(()),
        )?;

        assert_eq!(
            outcome.submission,
            Some(PullRequestFeedbackSubmission::NoPendingReview)
        );
        assert!(client.submitted.borrow().is_empty());
        let ledger =
            GitHubDeliveryLedger::load(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;
        assert!(ledger.pending_reviews(&metadata.pr).is_empty());

        Ok(())
    }

    #[test]
    fn pull_request_feedback_submit_ignores_pending_review_from_stale_head() -> Result<()> {
        let (repo_root, metadata) =
            single_commit_pull_request_fixture("feedback_submit_stale_head")?;
        let store = FileStore::for_root(&repo_root)?;
        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.base_sha,
            vec!["staged-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![pending]);
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            false,
            false,
            true,
            |_| Ok(()),
        )?;

        assert_eq!(
            outcome.submission,
            Some(PullRequestFeedbackSubmission::NoPendingReview)
        );
        assert!(client.submitted.borrow().is_empty());
        let ledger =
            GitHubDeliveryLedger::load(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;
        assert_eq!(ledger.pending_reviews(&metadata.pr).len(), 1);

        Ok(())
    }

    #[test]
    fn pull_request_feedback_submit_dry_run_reports_target_review() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_submit_dry_run")?;
        let store = FileStore::for_root(&repo_root)?;
        let pending = posted_review(
            17,
            PullRequestReviewState::Pending,
            TRUEFLOW_PENDING_REVIEW_MARKER,
        );
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &metadata.pr,
            pending.clone(),
            &metadata.head_sha,
            vec!["staged-record".to_string()],
        );
        ledger.save(&store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE))?;

        let client = FeedbackTestGitHubClient::new(vec![pending]);
        let prepared = prepared_review(metadata.clone());
        let outcome = run_prepared_pull_request_feedback(
            &repo_root,
            &prepared,
            &client,
            true,
            false,
            true,
            |_| Ok(()),
        )?;

        assert!(matches!(
            outcome.submission,
            Some(PullRequestFeedbackSubmission::Target { .. })
        ));
        assert!(client.submitted.borrow().is_empty());
        let lines = pull_request_feedback_outcome_lines(&metadata.pr, &outcome, true);
        assert!(lines.iter().any(|line| line.contains("Would submit")));

        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_maps_removed_diff_rows_to_left_comment() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_plan_removed")?;
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "removed-diff",
            &metadata.head_sha,
            Some(diff_anchor(
                &metadata,
                vec![crate::store::DiffCommentAnchorRow {
                    kind: crate::store::CommentAnchorDiffLineKind::Removed,
                    old_line: Some(1),
                    new_line: None,
                }],
            )?),
            Some("nit: removed line"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.staged_record_ids, vec!["removed-diff".to_string()]);
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.draft.comments.len(), 1);
        let comment = &plan.draft.comments[0];
        assert_eq!(comment.path, crate::repo_path::RepoPath::new("src/lib.rs")?);
        assert_eq!(comment.line, 1);
        assert_eq!(comment.side, GitHubCommentSide::Left);
        assert_eq!(comment.start_line, None);
        assert_eq!(comment.start_side, None);
        assert_eq!(comment.body, "nit: removed line");
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_maps_context_and_removed_rows_to_left_range() -> Result<()>
    {
        let (repo_root, metadata) = pull_request_fixture_with_file_contents(
            "feedback_plan_context_removed",
            "fn keep() {\n    old();\n}\n",
            "fn keep() {\n}\n",
        )?;
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "context-removed-diff",
            &metadata.head_sha,
            Some(diff_anchor(
                &metadata,
                vec![
                    crate::store::DiffCommentAnchorRow {
                        kind: crate::store::CommentAnchorDiffLineKind::Context,
                        old_line: Some(1),
                        new_line: Some(1),
                    },
                    crate::store::DiffCommentAnchorRow {
                        kind: crate::store::CommentAnchorDiffLineKind::Removed,
                        old_line: Some(2),
                        new_line: None,
                    },
                ],
            )?),
            Some("range note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert_eq!(plan.draft.comments.len(), 1);
        let comment = &plan.draft.comments[0];
        assert_eq!(comment.line, 2);
        assert_eq!(comment.side, GitHubCommentSide::Left);
        assert_eq!(comment.start_line, Some(1));
        assert_eq!(comment.start_side, Some(GitHubCommentSide::Left));
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_skips_mixed_added_and_removed_rows() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_plan_mixed")?;
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "mixed-diff",
            &metadata.head_sha,
            Some(diff_anchor(
                &metadata,
                vec![
                    crate::store::DiffCommentAnchorRow {
                        kind: crate::store::CommentAnchorDiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                    },
                    crate::store::DiffCommentAnchorRow {
                        kind: crate::store::CommentAnchorDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                    },
                ],
            )?),
            Some("mixed note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert!(plan.draft.comments.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(
            plan.skipped[0].reason,
            PullRequestFeedbackSkipReason::MixedDiffRowsUnsupported
        );
        Ok(())
    }

    #[test]
    fn build_pull_request_feedback_plan_skips_noncontiguous_removed_rows() -> Result<()> {
        let (repo_root, metadata) = pull_request_fixture_with_file_contents(
            "feedback_plan_removed_noncontiguous",
            "one\ntwo\nthree\n",
            "two\n",
        )?;
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "noncontiguous-removed-diff",
            &metadata.head_sha,
            Some(diff_anchor(
                &metadata,
                vec![
                    crate::store::DiffCommentAnchorRow {
                        kind: crate::store::CommentAnchorDiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                    },
                    crate::store::DiffCommentAnchorRow {
                        kind: crate::store::CommentAnchorDiffLineKind::Removed,
                        old_line: Some(3),
                        new_line: None,
                    },
                ],
            )?),
            Some("ambiguous note"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert!(plan.draft.comments.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(
            plan.skipped[0].reason,
            PullRequestFeedbackSkipReason::AmbiguousLineTranslation
        );
        Ok(())
    }

    struct FeedbackTestGitHubClient {
        reviews: Vec<PostedPullRequestReview>,
        created: RefCell<Vec<GitHubReviewDraft>>,
        appended: RefCell<Vec<(u64, GitHubInlineComment)>>,
        submitted: RefCell<Vec<u64>>,
        submit_state: PullRequestReviewState,
        append_failure_at: Option<usize>,
    }

    impl FeedbackTestGitHubClient {
        fn new(reviews: Vec<PostedPullRequestReview>) -> Self {
            Self {
                reviews,
                created: RefCell::new(Vec::new()),
                appended: RefCell::new(Vec::new()),
                submitted: RefCell::new(Vec::new()),
                submit_state: PullRequestReviewState::Commented,
                append_failure_at: None,
            }
        }

        fn with_submit_state(mut self, submit_state: PullRequestReviewState) -> Self {
            self.submit_state = submit_state;
            self
        }

        fn with_append_failure_at(mut self, append_failure_at: usize) -> Self {
            self.append_failure_at = Some(append_failure_at);
            self
        }
    }

    impl GitHubClient for FeedbackTestGitHubClient {
        fn resolve_pull_request(
            &self,
            _pr: &ResolvedPullRequestRef,
        ) -> Result<PullRequestMetadata> {
            anyhow::bail!("not used in feedback tests")
        }

        fn create_pending_pull_request_review(
            &self,
            _pr: &ResolvedPullRequestRef,
            _head_sha: &CommitId,
            draft: &GitHubReviewDraft,
        ) -> Result<PostedPullRequestReview> {
            self.created.borrow_mut().push(draft.clone());
            Ok(posted_review(
                99,
                PullRequestReviewState::Pending,
                TRUEFLOW_PENDING_REVIEW_MARKER,
            ))
        }

        fn add_comment_to_pending_pull_request_review(
            &self,
            _pr: &ResolvedPullRequestRef,
            review: &PostedPullRequestReview,
            comment: &GitHubInlineComment,
        ) -> Result<()> {
            let append_index = self.appended.borrow().len() + 1;
            if self.append_failure_at == Some(append_index) {
                anyhow::bail!("injected append failure at {append_index}");
            }
            self.appended
                .borrow_mut()
                .push((review.id, comment.clone()));
            Ok(())
        }

        fn submit_pending_pull_request_review(
            &self,
            _pr: &ResolvedPullRequestRef,
            review_id: u64,
        ) -> Result<PostedPullRequestReview> {
            self.submitted.borrow_mut().push(review_id);
            Ok(posted_review(
                review_id,
                self.submit_state,
                TRUEFLOW_PENDING_REVIEW_MARKER,
            ))
        }

        fn pull_request_review_status(
            &self,
            _pr: &ResolvedPullRequestRef,
            review_id: u64,
        ) -> Result<Option<PostedPullRequestReview>> {
            Ok(self
                .reviews
                .iter()
                .find(|review| review.id == review_id)
                .cloned())
        }
    }

    fn posted_review(
        id: u64,
        state: PullRequestReviewState,
        body: &str,
    ) -> PostedPullRequestReview {
        PostedPullRequestReview {
            id,
            html_url: format!("https://example.test/review/{id}"),
            state,
            body: body.to_string(),
            node_id: Some(format!("R_{id}")),
        }
    }

    fn prepared_review(metadata: PullRequestMetadata) -> PreparedPullRequestReview {
        PreparedPullRequestReview {
            remote: GitRemote {
                name: "origin".to_string(),
                fetch_url: "git@github.com:jmqd/trueflow.git".to_string(),
                host: metadata.pr.host.clone(),
                owner: metadata.pr.owner.clone(),
                repo: metadata.pr.repo.clone(),
            },
            metadata,
        }
    }

    fn source_anchor(
        metadata: &PullRequestMetadata,
        start_line: u32,
        end_line: u32,
    ) -> Result<CommentAnchor> {
        Ok(CommentAnchor::Source(SourceCommentAnchor {
            revision: metadata.head_sha.clone(),
            path: crate::repo_path::RepoPath::new("src/lib.rs")?,
            start_line,
            end_line,
        }))
    }

    fn diff_anchor(
        metadata: &PullRequestMetadata,
        rows: Vec<crate::store::DiffCommentAnchorRow>,
    ) -> Result<CommentAnchor> {
        Ok(CommentAnchor::Diff(DiffCommentAnchor {
            revision: metadata.head_sha.clone(),
            path: crate::repo_path::RepoPath::new("src/lib.rs")?,
            rows,
        }))
    }

    fn commit_id_at_head(repo_root: &std::path::Path) -> Result<CommitId> {
        CommitId::new(run_git_stdout(repo_root, &["rev-parse", "HEAD"]).trim())
    }

    fn pull_request_metadata(
        base_sha: CommitId,
        head_sha: CommitId,
        commits: Vec<(CommitId, &str)>,
    ) -> PullRequestMetadata {
        PullRequestMetadata {
            pr: ResolvedPullRequestRef {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            },
            title: "Update value".to_string(),
            base_ref: "main".to_string(),
            base_sha,
            head_ref: "feature/value".to_string(),
            head_sha,
            commits: commits
                .into_iter()
                .map(|(sha, summary)| crate::github::PullRequestCommit {
                    sha,
                    summary: summary.to_string(),
                })
                .collect(),
        }
    }

    fn single_commit_pull_request_fixture(
        name: &str,
    ) -> Result<(std::path::PathBuf, PullRequestMetadata)> {
        pull_request_fixture_with_file_contents(
            name,
            "pub fn value() -> u32 { 1 }\n",
            "pub fn value() -> u32 { 2 }\n",
        )
    }

    fn pull_request_fixture_with_file_contents(
        name: &str,
        base_contents: &str,
        head_contents: &str,
    ) -> Result<(std::path::PathBuf, PullRequestMetadata)> {
        let repo_root = temp_git_repo(name);
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, base_contents)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = run_git_stdout(&repo_root, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        fs::write(&file_path, head_contents)?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update value"]);
        let head_sha = run_git_stdout(&repo_root, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        Ok((
            repo_root,
            PullRequestMetadata {
                pr: ResolvedPullRequestRef {
                    host: "github.com".to_string(),
                    owner: "jmqd".to_string(),
                    repo: "trueflow".to_string(),
                    number: 11,
                },
                title: "Update value".to_string(),
                base_ref: "main".to_string(),
                base_sha: CommitId::new(base_sha)?,
                head_ref: "feature/value".to_string(),
                head_sha: CommitId::new(&head_sha)?,
                commits: vec![crate::github::PullRequestCommit {
                    sha: CommitId::new(head_sha)?,
                    summary: "Update value".to_string(),
                }],
            },
        ))
    }

    fn review_record(
        id: &str,
        revision: &CommitId,
        comment_anchor: Option<CommentAnchor>,
        note: Option<&str>,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: crate::store::CURRENT_VERSION,
            target: ReviewTargetRef::Block {
                hash: TreeHash::from_content(id),
            },
            check: ReviewCheck::review(),
            verdict: crate::store::Verdict::Comment,
            identity: Identity::Email {
                email: "dev@example.com".to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: revision.clone(),
            },
            block_state: BlockState::Committed,
            timestamp: 1,
            path_hint: Some(crate::repo_path::RepoPath::new("src/lib.rs").unwrap()),
            line_hint: Some(0),
            note: note.map(str::to_string),
            comment_scope: None,
            comment_context: None,
            comment_anchor,
            tags: None,
            attestations: None,
        }
    }
}
