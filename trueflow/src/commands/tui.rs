use crate::analysis::Language;
use crate::commands::mark;
use crate::commands::review::collect_review_summary;
use crate::config::{BlockFilters, TuiConfig, TuiDiffFocusMode, load as load_config};
use crate::context::TrueflowContext;
use crate::review_metadata;
use crate::review_navigator::ReviewNavigator;
use crate::review_order::ReviewOrder;
use crate::review_scope::{
    DiffQuery, ReviewScope, ScopeOption, default_scope_options, diff_query_for_scope,
};
use crate::review_session;
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

// --- Application Logic ---

#[derive(Clone, PartialEq)]
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
    remaining_blocks: usize,
    reviewable_nodes: HashSet<TreeNodeId>,
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
    file_diff_cache: HashMap<PathBuf, Vec<vcs::DiffHunk>>,
    content_frame_cache: HashMap<ContentFrameCacheKey, ContentFrameCacheEntry>,
    highlighted_line_cache: HashMap<HighlightLineCacheKey, Vec<HighlightToken>>,
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
    path: String,
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

pub fn run(context: &TrueflowContext) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let config = load_config()?;
    let run_result = (|| {
        let scope_options = load_scope_options()?;
        let selection = run_scope_selector(&mut terminal, ScopeSelector::new(scope_options))?;

        match selection {
            ScopeSelection::Quit => Ok(()),
            ScopeSelection::Selected(scope) => {
                let filters = config.review.resolve_filters(&[], &[]);
                let summary = load_review_state(context, &scope, &filters)?;
                let state = build_review_state(
                    context,
                    summary,
                    scope.clone(),
                    config.tui.confirm_batch,
                    block_diff_focus_mode_from_config(&config.tui),
                    scope.label(),
                    workdir_prefix_from_git_root(),
                )?;
                run_app(context, &mut terminal, state)
            }
        }
    })();
    restore_terminal(&mut terminal)?;
    run_result
}

fn build_review_state(
    context: &TrueflowContext,
    summary: crate::commands::review::ReviewSummary,
    review_scope: ReviewScope,
    confirm_batch: bool,
    block_diff_focus_mode: vcs::BlockDiffFocusMode,
    scope_label: String,
    workdir_prefix: Option<String>,
) -> Result<AppState> {
    let reviewable_nodes: HashSet<TreeNodeId> = summary
        .unreviewed_block_nodes
        .iter()
        .copied()
        .filter(|&id| matches!(summary.tree.node(id).kind, TreeNodeKind::Block))
        .collect();
    let remaining_blocks = reviewable_nodes.len();

    let root_children = summary.tree.node(summary.tree.root()).children.clone();
    let root_cursor = root_children.first().copied();

    let review_order = ReviewOrder::from_tree(&summary.tree, &summary.unreviewed_block_nodes);
    let navigator = ReviewNavigator::new(summary.tree, summary.unreviewed_block_nodes)?;

    Ok(AppState {
        review_scope,
        navigator,
        review_order,
        total_blocks: summary.total_blocks,
        remaining_blocks,
        reviewable_nodes,
        scope_label,
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        confirm_batch,
        repo_name: detect_repo_name(context),
        workdir_prefix,
        file_cache: HashMap::new(),
        root_cursor,
        scroll_offset: 0,
        content_height: 0,
        viewport_height: 0,
        view_mode: ViewMode::Diff,
        block_diff_focus_mode,
        file_diff_cache: HashMap::new(),
        content_frame_cache: HashMap::new(),
        highlighted_line_cache: HashMap::new(),
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
    F: FnMut(&str) -> Result<HashSet<String>>,
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
                    .any(|path| path_matches_workdir_prefix(path, &prefix)),
                // If we can't resolve changed paths, keep the option instead of hiding it.
                Err(_) => true,
            }
        })
        .collect()
}

fn path_matches_workdir_prefix(path: &str, prefix: &str) -> bool {
    let normalized_path = normalize_path_str(path);
    let normalized_prefix = normalize_path_str(prefix);
    normalized_path == normalized_prefix
        || normalized_path.starts_with(&format!("{normalized_prefix}/"))
}

fn workdir_prefix_from_git_root() -> Option<String> {
    let repo_root = vcs::git_root_from_workdir().ok().flatten()?;
    let cwd = std::env::current_dir().ok()?;
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let relative = cwd.strip_prefix(&repo_root).ok()?;
    let relative_str = normalize_path_str(relative.to_string_lossy().as_ref());
    if relative_str.is_empty() || relative_str == "." {
        None
    } else {
        Some(relative_str)
    }
}

fn normalize_path_str(path: &str) -> String {
    path.trim_start_matches("./").replace('\\', "/")
}

fn repo_relative_path_for_diff(path: &str, workdir_prefix: Option<&str>) -> String {
    let normalized_path = normalize_path_str(path);
    let Some(prefix) = workdir_prefix
        .map(normalize_path_str)
        .filter(|value| !value.is_empty())
    else {
        return normalized_path;
    };

    if normalized_path.is_empty()
        || normalized_path == prefix
        || normalized_path.starts_with(&format!("{prefix}/"))
    {
        return normalized_path;
    }

    format!("{prefix}/{normalized_path}")
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
) -> Result<ScopeSelection> {
    let mut needs_render = true;

    loop {
        if needs_render {
            terminal.draw(|f| render_scope_selector(f, &selector))?;
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

        match key_code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(ScopeSelection::Quit),
            KeyCode::Char('k') | KeyCode::Up => {
                selector.move_prev();
                needs_render = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                selector.move_next();
                needs_render = true;
            }
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

        let event = event::read()?;
        if should_rerender_on_event(&event) {
            needs_render = true;
            continue;
        }

        let Some(key_event) = key_event_for_press_event(&event) else {
            continue;
        };
        let key_code = key_event.code;

        match &state.input_mode {
            InputMode::Normal => match key_code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('k') | KeyCode::Down => {
                    handle_descend(&mut state);
                    needs_render = true;
                }
                KeyCode::Char('i') | KeyCode::Up => {
                    handle_ascend(&mut state);
                    needs_render = true;
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    handle_next(&mut state);
                    needs_render = true;
                }
                KeyCode::Char('j') | KeyCode::Left => {
                    handle_prev(&mut state);
                    needs_render = true;
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
                    if let Some(first) = state.review_order.first_block() {
                        state.navigator.set_current(first);
                    }
                    needs_render = true;
                }
                _ => {}
            },
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
}

fn handle_descend(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        let root = state.navigator.tree.root();
        state.root_cursor = state
            .root_cursor
            .filter(|id| state.navigator.visible_nodes.contains(id))
            .or_else(|| {
                state
                    .navigator
                    .tree
                    .node(root)
                    .children
                    .iter()
                    .copied()
                    .find(|child| state.navigator.visible_nodes.contains(child))
            });

        if let Some(target) = state.root_cursor {
            state.navigator.set_current(target);
            state.scroll_offset = 0;
        }
    } else {
        state.navigator.descend();
        state.scroll_offset = 0;
    }
}

fn handle_prev(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, -1);
    } else {
        state.navigator.move_prev();
        state.scroll_offset = 0;
    }
}

fn handle_next(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, 1);
    } else {
        state.navigator.move_next();
        state.scroll_offset = 0;
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
        .filter(|child| state.navigator.visible_nodes.contains(child))
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
        let count = count_descendant_blocks(&state.navigator, state.navigator.current_id());
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
    let note = state.input_buffer.trim().to_string();
    if note.is_empty() {
        state.input_mode = InputMode::Normal;
        state.input_buffer.clear();
        return Ok(());
    }

    let action = match &state.input_mode {
        InputMode::Editing { action } => action.with_note(note),
        _ => return Ok(()),
    };

    state.input_mode = InputMode::Normal;
    state.input_buffer.clear();

    if matches!(action, PendingAction::Batch { .. }) && state.confirm_batch {
        let count = count_descendant_blocks(
            &state.navigator,
            match &action {
                PendingAction::Single { node_id, .. } | PendingAction::Batch { node_id, .. } => {
                    *node_id
                }
            },
        );
        state.input_mode = InputMode::ConfirmBatch { action, count };
    } else {
        execute_action(terminal, context, state, action)?;
    }
    Ok(())
}

fn handle_editing_cancel(state: &mut AppState) {
    state.input_mode = InputMode::Normal;
    state.input_buffer.clear();
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
        let path_hint = if node.path.is_empty() {
            None
        } else {
            Some(node.path.clone())
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

    apply_action_locally(state, node_id, &verdict, next_id);
    Ok(())
}

fn fingerprint_and_target_kind_for_node(
    node: &crate::tree::TreeNode,
) -> (String, ReviewTargetKind) {
    match node.kind {
        TreeNodeKind::Root | TreeNodeKind::Directory => (node.hash.clone(), ReviewTargetKind::Tree),
        TreeNodeKind::File => (node.hash.clone(), ReviewTargetKind::File),
        TreeNodeKind::Block => (node.hash.clone(), ReviewTargetKind::Block),
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
    context: &TrueflowContext,
    scope: &ReviewScope,
    filters: &BlockFilters,
) -> Result<crate::commands::review::ReviewSummary> {
    let options = scope.to_review_options();
    collect_review_summary(context, &options, filters)
}

fn apply_action_locally(
    state: &mut AppState,
    node_id: TreeNodeId,
    verdict: &Verdict,
    next_id: Option<TreeNodeId>,
) {
    let block_ids = review_session::action_block_ids(
        &state.navigator.tree,
        &state.navigator.visible_nodes,
        node_id,
    );

    if matches!(verdict, Verdict::Approved | Verdict::Rejected) {
        let mut removed_reviewable = 0;
        for block_id in block_ids {
            if state.navigator.visible_nodes.remove(&block_id)
                && state.reviewable_nodes.remove(&block_id)
            {
                removed_reviewable += 1;
            }
        }
        state.remaining_blocks = state.remaining_blocks.saturating_sub(removed_reviewable);
    }

    state.navigator.visible_nodes =
        review_session::prune_visible_nodes(&state.navigator.tree, &state.navigator.visible_nodes);

    if let Some(node_id) = next_id {
        state.navigator.set_current(node_id);
        state.scroll_offset = 0;
    } else {
        state.navigator.jump_root();
        state.scroll_offset = 0;
    }
}

fn compute_next_review_target(state: &AppState, node_id: TreeNodeId) -> Option<TreeNodeId> {
    review_session::next_review_target(
        &state.navigator.tree,
        &state.navigator.visible_nodes,
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

fn count_descendant_blocks(navigator: &ReviewNavigator, id: TreeNodeId) -> usize {
    let mut count = 0;
    let mut stack = vec![id];
    while let Some(curr) = stack.pop() {
        let node = navigator.tree.node(curr);
        if matches!(node.kind, TreeNodeKind::Block) && navigator.visible_nodes.contains(&curr) {
            count += 1;
        }
        for child in &node.children {
            if navigator.visible_nodes.contains(child) {
                stack.push(*child);
            }
        }
    }
    count
}

// --- UI Rendering ---

fn render_scope_selector(frame: &mut Frame, selector: &ScopeSelector) {
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
                "[Enter] select  [j/k] move  [q] quit",
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

    render_active_node(frame, state, content_area, &palette);
    render_footer(frame, state, footer_area, &palette);

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
    let actions_lines = build_action_lines(focus_layout.actions.width, palette);
    let node_snapshot = ContentNodeSnapshot::from_node(node);
    let (content_lines, total_lines) = build_content_lines_with_frame_cache(
        state,
        &node_snapshot,
        palette,
        focus_layout.code.height,
    );

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
                let path = if node.path.is_empty() {
                    "unknown"
                } else {
                    &node.path
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
        && !node.path.is_empty()
        && !matches!(node.kind, TreeNodeKind::Block)
    {
        lines.push(format_header_row(&node.path, palette, false));
    }

    if !matches!(node.kind, TreeNodeKind::Root) && !node.hash.is_empty() {
        lines.push(format_header_row(
            &format!("Hash: {}", &node.hash[..node.hash.len().min(12)]),
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

fn build_action_lines(width: u16, palette: &UiPalette) -> Vec<Line<'static>> {
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

    let pyramid_lines = vec![
        Line::from(Span::styled("[i]ascend", pyramid_style)),
        Line::from(Span::styled("[j]prev            [l]next", pyramid_style)),
        Line::from(Span::styled("  [k]descend", pyramid_style)),
        Line::from(Span::styled("  [d]toggle diff/source", pyramid_style)),
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

fn load_file_lines(state: &mut AppState, path: &str) -> Option<Arc<[String]>> {
    if path.is_empty() {
        return None;
    }

    let path_buf = PathBuf::from(path);
    if let Some(lines) = state.file_cache.get(&path_buf) {
        return Some(Arc::clone(lines));
    }

    let contents = std::fs::read_to_string(path).ok()?;
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
    code_height: u16,
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
    let block_line_count = block.content.lines().count();
    let extra_space =
        i32::from(code_height.saturating_sub(usize_to_u16_saturating(block_line_count)));

    // TODO: if paginating, we shouldn't truncate context based on viewport height alone.
    // However, existing context logic tries to center the block vertically.
    // For pagination, we probably want full context available but scrolled.
    // For now, let's keep context logic but return full lines.

    let total_context = usize::try_from((extra_space - 1).max(0)).unwrap_or(usize::MAX);
    // With scrolling, we might want more context if available, or just render everything?
    // The current design renders a *subset* of file lines around the block.
    // If we want to scroll *within* that subset, we return the subset.
    // If we want to scroll the *whole file*, that's a bigger change.
    // Let's assume we paginate the "view" constructed here.
    // If the block is huge, 'block_lines' is huge, and 'extra_space' is negative.
    // In that case, context is 0.

    let mut top_context = total_context / 2 + (total_context % 2);
    let mut bottom_context = total_context / 2;

    if extra_space < 2 {
        // Block is large or fits perfectly. Minimal context.
        // Actually, if block is large, extra_space < 0.
        // We should just render the block + maybe minimal context if we want?
        // Existing logic returns just block lines if extra_space < 2.
        // This is fine for now; large blocks will just be the block itself.
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

    let file_lines = match load_file_lines(state, &node.path) {
        Some(lines) => lines,
        None => {
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

    let available_top = start_line;
    let available_bottom = file_lines.len().saturating_sub(end_line);

    if top_context > available_top {
        let overflow = top_context - available_top;
        top_context = available_top;
        bottom_context = (bottom_context + overflow).min(available_bottom);
    }

    if bottom_context > available_bottom {
        let overflow = bottom_context - available_bottom;
        bottom_context = available_bottom;
        top_context = (top_context + overflow).min(available_top);
    }

    if top_context + bottom_context < total_context {
        let missing = total_context - (top_context + bottom_context);
        let add_top = missing.min(available_top.saturating_sub(top_context));
        top_context += add_top;
        let add_bottom = missing
            .saturating_sub(add_top)
            .min(available_bottom.saturating_sub(bottom_context));
        bottom_context += add_bottom;
    }

    let mut lines = Vec::new();
    if top_context > 0 {
        let start = start_line.saturating_sub(top_context);
        let end = start_line;
        for line in &file_lines[start..end] {
            lines.push(format_context_line(
                &mut state.highlighted_line_cache,
                line,
                palette,
                language.as_ref(),
            ));
        }
    }

    for line in block.content.lines() {
        lines.push(format_code_line(
            &mut state.highlighted_line_cache,
            line,
            palette,
            language.as_ref(),
        ));
    }

    if bottom_context > 0 {
        let start = end_line;
        let end = (end_line + bottom_context).min(file_lines.len());
        for line in &file_lines[start..end] {
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
    if node.path.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "(No path for diff)",
                Style::default().fg(palette.dim).bg(palette.code_bg),
            ))],
            1,
        );
    }

    let diff_path = repo_relative_path_for_diff(&node.path, state.workdir_prefix.as_deref());
    let path = PathBuf::from(&diff_path);
    let hunks = ensure_cached_diff_hunks(&mut state.file_diff_cache, &path, || {
        let query = diff_query_for_scope(&state.review_scope, &diff_path);
        let repo = vcs::repo_from_workdir()?;
        match query {
            DiffQuery::MainDiff { path } => vcs::diff_hunks_for_file(&repo, &path),
            DiffQuery::Revision { revision, path } => {
                vcs::diff_hunks_for_file_in_revision(&repo, &revision, &path)
            }
        }
    });
    let diff_view =
        vcs::extract_block_diff_view_for_block(block, hunks, state.block_diff_focus_mode);

    let Some(view) = diff_view else {
        // Fallback to source view if no diff overlap found (e.g. new file not in diff, or moved code?)
        // Or should we say "(No diff changes in this block)"?
        // User requested diff view. If block is pure addition, maybe diff covers it?
        // If the block is unchanged in the diff (e.g. we are reviewing it for other reasons),
        // we should probably say so.
        return (
            vec![Line::from(Span::styled(
                "(No diff changes in this block)",
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

fn ensure_cached_diff_hunks<'a, F>(
    cache: &'a mut HashMap<PathBuf, Vec<vcs::DiffHunk>>,
    path: &Path,
    load_hunks: F,
) -> &'a [vcs::DiffHunk]
where
    F: FnOnce() -> Result<Vec<vcs::DiffHunk>>,
{
    let entry = cache
        .entry(path.to_path_buf())
        .or_insert_with(|| load_hunks().unwrap_or_default());
    entry.as_slice()
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
        if !state.navigator.visible_nodes.contains(child_id) {
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
        .filter(|child| state.navigator.visible_nodes.contains(child))
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
        &state.navigator.visible_nodes,
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
            let input_lines = usize_to_u16_saturating(content.split('\n').count().max(1));
            (
                InputOverlayKind::Editing { input_lines },
                " Comment ",
                "Enter to submit • Shift+Enter newline • Esc to cancel",
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

fn input_overlay_rect(area: Rect, kind: InputOverlayKind) -> Rect {
    let preferred_height = match kind {
        InputOverlayKind::Editing { input_lines } => input_lines.saturating_add(4).clamp(5, 12),
        InputOverlayKind::ConfirmBatch => 4,
    };
    let height = area.height.min(preferred_height);
    let width = if area.width >= 20 {
        area.width.min(96)
    } else {
        area.width
    };
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

fn compute_focus_layout(area: Rect, header_lines: u16) -> FocusLayout {
    let code_width = area.width.min(120);
    let desired_code_height = area.height.min(32);
    let padding = u16::try_from((u32::from(area.height) * 5 + 50) / 100).unwrap_or(u16::MAX);

    let available_height = area.height.saturating_sub(padding * 2).max(1);
    let min_header_height = 3.min(available_height);
    let desired_header_height = header_lines.saturating_add(2).max(min_header_height);
    let total_height = (desired_header_height + desired_code_height + 1).min(available_height);
    let header_height = desired_header_height.min(total_height.saturating_sub(1).max(1));
    let remaining = total_height.saturating_sub(header_height + 1);
    let code_height = desired_code_height.min(remaining.max(1));

    let content_top = area.y + (area.height.saturating_sub(total_height)) / 2;
    let content_left = area.x + (area.width.saturating_sub(code_width)) / 2;

    let meta_height = header_height.max(1);
    let meta = Rect {
        x: content_left,
        y: content_top,
        width: code_width,
        height: meta_height,
    };

    let code = Rect {
        x: content_left,
        y: content_top + meta_height,
        width: code_width,
        height: code_height,
    };

    let actions = Rect {
        x: content_left,
        y: content_top + meta_height + code_height,
        width: code_width,
        height: 4,
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
        file_diff_cache: HashMap<PathBuf, Vec<vcs::DiffHunk>>,
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
            remaining_blocks: 0,
            reviewable_nodes: HashSet::new(),
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
            file_diff_cache,
            content_frame_cache: HashMap::new(),
            highlighted_line_cache: HashMap::new(),
        }
    }

    #[test]
    fn diff_query_uses_main_diff_for_main_scope() {
        let query = diff_query_for_scope(&ReviewScope::MainDiff, "src/lib.rs");
        assert_eq!(
            query,
            DiffQuery::MainDiff {
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_uses_revision_for_commit_scope() {
        let query = diff_query_for_scope(
            &ReviewScope::Commit {
                id: "abc123".to_string(),
                summary: "test".to_string(),
            },
            "src/lib.rs",
        );
        assert_eq!(
            query,
            DiffQuery::Revision {
                revision: "abc123".to_string(),
                path: "src/lib.rs".to_string(),
            }
        );
    }

    #[test]
    fn diff_query_uses_main_diff_for_all_scope() {
        let query = diff_query_for_scope(&ReviewScope::All, "src/lib.rs");
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
                "a" => HashSet::from([String::from("trueflow/src/lib.rs")]),
                "b" => HashSet::from([String::from("README.md")]),
                _ => HashSet::new(),
            };
            Ok(paths)
        });

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "a");
    }

    #[test]
    fn ensure_cached_diff_hunks_inserts_empty_on_loader_error() {
        let mut cache = HashMap::new();
        let path = PathBuf::from("src/lib.rs");

        let hunks = ensure_cached_diff_hunks(&mut cache, &path, || {
            Err(anyhow::anyhow!("repo unavailable"))
        });

        assert!(hunks.is_empty(), "expected empty hunks on load failure");
        assert!(
            cache.contains_key(&path),
            "failed loads should still cache empty hunks"
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
        };
        let mode = block_diff_focus_mode_from_config(&config);
        assert_eq!(
            mode,
            vcs::BlockDiffFocusMode::ChangedWithContext { context_lines: 7 }
        );
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
        let hunks = vcs::diff_hunks_for_file_in_revision(&repo, &revision, "pkg/src/lib.rs")
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
            HashMap::from([(PathBuf::from("pkg/src/lib.rs"), hunks)]),
        );

        let block = Block {
            hash: "block".to_string(),
            content: "fn main() {\n    println!(\"Hello from commit scope\");\n}".to_string(),
            kind: BlockKind::Function,
            tags: vec![],
            complexity: 0,
            start_line: 0,
            end_line: 3,
        };
        let node = ContentNodeSnapshot {
            id: state.navigator.tree.root(),
            kind: TreeNodeKind::Block,
            path: "src/lib.rs".to_string(),
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
        assert_eq!(fingerprint, root_node.hash);
        assert_eq!(kind, ReviewTargetKind::Tree);
    }
}

struct UiPalette {
    bg: Color,
    fg: Color,
    code_fg: Color,
    dim: Color,
    add: Color,
    #[allow(dead_code)]
    del: Color,
    keyword: Color,
    string: Color,
    number: Color,
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
    #[allow(dead_code)]
    String,
    Number,
    #[allow(dead_code)]
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

fn highlight_line(line: &str, _language: Option<&Language>) -> Vec<HighlightToken> {
    // Very basic highlighting for now
    let mut tokens = Vec::new();
    let mut current_word = String::new();

    for c in line.chars() {
        if c.is_alphanumeric() || c == '_' {
            current_word.push(c);
        } else {
            if !current_word.is_empty() {
                tokens.push(classify_token(&current_word));
                current_word.clear();
            }
            tokens.push(HighlightToken {
                text: c.to_string(),
                kind: TokenKind::Base,
            });
        }
    }
    if !current_word.is_empty() {
        tokens.push(classify_token(&current_word));
    }
    tokens
}

fn classify_token(word: &str) -> HighlightToken {
    let kind = match word {
        "fn" | "struct" | "enum" | "impl" | "use" | "mod" | "pub" | "let" | "mut" | "if"
        | "else" | "match" | "for" | "while" | "return" | "break" | "continue" | "const"
        | "static" | "trait" | "type" => TokenKind::Keyword,
        "true" | "false" => TokenKind::Number,
        _ if word.chars().all(char::is_numeric) => TokenKind::Number,
        _ => TokenKind::Base,
    };
    HighlightToken {
        text: word.to_string(),
        kind,
    }
}

fn style_for_token(kind: TokenKind, palette: &UiPalette) -> Style {
    match kind {
        TokenKind::Base => Style::default().fg(palette.code_fg),
        TokenKind::Keyword => Style::default()
            .fg(palette.keyword)
            .add_modifier(Modifier::BOLD),
        TokenKind::String => Style::default().fg(palette.string),
        TokenKind::Number => Style::default().fg(palette.number),
        TokenKind::Comment => Style::default().fg(palette.dim),
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
