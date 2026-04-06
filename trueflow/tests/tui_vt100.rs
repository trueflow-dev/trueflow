#![cfg(feature = "tui-test-support")]

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use trueflow::commands::tui::test_support::ScriptedTui;

mod common;
use common::TestRepo;
use common::vt100_backend::VT100Backend;

fn press_text(app: &mut ScriptedTui<VT100Backend>, text: &str) -> Result<()> {
    for ch in text.chars() {
        app.send_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?;
    }
    Ok(())
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
