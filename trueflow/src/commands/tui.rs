use crate::commands::mark;
use crate::commands::review::{ReviewOptions, collect_review_summary};
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::store::Verdict;
use crate::tree::{Tree, TreeNodeId, TreeNodeKind};
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Stdout};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// --- Core Structs ---

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NodeKey {
    Root,
    Directory(String), // path
    File(String),      // path
    Block(String),     // hash
}

impl NodeKey {
    fn from_node(tree: &Tree, id: TreeNodeId) -> Self {
        let node = tree.node(id);
        match node.kind {
            TreeNodeKind::Root => NodeKey::Root,
            TreeNodeKind::Directory => NodeKey::Directory(node.path.clone()),
            TreeNodeKind::File => NodeKey::File(node.path.clone()),
            TreeNodeKind::Block => NodeKey::Block(node.hash.clone()),
        }
    }
}

struct ReviewNavigator {
    tree: Tree,
    visible_nodes: HashSet<TreeNodeId>,
    current: TreeNodeId,
    visible_order: Vec<TreeNodeId>,
    index_by_node: HashMap<TreeNodeId, usize>,
    depth_by_node: HashMap<TreeNodeId, usize>,
    visible_by_depth: BTreeMap<usize, Vec<TreeNodeId>>,
    first_child_by_node: HashMap<TreeNodeId, TreeNodeId>,
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

        let mut visible_order = Vec::new();
        let mut index_by_node = HashMap::new();
        let mut depth_by_node = HashMap::new();
        let mut visible_by_depth: BTreeMap<usize, Vec<TreeNodeId>> = BTreeMap::new();
        let mut first_child_by_node = HashMap::new();

        let mut stack = vec![(root, 0)];
        while let Some((node_id, depth)) = stack.pop() {
            if !visible_nodes.contains(&node_id) {
                continue;
            }
            let idx = visible_order.len();
            visible_order.push(node_id);
            index_by_node.insert(node_id, idx);
            depth_by_node.insert(node_id, depth);
            visible_by_depth.entry(depth).or_default().push(node_id);

            let node = tree.node(node_id);
            let mut children: Vec<TreeNodeId> = node
                .children
                .iter()
                .copied()
                .filter(|child| visible_nodes.contains(child))
                .collect();
            if let Some(first_child) = children.first().copied() {
                first_child_by_node.insert(node_id, first_child);
            }

            // stack is LIFO; push in reverse to preserve order
            children.reverse();
            for child in children {
                stack.push((child, depth + 1));
            }
        }

        Ok(Self {
            tree,
            visible_nodes,
            current: root,
            visible_order,
            index_by_node,
            depth_by_node,
            visible_by_depth,
            first_child_by_node,
        })
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
        if let Some(child) = self.first_child_by_node.get(&self.current) {
            self.current = *child;
        }
    }

    fn ascend(&mut self) {
        if let Some(parent) = self.tree.parent(self.current)
            && self.visible_nodes.contains(&parent)
        {
            self.current = parent;
        }
    }

    // Move to next sibling (or wrap to cousin) at same depth
    fn move_next(&mut self) {
        let depth = self.depth_of(self.current);
        if let Some(nodes) = self.visible_by_depth.get(&depth)
            && let Some(pos) = nodes.iter().position(|&id| id == self.current)
            && pos + 1 < nodes.len()
        {
            self.current = nodes[pos + 1];
        }
    }

    // Move to prev sibling at same depth
    fn move_prev(&mut self) {
        let depth = self.depth_of(self.current);
        if let Some(nodes) = self.visible_by_depth.get(&depth)
            && let Some(pos) = nodes.iter().position(|&id| id == self.current)
            && pos > 0
        {
            self.current = nodes[pos - 1];
        }
    }

    fn move_next_visible(&mut self) {
        if let Some(current_idx) = self.index_by_node.get(&self.current)
            && current_idx + 1 < self.visible_order.len()
        {
            self.current = self.visible_order[current_idx + 1];
        }
    }

    fn move_prev_visible(&mut self) {
        if let Some(current_idx) = self.index_by_node.get(&self.current)
            && *current_idx > 0
        {
            self.current = self.visible_order[current_idx - 1];
        }
    }

    fn depth_of(&self, id: TreeNodeId) -> usize {
        self.depth_by_node.get(&id).copied().unwrap_or(0)
    }

    // For minimap: find logical neighbors in the level order
    fn neighbors(&self) -> (Option<TreeNodeId>, Option<TreeNodeId>) {
        let depth = self.depth_of(self.current);
        let nodes = self.visible_by_depth.get(&depth);
        let pos = nodes
            .and_then(|list| list.iter().position(|&id| id == self.current))
            .unwrap_or(0);
        let left = nodes
            .and_then(|list| list.get(pos.wrapping_sub(1)).copied())
            .filter(|_| pos > 0);
        let right = nodes.and_then(|list| list.get(pos + 1).copied());
        (left, right)
    }

    fn find_node_by_key(&self, key: &NodeKey) -> Option<TreeNodeId> {
        match key {
            NodeKey::Root => Some(self.tree.root()),
            NodeKey::Directory(p) | NodeKey::File(p) => self.tree.find_by_path(p),
            NodeKey::Block(h) => {
                for file_node in self.tree.file_nodes() {
                    if let Some(id) = self.tree.node_by_path_and_hash(&file_node.path, h) {
                        return Some(id);
                    }
                }
                None
            }
        }
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
    total_blocks: usize,
    remaining_blocks: usize,
    input_mode: InputMode,
    input_buffer: String,
    confirm_batch: bool,
    repo_name: String,
}

pub fn run(context: &TrueflowContext) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let config = load_config()?;
    let summary = load_review_state(context)?;

    let mut state = AppState {
        navigator: ReviewNavigator::new(summary.tree, summary.unreviewed_block_nodes)?,
        total_blocks: summary.total_blocks,
        remaining_blocks: summary.total_blocks, // Initial load
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        confirm_batch: config.tui.confirm_batch,
        repo_name: detect_repo_name(context),
    };

    // Refresh initially to get correct remaining counts
    refresh_state(context, &mut state)?;

    let run_result = run_app(context, &mut terminal, state);
    restore_terminal(&mut terminal)?;
    run_result
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

fn run_app(
    context: &TrueflowContext,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    mut state: AppState,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, &state))?;

        if event::poll(std::time::Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match &state.input_mode {
                InputMode::Normal => match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('s') => state.navigator.descend(),
                    KeyCode::Char('p') => state.navigator.ascend(),
                    KeyCode::Char('j') | KeyCode::Right => state.navigator.move_next(),
                    KeyCode::Char('k') | KeyCode::Left => state.navigator.move_prev(),
                    KeyCode::Char('n') => state.navigator.move_next_visible(),
                    KeyCode::Char('b') => state.navigator.move_prev_visible(),
                    KeyCode::Char('a') => {
                        handle_action(terminal, context, &mut state, Verdict::Approved)?
                    }
                    KeyCode::Char('x') => {
                        handle_action(terminal, context, &mut state, Verdict::Rejected)?
                    }
                    KeyCode::Char('c') => handle_comment_action(&mut state)?,
                    KeyCode::Char('g') => state.navigator.jump_root(),
                    _ => {}
                },
                InputMode::Editing { .. } => match key.code {
                    KeyCode::Enter => handle_editing_submit(terminal, context, &mut state)?,
                    KeyCode::Esc => handle_editing_cancel(&mut state),
                    KeyCode::Backspace => {
                        state.input_buffer.pop();
                    }
                    KeyCode::Char(c) => {
                        state.input_buffer.push(c);
                    }
                    _ => {}
                },
                InputMode::ConfirmBatch { .. } => match key.code {
                    KeyCode::Enter => handle_confirm_batch(terminal, context, &mut state)?,
                    KeyCode::Esc => handle_confirm_cancel(&mut state),
                    _ => {}
                },
            }
        }
    }
}

// ... helper functions for actions ...

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
                verdict,
                check: "review".to_string(),
                note,
                path: path_hint,
                line: line_hint,
            },
        )
    })?;

    refresh_state(context, state)?;
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

fn load_review_state(context: &TrueflowContext) -> Result<crate::commands::review::ReviewSummary> {
    let options = ReviewOptions {
        all: true,
        targets: vec![crate::commands::review::ReviewTarget::All],
        only: Vec::new(),
        exclude: Vec::new(),
    };
    let config = load_config()?;
    let filters = config
        .review
        .resolve_filters(&options.only, &options.exclude);
    collect_review_summary(context, &options, &filters)
}

fn refresh_state(context: &TrueflowContext, state: &mut AppState) -> Result<()> {
    let summary = load_review_state(context)?;
    let current_key = NodeKey::from_node(&state.navigator.tree, state.navigator.current_id());

    state.navigator = ReviewNavigator::new(summary.tree, summary.unreviewed_block_nodes)?;
    state.remaining_blocks = state
        .navigator
        .visible_nodes
        .iter()
        .filter(|&&id| matches!(state.navigator.tree.node(id).kind, TreeNodeKind::Block))
        .count();

    // Try to restore position
    if let Some(id) = state.navigator.find_node_by_key(&current_key) {
        state.navigator.set_current(id);
    } else {
        // Fallback: stay at root (default)
    }

    Ok(())
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

fn ui(frame: &mut Frame, state: &AppState) {
    let palette = UiPalette::default();
    let area = frame.size();

    // 1. Background
    frame.render_widget(
        UiBlock::default().style(Style::default().bg(palette.bg)),
        area,
    );

    // 2. Main Layout
    let layout = Layout::vertical([
        Constraint::Min(0),    // Content + Minimap
        Constraint::Length(1), // Footer
    ])
    .split(area);

    let main_area = layout[0];
    let footer_area = layout[1];

    // 3. Content Area (Minimap | Content)
    let content_layout = Layout::horizontal([
        Constraint::Length(28), // Fixed minimap width
        Constraint::Min(0),     // Content
    ])
    .split(main_area);

    render_minimap(frame, state, content_layout[0], &palette);
    render_active_node(frame, state, content_layout[1], &palette);

    // 4. Footer
    render_footer(frame, state, footer_area, &palette);

    // 5. Input Overlay
    if matches!(
        state.input_mode,
        InputMode::Editing { .. } | InputMode::ConfirmBatch { .. }
    ) {
        render_input_overlay(frame, state, area, &palette);
    }
}

fn render_minimap(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let block = UiBlock::default()
        .title(" Map ")
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(palette.bg).fg(palette.fg));

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    // Vertical layout for the cross:
    // Top: Parent (centered)
    // Middle: Left | Current | Right
    // Bottom: Child (centered)
    // We have 7 lines available (9 height - 2 borders).
    // Parent at index 1.
    // Siblings at index 3.
    // Child at index 5.

    let mut lines = vec![Line::from(""); 7];

    let current_id = state.navigator.current_id();
    let parent_id = state.navigator.tree.parent(current_id);
    let (left_id, right_id) = state.navigator.neighbors();

    // Child: first visible child
    let node = state.navigator.tree.node(current_id);
    let child_id = node
        .children
        .iter()
        .find(|&&c| state.navigator.visible_nodes.contains(&c))
        .copied();

    // 1. Parent Line (Top Center)
    if let Some(pid) = parent_id {
        let label = format_node_label(&state.navigator.tree, pid, &state.repo_name);
        lines[1] = format_center_line(&label, inner_area.width as usize, palette, false);
    } else {
        lines[1] = format_center_line("␀", inner_area.width as usize, palette, false);
    }

    // 2. Siblings Line (Middle)
    // Layout: [ Left (9) ] [ Current (8) ] [ Right (9) ] = 26 chars
    let w_side = 9;
    let w_mid = inner_area.width as usize - (w_side * 2);

    let left_text = left_id
        .map(|id| format_node_label(&state.navigator.tree, id, &state.repo_name))
        .unwrap_or_else(|| "␀".to_string());
    let curr_text = format_node_label(&state.navigator.tree, current_id, &state.repo_name);
    let right_text = right_id
        .map(|id| format_node_label(&state.navigator.tree, id, &state.repo_name))
        .unwrap_or_else(|| "␀".to_string());

    let spans = vec![
        // Left
        Span::styled(
            format_column(&left_text, w_side, Alignment::Right),
            Style::default().fg(palette.dim).bg(palette.bg),
        ),
        // Current (highlighted)
        Span::styled(
            format_column(&curr_text, w_mid, Alignment::Center),
            Style::default()
                .fg(palette.add)
                .bg(palette.bg)
                .add_modifier(Modifier::BOLD),
        ),
        // Right
        Span::styled(
            format_column(&right_text, w_side, Alignment::Left),
            Style::default().fg(palette.dim).bg(palette.bg),
        ),
    ];
    lines[3] = Line::from(spans);

    // 3. Child Line (Bottom Center)
    if let Some(cid) = child_id {
        let label = format_node_label(&state.navigator.tree, cid, &state.repo_name);
        lines[5] = format_center_line(&label, inner_area.width as usize, palette, false);
    } else {
        lines[5] = format_center_line("␀", inner_area.width as usize, palette, false);
    }

    frame.render_widget(
        Paragraph::new(lines).block(UiBlock::default().style(Style::default().bg(palette.bg))),
        inner_area,
    );
}

fn format_node_label(tree: &Tree, id: TreeNodeId, repo_name: &str) -> String {
    let node = tree.node(id);
    match node.kind {
        TreeNodeKind::Root => repo_name.to_string(),
        TreeNodeKind::Directory => format!("{}/", node.name),
        TreeNodeKind::File => node.name.clone(),
        TreeNodeKind::Block => node
            .name
            .split(':')
            .next()
            .unwrap_or(&node.name)
            .to_string(), // Just "function"
    }
}

fn format_center_line(text: &str, width: usize, palette: &UiPalette, bold: bool) -> Line<'static> {
    let style = if bold {
        Style::default()
            .fg(palette.add)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.dim).bg(palette.bg)
    };
    Line::from(Span::styled(
        format_column(text, width, Alignment::Center),
        style,
    ))
}

fn format_column(text: &str, width: usize, align: Alignment) -> String {
    let text_width = text.width();
    if text_width > width {
        // Truncate
        let mut trunc = String::new();
        let mut w = 0;
        for c in text.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > width {
                break;
            }
            trunc.push(c);
            w += cw;
        }
        return trunc;
    }

    let padding = width - text_width;
    match align {
        Alignment::Left => format!("{}{}", text, " ".repeat(padding)),
        Alignment::Right => format!("{}{}", " ".repeat(padding), text),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
    }
}

fn render_active_node(frame: &mut Frame, state: &AppState, area: Rect, palette: &UiPalette) {
    let node = state.navigator.tree.node(state.navigator.current_id());

    // Header
    let mut header_lines = Vec::new();
    let title = match node.kind {
        TreeNodeKind::Root => format!("Root: {}", state.repo_name),
        TreeNodeKind::Directory => format!("Directory: {}/", node.name),
        TreeNodeKind::File => format!("File: {}", node.name),
        TreeNodeKind::Block => format!("Block: {}", node.name),
    };

    header_lines.push(Line::from(Span::styled(
        title,
        Style::default()
            .fg(palette.fg)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    )));

    if !node.path.is_empty() {
        header_lines.push(Line::from(Span::styled(
            format!("Path: {}", node.path),
            Style::default().fg(palette.dim).bg(palette.bg),
        )));
    }

    header_lines.push(Line::from(Span::styled(
        format!("Hash: {}", &node.hash[..node.hash.len().min(12)]),
        Style::default().fg(palette.dim).bg(palette.bg),
    )));

    // Actions Hint
    let actions_text = "Actions: [a]pprove [x]reject [c]omment [s]descend [p]parent [n]next [b]prev [g]root [q]uit";
    let actions_line = Line::from(Span::styled(
        actions_text,
        Style::default()
            .fg(palette.dim)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    ));

    // Content
    let content_lines = if let Some(block) = &node.block {
        block
            .content
            .lines()
            .map(|l| format_code_line(l, palette))
            .collect::<Vec<_>>()
    } else {
        vec![Line::from(Span::styled(
            "(No content)",
            Style::default().fg(palette.dim).bg(palette.code_bg),
        ))]
    };

    let layout = Layout::vertical([
        Constraint::Length(header_lines.len() as u16 + 1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_lines), layout[0]);

    frame.render_widget(
        Paragraph::new(content_lines)
            .block(UiBlock::default().style(Style::default().bg(palette.code_bg)))
            .wrap(Wrap { trim: false }),
        layout[1],
    );

    frame.render_widget(
        Paragraph::new(actions_line).alignment(Alignment::Center),
        layout[2],
    );
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

struct UiPalette {
    bg: Color,
    fg: Color,
    code_fg: Color,
    dim: Color,
    add: Color,
    del: Color,
    code_bg: Color,
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
            code_bg: Color::Rgb(240, 240, 238),
        }
    }
}

fn format_code_line(line: &str, palette: &UiPalette) -> Line<'static> {
    let gutter_left = 4;
    let gutter_right = 2;
    let gutter_spacing = " ".repeat(gutter_left + gutter_right + 1);

    if let Some(rest) = line.strip_prefix('+') {
        let marker = format!("{}+{}", " ".repeat(gutter_left), " ".repeat(gutter_right));
        let style = Style::default()
            .fg(palette.add)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD);
        return Line::from(vec![
            Span::styled(marker, style),
            Span::styled(rest.to_string(), style),
        ]);
    }

    if let Some(rest) = line.strip_prefix('-') {
        let marker = format!("{}-{}", " ".repeat(gutter_left), " ".repeat(gutter_right));
        let style = Style::default()
            .fg(palette.del)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD);
        return Line::from(vec![
            Span::styled(marker, style),
            Span::styled(rest.to_string(), style),
        ]);
    }

    let trimmed = line.trim_start();
    let comment_style = if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        Style::default().fg(palette.dim).bg(palette.code_bg)
    } else {
        Style::default().fg(palette.code_fg).bg(palette.code_bg)
    };
    let gutter_style = Style::default().fg(palette.dim).bg(palette.code_bg);
    Line::from(vec![
        Span::styled(gutter_spacing, gutter_style),
        Span::styled(line.to_string(), comment_style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, BlockKind};
    use crate::tree::{self, TreeBuilder};
    use std::collections::HashSet;

    fn build_block(hash: &str, kind: BlockKind, start_line: usize) -> Block {
        Block {
            hash: hash.to_string(),
            content: "fn example() {}".to_string(),
            kind,
            tags: Vec::new(),
            complexity: 0,
            start_line,
            end_line: start_line + 1,
        }
    }

    #[test]
    fn navigator_moves_across_parents() {
        let block_a = build_block("hash-a", BlockKind::Function, 0);
        let block_b = build_block("hash-b", BlockKind::Function, 0);
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let dir_a = builder.add_dir(root, "alpha".to_string(), "alpha".to_string());
        let file_a = builder.add_file(
            dir_a,
            "a.rs".to_string(),
            "alpha/a.rs".to_string(),
            "file-a".to_string(),
            Language::Rust,
        );
        builder.add_block(
            file_a,
            block_a.kind.as_str().to_string(),
            "alpha/a.rs".to_string(),
            block_a,
            Language::Rust,
        );
        let dir_b = builder.add_dir(root, "beta".to_string(), "beta".to_string());
        let file_b = builder.add_file(
            dir_b,
            "b.rs".to_string(),
            "beta/b.rs".to_string(),
            "file-b".to_string(),
            Language::Rust,
        );
        builder.add_block(
            file_b,
            block_b.kind.as_str().to_string(),
            "beta/b.rs".to_string(),
            block_b,
            Language::Rust,
        );
        let tree = builder.finalize();
        let block_nodes: HashSet<_> = tree
            .nodes()
            .iter()
            .filter(|node| node.kind == tree::TreeNodeKind::Block)
            .map(|node| node.id)
            .collect();
        let mut navigator = ReviewNavigator::new(tree, block_nodes).expect("navigator");
        let block_b_id = navigator
            .tree
            .node_by_path_and_hash("beta/b.rs", "hash-b")
            .expect("block b");
        navigator.set_current(block_b_id);
        navigator.move_prev();
        let current = navigator.current_id();
        let current_node = navigator.tree.node(current);
        assert_eq!(current_node.hash, "hash-a");
    }
}
