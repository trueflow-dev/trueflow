#![cfg(feature = "tui-test-support")]

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::fs;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use vt100::Parser;

use trueflow_test_support::{TestRepo, read_review_records, run_git_output};

const TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV: &str = "TRUEFLOW_TUI_KEYBOARD_ENHANCEMENT_PROBE";
const TUI_KEYBOARD_ENHANCEMENT_PROBE_SKIP_VALUE: &str = "skip";

fn tui_pty_environment() -> [(&'static str, &'static str); 2] {
    [
        ("TERM", "xterm-256color"),
        (
            TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV,
            TUI_KEYBOARD_ENHANCEMENT_PROBE_SKIP_VALUE,
        ),
    ]
}

fn configure_tui_pty_command(cmd: &mut CommandBuilder, repo: &TestRepo) {
    cmd.env("HOME", &repo.path);
    for (key, value) in tui_pty_environment() {
        cmd.env(key, value);
    }
}

struct PtyOutput {
    state: Mutex<PtyOutputState>,
    changed: Condvar,
}

struct PtyOutputState {
    bytes: Vec<u8>,
    generation: u64,
    reader_done: bool,
}

impl PtyOutput {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PtyOutputState {
                bytes: Vec::new(),
                generation: 0,
                reader_done: false,
            }),
            changed: Condvar::new(),
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PtyOutputState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn append(&self, bytes: &[u8]) {
        let mut state = self.lock_state();
        state.bytes.extend_from_slice(bytes);
        state.generation += 1;
        self.changed.notify_all();
    }

    fn mark_reader_done(&self) {
        let mut state = self.lock_state();
        state.reader_done = true;
        state.generation += 1;
        self.changed.notify_all();
    }

    fn snapshot(&self) -> (Vec<u8>, u64) {
        let state = self.lock_state();
        (state.bytes.clone(), state.generation)
    }

    fn wait_for_change_or_deadline(&self, generation: u64, deadline: Instant) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }

        let state = self.lock_state();
        if state.generation != generation || state.reader_done {
            return;
        }

        let timeout = deadline.saturating_duration_since(now);
        let _guard = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.generation == generation && !state.reader_done
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn captured_output(output: &Arc<PtyOutput>) -> String {
    let (bytes, _) = output.snapshot();
    String::from_utf8_lossy(&bytes).to_string()
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    output: Arc<PtyOutput>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => output.append(&buf[..count]),
                Err(_) => break,
            }
        }
        output.mark_reader_done();
    })
}

fn wait_for_output(
    output: &Arc<PtyOutput>,
    needle: &str,
    timeout: Duration,
    child: &mut (dyn Child + Send + Sync),
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let (bytes, generation) = output.snapshot();
        let current = String::from_utf8_lossy(&bytes).to_string();
        if current.contains(needle) {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "trueflow tui exited before PTY output contained {needle:?}: {status}; output: {current}"
            );
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for PTY output to contain {needle:?}; output: {current}");
        }
        output.wait_for_change_or_deadline(generation, deadline);
    }
}

fn parsed_screen_contents(output: &Arc<PtyOutput>, rows: u16, cols: u16) -> String {
    let (bytes, _) = output.snapshot();
    let mut parser = Parser::new(rows, cols, 0);
    parser.process(&bytes);
    parser.screen().contents()
}

fn wait_for_screen_predicate<F>(
    output: &Arc<PtyOutput>,
    rows: u16,
    cols: u16,
    description: &str,
    timeout: Duration,
    child: &mut (dyn Child + Send + Sync),
    predicate: F,
) -> Result<()>
where
    F: Fn(&str) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let (bytes, generation) = output.snapshot();
        let mut parser = Parser::new(rows, cols, 0);
        parser.process(&bytes);
        let screen = parser.screen().contents();
        if predicate(&screen) {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "trueflow tui exited before PTY screen {description}: {status}; screen: {screen}"
            );
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for PTY screen {description}; screen: {screen}");
        }
        output.wait_for_change_or_deadline(generation, deadline);
    }
}

fn send_and_flush(writer: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

fn alternate_screen_leave_count(output: &Arc<PtyOutput>) -> usize {
    let (bytes, _) = output.snapshot();
    bytes
        .windows(b"\x1b[?1049l".len())
        .filter(|window| *window == b"\x1b[?1049l")
        .count()
}

fn write_fake_noninteractive_gpg(repo: &TestRepo) -> Result<String> {
    let bin_dir = repo
        .path
        .parent()
        .unwrap_or(repo.path.as_path())
        .join("fake-bin");
    fs::create_dir_all(&bin_dir)?;
    let gpg_path = bin_dir.join("gpg");
    fs::write(
        &gpg_path,
        r#"#!/bin/sh
case " $* " in
  *" --export "*) printf '%s\n' 'FAKE PUBLIC KEY'; exit 0 ;;
  *) cat >/dev/null; printf '%s\n' 'FAKE SIGNATURE'; exit 0 ;;
esac
"#,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gpg_path, fs::Permissions::from_mode(0o755))?;
    }

    let path = std::env::var("PATH").unwrap_or_default();
    Ok(format!("{}:{path}", bin_dir.display()))
}

fn deeply_nested_rust_function(depth: usize) -> String {
    let mut content = String::from("pub fn nested(mut value: i32) -> i32 {\n");
    for level in 1..=depth {
        content.push_str(&format!("if value >= {level} {{\n"));
    }
    content.push_str("value += 1;\n");
    for _ in 0..depth {
        content.push_str("}\n");
    }
    content.push_str("value\n}\n");
    content
}

fn wait_for_child_success(
    child: &mut (dyn Child + Send + Sync),
    output: &Arc<PtyOutput>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let output = captured_output(output);
                bail!("trueflow tui exited unsuccessfully: {status}; output: {output}");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = captured_output(output);
            bail!("timed out waiting for TUI PTY smoke test to exit; output: {output}");
        }
        let (_, generation) = output.snapshot();
        output.wait_for_change_or_deadline(generation, deadline);
    }
}

#[test]
fn pty_smoke_scope_selector_prechecks_deep_commit_without_stack_overflow() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = TestRepo::new("tui_pty_smoke_scope_selector_deep_commit")?;
    repo.write("src/lib.rs", &deeply_nested_rust_function(8_000))?;
    repo.commit_all("add deeply nested function")?;

    let rows = 24;
    let cols = 100;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.cwd(&repo.path);
    configure_tui_pty_command(&mut cmd, &repo);

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let output = PtyOutput::new();
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&output));

    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to show the scope selector",
        Duration::from_secs(10),
        &mut *child,
        |screen| screen.contains("Select review scope"),
    )?;
    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to finish commit precheck",
        Duration::from_secs(20),
        &mut *child,
        |screen| screen.contains("Commit ") && !screen.contains("[checking...]"),
    )?;

    send_and_flush(&mut *writer, b"q")?;
    drop(writer);
    wait_for_child_success(&mut *child, &output)?;

    if let Err(_panic) = reader_thread.join() {
        bail!("reader thread panicked");
    }

    Ok(())
}

#[test]
fn pty_smoke_scope_selector_scrolls_past_first_commit_page() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = TestRepo::new("tui_pty_smoke_scope_selector_deep_history")?;
    let commit_count = 65;
    for index in 1..=commit_count {
        repo.write(
            "src/lib.rs",
            &format!("pub fn value() -> u32 {{ {index} }}\n"),
        )?;
        repo.commit_all(&format!("history {index:03}"))?;
    }

    let rows = 24;
    let cols = 100;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.cwd(&repo.path);
    configure_tui_pty_command(&mut cmd, &repo);

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let output = PtyOutput::new();
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&output));

    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to show the scope selector",
        Duration::from_secs(10),
        &mut *child,
        |screen| screen.contains("Select review scope"),
    )?;

    // Move well past the first page of loaded commits, down to the root commit.
    for _ in 0..commit_count + 5 {
        send_and_flush(&mut *writer, b"\x1b[B")?;
    }
    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to select the root commit",
        Duration::from_secs(20),
        &mut *child,
        |screen| {
            screen
                .lines()
                .any(|line| line.contains("> Commit") && line.contains("history 001"))
        },
    )?;
    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to check the status of the visible root commit",
        Duration::from_secs(20),
        &mut *child,
        |screen| {
            screen
                .lines()
                .any(|line| line.contains("history 001") && line.contains("left]"))
        },
    )?;

    send_and_flush(&mut *writer, b"q")?;
    drop(writer);
    wait_for_child_success(&mut *child, &output)?;

    if let Err(_panic) = reader_thread.join() {
        bail!("reader thread panicked");
    }

    Ok(())
}

#[test]
fn pty_smoke_commit_diff_deep_nesting_does_not_stack_overflow() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = TestRepo::new("tui_pty_smoke_deep_nesting")?;
    repo.write("src/lib.rs", &deeply_nested_rust_function(4_000))?;
    repo.commit_all("add deeply nested function")?;
    let revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let target = format!("rev:{}", revision.trim());

    let rows = 24;
    let cols = 100;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.arg("--target");
    cmd.arg(target);
    cmd.cwd(&repo.path);
    configure_tui_pty_command(&mut cmd, &repo);

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let output = PtyOutput::new();
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&output));

    wait_for_output(&output, "Diff Mode", Duration::from_secs(15), &mut *child)?;

    send_and_flush(&mut *writer, b"q")?;
    drop(writer);
    wait_for_child_success(&mut *child, &output)?;

    if let Err(_panic) = reader_thread.join() {
        bail!("reader thread panicked");
    }

    Ok(())
}

#[test]
fn pty_smoke_signed_action_with_noninteractive_gpg_does_not_suspend_terminal() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = TestRepo::new("tui_pty_smoke_signed_no_suspend")?;
    repo.git(&["config", "user.signingkey", "ABC123"])?;
    repo.write("src/lib.rs", "fn demo() {\n    work();\n}\n")?;
    let path = write_fake_noninteractive_gpg(&repo)?;

    let rows = 20;
    let cols = 80;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.arg("--all");
    cmd.cwd(&repo.path);
    configure_tui_pty_command(&mut cmd, &repo);
    cmd.env("PATH", path);

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let output = PtyOutput::new();
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&output));

    wait_for_output(&output, "Mode", Duration::from_secs(10), &mut *child)?;
    send_and_flush(&mut *writer, b"a")?;
    wait_for_output(
        &output,
        "review something else",
        Duration::from_secs(10),
        &mut *child,
    )?;
    send_and_flush(&mut *writer, b"q")?;
    drop(writer);
    wait_for_child_success(&mut *child, &output)?;

    if let Err(_panic) = reader_thread.join() {
        bail!("reader thread panicked");
    }

    assert_eq!(
        alternate_screen_leave_count(&output),
        1,
        "signed non-interactive approval should only leave alternate screen on final TUI exit"
    );

    Ok(())
}

#[test]
fn pty_smoke_diff_mode_keeps_wrapped_rows_readable_in_narrow_terminal() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = TestRepo::new("tui_pty_smoke_diff_wrap")?;
    repo.git(&["checkout", "-b", "main"])?;
    repo.write("src/lib.rs", "fn demo() {\n    let value = \"short\";\n}\n")?;
    repo.add("src/lib.rs")?;
    repo.commit("add demo")?;
    repo.git(&["checkout", "-b", "feature"])?;
    repo.write(
        "src/lib.rs",
        "fn demo() {\n    let value = \"this_is_a_very_long_changed_line_that_should_wrap_in_a_narrow_terminal\";\n}\n",
    )?;
    repo.add("src/lib.rs")?;
    repo.commit("long change")?;

    let rows = 20;
    let cols = 12;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.arg("--all");
    cmd.cwd(&repo.path);
    configure_tui_pty_command(&mut cmd, &repo);

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let output = PtyOutput::new();
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&output));

    wait_for_output(&output, "Diff Mode", Duration::from_secs(5), &mut *child)?;
    let screen = parsed_screen_contents(&output, rows, cols);
    assert!(
        screen.lines().any(|row| row.starts_with('-')),
        "expected a compact removed diff row in a narrow terminal:\n{screen}"
    );
    assert!(
        screen.lines().any(|row| row.starts_with('+')),
        "expected a compact added diff row in a narrow terminal:\n{screen}"
    );
    assert!(
        screen.lines().all(|row| row.trim() != "2"),
        "did not expect narrow diff rows to split line-number gutters across rows:\n{screen}"
    );

    send_and_flush(&mut *writer, b"q")?;
    drop(writer);
    wait_for_child_success(&mut *child, &output)?;

    if let Err(_panic) = reader_thread.join() {
        bail!("reader thread panicked");
    }

    Ok(())
}

#[test]
fn pty_smoke_ctrl_j_submits_multiline_note() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = TestRepo::new("tui_pty_smoke")?;
    repo.write("src/lib.rs", "fn demo() {\n    work();\n}\n")?;
    repo.add("src/lib.rs")?;
    repo.commit("add demo")?;

    let rows = 30;
    let cols = 120;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.arg("--all");
    cmd.cwd(&repo.path);
    configure_tui_pty_command(&mut cmd, &repo);

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    let output = PtyOutput::new();
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&output));

    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to show initial progress counts",
        Duration::from_secs(10),
        &mut *child,
        |screen| screen.contains("0 reviewed · 0 commented · 1 remaining"),
    )?;
    send_and_flush(&mut *writer, b"c")?;
    wait_for_output(&output, "Type a note", Duration::from_secs(5), &mut *child)?;
    send_and_flush(&mut *writer, b"a")?;
    send_and_flush(&mut *writer, b"\n")?;
    send_and_flush(&mut *writer, b"b")?;
    send_and_flush(&mut *writer, b"\r")?;
    wait_for_screen_predicate(
        &output,
        rows,
        cols,
        "to close the note overlay after submit",
        Duration::from_secs(5),
        &mut *child,
        |screen| !screen.contains("┌ Note"),
    )?;
    send_and_flush(&mut *writer, b"q")?;
    drop(writer);
    wait_for_child_success(&mut *child, &output)?;

    if let Err(_panic) = reader_thread.join() {
        bail!("reader thread panicked");
    }

    let records = read_review_records(&repo.path.join(".trueflow").join("reviews.jsonl"))?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].note.as_deref(), Some("a\nb"));

    let output = captured_output(&output);
    assert!(
        !output.is_empty(),
        "expected PTY smoke test to capture some terminal output"
    );

    Ok(())
}

struct FocusPty {
    child: Box<dyn Child + Send + Sync>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<PtyOutput>,
    reader: Option<thread::JoinHandle<()>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl FocusPty {
    const ROWS: u16 = 24;
    const COLS: u16 = 120;

    fn open(repo: &TestRepo) -> Result<Self> {
        Self::open_with(repo, &["focus", "--plan", "focus.json", "--duration", "5m"])
    }

    fn open_with(repo: &TestRepo, args: &[&str]) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: Self::ROWS,
            cols: Self::COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
        cmd.args(args);
        cmd.cwd(&repo.path);
        configure_tui_pty_command(&mut cmd, repo);
        // Acquire fallible handles before spawning so setup errors cannot orphan a child.
        let writer = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let output = PtyOutput::new();
        let reader = spawn_reader_thread(reader, Arc::clone(&output));
        Ok(Self {
            child,
            writer: Some(writer),
            output,
            reader: Some(reader),
            _master: pair.master,
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        send_and_flush(
            &mut **self.writer.as_mut().context("PTY input closed")?,
            bytes,
        )
    }

    fn screen(&self) -> String {
        parsed_screen_contents(&self.output, Self::ROWS, Self::COLS)
    }

    fn wait_screen(&mut self, description: &str, predicate: impl Fn(&str) -> bool) -> Result<()> {
        wait_for_screen_predicate(
            &self.output,
            Self::ROWS,
            Self::COLS,
            description,
            Duration::from_secs(10),
            &mut *self.child,
            predicate,
        )
    }

    fn finish(&mut self) -> Result<()> {
        self.writer.take();
        wait_for_child_success(&mut *self.child, &self.output)?;
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_panic| anyhow::anyhow!("PTY reader panicked"))?;
        }
        assert!(
            alternate_screen_leave_count(&self.output) > 0,
            "focus must restore the terminal's main screen on exit"
        );
        let (bytes, _) = self.output.snapshot();
        let mut parser = Parser::new(Self::ROWS, Self::COLS, 0);
        parser.process(&bytes);
        assert!(
            !parser.screen().alternate_screen(),
            "focus left the terminal in the alternate screen"
        );
        assert!(
            !parser.screen().hide_cursor(),
            "focus left the terminal cursor hidden"
        );
        Ok(())
    }
}

impl Drop for FocusPty {
    fn drop(&mut self) {
        self.writer.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn pty_focus_preserves_selected_order_evidence_approvals_and_resume_scope() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    let repo = TestRepo::new("focus_pty")?;
    repo.write(
        "trueflow.toml",
        "[scan]\nuse_cache = false\nwrite_cache = false\n[ai]\nmode = \"off\"\n",
    )?;
    let names = ["a_unselected", "b_second", "c_third", "z_first"];
    for name in names {
        repo.write(
            &format!("src/{name}.rs"),
            &format!("pub fn {name}(balance: &mut u64) {{ *balance += 1; }}\n"),
        )?;
    }
    let home = repo.path.to_str().context("home path")?;
    let inventory = trueflow_test_support::json(&repo.run_with_env(
        &["focus", "--candidates", "--path", "src"],
        &[("HOME", home)],
    )?)?;
    let candidates = inventory["candidates"]
        .as_array()
        .context("focus candidates")?;
    let selected: Vec<_> = ["z_first", "b_second", "c_third"]
        .into_iter()
        .map(|name| {
            candidates
                .iter()
                .find(|candidate| candidate["target"]["path"] == format!("src/{name}.rs"))
                .with_context(|| format!("complete candidate for {name}"))
        })
        .collect::<Result<_>>()?;
    let evidence_id = selected[0]["evidence"][0]["id"]
        .as_str()
        .context("evidence id")?;
    let provenance: String = selected[0]["evidence"][0]["provenance"]
        .as_str()
        .context("evidence provenance")?
        .split_whitespace()
        .collect();
    let uncertainty: String = (0..20)
        .map(|n| format!("detail_context_{n:02}\n"))
        .collect();
    let steps: Vec<_> = selected
        .iter()
        .map(|candidate| {
            serde_json::json!({
                "target": candidate["target"],
                "reason": "Inspect arithmetic at the public boundary.",
                "evidence": [candidate["evidence"][0]["id"]],
                "uncertainty": uncertainty,
                "checks": ["Could balance overflow?", "Which callers constrain the input?"]
            })
        })
        .collect();
    let plan = serde_json::json!({
        "version": 1, "duration_seconds": 300, "title": "Selected balance checks",
        "description": "Read selected arithmetic only; no safety certification.", "steps": steps
    });
    repo.write("focus.json", &plan.to_string())?;
    let onboard = trueflow_test_support::json(&repo.run_with_env(
        &["onboard", "--candidates", "--path", "src"],
        &[("HOME", home)],
    )?)?;
    let first_fingerprint = onboard["candidates"]
        .as_array()
        .context("onboard candidates")?
        .iter()
        .find(|candidate| candidate["target"] == selected[0]["target"])
        .context("first fingerprint candidate")?["fingerprint"]
        .as_str()
        .context("fingerprint")?;

    let mut session = FocusPty::open(&repo)?;
    session.wait_screen(
        "to show the first plan target rather than filesystem order",
        |screen| {
            screen.contains("pub fn z_first")
                && !screen.contains("pub fn a_unselected")
                && !screen.contains("pub fn b_second")
        },
    )?;
    session.send(b"\x1bOP")?; // xterm F1
    session.wait_screen("to open full focus details", |screen| {
        screen.contains("detail_context_00")
    })?;
    assert!(
        !session.screen().contains(evidence_id),
        "fixture must require scrolling to reach evidence"
    );
    for _ in 0..80 {
        if session
            .screen()
            .split_whitespace()
            .collect::<String>()
            .contains(&provenance)
        {
            break;
        }
        let previous = session.screen();
        session.send(b"j")?;
        session.wait_screen("to scroll focus evidence", |screen| screen != previous)?;
    }
    assert!(
        session.screen().contains(evidence_id),
        "scrolling must reveal the actual referenced evidence, not just the plan summary"
    );
    assert!(
        session
            .screen()
            .split_whitespace()
            .collect::<String>()
            .contains(&provenance),
        "the reviewer must be able to inspect the complete cited provenance"
    );
    session.send(b"\x1b")?;
    session.wait_screen("to close evidence without leaving the card", |screen| {
        screen.contains("pub fn z_first") && !screen.contains(evidence_id)
    })?;
    session.send(b"a")?;
    session.wait_screen(
        "to advance approval to the second selected target",
        |screen| screen.contains("pub fn b_second") && !screen.contains("pub fn z_first"),
    )?;
    let record_path = repo.path.join(".trueflow/reviews.jsonl");
    let records = read_review_records(&record_path)?;
    assert_eq!(
        records.len(),
        1,
        "one card approval must persist exactly one review"
    );
    assert_eq!(
        trueflow_test_support::record_target_key(&records[0]),
        first_fingerprint
    );
    let approved_records = fs::read(&record_path)?;

    session.send(b"l")?;
    session.wait_screen("to skip to the third selected target", |screen| {
        screen.contains("pub fn c_third") && !screen.contains("pub fn b_second")
    })?;
    session.send(b" d")?; // Space skips the last card; d finishes its recap.
    session.finish()?;
    assert_eq!(
        fs::read(&record_path)?,
        approved_records,
        "skip and finish must not approve remaining cards"
    );

    let pending = trueflow_test_support::json(&repo.run_with_env(
        &["focus", "--candidates", "--path", "src"],
        &[("HOME", home)],
    )?)?;
    let pending = pending["candidates"]
        .as_array()
        .context("pending candidates")?;
    assert_eq!(pending.len(), 3);
    assert!(
        !pending
            .iter()
            .any(|candidate| candidate["target"] == selected[0]["target"])
    );
    for name in ["a_unselected", "b_second", "c_third"] {
        assert!(
            pending
                .iter()
                .any(|candidate| candidate["target"]["path"] == format!("src/{name}.rs")),
            "{name} must remain unreviewed"
        );
    }

    let mut resumed = FocusPty::open(&repo)?;
    resumed.wait_screen(
        "to resume at the first still-unreviewed selected card",
        |screen| {
            screen.contains("pub fn b_second")
                && !screen.contains("pub fn z_first")
                && !screen.contains("pub fn a_unselected")
        },
    )?;
    resumed.send(b"l")?;
    resumed.wait_screen(
        "to retain the remaining selected order after reopen",
        |screen| screen.contains("pub fn c_third"),
    )?;
    resumed.send(b"q")?;
    resumed.finish()?;
    assert_eq!(
        fs::read(&record_path)?,
        approved_records,
        "reopening and exiting must not manufacture approvals"
    );
    for name in names {
        assert_eq!(
            fs::read_to_string(repo.path.join(format!("src/{name}.rs")))?,
            format!("pub fn {name}(balance: &mut u64) {{ *balance += 1; }}\n"),
            "focus must preserve selected and unselected source"
        );
    }
    Ok(())
}

#[test]
fn pty_focused_pass_reviews_a_budgeted_subset_then_continues_the_full_review() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    let repo = TestRepo::new("focused_pass_pty")?;
    repo.write(
        "trueflow.toml",
        "[scan]\nuse_cache = false\nwrite_cache = false\n[ai]\nmode = \"off\"\n",
    )?;
    let helpers = |suffix: &str| -> String {
        (0..6)
            .map(|n| format!("fn helper_{n}() -> u32 {{\n    {n}{suffix}\n}}\n\n"))
            .collect()
    };
    repo.write("src/lib.rs", &helpers(""))?;
    repo.commit_all("base")?;
    repo.write(
        "src/lib.rs",
        &format!(
            "{}pub fn run_git(dir: &Path) -> io::Result<Output> {{\n    Command::new(\"git\").current_dir(dir).output()\n}}\n",
            helpers(" + 1")
        ),
    )?;
    repo.commit_all("change helpers and shell out")?;
    let revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let target = format!("rev:{}", revision.trim());

    // run_git costs 16s and each one-line helper change 18s, so 40s holds two cards.
    let mut session = FocusPty::open_with(
        &repo,
        &[
            "tui", "--target", &target, "--pass", "focused", "--budget", "40s",
        ],
    )?;
    session.wait_screen(
        "to open the trust-boundary block first in a focused pass",
        |screen| {
            screen.contains("Focused pass")
                && screen.contains("Card 1/2")
                && screen.contains("pub fn run_git")
                && screen.contains("trust boundary")
        },
    )?;
    session.send(b"a")?;
    session.wait_screen("to advance to the second card", |screen| {
        screen.contains("Card 2/2") && screen.contains("helper_0")
    })?;
    session.send(b" ")?;
    session.wait_screen("to recap the pass and offer the remainder", |screen| {
        screen.contains("Focused pass complete")
            && screen.contains("Outside this pass: 5 pending blocks")
            && screen.contains("[Enter] continue full review")
    })?;
    session.send(b"\r")?;
    session.wait_screen(
        "to reload the scope as a full review without the approved block",
        |screen| {
            screen.contains("Diff Mode")
                && screen.contains("6 remaining")
                && !screen.contains("Focused pass")
        },
    )?;
    session.send(b"q")?;
    session.finish()?;

    let records = read_review_records(&repo.path.join(".trueflow/reviews.jsonl"))?;
    assert_eq!(
        records.len(),
        1,
        "only the approved card may be recorded; the skipped card and remainder stay pending"
    );
    Ok(())
}
