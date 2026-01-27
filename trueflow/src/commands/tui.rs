use crate::analysis::Language;
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
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};

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
    siblings_by_node: HashMap<TreeNodeId, Vec<TreeNodeId>>,
    sibling_index_by_node: HashMap<TreeNodeId, usize>,
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

        let mut siblings_by_node: HashMap<TreeNodeId, Vec<TreeNodeId>> = HashMap::new();
        let mut sibling_index_by_node: HashMap<TreeNodeId, usize> = HashMap::new();
        let mut first_child_by_node = HashMap::new();

        let mut stack = vec![root];
        while let Some(node_id) = stack.pop() {
            if !visible_nodes.contains(&node_id) {
                continue;
            }

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

            for (index, child) in children.iter().enumerate() {
                siblings_by_node.insert(*child, children.clone());
                sibling_index_by_node.insert(*child, index);
            }

            // stack is LIFO; push in reverse to preserve order
            children.reverse();
            for child in children {
                stack.push(child);
            }
        }

        Ok(Self {
            tree,
            visible_nodes,
            current: root,
            siblings_by_node,
            sibling_index_by_node,
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
        let siblings = self.siblings_by_node.get(&node_id)?;
        let index = *self.sibling_index_by_node.get(&node_id)? as isize + offset;
        if index < 0 {
            return None;
        }
        siblings.get(index as usize).copied()
    }

    fn next_after_approval_key(&self, node_id: TreeNodeId) -> Option<NodeKey> {
        let parent = self.tree.parent(node_id)?;
        if let Some(next_sibling) = self.sibling_at_offset(node_id, 1) {
            return Some(NodeKey::from_node(&self.tree, next_sibling));
        }
        Some(NodeKey::from_node(&self.tree, parent))
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
                    KeyCode::Char('n') => state.navigator.move_next(),
                    KeyCode::Char('b') => state.navigator.move_prev(),
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

    let next_key = state.navigator.next_after_approval_key(node_id);

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
    if let Some(key) = next_key
        && let Some(node_id) = state.navigator.find_node_by_key(&key)
    {
        state.navigator.set_current(node_id);
    }
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
    let actions_text = "Actions: [a]pprove [x]reject [c]omment [s]descend [p]parent [n]next sibling [b]prev sibling [g]root [q]uit";
    let actions_line = Line::from(Span::styled(
        actions_text,
        Style::default()
            .fg(palette.dim)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    ));

    // Content
    let content_lines = if let Some(block) = &node.block {
        let language = node.language.clone();
        block
            .content
            .lines()
            .map(|line| format_code_line(line, palette, language.as_ref()))
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
    keyword: Color,
    string: Color,
    number: Color,
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
            keyword: Color::Rgb(69, 133, 136),
            string: Color::Rgb(215, 153, 33),
            number: Color::Rgb(177, 98, 134),
            code_bg: Color::Rgb(240, 240, 238),
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

impl HighlightToken {
    fn new(text: impl Into<String>, kind: TokenKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

fn format_code_line(line: &str, palette: &UiPalette, language: Option<&Language>) -> Line<'static> {
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

    let gutter_style = Style::default().fg(palette.dim).bg(palette.code_bg);
    let tokens = highlight_line(line, language);
    let mut spans = Vec::with_capacity(tokens.len() + 1);
    spans.push(Span::styled(gutter_spacing, gutter_style));
    for token in tokens {
        let style = style_for_token(&token.kind, palette);
        spans.push(Span::styled(token.text, style));
    }
    Line::from(spans)
}

fn highlight_line(line: &str, language: Option<&Language>) -> Vec<HighlightToken> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        return vec![HighlightToken::new(line.to_string(), TokenKind::Comment)];
    }

    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let mut in_string = false;
    let mut string_delim = '\0';
    let chars = line.chars().peekable();

    for ch in chars {
        if in_string {
            buffer.push(ch);
            if ch == string_delim {
                tokens.push(HighlightToken::new(
                    std::mem::take(&mut buffer),
                    TokenKind::String,
                ));
                in_string = false;
            }
            continue;
        }

        if matches!(ch, '"' | '\'') {
            if !buffer.is_empty() {
                tokens.extend(tokenize_buffer(&buffer, language));
                buffer.clear();
            }
            buffer.push(ch);
            in_string = true;
            string_delim = ch;
            continue;
        }

        buffer.push(ch);
    }

    if !buffer.is_empty() {
        let kind = if in_string {
            TokenKind::String
        } else {
            TokenKind::Base
        };
        if in_string {
            tokens.push(HighlightToken::new(buffer, kind));
        } else {
            tokens.extend(tokenize_buffer(&buffer, language));
        }
    }

    if tokens.is_empty() {
        tokens.push(HighlightToken::new(line.to_string(), TokenKind::Base));
    }

    tokens
}

fn tokenize_buffer(buffer: &str, language: Option<&Language>) -> Vec<HighlightToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in buffer.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            flush_word_token(&mut tokens, &mut current, language);
            tokens.push(HighlightToken::new(ch.to_string(), TokenKind::Base));
        }
    }

    flush_word_token(&mut tokens, &mut current, language);
    tokens
}

fn flush_word_token(
    tokens: &mut Vec<HighlightToken>,
    current: &mut String,
    language: Option<&Language>,
) {
    if current.is_empty() {
        return;
    }

    let kind = classify_word_token(current, language);
    tokens.push(HighlightToken::new(std::mem::take(current), kind));
}

fn classify_word_token(word: &str, language: Option<&Language>) -> TokenKind {
    if word.chars().all(|ch| ch.is_ascii_digit()) {
        return TokenKind::Number;
    }

    let Some(language) = language else {
        return TokenKind::Base;
    };

    let is_keyword = match language {
        Language::Rust => matches!(
            word,
            "fn" | "struct"
                | "enum"
                | "impl"
                | "mod"
                | "use"
                | "pub"
                | "let"
                | "mut"
                | "match"
                | "if"
                | "else"
                | "for"
                | "while"
                | "loop"
                | "return"
                | "async"
                | "await"
                | "crate"
                | "super"
                | "self"
                | "Self"
                | "const"
                | "static"
                | "trait"
                | "type"
        ),
        Language::Python => matches!(
            word,
            "def"
                | "class"
                | "import"
                | "from"
                | "as"
                | "if"
                | "elif"
                | "else"
                | "for"
                | "while"
                | "return"
                | "yield"
                | "async"
                | "await"
                | "with"
                | "try"
                | "except"
                | "finally"
                | "lambda"
                | "pass"
                | "break"
                | "continue"
        ),
        Language::JavaScript | Language::TypeScript => matches!(
            word,
            "function"
                | "class"
                | "import"
                | "export"
                | "const"
                | "let"
                | "var"
                | "if"
                | "else"
                | "for"
                | "while"
                | "return"
                | "async"
                | "await"
                | "try"
                | "catch"
                | "finally"
                | "switch"
                | "case"
                | "break"
                | "continue"
                | "new"
        ),
        Language::Shell => matches!(
            word,
            "function"
                | "if"
                | "then"
                | "fi"
                | "for"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "in"
                | "while"
                | "until"
                | "return"
                | "local"
        ),
        Language::Markdown
        | Language::Text
        | Language::Toml
        | Language::Nix
        | Language::Just
        | Language::Elisp
        | Language::Unknown => false,
    };

    if is_keyword {
        TokenKind::Keyword
    } else {
        TokenKind::Base
    }
}

fn style_for_token(token: &TokenKind, palette: &UiPalette) -> Style {
    match token {
        TokenKind::Base => Style::default().fg(palette.code_fg).bg(palette.code_bg),
        TokenKind::Keyword => Style::default().fg(palette.keyword).bg(palette.code_bg),
        TokenKind::String => Style::default().fg(palette.string).bg(palette.code_bg),
        TokenKind::Number => Style::default().fg(palette.number).bg(palette.code_bg),
        TokenKind::Comment => Style::default().fg(palette.dim).bg(palette.code_bg),
    }
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
    fn navigator_moves_within_parent_siblings() {
        let block_a = build_block("hash-a", BlockKind::Function, 0);
        let block_b = build_block("hash-b", BlockKind::Function, 2);
        let block_c = build_block("hash-c", BlockKind::Function, 4);
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
        builder.add_block(
            file_a,
            block_b.kind.as_str().to_string(),
            "alpha/a.rs".to_string(),
            block_b,
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
            block_c.kind.as_str().to_string(),
            "beta/b.rs".to_string(),
            block_c,
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
            .node_by_path_and_hash("alpha/a.rs", "hash-b")
            .expect("block b");
        navigator.set_current(block_b_id);
        navigator.move_prev();
        let current = navigator.current_id();
        let current_node = navigator.tree.node(current);
        assert_eq!(current_node.hash, "hash-a");
        navigator.move_prev();
        let current = navigator.current_id();
        let current_node = navigator.tree.node(current);
        assert_eq!(current_node.hash, "hash-a");
    }

    #[test]
    fn navigator_next_after_approval_prefers_next_sibling() {
        let block_a = build_block("hash-a", BlockKind::Function, 0);
        let block_b = build_block("hash-b", BlockKind::Function, 2);
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
        builder.add_block(
            file_a,
            block_b.kind.as_str().to_string(),
            "alpha/a.rs".to_string(),
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
        let navigator = ReviewNavigator::new(tree, block_nodes).expect("navigator");
        let block_a_id = navigator
            .tree
            .node_by_path_and_hash("alpha/a.rs", "hash-a")
            .expect("block a");
        let block_b_id = navigator
            .tree
            .node_by_path_and_hash("alpha/a.rs", "hash-b")
            .expect("block b");
        let next_from_a = navigator.next_after_approval_key(block_a_id);
        assert_eq!(next_from_a, Some(NodeKey::Block("hash-b".to_string())));
        let next_from_b = navigator.next_after_approval_key(block_b_id);
        assert_eq!(next_from_b, Some(NodeKey::File("alpha/a.rs".to_string())));
    }
}
