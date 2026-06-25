#![cfg(feature = "tui-test-support")]

use anyhow::{Result, bail};
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

fn configure_tui_pty_command(cmd: &mut CommandBuilder) {
    for (key, value) in tui_pty_environment() {
        cmd.env(key, value);
    }
}

#[test]
fn pty_smoke_harness_skips_keyboard_enhancement_probe() {
    assert!(tui_pty_environment().contains(&(
        TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV,
        TUI_KEYBOARD_ENHANCEMENT_PROBE_SKIP_VALUE,
    )));
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
    configure_tui_pty_command(&mut cmd);

    let mut child = pair.slave.spawn_command(cmd)?;
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
    configure_tui_pty_command(&mut cmd);

    let mut child = pair.slave.spawn_command(cmd)?;
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
    configure_tui_pty_command(&mut cmd);
    cmd.env("PATH", path);

    let mut child = pair.slave.spawn_command(cmd)?;
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
    configure_tui_pty_command(&mut cmd);

    let mut child = pair.slave.spawn_command(cmd)?;
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
    configure_tui_pty_command(&mut cmd);

    let mut child = pair.slave.spawn_command(cmd)?;
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
