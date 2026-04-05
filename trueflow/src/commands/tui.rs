use super::tui_speedread::SpeedReadController;
use crate::analysis::Language;
use crate::block::BlockKind;
use crate::commands::mark;
use crate::commands::review::{
    CollectedReview, ReviewTarget, collect_review, resolve_review_request,
};
use crate::config::{
    BlockFilters, TuiConfig, TuiDiffFocusMode, TuiKeybindsConfig, TuiSpeedReadConfig,
    load as load_config,
};
use crate::context::TrueflowContext;
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::review_metadata;
use crate::review_navigator::ReviewNavigator;
use crate::review_order::ReviewOrder;
use crate::review_scope::{
    CliSemanticReviewScope, DiffQuery, ReviewScope, ScopeOption, default_scope_options,
};
use crate::review_session;
use crate::review_speedread::PlaybackState;
use crate::store::{ReviewTargetKind, Verdict};
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
use crate::vcs;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block as UiBlock, Gauge, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

// --- Core Structs ---

#[derive(Debug, Clone)]
struct ScopeSelector {
    options: Vec<ScopeOption>,
    selected: usize,
}

impl ScopeSelector {
    fn new(options: Vec<ScopeOption>) -> Self {
        Self {
            options,
            selected: 0,
        }
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

    fn selected_scope(&self) -> Option<ReviewScope> {
        self.options
            .get(self.selected)
            .map(|option| option.scope.clone())
    }
}

enum ScopeSelection {
    Quit,
    Selected(ReviewScope),
}

struct LaunchSelection {
    scope: ReviewScope,
    review: CollectedReview,
    scope_label: String,
}

#[derive(Debug, Clone)]
struct CliReviewRequest {
    review_scope: CliSemanticReviewScope,
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
                verdict.as_str()
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

struct AppState {
    review_scope: ReviewScope,
    navigator: ReviewNavigator,
    review_order: ReviewOrder,
    total_blocks: usize,
    initial_remaining_blocks: usize,
    remaining_blocks: usize,
    reviewable_nodes: HashSet<TreeNodeId>,
    session_recap: SessionRecap,
    scope_label: String,
    input_mode: InputMode,
    input_buffer: String,
    confirm_batch: bool,
    repo_name: String,
    workdir_prefix: Option<String>,
    file_cache: HashMap<PathBuf, Arc<[String]>>,
    root_cursor: Option<TreeNodeId>,
    scroll_offset: u16,
    content_height: u16,
    viewport_height: u16,
    view_mode: ViewMode,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    keybinds: TuiKeybindsConfig,
    file_diff_cache: HashMap<PathBuf, vcs::FileDiff>,
    content_frame_cache: HashMap<ContentFrameCacheKey, ContentFrameCacheEntry>,
    highlighted_line_cache: HashMap<HighlightLineCacheKey, Vec<HighlightToken>>,
    speed_read: SpeedReadController,
}

struct ReviewStateBuildOptions {
    confirm_batch: bool,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    keybinds: TuiKeybindsConfig,
    scope_label: String,
    workdir_prefix: Option<String>,
    speed_read_config: TuiSpeedReadConfig,
    speed_read_config_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectionKeyAction {
    Up,
    Down,
    Left,
    Right,
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
struct ContentFrameCacheKey {
    node_id: TreeNodeId,
    variant: ContentFrameCacheVariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ContentFrameCacheVariant {
    File,
    BlockDiff { focus_mode: vcs::BlockDiffFocusMode },
    BlockSource { code_height: u16 },
}

#[derive(Clone)]
struct ContentFrameCacheEntry {
    lines: Vec<Line<'static>>,
    total_lines: usize,
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
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let config = load_config()?;
    let run_result = (|| {
        let scan_options = config.scan.resolve_options()?;
        let launch = if let Some(request) = cli_review_request(all, target, only, exclude)? {
            let filters = config.review.resolve_filters(only, exclude);
            let review = {
                let query = resolve_review_request(
                    request.review_scope.review_request(),
                    filters,
                    scan_options,
                )?;
                collect_review(&query)?
            };
            LaunchSelection {
                scope: request.review_scope.tui_scope(),
                review,
                scope_label: request.review_scope.label(),
            }
        } else {
            let scope_options = load_scope_options()?;
            let selection = run_scope_selector(
                &mut terminal,
                ScopeSelector::new(scope_options),
                &config.tui.keybinds,
            )?;
            match selection {
                ScopeSelection::Quit => return Ok(()),
                ScopeSelection::Selected(scope) => {
                    let filters = config.review.resolve_filters(&[], &[]);
                    let review = load_review_state(&scope, &filters, &scan_options)?;
                    LaunchSelection {
                        scope_label: scope.label(),
                        scope,
                        review,
                    }
                }
            }
        };

        let state = build_review_state(
            context,
            launch.review,
            launch.scope,
            ReviewStateBuildOptions {
                confirm_batch: config.tui.confirm_batch,
                block_diff_focus_mode: block_diff_focus_mode_from_config(&config.tui),
                keybinds: config.tui.keybinds,
                scope_label: launch.scope_label,
                workdir_prefix: workdir_prefix_from_git_root(),
                speed_read_config: config.tui.speed_read.clone(),
                speed_read_config_path: speed_read_config_path_for_repo_root(),
            },
        )?;
        run_app(context, &mut terminal, state)
    })();
    restore_terminal(&mut terminal)?;
    run_result
}

fn cli_review_request(
    all: bool,
    target: &[ReviewTarget],
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<Option<CliReviewRequest>> {
    let has_cli_overrides = all || !target.is_empty() || !only.is_empty() || !exclude.is_empty();
    if !has_cli_overrides {
        return Ok(None);
    }

    let review_scope = CliSemanticReviewScope::from_cli(all, target)?;
    Ok(Some(CliReviewRequest { review_scope }))
}

fn build_review_state(
    context: &TrueflowContext,
    review: CollectedReview,
    review_scope: ReviewScope,
    options: ReviewStateBuildOptions,
) -> Result<AppState> {
    let reviewable_nodes: HashSet<TreeNodeId> = review
        .unreviewed_block_nodes
        .iter()
        .copied()
        .filter(|&id| matches!(review.tree.node(id).kind, TreeNodeKind::Block))
        .collect();
    let remaining_blocks = reviewable_nodes.len();

    let root_children = review.tree.node(review.tree.root()).children.clone();
    let root_cursor = root_children.first().copied();

    let review_order = ReviewOrder::from_tree(&review.tree, &review.unreviewed_block_nodes);
    let navigator = ReviewNavigator::new(review.tree, review.unreviewed_block_nodes)?;

    Ok(AppState {
        review_scope,
        navigator,
        review_order,
        total_blocks: review.summary.total_blocks,
        initial_remaining_blocks: remaining_blocks,
        remaining_blocks,
        reviewable_nodes,
        session_recap: SessionRecap::default(),
        scope_label: options.scope_label,
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        confirm_batch: options.confirm_batch,
        repo_name: detect_repo_name(context),
        workdir_prefix: options.workdir_prefix,
        file_cache: HashMap::new(),
        root_cursor,
        scroll_offset: 0,
        content_height: 0,
        viewport_height: 0,
        view_mode: ViewMode::Diff,
        block_diff_focus_mode: options.block_diff_focus_mode,
        keybinds: options.keybinds,
        file_diff_cache: HashMap::new(),
        content_frame_cache: HashMap::new(),
        highlighted_line_cache: HashMap::new(),
        speed_read: SpeedReadController::new(
            options.speed_read_config,
            options.speed_read_config_path,
        ),
    })
}

fn block_diff_focus_mode_from_config(config: &TuiConfig) -> vcs::BlockDiffFocusMode {
    match config.diff_focus_mode {
        TuiDiffFocusMode::WholeBlock => vcs::BlockDiffFocusMode::WholeBlock,
        TuiDiffFocusMode::ChangedWithContext => vcs::BlockDiffFocusMode::ChangedWithContext {
            context_lines: config.diff_focus_context_lines,
        },
    }
}

fn direction_key_action_for_key_code(
    keybinds: &TuiKeybindsConfig,
    key_code: KeyCode,
) -> Option<DirectionKeyAction> {
    match key_code {
        KeyCode::Up => Some(DirectionKeyAction::Up),
        KeyCode::Down => Some(DirectionKeyAction::Down),
        KeyCode::Left => Some(DirectionKeyAction::Left),
        KeyCode::Right => Some(DirectionKeyAction::Right),
        KeyCode::Char(ch) if ch == keybinds.up => Some(DirectionKeyAction::Up),
        KeyCode::Char(ch) if ch == keybinds.down => Some(DirectionKeyAction::Down),
        KeyCode::Char(ch) if ch == keybinds.left => Some(DirectionKeyAction::Left),
        KeyCode::Char(ch) if ch == keybinds.right => Some(DirectionKeyAction::Right),
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

fn filter_commits_for_prefix<F>(
    commits: Vec<vcs::CommitInfo>,
    workdir_prefix: Option<&str>,
    mut changed_paths_for_revision: F,
) -> Vec<vcs::CommitInfo>
where
    F: FnMut(&str) -> Result<HashSet<RepoPath>>,
{
    let Some(prefix) = workdir_prefix
        .map(normalize_path_str)
        .filter(|p| !p.is_empty())
    else {
        return commits;
    };

    commits
        .into_iter()
        .filter(|commit| {
            match changed_paths_for_revision(&commit.id) {
                Ok(paths) => paths
                    .iter()
                    .any(|path| path_matches_workdir_prefix(path.as_str(), &prefix)),
                // If we can't resolve changed paths, keep the option instead of hiding it.
                Err(_) => true,
            }
        })
        .collect()
}

fn path_matches_workdir_prefix(path: &str, prefix: &str) -> bool {
    path_utils::path_matches_workdir_prefix(path, prefix)
}

fn workdir_prefix_from_git_root() -> Option<String> {
    let repo_root = vcs::git_root_from_workdir().ok().flatten()?;
    path_utils::current_workdir_prefix_for_repo_root(&repo_root)
}

fn speed_read_config_path_for_repo_root() -> PathBuf {
    match vcs::git_root_from_workdir() {
        Ok(Some(root)) => root.join("trueflow.toml"),
        Ok(None) | Err(_) => PathBuf::from("trueflow.toml"),
    }
}

fn normalize_path_str(path: &str) -> String {
    path_utils::normalize_path_str(path)
}

fn repo_relative_path_for_diff(path: &str, workdir_prefix: Option<&str>) -> String {
    path_utils::repo_relative_path_for_diff(path, workdir_prefix)
}

fn setup_terminal() -> Result<Terminal<ratatui::backend::CrosstermBackend<Stdout>>> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_scope_selector(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    mut selector: ScopeSelector,
    keybinds: &TuiKeybindsConfig,
) -> Result<ScopeSelection> {
    let mut needs_render = true;

    loop {
        if needs_render {
            terminal.draw(|f| render_scope_selector(f, &selector, keybinds))?;
            needs_render = false;
        }

        let event = event::read()?;
        if should_rerender_on_event(&event) {
            needs_render = true;
            continue;
        }

        let Some(key_event) = key_event_for_press_event(&event) else {
            continue;
        };
        let key_code = key_event.code;

        if let Some(action) = direction_key_action_for_key_code(keybinds, key_code) {
            match action {
                DirectionKeyAction::Up => {
                    selector.move_prev();
                    needs_render = true;
                    continue;
                }
                DirectionKeyAction::Down => {
                    selector.move_next();
                    needs_render = true;
                    continue;
                }
                DirectionKeyAction::Left | DirectionKeyAction::Right => {}
            }
        }

        match key_code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(ScopeSelection::Quit),
            KeyCode::Enter => {
                if let Some(scope) = selector.selected_scope() {
                    return Ok(ScopeSelection::Selected(scope));
                }
            }
            _ => {}
        }
    }
}

fn run_app(
    context: &TrueflowContext,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    mut state: AppState,
) -> Result<()> {
    let mut needs_render = true;

    loop {
        if needs_render {
            terminal.draw(|f| ui(f, &mut state))?;
            needs_render = false;
        }

        let event = read_next_app_event(&state)?;
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

        let Some(key_event) = key_event_for_press_event(&event) else {
            continue;
        };
        let key_code = key_event.code;

        match &state.input_mode {
            InputMode::Normal => {
                if is_recap_mode(&state) {
                    if recap_key_should_exit(key_code) {
                        flush_pending_speed_read_defaults(&mut state)?;
                        return Ok(());
                    }
                    continue;
                }

                if handle_speed_read_key_binding(&mut state, key_code) {
                    needs_render = true;
                    continue;
                }

                if let Some(action) = direction_key_action_for_key_code(&state.keybinds, key_code) {
                    match action {
                        DirectionKeyAction::Up => handle_ascend(&mut state),
                        DirectionKeyAction::Down => handle_descend(&mut state),
                        DirectionKeyAction::Left => handle_prev(&mut state),
                        DirectionKeyAction::Right => handle_next(&mut state),
                    }
                    needs_render = true;
                    continue;
                }

                match key_code {
                    KeyCode::Char('q') => {
                        flush_pending_speed_read_defaults(&mut state)?;
                        return Ok(());
                    }
                    KeyCode::Char('n') => {
                        handle_next(&mut state);
                        needs_render = true;
                    }
                    KeyCode::Char('b') => {
                        handle_prev(&mut state);
                        needs_render = true;
                    }
                    KeyCode::Char('a') => {
                        handle_action(terminal, context, &mut state, Verdict::Approved)?;
                        needs_render = true;
                    }
                    KeyCode::Char('x') => {
                        handle_action(terminal, context, &mut state, Verdict::Rejected)?;
                        needs_render = true;
                    }
                    KeyCode::Char('c') => {
                        handle_comment_action(&mut state)?;
                        needs_render = true;
                    }
                    KeyCode::Char(' ')
                        if state.navigator.current_id() != state.navigator.tree.root() =>
                    {
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
                    KeyCode::Char('g') => {
                        state.navigator.jump_root();
                        clear_speed_read_if_not_on_current_node(&mut state);
                        needs_render = true;
                    }
                    KeyCode::Char('d') => {
                        state.view_mode = match state.view_mode {
                            ViewMode::Source => ViewMode::Diff,
                            ViewMode::Diff => ViewMode::Source,
                        };
                        // Reset scroll when switching views because content height changes
                        state.scroll_offset = 0;
                        needs_render = true;
                    }
                    KeyCode::Enter | KeyCode::Char(' ')
                        if state.navigator.current_id() == state.navigator.tree.root() =>
                    {
                        if let Some(first) = state.review_order.first_reviewable_block() {
                            state.navigator.set_current(first);
                            clear_speed_read_if_not_on_current_node(&mut state);
                        }
                        needs_render = true;
                    }
                    KeyCode::Char('r') => {
                        toggle_speed_read_mode(&mut state);
                        needs_render = true;
                    }
                    _ => {}
                }
            }
            InputMode::Editing { .. } => match editing_key_action_for_event(&key_event) {
                EditingKeyAction::Submit => {
                    handle_editing_submit(terminal, context, &mut state)?;
                    needs_render = true;
                }
                EditingKeyAction::InsertNewline => {
                    state.input_buffer.push('\n');
                    needs_render = true;
                }
                EditingKeyAction::Cancel => {
                    handle_editing_cancel(&mut state);
                    needs_render = true;
                }
                EditingKeyAction::Backspace => {
                    state.input_buffer.pop();
                    needs_render = true;
                }
                EditingKeyAction::InsertChar(c) => {
                    state.input_buffer.push(c);
                    needs_render = true;
                }
                EditingKeyAction::Ignore => {}
            },
            InputMode::ConfirmBatch { .. } => match key_code {
                KeyCode::Enter => {
                    handle_confirm_batch(terminal, context, &mut state)?;
                    needs_render = true;
                }
                KeyCode::Esc => {
                    handle_confirm_cancel(&mut state);
                    needs_render = true;
                }
                _ => {}
            },
        }
    }
}

fn read_next_app_event(state: &AppState) -> Result<Option<Event>> {
    let Some(deadline) = next_app_deadline(state) else {
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

fn next_app_deadline(state: &AppState) -> Option<Instant> {
    state.speed_read.next_deadline(state.navigator.current_id())
}

fn clear_speed_read_if_not_on_current_node(state: &mut AppState) {
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
    InsertChar(char),
    Ignore,
}

fn editing_key_action_for_event(key_event: &KeyEvent) -> EditingKeyAction {
    match key_event.code {
        KeyCode::Enter if key_event.modifiers.contains(KeyModifiers::SHIFT) => {
            EditingKeyAction::InsertNewline
        }
        KeyCode::Enter => EditingKeyAction::Submit,
        KeyCode::Esc => EditingKeyAction::Cancel,
        KeyCode::Backspace => EditingKeyAction::Backspace,
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

fn key_event_for_press_event(event: &Event) -> Option<KeyEvent> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(*key),
        _ => None,
    }
}

#[cfg(test)]
fn key_code_for_press_event(event: &Event) -> Option<KeyCode> {
    key_event_for_press_event(event).map(|key| key.code)
}

fn should_rerender_on_event(event: &Event) -> bool {
    matches!(event, Event::Resize(_, _))
}

fn is_recap_mode(state: &AppState) -> bool {
    matches!(state.input_mode, InputMode::Normal) && state.remaining_blocks == 0
}

fn recap_key_should_exit(key_code: KeyCode) -> bool {
    matches!(
        key_code,
        KeyCode::Char('q') | KeyCode::Char('d') | KeyCode::Esc
    )
}

// ... helper functions for actions ...

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn handle_ascend(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        return;
    }
    state.navigator.ascend();
    state.scroll_offset = 0;
    clear_speed_read_if_not_on_current_node(state);
}

fn handle_descend(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        let root = state.navigator.tree.root();
        state.root_cursor = state
            .root_cursor
            .filter(|id| state.navigator.is_visible(*id))
            .or_else(|| state.navigator.first_visible_child(root));

        if let Some(target) = state.root_cursor {
            state.navigator.set_current(target);
            state.scroll_offset = 0;
            clear_speed_read_if_not_on_current_node(state);
        }
    } else {
        state.navigator.descend();
        state.scroll_offset = 0;
        clear_speed_read_if_not_on_current_node(state);
    }
}

fn handle_prev(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, -1);
    } else {
        state.navigator.move_prev();
        state.scroll_offset = 0;
        clear_speed_read_if_not_on_current_node(state);
    }
}

fn handle_next(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, 1);
    } else {
        state.navigator.move_next();
        state.scroll_offset = 0;
        clear_speed_read_if_not_on_current_node(state);
    }
}

fn handle_scroll_page_up(state: &mut AppState) {
    let scroll_amount = state.viewport_height.saturating_sub(1);
    state.scroll_offset = state.scroll_offset.saturating_sub(scroll_amount);
}

fn handle_scroll_page_down(state: &mut AppState) {
    let scroll_amount = state.viewport_height.saturating_sub(1);
    let max_scroll = state.content_height.saturating_sub(state.viewport_height);
    state.scroll_offset = (state.scroll_offset + scroll_amount).min(max_scroll);
}

fn move_root_cursor(state: &mut AppState, offset: isize) {
    let root = state.navigator.tree.root();
    let root_children: Vec<TreeNodeId> = state
        .navigator
        .tree
        .node(root)
        .children
        .iter()
        .copied()
        .filter(|child| state.navigator.is_visible(*child))
        .collect();

    if root_children.is_empty() {
        state.root_cursor = None;
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
}

fn handle_action(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    context: &TrueflowContext,
    state: &mut AppState,
    verdict: Verdict,
) -> Result<()> {
    let action =
        PendingAction::from_node(&state.navigator.tree, state.navigator.current_id(), verdict);

    if matches!(action, PendingAction::Batch { .. }) && state.confirm_batch {
        let count = state
            .navigator
            .count_visible_descendant_blocks(state.navigator.current_id());
        state.input_mode = InputMode::ConfirmBatch { action, count };
    } else {
        execute_action(terminal, context, state, action)?;
    }
    Ok(())
}

fn handle_comment_action(state: &mut AppState) -> Result<()> {
    let action = PendingAction::from_node(
        &state.navigator.tree,
        state.navigator.current_id(),
        Verdict::Comment,
    );
    state.input_mode = InputMode::Editing { action };
    state.input_buffer.clear();
    Ok(())
}

fn handle_editing_submit(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    context: &TrueflowContext,
    state: &mut AppState,
) -> Result<()> {
    let Some(submit) = editing_submit_decision(&state.input_mode, &state.input_buffer) else {
        return Ok(());
    };

    let action = match submit {
        EditingSubmitDecision::Empty => {
            state.input_buffer.clear();
            return Ok(());
        }
        EditingSubmitDecision::Ready(action) => action,
    };

    if matches!(action, PendingAction::Batch { .. }) && state.confirm_batch {
        let count = state
            .navigator
            .count_visible_descendant_blocks(match &action {
                PendingAction::Single { node_id, .. } | PendingAction::Batch { node_id, .. } => {
                    *node_id
                }
            });
        state.input_buffer.clear();
        state.input_mode = InputMode::ConfirmBatch { action, count };
    } else {
        state.input_mode = InputMode::Normal;
        state.input_buffer.clear();
        execute_action(terminal, context, state, action)?;
    }
    Ok(())
}

fn handle_editing_cancel(state: &mut AppState) {
    if state.input_buffer.is_empty() {
        state.input_mode = InputMode::Normal;
    } else {
        state.input_buffer.clear();
    }
}

fn handle_confirm_batch(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    context: &TrueflowContext,
    state: &mut AppState,
) -> Result<()> {
    let action = match &state.input_mode {
        InputMode::ConfirmBatch { action, .. } => action.clone(),
        _ => return Ok(()),
    };
    state.input_mode = InputMode::Normal;
    execute_action(terminal, context, state, action)
}

fn handle_confirm_cancel(state: &mut AppState) {
    state.input_mode = InputMode::Normal;
}

fn execute_action(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    context: &TrueflowContext,
    state: &mut AppState,
    action: PendingAction,
) -> Result<()> {
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

    let next_id = compute_next_review_target(state, node_id);

    with_terminal_suspend(terminal, || {
        let node = state.navigator.tree.node(node_id);
        let (fingerprint, target_kind) = fingerprint_and_target_kind_for_node(node);

        // For root/dir, path might be empty or a dir path.
        // For file/block, it's the file path.
        let path_hint = if node.path.is_root() {
            None
        } else {
            Some(node.path.to_string())
        };

        let line_hint = node
            .block
            .as_ref()
            .map(|block| usize_to_u32_saturating(block.start_line));

        mark::run(
            context,
            mark::MarkParams {
                fingerprint,
                target_kind: Some(target_kind),
                verdict: verdict.clone(),
                check: "review".to_string(),
                note,
                path: path_hint,
                line: line_hint,
            },
        )
    })?;

    let impact = apply_action_locally(state, node_id, &verdict, next_id);
    state.session_recap.record_action(&verdict, impact);
    Ok(())
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

fn with_terminal_suspend<F>(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    action: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    let result = action();
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    terminal.clear()?;
    result
}

fn load_review_state(
    scope: &ReviewScope,
    filters: &BlockFilters,
    scan_options: &crate::scanner::ScanOptions,
) -> Result<CollectedReview> {
    let request = scope.to_review_request()?;
    let query = resolve_review_request(request, filters.clone(), scan_options.clone())?;
    collect_review(&query)
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
        for block_id in block_ids {
            if state.navigator.remove_visible(block_id) && state.reviewable_nodes.remove(&block_id)
            {
                removed_reviewable += 1;
            }
        }
        state.remaining_blocks = state.remaining_blocks.saturating_sub(removed_reviewable);
    }

    state.navigator.prune_visible_to_block_ancestors();

    if let Some(node_id) = next_id {
        state.navigator.set_current(node_id);
        state.scroll_offset = 0;
    } else {
        state.navigator.jump_root();
        state.scroll_offset = 0;
    }
    clear_speed_read_if_not_on_current_node(state);

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

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Select review scope",
        Style::default()
            .fg(palette.fg)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (idx, option) in selector.options.iter().enumerate() {
        let prefix = if idx == selector.selected { "> " } else { "  " };
        let style = if idx == selector.selected {
            Style::default()
                .fg(palette.fg)
                .bg(palette.meta_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.dim).bg(palette.bg)
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", option.label),
            style,
        )));
    }

    let block = UiBlock::default()
        .title(" Review scope ")
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));

    let popup_area = centered_rect(area, 70, 60);
    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    let layout = scope_selector_content_layout(inner_area);

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
        render_recap_footer(frame, footer_area, &palette);
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

    let focus_layout = compute_focus_layout(area, usize_to_u16_saturating(header_lines.len()));
    let actions_lines = build_action_lines(focus_layout.actions.width, &state.keybinds, palette);
    let node_snapshot = ContentNodeSnapshot::from_node(node);
    let (content_lines, total_lines) = if let Some(speed_lines) =
        build_speed_read_lines(state, node_snapshot.id, palette, focus_layout.code.width)
    {
        speed_lines
    } else {
        build_content_lines_with_frame_cache(
            state,
            &node_snapshot,
            palette,
            focus_layout.code.height,
        )
    };

    state.content_height = usize_to_u16_saturating(total_lines);
    state.viewport_height = focus_layout.code.height;
    state.scroll_offset = state
        .scroll_offset
        .min(state.content_height.saturating_sub(state.viewport_height));

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

    frame.render_widget(
        Paragraph::new(content_lines)
            .block(UiBlock::default().style(Style::default().bg(palette.code_bg)))
            .scroll((state.scroll_offset, 0))
            .wrap(Wrap { trim: false }),
        focus_layout.code,
    );

    if state.content_height > state.viewport_height {
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
    node_kind: TreeNodeKind,
    view_mode: ViewMode,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    code_height: u16,
) -> Option<ContentFrameCacheKey> {
    if !is_content_kind_cacheable(node_kind) {
        return None;
    }

    let variant = match node_kind {
        TreeNodeKind::File => ContentFrameCacheVariant::File,
        TreeNodeKind::Block => match view_mode {
            ViewMode::Diff => ContentFrameCacheVariant::BlockDiff {
                focus_mode: block_diff_focus_mode,
            },
            ViewMode::Source => ContentFrameCacheVariant::BlockSource { code_height },
        },
        TreeNodeKind::Directory | TreeNodeKind::Root => return None,
    };

    Some(ContentFrameCacheKey { node_id, variant })
}

fn is_content_kind_cacheable(kind: TreeNodeKind) -> bool {
    matches!(kind, TreeNodeKind::Block | TreeNodeKind::File)
}

fn build_content_lines_with_frame_cache(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    code_height: u16,
) -> (Vec<Line<'static>>, usize) {
    let key = content_frame_cache_key(
        node.id,
        node.kind,
        state.view_mode,
        state.block_diff_focus_mode,
        code_height,
    );

    if let Some(key) = key {
        if let Some(cached) = state.content_frame_cache.get(&key) {
            return (cached.lines.clone(), cached.total_lines);
        }

        let (lines, total_lines) = build_content_lines(state, node, palette, code_height);
        state.content_frame_cache.insert(
            key,
            ContentFrameCacheEntry {
                lines: lines.clone(),
                total_lines,
            },
        );
        return (lines, total_lines);
    }

    build_content_lines(state, node, palette, code_height)
}

fn build_header_lines(
    node: &crate::tree::TreeNode,
    state: &AppState,
    palette: &UiPalette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let header_text = match node.kind {
        TreeNodeKind::Root => format!("Repository (Root node) @ {}", state.repo_name),
        TreeNodeKind::Directory => format!("Directory @ {}/", node.name),
        TreeNodeKind::File => format!("File @ {}", node.name),
        TreeNodeKind::Block => {
            if let Some(block) = &node.block {
                let start = block.start_line + 1;
                let end = block.end_line.max(start);
                let path = if node.path.is_root() {
                    "unknown"
                } else {
                    node.path.as_str()
                };
                format!("{} @ {}:{}-{}", block.kind.as_str(), path, start, end)
            } else {
                format!("Block @ {}", node.name)
            }
        }
    };

    lines.push(format_header_row(&header_text, palette, true));

    if matches!(node.kind, TreeNodeKind::Block)
        && let Some(breadcrumb) = review_metadata::block_breadcrumb(&state.navigator.tree, node.id)
    {
        lines.push(format_header_row(&breadcrumb, palette, false));
    }

    if !matches!(node.kind, TreeNodeKind::Root)
        && !node.path.is_root()
        && !matches!(node.kind, TreeNodeKind::Block)
    {
        lines.push(format_header_row(node.path.as_str(), palette, false));
    }

    let node_hash = node.hash.as_str();
    if !matches!(node.kind, TreeNodeKind::Root) && !node_hash.is_empty() {
        lines.push(format_header_row(
            &format!("Hash: {}", &node_hash[..node_hash.len().min(12)]),
            palette,
            false,
        ));
    }

    if lines.is_empty() {
        lines.push(format_header_row("(No details)", palette, true));
    }

    lines
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
        "[Enter] select  [{}/{}] move  [q] quit",
        keybinds.up, keybinds.down
    )
}

fn build_action_lines(
    width: u16,
    keybinds: &TuiKeybindsConfig,
    palette: &UiPalette,
) -> Vec<Line<'static>> {
    let top_left = "[a]pprove [c]omment [x]reject";
    let top_right = "[g]root [q]uit";
    let top_spacing = top_line_spacing(width, top_left, top_right);

    let top_line = Line::from(vec![
        Span::styled(top_left.to_string(), Style::default().fg(palette.dim)),
        Span::styled(top_spacing, Style::default().bg(palette.bg)),
        Span::styled(top_right.to_string(), Style::default().fg(palette.dim)),
    ]);

    let pyramid_style = Style::default()
        .fg(palette.dim)
        .add_modifier(Modifier::BOLD);

    let nav_top_left = "[d]toggle diff/source";
    let nav_top_right = format!("[{}]ascend", keybinds.up);
    let nav_top_spacing = top_line_spacing(width, nav_top_left, &nav_top_right);

    let pyramid_lines = vec![
        Line::from(vec![
            Span::styled(nav_top_left.to_string(), pyramid_style),
            Span::styled(nav_top_spacing, Style::default().bg(palette.bg)),
            Span::styled(nav_top_right, pyramid_style),
        ]),
        Line::from(Span::styled(
            format!(
                "[{}]prev            [{}]next",
                keybinds.left, keybinds.right
            ),
            pyramid_style,
        )),
        Line::from(Span::styled(
            format!("  [{}]descend", keybinds.down),
            pyramid_style,
        )),
    ];

    let mut lines = Vec::with_capacity(1 + pyramid_lines.len());
    lines.push(top_line);
    lines.extend(pyramid_lines);
    lines
}

fn top_line_spacing(width: u16, left: &str, right: &str) -> String {
    let total = left.len() + right.len();
    if width as usize <= total {
        return " ".to_string();
    }
    " ".repeat(width as usize - total)
}

fn render_footer(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let ratio = if state.total_blocks > 0 {
        (state.total_blocks - state.remaining_blocks) as f64 / state.total_blocks as f64
    } else {
        1.0
    };

    let label = format!(
        " {}/{} reviewed ",
        state.total_blocks - state.remaining_blocks,
        state.total_blocks
    );

    let gauge = Gauge::default()
        .block(UiBlock::default().borders(ratatui::widgets::Borders::NONE))
        .gauge_style(Style::default().fg(palette.add).bg(palette.bg))
        .ratio(ratio)
        .label(Span::styled(label, Style::default().fg(palette.fg)));

    frame.render_widget(gauge, area);
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

fn render_recap_footer(frame: &mut Frame, area: Rect, palette: &UiPalette) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Press [d] done or [q] quit",
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
    lines.push(format!(
        "Rejections: {} blocks",
        state.session_recap.rejected_blocks
    ));
    lines.push(format!("Comments: {}", state.session_recap.comments));
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
) -> (Vec<Line<'static>>, usize) {
    match node.kind {
        TreeNodeKind::Block => build_block_lines(state, node, palette, code_height),
        TreeNodeKind::File => build_file_lines(state, node, palette, code_height),
        TreeNodeKind::Directory => build_directory_lines(state, node, palette, code_height),
        TreeNodeKind::Root => build_root_lines(state, palette, code_height),
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

    let contents = std::fs::read_to_string(path.as_str()).ok()?;
    let lines: Arc<[String]> = contents
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .into();
    state.file_cache.insert(path_buf, Arc::clone(&lines));
    Some(lines)
}

fn build_block_lines(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    _code_height: u16,
) -> (Vec<Line<'static>>, usize) {
    let Some(block) = &node.block else {
        return (
            vec![Line::from(Span::styled(
                "(No content)",
                Style::default().fg(palette.dim).bg(palette.code_bg),
            ))],
            1,
        );
    };

    if state.view_mode == ViewMode::Diff {
        return build_block_diff_lines(state, node, block, palette);
    }

    let language = node.language;
    let file_lines = match load_file_lines(state, &node.path) {
        Some(lines) => lines,
        None => {
            let block_line_count = block.content.lines().count();
            let mut lines = Vec::with_capacity(block_line_count);
            for line in block.content.lines() {
                lines.push(format_code_line(
                    &mut state.highlighted_line_cache,
                    line,
                    palette,
                    language.as_ref(),
                ));
            }
            let len = lines.len();
            return (lines, len);
        }
    };

    let start_line = block.start_line.min(file_lines.len());
    let end_line = block.end_line.min(file_lines.len());
    if start_line >= end_line {
        let block_line_count = block.content.lines().count();
        let mut lines = Vec::with_capacity(block_line_count);
        for line in block.content.lines() {
            lines.push(format_code_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        }
        let len = lines.len();
        return (lines, len);
    }

    // With scrolling, keep the full file context and let the viewport clip.
    let mut lines = Vec::with_capacity(file_lines.len());
    for (index, line) in file_lines.iter().enumerate() {
        if (start_line..end_line).contains(&index) {
            lines.push(format_code_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        } else {
            lines.push(format_context_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        }
    }

    let len = lines.len();
    (lines, len)
}

fn build_block_diff_lines(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    block: &crate::block::Block,
    palette: &UiPalette,
) -> (Vec<Line<'static>>, usize) {
    if node.path.is_root() {
        return (
            vec![Line::from(Span::styled(
                "(No path for diff)",
                Style::default().fg(palette.dim).bg(palette.code_bg),
            ))],
            1,
        );
    }

    let diff_path =
        repo_relative_path_for_diff(node.path.as_str(), state.workdir_prefix.as_deref());
    let path = PathBuf::from(&diff_path);
    let file_diff = ensure_cached_file_diff(&mut state.file_diff_cache, &path, || {
        let query = state
            .review_scope
            .diff_selection()
            .query_for_path(&diff_path);
        let repo = vcs::repo_from_workdir()?;
        match query {
            DiffQuery::MainDiff { path } => vcs::diff_for_file(&repo, &RepoPath::new(path)?),
            DiffQuery::Revision { revision, path } => {
                vcs::diff_for_file_in_revision(&repo, &revision, &RepoPath::new(path)?)
            }
            DiffQuery::RevisionRange { start, end, path } => {
                vcs::diff_for_file_in_range(&repo, &start, &end, &RepoPath::new(path)?)
            }
        }
    });
    let diff_analysis =
        vcs::analyze_block_diff_for_file(block, file_diff, state.block_diff_focus_mode);

    let Some(view) = diff_analysis.view else {
        let message = match diff_analysis.change_kind {
            vcs::BlockDiffChangeKind::NoTextChanges => "(No diff changes in this block)",
            vcs::BlockDiffChangeKind::OnlyNonreviewableChurn => {
                "(Only nonreviewable formatting churn in this block)"
            }
            vcs::BlockDiffChangeKind::ReviewableChanges => {
                "(Diff analysis did not produce view output)"
            }
            vcs::BlockDiffChangeKind::DiffUnavailable(vcs::FileDiffUnavailableReason::Binary) => {
                "(Diff unavailable for binary file)"
            }
            vcs::BlockDiffChangeKind::DiffUnavailable(vcs::FileDiffUnavailableReason::External) => {
                "(Diff unavailable from external diff command)"
            }
        };
        return (
            vec![Line::from(Span::styled(
                message,
                Style::default().fg(palette.dim).bg(palette.code_bg),
            ))],
            1,
        );
    };

    let formatted = view
        .lines
        .iter()
        .map(|line| {
            let style = style_for_diff_overlay_line(line, palette);
            let text = format_diff_overlay_row(line);
            Line::from(Span::styled(text, style))
        })
        .collect::<Vec<_>>();

    let len = formatted.len();
    (formatted, len)
}

fn style_for_diff_overlay_line(line: &vcs::DiffLine, palette: &UiPalette) -> Style {
    match line.kind {
        vcs::DiffLineKind::Added => Style::default()
            .fg(palette.add)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD),
        vcs::DiffLineKind::Removed => Style::default()
            .fg(palette.del)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD),
        vcs::DiffLineKind::Context => {
            let style = Style::default().fg(palette.dim).bg(palette.code_bg);
            if line.is_focus {
                style
            } else {
                style.add_modifier(Modifier::DIM)
            }
        }
    }
}

fn format_diff_overlay_row(line: &vcs::DiffLine) -> String {
    let old_col = format_diff_line_number(line.old_line);
    let new_col = format_diff_line_number(line.new_line);
    let marker = match line.kind {
        vcs::DiffLineKind::Context => ' ',
        vcs::DiffLineKind::Added => '+',
        vcs::DiffLineKind::Removed => '-',
    };
    format!("{old_col} {new_col} {marker} {}", line.text)
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

fn build_file_lines(
    state: &mut AppState,
    node: &ContentNodeSnapshot,
    palette: &UiPalette,
    _code_height: u16,
) -> (Vec<Line<'static>>, usize) {
    let language = node.language;
    let Some(file_lines) = load_file_lines(state, &node.path) else {
        return (
            vec![Line::from(Span::styled(
                "(File missing)",
                Style::default().fg(palette.context).bg(palette.code_bg),
            ))],
            1,
        );
    };

    // With scrolling enabled, we return all lines and let the viewport clip them.
    let mut lines = Vec::with_capacity(file_lines.len());
    for line in file_lines.iter() {
        lines.push(format_code_line(
            &mut state.highlighted_line_cache,
            line,
            palette,
            language.as_ref(),
        ));
    }

    let len = lines.len();
    (lines, len)
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
    let root = state.navigator.tree.root();
    let root_children: Vec<TreeNodeId> = state
        .navigator
        .tree
        .node(root)
        .children
        .iter()
        .copied()
        .filter(|child| state.navigator.is_visible(*child))
        .collect();

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
    let (overlay_kind, title, hints, content) = match &state.input_mode {
        InputMode::Editing { .. } => {
            let content = state.input_buffer.clone();
            let input_lines = editing_input_lines(&content, input_overlay_width(area.width));
            (
                InputOverlayKind::Editing { input_lines },
                " Comment ",
                editing_overlay_hint(&content),
                content,
            )
        }
        InputMode::ConfirmBatch { count, action } => (
            InputOverlayKind::ConfirmBatch,
            " Batch Action ",
            "Enter to confirm • Esc to cancel",
            format!(
                "This will apply '{}' to {} unreviewed descendant block(s).",
                action.verdict_label(),
                count
            ),
        ),
        InputMode::Normal => return,
    };
    let popup_area = input_overlay_rect(area, overlay_kind);
    frame.render_widget(ratatui::widgets::Clear, popup_area);

    let block = UiBlock::default()
        .title(title)
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));

    let lines = input_overlay_lines(&content, hints, palette);

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup_area,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOverlayKind {
    Editing { input_lines: u16 },
    ConfirmBatch,
}

fn input_overlay_width(area_width: u16) -> u16 {
    if area_width >= 20 {
        area_width.min(96)
    } else {
        area_width
    }
}

fn editing_input_lines(content: &str, overlay_width: u16) -> u16 {
    let inner_width = usize::from(overlay_width.saturating_sub(2).max(1));
    let wrapped_lines = content
        .split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(inner_width))
        .sum::<usize>()
        .max(1);
    usize_to_u16_saturating(wrapped_lines)
}

fn input_overlay_rect(area: Rect, kind: InputOverlayKind) -> Rect {
    let preferred_height = match kind {
        InputOverlayKind::Editing { input_lines } => input_lines.saturating_add(4).clamp(5, 12),
        InputOverlayKind::ConfirmBatch => 4,
    };
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

fn input_overlay_lines(content: &str, hints: &str, palette: &UiPalette) -> Vec<Line<'static>> {
    let mut lines = content
        .split('\n')
        .map(|line| Line::from(line.to_string()))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        hints.to_string(),
        Style::default().fg(palette.dim),
    )));
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
    let note = input_buffer.trim().to_string();
    if note.is_empty() {
        return Some(EditingSubmitDecision::Empty);
    }
    Some(EditingSubmitDecision::Ready(action.with_note(note)))
}

fn editing_overlay_hint(content: &str) -> &'static str {
    if content.trim().is_empty() {
        "Comment required • Enter keeps editor open • Esc to cancel"
    } else {
        "Enter to submit • Shift+Enter newline • Esc to cancel"
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
    actions: Rect,
}

const ACTIONS_HEIGHT: u16 = 4;

fn compute_focus_layout(area: Rect, header_lines: u16) -> FocusLayout {
    let code_width = area.width.min(120);
    let desired_code_height = area.height.min(32);
    let padding = u16::try_from((u32::from(area.height) * 5 + 50) / 100).unwrap_or(u16::MAX);

    let available_height = area.height.saturating_sub(padding * 2).max(1);
    let actions_height = ACTIONS_HEIGHT.min(available_height.saturating_sub(2));
    let available_for_header_and_code = available_height.saturating_sub(actions_height);
    let min_header_height = 3.min(available_for_header_and_code);
    let desired_header_height = header_lines.saturating_add(2).max(min_header_height);
    let header_height = desired_header_height.min(available_for_header_and_code.saturating_sub(1));
    let code_height =
        desired_code_height.min(available_for_header_and_code.saturating_sub(header_height));
    let total_height = header_height + code_height + actions_height;

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
        y: content_top + header_height + code_height,
        width: code_width,
        height: actions_height,
    };

    FocusLayout {
        meta,
        code,
        actions,
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
        let layout = compute_focus_layout(area, 3);
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
        let layout = compute_focus_layout(area, 3);
        assert_eq!(layout.code.width, 120);
        assert_eq!(layout.actions.height, 4);
        assert!(layout.code.y > area.y);
    }

    #[test]
    fn focus_layout_reserves_header_border_space() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let layout = compute_focus_layout(area, 1);
        assert_eq!(layout.meta.height, 3);
    }

    #[test]
    fn focus_layout_keeps_actions_within_content_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let layout = compute_focus_layout(area, 3);
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
    use crate::block::{Block, BlockKind};
    use crate::store::ReviewTargetKind;
    use crate::tree::TreeBuilder;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use uuid::Uuid;

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap_or_else(|error| panic!("failed to execute git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_git_stdout(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap_or_else(|error| panic!("failed to execute git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap_or_else(|error| panic!("invalid UTF-8 from git {args:?}: {error}"))
    }

    fn build_test_state(
        review_scope: ReviewScope,
        workdir_prefix: Option<String>,
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
            session_recap: SessionRecap::default(),
            scope_label: String::new(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            confirm_batch: false,
            repo_name: "repo".to_string(),
            workdir_prefix,
            file_cache: HashMap::new(),
            root_cursor: None,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            view_mode: ViewMode::Diff,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache,
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
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
            review_scope: ReviewScope::All,
            navigator,
            review_order,
            total_blocks: 1,
            initial_remaining_blocks: 1,
            remaining_blocks: 1,
            reviewable_nodes: visible,
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            confirm_batch: false,
            repo_name: "repo".to_string(),
            workdir_prefix: None,
            file_cache: HashMap::new(),
            root_cursor: None,
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            view_mode: ViewMode::Diff,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
        };

        (state, block_id)
    }

    fn build_state_with_block_file(
        file_path: &Path,
        file_content: &str,
        block_content: &str,
        block_start_line: usize,
        block_end_line: usize,
    ) -> (AppState, TreeNodeId) {
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
            Language::Rust,
        );
        let block = Block::new(
            block_content.to_string(),
            BlockKind::Function,
            block_start_line,
            block_end_line,
        );
        let block_id = builder.add_block(
            file,
            "function".to_string(),
            repo_path.clone(),
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
            review_scope: ReviewScope::All,
            navigator,
            review_order,
            total_blocks: 1,
            initial_remaining_blocks: 1,
            remaining_blocks: 1,
            reviewable_nodes: visible,
            session_recap: SessionRecap::default(),
            scope_label: "All".to_string(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            confirm_batch: false,
            repo_name: "repo".to_string(),
            workdir_prefix: None,
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
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            view_mode: ViewMode::Source,
            block_diff_focus_mode: vcs::BlockDiffFocusMode::WholeBlock,
            keybinds: TuiKeybindsConfig::default(),
            file_diff_cache: HashMap::new(),
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
            speed_read: SpeedReadController::new(
                TuiSpeedReadConfig::default(),
                PathBuf::from("trueflow.toml"),
            ),
        };

        (state, block_id)
    }

    #[test]
    fn diff_query_uses_main_diff_for_main_scope() {
        let query = ReviewScope::MainDiff
            .diff_selection()
            .query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }
    #[test]
    fn diff_query_uses_revision_for_commit_scope() {
        let query = ReviewScope::Commit {
            id: "abc123".to_string(),
            summary: "test".to_string(),
        }
        .diff_selection()
        .query_for_path("src/lib.rs");
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
        let query = ReviewScope::RevisionRange {
            start: "abc123".to_string(),
            end: "def456".to_string(),
        }
        .diff_selection()
        .query_for_path("src/lib.rs");
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
        let query = ReviewScope::All
            .diff_selection()
            .query_for_path("src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }
    #[test]
    fn path_matches_workdir_prefix_matches_exact_and_descendants() {
        assert!(path_matches_workdir_prefix(
            "trueflow/src/lib.rs",
            "trueflow"
        ));
        assert!(path_matches_workdir_prefix("trueflow", "trueflow"));
        assert!(!path_matches_workdir_prefix("other/src/lib.rs", "trueflow"));
        assert!(!path_matches_workdir_prefix(
            "trueflowish/src/lib.rs",
            "trueflow"
        ));
    }

    #[test]
    fn filter_commits_for_prefix_keeps_only_commits_touching_prefix() {
        let commits = vec![
            vcs::CommitInfo {
                id: "a".to_string(),
                summary: "touches subtree".to_string(),
            },
            vcs::CommitInfo {
                id: "b".to_string(),
                summary: "outside subtree".to_string(),
            },
        ];

        let filtered = filter_commits_for_prefix(commits, Some("trueflow"), |revision| {
            let paths = match revision {
                "a" => HashSet::from([RepoPath::new("trueflow/src/lib.rs").unwrap()]),
                "b" => HashSet::from([RepoPath::new("README.md").unwrap()]),
                _ => HashSet::new(),
            };
            Ok(paths)
        });

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "a");
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
            confirm_batch: true,
            diff_focus_mode: TuiDiffFocusMode::ChangedWithContext,
            diff_focus_context_lines: 7,
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
    fn direction_key_action_uses_hjkl_defaults() {
        let keybinds = crate::config::TuiKeybindsConfig::default();
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('h')),
            Some(DirectionKeyAction::Left)
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('j')),
            Some(DirectionKeyAction::Down)
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('k')),
            Some(DirectionKeyAction::Up)
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('l')),
            Some(DirectionKeyAction::Right)
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('i')),
            None
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Left),
            Some(DirectionKeyAction::Left)
        );
    }

    #[test]
    fn direction_key_action_uses_configured_overrides() {
        let keybinds = crate::config::TuiKeybindsConfig {
            up: 'i',
            down: 'k',
            left: 'j',
            right: 'l',
        };
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('j')),
            Some(DirectionKeyAction::Left)
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('k')),
            Some(DirectionKeyAction::Down)
        );
        assert_eq!(
            direction_key_action_for_key_code(&keybinds, KeyCode::Char('i')),
            Some(DirectionKeyAction::Up)
        );
    }

    #[test]
    fn scope_selector_hint_text_uses_configured_keybinds() {
        let keybinds = crate::config::TuiKeybindsConfig {
            up: 'i',
            down: 'k',
            left: 'j',
            right: 'l',
        };
        assert_eq!(
            scope_selector_hint_text(&keybinds),
            "[Enter] select  [i/k] move  [q] quit"
        );
    }

    #[test]
    fn build_action_lines_uses_configured_keybinds() {
        let keybinds = crate::config::TuiKeybindsConfig {
            up: 'i',
            down: 'k',
            left: 'j',
            right: 'l',
        };
        let palette = UiPalette::default();
        let lines = build_action_lines(80, &keybinds, &palette);

        assert_eq!(lines.len(), 4);
        assert!(lines[1].to_string().contains("[d]toggle diff/source"));
        assert!(lines[1].to_string().contains("[i]ascend"));
        assert_eq!(lines[2].to_string(), "[j]prev            [l]next");
        assert_eq!(lines[3].to_string(), "  [k]descend");
    }

    #[test]
    fn cli_review_request_returns_none_without_cli_overrides() {
        let request = cli_review_request(false, &[], &[], &[])
            .unwrap_or_else(|error| panic!("expected no parse error: {error}"));
        assert!(request.is_none());
    }
    #[test]
    fn cli_review_request_file_target_uses_main_diff_scope() {
        let targets = vec![ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap())];
        let request = cli_review_request(false, &targets, &[], &[])
            .unwrap_or_else(|error| panic!("expected file target request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(
            request.review_scope,
            CliSemanticReviewScope::File(RepoPath::new("src/lib.rs").unwrap())
        );
    }

    #[test]
    fn cli_review_request_revision_range_target_uses_revision_range_scope() {
        let targets = vec![ReviewTarget::RevisionRange(
            crate::commands::review::RevisionRangeSpec::new("abc1234", "def5678").unwrap(),
        )];
        let request = cli_review_request(false, &targets, &[], &[])
            .unwrap_or_else(|error| panic!("expected revision range request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(
            request.review_scope,
            CliSemanticReviewScope::RevisionRange(
                crate::commands::review::RevisionRangeSpec::new("abc1234", "def5678").unwrap(),
            )
        );
    }

    #[test]
    fn cli_review_request_only_exclude_without_targets_is_supported() {
        let only = vec![BlockKind::Function];
        let exclude = vec![BlockKind::Comment];
        let request = cli_review_request(false, &[], &only, &exclude)
            .unwrap_or_else(|error| panic!("expected filter-only request: {error}"));
        let Some(request) = request else {
            panic!("expected cli request");
        };

        assert_eq!(request.review_scope, CliSemanticReviewScope::DirtyWorktree);
    }
    #[test]
    fn cli_review_request_errors_when_all_is_combined_with_targets() {
        let targets = vec![ReviewTarget::DirtyWorktree];
        let request = cli_review_request(true, &targets, &[], &[]);
        assert!(request.is_err());
    }

    #[test]
    fn format_diff_overlay_row_renders_old_new_gutter() {
        let line = vcs::DiffLine {
            kind: vcs::DiffLineKind::Added,
            old_line: None,
            new_line: Some(42),
            text: "let x = 1;".to_string(),
            is_focus: true,
        };
        assert_eq!(
            format_diff_overlay_row(&line),
            "         42 + let x = 1;".to_string()
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
    fn input_overlay_rect_anchors_to_bottom() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let rect = input_overlay_rect(area, InputOverlayKind::Editing { input_lines: 1 });
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
        let rect = input_overlay_rect(area, InputOverlayKind::Editing { input_lines: 4 });
        assert_eq!(rect.height, 8);
    }

    #[test]
    fn editing_input_lines_counts_soft_wrapped_single_line_content() {
        let content = "x".repeat(200);
        let wrapped = editing_input_lines(&content, 30);
        assert!(wrapped > 1, "expected soft wrapping to increase line count");
    }

    #[test]
    fn input_overlay_rect_grows_for_soft_wrapped_editing_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };
        let input_lines = editing_input_lines(&"x".repeat(200), input_overlay_width(area.width));
        let rect = input_overlay_rect(area, InputOverlayKind::Editing { input_lines });
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
        let rect = input_overlay_rect(area, InputOverlayKind::ConfirmBatch);
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
    fn editing_key_action_plain_enter_submits() {
        let key =
            crossterm::event::KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        assert_eq!(editing_key_action_for_event(&key), EditingKeyAction::Submit);
    }

    #[test]
    fn input_overlay_lines_preserve_multiline_content() {
        let palette = UiPalette::default();
        let lines = input_overlay_lines(
            "first line\nsecond line",
            "Enter to submit • Shift+Enter newline • Esc to cancel",
            &palette,
        );

        assert_eq!(lines[0].to_string(), "first line");
        assert_eq!(lines[1].to_string(), "second line");
        assert_eq!(lines[2].to_string(), "");
        assert!(
            lines[3].to_string().contains("Shift+Enter newline"),
            "expected multiline hint line to include Shift+Enter guidance"
        );
    }

    #[test]
    fn editing_submit_decision_returns_empty_for_blank_input() {
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
    fn editing_submit_decision_returns_ready_with_trimmed_note() {
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
        assert_eq!(note.as_deref(), Some("keep this note"));
    }

    #[test]
    fn editing_overlay_hint_requires_comment_for_empty_input() {
        assert_eq!(
            editing_overlay_hint(""),
            "Comment required • Enter keeps editor open • Esc to cancel"
        );
        assert_eq!(
            editing_overlay_hint("note"),
            "Enter to submit • Shift+Enter newline • Esc to cancel"
        );
    }

    #[test]
    fn editing_cancel_clears_non_empty_buffer_before_exit() {
        let mut state = build_test_state(ReviewScope::MainDiff, None, HashMap::new());
        state.input_mode = InputMode::Editing {
            action: PendingAction::Single {
                node_id: TreeBuilder::new().root(),
                verdict: Verdict::Comment,
                note: None,
            },
        };
        state.input_buffer = "note".to_string();

        handle_editing_cancel(&mut state);
        assert!(matches!(state.input_mode, InputMode::Editing { .. }));
        assert!(state.input_buffer.is_empty());

        handle_editing_cancel(&mut state);
        assert!(matches!(state.input_mode, InputMode::Normal));
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
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
        );
        let key_b = content_frame_cache_key(
            node_id,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
        );
        let key_c = content_frame_cache_key(
            node_id,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            25,
        );

        assert!(key_a.is_some());
        assert_eq!(key_a, key_b);
        assert_ne!(key_a, key_c);
    }

    #[test]
    fn build_block_lines_source_mode_includes_full_file_context_for_scroll() {
        let temp_root = std::env::temp_dir()
            .join("trueflow_tests")
            .join("tui_source_scroll_context")
            .join(Uuid::new_v4().to_string());
        let file_path = temp_root.join("src/lib.rs");
        let file_content = "line1\nline2\nline3\nline4\nline5\nline6\n";
        let block_content = "line3\nline4\n";
        let (mut state, block_id) =
            build_state_with_block_file(&file_path, file_content, block_content, 2, 4);
        let node = state.navigator.tree.node(block_id);
        let snapshot = ContentNodeSnapshot::from_node(node);
        let palette = UiPalette::default();

        let (lines, total_lines) = build_block_lines(&mut state, &snapshot, &palette, 2);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(total_lines, 6);
        assert_eq!(rendered.len(), 6);
        assert!(rendered[0].contains("line1"));
        assert!(rendered[1].contains("line2"));
        assert!(rendered[2].contains("line3"));
        assert!(rendered[3].contains("line4"));
        assert!(rendered[5].contains("line6"));
    }

    #[test]
    fn content_cache_key_ignores_height_for_block_diff() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
        );
        let key_b = content_frame_cache_key(
            node_id,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            40,
        );

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn content_cache_key_ignores_mode_and_height_for_file_nodes() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            TreeNodeKind::File,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
        );
        let key_b = content_frame_cache_key(
            node_id,
            TreeNodeKind::File,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::ChangedWithContext { context_lines: 3 },
            40,
        );

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn content_cache_key_is_none_for_non_cacheable_kinds() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key = content_frame_cache_key(
            node_id,
            TreeNodeKind::Directory,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            12,
        );
        assert!(key.is_none());
    }

    #[test]
    fn content_frame_cache_stores_multiple_keys() {
        let node_id = crate::tree::TreeBuilder::new().root();
        let key_a = content_frame_cache_key(
            node_id,
            TreeNodeKind::Block,
            ViewMode::Diff,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
        );
        let key_b = content_frame_cache_key(
            node_id,
            TreeNodeKind::Block,
            ViewMode::Source,
            vcs::BlockDiffFocusMode::WholeBlock,
            20,
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
            },
        );
        cache.insert(
            key_b,
            ContentFrameCacheEntry {
                lines: vec![Line::from("b")],
                total_lines: 1,
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
    fn build_block_diff_lines_uses_repo_relative_path_for_commit_scope_blocks() {
        let repo_root = std::env::temp_dir()
            .join("trueflow_tests")
            .join("tui_commit_scope")
            .join(Uuid::new_v4().to_string());
        let package_dir = repo_root.join("pkg");
        let file_path = package_dir.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_else(|error| panic!("failed to create fixture directory: {error}"));

        let initial = include_str!("../../example_repos/basic_changes/src/main.rs");
        let updated = initial.replace("Hello, world!", "Hello from commit scope");
        fs::write(&file_path, initial)
            .unwrap_or_else(|error| panic!("failed to write initial fixture file: {error}"));

        run_git(&repo_root, &["init", "-q"]);
        run_git(&repo_root, &["config", "user.email", "test@example.com"]);
        run_git(&repo_root, &["config", "user.name", "Test User"]);
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
            ReviewScope::Commit {
                id: revision,
                summary: "Update greeting".to_string(),
            },
            Some("pkg".to_string()),
            HashMap::from([(
                PathBuf::from("pkg/src/lib.rs"),
                vcs::FileDiff::Text {
                    path: RepoPath::new("pkg/src/lib.rs").unwrap(),
                    hunks,
                },
            )]),
        );

        let block = Block {
            hash: crate::hashing::TreeHash::new("block"),
            content: "fn main() {\n    println!(\"Hello from commit scope\");\n}".to_string(),
            kind: BlockKind::Function,
            tags: vec![],
            complexity: None,
            start_line: 0,
            end_line: 3,
        };
        let node = ContentNodeSnapshot {
            id: state.navigator.tree.root(),
            kind: TreeNodeKind::Block,
            path: RepoPath::new("src/lib.rs").unwrap(),
            children: Vec::new(),
            block: Some(block.clone()),
            language: Some(Language::Rust),
        };
        let palette = UiPalette::default();

        let (lines, _len) = build_block_diff_lines(&mut state, &node, &block, &palette);
        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();

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
    fn recap_summary_reports_no_activity_when_session_is_empty() {
        let mut state = build_test_state(ReviewScope::MainDiff, None, HashMap::new());
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
        let mut state = build_test_state(ReviewScope::MainDiff, None, HashMap::new());
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
            lines.iter().any(|line| line.contains("Comments: 2")),
            "expected comments rollup, got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("Questions:")),
            "did not expect separate questions rollup, got: {lines:?}"
        );
    }

    #[test]
    fn recap_key_handler_exits_on_q_and_d() {
        assert!(recap_key_should_exit(KeyCode::Char('q')));
        assert!(recap_key_should_exit(KeyCode::Char('d')));
        assert!(recap_key_should_exit(KeyCode::Esc));
        assert!(!recap_key_should_exit(KeyCode::Char('a')));
    }

    #[test]
    fn toggle_speed_read_mode_activates_on_block_node() {
        let (mut state, block_id) = build_state_with_single_block("alpha beta gamma");
        assert!(state.speed_read.is_none());

        toggle_speed_read_mode(&mut state);

        let Some(mode) = state.speed_read.as_ref() else {
            panic!("expected speed read mode to activate");
        };
        assert_eq!(mode.node_id, block_id);
        assert_eq!(mode.model.playback, PlaybackState::Paused);
    }

    #[test]
    fn toggle_speed_read_mode_ignores_non_block_nodes() {
        let mut state = build_test_state(ReviewScope::All, None, HashMap::new());
        state.navigator.jump_root();

        toggle_speed_read_mode(&mut state);

        assert!(state.speed_read.is_none());
    }

    #[test]
    fn speed_read_space_toggles_playback_and_sets_next_tick() {
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
    match line_highlighter_for(language) {
        Some(highlighter) => highlighter.tokenize_line(line),
        None => plain_text_tokens(line),
    }
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
        Language::Java => LanguageHighlightRules {
            keywords: JAVA_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Python => LanguageHighlightRules {
            keywords: PYTHON_KEYWORDS,
            line_comment_start: Some("#"),
        },
        Language::Go => LanguageHighlightRules {
            keywords: GO_KEYWORDS,
            line_comment_start: Some("//"),
        },
        Language::Cpp => LanguageHighlightRules {
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
        Language::Markdown | Language::Toml | Language::Text | Language::Unknown => return None,
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
    let key = HighlightLineCacheKey {
        line: line.to_string(),
        language: language.copied(),
    };
    if let Some(tokens) = cache.get(&key) {
        return tokens.clone();
    }

    let tokens = highlight_line(line, language);
    cache.insert(key, tokens.clone());
    tokens
}
