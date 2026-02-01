use crate::analysis::Language;
use crate::block::BlockKind;
use crate::commands::mark;
use crate::commands::review::{ReviewOptions, ReviewTarget, collect_review_summary};
use crate::config::{BlockFilters, load as load_config};
use crate::context::TrueflowContext;
use crate::store::Verdict;
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
use crate::vcs;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as UiBlock, Gauge, Paragraph, Wrap},
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};

// --- Core Structs ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewScope {
    All,
    MainDiff,
    Commit { id: String, summary: String },
}

impl ReviewScope {
    fn label(&self) -> String {
        match self {
            ReviewScope::All => "entire review".to_string(),
            ReviewScope::MainDiff => "diff vs main".to_string(),
            ReviewScope::Commit { id, summary } => {
                let short_id = short_commit_id(id);
                let summary = truncate_text(summary, 32);
                if summary.is_empty() {
                    format!("commit {short_id}")
                } else {
                    format!("commit {short_id} {summary}")
                }
            }
        }
    }

    fn to_review_options(&self) -> ReviewOptions {
        match self {
            ReviewScope::All => ReviewOptions {
                all: true,
                targets: vec![ReviewTarget::All],
                only: Vec::new(),
                exclude: Vec::new(),
            },
            ReviewScope::MainDiff => ReviewOptions {
                all: false,
                targets: vec![ReviewTarget::MainDiff],
                only: Vec::new(),
                exclude: Vec::new(),
            },
            ReviewScope::Commit { id, .. } => ReviewOptions {
                all: false,
                targets: vec![ReviewTarget::Revision(id.clone())],
                only: Vec::new(),
                exclude: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct ScopeOption {
    label: String,
    scope: ReviewScope,
}

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

struct ReviewNavigator {
    tree: Tree,
    visible_nodes: HashSet<TreeNodeId>,
    current: TreeNodeId,
}

impl ReviewNavigator {
    fn new(tree: Tree, unreviewed_blocks: HashSet<TreeNodeId>) -> Result<Self> {
        // Compute visible nodes: all unreviewed blocks + their ancestors
        let mut visible_nodes = HashSet::new();
        for block_id in unreviewed_blocks {
            visible_nodes.insert(block_id);
            for ancestor in tree.ancestors(block_id) {
                visible_nodes.insert(ancestor);
            }
        }

        let root = tree.root();
        visible_nodes.insert(root);

        Ok(Self {
            tree,
            visible_nodes,
            current: root,
        })
    }

    fn block_ids_in_subtree(&self, root: TreeNodeId) -> Vec<TreeNodeId> {
        let mut stack = vec![root];
        let mut blocks = Vec::new();
        while let Some(node_id) = stack.pop() {
            if !self.visible_nodes.contains(&node_id) {
                continue;
            }
            let node = self.tree.node(node_id);
            if matches!(node.kind, TreeNodeKind::Block) {
                blocks.push(node_id);
            }
            for child in &node.children {
                stack.push(*child);
            }
        }
        blocks
    }

    fn current_id(&self) -> TreeNodeId {
        self.current
    }

    fn set_current(&mut self, id: TreeNodeId) {
        if self.visible_nodes.contains(&id) {
            self.current = id;
        }
    }

    fn jump_root(&mut self) {
        self.current = self.tree.root();
    }

    fn descend(&mut self) {
        if let Some(child) = self
            .tree
            .node(self.current)
            .children
            .iter()
            .copied()
            .find(|child| self.visible_nodes.contains(child))
        {
            self.current = child;
        }
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.tree.parent(self.current)
            && self.visible_nodes.contains(&parent)
        {
            self.current = parent;
        }
    }

    // Move to next sibling (same parent)
    fn move_next(&mut self) {
        if let Some(next) = self.sibling_at_offset(self.current, 1) {
            self.current = next;
        }
    }

    // Move to prev sibling (same parent)
    fn move_prev(&mut self) {
        if let Some(prev) = self.sibling_at_offset(self.current, -1) {
            self.current = prev;
        }
    }

    fn sibling_at_offset(&self, node_id: TreeNodeId, offset: isize) -> Option<TreeNodeId> {
        let parent = self.tree.parent(node_id)?;
        let siblings: Vec<TreeNodeId> = self
            .tree
            .node(parent)
            .children
            .iter()
            .copied()
            .filter(|child| self.visible_nodes.contains(child))
            .collect();
        let index = siblings.iter().position(|&id| id == node_id)? as isize + offset;
        if index < 0 {
            return None;
        }
        siblings.get(index as usize).copied()
    }
}

fn review_band(block: &crate::block::Block) -> ReviewBand {
    match block.kind.default_review_priority() {
        0 => ReviewBand::Data,
        20 => ReviewBand::Const,
        _ => ReviewBand::Code,
    }
}

fn review_band_rank(band: ReviewBand) -> u8 {
    match band {
        ReviewBand::Data => 0,
        ReviewBand::Const => 1,
        ReviewBand::Code => 2,
    }
}

fn review_group(path: &str, node: &crate::tree::TreeNode) -> ReviewGroup {
    if is_test_block(path, node) {
        ReviewGroup::Test
    } else if is_library_path(path) {
        ReviewGroup::Library
    } else {
        ReviewGroup::Main
    }
}

fn review_group_rank(group: ReviewGroup) -> u8 {
    match group {
        ReviewGroup::Test => 0,
        ReviewGroup::Library => 1,
        ReviewGroup::Main => 2,
    }
}

fn is_library_path(path: &str) -> bool {
    path == "src/lib.rs"
        || (path.starts_with("src/")
            && !path.starts_with("src/main.rs")
            && !path.starts_with("src/bin/"))
}

fn is_test_block(path: &str, node: &crate::tree::TreeNode) -> bool {
    if is_test_path(path) {
        return true;
    }

    if let Some(block) = node.block.as_ref() {
        return block.tags.iter().any(|tag| tag == "test");
    }

    false
}

fn is_test_path(path: &str) -> bool {
    let path = Path::new(path);
    if path
        .components()
        .any(|component| component.as_os_str() == "tests")
    {
        return true;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    file_name.starts_with("test_")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.py")
        || file_name.ends_with("_test.js")
        || file_name.ends_with("_test.ts")
}

impl ReviewOrder {
    fn from_summary(summary: &crate::commands::review::ReviewSummary) -> Self {
        let mut ordered = Vec::new();
        let mut items: Vec<_> = summary
            .unreviewed_block_nodes
            .iter()
            .copied()
            .filter_map(|node_id| {
                let node = summary.tree.node(node_id);
                let block = node.block.as_ref()?;
                let file_path = if node.path.is_empty() {
                    node.name.clone()
                } else {
                    node.path.clone()
                };
                let cursor = ReviewCursor {
                    file_path,
                    band: review_band(block),
                    kind_rank: block.kind.default_review_priority(),
                    start_line: block.start_line,
                    node_id,
                };
                Some((cursor, node))
            })
            .collect();

        items.sort_by(|(a_cursor, a_node), (b_cursor, b_node)| {
            let a_group = review_group(&a_cursor.file_path, a_node);
            let b_group = review_group(&b_cursor.file_path, b_node);
            (
                review_group_rank(a_group),
                &a_cursor.file_path,
                review_band_rank(a_cursor.band),
                a_cursor.kind_rank,
                a_cursor.start_line,
            )
                .cmp(&(
                    review_group_rank(b_group),
                    &b_cursor.file_path,
                    review_band_rank(b_cursor.band),
                    b_cursor.kind_rank,
                    b_cursor.start_line,
                ))
        });

        for (cursor, _) in items {
            ordered.push(cursor);
        }

        Self { ordered }
    }

    fn first_block(&self) -> Option<TreeNodeId> {
        self.ordered.first().map(|cursor| cursor.node_id)
    }

    fn next_after_blocks(
        &self,
        current: TreeNodeId,
        remaining: &HashSet<TreeNodeId>,
    ) -> Option<TreeNodeId> {
        let index = self
            .ordered
            .iter()
            .position(|cursor| cursor.node_id == current)?;
        self.ordered
            .iter()
            .skip(index + 1)
            .find(|cursor| remaining.contains(&cursor.node_id))
            .map(|cursor| cursor.node_id)
    }

    fn next_after_subtree(
        &self,
        subtree_blocks: &HashSet<TreeNodeId>,
        remaining: &HashSet<TreeNodeId>,
    ) -> Option<TreeNodeId> {
        let start_index = self
            .ordered
            .iter()
            .position(|cursor| subtree_blocks.contains(&cursor.node_id))?;

        self.ordered
            .iter()
            .skip(start_index + 1)
            .find(|cursor| {
                remaining.contains(&cursor.node_id) && !subtree_blocks.contains(&cursor.node_id)
            })
            .map(|cursor| cursor.node_id)
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewGroup {
    Test,
    Library,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewBand {
    Data,
    Const,
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewCursor {
    file_path: String,
    band: ReviewBand,
    kind_rank: u8,
    start_line: usize,
    node_id: TreeNodeId,
}

#[derive(Debug, Clone)]
struct ReviewOrder {
    ordered: Vec<ReviewCursor>,
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
    last_frame: std::time::Instant,
    file_cache: HashMap<PathBuf, Vec<String>>,
    root_cursor: Option<TreeNodeId>,
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
                let state =
                    build_review_state(context, summary, config.tui.confirm_batch, scope.label())?;
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
    confirm_batch: bool,
    scope_label: String,
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

    let review_order = ReviewOrder::from_summary(&summary);
    let navigator = ReviewNavigator::new(summary.tree, summary.unreviewed_block_nodes)?;

    Ok(AppState {
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
        last_frame: std::time::Instant::now(),
        file_cache: HashMap::new(),
        root_cursor,
    })
}

fn load_scope_options() -> Result<Vec<ScopeOption>> {
    let mut options = vec![
        ScopeOption {
            label: "All files".to_string(),
            scope: ReviewScope::All,
        },
        ScopeOption {
            label: "Diff vs main".to_string(),
            scope: ReviewScope::MainDiff,
        },
    ];

    if let Ok(commits) = vcs::recent_commits(8) {
        for commit in commits {
            options.push(commit_scope_option(commit));
        }
    }

    Ok(options)
}

fn commit_scope_option(commit: vcs::CommitInfo) -> ScopeOption {
    let short_id = short_commit_id(&commit.id);
    let summary = truncate_text(&commit.summary, 60);
    let label = if summary.is_empty() {
        format!("Commit {short_id}")
    } else {
        format!("Commit {short_id} {summary}")
    };
    ScopeOption {
        label,
        scope: ReviewScope::Commit {
            id: commit.id,
            summary: commit.summary,
        },
    }
}

fn short_commit_id(id: &str) -> String {
    id.chars().take(7).collect()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
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
    let mut last_frame = std::time::Instant::now();

    loop {
        if needs_render || last_frame.elapsed().as_millis() >= 250 {
            terminal.draw(|f| render_scope_selector(f, &selector))?;
            last_frame = std::time::Instant::now();
            needs_render = false;
        }

        if event::poll(std::time::Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
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
}

fn run_app(
    context: &TrueflowContext,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    mut state: AppState,
) -> Result<()> {
    let mut needs_render = true;

    loop {
        if needs_render || state.last_frame.elapsed().as_millis() >= 250 {
            terminal.draw(|f| ui(f, &mut state))?;
            state.last_frame = std::time::Instant::now();
            needs_render = false;
        }

        if event::poll(std::time::Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match &state.input_mode {
                InputMode::Normal => match key.code {
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
                    KeyCode::Char('g') => {
                        state.navigator.jump_root();
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
                InputMode::Editing { .. } => match key.code {
                    KeyCode::Enter => {
                        handle_editing_submit(terminal, context, &mut state)?;
                        needs_render = true;
                    }
                    KeyCode::Esc => {
                        handle_editing_cancel(&mut state);
                        needs_render = true;
                    }
                    KeyCode::Backspace => {
                        state.input_buffer.pop();
                        needs_render = true;
                    }
                    KeyCode::Char(c) => {
                        state.input_buffer.push(c);
                        needs_render = true;
                    }
                    _ => {}
                },
                InputMode::ConfirmBatch { .. } => match key.code {
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
}

// ... helper functions for actions ...

fn handle_ascend(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        return;
    }
    state.navigator.ascend();
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
        }
    } else {
        state.navigator.descend();
    }
}

fn handle_prev(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, -1);
    } else {
        state.navigator.move_prev();
    }
}

fn handle_next(state: &mut AppState) {
    if state.navigator.current_id() == state.navigator.tree.root() {
        move_root_cursor(state, 1);
    } else {
        state.navigator.move_next();
    }
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
        .unwrap_or(0) as isize;
    let next = (current + offset).clamp(0, root_children.len() as isize - 1);
    state.root_cursor = root_children.get(next as usize).copied();
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
        let fingerprint = match node.kind {
            TreeNodeKind::Root => "root".to_string(), // Or repo hash?
            TreeNodeKind::Directory => node.hash.clone(),
            TreeNodeKind::File => node.hash.clone(),
            TreeNodeKind::Block => node.hash.clone(),
        };

        // For root/dir, path might be empty or a dir path.
        // For file/block, it's the file path.
        let path_hint = if node.path.is_empty() {
            None
        } else {
            Some(node.path.clone())
        };

        let line_hint = node.block.as_ref().map(|block| block.start_line as u32);

        mark::run(
            context,
            mark::MarkParams {
                fingerprint,
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
    let block_ids = collect_block_ids_for_action(state, node_id);

    if matches!(verdict, Verdict::Approved | Verdict::Rejected) {
        let mut removed_reviewable = 0;
        for block_id in block_ids {
            if state.navigator.visible_nodes.remove(&block_id) {
                if state.reviewable_nodes.remove(&block_id) {
                    removed_reviewable += 1;
                }
            }
        }
        state.remaining_blocks = state.remaining_blocks.saturating_sub(removed_reviewable);
    }

    prune_invisible_ancestors(state);

    if let Some(node_id) = next_id {
        state.navigator.set_current(node_id);
    } else {
        state.navigator.jump_root();
    }
}

fn collect_block_ids_for_action(state: &AppState, node_id: TreeNodeId) -> Vec<TreeNodeId> {
    let node = state.navigator.tree.node(node_id);
    match node.kind {
        TreeNodeKind::Block => {
            if node
                .block
                .as_ref()
                .is_some_and(|block| matches!(block.kind, BlockKind::Impl | BlockKind::Interface))
            {
                state.navigator.block_ids_in_subtree(node_id)
            } else {
                vec![node_id]
            }
        }
        _ => state.navigator.block_ids_in_subtree(node_id),
    }
}

fn compute_next_review_target(state: &AppState, node_id: TreeNodeId) -> Option<TreeNodeId> {
    let node = state.navigator.tree.node(node_id);
    let remaining = &state.reviewable_nodes;
    match node.kind {
        TreeNodeKind::Block => {
            if node
                .block
                .as_ref()
                .is_some_and(|block| matches!(block.kind, BlockKind::Impl | BlockKind::Interface))
            {
                let subtree_blocks: HashSet<_> = state
                    .navigator
                    .block_ids_in_subtree(node_id)
                    .into_iter()
                    .collect();
                state
                    .review_order
                    .next_after_subtree(&subtree_blocks, remaining)
            } else {
                state.review_order.next_after_blocks(node_id, remaining)
            }
        }
        _ => {
            let subtree_blocks: HashSet<_> = state
                .navigator
                .block_ids_in_subtree(node_id)
                .into_iter()
                .collect();
            state
                .review_order
                .next_after_subtree(&subtree_blocks, remaining)
        }
    }
}

fn prune_invisible_ancestors(state: &mut AppState) {
    let mut visible_nodes = HashSet::new();
    for node_id in state
        .navigator
        .visible_nodes
        .iter()
        .copied()
        .filter(|id| matches!(state.navigator.tree.node(*id).kind, TreeNodeKind::Block))
    {
        for ancestor in state.navigator.tree.ancestors(node_id) {
            visible_nodes.insert(ancestor);
        }
    }

    visible_nodes.insert(state.navigator.tree.root());
    state.navigator.visible_nodes = visible_nodes;
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

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[Enter] select  [j/k] move  [q] quit",
        Style::default().fg(palette.dim).bg(palette.bg),
    )));

    let block = UiBlock::default()
        .title(" Review scope ")
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));

    let popup_area = centered_rect(area, 70, 60);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false }),
        popup_area,
    );
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

    let focus_layout = compute_focus_layout(area, header_lines.len() as u16);
    let actions_lines = build_action_lines(focus_layout.actions.width, palette);
    let node_snapshot = node.clone();
    let content_lines =
        build_content_lines(state, &node_snapshot, palette, focus_layout.code.height);

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
            .wrap(Wrap { trim: false }),
        focus_layout.code,
    );

    let actions_paragraph = Paragraph::new(actions_lines)
        .alignment(Alignment::Center)
        .style(Style::default().bg(palette.bg));

    frame.render_widget(actions_paragraph, focus_layout.actions);
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
        && let Some(breadcrumb) = build_block_breadcrumb(node, state)
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

fn build_block_breadcrumb(node: &crate::tree::TreeNode, state: &AppState) -> Option<String> {
    if !matches!(node.kind, TreeNodeKind::Block) {
        return None;
    }

    let tree = &state.navigator.tree;
    let mut ancestors = tree.ancestors(node.id);
    ancestors.reverse();

    let mut parts = Vec::new();
    let mut file_path = None;
    let mut impl_parts = Vec::new();
    let mut current = None;

    for ancestor_id in ancestors {
        let ancestor = tree.node(ancestor_id);
        match ancestor.kind {
            TreeNodeKind::File => {
                if !ancestor.path.is_empty() {
                    file_path = Some(ancestor.path.clone());
                }
            }
            TreeNodeKind::Block => {
                let Some(block) = ancestor.block.as_ref() else {
                    continue;
                };
                let label = block_signature(block);
                if label.is_empty() {
                    continue;
                }
                if matches!(block.kind, BlockKind::Impl | BlockKind::Interface) {
                    impl_parts.push(label.clone());
                }
                if ancestor.id == node.id {
                    current = Some(label);
                }
            }
            _ => {}
        }
    }

    if let Some(path) = file_path {
        parts.push(format!("File ({path})"));
    }
    parts.extend(impl_parts);
    if let Some(current) = current {
        parts.push(current);
    }

    if parts.len() > 1 {
        Some(parts.join(" -> "))
    } else {
        None
    }
}

fn block_signature(block: &crate::block::Block) -> String {
    let Some(line) = block
        .content
        .lines()
        .find(|line| !line.trim().is_empty())
    else {
        return block.kind.as_str().to_string();
    };
    let mut text = line.trim().trim_end_matches('{').trim().to_string();

    if matches!(
        block.kind,
        BlockKind::Function | BlockKind::Method | BlockKind::FunctionSignature
    ) {
        if let Some(idx) = find_argument_list_start(&text) {
            text.truncate(idx);
        }
    }

    truncate_text(text.trim(), 72)
}

fn find_argument_list_start(text: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            '(' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
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
    node: &crate::tree::TreeNode,
    palette: &UiPalette,
    code_height: u16,
) -> Vec<Line<'static>> {
    match node.kind {
        TreeNodeKind::Block => build_block_lines(state, node, palette, code_height),
        TreeNodeKind::File => build_file_lines(state, node, palette, code_height),
        TreeNodeKind::Directory => build_directory_lines(state, node, palette, code_height),
        TreeNodeKind::Root => build_root_lines(state, palette, code_height),
    }
}

fn load_file_lines(state: &mut AppState, node: &crate::tree::TreeNode) -> Option<Vec<String>> {
    if node.path.is_empty() {
        return None;
    }

    let path = PathBuf::from(&node.path);
    if let Some(lines) = state.file_cache.get(&path) {
        return Some(lines.clone());
    }

    let contents = std::fs::read_to_string(&path).ok()?;
    let lines = contents
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    state.file_cache.insert(path, lines.clone());
    Some(lines)
}

fn build_block_lines(
    state: &mut AppState,
    node: &crate::tree::TreeNode,
    palette: &UiPalette,
    code_height: u16,
) -> Vec<Line<'static>> {
    let Some(block) = &node.block else {
        return vec![Line::from(Span::styled(
            "(No content)",
            Style::default().fg(palette.dim).bg(palette.code_bg),
        ))];
    };

    let language = node.language.clone();
    let block_lines: Vec<String> = block.content.lines().map(|line| line.to_string()).collect();
    let extra_space = code_height.saturating_sub(block_lines.len() as u16) as isize;

    if extra_space < 2 {
        return block_lines
            .iter()
            .map(|line| format_code_line(line, palette, language.as_ref()))
            .collect();
    }

    let total_context = (extra_space - 1).max(0) as usize;
    let mut top_context = total_context / 2 + (total_context % 2);
    let mut bottom_context = total_context / 2;

    let file_lines = match load_file_lines(state, node) {
        Some(lines) => lines,
        None => {
            return block_lines
                .iter()
                .map(|line| format_code_line(line, palette, language.as_ref()))
                .collect();
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
            lines.push(format_context_line(line, palette, language.as_ref()));
        }
    }

    for line in &block_lines {
        lines.push(format_code_line(line, palette, language.as_ref()));
    }

    if bottom_context > 0 {
        let start = end_line;
        let end = (end_line + bottom_context).min(file_lines.len());
        for line in &file_lines[start..end] {
            lines.push(format_context_line(line, palette, language.as_ref()));
        }
    }

    lines
}

fn build_file_lines(
    state: &mut AppState,
    node: &crate::tree::TreeNode,
    palette: &UiPalette,
    code_height: u16,
) -> Vec<Line<'static>> {
    let language = node.language.clone();
    let Some(file_lines) = load_file_lines(state, node) else {
        return vec![Line::from(Span::styled(
            "(File missing)",
            Style::default().fg(palette.context).bg(palette.code_bg),
        ))];
    };

    let max_lines = code_height as usize;
    let mut lines = file_lines
        .iter()
        .take(max_lines)
        .map(|line| format_code_line(line, palette, language.as_ref()))
        .collect::<Vec<_>>();

    if file_lines.len() > max_lines && !lines.is_empty() {
        let last_idx = lines.len().saturating_sub(1);
        lines[last_idx] = format_context_line("...", palette, language.as_ref());
    }

    lines
}

fn build_directory_lines(
    state: &AppState,
    node: &crate::tree::TreeNode,
    palette: &UiPalette,
    code_height: u16,
) -> Vec<Line<'static>> {
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
        return vec![Line::from(Span::styled(
            "(Empty)",
            Style::default().fg(palette.context).bg(palette.code_bg),
        ))];
    }

    let max_lines = code_height as usize;
    let mut entries_list = entries
        .iter()
        .take(max_lines)
        .map(|entry| format_directory_line(entry, palette))
        .collect::<Vec<_>>();

    if entries.len() > max_lines && !entries_list.is_empty() {
        let last_idx = entries_list.len().saturating_sub(1);
        entries_list[last_idx] = format_directory_line("...", palette);
    }

    entries_list
}

fn build_root_lines(
    state: &mut AppState,
    palette: &UiPalette,
    code_height: u16,
) -> Vec<Line<'static>> {
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

    if root_children.is_empty() {
        state.root_cursor = None;
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

    let mut kind_counts = count_block_kinds(state);
    kind_counts.sort_by(|a, b| {
        let parent_a = parent_kind(&a.0);
        let parent_b = parent_kind(&b.0);
        if parent_a != parent_b {
            parent_a.cmp(parent_b)
        } else {
            b.0.as_str().cmp(a.0.as_str())
        }
    });

    let mut last_parent = "";
    for (kind, count) in kind_counts {
        let parent = parent_kind(&kind);
        if parent != last_parent {
            if !last_parent.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("{}:", parent),
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

    let max_lines = code_height.saturating_sub(lines.len() as u16) as usize;
    if max_lines == 0 {
        return lines;
    }

    let mut listing = root_children
        .iter()
        .take(max_lines)
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

    if root_children.len() > max_lines && !listing.is_empty() {
        let last_idx = listing.len().saturating_sub(1);
        listing[last_idx] = format_root_entry_line("  ...", palette, false);
    }

    lines.append(&mut listing);
    lines
}

fn format_root_entry_line(entry: &str, palette: &UiPalette, selected: bool) -> Line<'static> {
    let style = if selected {
        Style::default().fg(palette.fg).bg(palette.meta_bg)
    } else {
        Style::default().fg(palette.context).bg(palette.code_bg)
    };
    Line::from(Span::styled(entry.to_string(), style)).style(style)
}

fn parent_kind(kind: &BlockKind) -> &'static str {
    match kind {
        BlockKind::Function
        | BlockKind::Method
        | BlockKind::FunctionSignature
        | BlockKind::CodeParagraph => "Code Logic",
        BlockKind::Struct
        | BlockKind::Enum
        | BlockKind::Class
        | BlockKind::Impl
        | BlockKind::Macro
        | BlockKind::Const
        | BlockKind::Static
        | BlockKind::Type => "Definitions",
        BlockKind::Module
        | BlockKind::Modules
        | BlockKind::Import
        | BlockKind::Imports
        | BlockKind::Export
        | BlockKind::Preamble => "Module Structure",
        BlockKind::Comment
        | BlockKind::TextBlock
        | BlockKind::Paragraph
        | BlockKind::ListItem
        | BlockKind::Header
        | BlockKind::Quote
        | BlockKind::Section => "Documentation",
        _ => "Other",
    }
}

fn count_block_kinds(state: &AppState) -> Vec<(BlockKind, usize)> {
    let mut counts = HashMap::new();
    for id in &state.navigator.visible_nodes {
        let node = state.navigator.tree.node(*id);
        if node.kind != TreeNodeKind::Block {
            continue;
        }
        let Some(block) = &node.block else {
            continue;
        };
        *counts.entry(block.kind.clone()).or_insert(0) += 1;
    }
    counts.into_iter().collect()
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
    line: &str,
    palette: &UiPalette,
    language: Option<&Language>,
) -> Line<'static> {
    let gutter_left = 4;
    let gutter_right = 2;
    let gutter_spacing = " ".repeat(gutter_left + gutter_right + 1);
    let tokens = highlight_line(line, language);
    let mut spans = Vec::with_capacity(tokens.len() + 1);
    spans.push(Span::styled(
        gutter_spacing,
        Style::default().fg(palette.context).bg(palette.code_bg),
    ));
    for token in tokens {
        let style = style_for_token(&token.kind, palette)
            .fg(palette.context)
            .bg(palette.code_bg);
        spans.push(Span::styled(token.text, style));
    }
    Line::from(spans)
}

fn render_input_overlay(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let popup_area = centered_rect(area, 60, 20);
    frame.render_widget(ratatui::widgets::Clear, popup_area);

    let (title, hints, content) = match &state.input_mode {
        InputMode::Editing { .. } => (
            " Comment ",
            "Enter to submit • Esc to cancel",
            state.input_buffer.clone(),
        ),
        InputMode::ConfirmBatch { count, action } => (
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

    let block = UiBlock::default()
        .title(title)
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));

    let lines = vec![
        Line::from(content),
        Line::from(""),
        Line::from(Span::styled(hints, Style::default().fg(palette.dim))),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup_area,
    );
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
    let padding = ((area.height as f32) * 0.05).round() as u16;

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

fn style_for_token(kind: &TokenKind, palette: &UiPalette) -> Style {
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

fn format_code_line(line: &str, palette: &UiPalette, language: Option<&Language>) -> Line<'static> {
    let tokens = highlight_line(line, language);
    let mut spans = Vec::with_capacity(tokens.len());
    for token in tokens {
        spans.push(Span::styled(
            token.text,
            style_for_token(&token.kind, palette).bg(palette.code_bg),
        ));
    }
    Line::from(spans)
}
