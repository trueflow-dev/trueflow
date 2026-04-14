#![cfg(feature = "tui-test-support")]

use anyhow::{Result, bail};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use vt100::Parser;

mod common;
use common::{TestRepo, read_review_records};

fn lock_output(output: &Arc<Mutex<Vec<u8>>>) -> MutexGuard<'_, Vec<u8>> {
    match output.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn captured_output(output: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&lock_output(output).clone()).to_string()
}

fn wait_for_output(
    output: &Arc<Mutex<Vec<u8>>>,
    needle: &str,
    timeout: Duration,
    child: &mut (dyn Child + Send + Sync),
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let current = captured_output(output);
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
        thread::sleep(Duration::from_millis(25));
    }
}

fn parsed_screen_contents(output: &Arc<Mutex<Vec<u8>>>, rows: u16, cols: u16) -> String {
    let bytes = lock_output(output).clone();
    let mut parser = Parser::new(rows, cols, 0);
    parser.process(&bytes);
    parser.screen().contents()
}

fn send_and_flush(writer: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes)?;
    writer.flush()?;
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
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd)?;
    let mut writer = pair.master.take_writer()?;
    let mut reader = pair.master.try_clone_reader()?;
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_reader = Arc::clone(&output);
    let reader_thread = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => {
                    let mut output = lock_output(&output_reader);
                    output.extend_from_slice(&buf[..count]);
                }
                Err(_) => break,
            }
        }
    });

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

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let output = captured_output(&output);
                bail!("trueflow tui exited unsuccessfully: {status}; output: {output}");
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = captured_output(&output);
            bail!("timed out waiting for TUI PTY smoke test to exit; output: {output}");
        }
        thread::sleep(Duration::from_millis(50));
    }

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

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.arg("tui");
    cmd.arg("--all");
    cmd.cwd(&repo.path);
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd)?;
    let mut writer = pair.master.take_writer()?;
    let mut reader = pair.master.try_clone_reader()?;
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_reader = Arc::clone(&output);
    let reader_thread = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => {
                    let mut output = lock_output(&output_reader);
                    output.extend_from_slice(&buf[..count]);
                }
                Err(_) => break,
            }
        }
    });

    wait_for_output(
        &output,
        "0/1 reviewed",
        Duration::from_secs(10),
        &mut *child,
    )?;
    send_and_flush(&mut *writer, b"n")?;
    wait_for_output(&output, "Type a note", Duration::from_secs(5), &mut *child)?;
    send_and_flush(&mut *writer, b"a")?;
    send_and_flush(&mut *writer, b"\n")?;
    send_and_flush(&mut *writer, b"b")?;
    send_and_flush(&mut *writer, b"\r")?;
    wait_for_output(
        &output,
        "Files/dirs: 1",
        Duration::from_secs(5),
        &mut *child,
    )?;
    send_and_flush(&mut *writer, b"q")?;
    drop(writer);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let output = captured_output(&output);
                bail!("trueflow tui exited unsuccessfully: {status}; output: {output}");
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = captured_output(&output);
            bail!("timed out waiting for TUI PTY smoke test to exit; output: {output}");
        }
        thread::sleep(Duration::from_millis(50));
    }

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
