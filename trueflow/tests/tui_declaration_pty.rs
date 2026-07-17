#![cfg(feature = "tui-test-support")]

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use trueflow::store::{CommentAnchor, Record, ReviewCheck, ReviewTargetRef, Verdict};
use trueflow_test_support::TestRepo;
use vt100::Parser;

const BODY_SENTINEL: &str = "EXECUTABLE BODY SENTINEL MUST NEVER RENDER";
const PTY_ROWS: u16 = 28;
const PTY_COLS: u16 = 120;
const SCREEN_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV: &str = "TRUEFLOW_TUI_KEYBOARD_ENHANCEMENT_PROBE";

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

    fn snapshot(&self) -> (Vec<u8>, u64, bool) {
        let state = self.lock_state();
        (state.bytes.clone(), state.generation, state.reader_done)
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

struct PtySession {
    child: Option<Box<dyn Child + Send + Sync>>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<PtyOutput>,
    reader_thread: Option<thread::JoinHandle<()>>,
}

impl PtySession {
    fn spawn(repo: &TestRepo) -> Result<Self> {
        let isolated_home = repo.path.join(".test-home");
        fs::create_dir_all(&isolated_home)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
        command.arg("tui");
        command.arg("--mode");
        command.arg("declarations");
        command.cwd(&repo.path);
        command.env("TERM", "xterm-256color");
        command.env(TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV, "skip");
        command.env("HOME", &isolated_home);
        command.env("XDG_CONFIG_HOME", isolated_home.join("config"));
        command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        command.env("GIT_CONFIG_NOSYSTEM", "1");

        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let output = PtyOutput::new();
        let reader_output = Arc::clone(&output);
        let reader_thread = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => reader_output.append(&buffer[..count]),
                    Err(_) => break,
                }
            }
            reader_output.mark_reader_done();
        });

        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            output,
            reader_thread: Some(reader_thread),
        })
    }

    fn screen(&self) -> String {
        let (bytes, _, _) = self.output.snapshot();
        let mut parser = Parser::new(PTY_ROWS, PTY_COLS, 0);
        parser.process(&bytes);
        parser.screen().contents()
    }

    fn captured_bytes(&self) -> Vec<u8> {
        self.output.snapshot().0
    }

    fn wait_for_screen<F>(&mut self, description: &str, predicate: F) -> Result<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            let (bytes, generation, reader_done) = self.output.snapshot();
            let mut parser = Parser::new(PTY_ROWS, PTY_COLS, 0);
            parser.process(&bytes);
            let screen = parser.screen().contents();
            if predicate(&screen) {
                return Ok(screen);
            }

            let child = self
                .child
                .as_mut()
                .context("PTY child was already reaped")?;
            if let Some(status) = child.try_wait()? {
                if !reader_done {
                    self.output
                        .wait_for_change_or_deadline(generation, deadline);
                    continue;
                }
                let transcript = String::from_utf8_lossy(&bytes);
                bail!(
                    "trueflow tui exited before PTY screen {description}: {status}; screen: {screen}; transcript: {transcript}"
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for PTY screen {description}; screen: {screen}; transcript: {}",
                    String::from_utf8_lossy(&bytes)
                );
            }
            self.output
                .wait_for_change_or_deadline(generation, deadline);
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .context("PTY input was already closed")?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    fn wait_for_success(&mut self) -> Result<()> {
        self.writer.take();
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let status = loop {
            let child = self
                .child
                .as_mut()
                .context("PTY child was already reaped")?;
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for trueflow tui to exit; screen: {}; transcript: {}",
                    self.screen(),
                    String::from_utf8_lossy(&self.captured_bytes())
                );
            }
            thread::sleep(Duration::from_millis(10));
        };
        ensure!(
            status.success(),
            "trueflow tui exited unsuccessfully: {status}; transcript: {}",
            String::from_utf8_lossy(&self.captured_bytes())
        );
        self.finish_reader()?;
        Ok(())
    }

    fn finish_reader(&mut self) -> Result<()> {
        if let Some(reader_thread) = self.reader_thread.take()
            && reader_thread.join().is_err()
        {
            bail!("PTY reader thread panicked");
        }
        Ok(())
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.writer.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child.take();
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
    }
}

fn declaration_repo(name: &str, base: &str, head: &str) -> Result<TestRepo> {
    let repo = TestRepo::new(name)?;
    repo.git(&["config", "--local", "user.email", "reviewer@example.com"])?;
    repo.git(&["config", "--local", "user.name", "Declaration Reviewer"])?;
    repo.git(&["config", "--local", "commit.gpgSign", "false"])?;
    repo.write("src/lib.rs", base)?;
    repo.commit_all("base declaration")?;
    repo.write("src/lib.rs", head)?;
    Ok(repo)
}

fn review_records(path: &Path) -> Result<Vec<Record>> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read review records from {}", path.display()))?;
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Record>(line)
                .context("review store contained invalid JSON or record shape")
        })
        .collect()
}

fn control_sequence_count(bytes: &[u8], sequence: &[u8]) -> usize {
    bytes
        .windows(sequence.len())
        .filter(|window| *window == sequence)
        .count()
}

fn assert_terminal_restored(session: &PtySession) {
    let bytes = session.captured_bytes();
    assert_eq!(
        control_sequence_count(&bytes, b"\x1b[?1049h"),
        1,
        "declaration TUI must enter the alternate screen exactly once"
    );
    assert_eq!(
        control_sequence_count(&bytes, b"\x1b[?1049l"),
        1,
        "declaration TUI must restore the original screen exactly once"
    );
}

#[test]
fn signature_change_renders_only_the_declaration_surface_and_quits_cleanly() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = declaration_repo(
        "tui_declaration_pty_signature",
        "/// Converts one byte.\npub fn convert(value: u8) -> u8 {\n    value\n}\n",
        "/// Converts a wider value.\npub fn convert(value: u16) -> u16 {\n    let hidden = \"EXECUTABLE BODY SENTINEL MUST NEVER RENDER\";\n    value\n}\n",
    )?;
    let mut session = PtySession::spawn(&repo)?;

    let screen = session.wait_for_screen("to render the changed declaration", |screen| {
        screen.contains("Declaration Review")
            && screen.contains("/// Converts a wider value.")
            && screen.contains("pub fn convert(value: u16) -> u16")
    })?;
    assert!(
        !screen.contains(BODY_SENTINEL),
        "executable body text leaked into the declaration screen:\n{screen}"
    );
    assert!(
        !screen.contains("Speed Read"),
        "block speed-read UI leaked into declaration mode:\n{screen}"
    );
    assert!(
        !screen.contains("AI Suggestion"),
        "block AI UI leaked into declaration mode:\n{screen}"
    );

    session.send(b"q")?;
    session.wait_for_success()?;
    assert!(!String::from_utf8_lossy(&session.captured_bytes()).contains(BODY_SENTINEL));
    assert_terminal_restored(&session);
    Ok(())
}

#[test]
fn approve_appends_one_valid_v5_declaration_record_then_advances() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = declaration_repo(
        "tui_declaration_pty_approve",
        "/// First operation.\npub fn alpha(value: u8) -> u8 { value }\n\n/// Second operation.\npub fn beta(value: u8) -> u8 { value }\n",
        "/// First operation.\npub fn alpha(value: u16) -> u16 { value }\n\n/// Second operation.\npub fn beta(value: u32) -> u32 { value }\n",
    )?;
    let records_path = repo.path.join(".trueflow/reviews.jsonl");
    let mut session = PtySession::spawn(&repo)?;

    session.wait_for_screen("to render alpha before approval", |screen| {
        screen.contains("Declaration Review") && screen.contains("pub fn alpha(value: u16) -> u16")
    })?;
    assert!(
        !records_path.exists(),
        "launching declaration review must not persist an approval"
    );

    session.send(b"a")?;
    session.wait_for_screen("to advance to beta after approval", |screen| {
        screen.contains("pub fn beta(value: u32) -> u32")
            && !screen.contains("pub fn alpha(value: u16) -> u16")
    })?;

    let records = review_records(&records_path)?;
    ensure!(
        records.len() == 1,
        "one approval must append exactly one record, got {records:#?}"
    );
    let record = &records[0];
    assert_eq!(record.version, 5);
    assert!(matches!(record.target, ReviewTargetRef::Declaration { .. }));
    assert_eq!(record.check, ReviewCheck::declaration());
    assert_eq!(record.verdict, Verdict::Approved);
    assert!(
        record.declaration_locator.is_some(),
        "declaration approval lost its locator"
    );
    assert!(matches!(
        record.comment_anchor,
        Some(CommentAnchor::Declaration(_))
    ));
    record.validate()?;

    session.send(b"q")?;
    session.wait_for_success()?;
    assert_terminal_restored(&session);
    Ok(())
}

#[test]
fn body_only_change_reports_no_declaration_surface_changes_and_quits() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let repo = declaration_repo(
        "tui_declaration_pty_body_only",
        "pub fn total(values: &[u64]) -> u64 { values.iter().sum() }\n",
        "pub fn total(values: &[u64]) -> u64 { values.iter().copied().sum() }\n",
    )?;
    let mut session = PtySession::spawn(&repo)?;

    let screen = session.wait_for_screen("to explain the empty declaration review", |screen| {
        screen.contains("No declaration surface changes")
    })?;
    assert!(
        !screen.contains("pub fn total"),
        "an unchanged declaration surface was presented as reviewable:\n{screen}"
    );

    session.send(b"q")?;
    session.wait_for_success()?;
    assert_terminal_restored(&session);
    Ok(())
}
