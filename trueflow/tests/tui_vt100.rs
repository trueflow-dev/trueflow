#![cfg(feature = "tui-test-support")]

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use trueflow::commands::tui::test_support::{
    MarkActionRunner, ScriptedMarkAction, ScriptedSessionRecap, ScriptedTui,
};

mod common;
use common::vt100_backend::VT100Backend;
use common::{TestRepo, read_review_records};

fn press_text(app: &mut ScriptedTui<VT100Backend>, text: &str) -> Result<()> {
    for ch in text.chars() {
        app.send_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?;
    }
    Ok(())
}

struct FailingMarkActionRunner;

impl MarkActionRunner for FailingMarkActionRunner {
    fn run_mark(&mut self, _action: &ScriptedMarkAction) -> Result<()> {
        Err(anyhow!("injected mark failure"))
    }
}

fn assert_no_local_comment_action_applied(app: &ScriptedTui<VT100Backend>) {
    let recap = app.session_recap();
    assert_eq!(
        app.remaining_blocks(),
        1,
        "expected no local visibility mutation"
    );
    assert_eq!(
        recap,
        ScriptedSessionRecap {
            comments: 0,
            blocks_touched: 0,
        },
        "expected no local recap mutation"
    );
}

fn app_with_validation_message() -> Result<ScriptedTui<VT100Backend>> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(120, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;

    app.open_note_overlay()?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(screen.contains("Note required • Type a note • Ctrl+J newline • Esc to cancel"));

    Ok(app)
}

#[test]
fn root_selection_stays_visible_when_scrolling_past_viewport_height() -> Result<()> {
    let file_names = (1..=12).map(|index| format!("file-{index:02}.rs"));
    let mut app = ScriptedTui::with_root_files(VT100Backend::new(80, 12), file_names)?;

    app.render()?;
    for _ in 0..9 {
        app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))?;
    }
    app.render()?;

    assert_eq!(app.root_cursor_label().as_deref(), Some("file-10.rs"));
    assert!(
        app.scroll_offset() > 0,
        "expected scrolling once selection moved below viewport"
    );
    assert!(
        app.backend()
            .rows()
            .iter()
            .any(|row| row.contains("file-10.rs")),
        "expected selected root entry to remain visible on screen:\n{}",
        app.backend().screen_contents()
    );

    Ok(())
}

#[test]
fn scrollbar_does_not_overwrite_wrapped_source_tail_characters() -> Result<()> {
    let long_line = format!("{}TAIL", "a".repeat(39));
    let file_content = format!(
        "{long_line}\nline-02\nline-03\nline-04\nline-05\nline-06\nline-07\nline-08\nline-09\nline-10\nline-11\nline-12\nline-13\nline-14\n"
    );
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(40, 20),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        14,
    )?;

    app.render()?;

    let rows = app.backend().rows();
    assert!(
        rows.iter().any(|row| row.contains("TAIL")),
        "expected wrapped tail to remain visible instead of being clipped by scrollbar:\n{}",
        app.backend().screen_contents()
    );

    Ok(())
}

#[test]
fn comment_overlay_hint_uses_portable_multiline_copy() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_hint")?;
    repo.write("src/lib.rs", "fn demo() {\n    work();\n}\n")?;

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(120, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;

    app.open_note_overlay()?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(
        screen.contains("Type a note"),
        "expected clearer empty-note copy:\n{screen}"
    );
    assert!(
        screen.contains("Ctrl+J newline"),
        "expected portable newline hint in overlay copy:\n{screen}"
    );
    assert!(
        !screen.contains("Shift+Enter"),
        "did not expect Shift+Enter-only guidance in overlay copy:\n{screen}"
    );

    Ok(())
}

#[test]
fn empty_note_submit_shows_validation_message_and_keeps_editor_open() -> Result<()> {
    let app = app_with_validation_message()?;

    let screen = app.backend().screen_contents();
    assert!(
        app.is_editing(),
        "expected empty submit to keep the editor open"
    );
    assert!(
        screen.contains("Note required • Type a note • Ctrl+J newline • Esc to cancel"),
        "expected explicit validation copy after empty submit:\n{screen}"
    );
    assert_no_local_comment_action_applied(&app);

    Ok(())
}

#[test]
fn note_validation_clears_after_character_input() -> Result<()> {
    let mut app = app_with_validation_message()?;

    app.send_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert_eq!(app.input_buffer(), "x");
    assert!(
        !screen.contains("Note required"),
        "expected validation copy to clear after typing:\n{screen}"
    );
    assert!(screen.contains("Enter to submit • Ctrl+J newline • Esc to cancel"));

    Ok(())
}

#[test]
fn note_validation_clears_after_backspace() -> Result<()> {
    let mut app = app_with_validation_message()?;

    app.send_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert_eq!(app.input_buffer(), "");
    assert!(
        !screen.contains("Note required"),
        "expected validation copy to clear after backspace:\n{screen}"
    );
    assert!(screen.contains("Type a note • Enter to submit • Ctrl+J newline • Esc to cancel"));

    Ok(())
}

#[test]
fn note_validation_clears_after_ctrl_j_newline() -> Result<()> {
    let mut app = app_with_validation_message()?;

    app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert_eq!(app.input_buffer(), "\n");
    assert!(
        !screen.contains("Note required"),
        "expected validation copy to clear after Ctrl+J:\n{screen}"
    );
    assert!(screen.contains("Type a note • Enter to submit • Ctrl+J newline • Esc to cancel"));

    Ok(())
}

#[test]
fn note_validation_clears_after_paste() -> Result<()> {
    let mut app = app_with_validation_message()?;

    app.send_paste("alpha")?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert_eq!(app.input_buffer(), "alpha");
    assert!(
        !screen.contains("Note required"),
        "expected validation copy to clear after paste:\n{screen}"
    );
    assert!(screen.contains("Enter to submit • Ctrl+J newline • Esc to cancel"));

    Ok(())
}

#[test]
fn ctrl_j_inserts_multiline_comment_content_in_overlay() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_newline")?;
    repo.write("src/lib.rs", "fn demo() {\n    work();\n}\n")?;

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(120, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;

    app.open_note_overlay()?;
    press_text(&mut app, "alpha")?;
    app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))?;
    press_text(&mut app, "beta")?;
    app.render()?;

    assert_eq!(app.input_buffer(), "alpha\nbeta");
    let rows = app.backend().rows();
    assert!(
        rows.iter().any(|row| row.contains("alpha")),
        "expected first line of note to render in overlay: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("beta")),
        "expected second line of note to render in overlay: {rows:?}"
    );

    Ok(())
}

#[test]
fn failed_note_submit_keeps_editor_open_and_preserves_note() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(120, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;
    app.install_mark_action_runner(FailingMarkActionRunner);

    app.open_note_overlay()?;
    press_text(&mut app, "alpha")?;
    app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))?;
    press_text(&mut app, "beta")?;

    let error = app
        .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .expect_err("expected injected mark failure to bubble out");
    assert!(error.to_string().contains("injected mark failure"));
    assert!(
        app.is_editing(),
        "expected submit failure to keep editor open"
    );
    assert_eq!(app.input_buffer(), "alpha\nbeta");
    assert_no_local_comment_action_applied(&app);

    Ok(())
}

#[test]
fn multiline_note_submit_persists_to_review_store_and_feedback_output() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_submit")?;
    let file_content = "fn demo() {\n    work();\n}\n";
    repo.write("src/lib.rs", file_content)?;

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(120, 20),
        "src/lib.rs",
        file_content,
        file_content,
        0,
        3,
    )?;
    app.install_mark_action_runner(
        trueflow::commands::tui::test_support::CliMarkActionRunner::new(
            env!("CARGO_BIN_EXE_trueflow"),
            &repo.path,
        ),
    );

    app.open_note_overlay()?;
    press_text(&mut app, "alpha")?;
    app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))?;
    press_text(&mut app, "beta")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    assert!(
        !app.is_editing(),
        "expected successful submit to close the editor"
    );

    let records = read_review_records(&repo.path.join(".trueflow").join("reviews.jsonl"))?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].verdict.as_str(), "comment");
    assert_eq!(records[0].note.as_deref(), Some("alpha\nbeta"));

    let feedback = repo.run(&["feedback", "--format", "xml", "--since", "all"])?;
    assert!(feedback.contains("<comment>alpha\nbeta</comment>"));

    Ok(())
}
