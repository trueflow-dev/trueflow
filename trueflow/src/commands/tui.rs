use super::tui_speedread::SpeedReadController;
#[cfg(test)]
use super::tui_terminal::{
    TerminalCapabilities, enter_tui_mode, leave_tui_mode, tui_keyboard_enhancement_flags,
};
use super::tui_terminal::{TerminalSession, TuiTerminal};
use crate::ai::{
    AiAvailability, AiEnvironment, AiProvider, AiReviewContext, AiReviewSetContext, AiSuggestion,
    AiSuggestionKey, AiSuggestionProvider, AiSuggestionRequest, CommandAiSuggestionProvider,
    DEFAULT_AI_RESPONSE_CHAR_LIMIT, resolve_ai_availability,
};
use crate::analysis::Language;
use crate::block::BlockKind;
use crate::commands::mark;
use crate::commands::review::{
    BlockChangeKind, CollectedReview, DiffBlockSides, FileChangeKind, ReviewRequest, ReviewSummary,
    ReviewTarget, RevisionExpr, collect_review, expand_cli_review_targets, resolve_review_request,
    review_request_from_cli_targets,
};
use crate::config::{
    BatchConfirmPolicy, BlockFilters, TrueflowConfig, TuiConfig, TuiDiffFocusMode,
    TuiDiffLineNumbers, TuiKeybindsConfig, TuiSpeedReadConfig, load as load_config,
};
use crate::context::TrueflowContext;
use crate::github::{
    GhGitHubClient, PullRequestCommit, PullRequestMetadata, PullRequestRef,
    prepare_pull_request_review,
};
use crate::hashing::{TreeHash, hash_str};
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::review_metadata;
use crate::review_navigator::ReviewNavigator;
use crate::review_order::{ReviewAnchor, ReviewOrder};
use crate::review_scope::{DiffQuery, ScopeOption, ScopePreset, default_scope_options};
use crate::review_session;
use crate::review_speedread::PlaybackState;
use crate::store::{
    CommentAnchor, CommentAnchorDiffLineKind, DiffCommentAnchor, DiffCommentAnchorRow, FileStore,
    ReviewCheck, ReviewTargetKind, SourceCommentAnchor, Verdict,
};
use crate::sub_splitter;
use crate::targets::{extract_pull_request_target, workdir_prefix_from_git_root};
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
use crate::vcs;
use anyhow::{Context, Result, anyhow};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as UiBlock, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[cfg(any(test, feature = "tui-test-support"))]
#[doc(hidden)]
pub mod test_support;

const REVIEW_COVERAGE_STATUS_CACHE_FILE: &str = "cache/review_coverage_status.json";
const REVIEW_COVERAGE_STATUS_CACHE_FORMAT_VERSION: u32 = 1;
const REVIEW_COVERAGE_STATUS_CACHE_MAX_ENTRIES: usize = 128;
const SCOPE_SELECTOR_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SCOPE_SELECTOR_STATUS_WORKER_COUNT: usize = 1;
const SCOPE_SELECTOR_STATUS_WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

// --- Core Structs ---

#[derive(Debug, Clone)]
struct ScopeSelector {
    options: Vec<ScopeSelectorOption>,
    selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeSelectorOption {
    label: String,
    scope: ScopePreset,
    status: ScopeSelectorStatus,
}

impl ScopeSelectorOption {
    fn from_scope_option(option: ScopeOption, status: ScopeSelectorStatus) -> Self {
        Self {
            label: option.label,
            scope: option.scope,
            status,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ScopeSelectorStatus {
    Checking,
    Deferred,
    Pending {
        remaining_blocks: usize,
        total_blocks: usize,
    },
    Reviewed {
        total_blocks: usize,
    },
    Empty,
    Unavailable,
}

impl ScopeSelectorStatus {
    fn from_summary(summary: &ReviewSummary) -> Self {
        let remaining_blocks = summary
            .files
            .iter()
            .map(|file| file.blocks.len())
            .sum::<usize>();
        match (summary.total_blocks, remaining_blocks) {
            (0, _) => Self::Empty,
            (_, 0) => Self::Reviewed {
                total_blocks: summary.total_blocks,
            },
            (total_blocks, remaining_blocks) => Self::Pending {
                remaining_blocks,
                total_blocks,
            },
        }
    }

    fn is_cacheable(self) -> bool {
        matches!(
            self,
            Self::Pending { .. } | Self::Reviewed { .. } | Self::Empty
        )
    }

    fn label(self) -> String {
        match self {
            Self::Checking => "[checking...]".to_string(),
            Self::Deferred => "[select to scan]".to_string(),
            Self::Pending {
                remaining_blocks,
                total_blocks,
            } => format!("[{remaining_blocks}/{total_blocks} left]"),
            Self::Reviewed { .. } => "[reviewed]".to_string(),
            Self::Empty => "[no items]".to_string(),
            Self::Unavailable => "[unavailable]".to_string(),
        }
    }
}

#[derive(Debug)]
struct LoadedScopeSelector {
    selector: ScopeSelector,
    status_poller: Option<ScopeSelectorStatusPoller>,
}

#[derive(Debug, Clone)]
struct ScopeSelectorStatusJob {
    index: usize,
    scope: ScopePreset,
    cache_key: String,
}

#[derive(Debug, Clone)]
struct ScopeSelectorStatusUpdate {
    index: usize,
    status: ScopeSelectorStatus,
    cache_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
struct ReviewDatabaseFingerprint {
    size_bytes: u64,
    modified_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReviewCoverageStatusCacheFile {
    format_version: u32,
    review_db_fingerprint: ReviewDatabaseFingerprint,
    entries: HashMap<String, ScopeSelectorStatus>,
    #[serde(default)]
    entry_updated_unix_ms: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
struct ReviewCoverageStatusCacheStore {
    path: PathBuf,
    fingerprint: ReviewDatabaseFingerprint,
    entries: HashMap<String, ScopeSelectorStatus>,
    entry_updated_unix_ms: HashMap<String, u64>,
    fresh: bool,
    dirty: bool,
}

impl ReviewCoverageStatusCacheStore {
    fn load() -> Option<Self> {
        let store = FileStore::new().ok()?;
        let path = store.trueflow_dir().join(REVIEW_COVERAGE_STATUS_CACHE_FILE);
        let fingerprint = review_database_fingerprint(&store.db_path());
        let cache = read_review_coverage_status_cache_file(&path);
        let fresh = cache
            .as_ref()
            .is_some_and(|cache| cache.review_db_fingerprint == fingerprint);
        let mut store = Self {
            path,
            fingerprint,
            entries: cache
                .as_ref()
                .map(|cache| cache.entries.clone())
                .unwrap_or_default(),
            entry_updated_unix_ms: cache
                .map(|cache| cache.entry_updated_unix_ms)
                .unwrap_or_default(),
            fresh,
            dirty: false,
        };
        if store.prune_to_bound() {
            store.dirty = true;
            store.flush();
        }
        Some(store)
    }

    fn cached_status(&self, cache_key: &str) -> Option<ScopeSelectorStatus> {
        self.entries
            .get(cache_key)
            .copied()
            .filter(|status| status.is_cacheable())
    }

    fn record(&mut self, cache_key: &str, status: ScopeSelectorStatus) {
        if !status.is_cacheable() {
            return;
        }

        let now = current_unix_ms();
        let previous = self.entries.insert(cache_key.to_string(), status);
        self.entry_updated_unix_ms
            .insert(cache_key.to_string(), now);
        if previous != Some(status) || !self.fresh {
            self.dirty = true;
        }
        self.fresh = true;
    }

    fn prune_to_bound(&mut self) -> bool {
        self.entry_updated_unix_ms
            .retain(|key, _| self.entries.contains_key(key));
        if self.entries.len() <= REVIEW_COVERAGE_STATUS_CACHE_MAX_ENTRIES {
            return false;
        }

        let mut ordered_keys = self.entries.keys().cloned().collect::<Vec<_>>();
        ordered_keys.sort_by(|left, right| {
            self.entry_updated_unix_ms
                .get(left)
                .copied()
                .unwrap_or_default()
                .cmp(
                    &self
                        .entry_updated_unix_ms
                        .get(right)
                        .copied()
                        .unwrap_or_default(),
                )
                .then_with(|| left.cmp(right))
        });

        let remove_count = self
            .entries
            .len()
            .saturating_sub(REVIEW_COVERAGE_STATUS_CACHE_MAX_ENTRIES);
        for key in ordered_keys.into_iter().take(remove_count) {
            self.entries.remove(&key);
            self.entry_updated_unix_ms.remove(&key);
        }
        true
    }

    fn flush(&mut self) {
        if !self.dirty {
            return;
        }

        if let Some(parent) = self.path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return;
        }

        self.prune_to_bound();
        let cache = ReviewCoverageStatusCacheFile {
            format_version: REVIEW_COVERAGE_STATUS_CACHE_FORMAT_VERSION,
            review_db_fingerprint: self.fingerprint,
            entries: self.entries.clone(),
            entry_updated_unix_ms: self.entry_updated_unix_ms.clone(),
        };
        let Ok(contents) = serde_json::to_string_pretty(&cache) else {
            return;
        };
        if fs::write(&self.path, format!("{contents}\n")).is_ok() {
            self.dirty = false;
        }
    }
}

#[derive(Debug)]
struct ScopeSelectorStatusPoller {
    receiver: mpsc::Receiver<ScopeSelectorStatusUpdate>,
    pending_jobs: usize,
    cache: Option<ReviewCoverageStatusCacheStore>,
    cancelled: Arc<AtomicBool>,
}

impl ScopeSelectorStatusPoller {
    fn spawn(
        jobs: Vec<ScopeSelectorStatusJob>,
        filters: &BlockFilters,
        scan_options: &crate::scanner::ScanOptions,
        cache: Option<ReviewCoverageStatusCacheStore>,
    ) -> Option<Self> {
        let filters = Arc::new(filters.clone());
        let scan_options = Arc::new(scan_options.clone());
        Self::spawn_with_loader(jobs, cache, move |scope| {
            match load_review_summary(&scope, &filters, &scan_options) {
                Ok(summary) => ScopeSelectorStatus::from_summary(&summary),
                Err(_) => ScopeSelectorStatus::Unavailable,
            }
        })
    }

    fn spawn_with_loader<L>(
        jobs: Vec<ScopeSelectorStatusJob>,
        cache: Option<ReviewCoverageStatusCacheStore>,
        load_status: L,
    ) -> Option<Self>
    where
        L: Fn(ScopePreset) -> ScopeSelectorStatus + Send + Sync + 'static,
    {
        if jobs.is_empty() {
            return None;
        }

        let pending_jobs = jobs.len();
        let jobs = Arc::new(Mutex::new(VecDeque::from(jobs)));
        let load_status = Arc::new(load_status);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let worker_count = pending_jobs.min(SCOPE_SELECTOR_STATUS_WORKER_COUNT);
        for worker_index in 0..worker_count {
            let sender = sender.clone();
            let jobs = Arc::clone(&jobs);
            let load_status = Arc::clone(&load_status);
            let worker_cancelled = Arc::clone(&cancelled);
            let spawn_result = thread::Builder::new()
                .name(format!("trueflow-scope-selector-status-{worker_index}"))
                .stack_size(SCOPE_SELECTOR_STATUS_WORKER_STACK_BYTES)
                .spawn(move || {
                    loop {
                        if worker_cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let job = {
                            let Ok(mut jobs) = jobs.lock() else {
                                break;
                            };
                            jobs.pop_front()
                        };
                        let Some(job) = job else {
                            break;
                        };
                        if worker_cancelled.load(Ordering::Relaxed) {
                            break;
                        }

                        let status = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            load_status(job.scope.clone())
                        }))
                        .unwrap_or(ScopeSelectorStatus::Unavailable);
                        if worker_cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        if sender
                            .send(ScopeSelectorStatusUpdate {
                                index: job.index,
                                status,
                                cache_key: job.cache_key,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            if spawn_result.is_err() {
                cancelled.store(true, Ordering::Relaxed);
                return None;
            }
        }
        drop(sender);

        Some(Self {
            receiver,
            pending_jobs,
            cache,
            cancelled,
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn has_pending_jobs(&self) -> bool {
        self.pending_jobs > 0
    }

    fn apply_update(&mut self, selector: &mut ScopeSelector, update: &ScopeSelectorStatusUpdate) {
        self.pending_jobs = self.pending_jobs.saturating_sub(1);
        if let Some(option) = selector.options.get_mut(update.index) {
            option.status = update.status;
        }
        if let Some(cache) = self.cache.as_mut() {
            cache.record(&update.cache_key, update.status);
        }
    }

    fn flush_cache_after_update(&mut self, updated: bool) {
        if updated && let Some(cache) = self.cache.as_mut() {
            cache.flush();
        }
    }

    fn drain_updates(&mut self, selector: &mut ScopeSelector) -> bool {
        let mut updated = false;
        loop {
            match self.receiver.try_recv() {
                Ok(update) => {
                    self.apply_update(selector, &update);
                    updated = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_jobs = 0;
                    for option in &mut selector.options {
                        if option.status == ScopeSelectorStatus::Checking {
                            option.status = ScopeSelectorStatus::Unavailable;
                            updated = true;
                        }
                    }
                    break;
                }
            }
        }

        self.flush_cache_after_update(updated);
        updated
    }

    #[cfg(test)]
    fn wait_for_update(&mut self, selector: &mut ScopeSelector, timeout: Duration) -> bool {
        let updated = match self.receiver.recv_timeout(timeout) {
            Ok(update) => {
                self.apply_update(selector, &update);
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.pending_jobs = 0;
                let mut changed = false;
                for option in &mut selector.options {
                    if option.status == ScopeSelectorStatus::Checking {
                        option.status = ScopeSelectorStatus::Unavailable;
                        changed = true;
                    }
                }
                changed
            }
        };
        let drained = self.drain_updates(selector);
        let updated = updated || drained;
        self.flush_cache_after_update(updated);
        updated
    }
}

impl Drop for ScopeSelectorStatusPoller {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
fn load_review_coverage_status_cache_file(
    path: &Path,
    fingerprint: ReviewDatabaseFingerprint,
) -> Option<ReviewCoverageStatusCacheFile> {
    let cache = read_review_coverage_status_cache_file(path)?;
    if cache.review_db_fingerprint != fingerprint {
        return None;
    }
    Some(cache)
}

fn read_review_coverage_status_cache_file(path: &Path) -> Option<ReviewCoverageStatusCacheFile> {
    let contents = fs::read_to_string(path).ok()?;
    let cache = serde_json::from_str::<ReviewCoverageStatusCacheFile>(&contents).ok()?;
    if cache.format_version != REVIEW_COVERAGE_STATUS_CACHE_FORMAT_VERSION {
        return None;
    }
    Some(cache)
}

fn review_database_fingerprint(path: &Path) -> ReviewDatabaseFingerprint {
    let Ok(metadata) = fs::metadata(path) else {
        return ReviewDatabaseFingerprint::default();
    };
    ReviewDatabaseFingerprint {
        size_bytes: metadata.len(),
        modified_unix_ms: metadata
            .modified()
            .ok()
            .and_then(system_time_to_unix_ms)
            .unwrap_or_default(),
    }
}

fn system_time_to_unix_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn current_unix_ms() -> u64 {
    system_time_to_unix_ms(SystemTime::now()).unwrap_or_default()
}

fn review_coverage_status_cache_key(
    scope: &ScopePreset,
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
    workdir_prefix: Option<&str>,
) -> String {
    let scope_key = match scope {
        ScopePreset::All => "all".to_string(),
        ScopePreset::MainDiff => "main-diff".to_string(),
        ScopePreset::Commit { id, .. } => format!("commit:{id}"),
        ScopePreset::RevisionRange { start, end } => format!("revision-range:{start}..{end}"),
    };
    let fingerprint = hash_str(&format!(
        "filters={filters:?}|scan_options={scan_options:?}|workdir_prefix={}",
        workdir_prefix.unwrap_or_default()
    ));
    format!("{scope_key}:{fingerprint}")
}

fn scope_selector_status_should_refresh_when_cached(scope: &ScopePreset) -> bool {
    matches!(
        scope,
        ScopePreset::MainDiff | ScopePreset::RevisionRange { .. }
    )
}

fn build_scope_selector_with_status_jobs(
    options: Vec<ScopeOption>,
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
    workdir_prefix: Option<&str>,
    cache: Option<&ReviewCoverageStatusCacheStore>,
) -> (ScopeSelector, Vec<ScopeSelectorStatusJob>) {
    let mut selector_options = Vec::with_capacity(options.len());
    let mut jobs = Vec::new();

    for (index, option) in options.into_iter().enumerate() {
        if matches!(option.scope, ScopePreset::All) {
            selector_options.push(ScopeSelectorOption::from_scope_option(
                option,
                ScopeSelectorStatus::Deferred,
            ));
            continue;
        }

        let cache_key =
            review_coverage_status_cache_key(&option.scope, filters, scan_options, workdir_prefix);
        let cached_status = cache.and_then(|cache| cache.cached_status(&cache_key));
        let should_refresh = cached_status.is_none()
            || cache.is_none_or(|cache| !cache.fresh)
            || scope_selector_status_should_refresh_when_cached(&option.scope);
        let status = cached_status.unwrap_or(ScopeSelectorStatus::Checking);
        if should_refresh {
            jobs.push(ScopeSelectorStatusJob {
                index,
                scope: option.scope.clone(),
                cache_key,
            });
        }
        selector_options.push(ScopeSelectorOption::from_scope_option(option, status));
    }
    jobs.sort_by_key(|job| (scope_selector_status_job_priority(&job.scope), job.index));

    (ScopeSelector::new(selector_options), jobs)
}

fn scope_selector_status_job_priority(scope: &ScopePreset) -> u8 {
    match scope {
        ScopePreset::MainDiff | ScopePreset::Commit { .. } | ScopePreset::RevisionRange { .. } => 0,
        ScopePreset::All => 1,
    }
}

impl ScopeSelector {
    fn new(options: Vec<ScopeSelectorOption>) -> Self {
        let selected = options
            .iter()
            .position(|option| matches!(option.scope, ScopePreset::MainDiff))
            .unwrap_or(0);
        Self { options, selected }
    }

    fn move_next(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.options.len() - 1);
    }

    fn move_prev(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    fn selected_scope(&self) -> Option<ScopePreset> {
        self.options
            .get(self.selected)
            .map(|option| option.scope.clone())
    }
}

enum ScopeSelection {
    Quit,
    Selected(ScopePreset),
}

enum AppExit {
    Quit,
    ReviewSomethingElse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecapAction {
    Exit,
    ReviewSomethingElse,
}

struct LaunchSelection {
    scope: ScopePreset,
    review: CollectedReview,
    scope_label: String,
    initial_view_mode: ViewMode,
}

#[derive(Debug, Clone)]
struct CliReviewRequest {
    request: ReviewRequest,
    scope: ScopePreset,
    scope_label: String,
    initial_view_mode: ViewMode,
}

// --- Application Logic ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    Single {
        node_id: TreeNodeId,
        verdict: Verdict,
        note: Option<String>,
    },
    Batch {
        node_id: TreeNodeId,
        verdict: Verdict,
        note: Option<String>,
    },
}

impl PendingAction {
    fn from_node(tree: &Tree, id: TreeNodeId, verdict: Verdict) -> Self {
        match tree.node(id).kind {
            TreeNodeKind::Block => Self::Single {
                node_id: id,
                verdict,
                note: None,
            },
            _ => Self::Batch {
                node_id: id,
                verdict,
                note: None,
            },
        }
    }

    fn with_note(&self, note: String) -> Self {
        match self {
            PendingAction::Single {
                node_id, verdict, ..
            } => PendingAction::Single {
                node_id: *node_id,
                verdict: verdict.clone(),
                note: Some(note),
            },
            PendingAction::Batch {
                node_id, verdict, ..
            } => PendingAction::Batch {
                node_id: *node_id,
                verdict: verdict.clone(),
                note: Some(note),
            },
        }
    }

    fn verdict_label(&self) -> &'static str {
        match self {
            PendingAction::Single { verdict, .. } | PendingAction::Batch { verdict, .. } => {
                match verdict {
                    Verdict::Comment => "note",
                    _ => verdict.as_str(),
                }
            }
        }
    }
}

#[derive(PartialEq, Default)]
enum InputMode {
    #[default]
    Normal,
    Editing {
        action: PendingAction,
    },
    ConfirmBatch {
        action: PendingAction,
        count: usize,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct InputCursor {
    offset: usize,
    goal_column: Option<usize>,
}

impl InputCursor {
    fn clamped_to_buffer(self, content: &str) -> Self {
        Self {
            offset: clamp_cursor_offset_to_char_boundary(content, self.offset),
            goal_column: self.goal_column,
        }
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.goal_column = None;
    }

    fn clear_goal_column(&mut self) {
        self.goal_column = None;
    }
}

struct AppState {
    review_scope: ScopePreset,
    navigator: ReviewNavigator,
    review_order: ReviewOrder,
    total_blocks: usize,
    initial_remaining_blocks: usize,
    remaining_blocks: usize,
    reviewable_nodes: HashSet<TreeNodeId>,
    commented_nodes: HashSet<TreeNodeId>,
    skipped_nodes: HashSet<TreeNodeId>,
    diff_block_sides: HashMap<TreeNodeId, DiffBlockSides>,
    file_change_kinds: HashMap<TreeNodeId, FileChangeKind>,
    block_change_kinds: HashMap<TreeNodeId, BlockChangeKind>,
    session_recap: SessionRecap,
    scope_label: String,
    input_mode: InputMode,
    input_buffer: String,
    input_cursor: InputCursor,
    input_draft: Option<String>,
    editing_validation: Option<EditingValidation>,
    confirm_batch: BatchConfirmPolicy,
    repo_name: String,
    repo_root: Option<PathBuf>,
    file_cache: HashMap<PathBuf, Arc<[String]>>,
    root_cursor: Option<TreeNodeId>,
    focus_block: Option<TreeNodeId>,
    pending_focus_scroll: bool,
    scroll_offset: u16,
    content_height: u16,
    viewport_height: u16,
    code_rect: Rect,
    visible_comment_capture: Option<VisibleCommentCapture>,
    view_mode: ViewMode,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    diff_line_numbers: TuiDiffLineNumbers,
    keybinds: TuiKeybindsConfig,
    file_diff_cache: HashMap<PathBuf, vcs::FileDiff>,
    content_frame_cache: HashMap<ContentFrameCacheKey, ContentFrameCacheEntry>,
    highlighted_line_cache: HashMap<HighlightLineCacheKey, Vec<HighlightToken>>,
    speed_read: SpeedReadController,
    ai: TuiAiState,
}

const MOUSE_WHEEL_SCROLL_LINES: u16 = 3;
const DISPLAY_TAB_WIDTH: usize = 8;

struct ReviewStateBuildOptions {
    confirm_batch: BatchConfirmPolicy,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    diff_line_numbers: TuiDiffLineNumbers,
    keybinds: TuiKeybindsConfig,
    scope_label: String,
    initial_view_mode: ViewMode,
    speed_read_config: TuiSpeedReadConfig,
    speed_read_config_path: PathBuf,
    ai: TuiAiState,
}

struct TuiAiState {
    availability: Option<AiAvailability>,
    review_set: Option<AiReviewSetContext>,
    max_context_lines: usize,
    cache_enabled: bool,
    provider: Option<Arc<dyn AiSuggestionProvider>>,
    cache: HashMap<AiSuggestionKey, AiSuggestion>,
    pending: Option<PendingAiSuggestion>,
    status: TuiAiStatus,
}

struct PendingAiSuggestion {
    key: AiSuggestionKey,
    receiver: mpsc::Receiver<AiSuggestionWorkerResult>,
    next_frame_at: Instant,
}

impl PendingAiSuggestion {
    fn new(
        key: AiSuggestionKey,
        receiver: mpsc::Receiver<AiSuggestionWorkerResult>,
        now: Instant,
    ) -> Self {
        Self {
            key,
            receiver,
            next_frame_at: now + AI_LOADING_FRAME_INTERVAL,
        }
    }
}

struct AiSuggestionWorkerResult {
    key: AiSuggestionKey,
    result: std::result::Result<AiSuggestion, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiAiStatus {
    Availability,
    Loading {
        key: AiSuggestionKey,
        frame: usize,
    },
    Suggestion {
        key: AiSuggestionKey,
        suggestion: AiSuggestion,
    },
    Error {
        key: AiSuggestionKey,
        message: String,
    },
}

impl TuiAiState {
    #[cfg(any(test, feature = "tui-test-support"))]
    fn empty() -> Self {
        Self {
            availability: None,
            review_set: None,
            max_context_lines: 80,
            cache_enabled: true,
            provider: None,
            cache: HashMap::new(),
            pending: None,
            status: TuiAiStatus::Availability,
        }
    }

    fn from_availability(
        availability: AiAvailability,
        max_context_lines: usize,
        cache_enabled: bool,
    ) -> Self {
        Self {
            availability: Some(availability),
            review_set: None,
            max_context_lines,
            cache_enabled,
            provider: None,
            cache: HashMap::new(),
            pending: None,
            status: TuiAiStatus::Availability,
        }
    }

    fn hint_line_text(&self) -> Option<String> {
        match &self.status {
            TuiAiStatus::Availability => self.availability.as_ref().map(|availability| {
                if self.provider.is_none()
                    && let AiAvailability::Ready { provider, .. } = availability
                    && !matches!(provider, AiProvider::ClaudeCli | AiProvider::CodexCli)
                {
                    return format!(
                        "Suggestion unavailable ({} direct API suggestions not implemented; set provider = \"claude_cli\" or \"codex_cli\")",
                        provider.label()
                    );
                }
                availability.modeline_text()
            }),
            TuiAiStatus::Loading { frame, .. } => Some(ai_loading_hint_text(*frame).to_string()),
            TuiAiStatus::Suggestion { suggestion, .. } => {
                suggestion.visible_sentence().map(str::to_string)
            }
            TuiAiStatus::Error { message, .. } => Some(format!("Suggestion error ({message})")),
        }
    }

    fn current_suggestion_sentence(&self) -> Option<&str> {
        let TuiAiStatus::Suggestion { suggestion, .. } = &self.status else {
            return None;
        };
        suggestion.proposed_change_sentence()
    }

    fn ai_poll_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.next_frame_at)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeybindAction {
    Up,
    Down,
    Prev,
    Next,
    Parent,
    Child,
    Approve,
    Note,
    ToggleView,
    SpeedRead,
    Root,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditingValidation {
    NoteRequired,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SessionRecap {
    approved_blocks: usize,
    rejected_blocks: usize,
    comments: usize,
    blocks_touched: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ActionImpact {
    affected_blocks: usize,
    removed_reviewable: usize,
}

impl SessionRecap {
    fn has_activity(self) -> bool {
        self.approved_blocks > 0 || self.rejected_blocks > 0 || self.comments > 0
    }

    fn record_action(&mut self, verdict: &Verdict, impact: ActionImpact) {
        self.blocks_touched = self.blocks_touched.saturating_add(impact.affected_blocks);
        match verdict {
            Verdict::Approved => {
                self.approved_blocks = self
                    .approved_blocks
                    .saturating_add(impact.removed_reviewable);
            }
            Verdict::Rejected => {
                self.rejected_blocks = self
                    .rejected_blocks
                    .saturating_add(impact.removed_reviewable);
            }
            Verdict::Comment => {
                self.comments = self.comments.saturating_add(1);
            }
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ViewMode {
    Source,
    #[default]
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UiMode {
    Navigation,
    DiffReview,
    SourceReview,
    SpeedRead,
    Recap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderChangeKind {
    File(FileChangeKind),
    Block(BlockChangeKind),
    Unknown,
}

impl HeaderChangeKind {
    fn label(self) -> &'static str {
        match self {
            HeaderChangeKind::File(FileChangeKind::Added) => "File Added",
            HeaderChangeKind::File(FileChangeKind::Deleted) => "File Deleted",
            HeaderChangeKind::File(FileChangeKind::Changed) => "File Changed",
            HeaderChangeKind::Block(BlockChangeKind::Added) => "Block Added",
            HeaderChangeKind::Block(BlockChangeKind::Deleted) => "Block Deleted",
            HeaderChangeKind::Block(BlockChangeKind::Changed) => "Block Changed",
            HeaderChangeKind::Unknown => "Unknown Change",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ContentFrameCacheKey {
    node_id: TreeNodeId,
    focus_block: Option<TreeNodeId>,
    variant: ContentFrameCacheVariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ContentFrameCacheVariant {
    FileDiff {
        code_width: u16,
    },
    FileSource,
    BlockDiff {
        focus_mode: vcs::BlockDiffFocusMode,
        code_width: u16,
    },
    BlockSource {
        code_height: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommentAnchorRowCapture {
    SourceLine { line_index: usize },
    DiffRow { row: DiffCommentAnchorRow },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommentContextRow {
    scope_line_index: usize,
    text: String,
    display_row_range: std::ops::Range<usize>,
    anchor: CommentAnchorRowCapture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisibleCommentCapture {
    scope: crate::store::CommentScope,
    context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommentAnchorSelection {
    Source { start_line: u32, end_line: u32 },
    Diff { rows: Vec<DiffCommentAnchorRow> },
}

#[derive(Clone)]
struct ContentFrameCacheEntry {
    lines: Vec<Line<'static>>,
    total_lines: usize,
    focus_row_range: Option<std::ops::Range<usize>>,
    comment_rows: Option<Vec<CommentContextRow>>,
}

#[derive(Clone)]
struct BuiltContent {
    lines: Vec<Line<'static>>,
    total_lines: usize,
    focus_row_range: Option<std::ops::Range<usize>>,
    comment_rows: Option<Vec<CommentContextRow>>,
}

#[derive(Clone)]
struct ContentNodeSnapshot {
    id: TreeNodeId,
    kind: TreeNodeKind,
    path: RepoPath,
    children: Vec<TreeNodeId>,
    block: Option<crate::block::Block>,
    language: Option<Language>,
}

impl ContentNodeSnapshot {
    fn from_node(node: &crate::tree::TreeNode) -> Self {
        let children = if matches!(node.kind, TreeNodeKind::Directory) {
            node.children.clone()
        } else {
            Vec::new()
        };
        let block = if matches!(node.kind, TreeNodeKind::Block) {
            node.block.clone()
        } else {
            None
        };
        Self {
            id: node.id,
            kind: node.kind,
            path: node.path.clone(),
            children,
            block,
            language: node.language,
        }
    }
}

pub fn run(
    context: &TrueflowContext,
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    let config = load_config()?;
    let scan_options = config.scan.resolve_options();
    let filters = config.review.resolve_filters(only, exclude);

    if let Some(pull_request) = resolve_pull_request_target_for_tui(all, target, since)? {
        return run_pull_request_review(
            context,
            &config,
            &scan_options,
            &filters,
            &pull_request,
            only,
            exclude,
        );
    }

    let pending_cli_requests = cli_review_request(all, target, since, only, exclude)?
        .into_iter()
        .collect::<VecDeque<_>>();
    run_tui_review_loop(
        context,
        &config,
        &scan_options,
        &filters,
        pending_cli_requests,
        true,
    )
}

fn run_tui_review_loop(
    context: &TrueflowContext,
    config: &TrueflowConfig,
    scan_options: &crate::scanner::ScanOptions,
    filters: &BlockFilters,
    mut pending_cli_requests: VecDeque<CliReviewRequest>,
    allow_scope_selector: bool,
) -> Result<()> {
    let mut session = TerminalSession::enter()?;
    let run_result = (|| {
        loop {
            let launch = if let Some(request) = pending_cli_requests.pop_front() {
                let review = {
                    let query = resolve_review_request(
                        request.request,
                        filters.clone(),
                        scan_options.clone(),
                    )?;
                    collect_review(&query)?
                };
                LaunchSelection {
                    scope: request.scope,
                    review,
                    scope_label: request.scope_label,
                    initial_view_mode: request.initial_view_mode,
                }
            } else if allow_scope_selector {
                let LoadedScopeSelector {
                    selector,
                    status_poller,
                } = load_scope_selector(filters, scan_options)?;
                let selection = run_scope_selector(
                    session.terminal_mut(),
                    selector,
                    status_poller,
                    &config.tui.keybinds,
                )?;
                match selection {
                    ScopeSelection::Quit => return Ok(()),
                    ScopeSelection::Selected(scope) => {
                        let review = load_review_state(&scope, filters, scan_options)?;
                        LaunchSelection {
                            scope_label: scope.label(),
                            scope,
                            review,
                            initial_view_mode: ViewMode::Diff,
                        }
                    }
                }
            } else {
                return Ok(());
            };

            let state = build_review_state(
                context,
                launch.review,
                launch.scope,
                ReviewStateBuildOptions {
                    confirm_batch: config.tui.confirm_batch_sub_blocks,
                    block_diff_focus_mode: block_diff_focus_mode_from_config(&config.tui),
                    diff_line_numbers: config.tui.diff_line_numbers,
                    keybinds: config.tui.keybinds,
                    scope_label: launch.scope_label,
                    initial_view_mode: launch.initial_view_mode,
                    speed_read_config: config.tui.speed_read.clone(),
                    speed_read_config_path: speed_read_config_path_for_repo_root(),
                    ai: tui_ai_state_for_config(config),
                },
            )?;

            match run_app(context, &mut session, state)? {
                AppExit::Quit => return Ok(()),
                AppExit::ReviewSomethingElse => {
                    if pending_cli_requests.is_empty() && !allow_scope_selector {
                        return Ok(());
                    }
                }
            }
        }
    })();
    let restore_result = session.restore();
    match (run_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(restore)) => Err(anyhow::anyhow!(
            "{primary:#}\nterminal restore also failed: {restore:#}"
        )),
    }
}

fn resolve_pull_request_target_for_tui(
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
) -> Result<Option<PullRequestRef>> {
    let targets = expand_cli_review_targets(target, since)?;
    let _ = review_request_from_cli_targets(all, &targets)?;
    Ok(extract_pull_request_target(&targets)?.cloned())
}

fn run_pull_request_review(
    context: &TrueflowContext,
    config: &TrueflowConfig,
    scan_options: &crate::scanner::ScanOptions,
    filters: &BlockFilters,
    pull_request: &PullRequestRef,
    _only: &[BlockKind],
    _exclude: &[BlockKind],
) -> Result<()> {
    let repo_root = vcs::git_root_from_workdir()?
        .ok_or_else(|| anyhow!("git repository required for pull request review targets"))?;
    let client = GhGitHubClient;
    let prepared = prepare_pull_request_review(&repo_root, pull_request, &client)?;
    let pending_cli_requests = build_pull_request_cli_requests(&prepared.metadata)?;
    run_tui_review_loop(
        context,
        config,
        scan_options,
        filters,
        pending_cli_requests,
        false,
    )
}

fn build_pull_request_cli_requests(
    metadata: &PullRequestMetadata,
) -> Result<VecDeque<CliReviewRequest>> {
    if metadata.commits.is_empty() {
        return Err(anyhow!(
            "Pull request {} has no commits to review",
            metadata.pr.number
        ));
    }

    metadata
        .commits
        .iter()
        .enumerate()
        .map(|(index, commit)| pull_request_cli_review_request(metadata, index, commit))
        .collect()
}

fn pull_request_cli_review_request(
    metadata: &PullRequestMetadata,
    index: usize,
    commit: &PullRequestCommit,
) -> Result<CliReviewRequest> {
    let revision = RevisionExpr::new(commit.sha.as_str())?;
    let request = ReviewRequest::Targets(vec![ReviewTarget::Revision(revision.clone())]);
    let scope = ScopePreset::Commit {
        id: revision.as_str().to_string(),
        summary: commit.summary.clone(),
    };
    let scope_label = pull_request_scope_label(metadata, index, commit);
    Ok(CliReviewRequest {
        request,
        scope,
        scope_label,
        initial_view_mode: ViewMode::Diff,
    })
}

fn pull_request_scope_label(
    metadata: &PullRequestMetadata,
    index: usize,
    commit: &PullRequestCommit,
) -> String {
    let pr_title = truncate_scope_text(&metadata.title, 28);
    let commit_summary = truncate_scope_text(&commit.summary, 40);
    let short_sha = short_commit_id(commit.sha.as_str());
    if pr_title.is_empty() && commit_summary.is_empty() {
        format!(
            "PR #{} [{}/{}] {}",
            metadata.pr.number,
            index + 1,
            metadata.commits.len(),
            short_sha
        )
    } else if commit_summary.is_empty() {
        format!(
            "PR #{} {} [{}/{}] {}",
            metadata.pr.number,
            pr_title,
            index + 1,
            metadata.commits.len(),
            short_sha
        )
    } else if pr_title.is_empty() {
        format!(
            "PR #{} [{}/{}] {} {}",
            metadata.pr.number,
            index + 1,
            metadata.commits.len(),
            short_sha,
            commit_summary
        )
    } else {
        format!(
            "PR #{} {} [{}/{}] {} {}",
            metadata.pr.number,
            pr_title,
            index + 1,
            metadata.commits.len(),
            short_sha,
            commit_summary
        )
    }
}

fn short_commit_id(id: &str) -> String {
    id.chars().take(7).collect()
}

fn truncate_scope_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if max_chars == 0 || trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let cutoff = max_chars.saturating_sub(3).max(1);
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= cutoff {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn cli_review_request(
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<Option<CliReviewRequest>> {
    cli_review_request_with(
        all,
        target,
        since,
        only,
        exclude,
        resolve_cli_review_request,
    )
}

fn cli_review_request_with<F>(
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
    only: &[BlockKind],
    exclude: &[BlockKind],
    request_resolver: F,
) -> Result<Option<CliReviewRequest>>
where
    F: Fn(bool, &[ReviewTarget], Option<&str>) -> Result<CliReviewRequest>,
{
    let has_cli_overrides =
        all || !target.is_empty() || since.is_some() || !only.is_empty() || !exclude.is_empty();
    if !has_cli_overrides {
        return Ok(None);
    }

    Ok(Some(request_resolver(all, target, since)?))
}

fn resolve_cli_review_request(
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
) -> Result<CliReviewRequest> {
    let targets = expand_cli_review_targets(target, since)?;
    let request = review_request_from_cli_targets(all, &targets)?;
    let (scope, scope_label) = scope_preset_for_cli_targets(all, &targets);
    let initial_view_mode = initial_view_mode_for_cli_targets(all, &targets);
    Ok(CliReviewRequest {
        request,
        scope,
        scope_label,
        initial_view_mode,
    })
}

fn initial_view_mode_for_cli_targets(all: bool, targets: &[ReviewTarget]) -> ViewMode {
    if all {
        return ViewMode::Diff;
    }

    match targets {
        [ReviewTarget::File(_)] => ViewMode::Source,
        _ => ViewMode::Diff,
    }
}

fn scope_preset_for_cli_targets(all: bool, targets: &[ReviewTarget]) -> (ScopePreset, String) {
    if all {
        return (ScopePreset::All, "all files (CLI)".to_string());
    }

    match targets {
        [] | [ReviewTarget::DirtyWorktree] => (ScopePreset::MainDiff, "dirty worktree".to_string()),
        [ReviewTarget::MainDiff] => (ScopePreset::MainDiff, "diff vs main".to_string()),
        [ReviewTarget::File(path)] => (ScopePreset::MainDiff, format!("file {path}")),
        [ReviewTarget::Dir(path)] => (ScopePreset::MainDiff, format!("dir:{path}")),
        [ReviewTarget::Revision(revision)] => (
            ScopePreset::Commit {
                id: revision.as_str().to_string(),
                summary: String::new(),
            },
            format!("revision {revision}"),
        ),
        [ReviewTarget::RevisionRange(range)] => (
            ScopePreset::RevisionRange {
                start: range.start.as_str().to_string(),
                end: range.end.as_str().to_string(),
            },
            format!("revisions {}..{}", range.start, range.end),
        ),
        _ => (ScopePreset::MainDiff, format!("{} targets", targets.len())),
    }
}

fn build_review_state(
    context: &TrueflowContext,
    review: CollectedReview,
    review_scope: ScopePreset,
    options: ReviewStateBuildOptions,
) -> Result<AppState> {
    let CollectedReview {
        summary,
        tree,
        unreviewed_block_nodes,
        commented_block_nodes,
        diff_block_sides,
        file_change_kinds,
        block_change_kinds,
    } = review;
    let reviewable_nodes: HashSet<TreeNodeId> = unreviewed_block_nodes
        .iter()
        .copied()
        .filter(|&id| matches!(tree.node(id).kind, TreeNodeKind::Block))
        .collect();
    let remaining_blocks = reviewable_nodes.len();
    let commented_nodes = commented_block_nodes
        .intersection(&reviewable_nodes)
        .copied()
        .collect::<HashSet<_>>();

    let root_children = tree.node(tree.root()).children.clone();
    let review_order = ReviewOrder::from_tree(&tree, &unreviewed_block_nodes);
    let mut navigator = ReviewNavigator::new(tree, unreviewed_block_nodes)?;
    let mut root_cursor = root_children.first().copied();
    let mut focus_block = None;
    let mut pending_focus_scroll = false;
    if let Some(initial_block) = review_order.first_reviewable_block() {
        navigator.set_current(initial_block);
        root_cursor = root_child_for_node(&navigator.tree, initial_block).or(root_cursor);
        focus_block = Some(initial_block);
        pending_focus_scroll = true;
    }

    let mut ai = options.ai;
    ai.review_set = Some(ai_review_set_context_for_tree(
        &navigator.tree,
        &reviewable_nodes,
        &diff_block_sides,
        &review_scope,
        &options.scope_label,
        ai.max_context_lines,
    ));

    Ok(AppState {
        review_scope,
        navigator,
        review_order,
        total_blocks: summary.total_blocks,
        initial_remaining_blocks: remaining_blocks,
        remaining_blocks,
        reviewable_nodes,
        commented_nodes,
        skipped_nodes: HashSet::new(),
        diff_block_sides,
        file_change_kinds,
        block_change_kinds,
        session_recap: SessionRecap::default(),
        scope_label: options.scope_label,
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        input_cursor: InputCursor::default(),
        input_draft: None,
        editing_validation: None,
        confirm_batch: options.confirm_batch,
        repo_name: detect_repo_name(context),
        repo_root: vcs::git_root_from_workdir().ok().flatten(),
        file_cache: HashMap::new(),
        root_cursor,
        focus_block,
        pending_focus_scroll,
        scroll_offset: 0,
        content_height: 0,
        viewport_height: 0,
        code_rect: Rect::default(),
        visible_comment_capture: None,
        view_mode: options.initial_view_mode,
        block_diff_focus_mode: options.block_diff_focus_mode,
        diff_line_numbers: options.diff_line_numbers,
        keybinds: options.keybinds,
        file_diff_cache: HashMap::new(),
        content_frame_cache: HashMap::new(),
        highlighted_line_cache: HashMap::new(),
        speed_read: SpeedReadController::new(
            options.speed_read_config,
            options.speed_read_config_path,
        ),
        ai,
    })
}

fn tui_ai_state_for_config(config: &TrueflowConfig) -> TuiAiState {
    let availability = resolve_ai_availability(&config.ai, &AiEnvironment::detect_current());
    let mut state =
        TuiAiState::from_availability(availability, config.ai.max_context_lines, config.ai.cache);
    state.provider = ai_suggestion_provider_for_availability(state.availability.as_ref());
    state
}

fn ai_suggestion_provider_for_availability(
    availability: Option<&AiAvailability>,
) -> Option<Arc<dyn AiSuggestionProvider>> {
    let Some(AiAvailability::Ready { provider, model }) = availability else {
        return None;
    };
    if !matches!(provider, AiProvider::ClaudeCli | AiProvider::CodexCli) {
        return None;
    }
    CommandAiSuggestionProvider::new(*provider, model.clone())
        .map(|provider| Arc::new(provider) as Arc<dyn AiSuggestionProvider>)
        .ok()
}

fn root_child_for_node(tree: &Tree, node_id: TreeNodeId) -> Option<TreeNodeId> {
    let root = tree.root();
    let mut current = node_id;
    while let Some(parent) = tree.parent(current) {
        if parent == root {
            return Some(current);
        }
        current = parent;
    }
    None
}

fn block_diff_focus_mode_from_config(config: &TuiConfig) -> vcs::BlockDiffFocusMode {
    match config.diff_focus_mode {
        TuiDiffFocusMode::WholeBlock => vcs::BlockDiffFocusMode::WholeBlock,
        TuiDiffFocusMode::ChangedWithContext => vcs::BlockDiffFocusMode::ChangedWithContext {
            context_lines: config.diff_focus_context_lines,
        },
    }
}

fn keybind_action_for_key_code(
    keybinds: &TuiKeybindsConfig,
    key_code: KeyCode,
) -> Option<KeybindAction> {
    match key_code {
        KeyCode::Up => Some(KeybindAction::Up),
        KeyCode::Down => Some(KeybindAction::Down),
        KeyCode::Left => Some(KeybindAction::Prev),
        KeyCode::Right => Some(KeybindAction::Next),
        KeyCode::Char(ch) if ch == keybinds.scroll_up => Some(KeybindAction::Up),
        KeyCode::Char(ch) if ch == keybinds.scroll_down => Some(KeybindAction::Down),
        KeyCode::Char(ch) if ch == keybinds.prev => Some(KeybindAction::Prev),
        KeyCode::Char(ch) if ch == keybinds.next => Some(KeybindAction::Next),
        KeyCode::Char(ch) if ch == keybinds.parent => Some(KeybindAction::Parent),
        KeyCode::Char(ch) if ch == keybinds.child => Some(KeybindAction::Child),
        KeyCode::Char(ch) if ch == keybinds.approve => Some(KeybindAction::Approve),
        KeyCode::Char(ch) if ch == keybinds.note => Some(KeybindAction::Note),
        KeyCode::Char(ch) if ch == keybinds.toggle_view => Some(KeybindAction::ToggleView),
        KeyCode::Char(ch) if ch == keybinds.speed_read => Some(KeybindAction::SpeedRead),
        KeyCode::Char(ch) if ch == keybinds.root => Some(KeybindAction::Root),
        KeyCode::Char(ch) if ch == keybinds.quit => Some(KeybindAction::Quit),
        _ => None,
    }
}

fn load_scope_options() -> Result<Vec<ScopeOption>> {
    let commits = vcs::recent_commits(8).unwrap_or_default();
    let workdir_prefix = workdir_prefix_from_git_root();
    let commits = filter_commits_for_prefix(
        commits,
        workdir_prefix.as_deref(),
        vcs::files_changed_in_revision,
    );
    Ok(default_scope_options(&commits))
}

fn load_scope_selector(
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
) -> Result<LoadedScopeSelector> {
    let options = load_scope_options()?;
    let workdir_prefix = workdir_prefix_from_git_root();
    let cache = ReviewCoverageStatusCacheStore::load();
    let (selector, jobs) = build_scope_selector_with_status_jobs(
        options,
        filters,
        scan_options,
        workdir_prefix.as_deref(),
        cache.as_ref(),
    );
    let status_poller = ScopeSelectorStatusPoller::spawn(jobs, filters, scan_options, cache);
    Ok(LoadedScopeSelector {
        selector,
        status_poller,
    })
}

#[cfg(test)]
fn load_scope_selector_with<F>(
    options: Vec<ScopeOption>,
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
    mut load_summary: F,
) -> Result<ScopeSelector>
where
    F: FnMut(&ScopePreset, &BlockFilters, &crate::scanner::ScanOptions) -> Result<ReviewSummary>,
{
    let options = options
        .into_iter()
        .map(|option| {
            let status = match load_summary(&option.scope, filters, scan_options) {
                Ok(summary) => ScopeSelectorStatus::from_summary(&summary),
                Err(_) => ScopeSelectorStatus::Unavailable,
            };
            ScopeSelectorOption::from_scope_option(option, status)
        })
        .collect();
    Ok(ScopeSelector::new(options))
}

fn filter_commits_for_prefix<F>(
    commits: Vec<vcs::CommitInfo>,
    workdir_prefix: Option<&str>,
    mut changed_paths_for_revision: F,
) -> Vec<vcs::CommitInfo>
where
    F: FnMut(&str) -> Result<HashSet<RepoPath>>,
{
    let Some(prefix) = workdir_prefix
        .map(path_utils::normalize_path_str)
        .filter(|p| !p.is_empty())
    else {
        return commits;
    };

    commits
        .into_iter()
        .filter(|commit| {
            match changed_paths_for_revision(commit.id.as_str()) {
                Ok(paths) => paths
                    .iter()
                    .any(|path| path_utils::path_matches_workdir_prefix(path.as_str(), &prefix)),
                // If we can't resolve changed paths, keep the option instead of hiding it.
                Err(_) => true,
            }
        })
        .collect()
}

fn speed_read_config_path_for_repo_root() -> PathBuf {
    match vcs::git_root_from_workdir() {
        Ok(Some(root)) => root.join("trueflow.toml"),
        Ok(None) | Err(_) => PathBuf::from("trueflow.toml"),
    }
}

#[derive(Default)]
struct EventPump {
    pending: Option<Event>,
}

impl EventPump {
    fn read_blocking(&mut self) -> Result<Event> {
        if let Some(event) = self.pending.take() {
            return Ok(event);
        }
        Ok(event::read()?)
    }

    fn read_with_deadline(&mut self, deadline: Option<Instant>) -> Result<Option<Event>> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }

        let Some(deadline) = deadline else {
            return Ok(Some(event::read()?));
        };

        let now = Instant::now();
        if deadline <= now {
            return Ok(None);
        }

        let timeout = deadline.saturating_duration_since(now);
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }

    fn coalesce_resize_burst(&mut self, first_event: Event) -> Result<Event> {
        coalesce_resize_event_with(
            first_event,
            &mut self.pending,
            || Ok(event::poll(Duration::from_millis(0))?),
            || Ok(event::read()?),
        )
    }
}

fn coalesce_resize_event_with<P, R>(
    first_event: Event,
    pending: &mut Option<Event>,
    mut poll_ready: P,
    mut read_event: R,
) -> Result<Event>
where
    P: FnMut() -> Result<bool>,
    R: FnMut() -> Result<Event>,
{
    let mut event = first_event;
    if !matches!(event, Event::Resize(_, _)) {
        return Ok(event);
    }

    while poll_ready()? {
        let next_event = read_event()?;
        if matches!(next_event, Event::Resize(_, _)) {
            event = next_event;
            continue;
        }

        let previous = pending.replace(next_event);
        debug_assert!(
            previous.is_none(),
            "pending event slot was unexpectedly occupied"
        );
        break;
    }

    Ok(event)
}

fn run_scope_selector(
    terminal: &mut TuiTerminal,
    mut selector: ScopeSelector,
    mut status_poller: Option<ScopeSelectorStatusPoller>,
    keybinds: &TuiKeybindsConfig,
) -> Result<ScopeSelection> {
    let mut needs_render = true;
    let mut event_pump = EventPump::default();

    loop {
        if let Some(poller) = status_poller.as_mut()
            && poller.drain_updates(&mut selector)
        {
            needs_render = true;
        }

        if needs_render {
            terminal.draw(|f| render_scope_selector(f, &selector, keybinds))?;
            needs_render = false;
        }

        let event = if status_poller
            .as_ref()
            .is_some_and(ScopeSelectorStatusPoller::has_pending_jobs)
        {
            event_pump
                .read_with_deadline(Some(Instant::now() + SCOPE_SELECTOR_STATUS_POLL_INTERVAL))?
        } else {
            Some(event_pump.read_blocking()?)
        };
        let Some(event) = event else {
            continue;
        };

        let event = event_pump.coalesce_resize_burst(event)?;
        if should_rerender_on_event(&event) {
            needs_render = true;
            continue;
        }

        let Some(key_event) = key_event_for_press_or_repeat_event(&event) else {
            continue;
        };
        let key_code = key_event.code;

        if let Some(action) = keybind_action_for_key_code(keybinds, key_code) {
            if key_event.kind == KeyEventKind::Repeat && !keybind_action_accepts_repeat(action) {
                continue;
            }

            match action {
                KeybindAction::Up => {
                    selector.move_prev();
                    needs_render = true;
                    continue;
                }
                KeybindAction::Down => {
                    selector.move_next();
                    needs_render = true;
                    continue;
                }
                KeybindAction::Quit => {
                    if let Some(poller) = status_poller.as_ref() {
                        poller.cancel();
                    }
                    return Ok(ScopeSelection::Quit);
                }
                KeybindAction::Prev
                | KeybindAction::Next
                | KeybindAction::Parent
                | KeybindAction::Child
                | KeybindAction::Approve
                | KeybindAction::Note
                | KeybindAction::ToggleView
                | KeybindAction::SpeedRead
                | KeybindAction::Root => {}
            }
        }

        if key_event.kind == KeyEventKind::Repeat {
            continue;
        }

        match key_code {
            KeyCode::Esc => {
                if let Some(poller) = status_poller.as_ref() {
                    poller.cancel();
                }
                return Ok(ScopeSelection::Quit);
            }
            KeyCode::Enter => {
                if let Some(scope) = selector.selected_scope() {
                    if let Some(poller) = status_poller.as_ref() {
                        poller.cancel();
                    }
                    return Ok(ScopeSelection::Selected(scope));
                }
            }
            _ => {}
        }
    }
}

fn run_app(
    context: &TrueflowContext,
    session: &mut TerminalSession,
    mut state: AppState,
) -> Result<AppExit> {
    let mut needs_render = true;
    let mut event_pump = EventPump::default();

    loop {
        if needs_render {
            session.terminal_mut().draw(|f| ui(f, &mut state))?;
            needs_render = false;
        }

        if refresh_ai_suggestion_state(&mut state) {
            needs_render = true;
            continue;
        }

        let event = read_next_app_event(&state, &mut event_pump)?;
        if event.is_none() {
            let now = Instant::now();
            let mut rerender = false;
            if handle_speed_read_autoplay_timeout(&mut state, now) {
                rerender = true;
            }
            if flush_due_speed_read_defaults(&mut state, now)? {
                rerender = true;
            }
            if rerender {
                needs_render = true;
            }
            continue;
        }

        let Some(event) = event else {
            continue;
        };
        if should_rerender_on_event(&event) {
            needs_render = true;
            continue;
        }

        if let Event::Paste(pasted) = &event {
            if handle_paste_event(&mut state, pasted) {
                needs_render = true;
            }
            continue;
        }

        if let Event::Mouse(mouse_event) = &event {
            if handle_mouse_event(&mut state, *mouse_event) {
                needs_render = true;
            }
            continue;
        }

        match &state.input_mode {
            InputMode::Normal => {
                let Some(key_event) = key_event_for_press_or_repeat_event(&event) else {
                    continue;
                };
                let key_code = key_event.code;
                let ui_mode = current_ui_mode(&state);

                if matches!(ui_mode, UiMode::Recap) {
                    if key_event.kind == KeyEventKind::Press
                        && let Some(action) = recap_action_for_key_code(&state.keybinds, key_code)
                    {
                        flush_pending_speed_read_defaults(&mut state)?;
                        return Ok(match action {
                            RecapAction::Exit => AppExit::Quit,
                            RecapAction::ReviewSomethingElse => AppExit::ReviewSomethingElse,
                        });
                    }
                    continue;
                }

                if matches!(ui_mode, UiMode::SpeedRead)
                    && key_event.kind == KeyEventKind::Press
                    && handle_speed_read_key_binding(&mut state, key_code)
                {
                    needs_render = true;
                    continue;
                }

                if let Some(action) = keybind_action_for_key_code(&state.keybinds, key_code) {
                    if key_event.kind == KeyEventKind::Repeat
                        && !keybind_action_accepts_repeat(action)
                    {
                        continue;
                    }

                    match action {
                        KeybindAction::Up => handle_scroll_line_up(&mut state),
                        KeybindAction::Down => handle_scroll_line_down(&mut state),
                        KeybindAction::Prev => handle_prev(&mut state),
                        KeybindAction::Next => handle_next(&mut state),
                        KeybindAction::Parent => handle_parent(&mut state),
                        KeybindAction::Child => handle_child(&mut state),
                        KeybindAction::Approve => {
                            handle_action(session, context, &mut state, Verdict::Approved)?;
                        }
                        KeybindAction::Note => {
                            handle_note_action(&mut state)?;
                        }
                        KeybindAction::ToggleView => {
                            state.view_mode = match state.view_mode {
                                ViewMode::Source => ViewMode::Diff,
                                ViewMode::Diff => ViewMode::Source,
                            };
                            let preferred_focus = state.focus_block;
                            set_focus_for_current_node(&mut state, preferred_focus);
                        }
                        KeybindAction::SpeedRead => toggle_speed_read_mode(&mut state),
                        KeybindAction::Root => {
                            state.navigator.jump_root();
                            clear_focus_scroll(&mut state);
                            sync_speed_read_focus(&mut state);
                        }
                        KeybindAction::Quit => {
                            flush_pending_speed_read_defaults(&mut state)?;
                            return Ok(AppExit::Quit);
                        }
                    }
                    needs_render = true;
                    continue;
                }

                if key_event.kind == KeyEventKind::Repeat
                    && matches!(key_code, KeyCode::Char(' '))
                    && state.navigator.current_id() != state.navigator.tree.root()
                {
                    continue;
                }

                if key_event.kind == KeyEventKind::Repeat
                    && !key_code_accepts_repeat_in_normal_mode(key_code)
                {
                    continue;
                }

                match key_code {
                    KeyCode::Char(' ')
                        if key_event.kind == KeyEventKind::Press
                            && state.navigator.current_id() != state.navigator.tree.root() =>
                    {
                        handle_advance_review_target(&mut state);
                        needs_render = true;
                    }
                    KeyCode::Char(' ') => {
                        handle_scroll_page_down(&mut state);
                        needs_render = true;
                    }
                    KeyCode::PageUp => {
                        handle_scroll_page_up(&mut state);
                        needs_render = true;
                    }
                    KeyCode::PageDown => {
                        handle_scroll_page_down(&mut state);
                        needs_render = true;
                    }
                    KeyCode::Home => {
                        state.scroll_offset = 0;
                        needs_render = true;
                    }
                    KeyCode::End => {
                        state.scroll_offset =
                            state.content_height.saturating_sub(state.viewport_height);
                        needs_render = true;
                    }
                    KeyCode::Enter
                        if key_event.kind == KeyEventKind::Press
                            && state.navigator.current_id() == state.navigator.tree.root() =>
                    {
                        handle_child(&mut state);
                        needs_render = true;
                    }
                    _ => {}
                }
            }
            InputMode::Editing { .. } => {
                let Some(key_event) = key_event_for_press_or_repeat_event(&event) else {
                    continue;
                };

                match handle_editing_key_action(
                    &mut state,
                    editing_key_action_for_event(&key_event),
                ) {
                    EditingActionResult::Submit => {
                        handle_editing_submit(session, context, &mut state)?;
                        needs_render = true;
                    }
                    EditingActionResult::Handled => {
                        needs_render = true;
                    }
                    EditingActionResult::Noop => {}
                }
            }
            InputMode::ConfirmBatch { .. } => {
                let Some(key_event) = key_event_for_press_event(&event) else {
                    continue;
                };

                match key_event.code {
                    KeyCode::Enter => {
                        handle_confirm_batch(session, context, &mut state)?;
                        needs_render = true;
                    }
                    KeyCode::Esc => {
                        handle_confirm_cancel(&mut state);
                        needs_render = true;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn read_next_app_event(state: &AppState, event_pump: &mut EventPump) -> Result<Option<Event>> {
    let Some(event) = event_pump.read_with_deadline(next_app_deadline(state))? else {
        return Ok(None);
    };
    Ok(Some(event_pump.coalesce_resize_burst(event)?))
}

fn next_app_deadline(state: &AppState) -> Option<Instant> {
    earliest_deadline(
        state.speed_read.next_deadline(state.navigator.current_id()),
        state.ai.ai_poll_deadline(),
    )
}

fn earliest_deadline(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

fn refresh_ai_suggestion_state(state: &mut AppState) -> bool {
    let mut changed = poll_ai_suggestion_result(state);
    if ensure_ai_suggestion_for_current_focus(state) {
        changed = true;
    }
    if !changed && advance_ai_loading_frame(state) {
        changed = true;
    }
    changed
}

fn poll_ai_suggestion_result(state: &mut AppState) -> bool {
    let Some(poll_result) = state
        .ai
        .pending
        .as_ref()
        .map(|pending| (pending.key.clone(), pending.receiver.try_recv()))
    else {
        return false;
    };

    match poll_result {
        (_, Ok(result)) => {
            state.ai.pending = None;
            match result.result {
                Ok(suggestion) => {
                    if state.ai.cache_enabled {
                        state
                            .ai
                            .cache
                            .insert(result.key.clone(), suggestion.clone());
                    }
                    state.ai.status = TuiAiStatus::Suggestion {
                        key: result.key,
                        suggestion,
                    };
                }
                Err(message) => {
                    state.ai.status = TuiAiStatus::Error {
                        key: result.key,
                        message: truncate_ai_error(&message),
                    };
                }
            }
            true
        }
        (_, Err(mpsc::TryRecvError::Empty)) => false,
        (key, Err(mpsc::TryRecvError::Disconnected)) => {
            state.ai.pending = None;
            state.ai.status = TuiAiStatus::Error {
                key,
                message: "provider worker disconnected".to_string(),
            };
            true
        }
    }
}

fn ensure_ai_suggestion_for_current_focus(state: &mut AppState) -> bool {
    let Some(request) = ai_suggestion_request_for_current_focus(state) else {
        return reset_ai_status_to_availability(state);
    };
    let key = request.key.clone();

    if state.ai.status.key() == Some(&key) {
        return false;
    }

    if state.ai.cache_enabled
        && let Some(suggestion) = state.ai.cache.get(&key).cloned()
    {
        state.ai.pending = None;
        state.ai.status = TuiAiStatus::Suggestion { key, suggestion };
        return true;
    }

    let Some(provider) = state.ai.provider.clone() else {
        return reset_ai_status_to_availability(state);
    };

    let (sender, receiver) = mpsc::channel();
    let thread_key = key.clone();
    let _worker = thread::spawn(move || {
        let result = provider
            .suggest(&request)
            .map_err(|error| truncate_ai_error(&error.to_string()));
        let _ = sender.send(AiSuggestionWorkerResult {
            key: thread_key,
            result,
        });
    });

    state.ai.pending = Some(PendingAiSuggestion::new(
        key.clone(),
        receiver,
        Instant::now(),
    ));
    state.ai.status = TuiAiStatus::Loading { key, frame: 0 };
    true
}

const AI_LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(480);
const AI_LOADING_HINT_FRAMES: &[&str] = &["✦ · ·", "· ✧ ·", "· · ✦", "· ✧ ·"];

fn ai_loading_hint_text(frame: usize) -> &'static str {
    AI_LOADING_HINT_FRAMES[frame % AI_LOADING_HINT_FRAMES.len()]
}

fn advance_ai_loading_frame(state: &mut AppState) -> bool {
    let Some(pending) = state.ai.pending.as_mut() else {
        return false;
    };
    let now = Instant::now();
    if pending.next_frame_at > now {
        return false;
    }
    let TuiAiStatus::Loading { frame, .. } = &mut state.ai.status else {
        return false;
    };
    *frame = frame.saturating_add(1);
    pending.next_frame_at = now + AI_LOADING_FRAME_INTERVAL;
    true
}

fn cancel_ai_suggestion(state: &mut AppState) -> bool {
    reset_ai_status_to_availability(state)
}

fn reset_ai_status_to_availability(state: &mut AppState) -> bool {
    state.ai.pending = None;
    if matches!(state.ai.status, TuiAiStatus::Availability) {
        return false;
    }
    state.ai.status = TuiAiStatus::Availability;
    true
}

const AI_REVIEW_SET_CONTEXT_MAX_BLOCKS: usize = 120;
const AI_REVIEW_SET_CONTEXT_LINES_PER_BLOCK: usize = 12;

fn ai_review_set_context_for_tree(
    tree: &Tree,
    reviewable_nodes: &HashSet<TreeNodeId>,
    diff_block_sides: &HashMap<TreeNodeId, DiffBlockSides>,
    review_scope: &ScopePreset,
    scope_label: &str,
    max_context_lines: usize,
) -> AiReviewSetContext {
    let entries = ai_review_set_entries(tree, reviewable_nodes, diff_block_sides);
    let review_set_hash = ai_review_set_hash(review_scope, scope_label, &entries);
    let overview = ai_review_set_overview(scope_label, review_scope, &entries, max_context_lines);
    AiReviewSetContext::new(review_set_hash, overview)
}

fn ai_review_set_entries(
    tree: &Tree,
    reviewable_nodes: &HashSet<TreeNodeId>,
    diff_block_sides: &HashMap<TreeNodeId, DiffBlockSides>,
) -> Vec<AiReviewContext> {
    let mut entries = reviewable_nodes
        .iter()
        .filter_map(|node_id| ai_review_context_for_node(tree, *node_id, diff_block_sides))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        (
            left.path.as_str(),
            left.start_line,
            left.end_line,
            left.block_hash.as_str(),
        )
            .cmp(&(
                right.path.as_str(),
                right.start_line,
                right.end_line,
                right.block_hash.as_str(),
            ))
    });
    entries
}

fn ai_review_set_hash(
    review_scope: &ScopePreset,
    scope_label: &str,
    entries: &[AiReviewContext],
) -> TreeHash {
    let mut input = format!("scope:{review_scope:?}\nlabel:{scope_label}\n");
    for entry in entries {
        input.push_str(&format!(
            "{}:{}:{}:{}:{}:{}\n",
            entry.path,
            entry.start_line,
            entry.end_line,
            entry.block_kind.as_str(),
            entry.block_hash,
            hash_str(&entry.content),
        ));
    }
    TreeHash::new(hash_str(&input))
}

fn ai_review_set_overview(
    scope_label: &str,
    review_scope: &ScopePreset,
    entries: &[AiReviewContext],
    max_context_lines: usize,
) -> String {
    let mut lines = vec![
        format!("Scope: {scope_label}"),
        format!("Scope preset: {review_scope:?}"),
        format!("Review blocks: {}", entries.len()),
    ];
    let mut used_content_lines = 0;
    let content_line_budget = max_context_lines.max(1);

    for (index, entry) in entries
        .iter()
        .take(AI_REVIEW_SET_CONTEXT_MAX_BLOCKS)
        .enumerate()
    {
        if used_content_lines >= content_line_budget {
            lines.push("...".to_string());
            break;
        }
        let line_start = entry.start_line.saturating_add(1);
        let line_end = entry.end_line.max(entry.start_line.saturating_add(1));
        lines.push(format!(
            "## {}. {} lines {line_start}-{line_end} {} {}",
            index + 1,
            entry.path,
            entry.block_kind.as_str(),
            short_ai_hash(&entry.block_hash),
        ));
        let remaining_lines = content_line_budget.saturating_sub(used_content_lines);
        let snippet_lines = remaining_lines.min(AI_REVIEW_SET_CONTEXT_LINES_PER_BLOCK);
        used_content_lines +=
            push_ai_review_set_snippet_lines(&mut lines, &entry.content, snippet_lines);
    }

    if entries.len() > AI_REVIEW_SET_CONTEXT_MAX_BLOCKS {
        lines.push(format!(
            "... {} more review blocks omitted from AI context",
            entries.len() - AI_REVIEW_SET_CONTEXT_MAX_BLOCKS
        ));
    }

    lines.join("\n")
}

fn push_ai_review_set_snippet_lines(
    out: &mut Vec<String>,
    content: &str,
    max_lines: usize,
) -> usize {
    if max_lines == 0 {
        return 0;
    }
    let mut pushed = 0;
    let mut lines = content.lines().peekable();
    while pushed < max_lines {
        let Some(line) = lines.next() else {
            break;
        };
        out.push(line.to_string());
        pushed += 1;
    }
    if lines.peek().is_some() {
        out.push("...".to_string());
    }
    if pushed == 0 {
        out.push("(empty block)".to_string());
        return 1;
    }
    pushed
}

fn short_ai_hash(hash: &TreeHash) -> &str {
    hash.as_str().get(..12).unwrap_or_else(|| hash.as_str())
}

fn ai_review_context_for_node(
    tree: &Tree,
    node_id: TreeNodeId,
    diff_block_sides: &HashMap<TreeNodeId, DiffBlockSides>,
) -> Option<AiReviewContext> {
    let node = tree.node(node_id);
    let block = node.block.as_ref()?;
    Some(AiReviewContext {
        path: node.path.as_str().to_string(),
        language: node.language.unwrap_or_default(),
        block_kind: block.kind,
        block_hash: block.hash.clone(),
        start_line: block.start_line,
        end_line: block.end_line,
        content: ai_block_context_content(block.content.as_str(), diff_block_sides.get(&node_id)),
    })
}

fn ai_block_context_content(display_content: &str, sides: Option<&DiffBlockSides>) -> String {
    let Some(sides) = sides else {
        return display_content.to_string();
    };
    let mut out = String::new();
    if let Some(base) = sides.base.as_ref() {
        out.push_str("[base]\n");
        out.push_str(&base.content);
        if !base.content.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(head) = sides.head.as_ref() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[head]\n");
        out.push_str(&head.content);
    }
    if out.trim().is_empty() {
        display_content.to_string()
    } else {
        out
    }
}

fn ai_suggestion_request_for_current_focus(state: &AppState) -> Option<AiSuggestionRequest> {
    let AiAvailability::Ready { provider, model } = state.ai.availability.as_ref()? else {
        return None;
    };
    let focus_block = state.focus_block?;
    let context =
        ai_review_context_for_node(&state.navigator.tree, focus_block, &state.diff_block_sides)?;
    let review_set = state.ai.review_set.clone().unwrap_or_else(|| {
        ai_review_set_context_for_tree(
            &state.navigator.tree,
            &state.reviewable_nodes,
            &state.diff_block_sides,
            &state.review_scope,
            &state.scope_label,
            state.ai.max_context_lines,
        )
    });
    Some(AiSuggestionRequest::with_response_char_limit(
        *provider,
        model.clone(),
        review_set,
        context,
        state.ai.max_context_lines,
        ai_response_char_limit(state),
    ))
}

fn ai_response_char_limit(state: &AppState) -> usize {
    let viewport_width = usize::from(state.code_rect.width);
    if viewport_width == 0 {
        DEFAULT_AI_RESPONSE_CHAR_LIMIT
    } else {
        viewport_width
    }
}

impl TuiAiStatus {
    fn key(&self) -> Option<&AiSuggestionKey> {
        match self {
            Self::Availability => None,
            Self::Loading { key, .. } | Self::Suggestion { key, .. } | Self::Error { key, .. } => {
                Some(key)
            }
        }
    }
}

fn truncate_ai_error(message: &str) -> String {
    let max_chars = 120;
    if message.chars().count() <= max_chars {
        return message.to_string();
    }
    let mut out = message
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn sync_speed_read_focus(state: &mut AppState) {
    state
        .speed_read
        .clear_if_not_on_current_node(state.navigator.current_id());
}

fn toggle_speed_read_mode(state: &mut AppState) {
    let current_id = state.navigator.current_id();
    let node = state.navigator.tree.node(current_id);
    if state
        .speed_read
        .toggle_for_node(current_id, node.kind, node.block.as_ref())
    {
        state.scroll_offset = 0;
    }
}

fn handle_speed_read_key_binding(state: &mut AppState, key_code: KeyCode) -> bool {
    state
        .speed_read
        .handle_key_binding(key_code, state.navigator.current_id())
}

fn handle_speed_read_autoplay_timeout(state: &mut AppState, now: Instant) -> bool {
    state
        .speed_read
        .handle_autoplay_timeout(now, state.navigator.current_id())
}

fn flush_due_speed_read_defaults(state: &mut AppState, now: Instant) -> Result<bool> {
    state.speed_read.flush_due_defaults(now)
}

fn flush_pending_speed_read_defaults(state: &mut AppState) -> Result<()> {
    state.speed_read.flush_pending_defaults()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditingKeyAction {
    Submit,
    InsertNewline,
    Cancel,
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveHome,
    MoveEnd,
    InsertChar(char),
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditingActionResult {
    Noop,
    Handled,
    Submit,
}

fn editing_key_action_for_event(key_event: &KeyEvent) -> EditingKeyAction {
    match key_event.kind {
        KeyEventKind::Release => return EditingKeyAction::Ignore,
        KeyEventKind::Repeat => match key_event.code {
            KeyCode::Backspace => return EditingKeyAction::Backspace,
            KeyCode::Delete => return EditingKeyAction::Delete,
            KeyCode::Left => return EditingKeyAction::MoveLeft,
            KeyCode::Right => return EditingKeyAction::MoveRight,
            KeyCode::Up => return EditingKeyAction::MoveUp,
            KeyCode::Down => return EditingKeyAction::MoveDown,
            KeyCode::Home => return EditingKeyAction::MoveHome,
            KeyCode::End => return EditingKeyAction::MoveEnd,
            KeyCode::Char(c) => {
                if key_event
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    return EditingKeyAction::Ignore;
                }
                return EditingKeyAction::InsertChar(c);
            }
            _ => return EditingKeyAction::Ignore,
        },
        KeyEventKind::Press => {}
    }

    match key_event.code {
        KeyCode::Enter if key_event.modifiers.contains(KeyModifiers::SHIFT) => {
            EditingKeyAction::InsertNewline
        }
        KeyCode::Enter => EditingKeyAction::Submit,
        KeyCode::Esc => EditingKeyAction::Cancel,
        KeyCode::Backspace => EditingKeyAction::Backspace,
        KeyCode::Delete => EditingKeyAction::Delete,
        KeyCode::Left => EditingKeyAction::MoveLeft,
        KeyCode::Right => EditingKeyAction::MoveRight,
        KeyCode::Up => EditingKeyAction::MoveUp,
        KeyCode::Down => EditingKeyAction::MoveDown,
        KeyCode::Home => EditingKeyAction::MoveHome,
        KeyCode::End => EditingKeyAction::MoveEnd,
        KeyCode::Char('j')
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && !key_event
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            EditingKeyAction::InsertNewline
        }
        KeyCode::Char(c) => {
            if key_event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                EditingKeyAction::Ignore
            } else {
                EditingKeyAction::InsertChar(c)
            }
        }
        _ => EditingKeyAction::Ignore,
    }
}

fn handle_editing_key_action(
    state: &mut AppState,
    action: EditingKeyAction,
) -> EditingActionResult {
    match action {
        EditingKeyAction::Submit => EditingActionResult::Submit,
        EditingKeyAction::InsertNewline => {
            discard_input_draft(state);
            if insert_text_at_input_cursor(state, "\n") {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::Cancel => {
            handle_editing_cancel(state);
            EditingActionResult::Handled
        }
        EditingKeyAction::Backspace => {
            let cleared_validation = state.editing_validation.is_some();
            if cleared_validation {
                clear_editing_validation(state);
            }
            if discard_input_draft(state) || delete_before_input_cursor(state) || cleared_validation
            {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::Delete => {
            let cleared_validation = state.editing_validation.is_some();
            if cleared_validation {
                clear_editing_validation(state);
            }
            if discard_input_draft(state) || delete_after_input_cursor(state) || cleared_validation
            {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::MoveLeft => {
            if move_input_cursor_left(state) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::MoveRight => {
            if accept_input_draft(state) || move_input_cursor_right(state) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::MoveUp => {
            if move_input_cursor_vertically(state, -1) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::MoveDown => {
            if move_input_cursor_vertically(state, 1) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::MoveHome => {
            if move_input_cursor_to_comment_start(state) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::MoveEnd => {
            if move_input_cursor_to_comment_end(state) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::InsertChar(c) => {
            discard_input_draft(state);
            let mut utf8 = [0u8; 4];
            if insert_text_at_input_cursor(state, c.encode_utf8(&mut utf8)) {
                EditingActionResult::Handled
            } else {
                EditingActionResult::Noop
            }
        }
        EditingKeyAction::Ignore => EditingActionResult::Noop,
    }
}

fn clamp_cursor_offset_to_char_boundary(content: &str, offset: usize) -> usize {
    let mut clamped = offset.min(content.len());
    while clamped > 0 && !content.is_char_boundary(clamped) {
        clamped = clamped.saturating_sub(1);
    }
    clamped
}

fn previous_char_boundary(content: &str, offset: usize) -> usize {
    let offset = clamp_cursor_offset_to_char_boundary(content, offset);
    if offset == 0 {
        return 0;
    }
    content[..offset]
        .char_indices()
        .last()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(content: &str, offset: usize) -> usize {
    let offset = clamp_cursor_offset_to_char_boundary(content, offset);
    if offset >= content.len() {
        return content.len();
    }
    let next_char_len = content[offset..].chars().next().map_or(0, char::len_utf8);
    offset.saturating_add(next_char_len)
}

fn line_start_for_cursor(content: &str, offset: usize) -> usize {
    let offset = clamp_cursor_offset_to_char_boundary(content, offset);
    content[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end_for_start(content: &str, line_start: usize) -> usize {
    content[line_start..]
        .find('\n')
        .map_or(content.len(), |index| line_start + index)
}

fn char_column_for_offset(content: &str, line_start: usize, offset: usize) -> usize {
    let offset = clamp_cursor_offset_to_char_boundary(content, offset);
    content[line_start..offset].chars().count()
}

fn byte_offset_for_char_column(content: &str, column: usize) -> usize {
    let mut byte_offset = 0usize;
    let mut chars_seen = 0usize;
    for ch in content.chars() {
        if chars_seen == column {
            break;
        }
        byte_offset = byte_offset.saturating_add(ch.len_utf8());
        chars_seen = chars_seen.saturating_add(1);
    }
    byte_offset
}

fn previous_line_start(content: &str, current_line_start: usize) -> Option<usize> {
    if current_line_start == 0 {
        return None;
    }

    let search_end = current_line_start.saturating_sub(1);
    Some(
        content[..search_end]
            .rfind('\n')
            .map_or(0, |index| index + 1),
    )
}

fn next_line_start(content: &str, current_line_end: usize) -> Option<usize> {
    (current_line_end < content.len()).then_some(current_line_end + 1)
}

fn accept_input_draft(state: &mut AppState) -> bool {
    if !matches!(state.input_mode, InputMode::Editing { .. }) || !state.input_buffer.is_empty() {
        return false;
    }
    let Some(draft) = state.input_draft.take() else {
        return false;
    };
    state.input_buffer = draft;
    state.input_cursor.offset = state.input_buffer.len();
    state.input_cursor.clear_goal_column();
    clear_editing_validation(state);
    true
}

fn discard_input_draft(state: &mut AppState) -> bool {
    state.input_draft.take().is_some()
}

fn insert_text_at_input_cursor(state: &mut AppState, inserted: &str) -> bool {
    if inserted.is_empty() {
        return false;
    }

    discard_input_draft(state);
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    clear_editing_validation(state);
    state
        .input_buffer
        .insert_str(state.input_cursor.offset, inserted);
    state.input_cursor.offset = state.input_cursor.offset.saturating_add(inserted.len());
    state.input_cursor.clear_goal_column();
    true
}

fn delete_before_input_cursor(state: &mut AppState) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    if state.input_cursor.offset == 0 {
        return false;
    }

    let previous = previous_char_boundary(&state.input_buffer, state.input_cursor.offset);
    clear_editing_validation(state);
    state
        .input_buffer
        .replace_range(previous..state.input_cursor.offset, "");
    state.input_cursor.offset = previous;
    state.input_cursor.clear_goal_column();
    true
}

fn delete_after_input_cursor(state: &mut AppState) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    if state.input_cursor.offset >= state.input_buffer.len() {
        return false;
    }

    let next = next_char_boundary(&state.input_buffer, state.input_cursor.offset);
    clear_editing_validation(state);
    state
        .input_buffer
        .replace_range(state.input_cursor.offset..next, "");
    state.input_cursor.clear_goal_column();
    true
}

fn move_input_cursor_left(state: &mut AppState) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    if state.input_cursor.offset == 0 {
        return false;
    }

    state.input_cursor.offset =
        previous_char_boundary(&state.input_buffer, state.input_cursor.offset);
    state.input_cursor.clear_goal_column();
    true
}

fn move_input_cursor_right(state: &mut AppState) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    if state.input_cursor.offset >= state.input_buffer.len() {
        return false;
    }

    state.input_cursor.offset = next_char_boundary(&state.input_buffer, state.input_cursor.offset);
    state.input_cursor.clear_goal_column();
    true
}

fn move_input_cursor_to_comment_start(state: &mut AppState) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    if state.input_cursor.offset == 0 {
        state.input_cursor.clear_goal_column();
        return false;
    }

    state.input_cursor.offset = 0;
    state.input_cursor.clear_goal_column();
    true
}

fn move_input_cursor_to_comment_end(state: &mut AppState) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    let end = state.input_buffer.len();
    if state.input_cursor.offset == end {
        state.input_cursor.clear_goal_column();
        return false;
    }

    state.input_cursor.offset = end;
    state.input_cursor.clear_goal_column();
    true
}

fn move_input_cursor_vertically(state: &mut AppState, direction: isize) -> bool {
    state.input_cursor = state.input_cursor.clamped_to_buffer(&state.input_buffer);
    if state.input_buffer.is_empty() {
        return false;
    }

    let line_start = line_start_for_cursor(&state.input_buffer, state.input_cursor.offset);
    let line_end = line_end_for_start(&state.input_buffer, line_start);
    let current_column =
        char_column_for_offset(&state.input_buffer, line_start, state.input_cursor.offset);
    let goal_column = state.input_cursor.goal_column.unwrap_or(current_column);

    let target_start = if direction.is_negative() {
        previous_line_start(&state.input_buffer, line_start)
    } else {
        next_line_start(&state.input_buffer, line_end)
    };
    let Some(target_start) = target_start else {
        return false;
    };

    let target_end = line_end_for_start(&state.input_buffer, target_start);
    let target_line = &state.input_buffer[target_start..target_end];
    let target_column = goal_column.min(target_line.chars().count());
    let target_offset = target_start + byte_offset_for_char_column(target_line, target_column);

    if state.input_cursor.offset == target_offset {
        state.input_cursor.goal_column = Some(goal_column);
        return false;
    }

    state.input_cursor.offset = target_offset;
    state.input_cursor.goal_column = Some(goal_column);
    true
}

fn key_event_for_press_event(event: &Event) -> Option<KeyEvent> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(*key),
        _ => None,
    }
}

fn key_event_for_press_or_repeat_event(event: &Event) -> Option<KeyEvent> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(*key)
        }
        _ => None,
    }
}

fn keybind_action_accepts_repeat(action: KeybindAction) -> bool {
    matches!(
        action,
        KeybindAction::Up
            | KeybindAction::Down
            | KeybindAction::Prev
            | KeybindAction::Next
            | KeybindAction::Parent
            | KeybindAction::Child
    )
}

fn key_code_accepts_repeat_in_normal_mode(key_code: KeyCode) -> bool {
    matches!(
        key_code,
        KeyCode::Char(' ') | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
    )
}

#[cfg(test)]
fn key_code_for_press_event(event: &Event) -> Option<KeyCode> {
    key_event_for_press_event(event).map(|key| key.code)
}

#[cfg(test)]
fn key_code_for_press_or_repeat_event(event: &Event) -> Option<KeyCode> {
    key_event_for_press_or_repeat_event(event).map(|key| key.code)
}

fn should_rerender_on_event(event: &Event) -> bool {
    matches!(event, Event::Resize(_, _))
}

fn handle_paste_event(state: &mut AppState, pasted: &str) -> bool {
    if pasted.is_empty() || !matches!(state.input_mode, InputMode::Editing { .. }) {
        return false;
    }

    insert_text_at_input_cursor(state, pasted)
}

fn handle_mouse_event(state: &mut AppState, mouse_event: MouseEvent) -> bool {
    if !matches!(state.input_mode, InputMode::Normal) || is_recap_mode(state) {
        return false;
    }
    if !rect_contains(state.code_rect, mouse_event.column, mouse_event.row) {
        return false;
    }
    if state.content_height <= state.viewport_height {
        return false;
    }

    match mouse_event.kind {
        MouseEventKind::ScrollUp => {
            scroll_up_by(state, MOUSE_WHEEL_SCROLL_LINES);
            true
        }
        MouseEventKind::ScrollDown => {
            scroll_down_by(state, MOUSE_WHEEL_SCROLL_LINES);
            true
        }
        _ => false,
    }
}

fn is_recap_mode(state: &AppState) -> bool {
    matches!(state.input_mode, InputMode::Normal) && state.remaining_blocks == 0
}

fn recap_done_key(keybinds: &TuiKeybindsConfig) -> char {
    keybinds.recap_done
}

fn recap_done_action_text(keybinds: &TuiKeybindsConfig) -> String {
    format_key_action(recap_done_key(keybinds), "choose scope")
}

fn recap_action_for_key_code(
    keybinds: &TuiKeybindsConfig,
    key_code: KeyCode,
) -> Option<RecapAction> {
    if matches!(key_code, KeyCode::Esc)
        || matches!(
            keybind_action_for_key_code(keybinds, key_code),
            Some(KeybindAction::Quit)
        )
    {
        return Some(RecapAction::Exit);
    }

    matches!(key_code, KeyCode::Char(ch) if ch == recap_done_key(keybinds))
        .then_some(RecapAction::ReviewSomethingElse)
}

// ... helper functions for actions ...

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn handle_parent(state: &mut AppState) {
    let previous_current = state.navigator.current_id();
    if previous_current == state.navigator.tree.root() {
        return;
    }
    state.navigator.ascend();
    state.scroll_offset = 0;
    set_focus_for_current_node(state, Some(previous_current));
    sync_speed_read_focus(state);
}

fn handle_child(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        let root = state.navigator.tree.root();
        state.root_cursor = state
            .root_cursor
            .filter(|id| state.navigator.is_visible(*id))
            .or_else(|| state.navigator.first_visible_child(root));

        if let Some(target) = state.root_cursor {
            state.navigator.set_current(target);
            state.scroll_offset = 0;
            set_focus_for_current_node(state, None);
            sync_speed_read_focus(state);
        }
    } else {
        let current = state.navigator.current_id();
        if state.navigator.first_visible_child(current).is_none()
            && state.navigator.tree.node(current).children.is_empty()
        {
            let _ = expand_current_node_children(state, current);
        }
        state.navigator.descend();
        state.scroll_offset = 0;
        set_focus_for_current_node(state, None);
        sync_speed_read_focus(state);
    }
}

fn expand_current_node_children(state: &mut AppState, node_id: TreeNodeId) -> bool {
    let node = state.navigator.tree.node(node_id);
    let Some(block) = node.block.clone() else {
        return false;
    };
    let Some(language) = node.language else {
        return false;
    };

    let Ok(sub_split) = sub_splitter::split_result_for_child_navigation(&block, language) else {
        return false;
    };

    let child_blocks = sub_split
        .blocks
        .into_iter()
        .filter(|child| child.kind != BlockKind::Gap)
        .filter(|child| !is_identity_subblock(&block, child))
        .collect::<Vec<_>>();
    if child_blocks.is_empty() {
        return false;
    }

    let previous_remaining = state.reviewable_nodes.len();
    state.reviewable_nodes.remove(&node_id);
    let parent_was_commented = state.commented_nodes.remove(&node_id);
    let parent_was_skipped = state.skipped_nodes.remove(&node_id);

    let inserted_ids = state
        .navigator
        .tree
        .insert_block_children(node_id, child_blocks);
    if inserted_ids.is_empty() {
        return false;
    }

    for child_id in &inserted_ids {
        state.reviewable_nodes.insert(*child_id);
        if parent_was_commented {
            state.commented_nodes.insert(*child_id);
        } else if parent_was_skipped {
            state.skipped_nodes.insert(*child_id);
        }
    }
    state.navigator.reveal_blocks(inserted_ids.iter().copied());
    refresh_review_state_after_refinement(state, previous_remaining);
    state.content_frame_cache.clear();

    true
}

fn is_identity_subblock(parent: &crate::block::Block, child: &crate::block::Block) -> bool {
    child.kind == parent.kind
        && child.start_line == parent.start_line
        && child.end_line == parent.end_line
        && child.hash == parent.hash
}

fn refresh_review_state_after_refinement(state: &mut AppState, previous_remaining: usize) {
    let next_remaining = state.reviewable_nodes.len();
    if next_remaining >= previous_remaining {
        let delta = next_remaining - previous_remaining;
        state.total_blocks = state.total_blocks.saturating_add(delta);
        state.initial_remaining_blocks = state.initial_remaining_blocks.saturating_add(delta);
    } else {
        let delta = previous_remaining - next_remaining;
        state.total_blocks = state.total_blocks.saturating_sub(delta);
        state.initial_remaining_blocks = state.initial_remaining_blocks.saturating_sub(delta);
    }
    state.remaining_blocks = next_remaining;
    state.review_order = ReviewOrder::from_tree(&state.navigator.tree, &state.reviewable_nodes);
}

fn handle_scroll_line_up(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, -1);
    } else {
        scroll_up_by(state, 1);
    }
}

fn handle_scroll_line_down(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, 1);
    } else {
        scroll_down_by(state, 1);
    }
}

fn handle_prev(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        return;
    }

    state.navigator.move_prev();
    state.scroll_offset = 0;
    set_focus_for_current_node(state, None);
    sync_speed_read_focus(state);
}

fn mark_focused_block_temporarily_skipped(state: &mut AppState) {
    let Some(block_id) = state.focus_block else {
        return;
    };
    if state.reviewable_nodes.contains(&block_id) && !state.commented_nodes.contains(&block_id) {
        state.skipped_nodes.insert(block_id);
    }
}

fn handle_next(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        handle_child(state);
        return;
    }

    mark_focused_block_temporarily_skipped(state);
    state.navigator.move_next();
    state.scroll_offset = 0;
    set_focus_for_current_node(state, None);
    sync_speed_read_focus(state);
}

fn handle_scroll_page_up(state: &mut AppState) {
    let scroll_amount = state.viewport_height.saturating_sub(1);
    scroll_up_by(state, scroll_amount);
}

fn handle_scroll_page_down(state: &mut AppState) {
    let scroll_amount = state.viewport_height.saturating_sub(1);
    scroll_down_by(state, scroll_amount);
}

fn scroll_up_by(state: &mut AppState, scroll_amount: u16) {
    state.scroll_offset = state.scroll_offset.saturating_sub(scroll_amount);
}

fn scroll_down_by(state: &mut AppState, scroll_amount: u16) {
    state.scroll_offset = state
        .scroll_offset
        .saturating_add(scroll_amount)
        .min(max_scroll_offset(state));
}

fn max_scroll_offset(state: &AppState) -> u16 {
    state.content_height.saturating_sub(state.viewport_height)
}

fn node_contains_block(tree: &Tree, node_id: TreeNodeId, block_id: TreeNodeId) -> bool {
    tree.ancestors(block_id).contains(&node_id)
}

fn first_focusable_descendant_block(state: &AppState, node_id: TreeNodeId) -> Option<TreeNodeId> {
    let tree = &state.navigator.tree;
    let mut block_ids = state.navigator.visible_descendant_block_ids(node_id);
    block_ids.sort_by(|a, b| {
        let a_node = tree.node(*a);
        let b_node = tree.node(*b);
        let a_start = a_node
            .block
            .as_ref()
            .map(|block| block.start_line)
            .unwrap_or(usize::MAX);
        let b_start = b_node
            .block
            .as_ref()
            .map(|block| block.start_line)
            .unwrap_or(usize::MAX);
        (a_node.path.as_str(), a_start).cmp(&(b_node.path.as_str(), b_start))
    });
    block_ids.into_iter().next()
}

fn focus_block_for_node(
    state: &AppState,
    node_id: TreeNodeId,
    preferred_child: Option<TreeNodeId>,
) -> Option<TreeNodeId> {
    let tree = &state.navigator.tree;
    match tree.node(node_id).kind {
        TreeNodeKind::Root | TreeNodeKind::Directory => None,
        TreeNodeKind::Block => preferred_child
            .filter(|child| {
                matches!(tree.node(*child).kind, TreeNodeKind::Block)
                    && node_contains_block(tree, node_id, *child)
            })
            .or(Some(node_id)),
        TreeNodeKind::File => preferred_child
            .filter(|child| {
                matches!(tree.node(*child).kind, TreeNodeKind::Block)
                    && node_contains_block(tree, node_id, *child)
            })
            .or_else(|| first_focusable_descendant_block(state, node_id)),
    }
}

fn set_focus_for_current_node(state: &mut AppState, preferred_child: Option<TreeNodeId>) {
    let previous_focus = state.focus_block;
    let current = state.navigator.current_id();
    state.focus_block = focus_block_for_node(state, current, preferred_child);
    state.pending_focus_scroll = matches!(
        state.navigator.tree.node(current).kind,
        TreeNodeKind::File | TreeNodeKind::Block
    ) && state.focus_block.is_some();
    if state.focus_block != previous_focus {
        cancel_ai_suggestion(state);
    }
}

fn clear_focus_scroll(state: &mut AppState) {
    let previous_focus = state.focus_block;
    state.focus_block = None;
    state.pending_focus_scroll = false;
    if previous_focus.is_some() {
        cancel_ai_suggestion(state);
    }
}

fn scroll_offset_for_focus_range(
    focus_row_range: &std::ops::Range<usize>,
    viewport_height: u16,
    total_lines: usize,
) -> u16 {
    if viewport_height == 0 || total_lines <= usize::from(viewport_height) {
        return 0;
    }

    let focus_start = focus_row_range.start.min(total_lines.saturating_sub(1));
    let focus_end = focus_row_range
        .end
        .max(focus_start.saturating_add(1))
        .min(total_lines);
    let focus_height = focus_end.saturating_sub(focus_start);
    let viewport_height = usize::from(viewport_height);

    let target = if focus_height > viewport_height {
        focus_start
    } else {
        focus_start.saturating_sub((viewport_height.saturating_sub(focus_height)) / 2)
    };

    let max_scroll = total_lines.saturating_sub(viewport_height);
    usize_to_u16_saturating(target.min(max_scroll))
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    if rect.width == 0 || rect.height == 0 {
        return false;
    }

    let right = rect.x.saturating_add(rect.width);
    let bottom = rect.y.saturating_add(rect.height);
    column >= rect.x && column < right && row >= rect.y && row < bottom
}

fn visible_root_children(state: &AppState) -> Vec<TreeNodeId> {
    let root = state.navigator.tree.root();
    state
        .navigator
        .tree
        .node(root)
        .children
        .iter()
        .copied()
        .filter(|child| state.navigator.is_visible(*child))
        .collect()
}

fn root_listing_prefix_line_count(state: &AppState) -> usize {
    let mut line_count = 2usize;
    let kind_counts = review_metadata::sorted_visible_block_kind_counts(
        &state.navigator.tree,
        state.navigator.visible_nodes(),
    );

    let mut last_parent = "";
    for (kind, _) in kind_counts {
        let parent = review_metadata::parent_kind_label(kind);
        if parent != last_parent {
            if !last_parent.is_empty() {
                line_count = line_count.saturating_add(1);
            }
            line_count = line_count.saturating_add(1);
            last_parent = parent;
        }
        line_count = line_count.saturating_add(1);
    }

    line_count.saturating_add(1)
}

fn root_selected_line_index(state: &AppState, root_children: &[TreeNodeId]) -> Option<usize> {
    let selected_index = state
        .root_cursor
        .and_then(|id| root_children.iter().position(|&child| child == id))?;
    Some(root_listing_prefix_line_count(state).saturating_add(selected_index))
}

fn root_total_line_count(state: &AppState, root_children: &[TreeNodeId]) -> usize {
    root_listing_prefix_line_count(state).saturating_add(root_children.len().max(1))
}

fn scroll_offset_to_keep_line_visible(
    current_scroll_offset: u16,
    viewport_height: u16,
    line_index: usize,
    total_lines: usize,
) -> u16 {
    if viewport_height == 0 || total_lines <= usize::from(viewport_height) {
        return 0;
    }

    let viewport_height = usize::from(viewport_height);
    let current_top = usize::from(current_scroll_offset);
    let next_top = if line_index < current_top {
        line_index
    } else if line_index >= current_top.saturating_add(viewport_height) {
        line_index.saturating_add(1).saturating_sub(viewport_height)
    } else {
        current_top
    };
    let max_scroll = total_lines.saturating_sub(viewport_height);
    usize_to_u16_saturating(next_top.min(max_scroll))
}

fn ensure_root_cursor_visible(state: &mut AppState) {
    let root_children = visible_root_children(state);
    let Some(selected_line_index) = root_selected_line_index(state, &root_children) else {
        if root_total_line_count(state, &root_children) <= usize::from(state.viewport_height) {
            state.scroll_offset = 0;
        }
        return;
    };

    state.scroll_offset = scroll_offset_to_keep_line_visible(
        state.scroll_offset,
        state.viewport_height,
        selected_line_index,
        root_total_line_count(state, &root_children),
    );
}

fn move_root_cursor(state: &mut AppState, offset: isize) {
    let root_children = visible_root_children(state);

    if root_children.is_empty() {
        state.root_cursor = None;
        state.scroll_offset = 0;
        return;
    }

    let current = state
        .root_cursor
        .and_then(|id| root_children.iter().position(|&child| child == id))
        .unwrap_or(0);
    let last_index = root_children.len().saturating_sub(1);
    let next = current
        .checked_add_signed(offset)
        .unwrap_or(if offset.is_negative() { 0 } else { last_index })
        .min(last_index);
    state.root_cursor = root_children.get(next).copied();
    ensure_root_cursor_visible(state);
}

fn handle_action(
    session: &mut TerminalSession,
    context: &TrueflowContext,
    state: &mut AppState,
    verdict: Verdict,
) -> Result<()> {
    let action =
        PendingAction::from_node(&state.navigator.tree, state.navigator.current_id(), verdict);

    if let Some(count) = batch_confirmation_count_for_action(state, &action) {
        state.input_mode = InputMode::ConfirmBatch { action, count };
    } else {
        execute_action(session, context, state, action)?;
    }
    Ok(())
}

fn batch_confirmation_count_for_action(state: &AppState, action: &PendingAction) -> Option<usize> {
    let PendingAction::Batch { node_id, .. } = action else {
        return None;
    };
    let count = state.navigator.count_visible_descendant_blocks(*node_id);
    state.confirm_batch.should_confirm(count).then_some(count)
}

fn handle_note_action(state: &mut AppState) -> Result<()> {
    let action = PendingAction::from_node(
        &state.navigator.tree,
        state.navigator.current_id(),
        Verdict::Comment,
    );
    let draft = current_comment_draft(state);
    cancel_ai_suggestion(state);
    state.input_mode = InputMode::Editing { action };
    state.input_buffer.clear();
    state.input_draft = draft;
    state.input_cursor.reset();
    clear_editing_validation(state);
    Ok(())
}

fn current_comment_draft(state: &AppState) -> Option<String> {
    state
        .ai
        .current_suggestion_sentence()
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
        .map(str::to_string)
}

fn handle_editing_submit(
    session: &mut TerminalSession,
    context: &TrueflowContext,
    state: &mut AppState,
) -> Result<()> {
    handle_editing_submit_with(state, |action, state| {
        execute_action(session, context, state, action)
    })
}

fn handle_editing_submit_with<F>(state: &mut AppState, on_action: F) -> Result<()>
where
    F: FnOnce(PendingAction, &mut AppState) -> Result<()>,
{
    let Some(submit) = editing_submit_decision(&state.input_mode, &state.input_buffer) else {
        return Ok(());
    };

    match submit {
        EditingSubmitDecision::Empty => {
            state.editing_validation = Some(EditingValidation::NoteRequired);
        }
        EditingSubmitDecision::Ready(action) => {
            clear_editing_validation(state);
            if let Some(count) = batch_confirmation_count_for_action(state, &action) {
                state.input_buffer.clear();
                state.input_draft = None;
                state.input_cursor.reset();
                state.input_mode = InputMode::ConfirmBatch { action, count };
            } else {
                on_action(action, state)?;
                state.input_mode = InputMode::Normal;
                state.input_buffer.clear();
                state.input_draft = None;
                state.input_cursor.reset();
            }
        }
    }
    Ok(())
}

fn handle_editing_cancel(state: &mut AppState) {
    clear_editing_validation(state);
    if state.input_buffer.is_empty() {
        state.input_mode = InputMode::Normal;
        state.input_draft = None;
        state.input_cursor.reset();
    } else {
        state.input_buffer.clear();
        state.input_draft = None;
        state.input_cursor.reset();
    }
}

fn handle_confirm_batch(
    session: &mut TerminalSession,
    context: &TrueflowContext,
    state: &mut AppState,
) -> Result<()> {
    let action = match &state.input_mode {
        InputMode::ConfirmBatch { action, .. } => action.clone(),
        _ => return Ok(()),
    };
    state.input_mode = InputMode::Normal;
    state.input_draft = None;
    execute_action(session, context, state, action)
}

fn handle_confirm_cancel(state: &mut AppState) {
    state.input_mode = InputMode::Normal;
    state.input_draft = None;
}

fn execute_action(
    session: &mut TerminalSession,
    context: &TrueflowContext,
    state: &mut AppState,
    action: PendingAction,
) -> Result<()> {
    execute_action_with(state, action, |params| {
        let noninteractive_params = params.clone();
        execute_mark_for_tui(
            mark::terminal_suspend_requirement_from_workdir(),
            move || mark::run_with_noninteractive_signing(context, noninteractive_params),
            || session.suspend(|| mark::run(context, params)),
        )
    })
}

fn execute_mark_for_tui<F, G>(
    suspend_requirement: mark::TerminalSuspendRequirement,
    run_noninteractive: F,
    run_with_terminal_suspend: G,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
    G: FnOnce() -> Result<()>,
{
    match suspend_requirement {
        mark::TerminalSuspendRequirement::NotRequired => run_noninteractive(),
        mark::TerminalSuspendRequirement::Required => match run_noninteractive() {
            Ok(()) => Ok(()),
            Err(error) if mark::is_noninteractive_signing_failure(&error) => {
                run_with_terminal_suspend()
            }
            Err(error) => Err(error),
        },
    }
}

fn execute_action_with<F>(state: &mut AppState, action: PendingAction, run_mark: F) -> Result<()>
where
    F: FnOnce(mark::MarkParams) -> Result<()>,
{
    let (node_id, verdict, note) = match action {
        PendingAction::Single {
            node_id,
            verdict,
            note,
        }
        | PendingAction::Batch {
            node_id,
            verdict,
            note,
        } => (node_id, verdict, note),
    };

    let next_id = if matches!(verdict, Verdict::Comment) {
        Some(node_id)
    } else {
        compute_next_review_target(state, node_id)
    };
    let params = mark_params_for_action(state, node_id, verdict.clone(), note)?;
    cancel_ai_suggestion(state);
    run_mark(params)?;

    let impact = apply_action_locally(state, node_id, &verdict, next_id);
    state.session_recap.record_action(&verdict, impact);
    Ok(())
}

fn mark_params_for_action(
    state: &mut AppState,
    node_id: TreeNodeId,
    verdict: Verdict,
    note: Option<String>,
) -> Result<mark::MarkParams> {
    let node = state.navigator.tree.node(node_id);
    let (fingerprint, target_kind) = fingerprint_and_target_kind_for_node(node);

    // For root/dir, path might be empty or a dir path.
    // For file/block, it's the file path.
    let path_hint = if node.path.is_root() {
        None
    } else {
        Some(node.path.clone())
    };

    let scoped_comment = (matches!(verdict, Verdict::Comment)
        && node_id == state.navigator.current_id())
    .then(|| state.visible_comment_capture.clone())
    .flatten();
    let line_hint = node
        .block
        .as_ref()
        .map(|block| usize_to_u32_saturating(block.start_line))
        .or_else(|| {
            scoped_comment
                .as_ref()
                .map(|capture| capture.scope.start_line)
        });

    let comment_anchor =
        if matches!(verdict, Verdict::Comment) && node_id == state.navigator.current_id() {
            comment_anchor_for_current_action(state, node_id, path_hint.as_ref())?
        } else {
            None
        };

    Ok(mark::MarkParams {
        fingerprint,
        target_kind: Some(target_kind),
        verdict,
        check: ReviewCheck::review(),
        note,
        path: path_hint,
        line: line_hint,
        comment_scope: scoped_comment.as_ref().map(|capture| capture.scope.clone()),
        comment_context: scoped_comment.map(|capture| capture.context),
        comment_anchor,
    })
}

fn comment_anchor_for_current_action(
    state: &mut AppState,
    node_id: TreeNodeId,
    path_hint: Option<&RepoPath>,
) -> Result<Option<CommentAnchor>> {
    let Some(path) = path_hint.cloned() else {
        return Ok(None);
    };
    let Some(revision) = review_scope_revision(state)? else {
        return Ok(None);
    };

    let node = state.navigator.tree.node(node_id);
    let snapshot = ContentNodeSnapshot::from_node(node);
    let content = build_content_lines(
        state,
        &snapshot,
        &UiPalette::default(),
        state.code_rect.height.max(1),
        state.code_rect.width.max(1),
    );
    let Some(selection) = comment_anchor_selection_for_content(
        &content,
        state.scroll_offset,
        state.viewport_height,
        state.content_height,
    ) else {
        return Ok(None);
    };

    Ok(Some(match selection {
        CommentAnchorSelection::Source {
            start_line,
            end_line,
        } => CommentAnchor::Source(SourceCommentAnchor {
            revision,
            path,
            start_line,
            end_line,
        }),
        CommentAnchorSelection::Diff { rows } => CommentAnchor::Diff(DiffCommentAnchor {
            revision,
            path,
            rows,
        }),
    }))
}

fn review_scope_revision(state: &AppState) -> Result<Option<crate::store::CommitId>> {
    match &state.review_scope {
        ScopePreset::Commit { id, .. } => Ok(Some(resolve_review_scope_revision(
            id,
            state.repo_root.as_deref(),
        )?)),
        ScopePreset::RevisionRange { end, .. } => Ok(Some(resolve_review_scope_revision(
            end,
            state.repo_root.as_deref(),
        )?)),
        ScopePreset::All | ScopePreset::MainDiff => {
            Ok(vcs::snapshot_from_workdir().repo_ref_revision)
        }
    }
}

fn resolve_review_scope_revision(
    revision: &str,
    repo_root: Option<&Path>,
) -> Result<crate::store::CommitId> {
    let resolved = match repo_root {
        Some(repo_root) => gix::open(repo_root)
            .with_context(|| format!("failed to open git repository at {}", repo_root.display()))
            .and_then(|repo| vcs::resolve_commit_id_in_repo(&repo, revision)),
        None => vcs::resolve_commit_id_from_workdir(revision),
    };

    match resolved {
        Ok(commit_id) => Ok(commit_id),
        Err(resolve_error) => crate::store::CommitId::new(revision).map_err(|parse_error| {
            anyhow!(
                "review scope revision `{revision}` could not be resolved as a git commit ({resolve_error}) or parsed as a commit id ({parse_error})"
            )
        }),
    }
}

fn fingerprint_and_target_kind_for_node(
    node: &crate::tree::TreeNode,
) -> (String, ReviewTargetKind) {
    match node.kind {
        TreeNodeKind::Root | TreeNodeKind::Directory => {
            (node.hash.to_string(), ReviewTargetKind::Tree)
        }
        TreeNodeKind::File => (node.hash.to_string(), ReviewTargetKind::File),
        TreeNodeKind::Block => (node.hash.to_string(), ReviewTargetKind::Block),
    }
}

fn load_review_state(
    scope: &ScopePreset,
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
) -> Result<CollectedReview> {
    let request = scope.to_review_request()?;
    let query = resolve_review_request(request, filters.clone(), scan_options.clone())?;
    collect_review(&query)
}

fn load_review_summary(
    scope: &ScopePreset,
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
) -> Result<ReviewSummary> {
    Ok(load_review_state(scope, filters, scan_options)?.summary)
}

fn apply_action_locally(
    state: &mut AppState,
    node_id: TreeNodeId,
    verdict: &Verdict,
    next_id: Option<TreeNodeId>,
) -> ActionImpact {
    let block_ids = review_session::action_block_ids(&state.navigator, node_id);
    let affected_blocks = block_ids.len();

    let mut removed_reviewable = 0;
    if matches!(verdict, Verdict::Approved | Verdict::Rejected) {
        for &block_id in &block_ids {
            state.commented_nodes.remove(&block_id);
            state.skipped_nodes.remove(&block_id);
            if state.navigator.remove_visible(block_id) && state.reviewable_nodes.remove(&block_id)
            {
                removed_reviewable += 1;
            }
        }
        state.remaining_blocks = state.remaining_blocks.saturating_sub(removed_reviewable);
    }

    state.navigator.prune_visible_to_block_ancestors();

    if matches!(verdict, Verdict::Comment) {
        for &block_id in &block_ids {
            if state.reviewable_nodes.contains(&block_id) {
                state.skipped_nodes.remove(&block_id);
                state.commented_nodes.insert(block_id);
            }
        }
        sync_speed_read_focus(state);
        return ActionImpact {
            affected_blocks,
            removed_reviewable,
        };
    }

    if let Some(node_id) = next_id {
        state.navigator.set_current(node_id);
        state.scroll_offset = 0;
        set_focus_for_current_node(state, None);
    } else {
        state.navigator.jump_root();
        state.scroll_offset = 0;
        clear_focus_scroll(state);
    }
    sync_speed_read_focus(state);

    ActionImpact {
        affected_blocks,
        removed_reviewable,
    }
}

fn compute_next_review_target(state: &AppState, node_id: TreeNodeId) -> Option<TreeNodeId> {
    review_session::next_review_target(
        &state.navigator,
        &state.review_order,
        &state.reviewable_nodes,
        node_id,
    )
}

fn compute_manual_next_review_target(state: &AppState) -> Option<TreeNodeId> {
    let current = state.navigator.current_id();
    let tree = &state.navigator.tree;
    if current == tree.root() {
        return None;
    }

    match tree.node(current).kind {
        TreeNodeKind::Block => state
            .review_order
            .next_remaining_after(ReviewAnchor::Block(current), &state.reviewable_nodes),
        _ => first_focusable_descendant_block(state, current)
            .filter(|node_id| state.reviewable_nodes.contains(node_id))
            .or_else(|| compute_next_review_target(state, current)),
    }
}

fn handle_advance_review_target(state: &mut AppState) {
    let next_id = compute_manual_next_review_target(state);
    if next_id != state.focus_block {
        mark_focused_block_temporarily_skipped(state);
    }
    let Some(next_id) = next_id else {
        return;
    };

    state.navigator.set_current(next_id);
    state.scroll_offset = 0;
    set_focus_for_current_node(state, None);
    sync_speed_read_focus(state);
}

fn detect_repo_name(context: &TrueflowContext) -> String {
    if let Ok(path) = context.trueflow_dir() {
        // Try to get parent of .trueflow
        if let Some(parent) = path.parent() {
            return parent
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string());
        }
    }
    "repo".to_string()
}

// --- UI Rendering ---

fn render_scope_selector(
    frame: &mut Frame,
    selector: &ScopeSelector,
    keybinds: &TuiKeybindsConfig,
) {
    let palette = UiPalette::default();
    let area = frame.area();

    frame.render_widget(
        UiBlock::default().style(Style::default().bg(palette.bg)),
        area,
    );

    let block = UiBlock::default()
        .title(" Review scope ")
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));

    let popup_area = centered_rect(area, 70, 60);
    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    let layout = scope_selector_content_layout(inner_area);

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Select review scope",
        Style::default()
            .fg(palette.fg)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    let visible_options = visible_scope_selector_option_range(
        selector.options.len(),
        selector.selected,
        usize::from(layout.content.height).saturating_sub(2),
    );
    for idx in visible_options {
        let option = &selector.options[idx];
        let prefix = if idx == selector.selected { "> " } else { "  " };
        let style = if idx == selector.selected {
            Style::default()
                .fg(palette.fg)
                .bg(palette.meta_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.dim).bg(palette.bg)
        };
        let status = option.status.label();
        let row =
            format_scope_selector_option_text(prefix, &option.label, &status, layout.content.width);
        lines.push(Line::from(Span::styled(row, style)));
    }

    if layout.content.height > 0 && layout.content.width > 0 {
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false }),
            layout.content,
        );
    }

    if layout.hints.height > 0 && layout.hints.width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                scope_selector_hint_text(keybinds),
                Style::default().fg(palette.dim).bg(palette.bg),
            )))
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false }),
            layout.hints,
        );
    }
}

fn visible_scope_selector_option_range(
    option_count: usize,
    selected: usize,
    max_visible_options: usize,
) -> std::ops::Range<usize> {
    if option_count == 0 || max_visible_options == 0 {
        return 0..0;
    }

    let visible_count = max_visible_options.min(option_count);
    let selected = selected.min(option_count - 1);
    let end = selected
        .saturating_add(1)
        .max(visible_count)
        .min(option_count);
    let start = end - visible_count;
    start..end
}

fn format_scope_selector_option_text(
    prefix: &str,
    label: &str,
    status: &str,
    width: u16,
) -> String {
    let max_width = usize::from(width);
    if max_width == 0 {
        return String::new();
    }
    if status.is_empty() {
        return truncate_text_to_width(&format!("{prefix}{label}"), max_width);
    }

    let status_width = UnicodeWidthStr::width(status);
    if status_width >= max_width {
        return truncate_text_to_width(status, max_width);
    }

    let left_width_budget = max_width.saturating_sub(status_width + 1);
    let left = truncate_text_to_width(&format!("{prefix}{label}"), left_width_budget);
    let left_width = UnicodeWidthStr::width(left.as_str());
    let gap_width = max_width.saturating_sub(left_width + status_width);
    format!("{left}{}{status}", " ".repeat(gap_width))
}

fn truncate_text_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut truncated = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width + 1 > max_width {
            break;
        }
        truncated.push(ch);
        width += ch_width;
    }
    truncated.push('…');
    truncated
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScopeSelectorContentLayout {
    content: Rect,
    hints: Rect,
}

fn scope_selector_content_layout(inner: Rect) -> ScopeSelectorContentLayout {
    let zero = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 0,
    };
    if inner.height == 0 || inner.width == 0 {
        return ScopeSelectorContentLayout {
            content: zero,
            hints: zero,
        };
    }
    if inner.height == 1 {
        return ScopeSelectorContentLayout {
            content: zero,
            hints: inner,
        };
    }

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    ScopeSelectorContentLayout {
        content: chunks[0],
        hints: chunks[1],
    }
}

fn ui(frame: &mut Frame, state: &mut AppState) {
    let palette = UiPalette::default();
    let area = frame.area();
    state.code_rect = Rect::default();

    // 1. Background
    frame.render_widget(
        UiBlock::default().style(Style::default().bg(palette.bg)),
        area,
    );

    // 2. Main Layout
    let layout = Layout::vertical([
        Constraint::Min(0),    // Content
        Constraint::Length(1), // Footer
    ])
    .split(area);

    let content_area = layout[0];
    let footer_area = layout[1];

    if is_recap_mode(state) {
        render_recap_view(frame, state, content_area, &palette);
        render_recap_footer(frame, footer_area, &palette, &state.keybinds);
    } else {
        render_active_node(frame, state, content_area, &palette);
        render_footer(frame, state, footer_area, &palette);
    }

    // 3. Input Overlay
    if matches!(
        state.input_mode,
        InputMode::Editing { .. } | InputMode::ConfirmBatch { .. }
    ) {
        render_input_overlay(frame, state, area, &palette);
    }
}

fn render_active_node(frame: &mut Frame, state: &mut AppState, area: Rect, palette: &UiPalette) {
    let node = state.navigator.tree.node(state.navigator.current_id());

    let header_lines = build_header_lines(node, state, palette);
    let mode_banner_line = build_mode_banner_line(state, palette);
    let ai_hint_text = state.ai.hint_line_text();
    let ai_hint_width = focus_code_width(area);
    let ai_hint_lines = ai_hint_text
        .as_deref()
        .map(|text| ai_hint_wrapped_line_count(text, ai_hint_width))
        .unwrap_or_default();

    let focus_layout = compute_focus_layout(
        area,
        usize_to_u16_saturating(header_lines.len()),
        ai_hint_lines,
    );
    let ui_mode = current_ui_mode(state);
    let actions_lines = build_action_lines(
        focus_layout.actions.width,
        ui_mode,
        &state.keybinds,
        palette,
    );
    let node_snapshot = ContentNodeSnapshot::from_node(node);
    let full_code_width = focus_layout.code.width.max(1);
    let mut render_code_width = full_code_width;
    let mut content = build_render_content(
        state,
        &node_snapshot,
        palette,
        focus_layout.code.height,
        render_code_width,
    );
    let mut display_metrics =
        display_metrics_for_content(state, &node_snapshot, &content, render_code_width);

    loop {
        let show_scrollbar = display_metrics.0 > usize::from(focus_layout.code.height);
        let desired_code_width = if show_scrollbar {
            full_code_width.saturating_sub(1).max(1)
        } else {
            full_code_width
        };
        if desired_code_width == render_code_width {
            break;
        }

        render_code_width = desired_code_width;
        content = build_render_content(
            state,
            &node_snapshot,
            palette,
            focus_layout.code.height,
            render_code_width,
        );
        display_metrics =
            display_metrics_for_content(state, &node_snapshot, &content, render_code_width);
    }

    let (display_total_lines, display_focus_row_range) = display_metrics;
    state.content_height = usize_to_u16_saturating(display_total_lines);
    state.viewport_height = focus_layout.code.height;
    state.code_rect = focus_layout.code;
    if state.pending_focus_scroll {
        state.scroll_offset = display_focus_row_range
            .as_ref()
            .map(|range| {
                scroll_offset_for_focus_range(range, state.viewport_height, display_total_lines)
            })
            .unwrap_or(0);
        state.pending_focus_scroll = false;
    }
    state.scroll_offset = state
        .scroll_offset
        .min(state.content_height.saturating_sub(state.viewport_height));
    state.visible_comment_capture = visible_comment_capture_for_content(
        &content,
        state.scroll_offset,
        state.viewport_height,
        state.content_height,
    );

    let meta_block = UiBlock::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(palette.meta_border).bg(palette.meta_bg))
        .style(Style::default().bg(palette.meta_bg).fg(palette.fg));

    frame.render_widget(
        Paragraph::new(header_lines)
            .block(meta_block)
            .alignment(Alignment::Left),
        focus_layout.meta,
    );

    let show_scrollbar = state.content_height > state.viewport_height;
    let code_content_area = if show_scrollbar {
        Rect {
            width: render_code_width,
            ..focus_layout.code
        }
    } else {
        focus_layout.code
    };

    frame.render_widget(
        Paragraph::new(content.lines)
            .block(UiBlock::default().style(Style::default().bg(palette.code_bg)))
            .scroll((state.scroll_offset, 0))
            .wrap(Wrap { trim: false }),
        code_content_area,
    );

    if show_scrollbar {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state = ScrollbarState::new(
            state
                .content_height
                .saturating_sub(state.viewport_height)
                .into(),
        )
        .position(state.scroll_offset.into());

        frame.render_stateful_widget(scrollbar, focus_layout.code, &mut scrollbar_state);
    }

    let actions_paragraph = Paragraph::new(actions_lines)
        .alignment(Alignment::Center)
        .style(Style::default().bg(palette.bg));

    frame.render_widget(actions_paragraph, focus_layout.actions);

    frame.render_widget(
        Paragraph::new(mode_banner_line).alignment(Alignment::Center),
        focus_layout.mode,
    );

    if let Some(ai_hint_text) = ai_hint_text {
        frame.render_widget(
            Paragraph::new(build_ai_hint_lines_from_text(
                &ai_hint_text,
                palette,
                focus_layout.ai_hint.width,
            ))
            .alignment(Alignment::Center)
            .style(Style::default().bg(palette.bg)),
            focus_layout.ai_hint,
        );
    }
}

fn build_render_content(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    code_height: u16,
    code_width: u16,
) -> BuiltContent {
    if let Some((lines, total_lines)) = build_speed_read_lines(state, node.id, palette, code_width)
    {
        BuiltContent {
            lines,
            total_lines,
            focus_row_range: None,
            comment_rows: None,
        }
    } else {
        build_content_lines_with_frame_cache(state, node, palette, code_height, code_width)
    }
}

fn display_metrics_for_content(
    state: &AppState,
    node: &ContentNodeSnapshot,
    content: &BuiltContent,
    code_width: u16,
) -> (usize, Option<std::ops::Range<usize>>) {
    if matches!(state.view_mode, ViewMode::Source)
        && matches!(node.kind, TreeNodeKind::File | TreeNodeKind::Block)
    {
        wrapped_display_metrics_for_lines(
            &content.lines,
            content.focus_row_range.as_ref(),
            code_width,
        )
    } else {
        (content.total_lines, content.focus_row_range.clone())
    }
}

fn wrapped_display_metrics_for_lines(
    lines: &[Line<'static>],
    focus_line_range: Option<&std::ops::Range<usize>>,
    width: u16,
) -> (usize, Option<std::ops::Range<usize>>) {
    let display_row_prefixes = wrapped_display_row_prefixes(lines, width);
    let total_rows = *display_row_prefixes.last().unwrap_or(&0);

    let focus_row_range = focus_line_range.and_then(|range| {
        let start = range.start.min(lines.len());
        let end = range.end.max(start).min(lines.len());
        (start < end).then_some(display_row_prefixes[start]..display_row_prefixes[end])
    });

    (total_rows, focus_row_range)
}

fn wrapped_display_row_prefixes(lines: &[Line<'static>], width: u16) -> Vec<usize> {
    let wrap_width = usize::from(width.max(1));
    let mut display_row_prefixes = Vec::with_capacity(lines.len().saturating_add(1));
    let mut total_rows = 0usize;
    display_row_prefixes.push(total_rows);

    for line in lines {
        let line_text = line.to_string();
        let line_width = UnicodeWidthStr::width(line_text.as_str()).max(1);
        total_rows = total_rows.saturating_add(line_width.div_ceil(wrap_width));
        display_row_prefixes.push(total_rows);
    }

    display_row_prefixes
}

fn visible_comment_capture_for_content(
    content: &BuiltContent,
    scroll_offset: u16,
    viewport_height: u16,
    content_height: u16,
) -> Option<VisibleCommentCapture> {
    if content_height <= viewport_height {
        return None;
    }

    let selected_rows = selected_comment_rows(
        content,
        scroll_offset,
        viewport_height,
        content_height,
        CommentRowSelectionMode::VisibleOnly,
    )?;
    let (start_line, end_line) = selected_scope_lines(&selected_rows)?;

    Some(VisibleCommentCapture {
        scope: crate::store::CommentScope {
            start_line,
            end_line,
        },
        context: selected_rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentRowSelectionMode {
    VisibleOnly,
    VisibleOrAll,
}

fn comment_anchor_selection_for_content(
    content: &BuiltContent,
    scroll_offset: u16,
    viewport_height: u16,
    content_height: u16,
) -> Option<CommentAnchorSelection> {
    let selected_rows = selected_comment_rows(
        content,
        scroll_offset,
        viewport_height,
        content_height,
        CommentRowSelectionMode::VisibleOrAll,
    )?;

    let source_scope = selected_source_scope(&selected_rows);
    if let Some((start_line, end_line)) = source_scope {
        return Some(CommentAnchorSelection::Source {
            start_line,
            end_line,
        });
    }

    let diff_rows = selected_rows
        .iter()
        .map(|row| match &row.anchor {
            CommentAnchorRowCapture::DiffRow { row } => Some(row.clone()),
            CommentAnchorRowCapture::SourceLine { .. } => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CommentAnchorSelection::Diff { rows: diff_rows })
}

fn selected_comment_rows(
    content: &BuiltContent,
    scroll_offset: u16,
    viewport_height: u16,
    content_height: u16,
    mode: CommentRowSelectionMode,
) -> Option<Vec<CommentContextRow>> {
    let comment_rows = content.comment_rows.as_ref()?;
    let selected_range = if matches!(mode, CommentRowSelectionMode::VisibleOrAll)
        && content_height <= viewport_height
    {
        0..usize::from(content_height)
    } else {
        let visible_start = usize::from(scroll_offset);
        let visible_end = visible_start
            .saturating_add(usize::from(viewport_height))
            .min(usize::from(content_height));
        visible_start..visible_end
    };

    let selected_rows = comment_rows
        .iter()
        .filter(|row| {
            row.display_row_range.start < selected_range.end
                && row.display_row_range.end > selected_range.start
        })
        .cloned()
        .collect::<Vec<_>>();
    (!selected_rows.is_empty()).then_some(selected_rows)
}

fn selected_scope_lines(selected_rows: &[CommentContextRow]) -> Option<(u32, u32)> {
    let mut line_indices = selected_rows
        .iter()
        .map(|row| row.scope_line_index)
        .collect::<Vec<_>>();
    line_indices.sort_unstable();
    let start_line = *line_indices.first()?;
    let end_line = line_indices.last()?.saturating_add(1);
    Some((
        usize_to_u32_saturating(start_line),
        usize_to_u32_saturating(end_line),
    ))
}

fn selected_source_scope(selected_rows: &[CommentContextRow]) -> Option<(u32, u32)> {
    let mut line_indices = selected_rows
        .iter()
        .map(|row| match row.anchor {
            CommentAnchorRowCapture::SourceLine { line_index } => Some(line_index),
            CommentAnchorRowCapture::DiffRow { .. } => None,
        })
        .collect::<Option<Vec<_>>>()?;
    line_indices.sort_unstable();
    let start_line = *line_indices.first()?;
    let end_line = line_indices.last()?.saturating_add(1);
    Some((
        usize_to_u32_saturating(start_line),
        usize_to_u32_saturating(end_line),
    ))
}

fn comment_anchor_diff_line_kind(kind: vcs::DiffLineKind) -> CommentAnchorDiffLineKind {
    match kind {
        vcs::DiffLineKind::Context => CommentAnchorDiffLineKind::Context,
        vcs::DiffLineKind::Added => CommentAnchorDiffLineKind::Added,
        vcs::DiffLineKind::Removed => CommentAnchorDiffLineKind::Removed,
    }
}

fn build_speed_read_lines(
    state: &AppState,
    node_id: TreeNodeId,
    palette: &UiPalette,
    width: u16,
) -> Option<(Vec<Line<'static>>, usize)> {
    let mode = state.speed_read.active_for(node_id)?;

    let width = usize::from(width.max(1));
    let current_phrase = mode
        .model
        .phrases
        .get(mode.model.cursor)
        .map(|phrase| phrase.text.as_str())
        .unwrap_or("(No words)");
    let previous_phrase = mode
        .model
        .cursor
        .checked_sub(1)
        .and_then(|index| mode.model.phrases.get(index))
        .map(|phrase| phrase.text.as_str())
        .unwrap_or("");
    let next_phrase = mode
        .model
        .phrases
        .get(mode.model.cursor.saturating_add(1))
        .map(|phrase| phrase.text.as_str())
        .unwrap_or("");

    let status = format!(
        "Speed {}  WPM:{}  Chunk:{}  {}/{}",
        match mode.model.playback {
            PlaybackState::Paused => "paused",
            PlaybackState::Playing => "playing",
        },
        mode.model.settings.wpm,
        mode.model.settings.chunk_words,
        mode.model.cursor.saturating_add(1),
        mode.model.phrases.len(),
    );

    let mut lines = vec![
        Line::from(Span::styled(
            center_text(previous_phrase, width),
            Style::default().fg(palette.dim).bg(palette.code_bg),
        )),
        Line::from(""),
        Line::from(Span::styled(
            center_text(current_phrase, width),
            if state.speed_read.config().show_orp_highlight {
                Style::default()
                    .fg(palette.fg)
                    .bg(palette.code_bg)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(palette.fg)
                    .bg(palette.code_bg)
                    .add_modifier(Modifier::BOLD)
            },
        )),
        Line::from(Span::styled(
            center_text("^", width),
            Style::default().fg(palette.add).bg(palette.code_bg),
        )),
        Line::from(""),
        Line::from(Span::styled(
            center_text(next_phrase, width),
            Style::default().fg(palette.dim).bg(palette.code_bg),
        )),
        Line::from(""),
        Line::from(Span::styled(
            status,
            Style::default().fg(palette.dim).bg(palette.code_bg),
        )),
    ];

    if mode.show_prose_optimization_hint {
        lines.push(Line::from(Span::styled(
            "Speed mode is prose-optimized; code accuracy may be better in source/diff mode.",
            Style::default().fg(palette.dim).bg(palette.code_bg),
        )));
    }

    let line_count = lines.len();
    Some((lines, line_count))
}

fn center_text(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let padding = (width - len) / 2;
    format!("{}{}", " ".repeat(padding), text)
}

fn content_frame_cache_key(
    node_id: TreeNodeId,
    focus_block: Option<TreeNodeId>,
    node_kind: TreeNodeKind,
    view_mode: ViewMode,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    code_height: u16,
    code_width: u16,
) -> Option<ContentFrameCacheKey> {
    if !is_content_kind_cacheable(node_kind) {
        return None;
    }

    let variant = match node_kind {
        TreeNodeKind::File => match view_mode {
            ViewMode::Diff => ContentFrameCacheVariant::FileDiff { code_width },
            ViewMode::Source => ContentFrameCacheVariant::FileSource,
        },
        TreeNodeKind::Block => match view_mode {
            ViewMode::Diff => ContentFrameCacheVariant::BlockDiff {
                focus_mode: block_diff_focus_mode,
                code_width,
            },
            ViewMode::Source => ContentFrameCacheVariant::BlockSource { code_height },
        },
        TreeNodeKind::Directory | TreeNodeKind::Root => return None,
    };

    Some(ContentFrameCacheKey {
        node_id,
        focus_block,
        variant,
    })
}

fn is_content_kind_cacheable(kind: TreeNodeKind) -> bool {
    matches!(kind, TreeNodeKind::Block | TreeNodeKind::File)
}

fn build_content_lines_with_frame_cache(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    code_height: u16,
    code_width: u16,
) -> BuiltContent {
    let key = content_frame_cache_key(
        node.id,
        state.focus_block,
        node.kind,
        state.view_mode,
        state.block_diff_focus_mode,
        code_height,
        code_width,
    );

    if let Some(key) = key {
        if let Some(cached) = state.content_frame_cache.get(&key) {
            return BuiltContent {
                lines: cached.lines.clone(),
                total_lines: cached.total_lines,
                focus_row_range: cached.focus_row_range.clone(),
                comment_rows: cached.comment_rows.clone(),
            };
        }

        let content = build_content_lines(state, node, palette, code_height, code_width);
        state.content_frame_cache.insert(
            key,
            ContentFrameCacheEntry {
                lines: content.lines.clone(),
                total_lines: content.total_lines,
                focus_row_range: content.focus_row_range.clone(),
                comment_rows: content.comment_rows.clone(),
            },
        );
        return content;
    }

    build_content_lines(state, node, palette, code_height, code_width)
}

fn build_header_lines(
    node: &crate::tree::TreeNode,
    state: &AppState,
    palette: &UiPalette,
) -> Vec<Line<'static>> {
    let mut header_texts = block_header_lines(&state.navigator.tree, node.id)
        .unwrap_or_else(|| vec![compact_header_text(node, state)]);
    if let Some(change_kind) = header_change_kind_for_node(node, state) {
        if node.kind == TreeNodeKind::Block && header_texts.len() > 1 {
            header_texts[1] = prefix_arrow_header_change(&header_texts[1], change_kind);
        } else if let Some(header_text) = header_texts.first_mut() {
            *header_text = format!("{} · {header_text}", change_kind.label());
        }
    }

    header_texts
        .into_iter()
        .enumerate()
        .map(|(index, text)| format_header_row(&text, palette, index == 0))
        .collect()
}

fn compact_header_text(node: &crate::tree::TreeNode, state: &AppState) -> String {
    match node.kind {
        TreeNodeKind::Root => format!("Repository {}", state.repo_name),
        TreeNodeKind::Directory => format!("Directory {}/", node.name),
        TreeNodeKind::File => compact_file_header_text(node),
        TreeNodeKind::Block => compact_block_header_text(&state.navigator.tree, node.id)
            .unwrap_or_else(|| format!("Block {}", node.name)),
    }
}

fn compact_file_header_text(node: &crate::tree::TreeNode) -> String {
    if node.path.is_root() {
        format!("File {}", node.name)
    } else {
        format!("File {}", node.path.as_str())
    }
}

fn compact_block_header_text(tree: &Tree, node_id: TreeNodeId) -> Option<String> {
    let node = tree.node(node_id);
    let block = node.block.as_ref()?;
    let mut segments = vec![block_header_segment(block)];
    let mut file_segment = None;

    for ancestor_id in tree.ancestors(node_id).into_iter().skip(1) {
        let ancestor = tree.node(ancestor_id);
        match ancestor.kind {
            TreeNodeKind::Block if tree.is_container_block(ancestor_id) => {
                if let Some(block) = ancestor.block.as_ref() {
                    segments.push(block_header_segment(block));
                }
            }
            TreeNodeKind::File => {
                file_segment = Some(compact_file_header_text(ancestor));
            }
            TreeNodeKind::Root | TreeNodeKind::Directory | TreeNodeKind::Block => {}
        }
    }

    if let Some(file_segment) = file_segment {
        segments.push(file_segment);
    }

    Some(segments.join(" @ "))
}

fn block_header_lines(tree: &Tree, node_id: TreeNodeId) -> Option<Vec<String>> {
    let node = tree.node(node_id);
    if node.kind != TreeNodeKind::Block {
        return None;
    }

    let block = node.block.as_ref()?;
    let file_path = block_file_path(tree, node_id)?;
    let mut lines = vec![
        file_path,
        format!(
            "  -> {} (hash={})",
            raw_block_header_segment(block),
            short_tree_hash(&block.hash)
        ),
    ];
    lines.extend(subblock_tree_lines(tree, node_id));
    Some(lines)
}

fn prefix_arrow_header_change(line: &str, change_kind: HeaderChangeKind) -> String {
    let Some(rest) = line.strip_prefix("  -> ") else {
        return format!("{} · {line}", change_kind.label());
    };
    format!("  -> {} · {rest}", change_kind.label())
}

fn block_file_path(tree: &Tree, node_id: TreeNodeId) -> Option<String> {
    tree.ancestors(node_id)
        .into_iter()
        .skip(1)
        .find_map(|ancestor_id| {
            let ancestor = tree.node(ancestor_id);
            (ancestor.kind == TreeNodeKind::File).then(|| ancestor.path.as_str().to_string())
        })
}

fn subblock_tree_lines(tree: &Tree, node_id: TreeNodeId) -> Vec<String> {
    let children = block_child_ids(tree, node_id);
    let mut lines = Vec::new();
    append_subblock_tree_lines(tree, &children, &mut lines);
    lines
}

fn append_subblock_tree_lines(tree: &Tree, children: &[TreeNodeId], lines: &mut Vec<String>) {
    let mut stack = children
        .iter()
        .enumerate()
        .rev()
        .map(|(index, child_id)| (*child_id, String::new(), index + 1 == children.len()))
        .collect::<Vec<_>>();

    while let Some((child_id, prefix, is_last)) = stack.pop() {
        let child = tree.node(child_id);
        let connector = if is_last { "└─" } else { "├─" };
        let label = child
            .block
            .as_ref()
            .map(raw_block_header_segment)
            .unwrap_or_else(|| child.name.clone());
        lines.push(format!("     {prefix}{connector} {label}"));

        let nested_children = block_child_ids(tree, child_id);
        let nested_prefix = if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };

        stack.extend(
            nested_children
                .iter()
                .enumerate()
                .rev()
                .map(|(index, nested_child_id)| {
                    (
                        *nested_child_id,
                        nested_prefix.clone(),
                        index + 1 == nested_children.len(),
                    )
                }),
        );
    }
}

fn block_child_ids(tree: &Tree, node_id: TreeNodeId) -> Vec<TreeNodeId> {
    tree.node(node_id)
        .children
        .iter()
        .copied()
        .filter(|child_id| tree.node(*child_id).kind == TreeNodeKind::Block)
        .collect()
}

fn raw_block_header_segment(block: &crate::block::Block) -> String {
    let kind = block.kind.as_str();
    match review_metadata::semantic_block_identifier(block) {
        Some(identifier) => format!("{kind} {identifier}"),
        None => kind.to_string(),
    }
}

fn short_tree_hash(hash: &crate::hashing::TreeHash) -> String {
    hash.as_str().chars().take(8).collect()
}

fn block_header_segment(block: &crate::block::Block) -> String {
    let kind = header_kind_label(block.kind);
    match review_metadata::semantic_block_identifier(block) {
        Some(identifier) => format!("{kind} {identifier}"),
        None => kind,
    }
}

fn header_kind_label(kind: BlockKind) -> String {
    let raw = kind.as_str();
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return "Block".to_string();
    };
    let mut out = first.to_uppercase().collect::<String>();
    out.push_str(chars.as_str());
    out
}

fn build_mode_banner_line(state: &AppState, palette: &UiPalette) -> Line<'static> {
    Line::from(Span::styled(
        mode_banner_label(current_ui_mode(state)).to_string(),
        Style::default()
            .fg(palette.fg)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    ))
}

#[cfg(test)]
fn build_ai_hint_line(state: &AppState, palette: &UiPalette) -> Option<Line<'static>> {
    state
        .ai
        .hint_line_text()
        .map(|text| styled_ai_hint_line(text, palette))
}

#[cfg(test)]
fn build_ai_hint_lines(
    state: &AppState,
    palette: &UiPalette,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    state
        .ai
        .hint_line_text()
        .map(|text| build_ai_hint_lines_from_text(&text, palette, width))
}

fn build_ai_hint_lines_from_text(
    text: &str,
    palette: &UiPalette,
    width: u16,
) -> Vec<Line<'static>> {
    word_wrapped_text_to_width(text, usize::from(width.max(1)))
        .into_iter()
        .map(|line| styled_ai_hint_line(line, palette))
        .collect()
}

fn styled_ai_hint_line(text: String, palette: &UiPalette) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(palette.fg)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    ))
}

fn ai_hint_wrapped_line_count(text: &str, width: u16) -> u16 {
    usize_to_u16_saturating(word_wrapped_text_to_width(text, usize::from(width.max(1))).len())
}

fn word_wrapped_text_to_width(text: &str, width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| word_wrap_line_to_width(line, width.max(1)))
        .collect()
}

fn word_wrap_line_to_width(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut wrapped = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in line.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        if current.is_empty() {
            push_word_wrapped(&mut wrapped, &mut current, &mut current_width, word, width);
            continue;
        }

        if current_width.saturating_add(1).saturating_add(word_width) <= width {
            current.push(' ');
            current.push_str(word);
            current_width = current_width.saturating_add(1).saturating_add(word_width);
        } else {
            wrapped.push(std::mem::take(&mut current));
            current_width = 0;
            push_word_wrapped(&mut wrapped, &mut current, &mut current_width, word, width);
        }
    }

    if !current.is_empty() || wrapped.is_empty() {
        wrapped.push(current);
    }
    wrapped
}

fn push_word_wrapped(
    wrapped: &mut Vec<String>,
    current: &mut String,
    current_width: &mut usize,
    word: &str,
    width: usize,
) {
    for segment in hard_wrap_word_to_width(word, width) {
        if segment.width >= width {
            wrapped.push(segment.text);
            *current = String::new();
            *current_width = 0;
        } else {
            *current = segment.text;
            *current_width = segment.width;
        }
    }
}

struct WrappedWordSegment {
    text: String,
    width: usize,
}

fn hard_wrap_word_to_width(word: &str, width: usize) -> Vec<WrappedWordSegment> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width > 0 && current_width.saturating_add(ch_width) > width {
            out.push(WrappedWordSegment {
                text: std::mem::take(&mut current),
                width: current_width,
            });
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(ch_width);
    }

    if !current.is_empty() || out.is_empty() {
        out.push(WrappedWordSegment {
            text: current,
            width: current_width,
        });
    }
    out
}

fn should_show_change_metadata(state: &AppState) -> bool {
    !matches!(state.review_scope, ScopePreset::All)
}

fn header_change_kind_for_node(
    node: &crate::tree::TreeNode,
    state: &AppState,
) -> Option<HeaderChangeKind> {
    if !should_show_change_metadata(state) {
        return None;
    }

    match node.kind {
        TreeNodeKind::File => Some(
            state
                .file_change_kinds
                .get(&node.id)
                .copied()
                .map(HeaderChangeKind::File)
                .unwrap_or(HeaderChangeKind::Unknown),
        ),
        TreeNodeKind::Block => Some(
            state
                .block_change_kinds
                .get(&node.id)
                .copied()
                .map(HeaderChangeKind::Block)
                .unwrap_or(HeaderChangeKind::Unknown),
        ),
        TreeNodeKind::Root | TreeNodeKind::Directory => None,
    }
}

/// UI mode precedence matters here: recap overrides everything, then speed read
/// for the current block, then root navigation, then diff/source review.
fn current_ui_mode(state: &AppState) -> UiMode {
    if is_recap_mode(state) {
        UiMode::Recap
    } else if state.speed_read.is_active_for(state.navigator.current_id()) {
        UiMode::SpeedRead
    } else if state.navigator.current_id() == state.navigator.tree.root() {
        UiMode::Navigation
    } else {
        match state.view_mode {
            ViewMode::Diff => UiMode::DiffReview,
            ViewMode::Source => UiMode::SourceReview,
        }
    }
}

fn mode_banner_label(mode: UiMode) -> &'static str {
    match mode {
        UiMode::Navigation => "Navigation Mode",
        UiMode::DiffReview => "Diff Mode",
        UiMode::SourceReview => "Source Mode",
        UiMode::SpeedRead => "Speed Read Mode",
        UiMode::Recap => "Recap Mode",
    }
}

fn format_header_row(text: &str, palette: &UiPalette, bold: bool) -> Line<'static> {
    let style = if bold {
        Style::default()
            .fg(palette.fg)
            .bg(palette.meta_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.dim).bg(palette.meta_bg)
    };
    Line::from(Span::styled(text.to_string(), style))
}

fn scope_selector_hint_text(keybinds: &TuiKeybindsConfig) -> String {
    format!(
        "[Enter] select  [{}/{}] move  [{}] quit",
        keybinds.scroll_down, keybinds.scroll_up, keybinds.quit
    )
}

fn build_action_lines(
    width: u16,
    mode: UiMode,
    keybinds: &TuiKeybindsConfig,
    palette: &UiPalette,
) -> Vec<Line<'static>> {
    let approve_action = format_key_action(keybinds.approve, "approve");
    let note_action = format_key_action(keybinds.note, note_action_label(keybinds.note));
    let mode_action = format_key_action(keybinds.toggle_view, "mode");
    let quit_action = format_key_action(keybinds.quit, "quit");
    let phrases = match mode {
        UiMode::Navigation => vec![
            format!("[{}/{}]move", keybinds.scroll_down, keybinds.scroll_up),
            format!("[{}/{}/Enter]open", keybinds.next, keybinds.child),
            format!("[{}/{}]back", keybinds.prev, keybinds.parent),
            approve_action,
            note_action,
            quit_action,
        ],
        UiMode::DiffReview | UiMode::SourceReview => vec![
            approve_action,
            note_action,
            mode_action,
            format_key_action(keybinds.parent, "parent"),
            format_key_action(keybinds.child, "child"),
            "[Space]advance".to_string(),
            "[PgUp/PgDown]".to_string(),
            format_key_action(keybinds.prev, "prev"),
            format_key_action(keybinds.next, "next"),
            format_key_action(keybinds.scroll_down, "down"),
            format_key_action(keybinds.scroll_up, "up"),
            quit_action,
        ],
        UiMode::SpeedRead => vec![
            "[Space]play/pause".to_string(),
            "[j]prev".to_string(),
            "[l]next".to_string(),
            "[-/=]wpm".to_string(),
            "[[/]]words".to_string(),
            "[0]reset".to_string(),
            format!("[{}/Esc]exit", keybinds.speed_read),
        ],
        UiMode::Recap => vec![recap_done_action_text(keybinds), quit_action],
    };

    pack_action_phrases(width, &phrases)
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                Style::default()
                    .fg(palette.dim)
                    .bg(palette.bg)
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect()
}

fn pack_action_phrases(width: u16, phrases: &[String]) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut current = String::new();

    for phrase in phrases {
        if current.is_empty() {
            current.push_str(phrase);
            continue;
        }

        let candidate = format!("{current} {phrase}");
        if UnicodeWidthStr::width(candidate.as_str()) <= max_width {
            current = candidate;
        } else {
            lines.push(current);
            current = phrase.clone();
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn note_action_label(key: char) -> &'static str {
    if key.eq_ignore_ascii_case(&'c') {
        "comment"
    } else {
        "note"
    }
}

fn format_key_action(key: char, label: &str) -> String {
    let mut chars = label.chars();
    let Some(first) = chars.next() else {
        return format!("[{key}]");
    };
    if first.eq_ignore_ascii_case(&key) {
        format!("[{key}]{}", chars.as_str())
    } else {
        format!("[{key}]{label}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FooterProgressCounts {
    reviewed: usize,
    commented: usize,
    skipped: usize,
    remaining: usize,
    total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FooterProgressBarWidths {
    commented: u16,
    skipped: u16,
    reviewed: u16,
    empty: u16,
}

fn footer_progress_counts(state: &AppState) -> FooterProgressCounts {
    let total = state.initial_remaining_blocks;
    let reviewed = total.saturating_sub(state.remaining_blocks);
    // Commented and skipped blocks are still reviewable; both are highlighted subsets of
    // `remaining`, with comments taking visual precedence over skip state.
    let commented = state
        .commented_nodes
        .intersection(&state.reviewable_nodes)
        .count();
    let skipped = state
        .skipped_nodes
        .intersection(&state.reviewable_nodes)
        .filter(|node_id| !state.commented_nodes.contains(node_id))
        .count();

    FooterProgressCounts {
        reviewed,
        commented,
        skipped,
        remaining: state.remaining_blocks,
        total,
    }
}

#[cfg(test)]
fn footer_progress_ratio(state: &AppState) -> f64 {
    let counts = footer_progress_counts(state);
    if counts.total > 0 {
        counts.reviewed as f64 / counts.total as f64
    } else {
        1.0
    }
}

fn footer_progress_bar_widths(counts: FooterProgressCounts, width: u16) -> FooterProgressBarWidths {
    if width == 0 {
        return FooterProgressBarWidths {
            commented: 0,
            skipped: 0,
            reviewed: 0,
            empty: 0,
        };
    }
    if counts.total == 0 {
        return FooterProgressBarWidths {
            commented: 0,
            skipped: 0,
            reviewed: width,
            empty: 0,
        };
    }

    let commented_count = counts.commented.min(counts.total);
    let skipped_count = counts
        .skipped
        .min(counts.total.saturating_sub(commented_count));
    let reviewed_count = counts.reviewed.min(
        counts
            .total
            .saturating_sub(commented_count.saturating_add(skipped_count)),
    );
    let commented_end = scaled_footer_progress_width(commented_count, counts.total, width);
    let skipped_end = scaled_footer_progress_width(
        commented_count.saturating_add(skipped_count),
        counts.total,
        width,
    )
    .max(commented_end);
    let reviewed_end = scaled_footer_progress_width(
        commented_count
            .saturating_add(skipped_count)
            .saturating_add(reviewed_count),
        counts.total,
        width,
    )
    .max(skipped_end);

    FooterProgressBarWidths {
        commented: commented_end,
        skipped: skipped_end.saturating_sub(commented_end),
        reviewed: reviewed_end.saturating_sub(skipped_end),
        empty: width.saturating_sub(reviewed_end),
    }
}

fn scaled_footer_progress_width(count: usize, total: usize, width: u16) -> u16 {
    if total == 0 {
        return width;
    }
    let count = count.min(total) as u128;
    let total = total as u128;
    let width = u128::from(width);
    u16::try_from((count * width + (total / 2)) / total).unwrap_or(u16::MAX)
}

fn footer_progress_line(state: &AppState, palette: &UiPalette) -> Line<'static> {
    let counts = footer_progress_counts(state);
    let label_bg = Style::default().bg(palette.bg);
    let mut spans = vec![
        Span::styled(
            format!("{} reviewed", counts.reviewed),
            label_bg.fg(palette.add),
        ),
        Span::styled(" · ", label_bg.fg(palette.dim)),
        Span::styled(
            format!("{} commented", counts.commented),
            label_bg.fg(palette.orange),
        ),
        Span::styled(" · ", label_bg.fg(palette.dim)),
    ];
    if counts.skipped > 0 {
        spans.push(Span::styled(
            format!("{} skipped", counts.skipped),
            label_bg.fg(palette.dim),
        ));
        spans.push(Span::styled(" · ", label_bg.fg(palette.dim)));
    }
    spans.push(Span::styled(
        format!("{} remaining", counts.remaining),
        label_bg.fg(palette.yellow),
    ));
    Line::from(spans)
}

fn render_footer_progress_bar(
    frame: &mut Frame,
    counts: FooterProgressCounts,
    area: Rect,
    palette: &UiPalette,
) {
    let widths = footer_progress_bar_widths(counts, area.width);
    let commented_end = widths.commented;
    let skipped_end = commented_end.saturating_add(widths.skipped);
    let reviewed_end = skipped_end.saturating_add(widths.reviewed);
    let buffer = frame.buffer_mut();
    let bottom = area.y.saturating_add(area.height);
    let right = area.x.saturating_add(area.width);
    for row in area.y..bottom {
        for column in area.x..right {
            let offset = column.saturating_sub(area.x);
            let segment_color = if offset < commented_end {
                Some(palette.orange)
            } else if offset < skipped_end {
                Some(palette.context)
            } else if offset < reviewed_end {
                Some(palette.add)
            } else {
                None
            };
            let Some(cell) = buffer.cell_mut((column, row)) else {
                continue;
            };
            match segment_color {
                Some(color) => {
                    cell.set_symbol("█").set_fg(color).set_bg(palette.bg);
                }
                None => {
                    cell.set_symbol(" ").set_bg(palette.bg);
                }
            }
        }
    }
}

fn render_right_aligned_line(frame: &mut Frame, area: Rect, line: &Line<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text_width = UnicodeWidthStr::width(line.to_string().as_str());
    let text_width = u16::try_from(text_width).unwrap_or(u16::MAX);
    let mut x = area.x.saturating_add(area.width.saturating_sub(text_width));
    let y = area.y;
    let right = area.x.saturating_add(area.width);
    let buffer = frame.buffer_mut();
    for span in &line.spans {
        for ch in span.content.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width == 0 {
                continue;
            }
            if x >= right {
                return;
            }
            let symbol = ch.to_string();
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(&symbol).set_style(span.style);
            }
            x = x.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
        }
    }
}

fn render_footer(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let counts = footer_progress_counts(state);
    render_footer_progress_bar(frame, counts, area, palette);
    let line = footer_progress_line(state, palette);
    render_right_aligned_line(frame, area, &line);
}

fn render_recap_view(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let lines = recap_summary_lines(state)
        .into_iter()
        .map(|line| {
            let style = if line == "All Done" {
                Style::default()
                    .fg(palette.fg)
                    .bg(palette.meta_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.dim).bg(palette.code_bg)
            };
            Line::from(Span::styled(line, style))
        })
        .collect::<Vec<_>>();

    let panel = UiBlock::default()
        .title(" Review Recap ")
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.code_bg).fg(palette.fg));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn recap_footer_hint_text(keybinds: &TuiKeybindsConfig) -> String {
    format!(
        "Press [{}] review something else or [{}/Esc] exit",
        recap_done_key(keybinds),
        keybinds.quit
    )
}

fn render_recap_footer(
    frame: &mut Frame,
    area: Rect,
    palette: &UiPalette,
    keybinds: &TuiKeybindsConfig,
) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            recap_footer_hint_text(keybinds),
            Style::default()
                .fg(palette.fg)
                .bg(palette.bg)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center)
        .style(Style::default().bg(palette.bg)),
        area,
    );
}

fn recap_summary_lines(state: &AppState) -> Vec<String> {
    let mut lines = vec![
        "All Done".to_string(),
        format!("Scope: {}", state.scope_label),
        String::new(),
    ];

    let initial_reviewed = state
        .total_blocks
        .saturating_sub(state.initial_remaining_blocks);
    let current_reviewed = state.total_blocks.saturating_sub(state.remaining_blocks);
    let session_delta = current_reviewed.saturating_sub(initial_reviewed);
    lines.push(format!(
        "Scope progress: {current_reviewed}/{} blocks reviewed (+{session_delta} this session)",
        state.total_blocks
    ));

    let start_coverage = coverage_percent(initial_reviewed, state.total_blocks);
    let end_coverage = coverage_percent(current_reviewed, state.total_blocks);
    lines.push(format!(
        "Scope coverage: {start_coverage:.1}% -> {end_coverage:.1}% ({:+.1}%)",
        end_coverage - start_coverage
    ));
    lines.push(String::new());

    if !state.session_recap.has_activity() {
        lines.push("All Done (no reviews or feedback recorded)".to_string());
        return lines;
    }

    lines.push("Session recap:".to_string());
    lines.push(format!(
        "Approvals: {} blocks",
        state.session_recap.approved_blocks
    ));
    lines.push(format!("Notes: {}", state.session_recap.comments));
    lines.push(format!(
        "Blocks touched: {}",
        state.session_recap.blocks_touched
    ));
    lines
}

fn coverage_percent(reviewed: usize, total: usize) -> f64 {
    if total == 0 {
        return 100.0;
    }
    (reviewed as f64 / total as f64) * 100.0
}

fn build_content_lines(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    code_height: u16,
    code_width: u16,
) -> BuiltContent {
    match node.kind {
        TreeNodeKind::Block => build_block_lines(state, node, palette, code_height, code_width),
        TreeNodeKind::File => build_file_lines(state, node, palette, code_height, code_width),
        TreeNodeKind::Directory => {
            let (lines, total_lines) = build_directory_lines(state, node, palette, code_height);
            BuiltContent {
                lines,
                total_lines,
                focus_row_range: None,
                comment_rows: None,
            }
        }
        TreeNodeKind::Root => {
            let (lines, total_lines) = build_root_lines(state, palette, code_height);
            BuiltContent {
                lines,
                total_lines,
                focus_row_range: None,
                comment_rows: None,
            }
        }
    }
}

fn load_file_lines(state: &mut AppState, path: &RepoPath) -> Option<Arc<[String]>> {
    if path.is_root() {
        return None;
    }

    let path_buf = PathBuf::from(path.as_str());
    if let Some(lines) = state.file_cache.get(&path_buf) {
        return Some(Arc::clone(lines));
    }

    let contents =
        load_file_contents_for_scope(&state.review_scope, state.repo_root.as_deref(), path)?;
    let lines: Arc<[String]> = contents
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .into();
    state.file_cache.insert(path_buf, Arc::clone(&lines));
    Some(lines)
}

fn load_file_contents_for_scope(
    scope: &ScopePreset,
    repo_root: Option<&Path>,
    path: &RepoPath,
) -> Option<String> {
    match scope {
        ScopePreset::All | ScopePreset::MainDiff => std::fs::read_to_string(path.as_str()).ok(),
        ScopePreset::Commit { id, .. } => load_file_contents_from_revision(repo_root, id, path),
        ScopePreset::RevisionRange { end, .. } => {
            load_file_contents_from_revision(repo_root, end, path)
        }
    }
}

fn load_file_contents_from_revision(
    repo_root: Option<&Path>,
    revision: &str,
    path: &RepoPath,
) -> Option<String> {
    let repo = repo_for_tui_state(repo_root).ok()?;
    let workdir_prefix = repo_root
        .and_then(path_utils::current_workdir_prefix_for_repo_root)
        .or_else(workdir_prefix_from_git_root);
    vcs::file_text_for_path_in_revision(&repo, revision, path, workdir_prefix.as_deref())
        .ok()
        .flatten()
}

fn repo_for_tui_state(repo_root: Option<&Path>) -> Result<gix::Repository> {
    match repo_root {
        Some(repo_root) => gix::open(repo_root)
            .with_context(|| format!("failed to open git repository at {}", repo_root.display())),
        None => vcs::repo_from_workdir(),
    }
}

fn focus_block_for_content_node(
    state: &AppState,
    node: &ContentNodeSnapshot,
) -> Option<crate::block::Block> {
    if let Some(focus_block_id) = state
        .focus_block
        .filter(|block_id| node_contains_block(&state.navigator.tree, node.id, *block_id))
    {
        return state.navigator.tree.node(focus_block_id).block.clone();
    }

    if matches!(node.kind, TreeNodeKind::Block) {
        node.block.clone()
    } else {
        None
    }
}

fn is_base_only_diff_node(state: &AppState, node: &ContentNodeSnapshot) -> bool {
    matches!(node.kind, TreeNodeKind::Block)
        && state
            .diff_block_sides
            .get(&node.id)
            .is_some_and(DiffBlockSides::is_base_only)
}

fn should_include_added_block_insertion_context(
    state: &AppState,
    node: &ContentNodeSnapshot,
) -> bool {
    // Newly inserted blocks need surrounding unchanged rows to show where the
    // insertion lands; otherwise a trailing added blank line has no context.
    matches!(node.kind, TreeNodeKind::Block)
        && state
            .block_change_kinds
            .get(&node.id)
            .is_some_and(|kind| matches!(kind, BlockChangeKind::Added))
}

fn focus_line_span_for_node(
    state: &AppState,
    node: &ContentNodeSnapshot,
    file_line_count: usize,
) -> Option<std::ops::Range<usize>> {
    let focus_block = focus_block_for_content_node(state, node)?;
    let mut start = focus_block.start_line.min(file_line_count);
    let mut end = focus_block.end_line.min(file_line_count);
    if start >= end {
        return None;
    }

    if state.view_mode == ViewMode::Diff
        && let vcs::BlockDiffFocusMode::ChangedWithContext { context_lines } =
            state.block_diff_focus_mode
    {
        start = start.saturating_sub(context_lines);
        end = end.saturating_add(context_lines).min(file_line_count);
    }

    Some(start..end)
}

fn block_line_span_for_node(
    node: &ContentNodeSnapshot,
    file_line_count: usize,
) -> Option<std::ops::Range<usize>> {
    let block = node.block.as_ref()?;
    let start = block.start_line.min(file_line_count);
    let end = block.end_line.min(file_line_count);
    (start < end).then_some(start..end)
}

fn focus_block_line_span_for_node(
    state: &AppState,
    node: &ContentNodeSnapshot,
    file_line_count: usize,
) -> Option<std::ops::Range<usize>> {
    let block = focus_block_for_content_node(state, node)?;
    let start = block.start_line.min(file_line_count);
    let end = block.end_line.min(file_line_count);
    (start < end).then_some(start..end)
}

fn changed_focus_line_span_for_source_node(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    file_line_count: usize,
) -> Option<std::ops::Range<usize>> {
    let focus_block_line_span = focus_block_line_span_for_node(state, node, file_line_count)?;
    let vcs::FileDiff::Text { hunks, .. } = cached_file_diff_for_node(state, node)? else {
        return None;
    };

    changed_line_span_for_block(hunks, &focus_block_line_span)
}

fn changed_line_span_for_block(
    hunks: &[vcs::DiffHunk],
    block_line_span: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let mut first = None;
    let mut last = None;

    for hunk in hunks {
        let mut new_line = hunk.new_start;
        for line in &hunk.lines {
            match line.kind {
                vcs::DiffLineKind::Context => {
                    new_line = new_line.saturating_add(1);
                }
                vcs::DiffLineKind::Added => {
                    let anchor_index =
                        usize::try_from(new_line.saturating_sub(1)).unwrap_or(usize::MAX);
                    if block_line_span.contains(&anchor_index) {
                        first.get_or_insert(anchor_index);
                        last = Some(anchor_index.saturating_add(1));
                    }
                    new_line = new_line.saturating_add(1);
                }
                vcs::DiffLineKind::Removed => {
                    let anchor_index =
                        usize::try_from(new_line.saturating_sub(1)).unwrap_or(usize::MAX);
                    if block_line_span.contains(&anchor_index) {
                        first.get_or_insert(anchor_index);
                        last = Some(anchor_index.saturating_add(1));
                    }
                }
            }
        }
    }

    first.zip(last).map(|(start, end)| start..end)
}

fn build_source_context_content(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    code_width: u16,
) -> BuiltContent {
    if is_base_only_diff_node(state, node)
        && let Some(block) = node.block.as_ref()
    {
        let lines = block
            .content
            .lines()
            .map(|line| {
                format_code_line(
                    &mut state.highlighted_line_cache,
                    line,
                    palette,
                    node.language.as_ref(),
                )
            })
            .collect::<Vec<_>>();
        let total_lines = lines.len();
        let comment_rows = Some(source_comment_rows(
            &lines,
            &block
                .content
                .lines()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            block.start_line,
            code_width,
        ));
        return BuiltContent {
            lines,
            total_lines,
            focus_row_range: Some(0..total_lines),
            comment_rows,
        };
    }

    let language = node.language;

    let Some(file_lines) = load_file_lines(state, &node.path) else {
        if let Some(block) = &node.block {
            let lines = block
                .content
                .lines()
                .map(|line| {
                    format_code_line(
                        &mut state.highlighted_line_cache,
                        line,
                        palette,
                        language.as_ref(),
                    )
                })
                .collect::<Vec<_>>();
            let total_lines = lines.len();
            let comment_rows = Some(source_comment_rows(
                &lines,
                &block
                    .content
                    .lines()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                block.start_line,
                code_width,
            ));
            return BuiltContent {
                lines,
                total_lines,
                focus_row_range: Some(0..total_lines),
                comment_rows,
            };
        }
        return BuiltContent {
            lines: vec![Line::from(Span::styled(
                "(File missing)",
                Style::default().fg(palette.context).bg(palette.code_bg),
            ))],
            total_lines: 1,
            focus_row_range: None,
            comment_rows: None,
        };
    };

    let highlight_line_span = focus_line_span_for_node(state, node, file_lines.len());
    let focus_row_range = changed_focus_line_span_for_source_node(state, node, file_lines.len())
        .or_else(|| highlight_line_span.clone());

    let mut lines = Vec::with_capacity(file_lines.len());
    for (index, line) in file_lines.iter().enumerate() {
        if highlight_line_span
            .as_ref()
            .is_some_and(|focus| focus.contains(&index))
        {
            lines.push(format_code_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        } else if highlight_line_span.is_some() {
            lines.push(format_context_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        } else {
            lines.push(format_code_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        }
    }

    let comment_rows = Some(source_comment_rows(
        &lines,
        file_lines.as_ref(),
        0,
        code_width,
    ));

    BuiltContent {
        total_lines: lines.len(),
        lines,
        focus_row_range,
        comment_rows,
    }
}

fn source_comment_rows(
    lines: &[Line<'static>],
    raw_lines: &[String],
    start_line: usize,
    code_width: u16,
) -> Vec<CommentContextRow> {
    let row_prefixes = wrapped_display_row_prefixes(lines, code_width);
    raw_lines
        .iter()
        .enumerate()
        .map(|(index, text)| CommentContextRow {
            scope_line_index: start_line.saturating_add(index),
            text: text.clone(),
            display_row_range: row_prefixes[index]..row_prefixes[index.saturating_add(1)],
            anchor: CommentAnchorRowCapture::SourceLine {
                line_index: start_line.saturating_add(index),
            },
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextualDiffRow {
    kind: vcs::DiffLineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
    text: String,
    anchor_index: usize,
}

fn build_contextual_diff_rows(
    file_lines: &[String],
    hunks: &[vcs::DiffHunk],
) -> Vec<ContextualDiffRow> {
    let mut rows = Vec::new();
    let mut old_line: u32 = 1;
    let mut new_line: u32 = 1;

    for hunk in hunks {
        while new_line < hunk.new_start {
            let line_index = usize::try_from(new_line.saturating_sub(1)).unwrap_or(usize::MAX);
            if let Some(text) = file_lines.get(line_index) {
                rows.push(ContextualDiffRow {
                    kind: vcs::DiffLineKind::Context,
                    old_line: Some(old_line),
                    new_line: Some(new_line),
                    text: text.clone(),
                    anchor_index: line_index,
                });
            }
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
        }

        for line in &hunk.lines {
            match line.kind {
                vcs::DiffLineKind::Context => {
                    let line_index = usize::try_from(new_line.saturating_sub(1)).unwrap_or(0);
                    rows.push(ContextualDiffRow {
                        kind: vcs::DiffLineKind::Context,
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                        text: line.text.trim_end_matches('\n').to_string(),
                        anchor_index: line_index,
                    });
                    old_line = old_line.saturating_add(1);
                    new_line = new_line.saturating_add(1);
                }
                vcs::DiffLineKind::Added => {
                    let line_index = usize::try_from(new_line.saturating_sub(1)).unwrap_or(0);
                    rows.push(ContextualDiffRow {
                        kind: vcs::DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(new_line),
                        text: line.text.trim_end_matches('\n').to_string(),
                        anchor_index: line_index,
                    });
                    new_line = new_line.saturating_add(1);
                }
                vcs::DiffLineKind::Removed => {
                    let line_index = usize::try_from(new_line.saturating_sub(1)).unwrap_or(0);
                    rows.push(ContextualDiffRow {
                        kind: vcs::DiffLineKind::Removed,
                        old_line: Some(old_line),
                        new_line: None,
                        text: line.text.trim_end_matches('\n').to_string(),
                        anchor_index: line_index,
                    });
                    old_line = old_line.saturating_add(1);
                }
            }
        }
    }

    while let Ok(line_index) = usize::try_from(new_line.saturating_sub(1)) {
        let Some(text) = file_lines.get(line_index) else {
            break;
        };
        rows.push(ContextualDiffRow {
            kind: vcs::DiffLineKind::Context,
            old_line: Some(old_line),
            new_line: Some(new_line),
            text: text.clone(),
            anchor_index: line_index,
        });
        old_line = old_line.saturating_add(1);
        new_line = new_line.saturating_add(1);
    }

    rows
}

fn focus_row_range_for_contextual_diff_rows(
    rows: &[ContextualDiffRow],
    focus_line_span: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let mut first = None;
    let mut last = None;

    for (index, row) in rows.iter().enumerate() {
        if focus_line_span.contains(&row.anchor_index) {
            first.get_or_insert(index);
            last = Some(index.saturating_add(1));
        }
    }

    first.zip(last).map(|(start, end)| start..end)
}

fn changed_focus_row_range_for_contextual_diff_rows(
    rows: &[ContextualDiffRow],
    focus_line_span: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let mut first = None;
    let mut last = None;

    for (index, row) in rows.iter().enumerate() {
        if row.kind == vcs::DiffLineKind::Context || !focus_line_span.contains(&row.anchor_index) {
            continue;
        }

        first.get_or_insert(index);
        last = Some(index.saturating_add(1));
    }

    first.zip(last).map(|(start, end)| start..end)
}

fn style_for_contextual_diff_row(
    row: &ContextualDiffRow,
    focus_line_span: Option<&std::ops::Range<usize>>,
    palette: &UiPalette,
) -> Style {
    match row.kind {
        vcs::DiffLineKind::Added => Style::default()
            .fg(palette.add)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD),
        vcs::DiffLineKind::Removed => Style::default()
            .fg(palette.del)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD),
        vcs::DiffLineKind::Context => {
            if focus_line_span.is_some_and(|focus| focus.contains(&row.anchor_index)) {
                Style::default()
                    .fg(palette.fg)
                    .bg(palette.code_bg)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default()
                    .fg(palette.context)
                    .bg(palette.code_bg)
                    .add_modifier(Modifier::DIM)
            }
        }
    }
}

struct RenderedContextualDiffLines {
    lines: Vec<Line<'static>>,
    row_ranges: Vec<std::ops::Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOverlayFormat {
    Full,
    Compact,
}

const FULL_DIFF_OVERLAY_GUTTER_WIDTH: usize = 14;

fn render_contextual_diff_lines(
    rows: &[ContextualDiffRow],
    focus_line_span: Option<&std::ops::Range<usize>>,
    palette: &UiPalette,
    code_width: u16,
    line_numbers: TuiDiffLineNumbers,
) -> RenderedContextualDiffLines {
    let mut lines = Vec::new();
    let mut row_ranges = Vec::with_capacity(rows.len());

    for row in rows {
        let style = style_for_contextual_diff_row(row, focus_line_span, palette);
        let row_lines = wrap_contextual_diff_row(row, style, code_width, line_numbers);
        let start = lines.len();
        lines.extend(row_lines);
        row_ranges.push(start..lines.len());
    }

    RenderedContextualDiffLines { lines, row_ranges }
}

fn wrap_contextual_diff_row(
    row: &ContextualDiffRow,
    style: Style,
    code_width: u16,
    line_numbers: TuiDiffLineNumbers,
) -> Vec<Line<'static>> {
    let diff_line = vcs::DiffLine {
        kind: row.kind,
        old_line: row.old_line,
        new_line: row.new_line,
        text: row.text.clone(),
        is_focus: false,
    };

    wrap_diff_overlay_row(&diff_line, style, code_width, line_numbers)
}

fn wrap_diff_overlay_row(
    line: &vcs::DiffLine,
    style: Style,
    code_width: u16,
    line_numbers: TuiDiffLineNumbers,
) -> Vec<Line<'static>> {
    let available_width = usize::from(code_width.max(1));
    let format = diff_overlay_format_for_width(code_width, line_numbers);
    let prefix = diff_overlay_prefix(line, format, available_width);
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let text = expand_tabs_for_display(&line.text);

    if prefix_width >= available_width {
        let mut lines = vec![Line::from(Span::styled(prefix, style))];
        if text.is_empty() {
            return lines;
        }
        lines.extend(
            wrap_text_to_width(&text, available_width)
                .into_iter()
                .map(|chunk| Line::from(Span::styled(chunk, style))),
        );
        return lines;
    }

    let continuation_prefix = " ".repeat(prefix_width);
    let text_width = available_width.saturating_sub(prefix_width).max(1);
    let wrapped_text = wrap_text_to_width(&text, text_width);

    if wrapped_text.is_empty() {
        return vec![Line::from(Span::styled(prefix, style))];
    }

    let mut lines = Vec::with_capacity(wrapped_text.len());
    for (index, chunk) in wrapped_text.into_iter().enumerate() {
        let row_text = if index == 0 {
            format!("{prefix}{chunk}")
        } else {
            format!("{continuation_prefix}{chunk}")
        };
        lines.push(Line::from(Span::styled(row_text, style)));
    }
    lines
}

fn diff_overlay_format_for_width(
    code_width: u16,
    line_numbers: TuiDiffLineNumbers,
) -> DiffOverlayFormat {
    match line_numbers {
        TuiDiffLineNumbers::Disabled => DiffOverlayFormat::Compact,
        TuiDiffLineNumbers::OldNew if usize::from(code_width) <= FULL_DIFF_OVERLAY_GUTTER_WIDTH => {
            DiffOverlayFormat::Compact
        }
        TuiDiffLineNumbers::OldNew => DiffOverlayFormat::Full,
    }
}

fn diff_overlay_prefix(
    line: &vcs::DiffLine,
    format: DiffOverlayFormat,
    available_width: usize,
) -> String {
    let marker = diff_overlay_marker(line.kind);
    match format {
        DiffOverlayFormat::Full if available_width > FULL_DIFF_OVERLAY_GUTTER_WIDTH => {
            let old_col = format_diff_line_number(line.old_line);
            let new_col = format_diff_line_number(line.new_line);
            format!("{old_col} {new_col} {marker} ")
        }
        DiffOverlayFormat::Full | DiffOverlayFormat::Compact => {
            if available_width > 1 {
                format!("{marker} ")
            } else {
                marker.to_string()
            }
        }
    }
}

fn diff_overlay_marker(kind: vcs::DiffLineKind) -> char {
    match kind {
        vcs::DiffLineKind::Context => ' ',
        vcs::DiffLineKind::Added => '+',
        vcs::DiffLineKind::Removed => '-',
    }
}

fn wrap_text_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut wrapped = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width > 0 && current_width.saturating_add(ch_width) > width {
            wrapped.push(std::mem::take(&mut current));
            current_width = 0;
        }

        current.push(ch);
        current_width = current_width.saturating_add(ch_width);

        if current_width >= width {
            wrapped.push(std::mem::take(&mut current));
            current_width = 0;
        }
    }

    if !current.is_empty() || wrapped.is_empty() {
        wrapped.push(current);
    }

    wrapped
}

fn focus_row_range_for_wrapped_contextual_diff_rows(
    row_ranges: &[std::ops::Range<usize>],
    focus_row_range: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    if focus_row_range.is_empty() {
        return None;
    }

    let start = row_ranges.get(focus_row_range.start)?.start;
    let end = row_ranges.get(focus_row_range.end.saturating_sub(1))?.end;
    Some(start..end)
}

fn source_hint_text(keybinds: &TuiKeybindsConfig) -> String {
    format!("Press [{}] to view source", keybinds.toggle_view)
}

fn content_message(
    message: &str,
    source_hint_keybinds: Option<&TuiKeybindsConfig>,
    palette: &UiPalette,
) -> BuiltContent {
    let mut lines = vec![Line::from(Span::styled(
        message.to_string(),
        Style::default().fg(palette.dim).bg(palette.code_bg),
    ))];
    if let Some(keybinds) = source_hint_keybinds {
        lines.push(Line::from(Span::styled(
            source_hint_text(keybinds),
            Style::default().fg(palette.dim).bg(palette.code_bg),
        )));
    }
    BuiltContent {
        total_lines: lines.len(),
        lines,
        focus_row_range: None,
        comment_rows: None,
    }
}

fn build_diff_context_content(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    code_width: u16,
) -> BuiltContent {
    if is_base_only_diff_node(state, node)
        && let Some(block) = node.block.as_ref()
    {
        return build_deleted_block_diff_content(state, block, palette, code_width);
    }

    let Some(file_lines) = load_file_lines(state, &node.path) else {
        return content_message("(File missing)", None, palette);
    };
    let display_line_span = focus_line_span_for_node(state, node, file_lines.len());
    let block_line_span = block_line_span_for_node(node, file_lines.len());
    let focus_block_line_span = focus_block_line_span_for_node(state, node, file_lines.len());

    let Some(file_diff) = cached_file_diff_for_node(state, node) else {
        return content_message("(No path for diff)", None, palette);
    };

    match file_diff {
        vcs::FileDiff::Text { hunks, .. } => {
            let rows = build_contextual_diff_rows(&file_lines, hunks);
            let include_added_block_insertion_context =
                should_include_added_block_insertion_context(state, node);
            let rows = match (&node.kind, display_line_span.as_ref()) {
                (TreeNodeKind::Block, Some(display_span)) => rows
                    .into_iter()
                    .filter(|row| {
                        display_span.contains(&row.anchor_index)
                            || (include_added_block_insertion_context
                                && row.kind == vcs::DiffLineKind::Context)
                    })
                    .collect::<Vec<_>>(),
                _ => rows,
            };
            let focus_row_range = focus_block_line_span
                .as_ref()
                .and_then(|focus| changed_focus_row_range_for_contextual_diff_rows(&rows, focus))
                .or_else(|| match node.kind {
                    TreeNodeKind::Block => block_line_span
                        .as_ref()
                        .and_then(|focus| focus_row_range_for_contextual_diff_rows(&rows, focus)),
                    _ => display_line_span
                        .as_ref()
                        .and_then(|focus| focus_row_range_for_contextual_diff_rows(&rows, focus)),
                });
            let highlight_span = match node.kind {
                TreeNodeKind::Block => block_line_span.as_ref(),
                _ => display_line_span.as_ref(),
            };
            let rendered = render_contextual_diff_lines(
                &rows,
                highlight_span,
                palette,
                code_width,
                state.diff_line_numbers,
            );
            let focus_row_range = focus_row_range.as_ref().and_then(|focus| {
                focus_row_range_for_wrapped_contextual_diff_rows(&rendered.row_ranges, focus)
            });
            let comment_rows = Some(
                rows.iter()
                    .zip(rendered.row_ranges.iter())
                    .map(|(row, display_row_range)| {
                        let diff_line = vcs::DiffLine {
                            kind: row.kind,
                            old_line: row.old_line,
                            new_line: row.new_line,
                            text: row.text.clone(),
                            is_focus: false,
                        };
                        CommentContextRow {
                            scope_line_index: row.anchor_index,
                            text: format_diff_overlay_row_for_width(
                                &diff_line,
                                state.diff_line_numbers,
                                code_width,
                            ),
                            display_row_range: display_row_range.clone(),
                            anchor: CommentAnchorRowCapture::DiffRow {
                                row: DiffCommentAnchorRow {
                                    kind: comment_anchor_diff_line_kind(row.kind),
                                    old_line: row.old_line,
                                    new_line: row.new_line,
                                },
                            },
                        }
                    })
                    .collect::<Vec<_>>(),
            );
            BuiltContent {
                total_lines: rendered.lines.len(),
                lines: rendered.lines,
                focus_row_range,
                comment_rows,
            }
        }
        vcs::FileDiff::NoTextChanges { .. } => content_message(
            "(No diff changes in this file)",
            Some(&state.keybinds),
            palette,
        ),
        vcs::FileDiff::Unavailable {
            reason: vcs::FileDiffUnavailableReason::Binary,
            ..
        } => content_message("(Diff unavailable for binary file)", None, palette),
        vcs::FileDiff::Unavailable {
            reason: vcs::FileDiffUnavailableReason::External,
            ..
        } => content_message(
            "(Diff unavailable from external diff command)",
            None,
            palette,
        ),
    }
}

fn build_deleted_block_diff_content(
    state: &AppState,
    block: &crate::block::Block,
    palette: &UiPalette,
    code_width: u16,
) -> BuiltContent {
    let style = Style::default()
        .fg(palette.del)
        .bg(palette.code_bg)
        .add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();

    for (offset, line) in block.content.lines().enumerate() {
        let diff_line = vcs::DiffLine {
            kind: vcs::DiffLineKind::Removed,
            old_line: Some(u32::try_from(block.start_line + offset + 1).unwrap_or(u32::MAX)),
            new_line: None,
            text: line.to_string(),
            is_focus: true,
        };
        lines.extend(wrap_diff_overlay_row(
            &diff_line,
            style,
            code_width,
            state.diff_line_numbers,
        ));
    }

    let total_lines = lines.len();
    let comment_rows = Some(
        block
            .content
            .lines()
            .enumerate()
            .scan(0usize, |display_start, (offset, line)| {
                let diff_line = vcs::DiffLine {
                    kind: vcs::DiffLineKind::Removed,
                    old_line: Some(
                        u32::try_from(block.start_line + offset + 1).unwrap_or(u32::MAX),
                    ),
                    new_line: None,
                    text: line.to_string(),
                    is_focus: false,
                };
                let row_line_count =
                    wrap_diff_overlay_row(&diff_line, style, code_width, state.diff_line_numbers)
                        .len();
                let row = CommentContextRow {
                    scope_line_index: block.start_line.saturating_add(offset),
                    text: format_diff_overlay_row_for_width(
                        &diff_line,
                        state.diff_line_numbers,
                        code_width,
                    ),
                    display_row_range: *display_start..display_start.saturating_add(row_line_count),
                    anchor: CommentAnchorRowCapture::DiffRow {
                        row: DiffCommentAnchorRow {
                            kind: CommentAnchorDiffLineKind::Removed,
                            old_line: diff_line.old_line,
                            new_line: diff_line.new_line,
                        },
                    },
                };
                *display_start = display_start.saturating_add(row_line_count);
                Some(row)
            })
            .collect::<Vec<_>>(),
    );
    BuiltContent {
        total_lines,
        lines,
        focus_row_range: Some(0..total_lines),
        comment_rows,
    }
}

fn build_block_lines(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    _code_height: u16,
    code_width: u16,
) -> BuiltContent {
    if state.view_mode == ViewMode::Diff {
        return build_diff_context_content(state, node, palette, code_width);
    }
    build_source_context_content(state, node, palette, code_width)
}

fn format_diff_overlay_row_for_width(
    line: &vcs::DiffLine,
    line_numbers: TuiDiffLineNumbers,
    code_width: u16,
) -> String {
    let available_width = usize::from(code_width.max(1));
    let format = diff_overlay_format_for_width(code_width, line_numbers);
    let prefix = diff_overlay_prefix(line, format, available_width);
    let text = expand_tabs_for_display(&line.text);
    format!("{prefix}{text}")
}

#[cfg(test)]
fn format_diff_overlay_row(line: &vcs::DiffLine, line_numbers: TuiDiffLineNumbers) -> String {
    format_diff_overlay_row_for_width(line, line_numbers, u16::MAX)
}

fn format_diff_line_number(line: Option<u32>) -> String {
    match line {
        Some(number) => format!("{number:>5}"),
        None => "     ".to_string(),
    }
}

fn ensure_cached_file_diff<'a, F>(
    cache: &'a mut HashMap<PathBuf, vcs::FileDiff>,
    path: &Path,
    load_file_diff: F,
) -> &'a vcs::FileDiff
where
    F: FnOnce() -> Result<vcs::FileDiff>,
{
    cache.entry(path.to_path_buf()).or_insert_with(|| {
        load_file_diff().unwrap_or_else(|_| vcs::FileDiff::NoTextChanges {
            path: RepoPath::new(path.to_string_lossy().as_ref())
                .unwrap_or_else(|_| RepoPath::root()),
        })
    })
}

fn cached_file_diff_for_node<'a>(
    state: &'a mut AppState,
    node: &ContentNodeSnapshot,
) -> Option<&'a vcs::FileDiff> {
    if node.path.is_root() {
        return None;
    }

    let review_scope = state.review_scope.clone();
    let diff_path = node.path.as_str().to_string();
    let path = PathBuf::from(&diff_path);

    Some(ensure_cached_file_diff(
        &mut state.file_diff_cache,
        &path,
        || {
            let query = review_scope.diff_query_for_path(&diff_path);
            let repo = repo_for_tui_state(state.repo_root.as_deref())?;
            match query {
                DiffQuery::MainDiff { path } => vcs::diff_for_file(&repo, &RepoPath::new(path)?),
                DiffQuery::Revision { revision, path } => {
                    vcs::diff_for_file_in_revision(&repo, &revision, &RepoPath::new(path)?)
                }
                DiffQuery::RevisionRange { start, end, path } => {
                    vcs::diff_for_file_in_range(&repo, &start, &end, &RepoPath::new(path)?)
                }
            }
        },
    ))
}

fn build_file_lines(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    _code_height: u16,
    code_width: u16,
) -> BuiltContent {
    if state.view_mode == ViewMode::Diff {
        return build_diff_context_content(state, node, palette, code_width);
    }
    build_source_context_content(state, node, palette, code_width)
}

fn build_directory_lines(
    state: &AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    _code_height: u16,
) -> (Vec<Line<'static>>, usize) {
    let mut entries = Vec::new();
    for child_id in &node.children {
        if !state.navigator.is_visible(*child_id) {
            continue;
        }
        let child = state.navigator.tree.node(*child_id);
        let label = match child.kind {
            TreeNodeKind::Directory => format!("{}/", child.name),
            TreeNodeKind::File => child.name.clone(),
            TreeNodeKind::Block => format!("{}:{}", child.name, child.hash),
            TreeNodeKind::Root => child.name.clone(),
        };
        entries.push(label);
    }
    entries.sort();

    if entries.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "(Empty)",
                Style::default().fg(palette.context).bg(palette.code_bg),
            ))],
            1,
        );
    }

    // Scrollable directory view
    let entries_list = entries
        .iter()
        .map(|entry| format_directory_line(entry, palette))
        .collect::<Vec<_>>();

    let len = entries_list.len();
    (entries_list, len)
}

fn build_root_lines(
    state: &mut AppState,
    palette: &UiPalette,
    _code_height: u16,
) -> (Vec<Line<'static>>, usize) {
    let root_children = visible_root_children(state);

    if state.root_cursor.is_none() {
        state.root_cursor = root_children.first().copied();
    }

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!("Unreviewed blocks: {}", state.remaining_blocks),
            Style::default().fg(palette.fg).bg(palette.code_bg),
        ),
        Span::styled(
            format!(" (scope: {})", state.scope_label),
            Style::default().fg(palette.dim).bg(palette.code_bg),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("Files/dirs: {}", root_children.len()),
        Style::default().fg(palette.dim).bg(palette.code_bg),
    )));

    let kind_counts = review_metadata::sorted_visible_block_kind_counts(
        &state.navigator.tree,
        state.navigator.visible_nodes(),
    );

    let mut last_parent = "";
    for (kind, count) in kind_counts {
        let parent = review_metadata::parent_kind_label(kind);
        if parent != last_parent {
            if !last_parent.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("{parent}:"),
                Style::default().fg(palette.fg).bg(palette.code_bg),
            )));
            last_parent = parent;
        }
        lines.push(Line::from(Span::styled(
            format!("  {}: {count}", kind.as_str()),
            Style::default().fg(palette.dim).bg(palette.code_bg),
        )));
    }

    if !lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "",
            Style::default().bg(palette.code_bg),
        )));
    }

    let mut listing = root_children
        .iter()
        .map(|id| {
            let child = state.navigator.tree.node(*id);
            let name = match child.kind {
                TreeNodeKind::Directory => format!("  {}/", child.name),
                TreeNodeKind::File => format!("  {}", child.name),
                TreeNodeKind::Block => format!("  {}", child.name),
                TreeNodeKind::Root => child.name.clone(),
            };
            let selected = state.root_cursor == Some(*id);
            format_root_entry_line(&name, palette, selected)
        })
        .collect::<Vec<_>>();

    if listing.is_empty() {
        listing.push(Line::from(Span::styled(
            "(Empty repository)",
            Style::default().fg(palette.context).bg(palette.code_bg),
        )));
    }

    lines.append(&mut listing);
    let len = lines.len();
    (lines, len)
}

fn format_root_entry_line(entry: &str, palette: &UiPalette, selected: bool) -> Line<'static> {
    let style = if selected {
        Style::default().fg(palette.fg).bg(palette.meta_bg)
    } else {
        Style::default().fg(palette.context).bg(palette.code_bg)
    };
    Line::from(Span::styled(entry.to_string(), style)).style(style)
}

fn format_directory_line(entry: &str, palette: &UiPalette) -> Line<'static> {
    let gutter_left = 4;
    let gutter_right = 2;
    let gutter_spacing = " ".repeat(gutter_left + gutter_right + 1);
    Line::from(vec![
        Span::styled(
            gutter_spacing,
            Style::default().fg(palette.context).bg(palette.code_bg),
        ),
        Span::styled(
            entry.to_string(),
            Style::default().fg(palette.context).bg(palette.code_bg),
        ),
    ])
}

fn format_context_line(
    highlighted_line_cache: &mut HashMap<HighlightLineCacheKey, Vec<HighlightToken>>,
    line: &str,
    palette: &UiPalette,
    language: Option<&Language>,
) -> Line<'static> {
    let gutter_left = 4;
    let gutter_right = 2;
    let gutter_spacing = " ".repeat(gutter_left + gutter_right + 1);
    let tokens = highlighted_tokens_for_line(highlighted_line_cache, line, language);
    let mut spans = Vec::with_capacity(tokens.len() + 1);
    spans.push(Span::styled(
        gutter_spacing,
        Style::default().fg(palette.context).bg(palette.code_bg),
    ));
    for token in tokens {
        let style = style_for_token(token.kind, palette)
            .fg(palette.context)
            .bg(palette.code_bg);
        spans.push(Span::styled(token.text, style));
    }
    Line::from(spans)
}

fn render_input_overlay(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let overlay_width = input_overlay_width(area.width);
    let (title, hints, content, draft, cursor) = match &state.input_mode {
        InputMode::Editing { .. } => {
            let content = state.input_buffer.clone();
            let draft = state.input_draft.as_deref().filter(|_| content.is_empty());
            (
                " Note ",
                editing_overlay_hint(&content, draft, state.editing_validation),
                content,
                draft,
                Some(state.input_cursor.clamped_to_buffer(&state.input_buffer)),
            )
        }
        InputMode::ConfirmBatch { count, action } => {
            let content = format!(
                "This will apply '{}' to {} unreviewed descendant block(s).",
                action.verdict_label(),
                count
            );
            (
                " Batch Action ",
                "Enter to confirm • Esc to cancel".to_string(),
                content,
                None,
                None,
            )
        }
        InputMode::Normal => return,
    };
    let popup_area = input_overlay_rect(
        area,
        input_overlay_body_lines(&content, draft, &hints, overlay_width),
    );
    frame.render_widget(ratatui::widgets::Clear, popup_area);

    let block = UiBlock::default()
        .title(title)
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));
    let inner_area = block.inner(popup_area);
    let lines = input_overlay_lines(&content, draft, &hints, palette, inner_area.width);

    frame.render_widget(Paragraph::new(lines).block(block), popup_area);

    if let Some(cursor) = cursor {
        let hint_lines = usize_to_u16_saturating(wrapped_editor_line_count(
            &hints,
            usize::from(inner_area.width.max(1)),
        ));
        frame.set_cursor_position(editing_cursor_position(
            &content, cursor, inner_area, hint_lines,
        ));
    }
}

fn visible_editor_text<'a>(content: &'a str, draft: Option<&'a str>) -> &'a str {
    if content.is_empty() {
        draft.unwrap_or(content)
    } else {
        content
    }
}

fn input_overlay_width(area_width: u16) -> u16 {
    if area_width >= 20 {
        area_width.min(96)
    } else {
        area_width
    }
}

fn input_overlay_body_lines(
    content: &str,
    draft: Option<&str>,
    hints: &str,
    overlay_width: u16,
) -> u16 {
    let inner_width = usize::from(overlay_width.saturating_sub(2).max(1));
    let editor_text = visible_editor_text(content, draft);
    let body_lines = wrapped_editor_line_count(editor_text, inner_width)
        .saturating_add(1)
        .saturating_add(wrapped_editor_line_count(hints, inner_width));
    usize_to_u16_saturating(body_lines)
}

#[cfg(test)]
fn editing_input_lines(content: &str, overlay_width: u16) -> u16 {
    let inner_width = usize::from(overlay_width.saturating_sub(2).max(1));
    usize_to_u16_saturating(wrapped_editor_line_count(content, inner_width))
}

fn wrapped_editor_line_count(text: &str, wrap_width: usize) -> usize {
    cell_wrapped_editor_lines(text, wrap_width.max(1)).len()
}

fn cell_wrapped_editor_lines(text: &str, wrap_width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| wrap_text_to_width(line, wrap_width.max(1)))
        .collect()
}

fn editor_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn editing_cursor_position(
    content: &str,
    cursor: InputCursor,
    inner_area: Rect,
    hint_lines: u16,
) -> Position {
    if inner_area.width == 0 || inner_area.height == 0 {
        return Position {
            x: inner_area.x,
            y: inner_area.y,
        };
    }

    let (column, row) = editing_cursor_visual_offset(content, cursor.offset, inner_area.width);
    let visible_content_rows = inner_area
        .height
        .saturating_sub(hint_lines.saturating_add(1));
    let max_row = visible_content_rows.saturating_sub(1);
    Position {
        x: inner_area.x + column.min(inner_area.width.saturating_sub(1)),
        y: inner_area.y + row.min(max_row),
    }
}

fn editing_cursor_visual_offset(
    content: &str,
    cursor_offset: usize,
    wrap_width: u16,
) -> (u16, u16) {
    if wrap_width == 0 {
        return (0, 0);
    }

    let wrap_width = usize::from(wrap_width.max(1));
    let cursor_offset = clamp_cursor_offset_to_char_boundary(content, cursor_offset);
    let wrapped_prefix = cell_wrapped_editor_lines(&content[..cursor_offset], wrap_width);
    let row = wrapped_prefix.len().saturating_sub(1);
    let column = wrapped_prefix
        .last()
        .map(|line| editor_display_width(line))
        .unwrap_or_default();

    (
        usize_to_u16_saturating(column.min(wrap_width.saturating_sub(1))),
        usize_to_u16_saturating(row),
    )
}

fn input_overlay_rect(area: Rect, body_lines: u16) -> Rect {
    let preferred_height = body_lines.saturating_add(2).clamp(5, 12);
    let height = area.height.min(preferred_height);
    let width = input_overlay_width(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height.saturating_sub(height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn input_overlay_lines(
    content: &str,
    draft: Option<&str>,
    hints: &str,
    palette: &UiPalette,
    wrap_width: u16,
) -> Vec<Line<'static>> {
    let wrap_width = usize::from(wrap_width.max(1));
    let mut lines = if content.is_empty() {
        cell_wrapped_editor_lines(visible_editor_text(content, draft), wrap_width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(palette.dim))))
            .collect::<Vec<_>>()
    } else {
        cell_wrapped_editor_lines(content, wrap_width)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>()
    };
    lines.push(Line::from(""));
    lines.extend(
        cell_wrapped_editor_lines(hints, wrap_width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(palette.dim)))),
    );
    lines
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditingSubmitDecision {
    Empty,
    Ready(PendingAction),
}

fn editing_submit_decision(
    input_mode: &InputMode,
    input_buffer: &str,
) -> Option<EditingSubmitDecision> {
    let InputMode::Editing { action } = input_mode else {
        return None;
    };
    // The TUI intentionally requires visible note text before it will submit a
    // comment action. Lower layers still model notes as optional so non-TUI
    // callers can pass `None`, but the overlay UX treats blank submit as
    // validation failure instead of as an empty comment.
    if input_buffer.trim().is_empty() {
        return Some(EditingSubmitDecision::Empty);
    }
    Some(EditingSubmitDecision::Ready(
        action.with_note(input_buffer.to_string()),
    ))
}

fn clear_editing_validation(state: &mut AppState) {
    state.editing_validation = None;
}

fn editing_overlay_hint(
    content: &str,
    draft: Option<&str>,
    validation: Option<EditingValidation>,
) -> String {
    if matches!(validation, Some(EditingValidation::NoteRequired)) {
        return "Note required • Type a note • Ctrl+J newline • Esc to cancel".to_string();
    }
    if content.trim().is_empty() && draft.is_some() {
        "Right arrow to use suggestion • Type to discard • Enter to submit • Ctrl+J newline • Esc to cancel".to_string()
    } else if content.trim().is_empty() {
        "Type a note • Enter to submit • Ctrl+J newline • Esc to cancel".to_string()
    } else {
        "Enter to submit • Ctrl+J newline • Esc to cancel".to_string()
    }
}

fn centered_rect(r: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

struct FocusLayout {
    meta: Rect,
    code: Rect,
    ai_hint: Rect,
    actions: Rect,
    mode: Rect,
}

const ACTIONS_HEIGHT: u16 = 2;
const MODE_BANNER_HEIGHT: u16 = 1;
const CONTROLS_PADDING_HEIGHT: u16 = 1;

fn focus_code_width(area: Rect) -> u16 {
    area.width.min(120)
}

fn compute_focus_layout(
    area: Rect,
    header_lines: u16,
    requested_ai_hint_height: u16,
) -> FocusLayout {
    let code_width = focus_code_width(area);
    let desired_code_height = area.height.min(32);
    let padding = u16::try_from((u32::from(area.height) * 5 + 50) / 100).unwrap_or(u16::MAX);

    let available_height = area.height.saturating_sub(padding * 2).max(1);
    let mode_height = MODE_BANNER_HEIGHT.min(available_height);
    let available_after_mode = available_height.saturating_sub(mode_height);
    let actions_height = ACTIONS_HEIGHT.min(available_after_mode.saturating_sub(2));
    let available_after_actions = available_after_mode.saturating_sub(actions_height);
    let ai_hint_height = requested_ai_hint_height.min(available_after_actions.saturating_sub(2));
    let available_after_ai = available_after_actions.saturating_sub(ai_hint_height);
    let controls_padding_height = CONTROLS_PADDING_HEIGHT.min(available_after_ai.saturating_sub(2));
    let available_for_header_and_code = available_after_ai.saturating_sub(controls_padding_height);
    let min_header_height = 3.min(available_for_header_and_code);
    let desired_header_height = header_lines.saturating_add(2).max(min_header_height);
    let header_height = desired_header_height.min(available_for_header_and_code.saturating_sub(1));
    let code_height =
        desired_code_height.min(available_for_header_and_code.saturating_sub(header_height));
    let total_height = header_height
        + code_height
        + controls_padding_height
        + ai_hint_height
        + actions_height
        + mode_height;

    let content_top = area.y + (area.height.saturating_sub(total_height)) / 2;
    let content_left = area.x + (area.width.saturating_sub(code_width)) / 2;

    let meta = Rect {
        x: content_left,
        y: content_top,
        width: code_width,
        height: header_height,
    };

    let code = Rect {
        x: content_left,
        y: content_top + header_height,
        width: code_width,
        height: code_height,
    };

    let actions = Rect {
        x: content_left,
        y: content_top + header_height + code_height + controls_padding_height,
        width: code_width,
        height: actions_height,
    };

    let mode = Rect {
        x: content_left,
        y: content_top + header_height + code_height + controls_padding_height + actions_height,
        width: code_width,
        height: mode_height,
    };

    let ai_hint = Rect {
        x: content_left,
        y: content_top
            + header_height
            + code_height
            + controls_padding_height
            + actions_height
            + mode_height,
        width: code_width,
        height: ai_hint_height,
    };

    FocusLayout {
        meta,
        code,
        ai_hint,
        actions,
        mode,
    }
}

#[cfg(test)]
mod focus_layout_tests {
    use super::*;

    #[test]
    fn focus_layout_shrinks_when_area_is_small() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let layout = compute_focus_layout(area, 3, 0);
        assert!(layout.code.width <= 80);
        assert!(layout.code.height <= 20);
        assert!(layout.meta.y >= area.y);
    }

    #[test]
    fn focus_layout_centers_when_space_allows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let layout = compute_focus_layout(area, 3, 0);
        assert_eq!(layout.code.width, 120);
        assert_eq!(layout.actions.height, 2);
        assert!(layout.code.y > area.y);
    }

    #[test]
    fn focus_layout_places_mode_banner_below_action_rows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = compute_focus_layout(area, 3, 0);
        assert_eq!(layout.mode.height, 1);
        assert_eq!(layout.mode.y, layout.actions.y + layout.actions.height);
    }

    #[test]
    fn focus_layout_leaves_blank_row_between_code_and_actions_when_space_allows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = compute_focus_layout(area, 3, 0);
        assert_eq!(layout.actions.y, layout.code.y + layout.code.height + 1);
    }

    #[test]
    fn focus_layout_reserves_header_border_space() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = compute_focus_layout(area, 1, 0);
        assert_eq!(layout.meta.height, 3);
    }

    #[test]
    fn focus_layout_places_ai_hint_below_mode_banner() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = compute_focus_layout(area, 1, 1);
        assert_eq!(layout.ai_hint.height, 1);
        assert_eq!(layout.mode.y, layout.actions.y + layout.actions.height);
        assert_eq!(layout.ai_hint.y, layout.mode.y + layout.mode.height);
    }

    #[test]
    fn focus_layout_reserves_multiple_ai_hint_rows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = compute_focus_layout(area, 1, 3);
        assert_eq!(layout.ai_hint.height, 3);
        assert_eq!(layout.ai_hint.y, layout.mode.y + layout.mode.height);
    }

    #[test]
    fn focus_layout_keeps_actions_within_content_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let layout = compute_focus_layout(area, 3, 0);
        assert!(
            layout.actions.y + layout.actions.height <= area.y + area.height,
            "actions rect should fit within content area"
        );
    }
}

#[cfg(test)]
mod diff_scope_tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind, FileState};
    use crate::block_splitter;
    use crate::cli::Cli;
    use crate::commands::review::{
        BlockChangeKind, CollectedReview, FileChangeKind, ReviewDiagnostic, ReviewRequest,
        ReviewSummary, UnreviewedFile,
    };
    use crate::config::{BatchConfirmPolicy, BlockFilters};
    use crate::context::TrueflowContext;
    use crate::repo_path::RepoPath;
    use crate::scanner::ScanOptions;
    use crate::store::{CommitId, ReviewTargetKind};
    use crate::test_git::{run_git, run_git_stdout, temp_git_repo, temp_test_dir};
    use crate::tree::{TreeBuilder, build_tree_from_files};
    use clap::Parser;
    use ratatui::{Terminal, backend::TestBackend};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, mpsc};

    fn build_test_state(
        review_scope: ScopePreset,
        file_diff_cache: HashMap<PathBuf, vcs::FileDiff>,
    ) -> AppState {
        let tree = TreeBuilder::new().finalize();
        let review_order = ReviewOrder::from_tree(&tree, &HashSet::new());
        let navigator = ReviewNavigator::new(tree, HashSet::new())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        AppState {
            review_scope,
            navigator,
            review_order,
            total_blocks: 0,
            initial_remaining_blocks: 0,
            remaining_blocks: 0,
            reviewable_nodes: HashSet::new(),
            commented_nodes: HashSet::new(),
            skipped_nodes: HashSet::new(),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
            session_recap: SessionRecap::default(),
            scope_label: String::new(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: InputCursor::default(),
            input_draft: None,
            editing_validation: None,
            confirm_batch: BatchConfirmPolicy::Never,
            repo_name: "repo".to_string(),
            repo_root: None,
            file_cache: HashMap::new(),
            root_cursor: None,
            focus_block: None,
            pending_focus_scroll: false,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            code_rect: Rect::default(),
            visible_comment_capture: None,
            view_mode: ViewMode::Diff,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache,
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
            ai: TuiAiState::empty(),
        }
    }

    fn build_state_with_single_block(content: &str) -> (AppState, TreeNodeId) {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let block = Block::new(content.to_string(), BlockKind::Paragraph, 0, 1);
        let block_id = builder.add_block(
            file,
            "paragraph".to_string(),
            "src/lib.rs".to_string(),
            block,
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = HashSet::from([block_id]);
        let review_order = ReviewOrder::from_tree(&tree, &visible);
        let mut navigator = ReviewNavigator::new(tree, visible.clone())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        navigator.set_current(block_id);

        let state = AppState {
            review_scope: ScopePreset::All,
            navigator,
            review_order,
            total_blocks: 1,
            initial_remaining_blocks: 1,
            remaining_blocks: 1,
            reviewable_nodes: visible,
            commented_nodes: HashSet::new(),
            skipped_nodes: HashSet::new(),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: InputCursor::default(),
            input_draft: None,
            editing_validation: None,
            confirm_batch: BatchConfirmPolicy::Never,
            repo_name: "repo".to_string(),
            repo_root: None,
            file_cache: HashMap::new(),
            root_cursor: None,
            focus_block: Some(block_id),
            pending_focus_scroll: false,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            code_rect: Rect::default(),
            visible_comment_capture: None,
            view_mode: ViewMode::Diff,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
            ai: TuiAiState::empty(),
        };

        (state, block_id)
    }

    struct TestSignal {
        ready: AtomicBool,
        lock: Mutex<()>,
        condvar: Condvar,
    }

    impl TestSignal {
        fn new() -> Self {
            Self {
                ready: AtomicBool::new(false),
                lock: Mutex::new(()),
                condvar: Condvar::new(),
            }
        }

        fn notify(&self) {
            let _guard = self
                .lock
                .lock()
                .unwrap_or_else(|error| panic!("failed to lock signal before notify: {error}"));
            self.ready.store(true, Ordering::Relaxed);
            self.condvar.notify_all();
        }

        fn wait(&self, timeout: Duration, description: &str) {
            if self.ready.load(Ordering::Relaxed) {
                return;
            }

            let guard = self
                .lock
                .lock()
                .unwrap_or_else(|error| panic!("failed to lock {description} signal: {error}"));
            let (_guard, _timeout) = self
                .condvar
                .wait_timeout_while(guard, timeout, |_| !self.ready.load(Ordering::Relaxed))
                .unwrap_or_else(|error| panic!("failed to wait for {description}: {error}"));
            assert!(
                self.ready.load(Ordering::Relaxed),
                "timed out waiting for {description}"
            );
        }
    }

    fn assert_phrase_order(rendered: &str, phrases: &[&str]) {
        let mut previous_index = None;
        for phrase in phrases {
            let index = rendered
                .find(phrase)
                .unwrap_or_else(|| panic!("expected phrase {phrase:?} in {rendered:?}"));
            if let Some(previous_index) = previous_index {
                assert!(
                    previous_index < index,
                    "expected phrases in order {phrases:?}, got {rendered:?}"
                );
            }
            previous_index = Some(index);
        }
    }

    fn build_state_at_root_with_two_files() -> (AppState, TreeNodeId, TreeNodeId) {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let first_file = builder.add_file(
            root,
            "a.rs".to_string(),
            "a.rs".to_string(),
            "a-file-hash".to_string(),
            Language::Rust,
        );
        let first_block = builder.add_block(
            first_file,
            "first".to_string(),
            "a.rs".to_string(),
            Block::new("fn a() {}".to_string(), BlockKind::Function, 0, 1),
            Language::Rust,
        );
        let second_file = builder.add_file(
            root,
            "b.rs".to_string(),
            "b.rs".to_string(),
            "b-file-hash".to_string(),
            Language::Rust,
        );
        let second_block = builder.add_block(
            second_file,
            "second".to_string(),
            "b.rs".to_string(),
            Block::new("fn b() {}".to_string(), BlockKind::Function, 0, 1),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = HashSet::from([first_block, second_block]);
        let review_order = ReviewOrder::from_tree(&tree, &visible);
        let mut navigator = ReviewNavigator::new(tree, visible.clone())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        navigator.jump_root();

        let state = AppState {
            review_scope: ScopePreset::All,
            navigator,
            review_order,
            total_blocks: 2,
            initial_remaining_blocks: 2,
            remaining_blocks: 2,
            reviewable_nodes: visible,
            commented_nodes: HashSet::new(),
            skipped_nodes: HashSet::new(),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: InputCursor::default(),
            input_draft: None,
            editing_validation: None,
            confirm_batch: BatchConfirmPolicy::Never,
            repo_name: "repo".to_string(),
            repo_root: None,
            file_cache: HashMap::new(),
            root_cursor: Some(first_file),
            focus_block: None,
            pending_focus_scroll: false,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            code_rect: Rect::default(),
            visible_comment_capture: None,
            view_mode: ViewMode::Diff,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
            ai: TuiAiState::empty(),
        };

        (state, first_file, second_file)
    }

    fn build_state_with_block_file(
        file_path: &Path,
        file_content: &str,
        block_content: &str,
        block_start_line: usize,
        block_end_line: usize,
    ) -> (AppState, TreeNodeId, TreeNodeId) {
        build_state_with_block_file_metadata(
            file_path,
            file_content,
            block_content,
            block_start_line,
            block_end_line,
            BlockKind::Function,
            Language::Rust,
        )
    }

    fn build_state_with_block_file_metadata(
        file_path: &Path,
        file_content: &str,
        block_content: &str,
        block_start_line: usize,
        block_end_line: usize,
        block_kind: BlockKind,
        language: Language,
    ) -> (AppState, TreeNodeId, TreeNodeId) {
        fs::create_dir_all(file_path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_else(|error| panic!("failed to create fixture directory: {error}"));
        fs::write(file_path, file_content)
            .unwrap_or_else(|error| panic!("failed to write fixture file: {error}"));

        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("fixture.rs")
            .to_string();
        let repo_path = format!("src/{file_name}");

        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            file_name,
            repo_path.clone(),
            "file-hash".to_string(),
            language,
        );
        let block = Block::new(
            block_content.to_string(),
            block_kind,
            block_start_line,
            block_end_line,
        );
        let block_id = builder.add_block(
            file,
            block_kind.as_str().to_string(),
            repo_path.clone(),
            block,
            language,
        );
        let tree = builder.finalize();
        let visible = HashSet::from([block_id]);
        let review_order = ReviewOrder::from_tree(&tree, &visible);
        let mut navigator = ReviewNavigator::new(tree, visible.clone())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        navigator.set_current(block_id);

        let state = AppState {
            review_scope: ScopePreset::All,
            navigator,
            review_order,
            total_blocks: 1,
            initial_remaining_blocks: 1,
            remaining_blocks: 1,
            reviewable_nodes: visible,
            commented_nodes: HashSet::new(),
            skipped_nodes: HashSet::new(),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: InputCursor::default(),
            input_draft: None,
            editing_validation: None,
            confirm_batch: BatchConfirmPolicy::Never,
            repo_name: "repo".to_string(),
            repo_root: None,
            file_cache: HashMap::from([(
                PathBuf::from(&repo_path),
                Arc::from(
                    file_content
                        .lines()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                ),
            )]),
            root_cursor: None,
            focus_block: Some(block_id),
            pending_focus_scroll: false,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            code_rect: Rect::default(),
            visible_comment_capture: None,
            view_mode: ViewMode::Source,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
            ai: TuiAiState::empty(),
        };

        (state, file, block_id)
    }

    fn build_state_with_file_block_count(
        block_count: usize,
    ) -> (AppState, TreeNodeId, Vec<TreeNodeId>) {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let mut block_ids = Vec::new();
        for index in 0..block_count {
            let block_id = builder.add_block(
                file,
                format!("function-{index}"),
                "src/lib.rs".to_string(),
                Block::new(
                    format!("fn block_{index}() {{}}\n"),
                    BlockKind::Function,
                    index,
                    index + 1,
                ),
                Language::Rust,
            );
            block_ids.push(block_id);
        }
        let tree = builder.finalize();
        let visible = block_ids.iter().copied().collect::<HashSet<_>>();
        let review_order = ReviewOrder::from_tree(&tree, &visible);
        let mut navigator = ReviewNavigator::new(tree, visible.clone())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        navigator.set_current(file);

        let state = AppState {
            review_scope: ScopePreset::All,
            navigator,
            review_order,
            total_blocks: visible.len(),
            initial_remaining_blocks: visible.len(),
            remaining_blocks: visible.len(),
            reviewable_nodes: visible,
            commented_nodes: HashSet::new(),
            skipped_nodes: HashSet::new(),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: InputCursor::default(),
            input_draft: None,
            editing_validation: None,
            confirm_batch: BatchConfirmPolicy::Threshold(2),
            repo_name: "repo".to_string(),
            repo_root: None,
            file_cache: HashMap::new(),
            root_cursor: Some(file),
            focus_block: None,
            pending_focus_scroll: false,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            code_rect: Rect::default(),
            visible_comment_capture: None,
            view_mode: ViewMode::Diff,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
            ai: TuiAiState::empty(),
        };

        (state, file, block_ids)
    }

    fn build_state_with_markdown_file(
        repo_path: &str,
        file_content: &str,
    ) -> (AppState, TreeNodeId, TreeNodeId) {
        build_state_with_markdown_blocks(
            repo_path,
            file_content,
            block_splitter::split(file_content, Language::Markdown).blocks,
        )
    }

    fn build_state_with_single_markdown_section(
        repo_path: &str,
        file_content: &str,
    ) -> (AppState, TreeNodeId, TreeNodeId) {
        let end_line = file_content.lines().count();
        build_state_with_markdown_blocks(
            repo_path,
            file_content,
            vec![Block::new(
                file_content.to_string(),
                BlockKind::Section,
                0,
                end_line,
            )],
        )
    }

    fn build_state_with_markdown_blocks(
        repo_path: &str,
        file_content: &str,
        blocks: Vec<Block>,
    ) -> (AppState, TreeNodeId, TreeNodeId) {
        let file_state = FileState::from_text(
            RepoPath::new(repo_path)
                .unwrap_or_else(|error| panic!("invalid repo path {repo_path}: {error}")),
            Language::Markdown,
            file_content.as_bytes(),
            blocks,
        );
        let tree = build_tree_from_files(&[file_state]);
        let file_id = tree
            .find_by_path(repo_path)
            .unwrap_or_else(|| panic!("expected file node for {repo_path}"));
        let top_level_blocks = tree.node(file_id).children.clone();
        let top_section = *top_level_blocks
            .first()
            .unwrap_or_else(|| panic!("expected at least one markdown review block"));
        let visible = top_level_blocks.into_iter().collect::<HashSet<_>>();
        let review_order = ReviewOrder::from_tree(&tree, &visible);
        let mut navigator = ReviewNavigator::new(tree, visible.clone())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        navigator.set_current(file_id);

        let mut state = AppState {
            review_scope: ScopePreset::All,
            navigator,
            review_order,
            total_blocks: visible.len(),
            initial_remaining_blocks: visible.len(),
            remaining_blocks: visible.len(),
            reviewable_nodes: visible,
            commented_nodes: HashSet::new(),
            skipped_nodes: HashSet::new(),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: InputCursor::default(),
            input_draft: None,
            editing_validation: None,
            confirm_batch: BatchConfirmPolicy::Never,
            repo_name: "repo".to_string(),
            repo_root: None,
            file_cache: HashMap::from([(
                PathBuf::from(repo_path),
                Arc::from(
                    file_content
                        .lines()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                ),
            )]),
            root_cursor: None,
            focus_block: None,
            pending_focus_scroll: false,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            code_rect: Rect::default(),
            visible_comment_capture: None,
            view_mode: ViewMode::Source,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
            ai: TuiAiState::empty(),
        };
        set_focus_for_current_node(&mut state, None);

        (state, file_id, top_section)
    }

    #[test]
    fn diff_query_uses_main_diff_for_main_scope() {
        let query = ScopePreset::MainDiff.diff_query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }
    #[test]
    fn diff_query_uses_revision_for_commit_scope() {
        let query = ScopePreset::Commit {
            id: "abc123".to_string(),
            summary: "test".to_string(),
        }
        .diff_query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::Revision {
                revision: "abc123".to_string(),
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_uses_revision_range_for_range_scope() {
        let query = ScopePreset::RevisionRange {
            start: "abc123".to_string(),
            end: "def456".to_string(),
        }
        .diff_query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::RevisionRange {
                start: "abc123".to_string(),
                end: "def456".to_string(),
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_uses_main_diff_for_all_scope() {
        let query = ScopePreset::All.diff_query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }
    #[test]
    fn path_matches_workdir_prefix_matches_exact_and_descendants() {
        assert!(path_utils::path_matches_workdir_prefix(
            "trueflow/src/lib.rs",
            "trueflow"
        ));
        assert!(path_utils::path_matches_workdir_prefix(
            "trueflow", "trueflow"
        ));
        assert!(!path_utils::path_matches_workdir_prefix(
            "other/src/lib.rs",
            "trueflow"
        ));
        assert!(!path_utils::path_matches_workdir_prefix(
            "trueflowish/src/lib.rs",
            "trueflow"
        ));
    }

    #[test]
    fn filter_commits_for_prefix_keeps_only_commits_touching_prefix() {
        let commits = vec![
            vcs::CommitInfo {
                id: CommitId::new("aaaaaaa").unwrap(),
                summary: "touches subtree".to_string(),
            },
            vcs::CommitInfo {
                id: CommitId::new("bbbbbbb").unwrap(),
                summary: "outside subtree".to_string(),
            },
        ];

        let filtered = filter_commits_for_prefix(commits, Some("trueflow"), |revision| {
            let paths = match revision {
                "aaaaaaa" => HashSet::from([RepoPath::new("trueflow/src/lib.rs").unwrap()]),
                "bbbbbbb" => HashSet::from([RepoPath::new("README.md").unwrap()]),
                _ => HashSet::new(),
            };
            Ok(paths)
        });

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, CommitId::new("aaaaaaa").unwrap());
    }

    #[test]
    fn ensure_cached_file_diff_inserts_no_text_changes_on_loader_error() {
        let mut cache = HashMap::new();
        let path = PathBuf::from("src/lib.rs");

        let file_diff = ensure_cached_file_diff(&mut cache, &path, || {
            Err(anyhow::anyhow!("repo unavailable"))
        });

        assert!(
            matches!(file_diff, vcs::FileDiff::NoTextChanges { .. }),
            "expected no-text-changes fallback on load failure"
        );
        assert!(
            cache.contains_key(&path),
            "failed loads should still cache a fallback diff result"
        );
    }

    #[test]
    fn block_diff_focus_mode_defaults_to_whole_block() {
        let config = TuiConfig::default();
        let mode = block_diff_focus_mode_from_config(&config);
        assert_eq!(mode, vcs::BlockDiffFocusMode::WholeBlock);
    }

    #[test]
    fn block_diff_focus_mode_uses_configured_context() {
        let config = TuiConfig {
            confirm_batch_sub_blocks: BatchConfirmPolicy::Threshold(2),
            diff_focus_mode: TuiDiffFocusMode::ChangedWithContext,
            diff_focus_context_lines: 7,
            diff_line_numbers: TuiDiffLineNumbers::Disabled,
            keybinds: crate::config::TuiKeybindsConfig::default(),
            speed_read: crate::config::TuiSpeedReadConfig::default(),
        };
        let mode = block_diff_focus_mode_from_config(&config);
        assert_eq!(
            mode,
            vcs::BlockDiffFocusMode::ChangedWithContext { context_lines: 7 }
        );
    }

    #[test]
    fn keybind_action_uses_default_review_and_scroll_bindings() {
        let keybinds = crate::config::TuiKeybindsConfig::default();
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('h')),
            Some(KeybindAction::Prev)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('j')),
            Some(KeybindAction::Down)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('k')),
            Some(KeybindAction::Up)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('l')),
            Some(KeybindAction::Next)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('P')),
            Some(KeybindAction::Parent)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('C')),
            Some(KeybindAction::Child)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('c')),
            Some(KeybindAction::Note)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('x')),
            None
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Left),
            Some(KeybindAction::Prev)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Up),
            Some(KeybindAction::Up)
        );
    }

    #[test]
    fn keybind_action_uses_configured_overrides() {
        let keybinds = crate::config::TuiKeybindsConfig {
            scroll_up: 'i',
            scroll_down: 'm',
            prev: 'j',
            next: 'l',
            parent: 'u',
            child: 'o',
            approve: 'y',
            note: 'e',
            toggle_view: 'v',
            speed_read: 's',
            root: 'z',
            recap_done: 'd',
            quit: 'x',
        };
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('j')),
            Some(KeybindAction::Prev)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('m')),
            Some(KeybindAction::Down)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('i')),
            Some(KeybindAction::Up)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('u')),
            Some(KeybindAction::Parent)
        );
        assert_eq!(
            keybind_action_for_key_code(&keybinds, KeyCode::Char('e')),
            Some(KeybindAction::Note)
        );
    }

    #[test]
    fn scope_selector_hint_text_uses_configured_keybinds() {
        let keybinds = crate::config::TuiKeybindsConfig {
            scroll_up: 'i',
            scroll_down: 'm',
            prev: 'j',
            next: 'l',
            parent: 'u',
            child: 'o',
            approve: 'y',
            note: 'e',
            toggle_view: 'v',
            speed_read: 's',
            root: 'z',
            recap_done: 'd',
            quit: 'x',
        };
        assert_eq!(
            scope_selector_hint_text(&keybinds),
            "[Enter] select  [m/i] move  [x] quit"
        );
    }

    #[test]
    fn scope_selector_status_from_summary_distinguishes_pending_reviewed_and_empty() {
        let pending_summary = ReviewSummary {
            files: vec![UnreviewedFile {
                path: RepoPath::new("src/lib.rs").unwrap(),
                language: Language::Rust,
                blocks: vec![
                    Block::new("fn alpha() {}".to_string(), BlockKind::Function, 1, 2),
                    Block::new("fn beta() {}".to_string(), BlockKind::Function, 3, 4),
                ],
            }],
            total_blocks: 5,
            diagnostics: Vec::new(),
        };
        assert_eq!(
            ScopeSelectorStatus::from_summary(&pending_summary),
            ScopeSelectorStatus::Pending {
                remaining_blocks: 2,
                total_blocks: 5,
            }
        );

        let reviewed_summary = ReviewSummary {
            files: Vec::new(),
            total_blocks: 3,
            diagnostics: Vec::new(),
        };
        let reviewed_status = ScopeSelectorStatus::from_summary(&reviewed_summary);
        assert_eq!(
            reviewed_status,
            ScopeSelectorStatus::Reviewed { total_blocks: 3 }
        );
        assert_eq!(reviewed_status.label(), "[reviewed]");

        let empty_summary = ReviewSummary {
            files: Vec::new(),
            total_blocks: 0,
            diagnostics: Vec::new(),
        };
        assert_eq!(
            ScopeSelectorStatus::from_summary(&empty_summary),
            ScopeSelectorStatus::Empty
        );
    }

    #[test]
    fn format_scope_selector_option_text_right_aligns_status_and_preserves_it_when_narrow() {
        let wide = format_scope_selector_option_text("> ", "All files", "[reviewed]", 28);
        assert_eq!(UnicodeWidthStr::width(wide.as_str()), 28);
        assert!(
            wide.starts_with("> All files"),
            "unexpected wide row: {wide:?}"
        );
        assert!(
            wide.ends_with("[reviewed]"),
            "unexpected wide row: {wide:?}"
        );

        let narrow = format_scope_selector_option_text(
            "> ",
            "An extremely long review scope label",
            "[4/9 left]",
            18,
        );
        assert_eq!(UnicodeWidthStr::width(narrow.as_str()), 18);
        assert!(
            narrow.ends_with("[4/9 left]"),
            "unexpected narrow row: {narrow:?}"
        );
        assert!(narrow.starts_with(">"), "unexpected narrow row: {narrow:?}");
    }

    #[test]
    fn visible_scope_selector_option_range_keeps_clamped_selection_visible() {
        for option_count in 0..12 {
            for selected in 0..14 {
                for max_visible_options in 0..8 {
                    let range = visible_scope_selector_option_range(
                        option_count,
                        selected,
                        max_visible_options,
                    );
                    if option_count == 0 || max_visible_options == 0 {
                        assert_eq!(range, 0..0);
                        continue;
                    }

                    let clamped_selected = selected.min(option_count - 1);
                    assert!(range.start <= clamped_selected, "range: {range:?}");
                    assert!(clamped_selected < range.end, "range: {range:?}");
                    assert!(range.end <= option_count, "range: {range:?}");
                    assert_eq!(range.len(), option_count.min(max_visible_options));
                }
            }
        }
    }

    #[test]
    fn scope_selector_render_keeps_selected_option_visible_when_options_overflow() {
        let options = (0..10)
            .map(|index| ScopeSelectorOption {
                label: format!("Commit {index:02}"),
                scope: ScopePreset::Commit {
                    id: format!("{index:07}"),
                    summary: format!("Commit {index:02}"),
                },
                status: ScopeSelectorStatus::Deferred,
            })
            .collect::<Vec<_>>();
        let mut selector = ScopeSelector::new(options);
        selector.selected = 9;

        let mut terminal = Terminal::new(TestBackend::new(80, 20))
            .unwrap_or_else(|error| panic!("failed to build test terminal: {error}"));
        terminal
            .draw(|frame| render_scope_selector(frame, &selector, &TuiKeybindsConfig::default()))
            .unwrap_or_else(|error| panic!("failed to render scope selector: {error}"));

        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let screen = buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("> Commit 09"),
            "expected selected overflowing option to stay visible:\n{screen}"
        );
    }

    #[test]
    fn load_scope_selector_with_populates_statuses_and_handles_lookup_failures() {
        let options = vec![
            ScopeOption {
                label: "All files".to_string(),
                scope: ScopePreset::All,
            },
            ScopeOption {
                label: "Diff vs main".to_string(),
                scope: ScopePreset::MainDiff,
            },
            ScopeOption {
                label: "Commit abc1234".to_string(),
                scope: ScopePreset::Commit {
                    id: "abc1234".to_string(),
                    summary: "Update".to_string(),
                },
            },
        ];

        let selector = load_scope_selector_with(
            options,
            &BlockFilters::default(),
            &ScanOptions::default(),
            |scope, _, _| match scope {
                ScopePreset::All => Ok(ReviewSummary {
                    files: Vec::new(),
                    total_blocks: 3,
                    diagnostics: Vec::new(),
                }),
                ScopePreset::MainDiff => Ok(ReviewSummary {
                    files: Vec::new(),
                    total_blocks: 0,
                    diagnostics: Vec::new(),
                }),
                ScopePreset::Commit { .. } => Err(anyhow::anyhow!("lookup failed")),
                ScopePreset::RevisionRange { .. } => panic!("unexpected test scope"),
            },
        )
        .unwrap_or_else(|error| panic!("expected selector with statuses: {error}"));

        assert_eq!(
            selector.options[0].status,
            ScopeSelectorStatus::Reviewed { total_blocks: 3 }
        );
        assert_eq!(selector.options[1].status, ScopeSelectorStatus::Empty);
        assert_eq!(selector.options[2].status, ScopeSelectorStatus::Unavailable);
    }

    #[test]
    fn scope_selector_status_label_includes_checking_and_deferred_markers() {
        assert_eq!(ScopeSelectorStatus::Checking.label(), "[checking...]");
        assert_eq!(ScopeSelectorStatus::Deferred.label(), "[select to scan]");
    }

    #[test]
    fn scope_selector_defaults_to_main_diff_when_available() {
        let selector = ScopeSelector::new(vec![
            ScopeSelectorOption {
                label: "All files".to_string(),
                scope: ScopePreset::All,
                status: ScopeSelectorStatus::Deferred,
            },
            ScopeSelectorOption {
                label: "Diff vs main".to_string(),
                scope: ScopePreset::MainDiff,
                status: ScopeSelectorStatus::Checking,
            },
        ]);

        assert_eq!(selector.selected, 1);
        assert_eq!(selector.selected_scope(), Some(ScopePreset::MainDiff));
    }

    #[test]
    fn build_scope_selector_with_status_jobs_uses_cached_commit_statuses() {
        let filters = BlockFilters::default();
        let scan_options = ScanOptions::default();
        let options = vec![
            ScopeOption {
                label: "All files".to_string(),
                scope: ScopePreset::All,
            },
            ScopeOption {
                label: "Commit abc1234".to_string(),
                scope: ScopePreset::Commit {
                    id: "abc1234".to_string(),
                    summary: "Update".to_string(),
                },
            },
        ];
        let cache_key = review_coverage_status_cache_key(
            &options[1].scope,
            &filters,
            &scan_options,
            Some("src"),
        );
        let cache = ReviewCoverageStatusCacheStore {
            path: PathBuf::from("/tmp/review_coverage_status.json"),
            fingerprint: ReviewDatabaseFingerprint {
                size_bytes: 0,
                modified_unix_ms: 0,
            },
            entries: HashMap::from([(
                cache_key.clone(),
                ScopeSelectorStatus::Reviewed { total_blocks: 7 },
            )]),
            entry_updated_unix_ms: HashMap::from([(cache_key, 1234)]),
            fresh: true,
            dirty: false,
        };

        let (selector, jobs) = build_scope_selector_with_status_jobs(
            options,
            &filters,
            &scan_options,
            Some("src"),
            Some(&cache),
        );

        assert_eq!(selector.options[0].status, ScopeSelectorStatus::Deferred);
        assert_eq!(
            selector.options[1].status,
            ScopeSelectorStatus::Reviewed { total_blocks: 7 }
        );
        assert!(jobs.is_empty());
    }

    #[test]
    fn stale_scope_selector_cache_seeds_statuses_but_refreshes_them() {
        let filters = BlockFilters::default();
        let scan_options = ScanOptions::default();
        let options = vec![
            ScopeOption {
                label: "All files".to_string(),
                scope: ScopePreset::All,
            },
            ScopeOption {
                label: "Commit abc1234".to_string(),
                scope: ScopePreset::Commit {
                    id: "abc1234".to_string(),
                    summary: "Update".to_string(),
                },
            },
        ];
        let all_cache_key =
            review_coverage_status_cache_key(&options[0].scope, &filters, &scan_options, None);
        let commit_cache_key =
            review_coverage_status_cache_key(&options[1].scope, &filters, &scan_options, None);
        let cache = ReviewCoverageStatusCacheStore {
            path: PathBuf::from("/tmp/review_coverage_status.json"),
            fingerprint: ReviewDatabaseFingerprint {
                size_bytes: 99,
                modified_unix_ms: 99,
            },
            entries: HashMap::from([
                (
                    all_cache_key,
                    ScopeSelectorStatus::Pending {
                        remaining_blocks: 2,
                        total_blocks: 3,
                    },
                ),
                (
                    commit_cache_key,
                    ScopeSelectorStatus::Reviewed { total_blocks: 7 },
                ),
            ]),
            entry_updated_unix_ms: HashMap::new(),
            fresh: false,
            dirty: false,
        };

        let (selector, jobs) = build_scope_selector_with_status_jobs(
            options,
            &filters,
            &scan_options,
            None,
            Some(&cache),
        );

        assert_eq!(selector.options[0].status, ScopeSelectorStatus::Deferred);
        assert_eq!(
            selector.options[1].status,
            ScopeSelectorStatus::Reviewed { total_blocks: 7 }
        );
        assert_eq!(jobs.len(), 1);
        assert!(matches!(jobs[0].scope, ScopePreset::Commit { .. }));
    }

    #[test]
    fn scope_selector_status_jobs_skip_expensive_all_scope() {
        let filters = BlockFilters::default();
        let scan_options = ScanOptions::default();
        let options = vec![
            ScopeOption {
                label: "All files".to_string(),
                scope: ScopePreset::All,
            },
            ScopeOption {
                label: "Diff vs main".to_string(),
                scope: ScopePreset::MainDiff,
            },
            ScopeOption {
                label: "Commit abc1234".to_string(),
                scope: ScopePreset::Commit {
                    id: "abc1234".to_string(),
                    summary: "Update".to_string(),
                },
            },
        ];

        let (_selector, jobs) =
            build_scope_selector_with_status_jobs(options, &filters, &scan_options, None, None);

        assert_eq!(jobs.len(), 2);
        assert!(matches!(jobs[0].scope, ScopePreset::MainDiff));
        assert!(matches!(jobs[1].scope, ScopePreset::Commit { .. }));
    }

    #[test]
    fn fresh_volatile_scope_selector_cache_seeds_statuses_but_still_refreshes() {
        let filters = BlockFilters::default();
        let scan_options = ScanOptions::default();
        let options = vec![ScopeOption {
            label: "Diff vs main".to_string(),
            scope: ScopePreset::MainDiff,
        }];
        let cache_key =
            review_coverage_status_cache_key(&options[0].scope, &filters, &scan_options, None);
        let cache = ReviewCoverageStatusCacheStore {
            path: PathBuf::from("/tmp/review_coverage_status.json"),
            fingerprint: ReviewDatabaseFingerprint::default(),
            entries: HashMap::from([(cache_key, ScopeSelectorStatus::Empty)]),
            entry_updated_unix_ms: HashMap::new(),
            fresh: true,
            dirty: false,
        };

        let (selector, jobs) = build_scope_selector_with_status_jobs(
            options,
            &filters,
            &scan_options,
            None,
            Some(&cache),
        );

        assert_eq!(selector.options[0].status, ScopeSelectorStatus::Empty);
        assert_eq!(jobs.len(), 1);
        assert!(matches!(jobs[0].scope, ScopePreset::MainDiff));
    }

    #[test]
    fn scope_selector_status_poller_applies_updates_and_writes_commit_cache_entries() {
        let cache_dir = temp_test_dir("scope_selector_status_cache");
        fs::create_dir_all(&cache_dir)
            .unwrap_or_else(|error| panic!("failed to create cache dir {cache_dir:?}: {error}"));
        let cache_path = cache_dir.join("review_coverage_status.json");
        let (sender, receiver) = mpsc::channel();
        let mut selector = ScopeSelector::new(vec![ScopeSelectorOption {
            label: "Commit abc1234".to_string(),
            scope: ScopePreset::Commit {
                id: "abc1234".to_string(),
                summary: "Update".to_string(),
            },
            status: ScopeSelectorStatus::Checking,
        }]);
        let fingerprint = ReviewDatabaseFingerprint {
            size_bytes: 42,
            modified_unix_ms: 1234,
        };
        let mut poller = ScopeSelectorStatusPoller {
            receiver,
            pending_jobs: 1,
            cache: Some(ReviewCoverageStatusCacheStore {
                path: cache_path.clone(),
                fingerprint,
                entries: HashMap::new(),
                entry_updated_unix_ms: HashMap::new(),
                fresh: true,
                dirty: false,
            }),
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        sender
            .send(ScopeSelectorStatusUpdate {
                index: 0,
                status: ScopeSelectorStatus::Reviewed { total_blocks: 3 },
                cache_key: "commit:abc1234".to_string(),
            })
            .unwrap_or_else(|error| panic!("failed to send selector update: {error}"));
        drop(sender);

        assert!(poller.drain_updates(&mut selector));
        assert_eq!(poller.pending_jobs, 0);
        assert_eq!(
            selector.options[0].status,
            ScopeSelectorStatus::Reviewed { total_blocks: 3 }
        );

        let cache_contents = fs::read_to_string(&cache_path)
            .unwrap_or_else(|error| panic!("failed to read cache file {cache_path:?}: {error}"));
        let cache_file: ReviewCoverageStatusCacheFile = serde_json::from_str(&cache_contents)
            .unwrap_or_else(|error| panic!("failed to parse cache json: {error}"));
        assert_eq!(cache_file.review_db_fingerprint, fingerprint);
        assert_eq!(
            cache_file.entries.get("commit:abc1234"),
            Some(&ScopeSelectorStatus::Reviewed { total_blocks: 3 })
        );
        assert!(
            cache_file
                .entry_updated_unix_ms
                .contains_key("commit:abc1234")
        );
    }

    #[test]
    fn scope_selector_status_poller_reports_unavailable_when_status_loader_panics() {
        let jobs = vec![ScopeSelectorStatusJob {
            index: 0,
            scope: ScopePreset::MainDiff,
            cache_key: "main-diff".to_string(),
        }];
        let mut selector = ScopeSelector::new(vec![ScopeSelectorOption {
            label: "Diff vs main".to_string(),
            scope: ScopePreset::MainDiff,
            status: ScopeSelectorStatus::Checking,
        }]);
        let mut poller = ScopeSelectorStatusPoller::spawn_with_loader(jobs, None, move |_| {
            panic!("simulated diff status panic");
        })
        .unwrap_or_else(|| panic!("expected status poller"));

        let changed = poller.wait_for_update(&mut selector, Duration::from_secs(1));

        assert!(changed, "expected panic to produce status update");
        assert_eq!(poller.pending_jobs, 0);
        assert_eq!(selector.options[0].status, ScopeSelectorStatus::Unavailable);
    }

    #[test]
    fn scope_selector_status_poller_marks_checking_rows_unavailable_on_worker_disconnect() {
        let (sender, receiver) = mpsc::channel::<ScopeSelectorStatusUpdate>();
        drop(sender);
        let mut selector = ScopeSelector::new(vec![ScopeSelectorOption {
            label: "Diff vs main".to_string(),
            scope: ScopePreset::MainDiff,
            status: ScopeSelectorStatus::Checking,
        }]);
        let mut poller = ScopeSelectorStatusPoller {
            receiver,
            pending_jobs: 1,
            cache: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        assert!(poller.drain_updates(&mut selector));
        assert_eq!(poller.pending_jobs, 0);
        assert_eq!(selector.options[0].status, ScopeSelectorStatus::Unavailable);
    }

    #[test]
    fn scope_selector_status_poller_bounds_background_checkers_and_cancels_queued_jobs() {
        let jobs = (0..3)
            .map(|index| ScopeSelectorStatusJob {
                index,
                scope: ScopePreset::Commit {
                    id: format!("commit-{index}"),
                    summary: "Update".to_string(),
                },
                cache_key: format!("commit:{index}"),
            })
            .collect::<Vec<_>>();
        let started_jobs = Arc::new(AtomicBool::new(false));
        let started_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_job_started = Arc::new(TestSignal::new());
        let release_first_job = Arc::new(TestSignal::new());
        let first_job_finished = Arc::new(TestSignal::new());
        let started_count_for_loader = Arc::clone(&started_count);
        let started_jobs_for_loader = Arc::clone(&started_jobs);
        let first_job_started_for_loader = Arc::clone(&first_job_started);
        let release_first_job_for_loader = Arc::clone(&release_first_job);
        let first_job_finished_for_loader = Arc::clone(&first_job_finished);
        let poller = ScopeSelectorStatusPoller::spawn_with_loader(jobs, None, move |_| {
            started_jobs_for_loader.store(true, Ordering::Relaxed);
            let previous_starts = started_count_for_loader.fetch_add(1, Ordering::Relaxed);
            if previous_starts == 0 {
                first_job_started_for_loader.notify();
                release_first_job_for_loader.wait(Duration::from_secs(1), "first-job release");
                first_job_finished_for_loader.notify();
            }
            ScopeSelectorStatus::Reviewed { total_blocks: 1 }
        })
        .unwrap_or_else(|| panic!("expected status poller"));

        first_job_started.wait(Duration::from_secs(1), "first job to start");
        assert!(started_jobs.load(Ordering::Relaxed));
        assert_eq!(started_count.load(Ordering::Relaxed), 1);

        poller.cancel();
        drop(poller);
        release_first_job.notify();
        first_job_finished.wait(Duration::from_secs(1), "first job to finish");

        assert_eq!(started_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stale_cache_record_flushes_current_review_database_fingerprint_even_when_status_matches() {
        let cache_dir = temp_test_dir("scope_selector_status_cache_stale_flush");
        fs::create_dir_all(&cache_dir)
            .unwrap_or_else(|error| panic!("failed to create cache dir {cache_dir:?}: {error}"));
        let cache_path = cache_dir.join("review_coverage_status.json");
        let fingerprint = ReviewDatabaseFingerprint {
            size_bytes: 42,
            modified_unix_ms: 1234,
        };
        let mut cache = ReviewCoverageStatusCacheStore {
            path: cache_path.clone(),
            fingerprint,
            entries: HashMap::from([(
                "commit:abc1234".to_string(),
                ScopeSelectorStatus::Reviewed { total_blocks: 3 },
            )]),
            entry_updated_unix_ms: HashMap::new(),
            fresh: false,
            dirty: false,
        };

        cache.record(
            "commit:abc1234",
            ScopeSelectorStatus::Reviewed { total_blocks: 3 },
        );
        cache.flush();

        let cache_contents = fs::read_to_string(&cache_path)
            .unwrap_or_else(|error| panic!("failed to read cache file {cache_path:?}: {error}"));
        let cache_file: ReviewCoverageStatusCacheFile = serde_json::from_str(&cache_contents)
            .unwrap_or_else(|error| panic!("failed to parse cache json: {error}"));
        assert_eq!(cache_file.review_db_fingerprint, fingerprint);
    }

    #[test]
    fn review_coverage_status_cache_store_prunes_oldest_entries_when_over_bound() {
        let mut store = ReviewCoverageStatusCacheStore {
            path: PathBuf::from("/tmp/review_coverage_status.json"),
            fingerprint: ReviewDatabaseFingerprint::default(),
            entries: HashMap::new(),
            entry_updated_unix_ms: HashMap::new(),
            fresh: true,
            dirty: false,
        };

        for index in 0..(REVIEW_COVERAGE_STATUS_CACHE_MAX_ENTRIES + 2) {
            let key = format!("commit:{index:03}");
            store.entries.insert(
                key.clone(),
                ScopeSelectorStatus::Reviewed {
                    total_blocks: index,
                },
            );
            store
                .entry_updated_unix_ms
                .insert(key, u64::try_from(index).unwrap_or(u64::MAX));
        }

        assert!(store.prune_to_bound());
        assert_eq!(
            store.entries.len(),
            REVIEW_COVERAGE_STATUS_CACHE_MAX_ENTRIES
        );
        assert!(!store.entries.contains_key("commit:000"));
        assert!(!store.entries.contains_key("commit:001"));
        assert!(store.entries.contains_key("commit:002"));
        assert!(!store.entry_updated_unix_ms.contains_key("commit:000"));
        assert!(!store.entry_updated_unix_ms.contains_key("commit:001"));
    }

    #[test]
    fn load_review_coverage_status_cache_file_defaults_missing_entry_timestamps() {
        let cache_dir = temp_test_dir("scope_selector_status_cache_legacy");
        fs::create_dir_all(&cache_dir)
            .unwrap_or_else(|error| panic!("failed to create cache dir {cache_dir:?}: {error}"));
        let cache_path = cache_dir.join("review_coverage_status.json");
        let legacy = serde_json::json!({
            "format_version": REVIEW_COVERAGE_STATUS_CACHE_FORMAT_VERSION,
            "review_db_fingerprint": {
                "size_bytes": 42,
                "modified_unix_ms": 1234
            },
            "entries": {
                "commit:abc1234": {
                    "Reviewed": {
                        "total_blocks": 3
                    }
                }
            }
        });
        fs::write(
            &cache_path,
            format!("{}\n", serde_json::to_string_pretty(&legacy).unwrap()),
        )
        .unwrap_or_else(|error| panic!("failed to write legacy cache file: {error}"));

        let cache = load_review_coverage_status_cache_file(
            &cache_path,
            ReviewDatabaseFingerprint {
                size_bytes: 42,
                modified_unix_ms: 1234,
            },
        )
        .unwrap_or_else(|| panic!("expected legacy cache file to load"));

        assert_eq!(
            cache.entries.get("commit:abc1234"),
            Some(&ScopeSelectorStatus::Reviewed { total_blocks: 3 })
        );
        assert!(cache.entry_updated_unix_ms.is_empty());
    }

    #[test]
    fn build_action_lines_use_root_spatial_navigation_labels() {
        let keybinds = crate::config::TuiKeybindsConfig {
            scroll_up: 'i',
            scroll_down: 'm',
            prev: 'j',
            next: 'l',
            parent: 'u',
            child: 'o',
            approve: 'y',
            note: 'e',
            toggle_view: 'v',
            speed_read: 's',
            root: 'z',
            recap_done: 'd',
            quit: 'x',
        };
        let palette = UiPalette::default();
        let lines = build_action_lines(80, UiMode::Navigation, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert_eq!(lines.len(), 1);
        assert!(joined.contains("[m/i]move"));
        assert!(joined.contains("[l/o/Enter]open"));
        assert!(joined.contains("[j/u]back"));
        assert!(!joined.contains('↓'));
        assert!(!joined.contains('↑'));
        assert!(!joined.contains('←'));
        assert!(!joined.contains('→'));
        assert!(joined.contains("[y]approve"));
        assert!(joined.contains("[e]note"));
        assert!(joined.contains("[x]quit"));
        assert!(!joined.contains("prev/next"));
        assert!(!joined.contains("line-scroll"));
        assert!(!joined.contains("root"));
    }

    #[test]
    fn build_action_lines_for_review_nodes_use_mode_label_and_configured_keys() {
        let keybinds = crate::config::TuiKeybindsConfig::default();
        let palette = UiPalette::default();
        let lines = build_action_lines(120, UiMode::DiffReview, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert_eq!(lines.len(), 1);
        assert!(!joined.contains("[r]speed"));
        assert!(joined.contains("[a]pprove"));
        assert!(joined.contains("[c]omment"));
        assert!(joined.contains("[m]ode"));
        assert!(joined.contains("[P]arent"));
        assert!(joined.contains("[C]hild"));
        assert!(joined.contains("[Space]advance"));
        assert!(joined.contains("[PgUp/PgDown]"));
        assert!(joined.contains("[h]prev"));
        assert!(joined.contains("[l]next"));
        assert!(joined.contains("[j]down"));
        assert!(joined.contains("[k]up"));
        assert!(joined.contains("[q]uit"));
        assert!(!joined.contains("[m]source"));
        assert!(!joined.contains("[m]diff"));
        assert!(!joined.contains('↓'));
        assert!(!joined.contains('↑'));
        assert!(!joined.contains('←'));
        assert!(!joined.contains('→'));
        assert!(!joined.contains("line-scroll"));
        assert!(!joined.contains("page-scroll"));
        assert!(!joined.contains("speed-read"));
        assert!(!joined.contains("root"));
        assert!(!joined.contains("[c]comment"));
        assert!(!joined.contains("[q]quit"));

        assert_phrase_order(
            &joined,
            &[
                "[a]pprove",
                "[c]omment",
                "[m]ode",
                "[P]arent",
                "[C]hild",
                "[Space]advance",
                "[PgUp/PgDown]",
                "[h]prev",
                "[l]next",
                "[j]down",
                "[k]up",
                "[q]uit",
            ],
        );
        assert!(
            joined.ends_with("[q]uit"),
            "expected quit to be last: {joined}"
        );
    }

    #[test]
    fn build_action_lines_for_source_review_use_mode_toggle_label() {
        let keybinds = crate::config::TuiKeybindsConfig::default();
        let palette = UiPalette::default();
        let lines = build_action_lines(120, UiMode::SourceReview, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert!(joined.contains("[m]ode"));
        assert!(!joined.contains("[m]diff"));
        assert!(!joined.contains("[m]source"));
    }

    #[test]
    fn build_action_lines_for_speed_read_mode_use_speed_read_controls() {
        let keybinds = crate::config::TuiKeybindsConfig::default();
        let palette = UiPalette::default();
        let lines = build_action_lines(120, UiMode::SpeedRead, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert!(joined.contains("[Space]play/pause"));
        assert!(joined.contains("[j]prev"));
        assert!(joined.contains("[l]next"));
        assert!(joined.contains("[-/=]wpm"));
        assert!(joined.contains("[[/]]words"));
        assert!(joined.contains("[0]reset"));
        assert!(joined.contains("[r/Esc]exit"));
    }

    #[test]
    fn build_action_lines_for_speed_read_mode_use_configured_exit_key() {
        let keybinds = crate::config::TuiKeybindsConfig {
            speed_read: 's',
            ..crate::config::TuiKeybindsConfig::default()
        };
        let palette = UiPalette::default();
        let lines = build_action_lines(120, UiMode::SpeedRead, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert!(joined.contains("[s/Esc]exit"));
        assert!(!joined.contains("[r/Esc]exit"));
    }

    #[test]
    fn build_action_lines_for_recap_mode_only_show_recap_actions() {
        let keybinds = crate::config::TuiKeybindsConfig {
            recap_done: 'f',
            quit: 'x',
            ..crate::config::TuiKeybindsConfig::default()
        };
        let palette = UiPalette::default();
        let lines = build_action_lines(80, UiMode::Recap, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert!(joined.contains("[f]choose scope"));
        assert!(joined.contains("[x]quit"));
        assert!(!joined.contains("approve"));
        assert!(!joined.contains("note"));
        assert!(!joined.contains("speed"));
        assert!(!joined.contains("source"));
        assert!(!joined.contains("diff"));
        assert!(!joined.contains("Esc"));
    }

    #[test]
    fn build_action_lines_wrap_when_width_is_narrow() {
        let keybinds = crate::config::TuiKeybindsConfig::default();
        let palette = UiPalette::default();
        let lines = build_action_lines(60, UiMode::DiffReview, &keybinds, &palette);
        let joined = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join(" ");

        assert!(lines.len() > 1, "expected narrow action area to wrap");
        assert_phrase_order(
            &joined,
            &[
                "[a]pprove",
                "[c]omment",
                "[m]ode",
                "[P]arent",
                "[C]hild",
                "[Space]advance",
                "[PgUp/PgDown]",
                "[h]prev",
                "[l]next",
                "[j]down",
                "[k]up",
                "[q]uit",
            ],
        );
    }

    #[test]
    fn build_action_lines_use_comment_label_when_note_key_is_c() {
        let keybinds = crate::config::TuiKeybindsConfig {
            note: 'c',
            ..crate::config::TuiKeybindsConfig::default()
        };
        let palette = UiPalette::default();
        let lines = build_action_lines(80, UiMode::DiffReview, &keybinds, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let joined = rendered.join(" ");

        assert!(joined.contains("[c]omment"));
        assert!(!joined.contains("[c]comment"));
    }

    #[test]
    fn handle_scroll_line_down_moves_root_cursor_down() {
        let (mut state, _first_file, second_file) = build_state_at_root_with_two_files();

        handle_scroll_line_down(&mut state);

        assert_eq!(state.navigator.current_id(), state.navigator.tree.root());
        assert_eq!(state.root_cursor, Some(second_file));
    }

    #[test]
    fn scroll_offset_to_keep_line_visible_scrolls_down_for_offscreen_selection() {
        let offset = scroll_offset_to_keep_line_visible(0, 4, 8, 16);
        assert_eq!(offset, 5);
    }

    #[test]
    fn scroll_offset_to_keep_line_visible_keeps_visible_selection_stable() {
        let offset = scroll_offset_to_keep_line_visible(4, 6, 7, 20);
        assert_eq!(offset, 4);
    }

    #[test]
    fn handle_scroll_line_up_moves_root_cursor_up() {
        let (mut state, first_file, second_file) = build_state_at_root_with_two_files();
        state.root_cursor = Some(second_file);

        handle_scroll_line_up(&mut state);

        assert_eq!(state.navigator.current_id(), state.navigator.tree.root());
        assert_eq!(state.root_cursor, Some(first_file));
    }

    #[test]
    fn handle_prev_is_noop_at_root() {
        let (mut state, _first_file, second_file) = build_state_at_root_with_two_files();
        state.root_cursor = Some(second_file);

        handle_prev(&mut state);

        assert_eq!(state.navigator.current_id(), state.navigator.tree.root());
        assert_eq!(state.root_cursor, Some(second_file));
    }

    #[test]
    fn handle_next_opens_selected_root_item() {
        let (mut state, _first_file, second_file) = build_state_at_root_with_two_files();
        state.root_cursor = Some(second_file);

        handle_next(&mut state);

        assert_eq!(state.navigator.current_id(), second_file);
    }

    #[test]
    fn cli_review_request_returns_none_without_cli_overrides() {
        let request = cli_review_request(false, &[], None, &[], &[])
            .unwrap_or_else(|error| panic!("expected no parse error: {error}"));
        assert!(request.is_none());
    }
    #[test]
    fn cli_review_request_file_target_uses_main_diff_scope() {
        let targets = vec![ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap())];
        let request = cli_review_request(false, &targets, None, &[], &[])
            .unwrap_or_else(|error| panic!("expected file target request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(
            request.request,
            ReviewRequest::Targets(vec![ReviewTarget::File(
                RepoPath::new("src/lib.rs").unwrap()
            )])
        );
        assert_eq!(request.scope, ScopePreset::MainDiff);
        assert_eq!(request.scope_label, "file src/lib.rs");
        assert_eq!(request.initial_view_mode, ViewMode::Source);
    }

    #[test]
    fn cli_review_request_main_diff_target_uses_diff_initial_mode() {
        let targets = vec![ReviewTarget::MainDiff];
        let request = cli_review_request(false, &targets, None, &[], &[])
            .unwrap_or_else(|error| panic!("expected main diff request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(request.initial_view_mode, ViewMode::Diff);
    }

    #[test]
    fn cli_review_request_revision_range_target_uses_revision_range_scope() {
        let targets = vec![ReviewTarget::RevisionRange(
            crate::commands::review::RevisionRangeExpr::new("abc1234", "def5678").unwrap(),
        )];
        let request = cli_review_request(false, &targets, None, &[], &[])
            .unwrap_or_else(|error| panic!("expected revision range request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(
            request.request,
            ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(
                crate::commands::review::RevisionRangeExpr::new("abc1234", "def5678").unwrap(),
            )])
        );
        assert_eq!(
            request.scope,
            ScopePreset::RevisionRange {
                start: "abc1234".to_string(),
                end: "def5678".to_string(),
            }
        );
        assert_eq!(request.scope_label, "revisions abc1234..def5678");
    }

    #[test]
    fn cli_review_request_only_exclude_without_targets_is_supported() {
        let only = vec![BlockKind::Function];
        let exclude = vec![BlockKind::Comment];
        let request = cli_review_request(false, &[], None, &only, &exclude)
            .unwrap_or_else(|error| panic!("expected filter-only request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(
            request.request,
            ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree])
        );
        assert_eq!(request.scope, ScopePreset::MainDiff);
        assert_eq!(request.scope_label, "dirty worktree");
    }
    #[test]
    fn cli_review_request_errors_when_all_is_combined_with_targets() {
        let targets = vec![ReviewTarget::DirtyWorktree];
        let request = cli_review_request(true, &targets, None, &[], &[]);
        assert!(request.is_err());
    }

    #[test]
    fn resolve_pull_request_target_for_tui_accepts_single_pr_target() {
        let pull_request = resolve_pull_request_target_for_tui(
            false,
            &[ReviewTarget::from_cli("pr:11").unwrap()],
            None,
        )
        .unwrap_or_else(|error| panic!("expected pull request target: {error}"));
        assert_eq!(pull_request, Some(PullRequestRef::Number { number: 11 }));
    }

    #[test]
    fn resolve_pull_request_target_for_tui_rejects_pr_target_with_all() {
        let err = resolve_pull_request_target_for_tui(
            true,
            &[ReviewTarget::from_cli("pr:11").unwrap()],
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Explicit review targets cannot be combined with --all")
        );
    }

    #[test]
    fn build_pull_request_cli_requests_orders_commits_oldest_to_newest() {
        let metadata = crate::github::PullRequestMetadata {
            pr: crate::github::ResolvedPullRequestRef {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            },
            title: "Add PR review support".to_string(),
            base_ref: "main".to_string(),
            base_sha: CommitId::new("1111111111111111111111111111111111111111").unwrap(),
            head_ref: "feature/pr-review".to_string(),
            head_sha: CommitId::new("3333333333333333333333333333333333333333").unwrap(),
            commits: vec![
                crate::github::PullRequestCommit {
                    sha: CommitId::new("2222222222222222222222222222222222222222").unwrap(),
                    summary: "Seed review flow".to_string(),
                },
                crate::github::PullRequestCommit {
                    sha: CommitId::new("3333333333333333333333333333333333333333").unwrap(),
                    summary: "Fetch PR refs".to_string(),
                },
            ],
        };

        let requests = build_pull_request_cli_requests(&metadata)
            .unwrap_or_else(|error| panic!("expected pull request review sequence: {error}"));
        let requests = requests.into_iter().collect::<Vec<_>>();

        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].request,
            ReviewRequest::Targets(vec![ReviewTarget::Revision(
                RevisionExpr::new("2222222222222222222222222222222222222222").unwrap()
            )])
        );
        assert_eq!(
            requests[1].request,
            ReviewRequest::Targets(vec![ReviewTarget::Revision(
                RevisionExpr::new("3333333333333333333333333333333333333333").unwrap()
            )])
        );
        assert!(requests[0].scope_label.contains("PR #11"));
        assert!(requests[0].scope_label.contains("[1/2]"));
        assert!(requests[1].scope_label.contains("[2/2]"));
        assert!(requests[1].scope_label.contains("Fetch PR refs"));
    }

    #[test]
    fn cli_review_request_since_uses_revision_range_scope() {
        let request =
            cli_review_request_with(false, &[], Some("HEAD"), &[], &[], |all, target, since| {
                let targets = crate::commands::review::expand_cli_review_targets_with(
                    target,
                    since,
                    &|_| Ok(()),
                )?;
                let request =
                    crate::commands::review::review_request_from_cli_targets(all, &targets)?;
                let (scope, scope_label) = scope_preset_for_cli_targets(all, &targets);
                Ok(CliReviewRequest {
                    request,
                    scope,
                    scope_label,
                    initial_view_mode: initial_view_mode_for_cli_targets(all, &targets),
                })
            })
            .unwrap_or_else(|error| panic!("expected since request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(
            request.request,
            ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(
                crate::commands::review::RevisionRangeExpr::new("HEAD", "HEAD").unwrap(),
            )])
        );
        assert_eq!(
            request.scope,
            ScopePreset::RevisionRange {
                start: "HEAD".to_string(),
                end: "HEAD".to_string(),
            }
        );
        assert_eq!(request.scope_label, "revisions HEAD..HEAD");
    }

    #[test]
    fn build_review_state_starts_direct_launch_on_first_reviewable_block() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let block = Block::new("fn helper() {}\n".to_string(), BlockKind::Function, 3, 4);
        let block_id = builder.add_block(
            file,
            "helper".to_string(),
            "src/lib.rs".to_string(),
            block.clone(),
            Language::Rust,
        );
        let tree = builder.finalize();
        let review = CollectedReview {
            summary: ReviewSummary {
                files: vec![UnreviewedFile {
                    path: RepoPath::new("src/lib.rs").unwrap(),
                    language: Language::Rust,
                    blocks: vec![block],
                }],
                total_blocks: 1,
                diagnostics: Vec::<ReviewDiagnostic>::new(),
            },
            tree,
            unreviewed_block_nodes: HashSet::from([block_id]),
            commented_block_nodes: HashSet::from([block_id]),
            diff_block_sides: HashMap::new(),
            file_change_kinds: HashMap::new(),
            block_change_kinds: HashMap::new(),
        };
        let context = TrueflowContext::new(Cli::parse_from(["trueflow", "tui"]));

        let state = build_review_state(
            &context,
            review,
            ScopePreset::RevisionRange {
                start: "abc1234".to_string(),
                end: "HEAD".to_string(),
            },
            ReviewStateBuildOptions {
                confirm_batch: BatchConfirmPolicy::Never,
                block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
                diff_line_numbers: TuiDiffLineNumbers::Disabled,
                keybinds: TuiKeybindsConfig::default(),
                scope_label: "rev:abc1234..HEAD".to_string(),
                initial_view_mode: ViewMode::Source,
                speed_read_config: TuiSpeedReadConfig::default(),
                speed_read_config_path: PathBuf::from("trueflow.toml"),
                ai: TuiAiState::empty(),
            },
        )
        .unwrap_or_else(|error| panic!("expected review state: {error}"));

        assert_eq!(state.navigator.current_id(), block_id);
        assert_eq!(state.focus_block, Some(block_id));
        assert!(state.pending_focus_scroll);
        assert_eq!(state.root_cursor, Some(src));
        assert!(state.commented_nodes.contains(&block_id));
        assert_ne!(state.navigator.current_id(), state.navigator.tree.root());
        assert_eq!(state.view_mode, ViewMode::Source);
    }

    #[test]
    fn format_diff_overlay_row_renders_disabled_line_numbers() {
        let line = vcs::DiffLine {
            kind: vcs::DiffLineKind::Added,
            old_line: None,
            new_line: Some(42),
            text: "let x = 1;".to_string(),
            is_focus: true,
        };
        assert_eq!(
            format_diff_overlay_row(&line, TuiDiffLineNumbers::Disabled),
            "+ let x = 1;".to_string()
        );
    }

    #[test]
    fn format_diff_overlay_row_renders_old_new_gutter_when_enabled() {
        let line = vcs::DiffLine {
            kind: vcs::DiffLineKind::Added,
            old_line: None,
            new_line: Some(42),
            text: "let x = 1;".to_string(),
            is_focus: true,
        };
        assert_eq!(
            format_diff_overlay_row(&line, TuiDiffLineNumbers::OldNew),
            "         42 + let x = 1;".to_string()
        );
    }

    #[test]
    fn format_diff_overlay_row_expands_tabs_before_rendering() {
        let line = vcs::DiffLine {
            kind: vcs::DiffLineKind::Context,
            old_line: Some(7),
            new_line: Some(9),
            text: "xx\tfoo".to_string(),
            is_focus: true,
        };
        assert_eq!(
            format_diff_overlay_row(&line, TuiDiffLineNumbers::Disabled),
            "  xx      foo".to_string()
        );
    }

    #[test]
    fn diff_mode_context_rows_are_dimmed_outside_focus_span() {
        let row = ContextualDiffRow {
            kind: vcs::DiffLineKind::Context,
            old_line: Some(7),
            new_line: Some(9),
            anchor_index: 12,
            text: "let stable = true;".to_string(),
        };
        let palette = UiPalette::default();

        let style = style_for_contextual_diff_row(&row, Some(&(20..25)), &palette);

        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn diff_mode_context_rows_are_dimmed_inside_focus_span() {
        let row = ContextualDiffRow {
            kind: vcs::DiffLineKind::Context,
            old_line: Some(7),
            new_line: Some(9),
            anchor_index: 12,
            text: "let stable = true;".to_string(),
        };
        let palette = UiPalette::default();

        let style = style_for_contextual_diff_row(&row, Some(&(10..15)), &palette);

        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn expand_tabs_for_display_accounts_for_wide_characters_before_tab() {
        assert_eq!(
            expand_tabs_for_display("界\tfoo"),
            "界      foo".to_string()
        );
    }

    #[test]
    fn key_code_for_press_event_extracts_key_code() {
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(key_code_for_press_event(&event), Some(KeyCode::Char('j')));
    }

    #[test]
    fn key_code_for_press_event_ignores_non_press_keys() {
        let event = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(key_code_for_press_event(&event), None);
    }

    #[test]
    fn key_code_for_press_or_repeat_event_extracts_repeat_key_code() {
        let event = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(
            key_code_for_press_or_repeat_event(&event),
            Some(KeyCode::Char('j'))
        );
    }

    #[test]
    fn should_rerender_on_event_handles_resize_only() {
        let resize = Event::Resize(120, 40);
        let key = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('k'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(should_rerender_on_event(&resize));
        assert!(!should_rerender_on_event(&key));
    }

    #[test]
    fn coalesce_resize_event_returns_last_resize_in_burst() {
        let mut pending = None;
        let queued = std::cell::RefCell::new(std::collections::VecDeque::from([
            Event::Resize(121, 40),
            Event::Resize(122, 41),
        ]));

        let event = coalesce_resize_event_with(
            Event::Resize(120, 40),
            &mut pending,
            || Ok(!queued.borrow().is_empty()),
            || {
                let event = queued
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_else(|| panic!("expected queued event"));
                Ok(event)
            },
        )
        .unwrap_or_else(|error| panic!("expected coalesced resize: {error}"));

        assert_eq!(event, Event::Resize(122, 41));
        assert!(pending.is_none());
    }

    #[test]
    fn coalesce_resize_event_preserves_following_non_resize_event() {
        let key_event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let mut pending = None;
        let queued = std::cell::RefCell::new(std::collections::VecDeque::from([
            Event::Resize(122, 41),
            key_event,
        ]));

        let event = coalesce_resize_event_with(
            Event::Resize(120, 40),
            &mut pending,
            || Ok(!queued.borrow().is_empty()),
            || {
                let event = queued
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_else(|| panic!("expected queued event"));
                Ok(event)
            },
        )
        .unwrap_or_else(|error| panic!("expected coalesced resize: {error}"));

        assert_eq!(event, Event::Resize(122, 41));
        let pending = pending.unwrap_or_else(|| panic!("expected pending key"));
        assert_eq!(key_code_for_press_event(&pending), Some(KeyCode::Char('j')));
    }

    #[test]
    fn event_pump_read_with_deadline_returns_pending_event_before_timeout() {
        let pending_key = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let mut event_pump = EventPump {
            pending: Some(pending_key),
        };

        let event = event_pump
            .read_with_deadline(Some(Instant::now()))
            .unwrap_or_else(|error| panic!("expected pending event: {error}"));

        let event = event.unwrap_or_else(|| panic!("expected pending event"));
        assert_eq!(key_code_for_press_event(&event), Some(KeyCode::Char('j')));
        assert!(event_pump.pending.is_none());
    }

    #[test]
    fn handle_mouse_event_scrolls_when_pointer_is_inside_code_pane() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.code_rect = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 12,
        };
        state.total_blocks = 1;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 1;
        state.content_height = 40;
        state.viewport_height = 12;
        state.scroll_offset = 4;

        let rerender = handle_mouse_event(
            &mut state,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 12,
                row: 8,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );

        assert!(rerender);
        assert_eq!(state.scroll_offset, 7);
    }

    #[test]
    fn handle_mouse_event_ignores_scroll_outside_code_pane() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.code_rect = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 12,
        };
        state.total_blocks = 1;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 1;
        state.content_height = 40;
        state.viewport_height = 12;
        state.scroll_offset = 4;

        let rerender = handle_mouse_event(
            &mut state,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 4,
                row: 8,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );

        assert!(!rerender);
        assert_eq!(state.scroll_offset, 4);
    }

    #[test]
    fn handle_mouse_event_ignores_scroll_while_editing() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.code_rect = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 12,
        };
        state.total_blocks = 1;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 1;
        state.content_height = 40;
        state.viewport_height = 12;
        state.scroll_offset = 4;
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };

        let rerender = handle_mouse_event(
            &mut state,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 12,
                row: 8,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );

        assert!(!rerender);
        assert_eq!(state.scroll_offset, 4);
    }

    #[test]
    fn input_overlay_rect_anchors_to_bottom() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let rect = input_overlay_rect(area, 3);
        assert_eq!(rect.width, 96);
        assert_eq!(rect.height, 5);
        assert_eq!(rect.y + rect.height, area.y + area.height);
    }

    #[test]
    fn input_overlay_rect_grows_for_multiline_editing() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let rect = input_overlay_rect(area, 6);
        assert_eq!(rect.height, 8);
    }

    #[test]
    fn editing_input_lines_counts_soft_wrapped_single_line_content() {
        let content = "x".repeat(200);
        let wrapped = editing_input_lines(&content, 30);
        assert!(wrapped > 1, "expected soft wrapping to increase line count");
    }

    #[test]
    fn editing_input_lines_counts_display_width_for_wide_characters() {
        assert_eq!(editing_input_lines("界界界", 4), 3);
    }

    #[test]
    fn editing_cursor_visual_offset_counts_display_width_for_wide_characters() {
        assert_eq!(editing_cursor_visual_offset("界a", "界".len(), 4), (2, 0));
        assert_eq!(editing_cursor_visual_offset("界a", "界a".len(), 4), (3, 0));
    }

    #[test]
    fn input_overlay_rect_grows_for_soft_wrapped_editing_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };
        let body_lines = input_overlay_body_lines(
            &"x".repeat(200),
            None,
            "Enter to submit • Ctrl+J newline • Esc to cancel",
            input_overlay_width(area.width),
        );
        let rect = input_overlay_rect(area, body_lines);
        assert!(rect.height > 5);
    }

    #[test]
    fn input_overlay_rect_grows_for_confirm_batch_message_and_hint() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let rect = input_overlay_rect(area, 3);
        assert_eq!(rect.height, 5);
    }

    #[test]
    fn input_overlay_rect_grows_for_soft_wrapped_confirm_batch_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };
        let body_lines = input_overlay_body_lines(
            &"x".repeat(200),
            None,
            "Enter to confirm • Esc to cancel",
            input_overlay_width(area.width),
        );
        let rect = input_overlay_rect(area, body_lines);
        assert!(rect.height > 5);
    }

    #[test]
    fn input_overlay_rect_clamps_with_small_viewport() {
        let area = Rect {
            x: 3,
            y: 2,
            width: 18,
            height: 3,
        };
        let rect = input_overlay_rect(area, 3);
        assert_eq!(rect.width, 18);
        assert_eq!(rect.height, 3);
        assert_eq!(rect.x, 3);
        assert_eq!(rect.y, 2);
    }

    #[test]
    fn scope_selector_content_layout_anchors_hints_to_bottom() {
        let inner = Rect {
            x: 4,
            y: 3,
            width: 70,
            height: 12,
        };

        let layout = scope_selector_content_layout(inner);
        assert_eq!(layout.hints.height, 1);
        assert_eq!(layout.hints.y + layout.hints.height, inner.y + inner.height);
        assert_eq!(layout.content.y, inner.y);
    }

    #[test]
    fn scope_selector_content_layout_handles_single_line_inner_area() {
        let inner = Rect {
            x: 1,
            y: 2,
            width: 30,
            height: 1,
        };

        let layout = scope_selector_content_layout(inner);
        assert_eq!(layout.content.height, 0);
        assert_eq!(layout.hints, inner);
    }

    #[test]
    fn editing_key_action_shift_enter_inserts_newline() {
        let key =
            crossterm::event::KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
        assert_eq!(
            editing_key_action_for_event(&key),
            EditingKeyAction::InsertNewline
        );
    }

    #[test]
    fn editing_key_action_ctrl_j_inserts_newline() {
        let key = crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(
            editing_key_action_for_event(&key),
            EditingKeyAction::InsertNewline
        );
    }

    #[test]
    fn editing_key_action_plain_enter_submits() {
        let key =
            crossterm::event::KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        assert_eq!(editing_key_action_for_event(&key), EditingKeyAction::Submit);
    }

    #[test]
    fn editing_key_action_repeat_char_inserts_char() {
        let key = crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Repeat,
        );
        assert_eq!(
            editing_key_action_for_event(&key),
            EditingKeyAction::InsertChar('x')
        );
    }

    #[test]
    fn editing_key_action_repeat_enter_is_ignored() {
        let key = crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Repeat,
        );
        assert_eq!(editing_key_action_for_event(&key), EditingKeyAction::Ignore);
    }

    #[test]
    fn editing_key_action_arrow_keys_home_end_and_delete_map_to_editor_actions() {
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            EditingKeyAction::MoveLeft
        );
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            EditingKeyAction::MoveRight
        );
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            EditingKeyAction::MoveUp
        );
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            EditingKeyAction::MoveDown
        );
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            EditingKeyAction::MoveHome
        );
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            EditingKeyAction::MoveEnd
        );
        assert_eq!(
            editing_key_action_for_event(&KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            EditingKeyAction::Delete
        );
    }

    #[test]
    fn handle_note_action_seeds_comment_draft_from_current_ai_suggestion() {
        let file_path = temp_test_file_path("tui_ai_comment_draft");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: AiProvider::CodexCli,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        state.ai.status = TuiAiStatus::Suggestion {
            key: request.key,
            suggestion: AiSuggestion {
                explanation: Some("This calls code that may panic.".to_string()),
                proposed_change: Some("Consider asking why this unwrap is safe.".to_string()),
            },
        };

        handle_note_action(&mut state).unwrap_or_else(|error| panic!("open note: {error}"));

        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert!(state.input_buffer.is_empty());
        assert_eq!(
            state.input_draft.as_deref(),
            Some("Consider asking why this unwrap is safe.")
        );
    }

    #[test]
    fn handle_note_action_does_not_seed_comment_draft_from_lgtm_explanation() {
        let file_path = temp_test_file_path("tui_ai_comment_explanation_only");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: AiProvider::CodexCli,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        state.ai.status = TuiAiStatus::Suggestion {
            key: request.key,
            suggestion: AiSuggestion {
                explanation: Some("This wrapper is straightforward; LGTM.".to_string()),
                proposed_change: None,
            },
        };

        handle_note_action(&mut state).unwrap_or_else(|error| panic!("open note: {error}"));

        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert!(state.input_buffer.is_empty());
        assert!(state.input_draft.is_none());
    }

    #[test]
    fn handle_editing_key_action_right_arrow_accepts_comment_draft() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_draft = Some("Consider asking why this unwrap is safe.".to_string());

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveRight),
            EditingActionResult::Handled
        );
        assert_eq!(
            state.input_buffer,
            "Consider asking why this unwrap is safe."
        );
        assert!(state.input_draft.is_none());
        assert_eq!(state.input_cursor.offset, state.input_buffer.len());
    }

    #[test]
    fn handle_editing_key_action_typing_discards_comment_draft() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_draft = Some("draft text".to_string());

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::InsertChar('x')),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_buffer, "x");
        assert!(state.input_draft.is_none());
    }

    #[test]
    fn handle_editing_key_action_inserts_and_deletes_at_cursor() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "ab".to_string();
        state.input_cursor = InputCursor {
            offset: 1,
            goal_column: None,
        };

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::InsertChar('x')),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_buffer, "axb");
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 2,
                goal_column: None,
            }
        );

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::Backspace),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_buffer, "ab");
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 1,
                goal_column: None,
            }
        );

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::Delete),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_buffer, "a");
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 1,
                goal_column: None,
            }
        );
    }

    #[test]
    fn handle_editing_key_action_moves_cursor_horizontally_and_to_comment_bounds() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "abcd".to_string();
        state.input_cursor = InputCursor {
            offset: 2,
            goal_column: None,
        };

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveLeft),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_cursor.offset, 1);

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveRight),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_cursor.offset, 2);

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveHome),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_cursor.offset, 0);

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveEnd),
            EditingActionResult::Handled
        );
        assert_eq!(state.input_cursor.offset, 4);
    }

    #[test]
    fn handle_editing_key_action_moves_cursor_vertically_across_multiline_comments() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "abcd\nx\nwxyz".to_string();
        state.input_cursor = InputCursor {
            offset: 3,
            goal_column: None,
        };

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveDown),
            EditingActionResult::Handled
        );
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 6,
                goal_column: Some(3),
            }
        );

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveDown),
            EditingActionResult::Handled
        );
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 10,
                goal_column: Some(3),
            }
        );

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveUp),
            EditingActionResult::Handled
        );
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 6,
                goal_column: Some(3),
            }
        );

        assert_eq!(
            handle_editing_key_action(&mut state, EditingKeyAction::MoveHome),
            EditingActionResult::Handled
        );
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 0,
                goal_column: None,
            }
        );
    }

    #[test]
    fn ui_renders_real_cursor_inside_editing_overlay() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "hello".to_string();
        state.input_cursor = InputCursor {
            offset: 2,
            goal_column: None,
        };

        let mut terminal = Terminal::new(TestBackend::new(40, 12))
            .unwrap_or_else(|error| panic!("failed to build test terminal: {error}"));
        terminal
            .draw(|frame| ui(frame, &mut state))
            .unwrap_or_else(|error| panic!("failed to render editing overlay: {error}"));

        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 12,
        };
        let hints = editing_overlay_hint("hello", None, None);
        let popup_area = input_overlay_rect(
            area,
            input_overlay_body_lines("hello", None, &hints, input_overlay_width(area.width)),
        );
        let inner = UiBlock::default()
            .title(" Note ")
            .borders(ratatui::widgets::Borders::ALL)
            .inner(popup_area);

        terminal
            .backend_mut()
            .assert_cursor_position((inner.x + 2, inner.y));
    }

    #[test]
    fn tui_keyboard_enhancement_flags_report_modified_enter_events() {
        let flags = tui_keyboard_enhancement_flags();
        assert!(
            flags.contains(crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        assert!(
            flags.contains(
                crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        );
        assert!(flags.contains(crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
    }

    #[test]
    fn enter_tui_mode_requests_keyboard_enhancement_flags() {
        let mut output = Vec::new();
        enter_tui_mode(
            &mut output,
            TerminalCapabilities::with_keyboard_enhancement_supported(true),
        )
        .unwrap_or_else(|err| panic!("enter tui mode: {err}"));

        let rendered =
            String::from_utf8(output).unwrap_or_else(|err| panic!("invalid ansi bytes: {err}"));
        assert!(
            rendered.contains("\u{1b}[>11u"),
            "expected keyboard enhancement push sequence in output: {rendered:?}"
        );
    }

    #[test]
    fn enter_tui_mode_enables_bracketed_paste() {
        let mut output = Vec::new();
        enter_tui_mode(
            &mut output,
            TerminalCapabilities::with_keyboard_enhancement_supported(true),
        )
        .unwrap_or_else(|err| panic!("enter tui mode: {err}"));

        let rendered =
            String::from_utf8(output).unwrap_or_else(|err| panic!("invalid ansi bytes: {err}"));
        assert!(
            rendered.contains("\u{1b}[?2004h"),
            "expected bracketed paste enable sequence in output: {rendered:?}"
        );
    }

    #[test]
    fn leave_tui_mode_pops_keyboard_enhancement_flags() {
        let mut output = Vec::new();
        leave_tui_mode(
            &mut output,
            TerminalCapabilities::with_keyboard_enhancement_supported(true),
        )
        .unwrap_or_else(|err| panic!("leave tui mode: {err}"));

        let rendered =
            String::from_utf8(output).unwrap_or_else(|err| panic!("invalid ansi bytes: {err}"));
        assert!(
            rendered.contains("\u{1b}[<1u"),
            "expected keyboard enhancement pop sequence in output: {rendered:?}"
        );
    }

    #[test]
    fn leave_tui_mode_disables_bracketed_paste() {
        let mut output = Vec::new();
        leave_tui_mode(
            &mut output,
            TerminalCapabilities::with_keyboard_enhancement_supported(true),
        )
        .unwrap_or_else(|err| panic!("leave tui mode: {err}"));

        let rendered =
            String::from_utf8(output).unwrap_or_else(|err| panic!("invalid ansi bytes: {err}"));
        assert!(
            rendered.contains("\u{1b}[?2004l"),
            "expected bracketed paste disable sequence in output: {rendered:?}"
        );
    }

    #[test]
    fn input_overlay_lines_preserve_multiline_content() {
        let palette = UiPalette::default();
        let lines = input_overlay_lines(
            "first line\nsecond line",
            None,
            "Enter to submit • Ctrl+J newline • Esc to cancel",
            &palette,
            80,
        );

        assert_eq!(lines[0].to_string(), "first line");
        assert_eq!(lines[1].to_string(), "second line");
        assert_eq!(lines[2].to_string(), "");
        assert!(
            lines[3].to_string().contains("Ctrl+J newline"),
            "expected multiline hint line to include Ctrl+J guidance"
        );
    }

    #[test]
    fn input_overlay_lines_render_comment_draft_as_visible_editor_text() {
        let palette = UiPalette::default();
        let lines = input_overlay_lines(
            "",
            Some("Consider asking why this unwrap is safe."),
            "Right arrow to use suggestion • Type to discard • Enter to submit",
            &palette,
            80,
        );

        assert_eq!(
            lines[0].to_string(),
            "Consider asking why this unwrap is safe."
        );
        assert_eq!(lines[1].to_string(), "");
        assert!(
            lines[2]
                .to_string()
                .contains("Right arrow to use suggestion")
        );
    }

    #[test]
    fn input_overlay_lines_cell_wrap_long_note_content() {
        let palette = UiPalette::default();
        let lines = input_overlay_lines(
            "hello world",
            None,
            "Enter to submit • Ctrl+J newline • Esc to cancel",
            &palette,
            8,
        );

        assert_eq!(lines[0].to_string(), "hello wo");
        assert_eq!(lines[1].to_string(), "rld");
        assert_eq!(lines[2].to_string(), "");
    }

    #[test]
    fn editing_submit_decision_returns_empty_for_blank_note_input() {
        let action = PendingAction::Single {
            node_id: TreeBuilder::new().root(),
            verdict: Verdict::Comment,
            note: None,
        };
        let input_mode = InputMode::Editing { action };
        let decision = editing_submit_decision(&input_mode, "   \n\t");
        assert_eq!(decision, Some(EditingSubmitDecision::Empty));
    }

    #[test]
    fn editing_submit_decision_returns_ready_with_original_note_whitespace() {
        let action = PendingAction::Single {
            node_id: TreeBuilder::new().root(),
            verdict: Verdict::Comment,
            note: None,
        };
        let input_mode = InputMode::Editing { action };
        let decision = editing_submit_decision(&input_mode, "  keep this note  ");
        let Some(EditingSubmitDecision::Ready(PendingAction::Single { note, .. })) = decision
        else {
            panic!("expected ready single action");
        };
        assert_eq!(note.as_deref(), Some("  keep this note  "));
    }

    #[test]
    fn editing_overlay_hint_allows_empty_note_input() {
        assert_eq!(
            editing_overlay_hint("", None, None),
            "Type a note • Enter to submit • Ctrl+J newline • Esc to cancel"
        );
        assert_eq!(
            editing_overlay_hint("", Some("draft"), None),
            "Right arrow to use suggestion • Type to discard • Enter to submit • Ctrl+J newline • Esc to cancel"
        );
        assert_eq!(
            editing_overlay_hint("note", None, None),
            "Enter to submit • Ctrl+J newline • Esc to cancel"
        );
        assert_eq!(
            editing_overlay_hint("", None, Some(EditingValidation::NoteRequired)),
            "Note required • Type a note • Ctrl+J newline • Esc to cancel"
        );
    }

    #[test]
    fn handle_editing_submit_with_empty_note_sets_validation_and_keeps_editing() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };

        handle_editing_submit_with(&mut state, |_action, _state| {
            panic!("empty note should not execute an action");
        })
        .unwrap_or_else(|error| panic!("expected empty submit to stay in editor: {error}"));

        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert_eq!(
            state.editing_validation,
            Some(EditingValidation::NoteRequired)
        );
    }

    #[test]
    fn handle_editing_submit_with_action_error_preserves_editor_state() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "keep me".to_string();

        let result = handle_editing_submit_with(&mut state, |_action, _state| {
            Err(anyhow::anyhow!("mark failed"))
        });
        let Err(error) = result else {
            panic!("expected submit failure to preserve editor state");
        };

        assert!(error.to_string().contains("mark failed"));
        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert_eq!(state.input_buffer, "keep me");
        assert_eq!(state.editing_validation, None);
    }

    #[test]
    fn editing_cancel_clears_non_empty_buffer_before_exit() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "note".to_string();
        state.editing_validation = Some(EditingValidation::NoteRequired);

        handle_editing_cancel(&mut state);
        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert!(state.input_buffer.is_empty());
        assert_eq!(state.editing_validation, None);

        handle_editing_cancel(&mut state);
        assert!(matches!(state.input_mode, InputMode::Normal));
    }

    #[test]
    fn handle_paste_event_inserts_single_line_text_at_cursor_while_editing() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "note".to_string();
        state.input_cursor = InputCursor {
            offset: 2,
            goal_column: None,
        };
        state.editing_validation = Some(EditingValidation::NoteRequired);

        let rerender = handle_paste_event(&mut state, " plus");

        assert!(rerender);
        assert_eq!(state.input_buffer, "no pluste");
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 7,
                goal_column: None,
            }
        );
        assert_eq!(state.editing_validation, None);
    }

    #[test]
    fn handle_paste_event_preserves_multiline_text_and_inserts_at_cursor_while_editing() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "ab".to_string();
        state.input_cursor = InputCursor {
            offset: 1,
            goal_column: None,
        };

        let rerender = handle_paste_event(&mut state, "x\ny");

        assert!(rerender);
        assert_eq!(state.input_buffer, "ax\nyb");
        assert_eq!(
            state.input_cursor,
            InputCursor {
                offset: 4,
                goal_column: None,
            }
        );
    }

    #[test]
    fn handle_paste_event_ignores_normal_mode() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_buffer = "note".to_string();

        let rerender = handle_paste_event(&mut state, " plus");

        assert!(!rerender);
        assert_eq!(state.input_buffer, "note");
    }

    #[test]
    fn handle_paste_event_ignores_confirm_batch_mode() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.input_mode = InputMode::ConfirmBatch {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Approved,
                note: None,
            },
            count: 3,
        };
        state.input_buffer = "note".to_string();

        let rerender = handle_paste_event(&mut state, " plus");

        assert!(!rerender);
        assert_eq!(state.input_buffer, "note");
    }

    #[test]
    fn content_cache_is_enabled_for_block_and_file_nodes() {
        assert!(is_content_kind_cacheable(TreeNodeKind::Block));
        assert!(is_content_kind_cacheable(TreeNodeKind::File));
        assert!(!is_content_kind_cacheable(TreeNodeKind::Directory));
        assert!(!is_content_kind_cacheable(TreeNodeKind::Root));
    }

    #[test]
    fn content_cache_key_tracks_block_source_height() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_b = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_c = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            25,
            80,
        );

        assert!(key_a.is_some());
        assert_eq!(key_a, key_b);
        assert_ne!(key_a, key_c);
    }

    fn temp_test_file_path(name: &str) -> PathBuf {
        temp_test_dir(name).join("src/lib.rs")
    }

    #[test]
    fn build_block_lines_source_mode_includes_full_file_context_for_scroll() {
        let file_path = temp_test_file_path("tui_source_scroll_context");
        let file_content = "line1\nline2\nline3\nline4\nline5\nline6\n";
        let block_content = "line3\nline4\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 2, 4);
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 2, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(content.total_lines, 6);
        assert_eq!(rendered.len(), 6);
        assert!(rendered[0].contains("line1"));
        assert!(rendered[1].contains("line2"));
        assert!(rendered[2].contains("line3"));
        assert!(rendered[3].contains("line4"));
        assert!(rendered[5].contains("line6"));
    }

    #[test]
    fn build_file_lines_source_mode_renders_full_file_source() {
        let file_path = temp_test_file_path("tui_file_source_mode");
        let file_content = "line1\nline2\nline3\n";
        let block_content = "line2\n";
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 1, 2);
        state.view_mode = ViewMode::Source;
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(content.total_lines, 3);
        assert!(rendered[0].contains("line1"));
        assert_eq!(rendered[1], "line2");
        assert!(rendered[2].contains("line3"));
        assert_eq!(content.focus_row_range, Some(1..2));
    }

    #[test]
    fn build_file_lines_source_mode_focuses_changed_rows_within_selected_block() {
        let file_path = temp_test_file_path("tui_file_source_focuses_changed_rows");
        let file_content = concat!(
            "line1
",
            "line2
",
            "line3
",
            "line4
",
            "line5
",
            "line6
",
            "line7
",
            "line8 changed
",
            "line9
",
            "line10
"
        );
        let block_content = concat!(
            "line5
",
            "line6
",
            "line7
",
            "line8 changed
",
            "line9
"
        );
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 4, 9);
        state.view_mode = ViewMode::Source;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::context(
                            "line1
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line2
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line3
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line4
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line5
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line6
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line7
",
                        ),
                        vcs::DiffHunkLine::removed(
                            "line8
",
                        ),
                        vcs::DiffHunkLine::added(
                            "line8 changed
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line9
",
                        ),
                        vcs::DiffHunkLine::context(
                            "line10
",
                        ),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 5, 80);

        assert_eq!(content.focus_row_range, Some(7..8));
        assert_eq!(
            scroll_offset_for_focus_range(
                content
                    .focus_row_range
                    .as_ref()
                    .unwrap_or_else(|| panic!("expected source focus row range")),
                5,
                content.total_lines,
            ),
            5
        );
    }

    #[test]
    fn wrapped_display_metrics_account_for_context_gutter_wrapping() {
        let palette = UiPalette::default();
        let mut highlighted_line_cache = HashMap::new();
        let lines = vec![
            format_context_line(
                &mut highlighted_line_cache,
                "abcdefghijklmnop",
                &palette,
                Some(&Language::Rust),
            ),
            format_code_line(
                &mut highlighted_line_cache,
                "focus()",
                &palette,
                Some(&Language::Rust),
            ),
        ];

        let (total_lines, focus_row_range) =
            wrapped_display_metrics_for_lines(&lines, Some(&(1..2)), 20);

        assert_eq!(total_lines, 3);
        assert_eq!(focus_row_range, Some(2..3));
    }

    #[test]
    fn build_file_lines_diff_mode_renders_diff_rows() {
        let file_path = temp_test_file_path("tui_file_diff_mode");
        let file_content = "line1 changed\nline2\n";
        let block_content = "line1 changed\n";
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::removed("line1\n"),
                        vcs::DiffHunkLine::added("line1 changed\n"),
                        vcs::DiffHunkLine::context("line2\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(content.total_lines, 3);
        assert_eq!(
            rendered,
            vec![
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        text: "line1".to_string(),
                        is_focus: true,
                    },
                    TuiDiffLineNumbers::Disabled,
                ),
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        text: "line1 changed".to_string(),
                        is_focus: true,
                    },
                    TuiDiffLineNumbers::Disabled,
                ),
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Context,
                        old_line: Some(2),
                        new_line: Some(2),
                        text: "line2".to_string(),
                        is_focus: false,
                    },
                    TuiDiffLineNumbers::Disabled,
                ),
            ]
        );
    }

    #[test]
    fn visible_comment_capture_for_source_content_uses_scrolled_logical_lines() {
        let file_path = temp_test_file_path("tui_comment_scope_source");
        let file_lines = (1..=12)
            .map(|index| format!("scope_line_{index:02}"))
            .collect::<Vec<_>>();
        let file_content = format!("{}\n", file_lines.join("\n"));
        let (mut state, _file_id, block_id) = build_state_with_block_file(
            &file_path,
            &file_content,
            &file_content,
            0,
            file_lines.len(),
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 6, 40);
        let capture = visible_comment_capture_for_content(&content, 3, 4, 12)
            .unwrap_or_else(|| panic!("expected visible comment capture"));

        assert_eq!(capture.scope.start_line, 3);
        assert_eq!(capture.scope.end_line, 7);
        assert_eq!(capture.context, file_lines[3..7].join("\n"));
    }

    #[test]
    fn visible_comment_capture_for_diff_content_uses_scrolled_diff_rows() {
        let file_path = temp_test_file_path("tui_comment_scope_diff");
        let file_lines = (1..=8)
            .map(|index| format!("diff_line_{index:02}"))
            .collect::<Vec<_>>();
        let file_content = format!("{}\n", file_lines.join("\n"));
        let (mut state, _file_id, block_id) = build_state_with_block_file(
            &file_path,
            &file_content,
            &file_content,
            0,
            file_lines.len(),
        );
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: file_lines
                        .iter()
                        .map(|line| vcs::DiffHunkLine::context(format!("{line}\n")))
                        .collect::<Vec<_>>(),
                }],
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 6, 80);
        let capture = visible_comment_capture_for_content(&content, 2, 3, 8)
            .unwrap_or_else(|| panic!("expected visible comment capture"));

        assert_eq!(capture.scope.start_line, 2);
        assert_eq!(capture.scope.end_line, 5);
        let expected = (3..=5)
            .map(|index| {
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Context,
                        old_line: Some(index),
                        new_line: Some(index),
                        text: format!("diff_line_{index:02}"),
                        is_focus: false,
                    },
                    TuiDiffLineNumbers::Disabled,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(capture.context, expected);
    }

    #[test]
    fn mark_params_for_action_persists_source_comment_anchor_for_visible_source_block() {
        let file_path = temp_test_file_path("tui_source_comment_anchor");
        let file_content = "fn demo() {\n    alpha();\n    beta();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 4);
        state.review_scope = ScopePreset::Commit {
            id: "1111111111111111111111111111111111111111".to_string(),
            summary: String::new(),
        };
        state.view_mode = ViewMode::Source;
        state.code_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();
        let content = build_block_lines(&mut state, &snapshot, &palette, 20, 80);
        state.content_height = usize_to_u16_saturating(content.total_lines);
        state.viewport_height = 20;

        let params = mark_params_for_action(
            &mut state,
            block_id,
            Verdict::Comment,
            Some("note".to_string()),
        )
        .unwrap_or_else(|error| panic!("expected source comment params: {error}"));

        assert!(params.comment_scope.is_none());
        assert!(params.comment_context.is_none());
        assert_eq!(
            params.comment_anchor,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: crate::store::CommitId::new("1111111111111111111111111111111111111111")
                    .unwrap(),
                path: RepoPath::new("src/lib.rs").unwrap(),
                start_line: 0,
                end_line: 4,
            }))
        );
    }

    #[test]
    fn review_scope_revision_resolves_symbolic_commit_scope_for_comment_anchors() {
        let repo_root = temp_git_repo("tui_symbolic_commit_anchor_revision");
        fs::write(repo_root.join("lib.rs"), "pub fn demo() {}\n")
            .unwrap_or_else(|error| panic!("failed to write fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial"]);
        let expected_revision =
            CommitId::new(run_git_stdout(&repo_root, &["rev-parse", "HEAD"]).trim())
                .unwrap_or_else(|error| panic!("expected valid fixture revision: {error}"));

        let mut state = build_test_state(
            ScopePreset::Commit {
                id: "HEAD".to_string(),
                summary: String::new(),
            },
            HashMap::new(),
        );
        state.repo_root = Some(repo_root);

        let revision = review_scope_revision(&state)
            .unwrap_or_else(|error| panic!("expected symbolic commit scope to resolve: {error}"));

        assert_eq!(revision, Some(expected_revision));
    }

    #[test]
    fn review_scope_revision_resolves_symbolic_revision_range_end_for_comment_anchors() {
        let repo_root = temp_git_repo("tui_symbolic_range_anchor_revision");
        fs::write(repo_root.join("lib.rs"), "pub fn demo() -> u8 { 1 }\n")
            .unwrap_or_else(|error| panic!("failed to write fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial"]);
        fs::write(repo_root.join("lib.rs"), "pub fn demo() -> u8 { 2 }\n")
            .unwrap_or_else(|error| panic!("failed to update fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update"]);
        let expected_revision =
            CommitId::new(run_git_stdout(&repo_root, &["rev-parse", "HEAD"]).trim())
                .unwrap_or_else(|error| panic!("expected valid fixture revision: {error}"));

        let mut state = build_test_state(
            ScopePreset::RevisionRange {
                start: "HEAD~1".to_string(),
                end: "HEAD".to_string(),
            },
            HashMap::new(),
        );
        state.repo_root = Some(repo_root);

        let revision = review_scope_revision(&state).unwrap_or_else(|error| {
            panic!("expected symbolic revision range end to resolve: {error}")
        });

        assert_eq!(revision, Some(expected_revision));
    }

    #[test]
    fn mark_params_for_action_resolves_symbolic_scope_for_source_comment_anchor() {
        let repo_root = temp_git_repo("tui_symbolic_source_comment_anchor");
        fs::write(repo_root.join("lib.rs"), "pub fn demo() {}\n")
            .unwrap_or_else(|error| panic!("failed to write fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial"]);
        let expected_revision =
            CommitId::new(run_git_stdout(&repo_root, &["rev-parse", "HEAD"]).trim())
                .unwrap_or_else(|error| panic!("expected valid fixture revision: {error}"));

        let file_path = temp_test_file_path("tui_symbolic_source_comment_anchor");
        let file_content = "fn demo() {\n    alpha();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 3);
        state.review_scope = ScopePreset::Commit {
            id: "HEAD".to_string(),
            summary: String::new(),
        };
        state.repo_root = Some(repo_root);
        state.view_mode = ViewMode::Source;
        state.code_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();
        let content = build_block_lines(&mut state, &snapshot, &palette, 20, 80);
        state.content_height = usize_to_u16_saturating(content.total_lines);
        state.viewport_height = 20;

        let params = mark_params_for_action(
            &mut state,
            block_id,
            Verdict::Comment,
            Some("note".to_string()),
        )
        .unwrap_or_else(|error| panic!("expected symbolic source comment params: {error}"));

        assert_eq!(
            params.comment_anchor,
            Some(CommentAnchor::Source(SourceCommentAnchor {
                revision: expected_revision,
                path: RepoPath::new("src/lib.rs").unwrap(),
                start_line: 0,
                end_line: 3,
            }))
        );
    }

    #[test]
    fn mark_params_for_action_persists_diff_comment_anchor_rows() {
        let file_path = temp_test_file_path("tui_diff_comment_anchor");
        let block_content = "fn demo() {\n    new();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, block_content, block_content, 0, 3);
        state.review_scope = ScopePreset::Commit {
            id: "2222222222222222222222222222222222222222".to_string(),
            summary: String::new(),
        };
        state.view_mode = ViewMode::Diff;
        state.code_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::context("fn demo() {\n"),
                        vcs::DiffHunkLine::removed("    old();\n"),
                        vcs::DiffHunkLine::added("    new();\n"),
                        vcs::DiffHunkLine::context("}\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();
        let content = build_block_lines(&mut state, &snapshot, &palette, 20, 80);
        state.content_height = usize_to_u16_saturating(content.total_lines);
        state.viewport_height = 20;

        let params = mark_params_for_action(
            &mut state,
            block_id,
            Verdict::Comment,
            Some("note".to_string()),
        )
        .unwrap_or_else(|error| panic!("expected diff comment params: {error}"));

        assert_eq!(
            params.comment_anchor,
            Some(CommentAnchor::Diff(DiffCommentAnchor {
                revision: crate::store::CommitId::new("2222222222222222222222222222222222222222")
                    .unwrap(),
                path: RepoPath::new("src/lib.rs").unwrap(),
                rows: vec![
                    DiffCommentAnchorRow {
                        kind: CommentAnchorDiffLineKind::Context,
                        old_line: Some(1),
                        new_line: Some(1),
                    },
                    DiffCommentAnchorRow {
                        kind: CommentAnchorDiffLineKind::Removed,
                        old_line: Some(2),
                        new_line: None,
                    },
                    DiffCommentAnchorRow {
                        kind: CommentAnchorDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(2),
                    },
                    DiffCommentAnchorRow {
                        kind: CommentAnchorDiffLineKind::Context,
                        old_line: Some(3),
                        new_line: Some(3),
                    },
                ],
            }))
        );
    }

    #[test]
    fn build_file_lines_diff_mode_can_render_old_new_line_numbers() {
        let file_path = temp_test_file_path("tui_file_diff_mode_old_new");
        let file_content = "line1\nline2\n";
        let block_content = "line1\n";
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        state.view_mode = ViewMode::Diff;
        state.diff_line_numbers = TuiDiffLineNumbers::OldNew;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::removed("line1\n"),
                        vcs::DiffHunkLine::added("line1 changed\n"),
                        vcs::DiffHunkLine::context("line2\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        text: "line1".to_string(),
                        is_focus: true,
                    },
                    TuiDiffLineNumbers::OldNew,
                ),
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        text: "line1 changed".to_string(),
                        is_focus: true,
                    },
                    TuiDiffLineNumbers::OldNew,
                ),
                format_diff_overlay_row(
                    &vcs::DiffLine {
                        kind: vcs::DiffLineKind::Context,
                        old_line: Some(2),
                        new_line: Some(2),
                        text: "line2".to_string(),
                        is_focus: false,
                    },
                    TuiDiffLineNumbers::OldNew,
                ),
            ]
        );
    }

    #[test]
    fn build_file_lines_diff_mode_uses_compact_gutter_when_code_width_is_narrow() {
        let file_path = temp_test_file_path("tui_file_diff_mode_narrow_gutter");
        let file_content =
            "fn demo() {\n    let value = \"this_is_a_very_long_changed_line\";\n}\n";
        let block_content = file_content;
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 3);
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::context("fn demo() {\n"),
                        vcs::DiffHunkLine::removed("    let value = \"short\";\n"),
                        vcs::DiffHunkLine::added(
                            "    let value = \"this_is_a_very_long_changed_line\";\n",
                        ),
                        vcs::DiffHunkLine::context("}\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 3, 12);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert!(
            rendered.iter().any(|line| line.starts_with('-')),
            "expected compact removed gutter in narrow diff mode: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with('+')),
            "expected compact added gutter in narrow diff mode: {rendered:?}"
        );
        assert!(
            rendered.iter().all(|line| line.trim() != "2"),
            "did not expect wrapped rows to split line-number gutters: {rendered:?}"
        );
    }

    #[test]
    fn build_file_lines_diff_mode_shows_no_changes_hint() {
        let file_path = temp_test_file_path("tui_file_diff_empty");
        let file_content = "line1\nline2\n";
        let block_content = "line1\n";
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::NoTextChanges {
                path: RepoPath::new("src/lib.rs").unwrap(),
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(content.total_lines, 2);
        assert_eq!(
            rendered,
            vec!["(No diff changes in this file)", "Press [m] to view source"]
        );
    }

    #[test]
    fn build_file_lines_diff_mode_uses_configured_toggle_view_key_for_source_hint() {
        let file_path = temp_test_file_path("tui_file_diff_toggle_hint");
        let file_content = "line1\nline2\n";
        let block_content = "line1\n";
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        state.view_mode = ViewMode::Diff;
        state.keybinds.toggle_view = 'v';
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::NoTextChanges {
                path: RepoPath::new("src/lib.rs").unwrap(),
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(rendered[1], "Press [v] to view source");
    }

    #[test]
    fn build_added_markdown_section_diff_lines_include_greyed_continuation_context() {
        let file_path =
            temp_test_dir("tui_added_markdown_section_diff_context").join("src/README.md");
        let file_content = concat!(
            "# Guide\n",
            "\n",
            "## Added section\n",
            "New paragraph.\n",
            "\n",
            "## Existing section\n",
            "Existing continuation.\n"
        );
        let block_content = concat!("## Added section\n", "New paragraph.\n", "\n");
        let (mut state, _file_id, block_id) = build_state_with_block_file_metadata(
            &file_path,
            file_content,
            block_content,
            2,
            5,
            BlockKind::Section,
            Language::Markdown,
        );
        state.view_mode = ViewMode::Diff;
        state
            .block_change_kinds
            .insert(block_id, BlockChangeKind::Added);
        state.file_diff_cache.insert(
            PathBuf::from("src/README.md"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/README.md").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/README.md").unwrap(),
                    old_start: 3,
                    new_start: 3,
                    lines: vec![
                        vcs::DiffHunkLine::added("## Added section\n"),
                        vcs::DiffHunkLine::added("New paragraph.\n"),
                        vcs::DiffHunkLine::added("\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 7, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        let blank_added_index = rendered
            .iter()
            .position(|line| line == "+ ")
            .unwrap_or_else(|| panic!("expected trailing blank added row: {rendered:?}"));
        let continuation_index = rendered
            .iter()
            .position(|line| line.contains("## Existing section"))
            .unwrap_or_else(|| {
                panic!("expected continuation context after added block: {rendered:?}")
            });
        assert!(
            blank_added_index < continuation_index,
            "expected blank added row to separate added content from continuation context: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Existing continuation.")),
            "expected file continuation context after added block: {rendered:?}"
        );

        let continuation_style = content.lines[continuation_index].spans[0].style;
        assert_eq!(continuation_style.fg, Some(palette.context));
        assert!(continuation_style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn build_block_diff_lines_no_changes_include_source_hint() {
        let file_path = temp_test_file_path("tui_block_diff_empty");
        let file_content = "line1\nline2\n";
        let block_content = "line1\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::NoTextChanges {
                path: RepoPath::new("src/lib.rs").unwrap(),
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        state.view_mode = ViewMode::Diff;
        let content = build_block_lines(&mut state, &snapshot, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(content.total_lines, 2);
        assert_eq!(
            rendered,
            vec!["(No diff changes in this file)", "Press [m] to view source"]
        );
    }

    #[test]
    fn build_block_lines_diff_mode_focuses_changed_rows_in_long_block() {
        let file_path = temp_test_file_path("tui_block_diff_focuses_changed_rows");
        let file_content = concat!(
            "line1\n",
            "line2\n",
            "line3\n",
            "line4\n",
            "line5\n",
            "line6\n",
            "line7\n",
            "line8 changed\n",
            "line9\n",
            "line10\n"
        );
        let block_content = file_content;
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 10);
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::context("line1\n"),
                        vcs::DiffHunkLine::context("line2\n"),
                        vcs::DiffHunkLine::context("line3\n"),
                        vcs::DiffHunkLine::context("line4\n"),
                        vcs::DiffHunkLine::context("line5\n"),
                        vcs::DiffHunkLine::context("line6\n"),
                        vcs::DiffHunkLine::context("line7\n"),
                        vcs::DiffHunkLine::removed("line8\n"),
                        vcs::DiffHunkLine::added("line8 changed\n"),
                        vcs::DiffHunkLine::context("line9\n"),
                        vcs::DiffHunkLine::context("line10\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 5, 80);
        let focus_row_range = content.focus_row_range.clone();

        assert_eq!(focus_row_range, Some(7..9));
        assert_eq!(
            scroll_offset_for_focus_range(
                focus_row_range
                    .as_ref()
                    .unwrap_or_else(|| panic!("expected diff focus row range")),
                5,
                content.total_lines,
            ),
            6
        );
    }

    #[test]
    fn build_file_lines_diff_mode_focuses_changed_rows_within_selected_block() {
        let file_path = temp_test_file_path("tui_file_diff_focuses_changed_rows");
        let file_content = concat!(
            "line1\n",
            "line2\n",
            "line3\n",
            "line4\n",
            "line5\n",
            "line6\n",
            "line7\n",
            "line8 changed\n",
            "line9\n",
            "line10\n"
        );
        let block_content = concat!(
            "line5\n",
            "line6\n",
            "line7\n",
            "line8 changed\n",
            "line9\n"
        );
        let (mut state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 4, 9);
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::context("line1\n"),
                        vcs::DiffHunkLine::context("line2\n"),
                        vcs::DiffHunkLine::context("line3\n"),
                        vcs::DiffHunkLine::context("line4\n"),
                        vcs::DiffHunkLine::context("line5\n"),
                        vcs::DiffHunkLine::context("line6\n"),
                        vcs::DiffHunkLine::context("line7\n"),
                        vcs::DiffHunkLine::removed("line8\n"),
                        vcs::DiffHunkLine::added("line8 changed\n"),
                        vcs::DiffHunkLine::context("line9\n"),
                        vcs::DiffHunkLine::context("line10\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(file_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_file_lines(&mut state, &snapshot, &palette, 5, 80);

        assert_eq!(content.focus_row_range, Some(7..9));
    }

    #[test]
    fn build_block_lines_diff_mode_excludes_previous_function_rows() {
        let file_path = temp_test_file_path("tui_block_diff_scoped");
        let file_content = concat!(
            "fn previous() {\n",
            "    Ok(());\n",
            "}\n",
            "fn myfun() {\n",
            "    new_body();\n",
            "}\n"
        );
        let block_content = "fn myfun() {\n    new_body();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 3, 6);
        state.view_mode = ViewMode::Diff;
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::context("fn previous()\n"),
                        vcs::DiffHunkLine::removed("    old_previous();\n"),
                        vcs::DiffHunkLine::added("    Ok(());\n"),
                        vcs::DiffHunkLine::context("}\n"),
                        vcs::DiffHunkLine::context("fn myfun() {\n"),
                        vcs::DiffHunkLine::removed("    old_body();\n"),
                        vcs::DiffHunkLine::added("    new_body();\n"),
                        vcs::DiffHunkLine::context("}\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 6, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert!(
            rendered.iter().all(|line| !line.contains("Ok(())")),
            "block diff should not include previous-function rows: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("fn myfun() {")),
            "expected block diff to include the current function signature: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("new_body();")),
            "expected block diff to include the current function change: {rendered:?}"
        );
    }

    #[test]
    fn build_block_lines_diff_mode_for_deleted_block_excludes_following_function_rows() {
        let file_path = temp_test_file_path("tui_deleted_block_diff_scoped");
        let file_content = concat!("fn kept() {\n", "    body();\n", "}\n");
        let deleted_block_content = "fn removed() {\n    old_body();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, deleted_block_content, 0, 3);
        state.view_mode = ViewMode::Diff;
        let deleted_block = state
            .navigator
            .tree
            .node(block_id)
            .block
            .clone()
            .unwrap_or_else(|| panic!("expected deleted block fixture"));
        state.diff_block_sides.insert(
            block_id,
            DiffBlockSides {
                base: Some(deleted_block),
                head: None,
            },
        );
        state.file_diff_cache.insert(
            PathBuf::from("src/lib.rs"),
            vcs::FileDiff::Text {
                path: RepoPath::new("src/lib.rs").unwrap(),
                hunks: vec![vcs::DiffHunk {
                    file_path: RepoPath::new("src/lib.rs").unwrap(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        vcs::DiffHunkLine::removed("fn removed() {\n"),
                        vcs::DiffHunkLine::removed("    old_body();\n"),
                        vcs::DiffHunkLine::removed("}\n"),
                        vcs::DiffHunkLine::context("fn kept() {\n"),
                        vcs::DiffHunkLine::context("    body();\n"),
                        vcs::DiffHunkLine::context("}\n"),
                    ],
                }],
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &snapshot, &palette, 6, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert!(
            rendered.iter().any(|line| line.contains("fn removed()")),
            "expected deleted block diff to include removed function: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("old_body();")),
            "expected deleted block diff to include removed body: {rendered:?}"
        );
        assert!(
            rendered.iter().all(|line| !line.contains("fn kept()")),
            "deleted block diff should not include following function rows: {rendered:?}"
        );
    }

    #[test]
    fn build_block_lines_source_mode_for_deleted_block_uses_base_content_only() {
        // GIVEN: a deleted base-only block exists at a path whose current file content is different
        let file_path = temp_test_file_path("tui_deleted_block_source_scoped");
        let file_content = concat!("fn kept() {\n", "    body();\n", "}\n");
        let deleted_block_content = "fn removed() {\n    old_body();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, deleted_block_content, 0, 3);
        state.view_mode = ViewMode::Source;
        let deleted_block = state
            .navigator
            .tree
            .node(block_id)
            .block
            .clone()
            .unwrap_or_else(|| panic!("expected deleted block fixture"));
        state.diff_block_sides.insert(
            block_id,
            DiffBlockSides {
                base: Some(deleted_block),
                head: None,
            },
        );
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        // WHEN: source mode renders that deleted block
        let content = build_block_lines(&mut state, &snapshot, &palette, 6, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        // THEN: source mode shows only the deleted base-side block content
        assert_eq!(content.total_lines, 3);
        assert_eq!(content.focus_row_range, Some(0..3));
        assert!(
            rendered.iter().any(|line| line.contains("fn removed()")),
            "expected deleted block source to include removed function: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("old_body();")),
            "expected deleted block source to include removed body: {rendered:?}"
        );
        assert!(
            rendered.iter().all(|line| !line.contains("fn kept()")),
            "deleted block source should not include current file rows: {rendered:?}"
        );
    }

    #[test]
    fn build_mode_banner_line_shows_diff_mode() {
        let file_path = temp_test_file_path("tui_mode_banner_label");
        let file_content = "line1\n";
        let block_content = "line1\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        state.view_mode = ViewMode::Diff;
        let palette = UiPalette::default();

        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Diff Mode"
        );

        state.view_mode = ViewMode::Source;
        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Source Mode"
        );
    }

    #[test]
    fn build_mode_banner_line_shows_navigation_mode() {
        let mut state = build_test_state(ScopePreset::All, HashMap::new());
        state.total_blocks = 1;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 1;
        state.navigator.jump_root();
        let palette = UiPalette::default();

        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Navigation Mode"
        );
    }

    #[test]
    fn build_mode_banner_line_shows_speed_read_mode() {
        let (mut state, _block_id) = build_state_with_single_block("alpha beta gamma delta");
        let palette = UiPalette::default();

        toggle_speed_read_mode(&mut state);

        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Speed Read Mode"
        );
    }

    #[test]
    fn build_ai_hint_line_shows_ai_availability_on_own_line() {
        let mut state = build_test_state(ScopePreset::All, HashMap::new());
        state.total_blocks = 1;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 1;
        state.navigator.jump_root();
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: AiProvider::CodexCli,
                model: "gpt-5-mini".to_string(),
            },
            80,
            true,
        );
        let palette = UiPalette::default();

        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Navigation Mode"
        );
        assert_eq!(
            build_ai_hint_line(&state, &palette).map(|line| line.to_string()),
            Some("AI: ready (Codex CLI / gpt-5-mini)".to_string())
        );
    }

    #[test]
    fn ai_suggestion_provider_for_availability_supports_cli_providers_only() {
        assert!(
            ai_suggestion_provider_for_availability(Some(&AiAvailability::Ready {
                provider: AiProvider::CodexCli,
                model: "auto".to_string(),
            }))
            .is_some()
        );
        assert!(
            ai_suggestion_provider_for_availability(Some(&AiAvailability::Ready {
                provider: AiProvider::Anthropic,
                model: "auto".to_string(),
            }))
            .is_none()
        );
    }

    #[test]
    fn ai_modeline_reports_direct_api_suggestions_as_unimplemented() {
        let state = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );

        assert_eq!(
            state.hint_line_text().as_deref(),
            Some(
                "Suggestion unavailable (Anthropic direct API suggestions not implemented; set provider = \"claude_cli\" or \"codex_cli\")"
            )
        );
    }

    #[test]
    fn ai_suggestion_request_for_current_focus_uses_block_metadata() {
        let file_path = temp_test_file_path("tui_ai_request");
        let file_content = "fn checked() {\n    call();\n}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 3);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "claude-3-5-haiku".to_string(),
            },
            2,
            true,
        );

        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };

        assert_eq!(request.key.path, "src/lib.rs");
        assert_eq!(request.key.model, "claude-3-5-haiku");
        assert_eq!(request.key.start_line, 0);
        assert_eq!(request.key.end_line, 3);
        assert_eq!(request.key.max_context_lines, 2);
        assert_eq!(
            request.key.max_response_chars,
            DEFAULT_AI_RESPONSE_CHAR_LIMIT
        );
        assert!(request.prompt.contains("within 90 visible characters"));
        assert_eq!(
            request.key.review_set_hash,
            request.review_set.review_set_hash
        );
        assert!(request.review_set.overview.contains("Review blocks: 1"));
        assert!(request.review_set.overview.contains("fn checked() {"));
        assert!(request.prompt.contains("fn checked() {\n    call();\n..."));
    }

    #[test]
    fn ai_suggestion_request_for_current_focus_uses_viewport_width_for_response_limit() {
        let file_path = temp_test_file_path("tui_ai_request_width");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.code_rect = Rect {
            x: 0,
            y: 0,
            width: 72,
            height: 10,
        };
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "claude-3-5-haiku".to_string(),
            },
            2,
            true,
        );

        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };

        assert_eq!(request.key.max_response_chars, 72);
        assert!(request.prompt.contains("within 72 visible characters"));

        state.code_rect.width = 120;
        let wide_request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };

        assert_eq!(wide_request.key.max_response_chars, 120);
        assert!(
            wide_request
                .prompt
                .contains("within 120 visible characters")
        );
    }

    #[test]
    fn ai_suggestion_request_for_current_focus_includes_base_and_head_diff_context() {
        let file_path = temp_test_file_path("tui_ai_diff_request");
        let head_content = "fn checked() {\n    new_call();\n}\n";
        let base_content = "fn checked() {\n    old_call();\n}\n";
        let (mut state, _file_id, block_id) =
            build_state_with_block_file(&file_path, head_content, head_content, 0, 3);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "claude-3-5-haiku".to_string(),
            },
            20,
            true,
        );
        state.diff_block_sides.insert(
            block_id,
            DiffBlockSides {
                base: Some(Block::new(
                    base_content.to_string(),
                    BlockKind::Function,
                    0,
                    3,
                )),
                head: Some(Block::new(
                    head_content.to_string(),
                    BlockKind::Function,
                    0,
                    3,
                )),
            },
        );

        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };

        assert!(request.prompt.contains("[base]\nfn checked() {"));
        assert!(request.prompt.contains("old_call();"));
        assert!(request.prompt.contains("[head]\nfn checked() {"));
        assert!(request.prompt.contains("new_call();"));
        assert!(request.review_set.overview.contains("[base]"));
        assert!(request.review_set.overview.contains("[head]"));
    }

    #[test]
    fn refresh_ai_suggestion_state_uses_cached_suggestion_in_ai_hint_line() {
        let file_path = temp_test_file_path("tui_ai_cached_suggestion");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        state.ai.cache.insert(
            request.key,
            AiSuggestion {
                explanation: Some("This is a straightforward wrapper.".to_string()),
                proposed_change: Some("Guard the wrapper against an empty input.".to_string()),
            },
        );

        assert!(refresh_ai_suggestion_state(&mut state));
        let palette = UiPalette::default();
        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Source Mode"
        );
        assert_eq!(
            build_ai_hint_line(&state, &palette).map(|line| line.to_string()),
            Some("Guard the wrapper against an empty input.".to_string())
        );
    }

    #[test]
    fn build_ai_hint_lines_wrap_long_suggestions_on_word_boundaries() {
        let file_path = temp_test_file_path("tui_ai_wrapped_suggestion");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        state.ai.status = TuiAiStatus::Suggestion {
            key: request.key,
            suggestion: AiSuggestion {
                explanation: None,
                proposed_change: Some(
                    "Ask reviewers to require a concrete timeout before this network wait ships."
                        .to_string(),
                ),
            },
        };
        let palette = UiPalette::default();

        let rendered = build_ai_hint_lines(&state, &palette, 24)
            .unwrap_or_else(|| panic!("expected AI hint lines"))
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "Ask reviewers to require",
                "a concrete timeout",
                "before this network wait",
                "ships."
            ]
        );
    }

    #[test]
    fn build_ai_hint_line_shows_explanation_for_lgtm_suggestion_without_proposed_change() {
        let file_path = temp_test_file_path("tui_ai_no_change_suggestion");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        state.ai.status = TuiAiStatus::Suggestion {
            key: request.key,
            suggestion: AiSuggestion {
                explanation: Some("This wrapper is straightforward; LGTM.".to_string()),
                proposed_change: None,
            },
        };
        let palette = UiPalette::default();

        assert_eq!(
            build_ai_hint_line(&state, &palette).map(|line| line.to_string()),
            Some("This wrapper is straightforward; LGTM.".to_string())
        );
    }

    #[test]
    fn build_ai_hint_line_shows_ai_loading_state() {
        let file_path = temp_test_file_path("tui_ai_loading");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        state.ai.status = TuiAiStatus::Loading {
            key: request.key,
            frame: 0,
        };
        let palette = UiPalette::default();

        assert_eq!(
            build_mode_banner_line(&state, &palette).to_string(),
            "Source Mode"
        );
        assert_eq!(
            build_ai_hint_line(&state, &palette).map(|line| line.to_string()),
            Some("✦ · ·".to_string())
        );
    }

    #[test]
    fn refresh_ai_suggestion_state_advances_loading_hint_frame() {
        let file_path = temp_test_file_path("tui_ai_loading_animation");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        let (_sender, receiver) = mpsc::channel();
        state.ai.pending = Some(PendingAiSuggestion::new(
            request.key.clone(),
            receiver,
            Instant::now() - AI_LOADING_FRAME_INTERVAL,
        ));
        state.ai.status = TuiAiStatus::Loading {
            key: request.key,
            frame: 0,
        };

        assert!(refresh_ai_suggestion_state(&mut state));
        let palette = UiPalette::default();
        assert_eq!(
            build_ai_hint_line(&state, &palette).map(|line| line.to_string()),
            Some("· ✧ ·".to_string())
        );
    }

    #[test]
    fn refresh_ai_suggestion_state_waits_for_loading_hint_deadline() {
        let file_path = temp_test_file_path("tui_ai_loading_animation_waits");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        let (_sender, receiver) = mpsc::channel();
        state.ai.pending = Some(PendingAiSuggestion::new(
            request.key.clone(),
            receiver,
            Instant::now(),
        ));
        state.ai.status = TuiAiStatus::Loading {
            key: request.key,
            frame: 0,
        };

        assert!(!refresh_ai_suggestion_state(&mut state));
        let palette = UiPalette::default();
        assert_eq!(
            build_ai_hint_line(&state, &palette).map(|line| line.to_string()),
            Some("✦ · ·".to_string())
        );
    }

    #[test]
    fn ai_loading_hint_animation_is_slow_enough_to_not_starve_input() {
        assert!(AI_LOADING_FRAME_INTERVAL >= Duration::from_millis(400));
    }

    #[test]
    fn handle_next_cancels_pending_ai_suggestion_for_previous_focus() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        state.navigator.set_current(block_ids[0]);
        state.focus_block = Some(block_ids[0]);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        let (_sender, receiver) = mpsc::channel();
        state.ai.pending = Some(PendingAiSuggestion::new(
            request.key.clone(),
            receiver,
            Instant::now(),
        ));
        state.ai.status = TuiAiStatus::Loading {
            key: request.key,
            frame: 0,
        };

        handle_next(&mut state);

        assert_eq!(state.focus_block, Some(block_ids[1]));
        assert!(state.ai.pending.is_none());
        assert_eq!(state.ai.status, TuiAiStatus::Availability);
    }

    #[test]
    fn handle_note_action_cancels_pending_ai_without_waiting() {
        let file_path = temp_test_file_path("tui_ai_note_cancels_pending");
        let file_content = "fn checked() {}\n";
        let (mut state, _file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, file_content, 0, 1);
        state.ai = TuiAiState::from_availability(
            AiAvailability::Ready {
                provider: crate::ai::AiProvider::Anthropic,
                model: "auto".to_string(),
            },
            80,
            true,
        );
        let request = match ai_suggestion_request_for_current_focus(&state) {
            Some(request) => request,
            None => panic!("expected AI request for focused block"),
        };
        let (_sender, receiver) = mpsc::channel();
        state.ai.pending = Some(PendingAiSuggestion::new(
            request.key.clone(),
            receiver,
            Instant::now(),
        ));
        state.ai.status = TuiAiStatus::Loading {
            key: request.key,
            frame: 0,
        };

        handle_note_action(&mut state).unwrap_or_else(|error| panic!("open note: {error}"));

        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert!(state.input_draft.is_none());
        assert!(state.ai.pending.is_none());
        assert_eq!(state.ai.status, TuiAiStatus::Availability);
    }

    #[test]
    fn compact_block_header_text_collapses_nested_breadcrumbs_into_one_row() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let impl_block = builder.add_block(
            file,
            "impl".to_string(),
            "src/lib.rs".to_string(),
            Block::new(
                "impl Thing {\n    fn do_it(&self) {}\n}\n".to_string(),
                BlockKind::Impl,
                0,
                3,
            ),
            Language::Rust,
        );
        let method = builder.add_block(
            impl_block,
            "method".to_string(),
            "src/lib.rs".to_string(),
            Block::new(
                "fn do_it(&self) {}\n".to_string(),
                BlockKind::Function,
                1,
                2,
            ),
            Language::Rust,
        );
        let tree = builder.finalize();

        assert_eq!(
            compact_block_header_text(&tree, method).as_deref(),
            Some("Function do_it @ Impl Thing @ File src/lib.rs")
        );
    }

    #[test]
    fn build_header_lines_show_block_path_named_hash_and_subblock_tree() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "main.rs".to_string(),
            "example_repos/all_languages/main.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let struct_block = Block::new(
            "#[derive(Debug, Clone)]\nstruct Config {\n    name: String,\n}\n".to_string(),
            BlockKind::Struct,
            0,
            4,
        );
        let expected_hash = struct_block
            .hash
            .as_str()
            .chars()
            .take(8)
            .collect::<String>();
        let struct_id = builder.add_block(
            file,
            "struct".to_string(),
            "example_repos/all_languages/main.rs".to_string(),
            struct_block,
            Language::Rust,
        );
        builder.add_block(
            struct_id,
            "CodeParagraph".to_string(),
            "example_repos/all_languages/main.rs".to_string(),
            Block::new(
                "name: String,\n".to_string(),
                BlockKind::CodeParagraph,
                2,
                3,
            ),
            Language::Rust,
        );
        let tree = builder.finalize();
        let visible = HashSet::from([struct_id]);
        let review_order = ReviewOrder::from_tree(&tree, &visible);
        let mut navigator = ReviewNavigator::new(tree, visible.clone())
            .unwrap_or_else(|error| panic!("failed to build navigator: {error}"));
        navigator.set_current(struct_id);
        let mut state = build_test_state(ScopePreset::All, HashMap::new());
        state.navigator = navigator;
        state.review_order = review_order;
        state.total_blocks = visible.len();
        state.initial_remaining_blocks = visible.len();
        state.remaining_blocks = visible.len();
        state.reviewable_nodes = visible;
        state.focus_block = Some(struct_id);
        let palette = UiPalette::default();
        let block_node = state.navigator.tree.node(struct_id);

        let header = build_header_lines(block_node, &state, &palette)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            header,
            vec![
                "example_repos/all_languages/main.rs".to_string(),
                format!("  -> struct Config (hash={expected_hash})"),
                "     └─ CodeParagraph".to_string(),
            ]
        );
    }

    #[test]
    fn subblock_tree_lines_render_nested_children_in_preorder() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        let parent = builder.add_block(
            file,
            "impl".to_string(),
            "src/lib.rs".to_string(),
            Block::new(
                "impl Thing {\n    fn first(&self) {}\n    fn second(&self) {}\n}\n".to_string(),
                BlockKind::Impl,
                0,
                4,
            ),
            Language::Rust,
        );
        let first = builder.add_block(
            parent,
            "function".to_string(),
            "src/lib.rs".to_string(),
            Block::new(
                "fn first(&self) {}\n".to_string(),
                BlockKind::Function,
                1,
                2,
            ),
            Language::Rust,
        );
        builder.add_block(
            first,
            "CodeParagraph".to_string(),
            "src/lib.rs".to_string(),
            Block::new(
                "self.value();\n".to_string(),
                BlockKind::CodeParagraph,
                1,
                2,
            ),
            Language::Rust,
        );
        builder.add_block(
            parent,
            "function".to_string(),
            "src/lib.rs".to_string(),
            Block::new(
                "fn second(&self) {}\n".to_string(),
                BlockKind::Function,
                2,
                3,
            ),
            Language::Rust,
        );
        let tree = builder.finalize();

        assert_eq!(
            subblock_tree_lines(&tree, parent),
            vec![
                "     ├─ function first".to_string(),
                "     │  └─ CodeParagraph".to_string(),
                "     └─ function second".to_string(),
            ]
        );
    }

    #[test]
    fn build_header_lines_show_file_change_metadata_without_mode_row() {
        let file_path = temp_test_file_path("tui_header_change_label");
        let file_content = "line1\n";
        let block_content = "line1\n";
        let (state, file_id, _block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        let mut state = state;
        state.review_scope = ScopePreset::MainDiff;
        state
            .file_change_kinds
            .insert(file_id, FileChangeKind::Deleted);
        let palette = UiPalette::default();
        let file_node = state.navigator.tree.node(file_id);

        let header = build_header_lines(file_node, &state, &palette)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert_eq!(header, vec!["File Deleted · File src/lib.rs"]);
        assert!(header.iter().all(|line| !line.starts_with("Mode: ")));

        state.view_mode = ViewMode::Source;
        let source_header = build_header_lines(file_node, &state, &palette)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert_eq!(source_header, vec!["File Deleted · File src/lib.rs"]);
    }

    #[test]
    fn build_header_lines_show_block_change_metadata_independent_from_file_metadata() {
        let file_path = temp_test_file_path("tui_header_block_change_label");
        let file_content = "fn demo() {\n    old();\n}\n";
        let block_content = file_content;
        let (state, file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 3);
        let mut state = state;
        state.review_scope = ScopePreset::MainDiff;
        state
            .file_change_kinds
            .insert(file_id, FileChangeKind::Changed);
        state
            .block_change_kinds
            .insert(block_id, BlockChangeKind::Added);
        let palette = UiPalette::default();
        let block_node = state.navigator.tree.node(block_id);

        let header = build_header_lines(block_node, &state, &palette)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            header,
            vec![
                "src/lib.rs".to_string(),
                format!(
                    "  -> Block Added · function demo (hash={})",
                    short_tree_hash(&block_node.hash)
                ),
            ]
        );
        assert!(header.iter().all(|line| line != "File Changed"));
    }

    #[test]
    fn build_header_lines_show_unknown_change_when_diff_metadata_missing() {
        let file_path = temp_test_file_path("tui_header_unknown_change_label");
        let file_content = "line1\n";
        let block_content = "line1\n";
        let (state, _file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 0, 1);
        let mut state = state;
        state.review_scope = ScopePreset::MainDiff;
        let palette = UiPalette::default();
        let block_node = state.navigator.tree.node(block_id);

        let header = build_header_lines(block_node, &state, &palette)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            header,
            vec![
                "src/lib.rs".to_string(),
                format!(
                    "  -> Unknown Change · function (hash={})",
                    short_tree_hash(&block_node.hash)
                ),
            ]
        );
    }

    #[test]
    fn content_cache_key_ignores_height_for_block_diff_when_width_matches() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_b = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            40,
            80,
        );

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn content_cache_key_tracks_block_diff_width() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_b = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            40,
        );

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn content_cache_key_distinguishes_file_modes() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let diff_key = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::File,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let source_key = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::File,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::ChangedWithContext { context_lines: 3 },
            40,
            80,
        );

        assert_ne!(diff_key, source_key);
    }

    #[test]
    fn content_cache_key_distinguishes_focus_blocks() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            Some(node_id),
            TreeNodeKind::File,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_b = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::File,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn scroll_offset_for_focus_range_centers_short_focus_span() {
        let offset = scroll_offset_for_focus_range(&(10..12), 8, 40);
        assert_eq!(offset, 7);
    }

    #[test]
    fn scroll_offset_for_focus_range_top_aligns_tall_focus_span() {
        let offset = scroll_offset_for_focus_range(&(10..20), 5, 40);
        assert_eq!(offset, 10);
    }

    #[test]
    fn handle_child_lazily_expands_small_markdown_section_into_inclusive_sections() {
        let content =
            "# Root\nIntro paragraph.\n\n## Coding\nDetails live here.\n\n### Dev Guide\nSteps.\n";
        let (mut state, file_id, root_section) =
            build_state_with_single_markdown_section("docs/guide.md", content);

        assert_eq!(state.navigator.current_id(), file_id);
        assert_eq!(state.reviewable_nodes.len(), 1);

        handle_child(&mut state);
        assert_eq!(state.navigator.current_id(), root_section);

        handle_child(&mut state);
        let current = state.navigator.tree.node(state.navigator.current_id());
        assert_eq!(
            current.block.as_ref().map(|block| block.kind),
            Some(BlockKind::Section)
        );
        assert_eq!(
            current.block.as_ref().map(|block| block.content.as_str()),
            Some("# Root\nIntro paragraph.\n\n")
        );

        let child_blocks: Vec<_> = state
            .navigator
            .tree
            .node(root_section)
            .children
            .iter()
            .map(|child| state.navigator.tree.node(*child).block.as_ref().unwrap())
            .collect();
        assert_eq!(child_blocks.len(), 2);
        assert!(
            child_blocks
                .iter()
                .all(|block| block.kind == BlockKind::Section)
        );
        assert_eq!(child_blocks[0].content, "# Root\nIntro paragraph.\n\n");
        assert_eq!(
            child_blocks[1].content,
            "## Coding\nDetails live here.\n\n### Dev Guide\nSteps.\n"
        );
        assert!(!state.reviewable_nodes.contains(&root_section));
        assert_eq!(state.reviewable_nodes.len(), 2);

        handle_next(&mut state);
        let nested_section = state.navigator.current_id();
        assert_eq!(
            state
                .navigator
                .tree
                .node(nested_section)
                .block
                .as_ref()
                .map(|block| block.kind),
            Some(BlockKind::Section)
        );

        handle_child(&mut state);
        let nested_current = state.navigator.tree.node(state.navigator.current_id());
        assert_eq!(
            nested_current.block.as_ref().map(|block| block.kind),
            Some(BlockKind::Section)
        );
        assert_eq!(
            nested_current
                .block
                .as_ref()
                .map(|block| block.content.as_str()),
            Some("## Coding\nDetails live here.\n\n")
        );

        let nested_child_blocks: Vec<_> = state
            .navigator
            .tree
            .node(nested_section)
            .children
            .iter()
            .map(|child| state.navigator.tree.node(*child).block.as_ref().unwrap())
            .collect();
        assert_eq!(nested_child_blocks.len(), 2);
        assert!(
            nested_child_blocks
                .iter()
                .all(|block| block.kind == BlockKind::Section)
        );
        assert_eq!(
            nested_child_blocks[0].content,
            "## Coding\nDetails live here.\n\n"
        );
        assert_eq!(nested_child_blocks[1].content, "### Dev Guide\nSteps.\n");
    }

    #[test]
    fn handle_child_lazily_expands_small_markdown_paragraph_into_sentences() {
        let content = "# Root\nSentence one. Sentence two?\n";
        let (mut state, _file_id, _root_section) =
            build_state_with_markdown_file("docs/guide.md", content);

        handle_child(&mut state);
        handle_child(&mut state);
        handle_next(&mut state);

        let paragraph_id = state.navigator.current_id();
        assert_eq!(
            state
                .navigator
                .tree
                .node(paragraph_id)
                .block
                .as_ref()
                .map(|block| block.kind),
            Some(BlockKind::Paragraph)
        );

        handle_child(&mut state);
        let first_sentence_id = state.navigator.current_id();
        let sentence = state.navigator.tree.node(first_sentence_id);
        assert_eq!(
            sentence.block.as_ref().map(|block| block.kind),
            Some(BlockKind::Sentence)
        );
        assert!(!state.reviewable_nodes.contains(&paragraph_id));

        let sentence_children = state.navigator.tree.node(paragraph_id).children.clone();
        let sentence_kinds: Vec<_> = sentence_children
            .iter()
            .map(|child| {
                state
                    .navigator
                    .tree
                    .node(*child)
                    .block
                    .as_ref()
                    .map(|block| block.kind)
            })
            .collect();
        assert_eq!(
            sentence_kinds,
            vec![Some(BlockKind::Sentence), Some(BlockKind::Sentence)]
        );
        assert_eq!(
            compute_next_review_target(&state, first_sentence_id),
            sentence_children.get(1).copied()
        );
    }

    #[test]
    fn execute_action_with_comment_keeps_current_block_selected() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        let first_block = block_ids[0];
        state.navigator.set_current(first_block);
        state.focus_block = Some(first_block);

        execute_action_with(
            &mut state,
            PendingAction::Single {
                node_id: first_block,
                verdict: Verdict::Comment,
                note: Some("note".to_string()),
            },
            |_params| Ok(()),
        )
        .unwrap_or_else(|error| panic!("expected comment action to succeed: {error}"));

        assert_eq!(state.navigator.current_id(), first_block);
        assert_eq!(state.remaining_blocks, 2);
        assert_eq!(state.session_recap.comments, 1);
    }

    #[test]
    fn execute_action_with_comment_preserves_scroll_position_and_focus_state() {
        let file_path = temp_test_file_path("tui_comment_scroll_preserve");
        let file_lines = (1..=20)
            .map(|index| format!("scroll_line_{index:02}"))
            .collect::<Vec<_>>();
        let file_content = format!("{}\n", file_lines.join("\n"));
        let (mut state, _file_id, block_id) = build_state_with_block_file(
            &file_path,
            &file_content,
            &file_content,
            0,
            file_lines.len(),
        );
        state.navigator.set_current(block_id);
        state.focus_block = Some(block_id);
        state.pending_focus_scroll = false;
        state.scroll_offset = 5;

        execute_action_with(
            &mut state,
            PendingAction::Single {
                node_id: block_id,
                verdict: Verdict::Comment,
                note: Some("note".to_string()),
            },
            |_params| Ok(()),
        )
        .unwrap_or_else(|error| panic!("expected comment action to succeed: {error}"));

        assert_eq!(state.navigator.current_id(), block_id);
        assert_eq!(state.scroll_offset, 5);
        assert_eq!(state.focus_block, Some(block_id));
        assert!(!state.pending_focus_scroll);
        assert_eq!(state.session_recap.comments, 1);
    }

    #[test]
    fn handle_advance_review_target_moves_to_next_remaining_block() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        let first_block = block_ids[0];
        let second_block = block_ids[1];
        state.navigator.set_current(first_block);
        state.focus_block = Some(first_block);

        handle_advance_review_target(&mut state);

        assert_eq!(state.navigator.current_id(), second_block);
    }

    #[test]
    fn handle_advance_review_target_marks_current_block_temporarily_skipped() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        let first_block = block_ids[0];
        state.navigator.set_current(first_block);
        state.focus_block = Some(first_block);

        handle_advance_review_target(&mut state);

        assert!(state.skipped_nodes.contains(&first_block));
        let counts = footer_progress_counts(&state);
        assert_eq!(counts.skipped, 1);
        assert_eq!(counts.remaining, 2);
    }

    #[test]
    fn handle_next_marks_current_block_temporarily_skipped() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        let first_block = block_ids[0];
        state.navigator.set_current(first_block);
        state.focus_block = Some(first_block);

        handle_next(&mut state);

        assert!(state.skipped_nodes.contains(&first_block));
    }

    #[test]
    fn comment_action_clears_temporary_skip_state() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(1);
        let block_id = block_ids[0];
        state.skipped_nodes.insert(block_id);

        execute_action_with(
            &mut state,
            PendingAction::Single {
                node_id: block_id,
                verdict: Verdict::Comment,
                note: Some("note".to_string()),
            },
            |_params| Ok(()),
        )
        .unwrap_or_else(|error| panic!("expected comment action to succeed: {error}"));

        assert!(!state.skipped_nodes.contains(&block_id));
        assert!(state.commented_nodes.contains(&block_id));
    }

    #[test]
    fn batch_confirmation_threshold_defaults_to_skipping_single_sub_block_batch_actions() {
        let (state, file_id, _block_ids) = build_state_with_file_block_count(1);
        let action = PendingAction::from_node(&state.navigator.tree, file_id, Verdict::Approved);

        assert_eq!(batch_confirmation_count_for_action(&state, &action), None);
    }

    #[test]
    fn batch_confirmation_threshold_can_confirm_single_sub_block_batch_actions() {
        let (mut state, file_id, _block_ids) = build_state_with_file_block_count(1);
        state.confirm_batch = BatchConfirmPolicy::Threshold(1);
        let action = PendingAction::from_node(&state.navigator.tree, file_id, Verdict::Approved);

        assert_eq!(
            batch_confirmation_count_for_action(&state, &action),
            Some(1)
        );
    }

    #[test]
    fn batch_confirmation_threshold_never_disables_batch_confirmation() {
        let (mut state, file_id, _block_ids) = build_state_with_file_block_count(3);
        state.confirm_batch = BatchConfirmPolicy::Never;
        let action = PendingAction::from_node(&state.navigator.tree, file_id, Verdict::Approved);

        assert_eq!(batch_confirmation_count_for_action(&state, &action), None);
    }

    #[test]
    fn handle_parent_preserves_child_focus_block_for_parent_file() {
        let file_path = temp_test_file_path("tui_parent_focus_anchor");
        let file_content = "line1\nline2\nline3\n";
        let block_content = "line2\n";
        let (mut state, file_id, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 1, 2);

        handle_parent(&mut state);

        assert_eq!(state.navigator.current_id(), file_id);
        assert_eq!(state.focus_block, Some(block_id));
        assert!(state.pending_focus_scroll);
    }

    #[test]
    fn content_cache_key_ignores_height_for_file_modes() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::File,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_b = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::File,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            40,
            80,
        );

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn content_cache_key_is_none_for_non_cacheable_kinds() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Directory,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            12,
            80,
        );
        assert!(key.is_none());
    }

    #[test]
    fn content_frame_cache_stores_multiple_keys() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );
        let key_b = content_frame_cache_key(
            node_id,
            None,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
            80,
        );

        let Some(key_a) = key_a else {
            panic!("expected cache key");
        };
        let Some(key_b) = key_b else {
            panic!("expected cache key");
        };

        let mut cache = HashMap::new();
        cache.insert(
            key_a,
            ContentFrameCacheEntry {
                lines: vec![Line::from("a")],
                total_lines: 1,
                focus_row_range: None,
                comment_rows: None,
            },
        );
        cache.insert(
            key_b,
            ContentFrameCacheEntry {
                lines: vec![Line::from("b")],
                total_lines: 1,
                focus_row_range: None,
                comment_rows: None,
            },
        );

        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn highlighted_tokens_for_line_reuses_cached_tokens() {
        let mut cache = HashMap::new();
        let first =
            highlighted_tokens_for_line(&mut cache, "let value = 42;", Some(&Language::Rust));
        let second =
            highlighted_tokens_for_line(&mut cache, "let value = 42;", Some(&Language::Rust));

        assert_eq!(first, second);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn highlight_line_rust_recognizes_keyword_number_and_comment() {
        let tokens = highlight_line("let value = 42; // note", Some(&Language::Rust));
        let rendered = tokens
            .iter()
            .map(|token| (token.text.as_str(), token.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                ("let", TokenKind::Keyword),
                (" value = ", TokenKind::Base),
                ("42", TokenKind::Number),
                ("; ", TokenKind::Base),
                ("// note", TokenKind::Comment),
            ]
        );
    }

    #[test]
    fn highlight_line_rust_recognizes_string_literals() {
        let tokens = highlight_line("println!(\"hi\");", Some(&Language::Rust));
        let rendered = tokens
            .iter()
            .map(|token| (token.text.as_str(), token.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                ("println!(", TokenKind::Base),
                ("\"hi\"", TokenKind::String),
                (");", TokenKind::Base),
            ]
        );
    }

    #[test]
    fn highlight_line_markdown_recognizes_headings() {
        let tokens = highlight_line("## Setup", Some(&Language::Markdown));
        let rendered = tokens
            .iter()
            .map(|token| (token.text.as_str(), token.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![("##", TokenKind::Keyword), (" Setup", TokenKind::Strong)]
        );
    }

    #[test]
    fn highlight_line_markdown_recognizes_lists_and_inline_code() {
        let tokens = highlight_line("- Run `trueflow tui` first", Some(&Language::Markdown));
        let rendered = tokens
            .iter()
            .map(|token| (token.text.as_str(), token.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                ("-", TokenKind::Keyword),
                (" Run ", TokenKind::Base),
                ("`trueflow tui`", TokenKind::InlineCode),
                (" first", TokenKind::Base),
            ]
        );
    }

    #[test]
    fn highlight_line_markdown_recognizes_bold_and_italic_spans() {
        let tokens = highlight_line("Use **bold** and *italic* text", Some(&Language::Markdown));
        let rendered = tokens
            .iter()
            .map(|token| (token.text.as_str(), token.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                ("Use ", TokenKind::Base),
                ("**bold**", TokenKind::Strong),
                (" and ", TokenKind::Base),
                ("*italic*", TokenKind::Emphasis),
                (" text", TokenKind::Base),
            ]
        );
    }

    #[test]
    fn highlight_line_markdown_recognizes_links_quotes_and_fences() {
        let link_tokens = highlight_line(
            "Read [docs](https://trueflow.dev).",
            Some(&Language::Markdown),
        );
        assert_eq!(
            link_tokens
                .iter()
                .map(|token| (token.text.as_str(), token.kind))
                .collect::<Vec<_>>(),
            vec![
                ("Read ", TokenKind::Base),
                ("[docs](https://trueflow.dev)", TokenKind::Link),
                (".", TokenKind::Base),
            ]
        );

        assert_eq!(
            highlight_line("> quoted", Some(&Language::Markdown))
                .iter()
                .map(|token| (token.text.as_str(), token.kind))
                .collect::<Vec<_>>(),
            vec![("> quoted", TokenKind::Comment)]
        );
        assert_eq!(
            highlight_line("```rust", Some(&Language::Markdown))
                .iter()
                .map(|token| (token.text.as_str(), token.kind))
                .collect::<Vec<_>>(),
            vec![("```rust", TokenKind::InlineCode)]
        );
    }

    #[test]
    fn highlight_line_unknown_language_falls_back_to_plain_text() {
        let tokens = highlight_line("let value = 42; // note", Some(&Language::Unknown));

        assert_eq!(
            tokens,
            vec![HighlightToken {
                text: "let value = 42; // note".to_string(),
                kind: TokenKind::Base,
            }]
        );
    }

    #[test]
    fn highlight_line_without_language_falls_back_to_plain_text() {
        let tokens = highlight_line("let value = 42;", None);

        assert_eq!(
            tokens,
            vec![HighlightToken {
                text: "let value = 42;".to_string(),
                kind: TokenKind::Base,
            }]
        );
    }

    #[test]
    fn build_commit_scope_diff_uses_historical_file_content_when_current_path_is_missing() {
        let repo_root = temp_git_repo("tui_commit_scope_missing_current_path");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_else(|error| panic!("failed to create fixture directory: {error}"));

        fs::write(
            &file_path,
            "pub fn demo() {\n    println!(\"before\");\n}\n",
        )
        .unwrap_or_else(|error| panic!("failed to write initial fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial"]);

        fs::write(
            &file_path,
            "pub fn demo() {\n    println!(\"historical target\");\n}\n",
        )
        .unwrap_or_else(|error| panic!("failed to write target fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update old path"]);
        let target_revision = run_git_stdout(&repo_root, &["rev-parse", "HEAD"])
            .trim()
            .to_string();

        run_git(&repo_root, &["mv", "src/lib.rs", "src/renamed.rs"]);
        run_git(&repo_root, &["commit", "-m", "Rename old path"]);
        assert!(
            !file_path.exists(),
            "test setup should remove old path from current worktree"
        );

        let mut state = build_test_state(
            ScopePreset::Commit {
                id: target_revision,
                summary: "Update old path".to_string(),
            },
            HashMap::new(),
        );
        state.repo_root = Some(repo_root);
        let node = ContentNodeSnapshot {
            id: state.navigator.tree.root(),
            kind: TreeNodeKind::Block,
            path: RepoPath::new("src/lib.rs").unwrap(),
            children: Vec::new(),
            block: Some(Block {
                hash: crate::hashing::TreeHash::new("block"),
                content: "pub fn demo() {\n    println!(\"historical target\");\n}".to_string(),
                kind: BlockKind::Function,
                tags: vec![],
                complexity: None,
                start_line: 0,
                end_line: 3,
            }),
            language: Some(Language::Rust),
        };
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &node, &palette, 5, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert!(
            rendered.iter().all(|line| !line.contains("(File missing)")),
            "historical commit scope should not depend on current worktree path: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("historical target")),
            "expected rendered diff to use historical file content: {rendered:?}"
        );
    }

    #[test]
    fn build_block_diff_lines_uses_repo_relative_path_for_commit_scope_blocks() {
        let repo_root = temp_git_repo("tui_commit_scope");
        let package_dir = repo_root.join("pkg");
        let file_path = package_dir.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_else(|error| panic!("failed to create fixture directory: {error}"));

        let initial = include_str!("../../example_repos/basic_changes/src/main.rs");
        let updated = initial.replace("Hello, world!", "Hello from commit scope");
        fs::write(&file_path, initial)
            .unwrap_or_else(|error| panic!("failed to write initial fixture file: {error}"));

        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial"]);

        fs::write(&file_path, updated)
            .unwrap_or_else(|error| panic!("failed to write updated fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Update greeting"]);

        let revision = run_git_stdout(&repo_root, &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let repo = gix::open(&repo_root)
            .unwrap_or_else(|error| panic!("failed to open git fixture repo: {error}"));
        let hunks = vcs::diff_hunks_for_file_in_revision(
            &repo,
            &revision,
            &RepoPath::new("pkg/src/lib.rs").unwrap(),
        )
        .unwrap_or_else(|error| panic!("failed to compute revision diff hunks: {error}"));
        assert!(
            !hunks.is_empty(),
            "expected fixture commit to produce diff hunks"
        );

        let mut state = build_test_state(
            ScopePreset::Commit {
                id: revision,
                summary: "Update greeting".to_string(),
            },
            HashMap::from([(
                PathBuf::from("pkg/src/lib.rs"),
                vcs::FileDiff::Text {
                    path: RepoPath::new("pkg/src/lib.rs").unwrap(),
                    hunks,
                },
            )]),
        );

        let node = ContentNodeSnapshot {
            id: state.navigator.tree.root(),
            kind: TreeNodeKind::Block,
            path: RepoPath::new("src/lib.rs").unwrap(),
            children: Vec::new(),
            block: Some(Block {
                hash: crate::hashing::TreeHash::new("block"),
                content: "fn main() {\n    println!(\"Hello from commit scope\");\n}".to_string(),
                kind: BlockKind::Function,
                tags: vec![],
                complexity: None,
                start_line: 0,
                end_line: 3,
            }),
            language: Some(Language::Rust),
        };
        let palette = UiPalette::default();

        let content = build_block_lines(&mut state, &node, &palette, 3, 80);
        let rendered = content
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert!(
            rendered
                .iter()
                .all(|line| !line.contains("(No diff changes in this block)")),
            "expected real diff rows for commit scope block, got: {rendered:?}"
        );
    }

    #[test]
    fn fingerprint_and_target_kind_for_root_uses_tree_hash() {
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let src = builder.add_dir(root, "src".to_string(), "src".to_string());
        let file = builder.add_file(
            src,
            "lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "file-hash".to_string(),
            Language::Rust,
        );
        builder.add_block(
            file,
            "function".to_string(),
            "src/lib.rs".to_string(),
            Block::new("fn run() {}".to_string(), BlockKind::Function, 0, 1),
            Language::Rust,
        );
        let tree = builder.finalize();
        let root_node = tree.node(root);

        let (fingerprint, kind) = fingerprint_and_target_kind_for_node(root_node);
        assert_eq!(fingerprint, root_node.hash.to_string());
        assert_eq!(kind, ReviewTargetKind::Tree);
    }

    #[test]
    fn footer_progress_uses_session_remaining_blocks_not_scope_total() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.total_blocks = 5;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 1;

        let counts = footer_progress_counts(&state);

        assert_eq!(counts.reviewed, 0);
        assert_eq!(counts.commented, 0);
        assert_eq!(counts.skipped, 0);
        assert_eq!(counts.remaining, 1);
        assert_eq!(counts.total, 1);
    }

    #[test]
    fn footer_progress_reaches_complete_when_session_blocks_are_done() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.total_blocks = 5;
        state.initial_remaining_blocks = 1;
        state.remaining_blocks = 0;

        let ratio = footer_progress_ratio(&state);
        let counts = footer_progress_counts(&state);

        assert_eq!(counts.reviewed, 1);
        assert_eq!(counts.commented, 0);
        assert_eq!(counts.skipped, 0);
        assert_eq!(counts.remaining, 0);
        assert_eq!(counts.total, 1);
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn footer_progress_counts_commented_blocks_as_still_remaining() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        let first_block = block_ids[0];

        execute_action_with(
            &mut state,
            PendingAction::Single {
                node_id: first_block,
                verdict: Verdict::Comment,
                note: Some("note".to_string()),
            },
            |_params| Ok(()),
        )
        .unwrap_or_else(|error| panic!("expected comment action to succeed: {error}"));

        let counts = footer_progress_counts(&state);

        assert_eq!(counts.reviewed, 0);
        assert_eq!(counts.commented, 1);
        assert_eq!(counts.skipped, 0);
        assert_eq!(counts.remaining, 2);
        assert_eq!(counts.total, 2);
    }

    #[test]
    fn footer_progress_line_formats_review_comment_and_remaining_counts() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(2);
        state.reviewable_nodes.remove(&block_ids[0]);
        state.remaining_blocks = 1;
        state.commented_nodes.insert(block_ids[1]);
        let palette = UiPalette::default();

        let line = footer_progress_line(&state, &palette);

        assert_eq!(line.to_string(), "1 reviewed · 1 commented · 1 remaining");
        assert_eq!(line.spans[0].style.fg, Some(palette.add));
        assert_eq!(line.spans[2].style.fg, Some(palette.orange));
        assert_eq!(line.spans[4].style.fg, Some(palette.yellow));
    }

    #[test]
    fn footer_progress_line_includes_skipped_count_when_present() {
        let (mut state, _file_id, block_ids) = build_state_with_file_block_count(3);
        state.reviewable_nodes.remove(&block_ids[0]);
        state.remaining_blocks = 2;
        state.commented_nodes.insert(block_ids[1]);
        state.skipped_nodes.insert(block_ids[2]);
        let palette = UiPalette::default();

        let line = footer_progress_line(&state, &palette);

        assert_eq!(
            line.to_string(),
            "1 reviewed · 1 commented · 1 skipped · 2 remaining"
        );
        assert_eq!(line.spans[4].style.fg, Some(palette.dim));
    }

    #[test]
    fn footer_progress_bar_widths_stack_comment_skip_and_review_segments() {
        let counts = FooterProgressCounts {
            reviewed: 2,
            commented: 1,
            skipped: 1,
            remaining: 2,
            total: 4,
        };

        let widths = footer_progress_bar_widths(counts, 20);

        assert_eq!(
            widths,
            FooterProgressBarWidths {
                commented: 5,
                skipped: 5,
                reviewed: 10,
                empty: 0,
            }
        );
    }

    #[test]
    fn footer_progress_bar_widths_leave_empty_space_for_unseen_remaining_blocks() {
        let counts = FooterProgressCounts {
            reviewed: 1,
            commented: 1,
            skipped: 1,
            remaining: 4,
            total: 5,
        };

        let widths = footer_progress_bar_widths(counts, 20);

        assert_eq!(
            widths,
            FooterProgressBarWidths {
                commented: 4,
                skipped: 4,
                reviewed: 4,
                empty: 8,
            }
        );
    }

    #[test]
    fn recap_summary_reports_no_activity_when_session_is_empty() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.total_blocks = 3;
        state.initial_remaining_blocks = 0;
        state.remaining_blocks = 0;

        let lines = recap_summary_lines(&state);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("All Done (no reviews or feedback recorded)")),
            "expected no-activity recap message, got: {lines:?}"
        );
    }

    #[test]
    fn recap_summary_reports_scope_coverage_delta_and_rollups() {
        let mut state = build_test_state(ScopePreset::MainDiff, HashMap::new());
        state.total_blocks = 10;
        state.initial_remaining_blocks = 4;
        state.remaining_blocks = 0;
        state.session_recap.approved_blocks = 4;
        state.session_recap.comments = 2;

        let lines = recap_summary_lines(&state);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Scope coverage: 60.0% -> 100.0% (+40.0%)")),
            "expected coverage delta line, got: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Approvals: 4 blocks")),
            "expected approvals rollup, got: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("Notes: 2")),
            "expected notes rollup, got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("Questions:")),
            "did not expect separate questions rollup, got: {lines:?}"
        );
    }

    #[test]
    fn recap_key_handler_uses_configured_done_quit_and_escape_keys() {
        let keybinds = crate::config::TuiKeybindsConfig {
            recap_done: 'f',
            quit: 'x',
            ..crate::config::TuiKeybindsConfig::default()
        };
        assert_eq!(
            recap_action_for_key_code(&keybinds, KeyCode::Char('f')),
            Some(RecapAction::ReviewSomethingElse)
        );
        assert_eq!(
            recap_action_for_key_code(&keybinds, KeyCode::Char('x')),
            Some(RecapAction::Exit)
        );
        assert_eq!(
            recap_action_for_key_code(&keybinds, KeyCode::Esc),
            Some(RecapAction::Exit)
        );
        assert_eq!(
            recap_action_for_key_code(&keybinds, KeyCode::Char('d')),
            None
        );
        assert_eq!(
            recap_action_for_key_code(&keybinds, KeyCode::Char('m')),
            None
        );
    }

    #[test]
    fn recap_footer_hint_text_uses_configured_keybinds() {
        let default_keybinds = crate::config::TuiKeybindsConfig::default();
        assert_eq!(
            recap_footer_hint_text(&default_keybinds),
            "Press [d] review something else or [q/Esc] exit"
        );

        let custom_keybinds = crate::config::TuiKeybindsConfig {
            scroll_up: 'i',
            scroll_down: 'k',
            prev: 'j',
            next: 'l',
            parent: 'u',
            child: 'o',
            approve: 'y',
            note: 'e',
            toggle_view: 'v',
            speed_read: 's',
            root: 'z',
            recap_done: 'f',
            quit: 'x',
        };
        assert_eq!(
            recap_footer_hint_text(&custom_keybinds),
            "Press [f] review something else or [x/Esc] exit"
        );
    }

    #[test]
    fn execute_mark_for_tui_without_suspend_requirement_runs_without_suspend() {
        let calls = std::cell::RefCell::new(Vec::new());

        execute_mark_for_tui(
            mark::TerminalSuspendRequirement::NotRequired,
            || {
                calls.borrow_mut().push("noninteractive");
                Ok(())
            },
            || {
                calls.borrow_mut().push("suspend");
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("expected mark execution: {error}"));

        assert_eq!(calls.into_inner(), vec!["noninteractive"]);
    }

    #[test]
    fn execute_mark_for_tui_with_suspend_requirement_skips_suspend_when_noninteractive_succeeds() {
        let calls = std::cell::RefCell::new(Vec::new());

        execute_mark_for_tui(
            mark::TerminalSuspendRequirement::Required,
            || {
                calls.borrow_mut().push("noninteractive");
                Ok(())
            },
            || {
                calls.borrow_mut().push("suspend");
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("expected mark execution: {error}"));

        assert_eq!(calls.into_inner(), vec!["noninteractive"]);
    }

    #[test]
    fn execute_mark_for_tui_falls_back_after_noninteractive_signing_failure() {
        let calls = std::cell::RefCell::new(Vec::new());

        execute_mark_for_tui(
            mark::TerminalSuspendRequirement::Required,
            || {
                calls.borrow_mut().push("noninteractive");
                Err(anyhow!("non-interactive GPG signing failed"))
            },
            || {
                calls.borrow_mut().push("suspend");
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("expected fallback mark execution: {error}"));

        assert_eq!(calls.into_inner(), vec!["noninteractive", "suspend"]);
    }

    #[test]
    fn execute_mark_for_tui_with_suspend_requirement_does_not_retry_non_signing_failure() {
        let calls = std::cell::RefCell::new(Vec::new());

        let error = execute_mark_for_tui(
            mark::TerminalSuspendRequirement::Required,
            || {
                calls.borrow_mut().push("noninteractive");
                Err(anyhow!("store append failed"))
            },
            || {
                calls.borrow_mut().push("suspend");
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "store append failed");
        assert_eq!(calls.into_inner(), vec!["noninteractive"]);
    }

    #[test]
    fn mark_terminal_suspend_requirement_without_signing_key_is_not_required() {
        assert_eq!(
            mark::suspend_policy_for_signing_key(None),
            mark::TerminalSuspendRequirement::NotRequired
        );
    }

    #[test]
    fn mark_terminal_suspend_requirement_with_signing_key_is_required() {
        assert_eq!(
            mark::suspend_policy_for_signing_key(Some("ABC123")),
            mark::TerminalSuspendRequirement::Required
        );
    }

    #[test]
    fn toggle_speed_read_mode_starts_paused_on_block_node() {
        let (mut state, block_id) = build_state_with_single_block("alpha beta gamma");
        assert!(state.speed_read.is_none());

        toggle_speed_read_mode(&mut state);

        let Some(mode) = state.speed_read.as_ref() else {
            panic!("expected speed read mode to activate");
        };
        assert_eq!(mode.node_id, block_id);
        assert_eq!(mode.model.playback, PlaybackState::Paused);
        assert!(mode.next_tick_at.is_none());
    }

    #[test]
    fn toggle_speed_read_mode_ignores_non_block_nodes() {
        let mut state = build_test_state(ScopePreset::All, HashMap::new());
        state.navigator.jump_root();

        toggle_speed_read_mode(&mut state);

        assert!(state.speed_read.is_none());
    }

    #[test]
    fn speed_read_space_starts_playback_when_mode_is_paused() {
        let (mut state, _) = build_state_with_single_block("alpha beta gamma delta");
        toggle_speed_read_mode(&mut state);

        assert!(handle_speed_read_key_binding(
            &mut state,
            KeyCode::Char(' ')
        ));

        let Some(mode) = state.speed_read.as_ref() else {
            panic!("expected speed read mode to remain active");
        };
        assert_eq!(mode.model.playback, PlaybackState::Playing);
        assert!(mode.next_tick_at.is_some());
    }

    #[test]
    fn speed_read_boundary_adjustments_do_not_write_unchanged_defaults() {
        let dir = temp_test_dir("speed_read_noop_boundary_persist");
        let config_path = dir.join("trueflow.toml");
        let config = TuiSpeedReadConfig {
            default_wpm: 900,
            max_wpm: 900,
            default_chunk_words: 5,
            max_chunk_words: 5,
            ..TuiSpeedReadConfig::default()
        };
        let (mut state, _) = build_state_with_single_block("alpha beta gamma delta");
        state.speed_read = SpeedReadController::new(config, config_path.clone());

        toggle_speed_read_mode(&mut state);
        assert!(handle_speed_read_key_binding(
            &mut state,
            KeyCode::Char('=')
        ));
        assert!(handle_speed_read_key_binding(
            &mut state,
            KeyCode::Char(']')
        ));

        let did_flush = state
            .speed_read
            .flush_due_defaults(Instant::now() + std::time::Duration::from_secs(1))
            .unwrap_or_else(|error| panic!("flush speed-read defaults: {error}"));

        assert!(!did_flush);
        assert!(
            !config_path.exists(),
            "boundary no-op speed-read adjustments should not create a config file"
        );
    }

    #[test]
    fn toggle_speed_read_mode_with_whitespace_only_content_stays_paused_without_next_tick() {
        let (mut state, _) = build_state_with_single_block("  \n\t  ");

        toggle_speed_read_mode(&mut state);

        let Some(mode) = state.speed_read.as_ref() else {
            panic!("expected speed read mode to activate");
        };
        assert!(mode.model.phrases.is_empty());
        assert_eq!(mode.model.playback, PlaybackState::Paused);
        assert!(mode.next_tick_at.is_none());
    }

    #[test]
    fn speed_read_autoplay_timeout_exits_to_normal_view_at_end() {
        let (mut state, _) = build_state_with_single_block("alpha beta");
        toggle_speed_read_mode(&mut state);
        assert!(handle_speed_read_key_binding(
            &mut state,
            KeyCode::Char(' ')
        ));

        if let Some(mode) = state.speed_read.as_mut() {
            mode.next_tick_at = Some(Instant::now());
        }

        let updated = handle_speed_read_autoplay_timeout(
            &mut state,
            Instant::now() + std::time::Duration::from_millis(5),
        );
        assert!(updated);
        assert!(state.speed_read.is_none());
    }
}

struct UiPalette {
    bg: Color,
    fg: Color,
    code_fg: Color,
    dim: Color,
    add: Color,
    del: Color,
    orange: Color,
    yellow: Color,
    keyword: Color,
    string: Color,
    number: Color,
    comment: Color,
    code_bg: Color,
    meta_bg: Color,
    meta_border: Color,
    context: Color,
}

impl Default for UiPalette {
    fn default() -> Self {
        Self {
            bg: Color::Rgb(248, 248, 245),
            fg: Color::Rgb(60, 56, 54),
            code_fg: Color::Rgb(40, 40, 40),
            dim: Color::Rgb(146, 131, 116),
            add: Color::Rgb(184, 187, 38),
            del: Color::Rgb(251, 73, 52),
            orange: Color::Rgb(214, 93, 14),
            yellow: Color::Rgb(215, 153, 33),
            keyword: Color::Rgb(69, 133, 136),
            string: Color::Rgb(215, 153, 33),
            number: Color::Rgb(177, 98, 134),
            comment: Color::Rgb(124, 111, 100),
            code_bg: Color::Rgb(240, 240, 238),
            meta_bg: Color::Rgb(244, 244, 242),
            meta_border: Color::Rgb(204, 204, 200),
            context: Color::Rgb(200, 200, 196),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Base,
    Keyword,
    String,
    Number,
    Comment,
    InlineCode,
    Strong,
    Emphasis,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HighlightToken {
    text: String,
    kind: TokenKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HighlightLineCacheKey {
    line: String,
    language: Option<Language>,
}

#[derive(Debug, Clone, Copy)]
struct LanguageHighlightRules {
    keywords: &'static [&'static str],
    line_comment_start: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct LineHighlighter {
    rules: LanguageHighlightRules,
}

impl LineHighlighter {
    fn tokenize_line(self, line: &str) -> Vec<HighlightToken> {
        if line.is_empty() {
            return Vec::new();
        }

        let mut tokens = Vec::new();
        let mut index = 0;

        while index < line.len() {
            let rest = &line[index..];

            if let Some(comment_start) = self.rules.line_comment_start
                && rest.starts_with(comment_start)
            {
                tokens.push(HighlightToken {
                    text: rest.to_string(),
                    kind: TokenKind::Comment,
                });
                break;
            }

            if let Some(string_end) = consume_string_literal(rest) {
                tokens.push(HighlightToken {
                    text: rest[..string_end].to_string(),
                    kind: TokenKind::String,
                });
                index += string_end;
                continue;
            }

            if let Some(number_end) = consume_number(rest) {
                tokens.push(HighlightToken {
                    text: rest[..number_end].to_string(),
                    kind: TokenKind::Number,
                });
                index += number_end;
                continue;
            }

            if let Some(identifier_end) = consume_identifier(rest) {
                let text = &rest[..identifier_end];
                let kind = if self.rules.keywords.contains(&text) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Base
                };
                push_token(&mut tokens, text, kind);
                index += identifier_end;
                continue;
            }

            let char_end = rest
                .char_indices()
                .nth(1)
                .map(|(next, _)| next)
                .unwrap_or(rest.len());
            push_token(&mut tokens, &rest[..char_end], TokenKind::Base);
            index += char_end;
        }

        tokens
    }
}

fn highlight_line(line: &str, language: Option<&Language>) -> Vec<HighlightToken> {
    if matches!(language, Some(Language::Markdown)) {
        return highlight_markdown_line(line);
    }

    match line_highlighter_for(language) {
        Some(highlighter) => highlighter.tokenize_line(line),
        None => plain_text_tokens(line),
    }
}

fn highlight_markdown_line(line: &str) -> Vec<HighlightToken> {
    if line.is_empty() {
        return Vec::new();
    }

    let content_start = markdown_content_start(line);
    let (leading, content) = line.split_at(content_start);

    if markdown_is_fence_line(content) {
        return markdown_prefix_tokens(leading, content, TokenKind::InlineCode);
    }

    if let Some(marker_len) = markdown_heading_marker_len(content) {
        let (marker, heading) = content.split_at(marker_len);
        let mut tokens = Vec::new();
        push_token(&mut tokens, leading, TokenKind::Base);
        push_token(&mut tokens, marker, TokenKind::Keyword);
        push_token(&mut tokens, heading, TokenKind::Strong);
        return tokens;
    }

    if content.starts_with('>') {
        return markdown_prefix_tokens(leading, content, TokenKind::Comment);
    }

    if let Some(marker_len) = markdown_list_marker_len(content) {
        let (marker, rest) = content.split_at(marker_len);
        let mut tokens = Vec::new();
        push_token(&mut tokens, leading, TokenKind::Base);
        push_token(&mut tokens, marker.trim_end(), TokenKind::Keyword);
        let marker_tail = &marker[marker.trim_end().len()..];
        push_token(&mut tokens, marker_tail, TokenKind::Base);
        extend_tokens(&mut tokens, highlight_markdown_inline(rest));
        return tokens;
    }

    let mut tokens = Vec::new();
    push_token(&mut tokens, leading, TokenKind::Base);
    extend_tokens(&mut tokens, highlight_markdown_inline(content));
    tokens
}

fn markdown_content_start(line: &str) -> usize {
    let mut spaces = 0usize;
    for (index, ch) in line.char_indices() {
        match ch {
            ' ' if spaces < 3 => spaces += 1,
            _ => return index,
        }
    }
    line.len()
}

fn markdown_prefix_tokens(leading: &str, content: &str, kind: TokenKind) -> Vec<HighlightToken> {
    let mut tokens = Vec::new();
    push_token(&mut tokens, leading, TokenKind::Base);
    push_token(&mut tokens, content, kind);
    tokens
}

fn markdown_is_fence_line(content: &str) -> bool {
    content.starts_with("```") || content.starts_with("~~~")
}

fn markdown_heading_marker_len(content: &str) -> Option<usize> {
    let marker_len = content.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&marker_len) {
        return None;
    }
    if content.len() == marker_len
        || content[marker_len..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        Some(marker_len)
    } else {
        None
    }
}

fn markdown_list_marker_len(content: &str) -> Option<usize> {
    if content.len() >= 2 {
        let mut chars = content.chars();
        if let (Some(marker @ ('-' | '*' | '+')), Some(space)) = (chars.next(), chars.next())
            && space.is_whitespace()
        {
            return Some(marker.len_utf8() + space.len_utf8());
        }
    }

    let digit_len = content.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len == 0 || digit_len > 9 {
        return None;
    }
    let rest = &content[digit_len..];
    let mut chars = rest.chars();
    if let (Some(marker @ ('.' | ')')), Some(space)) = (chars.next(), chars.next())
        && space.is_whitespace()
    {
        return Some(digit_len + marker.len_utf8() + space.len_utf8());
    }
    None
}

fn highlight_markdown_inline(line: &str) -> Vec<HighlightToken> {
    let mut tokens = Vec::new();
    let mut index = 0usize;

    while index < line.len() {
        let rest = &line[index..];

        if let Some(token_len) = markdown_delimited_span_len(rest, "`") {
            push_token(&mut tokens, &rest[..token_len], TokenKind::InlineCode);
            index += token_len;
            continue;
        }

        if let Some(token_len) = markdown_delimited_span_len(rest, "**") {
            push_token(&mut tokens, &rest[..token_len], TokenKind::Strong);
            index += token_len;
            continue;
        }

        if let Some(token_len) = markdown_delimited_span_len(rest, "__") {
            push_token(&mut tokens, &rest[..token_len], TokenKind::Strong);
            index += token_len;
            continue;
        }

        if let Some(token_len) = markdown_delimited_span_len(rest, "*") {
            push_token(&mut tokens, &rest[..token_len], TokenKind::Emphasis);
            index += token_len;
            continue;
        }

        if let Some(token_len) = markdown_delimited_span_len(rest, "_") {
            push_token(&mut tokens, &rest[..token_len], TokenKind::Emphasis);
            index += token_len;
            continue;
        }

        if let Some(token_len) = markdown_link_span_len(rest) {
            push_token(&mut tokens, &rest[..token_len], TokenKind::Link);
            index += token_len;
            continue;
        }

        let plain_len = markdown_plain_span_len(rest);
        push_token(&mut tokens, &rest[..plain_len], TokenKind::Base);
        index += plain_len;
    }

    tokens
}

fn markdown_delimited_span_len(rest: &str, delimiter: &str) -> Option<usize> {
    if !rest.starts_with(delimiter) {
        return None;
    }
    let start = delimiter.len();
    let end = rest[start..].find(delimiter)? + start;
    (end > start).then_some(end + delimiter.len())
}

fn markdown_link_span_len(rest: &str) -> Option<usize> {
    if !rest.starts_with('[') {
        return None;
    }
    let label_end = rest.find("](")?;
    if label_end <= 1 {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = rest[url_start..].find(')')? + url_start;
    (url_end > url_start).then_some(url_end + 1)
}

fn markdown_plain_span_len(rest: &str) -> usize {
    rest.char_indices()
        .skip(1)
        .find_map(|(index, ch)| matches!(ch, '`' | '*' | '_' | '[').then_some(index))
        .unwrap_or(rest.len())
}

fn line_highlighter_for(language: Option<&Language>) -> Option<LineHighlighter> {
    let rules = match language.copied()? {
        Language::Rust => LanguageHighlightRules {
            keywords: RUST_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Swift => LanguageHighlightRules {
            keywords: SWIFT_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Elisp => LanguageHighlightRules {
            keywords: ELISP_KEYWORDS,
            line_comment_start: Some(";"),
        },
        Language::JavaScript => LanguageHighlightRules {
            keywords: JAVASCRIPT_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::TypeScript => LanguageHighlightRules {
            keywords: TYPESCRIPT_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Java | Language::Kotlin => LanguageHighlightRules {
            keywords: JAVA_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::CSharp => LanguageHighlightRules {
            keywords: JAVA_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Python | Language::Ruby => LanguageHighlightRules {
            keywords: PYTHON_KEYWORDS,
            line_comment_start: Some("#"),
        },
        Language::Php => LanguageHighlightRules {
            keywords: JAVA_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Go => LanguageHighlightRules {
            keywords: GO_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::C | Language::Cpp => LanguageHighlightRules {
            keywords: CPP_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Shell => LanguageHighlightRules {
            keywords: SHELL_KEYWORDS,
            line_comment_start: Some("#"),
        },
        Language::Nix => LanguageHighlightRules {
            keywords: NIX_KEYWORDS,
            line_comment_start: Some("#"),
        },
        Language::Just => LanguageHighlightRules {
            keywords: JUST_KEYWORDS,
            line_comment_start: Some("#"),
        },
        _ => return None,
    };

    Some(LineHighlighter { rules })
}

fn plain_text_tokens(line: &str) -> Vec<HighlightToken> {
    if line.is_empty() {
        Vec::new()
    } else {
        vec![HighlightToken {
            text: line.to_string(),
            kind: TokenKind::Base,
        }]
    }
}

fn expand_tabs_for_display(line: &str) -> String {
    let mut expanded = String::with_capacity(line.len());
    let mut column = 0usize;

    for ch in line.chars() {
        if ch == '\t' {
            let spaces = DISPLAY_TAB_WIDTH - (column % DISPLAY_TAB_WIDTH);
            for _ in 0..spaces {
                expanded.push(' ');
            }
            column = column.saturating_add(spaces);
            continue;
        }

        expanded.push(ch);
        column = column.saturating_add(ch.width().unwrap_or(0));
    }

    expanded
}

fn consume_string_literal(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if first != '"' {
        return None;
    }

    let mut escaped = false;
    for (index, ch) in chars {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(index + ch.len_utf8()),
            _ => {}
        }
    }

    Some(rest.len())
}

fn consume_number(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_digit() {
        return None;
    }

    let mut end = first.len_utf8();
    for (index, ch) in chars {
        if ch.is_ascii_digit() || matches!(ch, '_' | '.') {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn consume_identifier(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_alphabetic() || first == '_') {
        return None;
    }

    let mut end = first.len_utf8();
    for (index, ch) in chars {
        if ch.is_alphanumeric() || ch == '_' {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn extend_tokens(tokens: &mut Vec<HighlightToken>, new_tokens: Vec<HighlightToken>) {
    for token in new_tokens {
        push_token(tokens, &token.text, token.kind);
    }
}

fn push_token(tokens: &mut Vec<HighlightToken>, text: &str, kind: TokenKind) {
    if text.is_empty() {
        return;
    }

    if kind == TokenKind::Base
        && let Some(last) = tokens.last_mut()
        && last.kind == TokenKind::Base
    {
        last.text.push_str(text);
        return;
    }

    tokens.push(HighlightToken {
        text: text.to_string(),
        kind,
    });
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "else", "enum", "fn", "for", "if",
    "impl", "in", "let", "loop", "match", "mod", "mut", "pub", "return", "self", "Self", "static",
    "struct", "trait", "type", "unsafe", "use", "where", "while",
];
const SWIFT_KEYWORDS: &[&str] = &[
    "actor",
    "break",
    "case",
    "class",
    "continue",
    "default",
    "defer",
    "do",
    "else",
    "enum",
    "extension",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "let",
    "mutating",
    "protocol",
    "return",
    "struct",
    "switch",
    "throw",
    "try",
    "var",
    "where",
    "while",
];
const ELISP_KEYWORDS: &[&str] = &[
    "defconst",
    "defcustom",
    "defgroup",
    "defmacro",
    "defun",
    "if",
    "lambda",
    "let",
    "let*",
    "progn",
    "setq",
    "when",
    "unless",
    "while",
];
const JAVASCRIPT_KEYWORDS: &[&str] = &[
    "async", "await", "break", "case", "class", "const", "continue", "default", "else", "export",
    "extends", "for", "function", "if", "import", "in", "let", "new", "return", "switch", "this",
    "throw", "try", "var", "while", "yield",
];
const TYPESCRIPT_KEYWORDS: &[&str] = &[
    "abstract",
    "async",
    "await",
    "break",
    "case",
    "class",
    "const",
    "continue",
    "default",
    "else",
    "enum",
    "export",
    "extends",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "interface",
    "let",
    "new",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "switch",
    "type",
    "var",
    "while",
];
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "record",
    "return",
    "sealed",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "var",
    "void",
    "volatile",
    "while",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "async", "await", "class", "def", "elif", "else", "except", "False", "for",
    "from", "if", "import", "in", "is", "lambda", "None", "not", "or", "pass", "return", "True",
    "try", "while", "with", "yield",
];
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];
const CPP_KEYWORDS: &[&str] = &[
    "auto",
    "break",
    "case",
    "class",
    "const",
    "continue",
    "else",
    "enum",
    "for",
    "if",
    "include",
    "namespace",
    "return",
    "struct",
    "switch",
    "template",
    "typename",
    "using",
    "virtual",
    "while",
];
const SHELL_KEYWORDS: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in", "local",
    "return", "then", "while",
];
const NIX_KEYWORDS: &[&str] = &[
    "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with",
];
const JUST_KEYWORDS: &[&str] = &["alias", "export", "import", "mod", "set", "unexport"];

fn style_for_token(kind: TokenKind, palette: &UiPalette) -> Style {
    match kind {
        TokenKind::Base => Style::default().fg(palette.code_fg),
        TokenKind::Keyword => Style::default()
            .fg(palette.keyword)
            .add_modifier(Modifier::BOLD),
        TokenKind::String => Style::default().fg(palette.string),
        TokenKind::Number => Style::default().fg(palette.number),
        TokenKind::Comment => Style::default().fg(palette.comment),
        TokenKind::InlineCode => Style::default()
            .fg(palette.string)
            .add_modifier(Modifier::BOLD),
        TokenKind::Strong => Style::default()
            .fg(palette.code_fg)
            .add_modifier(Modifier::BOLD),
        TokenKind::Emphasis => Style::default()
            .fg(palette.code_fg)
            .add_modifier(Modifier::ITALIC),
        TokenKind::Link => Style::default()
            .fg(palette.keyword)
            .add_modifier(Modifier::UNDERLINED),
    }
}

fn format_code_line(
    highlighted_line_cache: &mut HashMap<HighlightLineCacheKey, Vec<HighlightToken>>,
    line: &str,
    palette: &UiPalette,
    language: Option<&Language>,
) -> Line<'static> {
    let tokens = highlighted_tokens_for_line(highlighted_line_cache, line, language);
    let mut spans = Vec::with_capacity(tokens.len());
    for token in tokens {
        spans.push(Span::styled(
            token.text,
            style_for_token(token.kind, palette).bg(palette.code_bg),
        ));
    }
    Line::from(spans)
}

fn highlighted_tokens_for_line(
    cache: &mut HashMap<HighlightLineCacheKey, Vec<HighlightToken>>,
    line: &str,
    language: Option<&Language>,
) -> Vec<HighlightToken> {
    let display_line = expand_tabs_for_display(line);
    let key = HighlightLineCacheKey {
        line: display_line.clone(),
        language: language.copied(),
    };
    if let Some(tokens) = cache.get(&key) {
        return tokens.clone();
    }

    let tokens = highlight_line(&display_line, language);
    cache.insert(key, tokens.clone());
    tokens
}
