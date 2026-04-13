use super::{
    AppState, EditingKeyAction, Event, InputMode, KeyCode, KeyEvent, KeyEventKind, KeybindAction,
    Rect, SessionRecap, SpeedReadController, TuiDiffLineNumbers, TuiKeybindsConfig,
    TuiSpeedReadConfig, UiMode, ViewMode, clear_editing_validation, clear_focus_scroll,
    clear_speed_read_if_not_on_current_node, current_ui_mode, editing_key_action_for_event,
    execute_action_with, handle_child, handle_confirm_cancel, handle_editing_cancel,
    handle_editing_submit_with, handle_mouse_event, handle_next, handle_note_action, handle_parent,
    handle_paste_event, handle_prev, handle_scroll_line_down, handle_scroll_line_up,
    handle_scroll_page_down, handle_scroll_page_up, handle_speed_read_key_binding,
    key_code_accepts_repeat_in_normal_mode, key_event_for_press_event,
    key_event_for_press_or_repeat_event, keybind_action_accepts_repeat,
    keybind_action_for_key_code, set_focus_for_current_node, should_rerender_on_event,
    toggle_speed_read_mode, ui, vcs,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::commands::mark;
use crate::review_navigator::ReviewNavigator;
use crate::review_order::ReviewOrder;
use crate::review_scope::ReviewScope;
use crate::store::Verdict;
use crate::tree::TreeBuilder;
use anyhow::{Result, bail};
use ratatui::{Terminal, backend::Backend};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedMarkAction {
    pub fingerprint: String,
    pub verdict: Verdict,
    pub check: String,
    pub note: Option<String>,
    pub path: Option<String>,
    pub line: Option<u32>,
}

impl From<mark::MarkParams> for ScriptedMarkAction {
    fn from(params: mark::MarkParams) -> Self {
        Self {
            fingerprint: params.fingerprint,
            verdict: params.verdict,
            check: params.check,
            note: params.note,
            path: params.path,
            line: params.line,
        }
    }
}

pub trait MarkActionRunner {
    fn run_mark(&mut self, action: &ScriptedMarkAction) -> Result<()>;
}

pub struct CliMarkActionRunner {
    bin_path: PathBuf,
    repo_root: PathBuf,
}

impl CliMarkActionRunner {
    pub fn new(bin_path: impl AsRef<Path>, repo_root: impl AsRef<Path>) -> Self {
        Self {
            bin_path: bin_path.as_ref().to_path_buf(),
            repo_root: repo_root.as_ref().to_path_buf(),
        }
    }
}

impl MarkActionRunner for CliMarkActionRunner {
    fn run_mark(&mut self, action: &ScriptedMarkAction) -> Result<()> {
        let mut command = Command::new(&self.bin_path);
        command
            .current_dir(&self.repo_root)
            .arg("mark")
            .arg("--fingerprint")
            .arg(&action.fingerprint)
            .arg("--verdict")
            .arg(action.verdict.as_str())
            .arg("--check")
            .arg(&action.check)
            .arg("--quiet");

        if let Some(note) = &action.note {
            command.arg("--note").arg(note);
        }
        if let Some(path) = &action.path {
            command.arg("--path").arg(path);
        }
        if let Some(line) = action.line {
            command.arg("--line").arg(line.to_string());
        }

        let output = command.output()?;
        if !output.status.success() {
            bail!(
                "trueflow mark failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
}

/// Hidden test support for scripted TUI rendering/integration tests.
pub struct ScriptedTui<B>
where
    B: Backend<Error = std::io::Error>,
{
    terminal: Terminal<B>,
    state: AppState,
    mark_action_runner: Option<Box<dyn MarkActionRunner>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptedSessionRecap {
    pub comments: usize,
    pub blocks_touched: usize,
}

impl<B> ScriptedTui<B>
where
    B: Backend<Error = std::io::Error>,
{
    pub fn with_root_files<I, S>(backend: B, file_names: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let terminal = Terminal::new(backend)?;
        let state = build_root_state(file_names)?;
        Ok(Self {
            terminal,
            state,
            mark_action_runner: None,
        })
    }

    pub fn with_single_rust_block_file(
        backend: B,
        repo_path: impl Into<String>,
        file_content: &str,
        block_content: &str,
        block_start_line: usize,
        block_end_line: usize,
    ) -> Result<Self> {
        let terminal = Terminal::new(backend)?;
        let repo_path = repo_path.into();
        let state = build_state_with_single_rust_block_file(
            &repo_path,
            file_content,
            block_content,
            block_start_line,
            block_end_line,
        )?;
        Ok(Self {
            terminal,
            state,
            mark_action_runner: None,
        })
    }

    pub fn render(&mut self) -> Result<()> {
        self.terminal.draw(|frame| ui(frame, &mut self.state))?;
        Ok(())
    }

    pub fn show_diff(&mut self) {
        self.state.view_mode = ViewMode::Diff;
        let preferred_focus = self.state.focus_block;
        set_focus_for_current_node(&mut self.state, preferred_focus);
        self.state.content_frame_cache.clear();
    }

    pub fn preload_text_diff(
        &mut self,
        repo_path: impl Into<String>,
        hunks: Vec<vcs::DiffHunk>,
    ) -> Result<()> {
        let repo_path = repo_path.into();
        self.state.file_diff_cache.insert(
            PathBuf::from(&repo_path),
            vcs::FileDiff::Text {
                path: crate::repo_path::RepoPath::new(&repo_path)?,
                hunks,
            },
        );
        self.state.content_frame_cache.clear();
        Ok(())
    }

    pub fn send_key(&mut self, key_event: KeyEvent) -> Result<()> {
        let event = Event::Key(key_event);
        self.apply_event(&event)
    }

    pub fn open_note_overlay(&mut self) -> Result<()> {
        handle_note_action(&mut self.state)
    }

    pub fn send_paste(&mut self, pasted: impl Into<String>) -> Result<()> {
        self.apply_event(&Event::Paste(pasted.into()))
    }

    pub fn backend(&self) -> &B {
        self.terminal.backend()
    }

    pub fn install_mark_action_runner<R>(&mut self, runner: R)
    where
        R: MarkActionRunner + 'static,
    {
        self.mark_action_runner = Some(Box::new(runner));
    }

    pub fn is_editing(&self) -> bool {
        matches!(self.state.input_mode, InputMode::Editing { .. })
    }

    pub fn scroll_offset(&self) -> u16 {
        self.state.scroll_offset
    }

    pub fn input_buffer(&self) -> &str {
        &self.state.input_buffer
    }

    pub fn remaining_blocks(&self) -> usize {
        self.state.remaining_blocks
    }

    pub fn session_recap(&self) -> ScriptedSessionRecap {
        ScriptedSessionRecap {
            comments: self.state.session_recap.comments,
            blocks_touched: self.state.session_recap.blocks_touched,
        }
    }

    pub fn root_cursor_label(&self) -> Option<String> {
        let id = self.state.root_cursor?;
        Some(self.state.navigator.tree.node(id).name.clone())
    }

    pub fn is_at_root(&self) -> bool {
        self.state.navigator.current_id() == self.state.navigator.tree.root()
    }

    pub fn is_speed_read_active(&self) -> bool {
        matches!(current_ui_mode(&self.state), UiMode::SpeedRead)
    }

    pub fn speed_read_playback(&self) -> Option<crate::review_speedread::PlaybackState> {
        self.state
            .speed_read
            .active_for(self.state.navigator.current_id())
            .map(|mode| mode.model.playback)
    }

    fn apply_event(&mut self, event: &Event) -> Result<()> {
        if should_rerender_on_event(event) {
            return Ok(());
        }

        if let Event::Paste(pasted) = event {
            let _ = handle_paste_event(&mut self.state, pasted);
            return Ok(());
        }

        if let Event::Mouse(mouse_event) = event {
            let _ = handle_mouse_event(&mut self.state, *mouse_event);
            return Ok(());
        }

        let input_mode = match self.state.input_mode {
            InputMode::Normal => TestInputMode::Normal,
            InputMode::Editing { .. } => TestInputMode::Editing,
            InputMode::ConfirmBatch { .. } => TestInputMode::ConfirmBatch,
        };

        match input_mode {
            TestInputMode::Normal => self.apply_normal_mode_event(event),
            TestInputMode::Editing => self.apply_editing_mode_event(event),
            TestInputMode::ConfirmBatch => self.apply_confirm_batch_event(event),
        }
    }

    fn apply_normal_mode_event(&mut self, event: &Event) -> Result<()> {
        let Some(key_event) = key_event_for_press_or_repeat_event(event) else {
            return Ok(());
        };
        let key_code = key_event.code;
        let ui_mode = current_ui_mode(&self.state);

        if matches!(ui_mode, UiMode::Recap) {
            return Ok(());
        }

        if matches!(ui_mode, UiMode::SpeedRead)
            && key_event.kind == KeyEventKind::Press
            && handle_speed_read_key_binding(&mut self.state, key_code)
        {
            return Ok(());
        }

        if let Some(action) = keybind_action_for_key_code(&self.state.keybinds, key_code) {
            if key_event.kind == KeyEventKind::Repeat && !keybind_action_accepts_repeat(action) {
                return Ok(());
            }

            match action {
                KeybindAction::Up => handle_scroll_line_up(&mut self.state),
                KeybindAction::Down => handle_scroll_line_down(&mut self.state),
                KeybindAction::Prev => handle_prev(&mut self.state),
                KeybindAction::Next => handle_next(&mut self.state),
                KeybindAction::Parent => handle_parent(&mut self.state),
                KeybindAction::Child => handle_child(&mut self.state),
                KeybindAction::Approve => {
                    bail!("approve is not supported in the scripted test harness")
                }
                KeybindAction::Note => handle_note_action(&mut self.state)?,
                KeybindAction::ToggleView => {
                    self.state.view_mode = match self.state.view_mode {
                        ViewMode::Source => ViewMode::Diff,
                        ViewMode::Diff => ViewMode::Source,
                    };
                    let preferred_focus = self.state.focus_block;
                    set_focus_for_current_node(&mut self.state, preferred_focus);
                }
                KeybindAction::SpeedRead => toggle_speed_read_mode(&mut self.state),
                KeybindAction::Root => {
                    self.state.navigator.jump_root();
                    clear_focus_scroll(&mut self.state);
                    clear_speed_read_if_not_on_current_node(&mut self.state);
                }
                KeybindAction::Quit => {}
            }
            return Ok(());
        }

        if key_event.kind == KeyEventKind::Repeat
            && !key_code_accepts_repeat_in_normal_mode(key_code)
        {
            return Ok(());
        }

        match key_code {
            KeyCode::Char(' ') | KeyCode::PageDown => handle_scroll_page_down(&mut self.state),
            KeyCode::PageUp => handle_scroll_page_up(&mut self.state),
            KeyCode::Home => self.state.scroll_offset = 0,
            KeyCode::End => {
                self.state.scroll_offset = self
                    .state
                    .content_height
                    .saturating_sub(self.state.viewport_height);
            }
            KeyCode::Enter
                if key_event.kind == KeyEventKind::Press
                    && self.state.navigator.current_id() == self.state.navigator.tree.root() =>
            {
                handle_child(&mut self.state);
            }
            _ => {}
        }

        Ok(())
    }

    fn apply_editing_mode_event(&mut self, event: &Event) -> Result<()> {
        let Some(key_event) = key_event_for_press_or_repeat_event(event) else {
            return Ok(());
        };

        match editing_key_action_for_event(&key_event) {
            EditingKeyAction::Submit => {
                let runner = &mut self.mark_action_runner;
                handle_editing_submit_with(&mut self.state, |action, state| {
                    let Some(runner) = runner.as_mut() else {
                        bail!("submit is not supported in the scripted test harness")
                    };
                    execute_action_with(state, action, |params| {
                        let scripted_action = ScriptedMarkAction::from(params);
                        runner.run_mark(&scripted_action)
                    })
                })?;
            }
            EditingKeyAction::InsertNewline => {
                clear_editing_validation(&mut self.state);
                self.state.input_buffer.push('\n');
            }
            EditingKeyAction::Cancel => handle_editing_cancel(&mut self.state),
            EditingKeyAction::Backspace => {
                clear_editing_validation(&mut self.state);
                self.state.input_buffer.pop();
            }
            EditingKeyAction::InsertChar(c) => {
                clear_editing_validation(&mut self.state);
                self.state.input_buffer.push(c);
            }
            EditingKeyAction::Ignore => {}
        }

        Ok(())
    }

    fn apply_confirm_batch_event(&mut self, event: &Event) -> Result<()> {
        let Some(key_event) = key_event_for_press_event(event) else {
            return Ok(());
        };

        match key_event.code {
            KeyCode::Esc => handle_confirm_cancel(&mut self.state),
            KeyCode::Enter => {
                bail!("confirm batch submit is not supported in the scripted test harness")
            }
            _ => {}
        }

        Ok(())
    }
}

enum TestInputMode {
    Normal,
    Editing,
    ConfirmBatch,
}

fn build_root_state<I, S>(file_names: I) -> Result<AppState>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut builder = TreeBuilder::new();
    let root = builder.root();
    let mut visible = HashSet::new();
    let mut root_cursor = None;

    for name in file_names {
        let name = name.into();
        let file = builder.add_file(
            root,
            name.clone(),
            name.clone(),
            format!("{name}-hash"),
            Language::Rust,
        );
        root_cursor.get_or_insert(file);
        let block_id = builder.add_block(
            file,
            format!("{}-block", name.trim_end_matches(".rs")),
            name,
            Block::new("fn item() {}\n".to_string(), BlockKind::Function, 0, 1),
            Language::Rust,
        );
        visible.insert(block_id);
    }

    let tree = builder.finalize();
    let review_order = ReviewOrder::from_tree(&tree, &visible);
    let mut navigator = ReviewNavigator::new(tree, visible.clone())?;
    navigator.jump_root();

    Ok(AppState {
        review_scope: ReviewScope::All,
        navigator,
        review_order,
        total_blocks: visible.len(),
        initial_remaining_blocks: visible.len(),
        remaining_blocks: visible.len(),
        reviewable_nodes: visible,
        diff_block_sides: HashMap::new(),
        session_recap: SessionRecap::default(),
        scope_label: "All".to_string(),
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        editing_validation: None,
        confirm_batch: false,
        repo_name: "repo".to_string(),
        workdir_prefix: None,
        file_cache: HashMap::new(),
        root_cursor,
        focus_block: None,
        pending_focus_scroll: false,
        scroll_offset: 0,
        content_height: 0,
        viewport_height: 0,
        code_rect: Rect::default(),
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
    })
}

fn build_state_with_single_rust_block_file(
    repo_path: &str,
    file_content: &str,
    block_content: &str,
    block_start_line: usize,
    block_end_line: usize,
) -> Result<AppState> {
    let file_name = PathBuf::from(repo_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fixture.rs")
        .to_string();

    let mut builder = TreeBuilder::new();
    let root = builder.root();
    let file = builder.add_file(
        root,
        file_name,
        repo_path.to_string(),
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
        repo_path.to_string(),
        block,
        Language::Rust,
    );

    let tree = builder.finalize();
    let visible = HashSet::from([block_id]);
    let review_order = ReviewOrder::from_tree(&tree, &visible);
    let mut navigator = ReviewNavigator::new(tree, visible.clone())?;
    navigator.set_current(block_id);

    Ok(AppState {
        review_scope: ReviewScope::All,
        navigator,
        review_order,
        total_blocks: 1,
        initial_remaining_blocks: 1,
        remaining_blocks: 1,
        reviewable_nodes: visible,
        diff_block_sides: HashMap::new(),
        session_recap: SessionRecap::default(),
        scope_label: "All".to_string(),
        input_mode: InputMode::Normal,
        input_buffer: String::new(),
        editing_validation: None,
        confirm_batch: false,
        repo_name: "repo".to_string(),
        workdir_prefix: None,
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
        focus_block: Some(block_id),
        pending_focus_scroll: false,
        scroll_offset: 0,
        content_height: 0,
        viewport_height: 0,
        code_rect: Rect::default(),
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
    })
}
