use crate::block::{Block, BlockKind, ByteSpan, FileState};
use crate::config::BlockFilters;
use crate::feedback_since::ResolvedFeedbackSince as ParsedFeedbackSince;
use crate::repo_path::RepoPath;
use crate::scanner::{self, ScanOptions};
use crate::store::{FileStore, Record, RepoRef, ReviewTargetRef, TreeHash};
use crate::targets::{
    ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection,
};
use crate::vcs;
use anyhow::{Result, anyhow};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const FEEDBACK_CURSOR_FILE: &str = "feedback.cursor";
const FEEDBACK_CURSOR_SCHEMA_VERSION: u32 = 1;
const FEEDBACK_CURSOR_DIGEST_DOMAIN: &[u8] = b"trueflow.feedback.cursor.v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackSinceFilter {
    All,
    TimestampInclusive(i64),
    Cursor(FeedbackCursor),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackCursor {
    pub version: u32,
    pub checkpoint: FeedbackCursorCheckpoint,
    pub pending_record_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackCursorCheckpoint {
    pub record_count: u64,
    pub last_record_id: Option<String>,
    pub record_ids_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedbackBlockView {
    pub hash: TreeHash,
    pub content: String,
    pub kind: BlockKind,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u32>,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub byte_span: Option<ByteSpan>,
}

impl FeedbackBlockView {
    pub fn from_canonical_block(block: &Block) -> Self {
        Self {
            hash: block.hash.clone(),
            content: block.content.clone(),
            kind: block.kind,
            tags: block.tags.clone(),
            complexity: block.complexity,
            start_line: block.start_line,
            end_line: block.end_line,
            byte_span: Some(block.byte_span()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedbackEntry {
    pub file_path: String,
    pub block: FeedbackBlockView,
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
pub struct ResolvedFeedbackBlock {
    pub unscoped: Block,
    pub presentation: FeedbackBlockView,
    pub presentation_is_scoped: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedFeedbackContext {
    pub snapshot: FeedbackSnapshot,
    pub file_path: Option<String>,
    pub block: Option<ResolvedFeedbackBlock>,
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
struct FeedbackPresentationEntryKey {
    snapshot: FeedbackSnapshot,
    target: ReviewTargetRef,
    file_path: String,
    block_hash: TreeHash,
    start_line: usize,
    end_line: usize,
    discriminator: FeedbackPresentationDiscriminator,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FeedbackPresentationDiscriminator {
    Unscoped,
    Scoped {
        start_line: u32,
        end_line: u32,
        context: String,
        anchor: Option<FeedbackCommentAnchorKey>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FeedbackCommentAnchorKey {
    Source {
        revision: String,
        path: String,
        start_line: u32,
        end_line: u32,
    },
    Diff {
        revision: String,
        path: String,
        rows: Vec<FeedbackDiffAnchorRowKey>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FeedbackDiffAnchorRowKey {
    kind: FeedbackDiffAnchorRowKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FeedbackDiffAnchorRowKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FeedbackVerdictKey {
    snapshot: FeedbackSnapshot,
    locator: FeedbackVerdictLocator,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FeedbackVerdictLocator {
    Block {
        target_hash: TreeHash,
        file_path: String,
        block_hash: TreeHash,
        start_line: usize,
        end_line: usize,
    },
    File {
        target_hash: TreeHash,
        path: Option<String>,
    },
    Tree {
        target_hash: TreeHash,
        path: Option<String>,
    },
}

struct ResolvedFeedbackRecord<'a> {
    index: usize,
    record: &'a Record,
    file_path: String,
    block: FeedbackBlockView,
    entry_key: FeedbackPresentationEntryKey,
    verdict_key: FeedbackVerdictKey,
}

fn resolve_feedback_records<'a>(
    records: &'a [Record],
    allowed_revisions: Option<&HashSet<String>>,
    resolver: &mut impl FeedbackContextResolver,
) -> Result<Vec<ResolvedFeedbackRecord<'a>>> {
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| record_matches_allowed_revisions(record, allowed_revisions))
        .filter(|(_, record)| !matches!(record.target, ReviewTargetRef::Declaration { .. }))
        .map(|(index, record)| {
            let context = resolver.resolve_context(record)?;
            let (file_path, block, entry_key, verdict_key) = feedback_entry_parts(record, &context);
            Ok(ResolvedFeedbackRecord {
                index,
                record,
                file_path,
                block,
                entry_key,
                verdict_key,
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
    let validated_cursor = match since_filter {
        FeedbackSinceFilter::Cursor(cursor) => Some(validate_feedback_cursor(cursor, records)?),
        _ => None,
    };
    let resolved_records =
        resolve_feedback_records(records, query.allowed_revisions.as_ref(), resolver)?;
    let latest_verdicts = latest_verdicts_by_verdict_key(&resolved_records);

    let mut grouped = HashMap::<FeedbackPresentationEntryKey, FeedbackEntry>::new();
    for resolved in resolved_records {
        let record = resolved.record;
        if !record_matches_since(
            record,
            resolved.index,
            since_filter,
            validated_cursor.as_ref(),
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
            .get(&resolved.verdict_key)
            .copied()
            .unwrap_or("unreviewed");
        if !query.include_approved && latest_verdict == "approved" {
            continue;
        }
        if !query.filters.allows_block(block.kind) {
            continue;
        }
        if should_skip_feedback_whitespace_only_by_default(block, &query.filters) {
            continue;
        }
        if file_path != "<unknown>"
            && should_skip_feedback_imports_by_default(file_path.as_str(), block, &query.filters)
        {
            continue;
        }

        let ResolvedFeedbackRecord {
            record,
            file_path,
            block,
            entry_key,
            ..
        } = resolved;
        let entry = grouped.entry(entry_key).or_insert_with(|| FeedbackEntry {
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
) -> (
    String,
    FeedbackBlockView,
    FeedbackPresentationEntryKey,
    FeedbackVerdictKey,
) {
    let file_path = context
        .file_path
        .clone()
        .or_else(|| record.path_hint.as_ref().map(RepoPath::to_string))
        .unwrap_or_else(|| "<unknown>".to_string());
    let (block, presentation_is_scoped) = match context.block.as_ref() {
        Some(block) => (block.presentation.clone(), block.presentation_is_scoped),
        None => match scoped_unresolved_feedback_block_for_record(record) {
            Some(block) => (block, true),
            None => (unresolved_unscoped_feedback_block_for_record(record), false),
        },
    };
    let entry_key = FeedbackPresentationEntryKey {
        snapshot: context.snapshot.clone(),
        target: record.target.clone(),
        file_path: file_path.clone(),
        block_hash: block.hash.clone(),
        start_line: block.start_line,
        end_line: block.end_line,
        discriminator: feedback_presentation_discriminator(record, presentation_is_scoped),
    };
    let verdict_key = feedback_verdict_key(record, context, &file_path);

    (file_path, block, entry_key, verdict_key)
}

fn feedback_presentation_discriminator(
    record: &Record,
    presentation_is_scoped: bool,
) -> FeedbackPresentationDiscriminator {
    let (Some(scope), Some(context)) = (
        record.comment_scope.as_ref(),
        record.comment_context.as_ref(),
    ) else {
        return FeedbackPresentationDiscriminator::Unscoped;
    };
    if !presentation_is_scoped {
        return FeedbackPresentationDiscriminator::Unscoped;
    }

    FeedbackPresentationDiscriminator::Scoped {
        start_line: scope.start_line,
        end_line: scope.end_line,
        context: context.clone(),
        anchor: feedback_comment_anchor_key(record),
    }
}

fn feedback_comment_anchor_key(record: &Record) -> Option<FeedbackCommentAnchorKey> {
    match record.comment_anchor.as_ref()? {
        crate::store::CommentAnchor::Source(anchor) => Some(FeedbackCommentAnchorKey::Source {
            revision: anchor.revision.as_str().to_string(),
            path: anchor.path.as_str().to_string(),
            start_line: anchor.start_line,
            end_line: anchor.end_line,
        }),
        crate::store::CommentAnchor::Diff(anchor) => Some(FeedbackCommentAnchorKey::Diff {
            revision: anchor.revision.as_str().to_string(),
            path: anchor.path.as_str().to_string(),
            rows: anchor
                .rows
                .iter()
                .map(|row| FeedbackDiffAnchorRowKey {
                    kind: match row.kind {
                        crate::store::CommentAnchorDiffLineKind::Context => {
                            FeedbackDiffAnchorRowKind::Context
                        }
                        crate::store::CommentAnchorDiffLineKind::Added => {
                            FeedbackDiffAnchorRowKind::Added
                        }
                        crate::store::CommentAnchorDiffLineKind::Removed => {
                            FeedbackDiffAnchorRowKind::Removed
                        }
                    },
                    old_line: row.old_line,
                    new_line: row.new_line,
                })
                .collect(),
        }),
        crate::store::CommentAnchor::Declaration(_) => None,
    }
}

fn feedback_verdict_key(
    record: &Record,
    context: &ResolvedFeedbackContext,
    canonical_file_path: &str,
) -> FeedbackVerdictKey {
    let locator = match &record.target {
        ReviewTargetRef::Block { hash } => {
            let (block_hash, start_line, end_line) = context
                .block
                .as_ref()
                .map(|block| {
                    (
                        block.unscoped.hash.clone(),
                        block.unscoped.start_line,
                        block.unscoped.end_line,
                    )
                })
                .unwrap_or_else(|| {
                    let (start_line, end_line) = unresolved_block_line_range(record);
                    (feedback_target_hash(record), start_line, end_line)
                });
            FeedbackVerdictLocator::Block {
                target_hash: hash.clone(),
                file_path: canonical_file_path.to_string(),
                block_hash,
                start_line,
                end_line,
            }
        }
        ReviewTargetRef::File { hash } => FeedbackVerdictLocator::File {
            target_hash: hash.clone(),
            path: record.path_hint.as_ref().map(RepoPath::to_string),
        },
        ReviewTargetRef::Tree { hash } => FeedbackVerdictLocator::Tree {
            target_hash: hash.clone(),
            path: record.path_hint.as_ref().map(RepoPath::to_string),
        },
        ReviewTargetRef::Declaration { .. } => {
            unreachable!("declaration records are filtered before ordinary feedback export")
        }
    };

    FeedbackVerdictKey {
        snapshot: context.snapshot.clone(),
        locator,
    }
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

#[derive(Debug)]
struct ValidatedFeedbackCursor {
    checkpoint_len: usize,
    pending_record_ids: HashSet<String>,
}

pub(crate) struct FeedbackCursorReadGuard {
    lock_file: File,
    cursor: Option<FeedbackCursor>,
}

impl FeedbackCursorReadGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let lock_file = open_feedback_cursor_lock(path)?;
        lock_file.lock_shared()?;
        let cursor = read_feedback_cursor_unlocked(path)?;
        Ok(Self { lock_file, cursor })
    }

    pub(crate) fn cursor(&self) -> Option<&FeedbackCursor> {
        self.cursor.as_ref()
    }
}

impl Drop for FeedbackCursorReadGuard {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

pub(crate) struct FeedbackCursorUpdateGuard {
    path: PathBuf,
    lock_file: File,
    cursor: Option<FeedbackCursor>,
}

impl FeedbackCursorUpdateGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let lock_file = open_feedback_cursor_lock(path)?;
        lock_file.lock_exclusive()?;
        let cursor = read_feedback_cursor_unlocked(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            lock_file,
            cursor,
        })
    }

    pub(crate) fn cursor(&self) -> Option<&FeedbackCursor> {
        self.cursor.as_ref()
    }

    pub(crate) fn commit(
        self,
        records: &[Record],
        exported_record_ids: &HashSet<String>,
    ) -> Result<()> {
        let current = read_feedback_cursor_unlocked(&self.path)?;
        let next = advance_feedback_cursor(current.as_ref(), records, exported_record_ids)?;
        write_feedback_cursor_atomically(&self.path, &next)
    }
}

impl Drop for FeedbackCursorUpdateGuard {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

pub(crate) fn feedback_since_filter_for_cursor(
    cursor: Option<&FeedbackCursor>,
    records: &[Record],
) -> Result<FeedbackSinceFilter> {
    match cursor {
        Some(cursor) => {
            validate_feedback_cursor(cursor, records)?;
            Ok(FeedbackSinceFilter::Cursor(cursor.clone()))
        }
        None => {
            logical_record_indices(records)?;
            Ok(FeedbackSinceFilter::All)
        }
    }
}

pub fn read_feedback_cursor(path: &Path) -> Result<Option<FeedbackCursor>> {
    let guard = FeedbackCursorReadGuard::acquire(path)?;
    Ok(guard.cursor().cloned())
}

pub(crate) fn feedback_cursor_lock_path(cursor_path: &Path) -> PathBuf {
    let file_name = cursor_path
        .file_name()
        .unwrap_or_else(|| OsStr::new(FEEDBACK_CURSOR_FILE));
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    cursor_path.with_file_name(lock_name)
}

pub(crate) fn advance_feedback_cursor(
    previous: Option<&FeedbackCursor>,
    records: &[Record],
    exported_record_ids: &HashSet<String>,
) -> Result<FeedbackCursor> {
    let (checkpoint_len, mut pending_record_ids) = match previous {
        Some(cursor) => {
            let validated = validate_feedback_cursor(cursor, records)?;
            (validated.checkpoint_len, validated.pending_record_ids)
        }
        None => {
            logical_record_indices(records)?;
            (0, HashSet::new())
        }
    };
    let record_indices = logical_record_indices(records)?;

    for exported_id in exported_record_ids {
        let index = record_indices.get(exported_id.as_str()).ok_or_else(|| {
            anyhow!(
                "Cannot advance feedback cursor: exported record ID {exported_id:?} is absent from the transaction snapshot"
            )
        })?;
        if *index < checkpoint_len && !pending_record_ids.contains(exported_id) {
            return Err(anyhow!(
                "Cannot advance feedback cursor: exported record ID {exported_id:?} was already delivered"
            ));
        }
    }

    pending_record_ids.extend(
        records[checkpoint_len..]
            .iter()
            .map(|record| record.id.clone()),
    );
    pending_record_ids.retain(|id| !exported_record_ids.contains(id));
    let mut pending_record_ids = pending_record_ids.into_iter().collect::<Vec<_>>();
    pending_record_ids.sort_unstable();

    Ok(FeedbackCursor {
        version: FEEDBACK_CURSOR_SCHEMA_VERSION,
        checkpoint: feedback_cursor_checkpoint(records)?,
        pending_record_ids,
    })
}

fn open_feedback_cursor_lock(path: &Path) -> Result<File> {
    let parent = feedback_cursor_parent(path);
    fs::create_dir_all(parent)?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(feedback_cursor_lock_path(path))?)
}

fn feedback_cursor_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn read_feedback_cursor_unlocked(path: &Path) -> Result<Option<FeedbackCursor>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "Invalid feedback cursor at {}: cursor file is empty",
            path.display()
        ));
    }
    let cursor = serde_json::from_str::<FeedbackCursor>(trimmed).map_err(|error| {
        anyhow!(
            "Invalid feedback cursor at {}: expected feedback cursor schema v{FEEDBACK_CURSOR_SCHEMA_VERSION} ({error})",
            path.display()
        )
    })?;
    validate_feedback_cursor_shape(&cursor)?;
    Ok(Some(cursor))
}

fn validate_feedback_cursor_shape(cursor: &FeedbackCursor) -> Result<()> {
    if cursor.version != FEEDBACK_CURSOR_SCHEMA_VERSION {
        return Err(anyhow!(
            "Unsupported feedback cursor version {}; expected {}",
            cursor.version,
            FEEDBACK_CURSOR_SCHEMA_VERSION
        ));
    }
    match (
        cursor.checkpoint.record_count,
        cursor.checkpoint.last_record_id.as_ref(),
    ) {
        (0, Some(_)) => {
            return Err(anyhow!(
                "Invalid feedback cursor checkpoint: empty checkpoint must not contain a tail record ID"
            ));
        }
        (count, None) if count > 0 => {
            return Err(anyhow!(
                "Invalid feedback cursor checkpoint: non-empty checkpoint must contain a tail record ID"
            ));
        }
        _ => {}
    }
    if cursor
        .pending_record_ids
        .windows(2)
        .any(|ids| ids[0] >= ids[1])
    {
        return Err(anyhow!(
            "Invalid feedback cursor: pending record IDs must be sorted and unique"
        ));
    }
    Ok(())
}

fn validate_feedback_cursor(
    cursor: &FeedbackCursor,
    records: &[Record],
) -> Result<ValidatedFeedbackCursor> {
    validate_feedback_cursor_shape(cursor)?;
    logical_record_indices(records)?;
    let checkpoint_len = usize::try_from(cursor.checkpoint.record_count).map_err(|_error| {
        anyhow!(
            "Invalid feedback cursor checkpoint: record count {} does not fit this platform",
            cursor.checkpoint.record_count
        )
    })?;
    if checkpoint_len > records.len() {
        return Err(anyhow!(
            "Invalid feedback cursor checkpoint: record count {} exceeds the current database length {}",
            checkpoint_len,
            records.len()
        ));
    }
    let prefix = &records[..checkpoint_len];
    let expected_last_record_id = prefix.last().map(|record| record.id.as_str());
    if cursor.checkpoint.last_record_id.as_deref() != expected_last_record_id {
        return Err(anyhow!(
            "Invalid feedback cursor checkpoint: tail record ID does not match the database prefix"
        ));
    }
    let expected_digest = feedback_cursor_prefix_digest(prefix)?;
    if cursor.checkpoint.record_ids_sha256 != expected_digest {
        return Err(anyhow!(
            "Invalid feedback cursor checkpoint: record ID digest does not match the database prefix"
        ));
    }

    let prefix_ids = prefix
        .iter()
        .map(|record| record.id.as_str())
        .collect::<HashSet<_>>();
    let mut pending_record_ids = HashSet::with_capacity(cursor.pending_record_ids.len());
    for pending_id in &cursor.pending_record_ids {
        if !prefix_ids.contains(pending_id.as_str()) {
            return Err(anyhow!(
                "Invalid feedback cursor: pending record ID {pending_id:?} is outside the checkpoint prefix"
            ));
        }
        if !pending_record_ids.insert(pending_id.clone()) {
            return Err(anyhow!(
                "Invalid feedback cursor: duplicate pending record ID {pending_id:?}"
            ));
        }
    }
    Ok(ValidatedFeedbackCursor {
        checkpoint_len,
        pending_record_ids,
    })
}

fn logical_record_indices(records: &[Record]) -> Result<HashMap<&str, usize>> {
    let mut record_indices = HashMap::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        if record_indices.insert(record.id.as_str(), index).is_some() {
            return Err(anyhow!(
                "Invalid review database: duplicate feedback record ID {:?}",
                record.id
            ));
        }
    }
    Ok(record_indices)
}

fn feedback_cursor_checkpoint(records: &[Record]) -> Result<FeedbackCursorCheckpoint> {
    let record_count = u64::try_from(records.len()).map_err(|_error| {
        anyhow!(
            "Cannot advance feedback cursor: database length {} exceeds cursor capacity",
            records.len()
        )
    })?;
    Ok(FeedbackCursorCheckpoint {
        record_count,
        last_record_id: records.last().map(|record| record.id.clone()),
        record_ids_sha256: feedback_cursor_prefix_digest(records)?,
    })
}

fn feedback_cursor_prefix_digest(records: &[Record]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(FEEDBACK_CURSOR_DIGEST_DOMAIN);
    for record in records {
        let length = u64::try_from(record.id.len()).map_err(|_error| {
            anyhow!(
                "Cannot hash feedback cursor record ID {:?}: byte length exceeds cursor capacity",
                record.id
            )
        })?;
        hasher.update(length.to_be_bytes());
        hasher.update(record.id.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_feedback_cursor_atomically(path: &Path, cursor: &FeedbackCursor) -> Result<()> {
    write_feedback_cursor_atomically_with(path, cursor, |_| Ok(()))
}

fn write_feedback_cursor_atomically_with<F>(
    path: &Path,
    cursor: &FeedbackCursor,
    before_rename: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let mut serialized = serde_json::to_vec(cursor)?;
    serialized.push(b'\n');
    let parent = feedback_cursor_parent(path);
    fs::create_dir_all(parent)?;
    let (temporary_path, mut temporary) = create_feedback_cursor_temporary_file(path)?;
    let mut renamed = false;
    let result = (|| -> Result<()> {
        temporary.write_all(&serialized)?;
        temporary.sync_all()?;
        drop(temporary);
        before_rename(&temporary_path)?;
        fs::rename(&temporary_path, path)?;
        renamed = true;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && !renamed {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_feedback_cursor_temporary_file(path: &Path) -> Result<(PathBuf, File)> {
    let parent = feedback_cursor_parent(path);
    let cursor_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new(FEEDBACK_CURSOR_FILE))
        .to_string_lossy();
    for _ in 0..16 {
        let temporary_path = parent.join(format!(".{cursor_name}.{}.tmp", Uuid::new_v4()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!(
        "Could not create a unique temporary feedback cursor beside {}",
        path.display()
    ))
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

fn latest_verdicts_by_verdict_key(
    records: &[ResolvedFeedbackRecord<'_>],
) -> HashMap<FeedbackVerdictKey, &'static str> {
    let mut latest = HashMap::<FeedbackVerdictKey, (i64, usize, &'static str)>::new();
    for resolved in records {
        let record = resolved.record;
        let should_replace =
            latest
                .get(&resolved.verdict_key)
                .is_none_or(|(timestamp, existing_index, _)| {
                    record.timestamp > *timestamp
                        || (record.timestamp == *timestamp && resolved.index > *existing_index)
                });
        if should_replace {
            latest.insert(
                resolved.verdict_key.clone(),
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
    let main_id = crate::vcs::mainline_commit(repo)?.id().detach().to_string();
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
                return Ok(ResolvedFeedbackContext {
                    snapshot: snapshot.clone(),
                    file_path: Some(file_path),
                    block: Some(resolved_feedback_block_for_record(record, block)),
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
            return Ok(ResolvedFeedbackContext {
                snapshot: snapshot.clone(),
                file_path: Some(file_path),
                block: Some(resolved_feedback_block_for_record(record, block)),
            });
        }
    }

    Ok(ResolvedFeedbackContext {
        snapshot: default_snapshot.clone(),
        file_path: record.path_hint.as_ref().map(RepoPath::to_string),
        block: None,
    })
}

fn scoped_feedback_block_for_record(record: &Record, block: &Block) -> Option<FeedbackBlockView> {
    let scope = record.comment_scope.as_ref()?;
    let context = record.comment_context.as_ref()?;
    let start_line = usize::try_from(scope.start_line).unwrap_or(usize::MAX);
    let end_line = usize::try_from(scope.end_line).unwrap_or(usize::MAX);
    if start_line >= end_line {
        return None;
    }

    Some(FeedbackBlockView {
        hash: block.hash.clone(),
        content: context.clone(),
        kind: block.kind,
        tags: block.tags.clone(),
        complexity: block.complexity,
        start_line,
        end_line,
        byte_span: None,
    })
}

fn resolved_feedback_block_for_record(record: &Record, unscoped: Block) -> ResolvedFeedbackBlock {
    match scoped_feedback_block_for_record(record, &unscoped) {
        Some(presentation) => ResolvedFeedbackBlock {
            unscoped,
            presentation,
            presentation_is_scoped: true,
        },
        None => ResolvedFeedbackBlock {
            presentation: FeedbackBlockView::from_canonical_block(&unscoped),
            unscoped,
            presentation_is_scoped: false,
        },
    }
}

fn scoped_unresolved_feedback_block_for_record(record: &Record) -> Option<FeedbackBlockView> {
    let scope = record.comment_scope.as_ref()?;
    let context = record.comment_context.as_ref()?;
    let hash = feedback_target_hash(record);
    let start_line = usize::try_from(scope.start_line).unwrap_or(usize::MAX);
    let end_line = usize::try_from(scope.end_line).unwrap_or(usize::MAX);
    if start_line >= end_line {
        return None;
    }

    Some(FeedbackBlockView {
        hash,
        content: context.clone(),
        kind: BlockKind::Code,
        tags: Vec::new(),
        complexity: None,
        start_line,
        end_line,
        byte_span: None,
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
        ReviewTargetRef::Tree { .. } => None,
        ReviewTargetRef::Declaration { .. } => None,
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

fn unresolved_unscoped_feedback_block_for_record(record: &Record) -> FeedbackBlockView {
    let (start_line, end_line) = unresolved_block_line_range(record);
    FeedbackBlockView {
        hash: feedback_target_hash(record),
        content: "[unresolved historical context]".to_string(),
        kind: BlockKind::Code,
        tags: Vec::new(),
        complexity: None,
        start_line,
        end_line,
        byte_span: None,
    }
}

fn unresolved_block_line_range(record: &Record) -> (usize, usize) {
    let start_line = record
        .line_hint
        .and_then(|line| usize::try_from(line).ok())
        .unwrap_or(0);
    (start_line, start_line.saturating_add(1))
}

fn feedback_target_hash(record: &Record) -> TreeHash {
    match &record.target {
        ReviewTargetRef::Block { hash }
        | ReviewTargetRef::File { hash }
        | ReviewTargetRef::Tree { hash } => hash.clone(),
        ReviewTargetRef::Declaration { .. } => {
            unreachable!("declaration records are filtered before ordinary feedback export")
        }
    }
}

fn should_skip_feedback_whitespace_only_by_default(
    block: &FeedbackBlockView,
    filters: &BlockFilters,
) -> bool {
    block.content.trim().is_empty() && !filters.only_contains(block.kind)
}

fn should_skip_feedback_imports_by_default(
    path: &str,
    block: &FeedbackBlockView,
    filters: &BlockFilters,
) -> bool {
    block.kind.is_import_like()
        && !path.ends_with("/lib.rs")
        && path != "lib.rs"
        && !filters.only_contains(block.kind)
}

fn record_matches_since(
    record: &Record,
    index: usize,
    since_filter: &FeedbackSinceFilter,
    validated_cursor: Option<&ValidatedFeedbackCursor>,
) -> bool {
    match since_filter {
        FeedbackSinceFilter::All => true,
        FeedbackSinceFilter::TimestampInclusive(timestamp) => record.timestamp >= *timestamp,
        FeedbackSinceFilter::Cursor(_) => validated_cursor.is_some_and(|cursor| {
            index >= cursor.checkpoint_len || cursor.pending_record_ids.contains(&record.id)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::LineSpan;
    use crate::hashing::TreeHash;
    use crate::store::{BlockState, Identity, ReviewCheck, VcsSystem, Verdict};
    use std::iter::FromIterator;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::thread;

    struct FakeResolver {
        contexts: HashMap<String, ResolvedFeedbackContext>,
    }

    impl FeedbackContextResolver for FakeResolver {
        fn resolve_context(&mut self, record: &Record) -> Result<ResolvedFeedbackContext> {
            let mut context = self
                .contexts
                .get(&record.id)
                .cloned()
                .ok_or_else(|| anyhow!("missing fake context for {}", record.id))?;
            if let Some(block) = context.block.as_ref() {
                context.block = Some(resolved_feedback_block_for_record(
                    record,
                    block.unscoped.clone(),
                ));
            }
            Ok(context)
        }
    }

    #[test]
    fn collect_feedback_entries_filters_by_record_revision_membership() {
        let keep = build_record("keep", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        let skip = build_record("skip", "bbbbbbb", "src/lib.rs", 20, Verdict::Comment);
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([(
                "keep".to_string(),
                resolved_context("src/lib.rs", "pub fn keep() {}\n"),
            )]),
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
    fn collect_feedback_entries_excludes_scoped_comment_after_later_full_block_approval() {
        let mut comment = build_record("comment", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        comment.comment_scope = Some(crate::store::CommentScope {
            start_line: 11,
            end_line: 12,
        });
        comment.comment_context = Some("    work();\n".to_string());
        let approval = build_record("approval", "aaaaaaa", "src/lib.rs", 20, Verdict::Approved);
        let canonical = resolved_context_with_span(
            "src/lib.rs",
            "fn core() {\n    work();\n}\n",
            LineSpan::new(10, 13),
        );
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                ("comment".to_string(), canonical.clone()),
                ("approval".to_string(), canonical),
            ]),
        };

        let entries = collect_feedback_entries(
            &[comment, approval],
            &FeedbackSinceFilter::All,
            &unapproved_feedback_query(),
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert!(entries.is_empty());
    }

    #[test]
    fn collect_feedback_entries_ignores_approvals_outside_allowed_revisions() {
        let comment = build_record("comment", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        let approval = build_record("approval", "bbbbbbb", "src/lib.rs", 20, Verdict::Approved);
        let context = resolved_context("src/lib.rs", "pub fn core() {}\n");
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                ("comment".to_string(), context.clone()),
                ("approval".to_string(), context),
            ]),
        };
        let query = FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: Some(HashSet::from_iter(["aaaaaaa".to_string()])),
            include_approved: false,
        };

        let entries = collect_feedback_entries(
            &[comment.clone(), approval],
            &FeedbackSinceFilter::All,
            &query,
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].reviews.len(), 1);
        assert_eq!(entries[0].reviews[0].id, comment.id);
    }

    #[test]
    fn collect_feedback_entries_keeps_distinct_same_scope_diff_comments_for_one_target() {
        let mut removed = build_record("removed", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        removed.comment_scope = Some(crate::store::CommentScope {
            start_line: 11,
            end_line: 12,
        });
        removed.comment_context = Some("-before\n".to_string());
        removed.comment_anchor = Some(crate::store::CommentAnchor::Diff(
            crate::store::DiffCommentAnchor {
                revision: crate::store::CommitId::new("aaaaaaa")
                    .unwrap_or_else(|error| panic!("valid test revision: {error}")),
                path: RepoPath::new("src/lib.rs")
                    .unwrap_or_else(|error| panic!("valid repo path: {error}")),
                rows: vec![crate::store::DiffCommentAnchorRow {
                    kind: crate::store::CommentAnchorDiffLineKind::Removed,
                    old_line: Some(11),
                    new_line: None,
                }],
            },
        ));
        let mut added = build_record("added", "aaaaaaa", "src/lib.rs", 20, Verdict::Comment);
        added.comment_scope = removed.comment_scope.clone();
        added.comment_context = Some("+after\n".to_string());
        added.comment_anchor = Some(crate::store::CommentAnchor::Diff(
            crate::store::DiffCommentAnchor {
                revision: crate::store::CommitId::new("aaaaaaa")
                    .unwrap_or_else(|error| panic!("valid test revision: {error}")),
                path: RepoPath::new("src/lib.rs")
                    .unwrap_or_else(|error| panic!("valid repo path: {error}")),
                rows: vec![crate::store::DiffCommentAnchorRow {
                    kind: crate::store::CommentAnchorDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(11),
                }],
            },
        ));
        let canonical = resolved_context_with_span(
            "src/lib.rs",
            "fn core() {\n    work();\n}\n",
            LineSpan::new(10, 13),
        );
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                ("removed".to_string(), canonical.clone()),
                ("added".to_string(), canonical),
            ]),
        };

        let mut entries = collect_feedback_entries(
            &[removed, added],
            &FeedbackSinceFilter::All,
            &unapproved_feedback_query(),
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));
        entries.sort_by(|left, right| left.block.content.cmp(&right.block.content));

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].block.content, "+after\n");
        assert_eq!(entries[0].reviews.len(), 1);
        assert_eq!(entries[0].reviews[0].id, "added");
        assert_eq!(entries[1].block.content, "-before\n");
        assert_eq!(entries[1].reviews.len(), 1);
        assert_eq!(entries[1].reviews[0].id, "removed");
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
    fn collect_feedback_entries_keeps_scoped_comment_when_same_hash_in_other_path_is_approved() {
        let shared_hash = TreeHash::from_content("fn duplicate() {}\n");
        let mut comment = build_record("comment", "aaaaaaa", "src/a.rs", 10, Verdict::Comment);
        comment.target = ReviewTargetRef::Block {
            hash: shared_hash.clone(),
        };
        comment.comment_scope = Some(crate::store::CommentScope {
            start_line: 1,
            end_line: 2,
        });
        comment.comment_context = Some("fn duplicate() {}\n".to_string());
        let mut approval = build_record("approval", "aaaaaaa", "src/b.rs", 20, Verdict::Approved);
        approval.target = ReviewTargetRef::Block { hash: shared_hash };
        let canonical = "fn duplicate() {}\n";
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "comment".to_string(),
                    resolved_context_with_span("src/a.rs", canonical, LineSpan::new(0, 2)),
                ),
                (
                    "approval".to_string(),
                    resolved_context_with_span("src/b.rs", canonical, LineSpan::new(0, 2)),
                ),
            ]),
        };

        let entries = collect_feedback_entries(
            &[comment, approval],
            &FeedbackSinceFilter::All,
            &unapproved_feedback_query(),
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_path, "src/a.rs");
        assert_eq!(entries[0].block.content, canonical);
        assert_eq!(entries[0].reviews.len(), 1);
        assert_eq!(entries[0].reviews[0].id, "comment");
    }

    #[test]
    fn collect_feedback_entries_excludes_paged_file_comment_after_later_file_approval() {
        let file_hash = TreeHash::from_content("file target");
        let mut comment = build_record("comment", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        comment.target = ReviewTargetRef::File {
            hash: file_hash.clone(),
        };
        comment.line_hint = Some(11);
        comment.comment_scope = Some(crate::store::CommentScope {
            start_line: 11,
            end_line: 12,
        });
        comment.comment_context = Some("fn second() {}\n".to_string());
        let mut approval = build_record("approval", "aaaaaaa", "src/lib.rs", 20, Verdict::Approved);
        approval.target = ReviewTargetRef::File { hash: file_hash };
        approval.line_hint = None;
        let mut resolver = FakeResolver {
            contexts: HashMap::from_iter([
                (
                    "comment".to_string(),
                    resolved_context_with_span(
                        "src/lib.rs",
                        "fn second() {}\n",
                        LineSpan::new(10, 12),
                    ),
                ),
                (
                    "approval".to_string(),
                    resolved_context_with_span(
                        "src/lib.rs",
                        "fn first() {}\n",
                        LineSpan::new(0, 2),
                    ),
                ),
            ]),
        };

        let entries = collect_feedback_entries(
            &[comment, approval],
            &FeedbackSinceFilter::All,
            &unapproved_feedback_query(),
            &mut resolver,
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));

        assert!(entries.is_empty());
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
                changed: Some(HashSet::from_iter([crate::vcs::ChangedPath::identity(
                    RepoPath::new("src/keep.rs").unwrap(),
                )])),
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
    fn feedback_cursor_keeps_filtered_lower_timestamp_gap() {
        let newer_selected = build_record(
            "newer-selected",
            "aaaaaaa",
            "src/a.rs",
            2_000,
            Verdict::Comment,
        );
        let older_filtered = build_record(
            "older-filtered",
            "aaaaaaa",
            "src/b.rs",
            1_000,
            Verdict::Comment,
        );
        let records = vec![newer_selected, older_filtered];
        let cursor = advance_feedback_cursor(
            None,
            &records,
            &HashSet::from_iter(["newer-selected".to_string()]),
        )
        .unwrap_or_else(|error| panic!("cursor advance should succeed: {error}"));

        assert_eq!(cursor.pending_record_ids, vec!["older-filtered"]);

        let entries = collect_feedback_entries(
            &records,
            &FeedbackSinceFilter::Cursor(cursor),
            &unfiltered_feedback_query(),
            &mut fake_resolver_for(&records),
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));
        assert_eq!(feedback_entry_ids(&entries), vec!["older-filtered"]);
    }

    #[test]
    fn feedback_cursor_keeps_same_second_ids_independent() {
        let first = build_record("first", "aaaaaaa", "src/a.rs", 1_000, Verdict::Comment);
        let second = build_record("second", "aaaaaaa", "src/b.rs", 1_000, Verdict::Comment);
        let records = vec![first, second];
        let cursor =
            advance_feedback_cursor(None, &records, &HashSet::from_iter(["first".to_string()]))
                .unwrap_or_else(|error| panic!("first cursor advance should succeed: {error}"));

        let entries = collect_feedback_entries(
            &records,
            &FeedbackSinceFilter::Cursor(cursor.clone()),
            &unfiltered_feedback_query(),
            &mut fake_resolver_for(&records),
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));
        assert_eq!(feedback_entry_ids(&entries), vec!["second"]);

        let drained = advance_feedback_cursor(
            Some(&cursor),
            &records,
            &HashSet::from_iter(["second".to_string()]),
        )
        .unwrap_or_else(|error| panic!("second cursor advance should succeed: {error}"));
        let entries = collect_feedback_entries(
            &records,
            &FeedbackSinceFilter::Cursor(drained),
            &unfiltered_feedback_query(),
            &mut fake_resolver_for(&records),
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));
        assert!(entries.is_empty());
    }

    #[test]
    fn feedback_cursor_keeps_backdated_appended_record() {
        let initial = build_record("initial", "aaaaaaa", "src/a.rs", 2_000, Verdict::Comment);
        let appended = build_record("appended", "aaaaaaa", "src/b.rs", 1_000, Verdict::Comment);
        let first_snapshot = vec![initial.clone()];
        let cursor = advance_feedback_cursor(
            None,
            &first_snapshot,
            &HashSet::from_iter(["initial".to_string()]),
        )
        .unwrap_or_else(|error| panic!("initial cursor advance should succeed: {error}"));
        let records = vec![initial, appended];

        let entries = collect_feedback_entries(
            &records,
            &FeedbackSinceFilter::Cursor(cursor),
            &unfiltered_feedback_query(),
            &mut fake_resolver_for(&records),
        )
        .unwrap_or_else(|error| panic!("collection should succeed: {error}"));
        assert_eq!(feedback_entry_ids(&entries), vec!["appended"]);
    }

    #[test]
    fn resolve_record_in_files_uses_line_hint_for_duplicate_block_hashes() {
        let first = Block::new(
            "fn duplicate() {}\n".to_string(),
            BlockKind::Function,
            LineSpan::new(1, 2),
            ByteSpan::new(0, "fn duplicate() {}\n".len()),
        );
        let second = Block::new(
            "fn duplicate() {}\n".to_string(),
            BlockKind::Function,
            LineSpan::new(10, 11),
            ByteSpan::new(100, 100 + "fn duplicate() {}\n".len()),
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
    fn resolve_record_in_files_does_not_bind_tree_target_to_arbitrary_child() {
        let block = Block::new(
            "fn child() {}\n".to_string(),
            BlockKind::Function,
            LineSpan::new(0, 1),
            ByteSpan::new(0, "fn child() {}\n".len()),
        );
        let file = FileState::from_text(
            RepoPath::new("src/child.rs").unwrap(),
            crate::analysis::Language::Rust,
            b"fn child() {}\n",
            vec![block],
        );
        let mut record = build_record("tree", "aaaaaaa", "src", 10, Verdict::Comment);
        record.target = ReviewTargetRef::Tree {
            hash: TreeHash::new("unrelated-tree"),
        };

        assert!(resolve_record_in_files(&record, &[file]).is_none());
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
    fn feedback_cursor_rejects_legacy_corrupt_and_unsupported_files() {
        let dir = trueflow_test_support::temp_test_dir("feedback_cursor_rejection");
        let path = dir.join("feedback.cursor");
        fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("failed to create dir: {error}"));

        for content in [
            "1234\n",
            "{\"timestamp\":1234,\"record_ids_at_timestamp\":[]}\n",
            "\n",
            "{\n",
            "{\"version\":2,\"checkpoint\":{\"record_count\":0,\"last_record_id\":null,\"record_ids_sha256\":\"x\"},\"pending_record_ids\":[]}\n",
            "{\"version\":1,\"checkpoint\":{\"record_count\":0,\"last_record_id\":null,\"record_ids_sha256\":\"x\"},\"pending_record_ids\":[],\"unexpected\":true}\n",
            "{\"version\":1,\"checkpoint\":{\"record_count\":1,\"last_record_id\":null,\"record_ids_sha256\":\"x\"},\"pending_record_ids\":[]}\n",
        ] {
            fs::write(&path, content)
                .unwrap_or_else(|error| panic!("failed to write cursor fixture: {error}"));
            assert!(
                read_feedback_cursor(&path).is_err(),
                "cursor should reject {content:?}"
            );
        }

        fs::remove_file(&path).unwrap_or_else(|error| panic!("failed to remove cursor: {error}"));
        assert!(
            read_feedback_cursor(&path)
                .unwrap_or_else(|error| panic!("missing cursor should be accepted: {error}"))
                .is_none()
        );
    }

    #[test]
    fn feedback_cursor_rejects_inconsistent_and_outside_pending_ids() {
        let record = build_record("known", "aaaaaaa", "src/a.rs", 1, Verdict::Comment);
        let records = vec![record];
        let mut inconsistent = feedback_cursor_for_records(&records);
        inconsistent.checkpoint.last_record_id = None;
        assert!(
            feedback_since_filter_for_cursor(Some(&inconsistent), &records).is_err(),
            "a non-empty checkpoint without a tail ID must fail"
        );

        let mut outside_pending = feedback_cursor_for_records(&records);
        outside_pending.pending_record_ids = vec!["unknown".to_string()];
        assert!(
            feedback_since_filter_for_cursor(Some(&outside_pending), &records).is_err(),
            "pending IDs must occur in the checkpoint prefix"
        );
    }

    #[test]
    fn feedback_cursor_rejects_unsorted_or_duplicate_pending_ids() {
        let records = vec![
            build_record("first", "aaaaaaa", "src/a.rs", 1, Verdict::Comment),
            build_record("second", "aaaaaaa", "src/b.rs", 2, Verdict::Comment),
        ];
        let mut unsorted = feedback_cursor_for_records(&records);
        unsorted.pending_record_ids.reverse();
        assert!(
            feedback_since_filter_for_cursor(Some(&unsorted), &records).is_err(),
            "pending IDs must have deterministic sorted serialization"
        );

        let mut duplicate = feedback_cursor_for_records(&records);
        duplicate.pending_record_ids = vec!["first".to_string(), "first".to_string()];
        assert!(
            feedback_since_filter_for_cursor(Some(&duplicate), &records).is_err(),
            "pending IDs must be unique"
        );
    }

    #[test]
    fn feedback_cursor_rejects_prefix_mismatch() {
        let original = build_record("original", "aaaaaaa", "src/a.rs", 1, Verdict::Comment);
        let replaced = build_record("replaced", "aaaaaaa", "src/a.rs", 1, Verdict::Comment);
        let cursor = feedback_cursor_for_records(&[original]);

        assert!(
            feedback_since_filter_for_cursor(Some(&cursor), &[replaced]).is_err(),
            "a cursor checkpoint must match the exact logical record prefix"
        );
    }

    #[test]
    fn feedback_cursor_no_exported_entries_and_final_drain() {
        let first = build_record("first", "aaaaaaa", "src/a.rs", 2, Verdict::Comment);
        let second = build_record("second", "aaaaaaa", "src/b.rs", 1, Verdict::Comment);
        let records = vec![first, second];
        let pending = advance_feedback_cursor(None, &records, &HashSet::new())
            .unwrap_or_else(|error| panic!("empty export should advance cursor: {error}"));
        assert_eq!(pending.pending_record_ids, vec!["first", "second"]);

        let partially_drained = advance_feedback_cursor(
            Some(&pending),
            &records,
            &HashSet::from_iter(["first".to_string()]),
        )
        .unwrap_or_else(|error| panic!("partial drain should succeed: {error}"));
        assert_eq!(partially_drained.pending_record_ids, vec!["second"]);

        let drained = advance_feedback_cursor(
            Some(&partially_drained),
            &records,
            &HashSet::from_iter(["second".to_string()]),
        )
        .unwrap_or_else(|error| panic!("final drain should succeed: {error}"));
        assert!(drained.pending_record_ids.is_empty());
    }

    #[test]
    fn feedback_cursor_pre_rename_failure_preserves_complete_old_bytes() {
        let dir = trueflow_test_support::temp_test_dir("feedback_cursor_pre_rename_failure");
        let path = dir.join("feedback.cursor");
        let old_records = vec![build_record(
            "old",
            "aaaaaaa",
            "src/a.rs",
            1,
            Verdict::Comment,
        )];
        let old = feedback_cursor_for_records(&old_records);
        write_feedback_cursor_atomically(&path, &old)
            .unwrap_or_else(|error| panic!("initial cursor write should succeed: {error}"));
        let old_bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read initial cursor bytes: {error}"));

        let new_records = vec![
            old_records[0].clone(),
            build_record("new", "aaaaaaa", "src/b.rs", 2, Verdict::Comment),
        ];
        let new = feedback_cursor_for_records(&new_records);
        let result = write_feedback_cursor_atomically_with(&path, &new, |_| {
            Err(anyhow!("injected pre-rename failure"))
        });

        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).unwrap_or_else(|error| panic!("failed to re-read cursor: {error}")),
            old_bytes
        );
        assert_eq!(
            read_feedback_cursor(&path)
                .unwrap_or_else(|error| panic!("old cursor must still parse: {error}")),
            Some(old)
        );
        let directory_entries = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("failed to inspect cursor directory: {error}"))
            .map(|entry| entry.unwrap_or_else(|error| panic!("failed to read entry: {error}")))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            directory_entries.iter().all(|name| !name.ends_with(".tmp")),
            "failed atomic writes must not leave a temporary cursor candidate"
        );
    }

    #[test]
    fn feedback_cursor_reader_writer_concurrency_observes_complete_states() {
        let dir = trueflow_test_support::temp_test_dir("feedback_cursor_reader_writer");
        let path = dir.join("feedback.cursor");
        let records = vec![
            build_record("first", "aaaaaaa", "src/a.rs", 1, Verdict::Comment),
            build_record("second", "aaaaaaa", "src/b.rs", 2, Verdict::Comment),
        ];
        FeedbackCursorUpdateGuard::acquire(&path)
            .unwrap_or_else(|error| panic!("initial writer lock should succeed: {error}"))
            .commit(&records, &HashSet::from_iter(["first".to_string()]))
            .unwrap_or_else(|error| panic!("initial commit should succeed: {error}"));
        let old = read_feedback_cursor(&path)
            .unwrap_or_else(|error| panic!("old cursor should parse: {error}"))
            .unwrap_or_else(|| panic!("old cursor should exist"));
        let new = advance_feedback_cursor(
            Some(&old),
            &records,
            &HashSet::from_iter(["second".to_string()]),
        )
        .unwrap_or_else(|error| panic!("expected new cursor should build: {error}"));

        let start = Arc::new(Barrier::new(2));
        let reads = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_start = Arc::clone(&start);
        let reader_reads = Arc::clone(&reads);
        let reader_done = Arc::clone(&done);
        let reader_old = old;
        let reader_new = new;
        let reader = thread::spawn(move || {
            reader_start.wait();
            while !reader_done.load(Ordering::Acquire) {
                let observed = read_feedback_cursor(&reader_path).unwrap_or_else(|error| {
                    panic!("reader must see complete cursor state: {error}")
                });
                assert!(
                    observed == Some(reader_old.clone()) || observed == Some(reader_new.clone())
                );
                reader_reads.fetch_add(1, Ordering::Release);
                thread::yield_now();
            }
        });

        start.wait();
        FeedbackCursorUpdateGuard::acquire(&path)
            .unwrap_or_else(|error| panic!("writer lock should succeed: {error}"))
            .commit(&records, &HashSet::from_iter(["second".to_string()]))
            .unwrap_or_else(|error| panic!("writer commit should succeed: {error}"));
        done.store(true, Ordering::Release);
        reader
            .join()
            .unwrap_or_else(|_| panic!("reader thread must not panic"));
        assert!(reads.load(Ordering::Acquire) > 0);
    }

    #[test]
    fn feedback_cursor_writer_monotonic_merge() {
        let dir = trueflow_test_support::temp_test_dir("feedback_cursor_monotonic_merge");
        let path = dir.join("feedback.cursor");
        let records = vec![
            build_record("first", "aaaaaaa", "src/a.rs", 2, Verdict::Comment),
            build_record("second", "aaaaaaa", "src/b.rs", 1, Verdict::Comment),
        ];

        FeedbackCursorUpdateGuard::acquire(&path)
            .unwrap_or_else(|error| panic!("first writer lock should succeed: {error}"))
            .commit(&records, &HashSet::from_iter(["first".to_string()]))
            .unwrap_or_else(|error| panic!("first writer commit should succeed: {error}"));
        FeedbackCursorUpdateGuard::acquire(&path)
            .unwrap_or_else(|error| panic!("second writer lock should succeed: {error}"))
            .commit(&records, &HashSet::from_iter(["second".to_string()]))
            .unwrap_or_else(|error| panic!("second writer commit should merge: {error}"));

        let cursor = read_feedback_cursor(&path)
            .unwrap_or_else(|error| panic!("merged cursor should parse: {error}"))
            .unwrap_or_else(|| panic!("merged cursor should exist"));
        assert_eq!(cursor.checkpoint.record_count, 2);
        assert!(cursor.pending_record_ids.is_empty());
    }

    fn feedback_cursor_for_records(records: &[Record]) -> FeedbackCursor {
        advance_feedback_cursor(None, records, &HashSet::new())
            .unwrap_or_else(|error| panic!("cursor fixture should build: {error}"))
    }

    fn fake_resolver_for(records: &[Record]) -> FakeResolver {
        let contexts = records
            .iter()
            .map(|record| {
                let path = record
                    .path_hint
                    .as_ref()
                    .unwrap_or_else(|| panic!("cursor fixture record needs a path"))
                    .to_string();
                (
                    record.id.clone(),
                    resolved_context(&path, "pub fn feedback() {}\n"),
                )
            })
            .collect();
        FakeResolver { contexts }
    }

    fn feedback_entry_ids(entries: &[FeedbackEntry]) -> Vec<String> {
        entries
            .iter()
            .flat_map(|entry| entry.reviews.iter().map(|record| record.id.clone()))
            .collect()
    }

    fn unfiltered_feedback_query() -> FeedbackQuery {
        FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: true,
        }
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
            block: Some(fully_presented_feedback_block(Block::new(
                content.to_string(),
                BlockKind::Code,
                LineSpan::new(0, 1),
                ByteSpan::new(0, content.len()),
            ))),
        }
    }

    fn resolved_context_with_span(
        path: &str,
        content: &str,
        line_span: LineSpan,
    ) -> ResolvedFeedbackContext {
        ResolvedFeedbackContext {
            snapshot: FeedbackSnapshot::Workdir,
            file_path: Some(path.to_string()),
            block: Some(fully_presented_feedback_block(Block::new(
                content.to_string(),
                BlockKind::Code,
                line_span,
                ByteSpan::new(0, content.len()),
            ))),
        }
    }

    fn fully_presented_feedback_block(unscoped: Block) -> ResolvedFeedbackBlock {
        ResolvedFeedbackBlock {
            presentation: FeedbackBlockView::from_canonical_block(&unscoped),
            unscoped,
            presentation_is_scoped: false,
        }
    }

    fn unapproved_feedback_query() -> FeedbackQuery {
        FeedbackQuery {
            filters: BlockFilters::default(),
            explicit_selection: None,
            changed_selection: None,
            allowed_revisions: None,
            include_approved: false,
        }
    }

    #[test]
    fn canonical_feedback_view_serializes_its_source_span() {
        let content = "fn canonical() {}\n";
        let block = Block::new(
            content.to_string(),
            BlockKind::Function,
            LineSpan::new(2, 3),
            ByteSpan::new(17, 17 + content.len()),
        );

        let value = serde_json::to_value(FeedbackBlockView::from_canonical_block(&block))
            .unwrap_or_else(|error| panic!("canonical feedback view should serialize: {error}"));

        assert_eq!(value["start_byte"].as_u64(), Some(17));
        assert_eq!(
            value["end_byte"].as_u64(),
            u64::try_from(17 + content.len()).ok()
        );
    }

    #[test]
    fn scoped_and_unresolved_feedback_views_omit_source_spans() {
        let content = "fn canonical() {}\n";
        let block = Block::new(
            content.to_string(),
            BlockKind::Function,
            LineSpan::new(2, 3),
            ByteSpan::new(17, 17 + content.len()),
        );
        let mut record = build_record("scoped", "aaaaaaa", "src/lib.rs", 10, Verdict::Comment);
        record.comment_scope = Some(crate::store::CommentScope {
            start_line: 7,
            end_line: 9,
        });
        record.comment_context = Some("rewritten feedback context\n".to_string());

        let scoped = scoped_feedback_block_for_record(&record, &block)
            .unwrap_or_else(|| panic!("expected scoped feedback view"));
        let unresolved = scoped_unresolved_feedback_block_for_record(&record)
            .unwrap_or_else(|| panic!("expected unresolved feedback view"));

        for view in [scoped, unresolved] {
            assert_eq!(view.byte_span, None);
            let value = serde_json::to_value(view)
                .unwrap_or_else(|error| panic!("detached feedback view should serialize: {error}"));
            assert!(value.get("start_byte").is_none());
            assert!(value.get("end_byte").is_none());
        }
    }
}
