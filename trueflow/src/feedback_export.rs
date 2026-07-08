use crate::block::{Block, BlockKind, FileState};
use crate::config::BlockFilters;
use crate::feedback_since::ResolvedFeedbackSince as ParsedFeedbackSince;
use crate::policy::{should_skip_imports_by_default, should_skip_whitespace_only_by_default};
use crate::repo_path::RepoPath;
use crate::scanner::{self, ScanOptions};
use crate::store::{FileStore, Record, RepoRef, ReviewTargetRef, TreeHash};
use crate::targets::{
    ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection,
};
use crate::vcs;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const FEEDBACK_CURSOR_FILE: &str = "feedback.cursor";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackSinceFilter {
    All,
    TimestampInclusive(i64),
    Cursor(FeedbackCursor),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackCursor {
    pub timestamp: i64,
    pub record_ids_at_timestamp: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FeedbackEntry {
    pub file_path: String,
    pub block: Block,
    pub reviews: Vec<Record>,
    pub latest_verdict: String,
}

#[derive(Debug, Clone)]
pub struct FeedbackQuery {
    pub filters: BlockFilters,
    pub explicit_selection: Option<ReviewPathSelection>,
    pub changed_selection: Option<ReviewPathSelection>,
    pub allowed_revisions: Option<HashSet<String>>,
    pub include_approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FeedbackSnapshot {
    Workdir,
    Revision(String),
}

#[derive(Debug, Clone)]
pub struct ResolvedFeedbackContext {
    pub snapshot: FeedbackSnapshot,
    pub file_path: Option<String>,
    pub block: Option<Block>,
}

pub trait FeedbackContextResolver {
    fn resolve_context(&mut self, record: &Record) -> Result<ResolvedFeedbackContext>;
}

pub struct RepoFeedbackContextResolver<'a> {
    default_snapshot: FeedbackSnapshot,
    scan_options: &'a ScanOptions,
    workdir_prefix: Option<&'a str>,
    repo_root: Option<PathBuf>,
    snapshot_files: HashMap<FeedbackSnapshot, SnapshotFileCache>,
}

#[derive(Debug, Default)]
struct SnapshotFileCache {
    files: Vec<FileState>,
    complete: bool,
    attempted_paths: HashSet<RepoPath>,
}

impl<'a> RepoFeedbackContextResolver<'a> {
    pub fn new(
        content_source: &ReviewContentSource,
        scan_options: &'a ScanOptions,
        workdir_prefix: Option<&'a str>,
    ) -> Result<Self> {
        Self::new_with_repo_root(content_source, scan_options, workdir_prefix, None)
    }

    pub fn new_for_repo_root(
        content_source: &ReviewContentSource,
        scan_options: &'a ScanOptions,
        workdir_prefix: Option<&'a str>,
        repo_root: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::new_with_repo_root(
            content_source,
            scan_options,
            workdir_prefix,
            Some(repo_root.as_ref().to_path_buf()),
        )
    }

    fn new_with_repo_root(
        content_source: &ReviewContentSource,
        scan_options: &'a ScanOptions,
        workdir_prefix: Option<&'a str>,
        repo_root: Option<PathBuf>,
    ) -> Result<Self> {
        let default_snapshot = snapshot_from_content_source(content_source);
        let snapshot_files = HashMap::new();
        Ok(Self {
            default_snapshot,
            scan_options,
            workdir_prefix,
            repo_root,
            snapshot_files,
        })
    }
}

impl FeedbackContextResolver for RepoFeedbackContextResolver<'_> {
    fn resolve_context(&mut self, record: &Record) -> Result<ResolvedFeedbackContext> {
        resolve_feedback_context(
            record,
            &self.default_snapshot,
            &mut self.snapshot_files,
            self.scan_options,
            self.workdir_prefix,
            self.repo_root.as_deref(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FeedbackEntryKey {
    snapshot: FeedbackSnapshot,
    target: ReviewTargetRef,
    file_path: String,
    block_hash: TreeHash,
    start_line: usize,
    end_line: usize,
}

struct ResolvedFeedbackRecord<'a> {
    index: usize,
    record: &'a Record,
    file_path: String,
    block: Block,
    key: FeedbackEntryKey,
}

fn resolve_feedback_records<'a>(
    records: &'a [Record],
    resolver: &mut impl FeedbackContextResolver,
) -> Result<Vec<ResolvedFeedbackRecord<'a>>> {
    records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let context = resolver.resolve_context(record)?;
            let (file_path, block, key) = feedback_entry_parts(record, &context);
            Ok(ResolvedFeedbackRecord {
                index,
                record,
                file_path,
                block,
                key,
            })
        })
        .collect()
}

pub fn collect_feedback_entries(
    records: &[Record],
    since_filter: &FeedbackSinceFilter,
    query: &FeedbackQuery,
    resolver: &mut impl FeedbackContextResolver,
) -> Result<Vec<FeedbackEntry>> {
    let resolved_records = resolve_feedback_records(records, resolver)?;
    let latest_verdicts = latest_verdicts_by_entry_key(&resolved_records);
    let cursor_record_ids_at_timestamp = match since_filter {
        FeedbackSinceFilter::Cursor(cursor) => Some(
            cursor
                .record_ids_at_timestamp
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>(),
        ),
        _ => None,
    };

    let mut grouped = HashMap::<FeedbackEntryKey, FeedbackEntry>::new();
    for resolved in resolved_records {
        let record = resolved.record;
        if !record_matches_since(
            record,
            since_filter,
            cursor_record_ids_at_timestamp.as_ref(),
        ) || !record_matches_allowed_revisions(record, query.allowed_revisions.as_ref())
        {
            continue;
        }
        let file_path = &resolved.file_path;
        let block = &resolved.block;

        if !path_matches_feedback_selections(
            file_path.as_str(),
            query.explicit_selection.as_ref(),
            query.changed_selection.as_ref(),
        ) {
            continue;
        }

        let latest_verdict = latest_verdicts
            .get(&resolved.key)
            .copied()
            .unwrap_or("unreviewed");
        if !query.include_approved && latest_verdict == "approved" {
            continue;
        }
        if !query.filters.allows_block(block.kind) {
            continue;
        }
        if should_skip_whitespace_only_by_default(block, &query.filters) {
            continue;
        }
        if file_path != "<unknown>"
            && should_skip_imports_by_default(file_path.as_str(), block, &query.filters)
        {
            continue;
        }

        let ResolvedFeedbackRecord {
            record,
            file_path,
            block,
            key,
            ..
        } = resolved;
        let entry = grouped.entry(key).or_insert_with(|| FeedbackEntry {
            file_path,
            block,
            reviews: Vec::new(),
            latest_verdict: latest_verdict.to_string(),
        });
        entry.reviews.push(record.clone());
    }

    let mut entries = grouped.into_values().collect::<Vec<_>>();
    for entry in &mut entries {
        entry.reviews.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.id.cmp(&right.id))
        });
    }
    entries.sort_by(|left, right| {
        left.file_path
            .cmp(&right.file_path)
            .then_with(|| left.block.start_line.cmp(&right.block.start_line))
            .then_with(|| left.block.end_line.cmp(&right.block.end_line))
    });

    Ok(entries)
}

fn feedback_entry_parts(
    record: &Record,
    context: &ResolvedFeedbackContext,
) -> (String, Block, FeedbackEntryKey) {
    let file_path = context
        .file_path
        .clone()
        .or_else(|| record.path_hint.as_ref().map(RepoPath::to_string))
        .unwrap_or_else(|| "<unknown>".to_string());
    let block = context
        .block
        .clone()
        .unwrap_or_else(|| unresolved_block_for_record(record));
    let key = FeedbackEntryKey {
        snapshot: context.snapshot.clone(),
        target: record.target.clone(),
        file_path: file_path.clone(),
        block_hash: block.hash.clone(),
        start_line: block.start_line,
        end_line: block.end_line,
    };

    (file_path, block, key)
}

pub fn resolve_since_filter(
    store: &FileStore,
    since: ParsedFeedbackSince,
) -> Result<FeedbackSinceFilter> {
    Ok(match since {
        ParsedFeedbackSince::All => FeedbackSinceFilter::All,
        ParsedFeedbackSince::Timestamp(timestamp) => {
            FeedbackSinceFilter::TimestampInclusive(timestamp)
        }
        ParsedFeedbackSince::Last => read_feedback_cursor(feedback_cursor_path(store).as_path())?
            .map(FeedbackSinceFilter::Cursor)
            .unwrap_or(FeedbackSinceFilter::All),
    })
}

pub fn feedback_cursor_path(store: &FileStore) -> PathBuf {
    store.trueflow_dir().join(FEEDBACK_CURSOR_FILE)
}

pub fn read_feedback_cursor(path: &Path) -> Result<Option<FeedbackCursor>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if let Ok(timestamp) = trimmed.parse::<i64>() {
        return Ok(Some(FeedbackCursor {
            timestamp,
            record_ids_at_timestamp: Vec::new(),
        }));
    }

    let cursor = serde_json::from_str::<FeedbackCursor>(trimmed).map_err(|error| {
        anyhow!(
            "Invalid feedback cursor at {}: expected unix timestamp or JSON cursor ({error})",
            path.display()
        )
    })?;
    Ok(Some(cursor))
}

pub fn write_feedback_cursor(path: &Path, cursor: &FeedbackCursor) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{}\n", serde_json::to_string(cursor)?))?;
    Ok(())
}

pub fn resolve_allowed_revisions(
    diff_selection: &ReviewDiffSelection,
) -> Result<Option<HashSet<String>>> {
    let Some(diff_targets) = diff_selection.targets() else {
        return Ok(None);
    };

    let repo = vcs::repo_from_workdir()?;
    let mut allowed = HashSet::new();
    for target in diff_targets {
        match target {
            ReviewDiffTarget::MainDiff => {
                allowed.extend(revisions_in_main_to_head(&repo)?);
            }
            ReviewDiffTarget::Revision(revision) => {
                allowed.insert(resolve_revision_id(&repo, revision.as_str())?);
            }
            ReviewDiffTarget::RevisionRange(range) => {
                allowed.extend(revisions_in_range(
                    &repo,
                    range.start.as_str(),
                    range.end.as_str(),
                )?);
            }
        }
    }
    Ok(Some(allowed))
}

fn latest_verdicts_by_entry_key(
    records: &[ResolvedFeedbackRecord<'_>],
) -> HashMap<FeedbackEntryKey, &'static str> {
    let mut latest = HashMap::<FeedbackEntryKey, (i64, usize, &'static str)>::new();
    for resolved in records {
        let record = resolved.record;
        let should_replace =
            latest
                .get(&resolved.key)
                .is_none_or(|(timestamp, existing_index, _)| {
                    record.timestamp > *timestamp
                        || (record.timestamp == *timestamp && resolved.index > *existing_index)
                });
        if should_replace {
            latest.insert(
                resolved.key.clone(),
                (record.timestamp, resolved.index, record.verdict.as_str()),
            );
        }
    }
    latest
        .into_iter()
        .map(|(target, (_, _, verdict))| (target, verdict))
        .collect()
}

fn resolve_revision_id(repo: &gix::Repository, revision: &str) -> Result<String> {
    let object = repo.rev_parse_single(revision)?;
    let commit = object
        .object()?
        .peel_to_commit()
        .map_err(|error| anyhow!("revision must resolve to a commit: {error}"))?;
    Ok(commit.id().detach().to_string())
}

fn revisions_in_range(repo: &gix::Repository, start: &str, end: &str) -> Result<HashSet<String>> {
    let start_id = resolve_revision_id(repo, start)?;
    let end_id = resolve_revision_id(repo, end)?;
    revisions_reachable_from_tip_with_hidden(repo, &end_id, &[start_id])
}

fn revisions_in_main_to_head(repo: &gix::Repository) -> Result<HashSet<String>> {
    let head_id = repo.head_commit()?.id().detach().to_string();
    let mut main_ref = repo
        .find_reference("main")
        .or_else(|_| repo.find_reference("master"))
        .map_err(|error| anyhow!("Could not find main or master branch: {error}"))?;
    let main_id = main_ref.peel_to_commit()?.id().detach().to_string();
    revisions_reachable_from_tip_with_hidden(repo, &head_id, &[main_id])
}

fn revisions_reachable_from_tip_with_hidden(
    repo: &gix::Repository,
    tip: &str,
    hidden: &[String],
) -> Result<HashSet<String>> {
    let tip_id = gix::hash::ObjectId::from_hex(tip.as_bytes())?;
    let hidden_ids = hidden
        .iter()
        .map(|revision| gix::hash::ObjectId::from_hex(revision.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut revisions = HashSet::new();
    let walk = repo.rev_walk([tip_id]).with_hidden(hidden_ids).all()?;
    for info in walk {
        revisions.insert(info?.id.to_string());
    }
    Ok(revisions)
}

fn record_matches_allowed_revisions(record: &Record, allowed: Option<&HashSet<String>>) -> bool {
    let Some(allowed) = allowed else {
        return true;
    };
    match &record.repo_ref {
        RepoRef::Vcs { revision, .. } => allowed.contains(revision.as_str()),
        RepoRef::Unknown => false,
    }
}

fn snapshot_from_content_source(content_source: &ReviewContentSource) -> FeedbackSnapshot {
    match content_source {
        ReviewContentSource::Workdir => FeedbackSnapshot::Workdir,
        ReviewContentSource::Revision(revision) => {
            FeedbackSnapshot::Revision(revision.as_str().to_string())
        }
    }
}

fn resolve_feedback_context(
    record: &Record,
    default_snapshot: &FeedbackSnapshot,
    snapshot_files: &mut HashMap<FeedbackSnapshot, SnapshotFileCache>,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
) -> Result<ResolvedFeedbackContext> {
    for snapshot in candidate_snapshots_for_record(record, default_snapshot)
        .iter()
        .flatten()
    {
        if let Some(path_hint) = record.path_hint.as_ref() {
            let resolved = {
                let cache = snapshot_file_cache_for_path(
                    snapshot_files,
                    snapshot,
                    path_hint,
                    scan_options,
                    workdir_prefix,
                    repo_root,
                    snapshot == default_snapshot,
                )?;
                resolve_record_in_files(record, &cache.files)
            };
            if let Some((file_path, block)) = resolved {
                let block = scoped_block_for_record(record, &block).unwrap_or(block);
                return Ok(ResolvedFeedbackContext {
                    snapshot: snapshot.clone(),
                    file_path: Some(file_path),
                    block: Some(block),
                });
            }

            if snapshot_files
                .get(snapshot)
                .is_some_and(|cache| cache.complete)
            {
                continue;
            }
        }

        let cache = complete_snapshot_file_cache(
            snapshot_files,
            snapshot,
            scan_options,
            workdir_prefix,
            repo_root,
            snapshot == default_snapshot,
        )?;
        if let Some((file_path, block)) = resolve_record_in_files(record, &cache.files) {
            let block = scoped_block_for_record(record, &block).unwrap_or(block);
            return Ok(ResolvedFeedbackContext {
                snapshot: snapshot.clone(),
                file_path: Some(file_path),
                block: Some(block),
            });
        }
    }

    Ok(ResolvedFeedbackContext {
        snapshot: default_snapshot.clone(),
        file_path: record.path_hint.as_ref().map(RepoPath::to_string),
        block: scoped_unresolved_block_for_record(record),
    })
}

fn scoped_block_for_record(record: &Record, block: &Block) -> Option<Block> {
    let scope = record.comment_scope.as_ref()?;
    let context = record.comment_context.as_ref()?;
    let start_line = usize::try_from(scope.start_line).unwrap_or(usize::MAX);
    let end_line = usize::try_from(scope.end_line).unwrap_or(usize::MAX);
    if start_line >= end_line {
        return None;
    }

    Some(Block {
        hash: block.hash.clone(),
        content: context.clone(),
        kind: block.kind,
        tags: block.tags.clone(),
        complexity: block.complexity,
        start_line,
        end_line,
    })
}

fn scoped_unresolved_block_for_record(record: &Record) -> Option<Block> {
    let scope = record.comment_scope.as_ref()?;
    let context = record.comment_context.as_ref()?;
    let hash = match &record.target {
        ReviewTargetRef::Block { hash }
        | ReviewTargetRef::File { hash }
        | ReviewTargetRef::Tree { hash } => hash.clone(),
    };
    let start_line = usize::try_from(scope.start_line).unwrap_or(usize::MAX);
    let end_line = usize::try_from(scope.end_line).unwrap_or(usize::MAX);
    if start_line >= end_line {
        return None;
    }

    Some(Block {
        hash,
        content: context.clone(),
        kind: BlockKind::Code,
        tags: Vec::new(),
        complexity: None,
        start_line,
        end_line,
    })
}

fn candidate_snapshots_for_record(
    record: &Record,
    default_snapshot: &FeedbackSnapshot,
) -> [Option<FeedbackSnapshot>; 3] {
    let record_snapshot = match &record.repo_ref {
        RepoRef::Vcs { revision, .. } => {
            Some(FeedbackSnapshot::Revision(revision.as_str().to_string()))
        }
        RepoRef::Unknown => None,
    };
    let default_snapshot =
        (record_snapshot.as_ref() != Some(default_snapshot)).then(|| default_snapshot.clone());
    let workdir_snapshot = (!matches!(record_snapshot.as_ref(), Some(FeedbackSnapshot::Workdir))
        && !matches!(default_snapshot.as_ref(), Some(FeedbackSnapshot::Workdir)))
    .then_some(FeedbackSnapshot::Workdir);

    [record_snapshot, default_snapshot, workdir_snapshot]
}

fn load_snapshot_files_strict(
    snapshot: &FeedbackSnapshot,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
) -> Result<Vec<FileState>> {
    match snapshot {
        FeedbackSnapshot::Workdir => Ok(scanner::scan_directory(
            repo_root.unwrap_or_else(|| Path::new(".")),
            scan_options,
        )?
        .files),
        FeedbackSnapshot::Revision(revision) => {
            let repo = if let Some(repo_root) = repo_root {
                gix::discover(repo_root)?
            } else {
                vcs::repo_from_workdir()?
            };
            vcs::file_states_in_revision(&repo, revision, workdir_prefix)
        }
    }
}

fn load_snapshot_files_for_paths_strict(
    snapshot: &FeedbackSnapshot,
    paths: &HashSet<RepoPath>,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
) -> Result<Vec<FileState>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    match snapshot {
        FeedbackSnapshot::Workdir => Ok(scanner::scan_paths(
            repo_root.unwrap_or_else(|| Path::new(".")),
            paths,
            scan_options,
        )?
        .files),
        FeedbackSnapshot::Revision(revision) => {
            let repo = if let Some(repo_root) = repo_root {
                gix::discover(repo_root)?
            } else {
                vcs::repo_from_workdir()?
            };
            vcs::file_states_for_paths_in_revision(&repo, revision, paths, workdir_prefix)
        }
    }
}

fn load_snapshot_files_best_effort(
    snapshot: &FeedbackSnapshot,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
) -> Vec<FileState> {
    load_snapshot_files_strict(snapshot, scan_options, workdir_prefix, repo_root)
        .unwrap_or_default()
}

fn load_snapshot_files_for_paths_best_effort(
    snapshot: &FeedbackSnapshot,
    paths: &HashSet<RepoPath>,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
) -> Vec<FileState> {
    load_snapshot_files_for_paths_strict(snapshot, paths, scan_options, workdir_prefix, repo_root)
        .unwrap_or_default()
}

fn snapshot_file_cache_for_path<'a>(
    snapshot_files: &'a mut HashMap<FeedbackSnapshot, SnapshotFileCache>,
    snapshot: &FeedbackSnapshot,
    path: &RepoPath,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
    strict: bool,
) -> Result<&'a SnapshotFileCache> {
    let cache = snapshot_files.entry(snapshot.clone()).or_default();
    if cache.complete || cache.attempted_paths.contains(path) {
        return Ok(cache);
    }

    let paths = HashSet::from([path.clone()]);
    let files = if strict {
        load_snapshot_files_for_paths_strict(
            snapshot,
            &paths,
            scan_options,
            workdir_prefix,
            repo_root,
        )?
    } else {
        load_snapshot_files_for_paths_best_effort(
            snapshot,
            &paths,
            scan_options,
            workdir_prefix,
            repo_root,
        )
    };
    merge_snapshot_files(cache, files);
    cache.attempted_paths.insert(path.clone());
    Ok(cache)
}

fn complete_snapshot_file_cache<'a>(
    snapshot_files: &'a mut HashMap<FeedbackSnapshot, SnapshotFileCache>,
    snapshot: &FeedbackSnapshot,
    scan_options: &ScanOptions,
    workdir_prefix: Option<&str>,
    repo_root: Option<&Path>,
    strict: bool,
) -> Result<&'a SnapshotFileCache> {
    let already_complete = snapshot_files
        .get(snapshot)
        .is_some_and(|cache| cache.complete);
    if !already_complete {
        let files = if strict {
            load_snapshot_files_strict(snapshot, scan_options, workdir_prefix, repo_root)?
        } else {
            load_snapshot_files_best_effort(snapshot, scan_options, workdir_prefix, repo_root)
        };
        let cache = snapshot_files.entry(snapshot.clone()).or_default();
        cache.files = files;
        cache.complete = true;
    }

    Ok(snapshot_files
        .get(snapshot)
        .unwrap_or_else(|| panic!("snapshot cache should contain {snapshot:?}")))
}

fn merge_snapshot_files(cache: &mut SnapshotFileCache, files: Vec<FileState>) {
    for file in files {
        match cache
            .files
            .binary_search_by(|existing| existing.path.cmp(&file.path))
        {
            Ok(index) => cache.files[index] = file,
            Err(index) => cache.files.insert(index, file),
        }
    }
}

fn resolve_record_in_files(record: &Record, files: &[FileState]) -> Option<(String, Block)> {
    match &record.target {
        ReviewTargetRef::Block { hash } => {
            if let Some(path_hint) = record.path_hint.as_ref()
                && let Some(file) = files.iter().find(|file| file.path == *path_hint)
                && let Some(block) = best_block_for_hash(file, hash, record.line_hint)
            {
                return Some((file.path.as_str().to_string(), block.clone()));
            }

            files.iter().find_map(|file| {
                best_block_for_hash(file, hash, record.line_hint)
                    .map(|block| (file.path.as_str().to_string(), block.clone()))
            })
        }
        ReviewTargetRef::File { hash } => {
            if let Some(path_hint) = record.path_hint.as_ref()
                && let Some(file) = files.iter().find(|file| file.path == *path_hint)
                && file.tree_hash == *hash
                && let Some(block) = best_block_for_file(file, record.line_hint)
            {
                return Some((file.path.as_str().to_string(), block.clone()));
            }

            files.iter().find_map(|file| {
                if file.tree_hash != *hash {
                    return None;
                }
                best_block_for_file(file, record.line_hint)
                    .map(|block| (file.path.as_str().to_string(), block.clone()))
            })
        }
        ReviewTargetRef::Tree { .. } => {
            if let Some(path_hint) = record.path_hint.as_ref() {
                if let Some(file) = files.iter().find(|file| file.path == *path_hint) {
                    return best_block_for_file(file, record.line_hint)
                        .map(|block| (file.path.as_str().to_string(), block.clone()));
                }

                let path_hint = path_hint.as_str();
                if let Some(file) = files
                    .iter()
                    .find(|file| path_matches_dir_hint(file.path.as_str(), path_hint))
                {
                    return best_block_for_file(file, record.line_hint)
                        .map(|block| (file.path.as_str().to_string(), block.clone()));
                }
            }

            files.iter().find_map(|file| {
                best_block_for_file(file, record.line_hint)
                    .map(|block| (file.path.as_str().to_string(), block.clone()))
            })
        }
    }
}

fn best_block_for_hash<'a>(
    file: &'a FileState,
    hash: &TreeHash,
    line_hint: Option<u32>,
) -> Option<&'a Block> {
    let line_hint = line_hint.map(|line| line as usize);
    line_hint
        .and_then(|line| {
            file.blocks.iter().find(|block| {
                block.hash == *hash && block.start_line <= line && line < block.end_line
            })
        })
        .or_else(|| file.blocks.iter().find(|block| block.hash == *hash))
}

fn best_block_for_file(file: &FileState, line_hint: Option<u32>) -> Option<&Block> {
    let line_hint = line_hint.map(|line| line as usize);
    line_hint
        .and_then(|line| {
            file.blocks
                .iter()
                .find(|block| block.start_line <= line && line < block.end_line)
        })
        .or_else(|| file.blocks.first())
}

fn path_matches_dir_hint(file_path: &str, dir_hint: &str) -> bool {
    file_path == dir_hint
        || file_path
            .strip_prefix(dir_hint)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_matches_feedback_selections(
    file_path: &str,
    explicit_selection: Option<&ReviewPathSelection>,
    changed_selection: Option<&ReviewPathSelection>,
) -> bool {
    let Ok(repo_path) = RepoPath::new(file_path) else {
        return false;
    };

    if let Some(selection) = explicit_selection
        && !selection.includes(&repo_path)
    {
        return false;
    }

    if let Some(selection) = changed_selection
        && !selection.includes(&repo_path)
    {
        return false;
    }

    true
}

fn unresolved_block_for_record(record: &Record) -> Block {
    if let Some(block) = scoped_unresolved_block_for_record(record) {
        return block;
    }

    let hash = match &record.target {
        ReviewTargetRef::Block { hash }
        | ReviewTargetRef::File { hash }
        | ReviewTargetRef::Tree { hash } => hash.clone(),
    };
    let start_line = record.line_hint.unwrap_or(0) as usize;
    let end_line = start_line.saturating_add(1);

    Block {
        hash,
        content: "[unresolved historical context]".to_string(),
        kind: BlockKind::Code,
        tags: Vec::new(),
        complexity: None,
        start_line,
        end_line,
    }
}

fn record_matches_since(
    record: &Record,
    since_filter: &FeedbackSinceFilter,
    cursor_record_ids_at_timestamp: Option<&HashSet<&str>>,
) -> bool {
    match since_filter {
        FeedbackSinceFilter::All => true,
        FeedbackSinceFilter::TimestampInclusive(timestamp) => record.timestamp >= *timestamp,
        FeedbackSinceFilter::Cursor(cursor) => {
            record.timestamp > cursor.timestamp
                || (record.timestamp == cursor.timestamp
                    && !cursor_record_ids_at_timestamp.map_or_else(
                        || {
                            cursor
                                .record_ids_at_timestamp
                                .iter()
                                .any(|id| id == &record.id)
                        },
                        |record_ids| record_ids.contains(record.id.as_str()),
                    ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;
    use crate::store::{BlockState, Identity, ReviewCheck, VcsSystem, Verdict};
    use std::iter::FromIterator;

    struct FakeResolver {
        contexts: HashMap<String, ResolvedFeedbackContext>,
    }

    impl FeedbackContextResolver for FakeResolver {
        fn resolve_context(&mut self, record: &Record) -> Result<ResolvedFeedbackContext> {
            self.contexts
                .get(&record.id)
                .cloned()
                .ok_or_else(|| anyhow!("missing fake context for {}", record.id))
        }
    }

    #[test]
    fn collect_feedback_entries_filters_by_record_revision_membership() {
        let keep = build_record("keep", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        let skip = build_record("skip", "bbbbbbb", "src/lib.rs", 20, Verdict::Comment);
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "keep".to_string(),
                    resolved_context("src/lib.rs", "pub fn keep() {}\n"),
                ),
                (
                    "skip".to_string(),
                    resolved_context("src/lib.rs", "pub fn keep() {}\n"),
                ),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: Some(HashSet::from_iter(["aaaaaaa".to_string()])),
            include_approved: true,
        };

        let entries = collect_feedback_entries(
            &[keep.clone(), skip],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].reviews.len(), 1);
        assert_eq!(entries[0].reviews[0].id, keep.id);
    }

    #[test]
    fn collect_feedback_entries_excludes_targets_whose_latest_verdict_is_approved() {
        let earlier = build_record("earlier", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        let later = build_record("later", "aaaaaaa", "src/lib.rs", 20, Verdict::Approved);
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "earlier".to_string(),
                    resolved_context("src/lib.rs", "pub fn core() {}\n"),
                ),
                (
                    "later".to_string(),
                    resolved_context("src/lib.rs", "pub fn core() {}\n"),
                ),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: false,
        };

        let entries = collect_feedback_entries(
            &[earlier, later],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert!(entries.is_empty());
    }

    #[test]
    fn collect_feedback_entries_excludes_approved_target_across_hint_precision() {
        let mut earlier = build_record("earlier", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        earlier.line_hint = Some(0);
        let mut later = build_record("later", "aaaaaaa", "src/lib.rs", 20, Verdict::Approved);
        later.line_hint = None;
        let context = resolved_context("src/lib.rs", "pub fn core() {}\n");
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                ("earlier".to_string(), context.clone()),
                ("later".to_string(), context),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: false,
        };

        let entries = collect_feedback_entries(
            &[earlier, later],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert!(entries.is_empty());
    }

    #[test]
    fn collect_feedback_entries_keeps_same_hash_comment_when_other_file_is_approved() {
        let shared_hash = TreeHash::from_content("fn duplicate() {}\n");
        let mut comment = build_record("comment", "aaaaaaa", "src/a.rs", 10, Verdict::Comment);
        comment.target = ReviewTargetRef::Block {
            hash: shared_hash.clone(),
        };
        let mut approved = build_record("approved", "aaaaaaa", "src/b.rs", 20, Verdict::Approved);
        approved.target = ReviewTargetRef::Block { hash: shared_hash };
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "comment".to_string(),
                    resolved_context("src/a.rs", "fn duplicate() {}\n"),
                ),
                (
                    "approved".to_string(),
                    resolved_context("src/b.rs", "fn duplicate() {}\n"),
                ),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: false,
        };

        let entries = collect_feedback_entries(
            &[comment.clone(), approved],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_path, "src/a.rs");
        assert_eq!(entries[0].reviews[0].id, comment.id);
    }

    #[test]
    fn collect_feedback_entries_intersects_explicit_and_changed_path_selections() {
        let keep = build_record("keep", "aaaaaaa", "src/keep.rs", 10, Verdict::Comment);
        let skip = build_record("skip", "aaaaaaa", "src/skip.rs", 20, Verdict::Comment);
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "keep".to_string(),
                    resolved_context("src/keep.rs", "pub fn keep() {}\n"),
                ),
                (
                    "skip".to_string(),
                    resolved_context("src/skip.rs", "pub fn skip() {}\n"),
                ),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: Some(ReviewPathSelection::Scoped {
                files: HashSet::new(),
                dirs: vec![RepoPath::new("src").unwrap()],
                changed: None,
            }),
            changed_selection: Some(ReviewPathSelection::Scoped {
                files: HashSet::new(),
                dirs: Vec::new(),
                changed: Some(HashSet::from_iter([RepoPath::new("src/keep.rs").unwrap()])),
            }),
            allowed_revisions: None,
            include_approved: true,
        };

        let entries = collect_feedback_entries(
            &[keep.clone(), skip],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_path, "src/keep.rs");
        assert_eq!(entries[0].reviews[0].id, keep.id);
    }

    #[test]
    fn collect_feedback_entries_groups_reviews_and_sorts_by_timestamp() {
        let later = build_record("later", "aaaaaaa", "src/lib.rs", 20, Verdict::Comment);
        let earlier = build_record("earlier", "aaaaaaa", "src/lib.rs", 10, Verdict::Rejected);
        let context = resolved_context("src/lib.rs", "pub fn core() {}\n");
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                ("later".to_string(), context.clone()),
                ("earlier".to_string(), context),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: true,
        };

        let entries = collect_feedback_entries(
            &[later, earlier],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]
                .reviews
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            vec!["earlier", "later"]
        );
    }

    #[test]
    fn collect_feedback_entries_filters_cursor_ids_once_per_export() {
        let previous = build_record(
            "previous",
            "aaaaaaa",
            "src/previous.rs",
            10,
            Verdict::Comment,
        );
        let keep_same_timestamp =
            build_record("keep", "aaaaaaa", "src/keep.rs", 10, Verdict::Comment);
        let keep_newer = build_record("newer", "aaaaaaa", "src/newer.rs", 11, Verdict::Comment);
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "previous".to_string(),
                    resolved_context("src/previous.rs", "pub fn previous() {}\n"),
                ),
                (
                    "keep".to_string(),
                    resolved_context("src/keep.rs", "pub fn keep() {}\n"),
                ),
                (
                    "newer".to_string(),
                    resolved_context("src/newer.rs", "pub fn newer() {}\n"),
                ),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: true,
        };
        let since_filter = FeedbackSinceFilter::Cursor(FeedbackCursor {
            timestamp: 10,
            record_ids_at_timestamp: vec!["previous".to_string()],
        });

        let entries = collect_feedback_entries(
            &[previous, keep_same_timestamp, keep_newer],
            &since_filter,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(
            entries
                .iter()
                .flat_map(|entry| entry.reviews.iter())
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            vec!["keep", "newer"]
        );
    }

    #[test]
    fn collect_feedback_entries_resolves_each_record_once() {
        struct CountingResolver {
            contexts: HashMap<String, ResolvedFeedbackContext>,
            calls: Vec<String>,
        }

        impl FeedbackContextResolver for CountingResolver {
            fn resolve_context(&mut self, record: &Record) -> Result<ResolvedFeedbackContext> {
                self.calls.push(record.id.clone());
                self.contexts
                    .get(&record.id)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing fake context for {}", record.id))
            }
        }

        let earlier = build_record("earlier", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        let later = build_record("later", "aaaaaaa", "src/lib.rs", 20, Verdict::Comment);
        let context = resolved_context("src/lib.rs", "pub fn core() {}\n");
        let mut resolver = CountingResolver {
            contexts: HashMap::from_iter([
                ("earlier".to_string(), context.clone()),
                ("later".to_string(), context),
            ]),
            calls: Vec::new(),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: true,
        };

        collect_feedback_entries(
            &[earlier, later],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(resolver.calls, vec!["earlier", "later"]);
    }

    #[test]
    fn resolve_record_in_files_uses_line_hint_for_duplicate_block_hashes() {
        let first = Block::new("fn duplicate() {}\n".to_string(), BlockKind::Function, 1, 2);
        let second = Block::new(
            "fn duplicate() {}\n".to_string(),
            BlockKind::Function,
            10,
            11,
        );
        let file = FileState::from_text(
            RepoPath::new("src/lib.rs").unwrap(),
            crate::analysis::Language::Rust,
            b"fn duplicate() {}\n\nfn duplicate() {}\n",
            vec![first.clone(), second.clone()],
        );
        let mut record = build_record("duplicate", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        record.target = ReviewTargetRef::Block { hash: first.hash };
        record.line_hint = Some(10);

        let (_, block) = resolve_record_in_files(&record, &[file])
            .unwrap_or_else(|| panic!("expected duplicate block to resolve"));

        assert_eq!(block.start_line, second.start_line);
    }

    #[test]
    fn candidate_snapshots_keep_record_default_workdir_order_without_duplicates() {
        let record = build_record("record", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);

        assert_eq!(
            candidate_snapshots_for_record(
                &record,
                &FeedbackSnapshot::Revision("aaaaaaa".to_string()),
            )
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
            vec![
                FeedbackSnapshot::Revision("aaaaaaa".to_string()),
                FeedbackSnapshot::Workdir,
            ]
        );
        assert_eq!(
            candidate_snapshots_for_record(&record, &FeedbackSnapshot::Workdir)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            vec![
                FeedbackSnapshot::Revision("aaaaaaa".to_string()),
                FeedbackSnapshot::Workdir,
            ]
        );

        let mut workdir_record = record;
        workdir_record.repo_ref = RepoRef::Unknown;
        assert_eq!(
            candidate_snapshots_for_record(&workdir_record, &FeedbackSnapshot::Workdir)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            vec![FeedbackSnapshot::Workdir]
        );
    }

    #[test]
    fn merge_snapshot_files_replaces_paths_and_preserves_sorted_order() {
        let mut cache = SnapshotFileCache::default();
        merge_snapshot_files(
            &mut cache,
            vec![
                test_file_state("src/c.rs", b"fn c() {}\n"),
                test_file_state("src/a.rs", b"fn a() {}\n"),
                test_file_state("src/b.rs", b"fn old_b() {}\n"),
            ],
        );
        let replacement = test_file_state("src/b.rs", b"fn new_b() {}\n");
        let replacement_hash = replacement.tree_hash.clone();

        merge_snapshot_files(&mut cache, vec![replacement]);

        assert_eq!(
            cache
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/b.rs", "src/c.rs"]
        );
        assert_eq!(cache.files[1].tree_hash, replacement_hash);
    }

    #[test]
    fn repo_resolver_uses_path_hint_without_full_workdir_scan() {
        let repo = trueflow_test_support::temp_test_dir("feedback_resolver_targeted");
        write_rust_file(&repo, "src/target.rs", "fn target() {}\n");
        write_rust_file(&repo, "src/unrelated.rs", "fn unrelated() {}\n");
        let hash = first_scanned_block_hash(&repo, "src/target.rs");
        let mut record = build_record("target", "aaaaaaa", "src/target.rs", 10, Verdict::Comment);
        record.repo_ref = RepoRef::Unknown;
        record.target = ReviewTargetRef::Block { hash };

        let scan_options = ScanOptions::default();
        let mut resolver = RepoFeedbackContextResolver::new_for_repo_root(
            &ReviewContentSource::Workdir,
            &scan_options,
            None,
            &repo,
        )
        .unwrap_or_else(|error| panic!("resolver construction should be lazy: {error}"));

        let context = resolver
            .resolve_context(&record)
            .unwrap_or_else(|error| panic!("context should resolve: {error}"));

        assert_eq!(context.file_path.as_deref(), Some("src/target.rs"));
        let cache = resolver
            .snapshot_files
            .get(&FeedbackSnapshot::Workdir)
            .unwrap_or_else(|| panic!("workdir cache should be present"));
        assert!(!cache.complete);
        assert_eq!(cache.files.len(), 1);
        assert_eq!(cache.files[0].path.as_str(), "src/target.rs");
        assert!(
            cache
                .attempted_paths
                .contains(&RepoPath::new("src/target.rs").unwrap())
        );
    }

    #[test]
    fn repo_resolver_falls_back_to_full_scan_when_path_hint_misses() {
        let repo = trueflow_test_support::temp_test_dir("feedback_resolver_fallback");
        write_rust_file(&repo, "src/target.rs", "fn target() {}\n");
        write_rust_file(&repo, "src/unrelated.rs", "fn unrelated() {}\n");
        let hash = first_scanned_block_hash(&repo, "src/target.rs");
        let mut record = build_record("target", "aaaaaaa", "src/moved.rs", 10, Verdict::Comment);
        record.repo_ref = RepoRef::Unknown;
        record.target = ReviewTargetRef::Block { hash };

        let scan_options = ScanOptions::default();
        let mut resolver = RepoFeedbackContextResolver::new_for_repo_root(
            &ReviewContentSource::Workdir,
            &scan_options,
            None,
            &repo,
        )
        .unwrap_or_else(|error| panic!("resolver construction should be lazy: {error}"));

        let context = resolver
            .resolve_context(&record)
            .unwrap_or_else(|error| panic!("context should resolve after fallback: {error}"));

        assert_eq!(context.file_path.as_deref(), Some("src/target.rs"));
        let cache = resolver
            .snapshot_files
            .get(&FeedbackSnapshot::Workdir)
            .unwrap_or_else(|| panic!("workdir cache should be present"));
        assert!(cache.complete);
        assert_eq!(
            cache
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/target.rs", "src/unrelated.rs"]
        );
    }

    #[test]
    fn read_feedback_cursor_supports_legacy_timestamp_format() {
        let dir = trueflow_test_support::temp_test_dir("feedback_cursor_legacy");
        let path = dir.join("feedback.cursor");
        fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("failed to create dir: {error}"));
        fs::write(&path, "1234\n")
            .unwrap_or_else(|error| panic!("failed to write cursor: {error}"));

        let cursor = read_feedback_cursor(&path)
            .unwrap_or_else(|error| panic!("legacy cursor should parse: {error}"))
            .unwrap_or_else(|| panic!("expected cursor"));
        assert_eq!(cursor.timestamp, 1234);
        assert!(cursor.record_ids_at_timestamp.is_empty());
    }

    fn test_file_state(path: &str, content: &[u8]) -> FileState {
        FileState::from_text(
            RepoPath::new(path).unwrap_or_else(|error| panic!("valid repo path: {error}")),
            crate::analysis::Language::Rust,
            content,
            Vec::new(),
        )
    }

    fn write_rust_file(root: &Path, path: &str, content: &str) {
        let full_path = root.join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("failed to create test parent: {error}"));
        }
        fs::write(&full_path, content)
            .unwrap_or_else(|error| panic!("failed to write test file: {error}"));
    }

    fn first_scanned_block_hash(root: &Path, path: &str) -> TreeHash {
        let repo_path = RepoPath::new(path).unwrap_or_else(|error| panic!("valid path: {error}"));
        let paths = HashSet::from_iter([repo_path.clone()]);
        let files = scanner::scan_paths(root, &paths, &ScanOptions::default())
            .unwrap_or_else(|error| panic!("test scan should succeed: {error}"))
            .files;
        let file = files
            .iter()
            .find(|file| file.path == repo_path)
            .unwrap_or_else(|| panic!("test scan should include {path}"));
        file.blocks
            .first()
            .unwrap_or_else(|| panic!("test scan should find a block in {path}"))
            .hash
            .clone()
    }

    fn build_record(
        id: &str,
        revision: &str,
        path: &str,
        timestamp: i64,
        verdict: Verdict,
    ) -> Record {
        Record {
            id: id.to_string(),
            version: crate::store::CURRENT_VERSION,
            target: ReviewTargetRef::Block {
                hash: TreeHash::from_content(path),
            },
            check: ReviewCheck::review(),
            verdict,
            identity: Identity::Email {
                email: "dev@example.com".to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: crate::store::CommitId::new(revision)
                    .unwrap_or_else(|error| panic!("valid test revision: {error}")),
            },
            block_state: BlockState::Committed,
            timestamp,
            path_hint: Some(
                RepoPath::new(path).unwrap_or_else(|error| panic!("valid repo path: {error}")),
            ),
            line_hint: Some(0),
            note: None,
            comment_scope: None,
            comment_context: None,
            comment_anchor: None,
            tags: None,
            attestations: None,
        }
    }

    fn resolved_context(path: &str, content: &str) -> ResolvedFeedbackContext {
        ResolvedFeedbackContext {
            snapshot: FeedbackSnapshot::Workdir,
            file_path: Some(path.to_string()),
            block: Some(Block::new(content.to_string(), BlockKind::Code, 0, 1)),
        }
    }
}
