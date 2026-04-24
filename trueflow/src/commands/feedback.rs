use crate::block::BlockKind;
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::feedback_export::{
    FeedbackEntry, FeedbackQuery, RepoFeedbackContextResolver, build_feedback_cursor,
    collect_feedback_entries, feedback_cursor_path, resolve_allowed_revisions,
    resolve_since_filter, write_feedback_cursor,
};
use crate::feedback_since::{FeedbackSinceExpr, ResolvedFeedbackSince as ParsedFeedbackSince};
use crate::github::{
    GhGitHubClient, GitHubClient, GitHubCommentSide, GitHubInlineComment, GitHubReviewDraft,
    PreparedPullRequestReview, PullRequestMetadata, PullRequestRef, ResolvedPullRequestRef,
    prepare_pull_request_review,
};
use crate::github_delivery::{GITHUB_DELIVERY_LEDGER_FILE, GitHubDeliveryLedger};
use crate::store::{
    CommentAnchor, CommitId, DiffCommentAnchor, FileStore, Record, RepoRef, ReviewStore,
    SourceCommentAnchor,
};
use crate::targets::{
    ReviewTarget, extract_pull_request_target, resolve_targets, workdir_prefix_from_git_root,
};
use crate::vcs;
use anyhow::{Result, anyhow};
use clap::ValueEnum;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

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
    pub targets: &'a [ReviewTarget],
    pub include_approved: bool,
    pub only: &'a [BlockKind],
    pub exclude: &'a [BlockKind],
}

pub fn run(_context: &TrueflowContext, params: FeedbackParams<'_>) -> Result<()> {
    let FeedbackParams {
        format,
        since,
        pr,
        dry_run,
        open,
        targets,
        include_approved,
        only,
        exclude,
    } = params;

    validate_feedback_command_args(targets, pr)?;
    if let Some(pr) = pr {
        return run_pull_request_feedback(pr, dry_run, open);
    }

    let config = load_config()?;
    let filters = config.feedback.filters.resolve_filters(only, exclude);
    let scan_options = config.scan.resolve_options();
    let effective_since = since.unwrap_or(&config.feedback.default_since);
    let resolved_targets = resolve_targets(targets)?;

    let store = crate::store::FileStore::new()?;
    let database = store.load_database()?;
    let since_mode = effective_since.resolve()?;
    let since_filter = resolve_since_filter(&store, since_mode)?;
    let explicit_selection = resolved_targets.explicit_selection();
    let changed_selection = targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::DirtyWorktree))
        .then(|| resolved_targets.changed_selection())
        .flatten();
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

    render_feedback(format, entries)?;

    if matches!(since_mode, ParsedFeedbackSince::Last)
        && let Some(cursor) = build_feedback_cursor(database.records())
    {
        write_feedback_cursor(feedback_cursor_path(&store).as_path(), &cursor)?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestFeedbackPlan {
    draft: GitHubReviewDraft,
    staged_record_ids: Vec<String>,
    skipped: Vec<SkippedPullRequestRecord>,
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
    RemovedDiffRowsUnsupported,
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
            Self::RemovedDiffRowsUnsupported => {
                "diff anchors containing removed-only rows are not supported yet"
            }
        };
        f.write_str(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestFeedbackOutcome {
    plan: PullRequestFeedbackPlan,
    review_url: Option<String>,
}

fn run_pull_request_feedback(pr: &PullRequestRef, dry_run: bool, open: bool) -> Result<()> {
    let repo_root = vcs::git_root_from_workdir()?
        .ok_or_else(|| anyhow!("git repository required for pull request feedback"))?;
    let client = GhGitHubClient;
    let prepared = prepare_pull_request_review(&repo_root, pr, &client)?;
    let outcome = run_prepared_pull_request_feedback(
        &repo_root,
        &prepared,
        &client,
        dry_run,
        open,
        open_url_in_browser,
    )?;
    print_pull_request_feedback_outcome(&prepared.metadata.pr, &outcome, dry_run);
    Ok(())
}

fn run_prepared_pull_request_feedback<C, O>(
    repo_root: &Path,
    prepared: &PreparedPullRequestReview,
    client: &C,
    dry_run: bool,
    open: bool,
    mut open_url: O,
) -> Result<PullRequestFeedbackOutcome>
where
    C: GitHubClient,
    O: FnMut(&str) -> Result<()>,
{
    let repo = gix::discover(repo_root)?;
    let store = FileStore::new()?;
    let database = store.load_database()?;
    let ledger_path = store.trueflow_dir().join(GITHUB_DELIVERY_LEDGER_FILE);
    let mut ledger = GitHubDeliveryLedger::load(&ledger_path)?;
    ledger.sync_pending_reviews(&prepared.metadata.pr, |review_id| {
        client.pull_request_review_status(&prepared.metadata.pr, review_id)
    })?;

    let excluded_ids = ledger.excluded_record_ids(&prepared.metadata.pr);
    let plan = build_pull_request_feedback_plan(
        &repo,
        &prepared.metadata,
        database.records(),
        &excluded_ids,
    )?;

    if dry_run || plan.staged_record_ids.is_empty() {
        return Ok(PullRequestFeedbackOutcome {
            plan,
            review_url: None,
        });
    }

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
    ledger.save(&ledger_path)?;

    if open && let Err(error) = open_url(&review_url) {
        eprintln!("warning: failed to open pending review URL {review_url}: {error:#}");
    }

    Ok(PullRequestFeedbackOutcome {
        plan,
        review_url: Some(review_url),
    })
}

fn build_pull_request_feedback_plan(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    records: &[Record],
    excluded_ids: &HashSet<String>,
) -> Result<PullRequestFeedbackPlan> {
    let mut comments = Vec::new();
    let mut staged_record_ids = Vec::new();
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
            Ok(comment) => {
                comments.push(comment);
                staged_record_ids.push(record.id.clone());
            }
            Err(reason) => skipped.push(SkippedPullRequestRecord {
                record_id: record.id.clone(),
                reason,
            }),
        }
    }

    comments.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.body.cmp(&right.body))
    });
    staged_record_ids.sort();
    staged_record_ids.dedup();

    Ok(PullRequestFeedbackPlan {
        draft: GitHubReviewDraft {
            body: build_pull_request_review_body(metadata, comments.len(), skipped.len()),
            comments,
        },
        staged_record_ids,
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

    let Some((first_line, last_line)) = translate_source_anchor_to_head(repo, metadata, anchor)?
    else {
        return Ok(Err(
            PullRequestFeedbackSkipReason::RangeDeletedByLaterCommit,
        ));
    };
    if !head_diff_contains_right_side_range(repo, metadata, &anchor.path, first_line, last_line)? {
        return Ok(Err(PullRequestFeedbackSkipReason::NotPresentInPrHeadDiff));
    }

    Ok(Ok(GitHubInlineComment {
        path: anchor.path.clone(),
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
    let new_lines = anchor
        .rows
        .iter()
        .map(|row| {
            row.new_line?;
            Some(row.new_line)
        })
        .collect::<Option<Vec<_>>>();
    let Some(new_lines) = new_lines else {
        return Ok(Err(
            PullRequestFeedbackSkipReason::RemovedDiffRowsUnsupported,
        ));
    };
    let mapped_lines = new_lines.into_iter().flatten().collect::<Vec<_>>();
    if mapped_lines.is_empty() {
        return Ok(Err(
            PullRequestFeedbackSkipReason::RemovedDiffRowsUnsupported,
        ));
    }
    if !is_contiguous(&mapped_lines) {
        return Ok(Err(PullRequestFeedbackSkipReason::AmbiguousLineTranslation));
    }

    let source_anchor = SourceCommentAnchor {
        revision: anchor.revision.clone(),
        path: anchor.path.clone(),
        start_line: mapped_lines[0].saturating_sub(1),
        end_line: *mapped_lines.last().unwrap_or(&mapped_lines[0]),
    };
    map_source_anchor_to_github_comment(repo, metadata, &source_anchor, note)
}

fn translate_source_anchor_to_head(
    repo: &gix::Repository,
    metadata: &PullRequestMetadata,
    anchor: &SourceCommentAnchor,
) -> Result<Option<(u32, u32)>> {
    if anchor.end_line <= anchor.start_line {
        return Ok(None);
    }
    if !path_exists_in_revision(repo, &anchor.revision, &anchor.path)? {
        return Ok(None);
    }

    let mut mapped_lines =
        (anchor.start_line.saturating_add(1)..=anchor.end_line).collect::<Vec<_>>();
    let Some(start_index) = metadata
        .commits
        .iter()
        .position(|commit| commit.sha == anchor.revision)
    else {
        return Ok(None);
    };

    for pair in metadata.commits[start_index..].windows(2) {
        let current = &pair[0].sha;
        let next = &pair[1].sha;
        if !path_exists_in_revision(repo, next, &anchor.path)? {
            return Ok(None);
        }
        let hunks =
            vcs::diff_hunks_for_file_in_range(repo, current.as_str(), next.as_str(), &anchor.path)?;
        if hunks.is_empty() {
            continue;
        }
        let Some(next_lines) = mapped_lines
            .into_iter()
            .map(|line| translate_old_line_to_new_line_strict(line, &hunks))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        mapped_lines = next_lines;
        if !is_contiguous(&mapped_lines) {
            return Ok(None);
        }
    }

    Ok(mapped_lines
        .first()
        .zip(mapped_lines.last())
        .map(|(first, last)| (*first, *last)))
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
    let visible_lines = visible_head_diff_lines(&hunks);
    Ok((first_line..=last_line).all(|line| visible_lines.contains(&line)))
}

fn visible_head_diff_lines(hunks: &[crate::vcs::DiffHunk]) -> HashSet<u32> {
    let mut lines = HashSet::new();
    for hunk in hunks {
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        for line in &hunk.lines {
            match line.kind {
                crate::vcs::DiffLineKind::Context => {
                    lines.insert(new_line);
                    old_line = old_line.saturating_add(1);
                    new_line = new_line.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Added => {
                    lines.insert(new_line);
                    new_line = new_line.saturating_add(1);
                }
                crate::vcs::DiffLineKind::Removed => {
                    old_line = old_line.saturating_add(1);
                }
            }
        }
    }
    lines
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
        "<!-- trueflow:pending-review -->\nGenerated by trueflow for PR #{} at head {}.\nInline comments staged: {}.\nSkipped locally: {}.",
        metadata.pr.number, metadata.head_sha, staged_comments, skipped_comments
    )
}

fn print_pull_request_feedback_outcome(
    pr: &ResolvedPullRequestRef,
    outcome: &PullRequestFeedbackOutcome,
    dry_run: bool,
) {
    let action = if dry_run {
        "Planned"
    } else if outcome.review_url.is_some() {
        "Staged"
    } else {
        "No-op"
    };
    println!(
        "{action} {} inline comment(s) for PR {} (skipped {}).",
        outcome.plan.draft.comments.len(),
        pr.number,
        outcome.plan.skipped.len()
    );
    if let Some(url) = &outcome.review_url {
        println!("Pending review: {url}");
    }
    for skipped in &outcome.plan.skipped {
        println!("Skipped record {}: {}", skipped.record_id, skipped.reason);
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
            let export_list = entries
                .into_iter()
                .map(|entry| {
                    serde_json::json!({
                        "file": entry.file_path,
                        "block": entry.block,
                        "reviews": entry.reviews,
                        "latest_verdict": entry.latest_verdict,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&export_list)?);
        }
        FeedbackFormat::Xml => {
            println!("<trueflow_feedback>");

            let mut current_file_path: Option<String> = None;
            for entry in entries {
                if current_file_path.as_deref() != Some(entry.file_path.as_str()) {
                    if current_file_path.is_some() {
                        println!("  </file>");
                    }
                    println!("  <file path=\"{}\">", escape_xml(&entry.file_path));
                    current_file_path = Some(entry.file_path.clone());
                }

                print_block_xml(&entry.block, &entry.reviews);
            }

            if current_file_path.is_some() {
                println!("  </file>");
            }

            println!("</trueflow_feedback>");
        }
    }

    Ok(())
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
    let safe_content = block.content.replace("]]>", "]]]]><![CDATA[>");
    println!("{safe_content}");
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

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;
    use crate::store::{BlockState, Identity, ReviewCheck, ReviewTargetRef, VcsSystem};
    use crate::test_git::{run_git, run_git_stdout, temp_git_repo};
    use std::fs;

    #[test]
    fn feedback_format_exposes_xml_and_json_variants() {
        assert_eq!(
            FeedbackFormat::value_variants(),
            &[FeedbackFormat::Xml, FeedbackFormat::Json]
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
    fn build_pull_request_feedback_plan_skips_removed_diff_rows() -> Result<()> {
        let (repo_root, metadata) = single_commit_pull_request_fixture("feedback_plan_removed")?;
        let repo = gix::discover(&repo_root)?;
        let record = review_record(
            "removed-diff",
            &metadata.head_sha,
            Some(CommentAnchor::Diff(DiffCommentAnchor {
                revision: metadata.head_sha.clone(),
                path: crate::repo_path::RepoPath::new("src/lib.rs")?,
                rows: vec![crate::store::DiffCommentAnchorRow {
                    kind: crate::store::CommentAnchorDiffLineKind::Removed,
                    old_line: Some(1),
                    new_line: None,
                }],
            })),
            Some("nit: removed line"),
        );

        let plan = build_pull_request_feedback_plan(&repo, &metadata, &[record], &HashSet::new())?;

        assert!(plan.draft.comments.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(
            plan.skipped[0].reason,
            PullRequestFeedbackSkipReason::RemovedDiffRowsUnsupported
        );
        Ok(())
    }

    fn single_commit_pull_request_fixture(
        name: &str,
    ) -> Result<(std::path::PathBuf, PullRequestMetadata)> {
        let repo_root = temp_git_repo(name);
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap())?;
        fs::write(&file_path, "pub fn value() -> u32 { 1 }\n")?;
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial main"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        let base_sha = run_git_stdout(&repo_root, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        fs::write(&file_path, "pub fn value() -> u32 { 2 }\n")?;
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
