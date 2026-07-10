#![cfg(feature = "tui-test-support")]

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::{Arc, Mutex};
use trueflow::commands::review::{BlockChangeKind, FileChangeKind};
use trueflow::commands::tui::test_support::{
    MarkActionRunner, ScriptedMarkAction, ScriptedSessionAction, ScriptedSessionRecap, ScriptedTui,
};
use trueflow::hashing::hash_str;
use trueflow::repo_path::RepoPath;
use trueflow::review_scope::ScopePreset;
use trueflow::review_speedread::PlaybackState;
use trueflow::store::{
    CommentAnchor, CommentAnchorDiffLineKind, CommentScope, DiffCommentAnchorRow,
};
use trueflow::vcs;

use trueflow_test_support::vt100_backend::VT100Backend;
use trueflow_test_support::{TestRepo, read_review_records};

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

#[derive(Clone)]
struct RecordingMarkActionRunner {
    actions: Arc<Mutex<Vec<ScriptedMarkAction>>>,
}

impl MarkActionRunner for RecordingMarkActionRunner {
    fn run_mark(&mut self, action: &ScriptedMarkAction) -> Result<()> {
        self.actions
            .lock()
            .map_err(|error| anyhow!("recorded mark action lock poisoned: {error}"))?
            .push(action.clone());
        Ok(())
    }
}

fn recording_mark_action_runner() -> (
    RecordingMarkActionRunner,
    Arc<Mutex<Vec<ScriptedMarkAction>>>,
) {
    let actions = Arc::new(Mutex::new(Vec::new()));
    (
        RecordingMarkActionRunner {
            actions: Arc::clone(&actions),
        },
        actions,
    )
}

fn only_recorded_mark_action(
    actions: &Arc<Mutex<Vec<ScriptedMarkAction>>>,
) -> Result<ScriptedMarkAction> {
    let actions = actions
        .lock()
        .map_err(|error| anyhow!("recorded mark action lock poisoned: {error}"))?;
    assert_eq!(
        actions.len(),
        1,
        "expected exactly one submitted mark action"
    );
    Ok(actions[0].clone())
}
fn full_block_fingerprint(content: &str) -> String {
    hash_str(content)
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
fn diff_view_initial_render_scrolls_to_changed_rows_for_long_block() -> Result<()> {
    let mut file_content = String::new();
    let mut diff_lines = Vec::new();
    for line_number in 1..=24 {
        if line_number == 22 {
            file_content.push_str("line22 changed\n");
            diff_lines.push(vcs::DiffHunkLine::removed("line22\n"));
            diff_lines.push(vcs::DiffHunkLine::added("line22 changed\n"));
            continue;
        }

        let text = format!("line{line_number}\n");
        file_content.push_str(&text);
        diff_lines.push(vcs::DiffHunkLine::context(text));
    }

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(80, 18),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        24,
    )?;
    app.preload_text_diff(
        "src/lib.rs",
        vec![vcs::DiffHunk {
            file_path: RepoPath::new("src/lib.rs")?,
            old_start: 1,
            new_start: 1,
            lines: diff_lines,
        }],
    )?;
    app.show_diff();

    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(
        app.scroll_offset() > 0,
        "expected diff view to auto-scroll to changed rows"
    );
    assert!(
        screen.contains("- line22"),
        "expected initial diff render to show removed line:\n{screen}"
    );
    assert!(
        screen.contains("+ line22 changed"),
        "expected initial diff render to show added line:\n{screen}"
    );

    Ok(())
}

#[test]
fn diff_view_renders_file_deleted_banner_and_header_metadata() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(100, 20),
        "src/lib.rs",
        "fn removed() {\n    old_body();\n}\n",
        "fn removed() {\n    old_body();\n}\n",
        0,
        3,
    )?;
    app.set_review_scope(ScopePreset::MainDiff);
    app.set_current_file_change_kind(FileChangeKind::Deleted);
    app.show_diff();
    app.go_parent();

    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(
        screen.contains("Diff Mode"),
        "expected diff mode banner:\n{screen}"
    );
    assert!(
        screen.contains("File Deleted"),
        "expected deleted-file header metadata:\n{screen}"
    );
    assert!(
        !screen.contains("Mode: Diff"),
        "did not expect legacy mode row copy:\n{screen}"
    );

    Ok(())
}

#[test]
fn diff_view_renders_block_change_metadata_independent_from_file_change_metadata() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(100, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;
    app.set_review_scope(ScopePreset::MainDiff);
    app.set_current_file_change_kind(FileChangeKind::Changed);
    app.set_current_block_change_kind(BlockChangeKind::Added);
    app.show_diff();

    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(
        screen.contains("Diff Mode"),
        "expected diff mode banner:\n{screen}"
    );
    assert!(
        screen.contains("Block Added"),
        "expected block-local change metadata:\n{screen}"
    );
    assert!(
        !screen.contains("File Changed"),
        "did not expect parent file label on block header:\n{screen}"
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
fn scrollbar_boundary_source_note_persists_matching_scope_context_and_anchor() -> Result<()> {
    const REVISION: &str = "1111111111111111111111111111111111111111";

    let mut source_lines = (0..24)
        .map(|index| format!("source_line_{index:02}"))
        .collect::<Vec<_>>();
    let boundary_line = format!("SBOUNDARY_{}Z", "x".repeat(29));
    assert_eq!(boundary_line.len(), 40);
    source_lines[6] = boundary_line;
    let file_content = format!("{}\n", source_lines.join("\n"));
    let expected_fingerprint = full_block_fingerprint(&file_content);

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(40, 20),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        source_lines.len(),
    )?;
    app.set_review_scope(ScopePreset::Commit {
        id: REVISION.to_string(),
        summary: "boundary fixture".to_string(),
    });
    let (runner, recorded_actions) = recording_mark_action_runner();
    app.install_mark_action_runner(runner);

    app.render()?;
    for _ in 0..7 {
        app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))?;
    }
    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(
        app.backend()
            .rows()
            .iter()
            .any(|row| row.trim_start().starts_with('Z')),
        "expected the viewport to begin in the scrollbar-only continuation row:\n{screen}"
    );
    assert!(
        screen.contains("source_line_07"),
        "expected the boundary viewport to render the next logical source row:\n{screen}"
    );
    assert!(
        !screen.contains("source_line_15"),
        "did not expect a hidden boundary row in the viewport:\n{screen}"
    );

    app.open_note_overlay()?;
    press_text(&mut app, "boundary source note")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    let action = only_recorded_mark_action(&recorded_actions)?;
    assert_eq!(action.fingerprint, expected_fingerprint);
    assert_eq!(action.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(
        action.comment_scope,
        Some(CommentScope {
            start_line: 6,
            end_line: 15,
        })
    );
    assert_eq!(
        action.comment_context.as_deref(),
        Some(source_lines[6..15].join("\n").as_str())
    );

    let Some(CommentAnchor::Source(anchor)) = action.comment_anchor else {
        panic!("expected a source comment anchor");
    };
    assert_eq!(anchor.revision.as_str(), REVISION);
    assert_eq!(anchor.path, RepoPath::new("src/lib.rs")?);
    assert_eq!(anchor.start_line, 6);
    assert_eq!(anchor.end_line, 15);

    Ok(())
}

#[test]
fn scrollbar_boundary_diff_note_persists_matching_scope_context_and_anchor_rows() -> Result<()> {
    const REVISION: &str = "2222222222222222222222222222222222222222";

    let removed_text = format!("RBOUNDARY_{}Z", "x".repeat(27));
    let added_text = format!("ABOUNDARY_{}Y", "y".repeat(27));
    assert_eq!(removed_text.len(), 38);
    assert_eq!(added_text.len(), 38);
    let mut file_lines = vec!["diff_context_01".to_string(), added_text.clone()];
    file_lines.extend((3..=23).map(|index| format!("diff_context_{index:02}")));
    let file_content = format!("{}\n", file_lines.join("\n"));
    let expected_fingerprint = full_block_fingerprint(&file_content);
    let mut diff_lines = vec![
        vcs::DiffHunkLine::context("diff_context_01\n"),
        vcs::DiffHunkLine::removed(format!("{removed_text}\n")),
        vcs::DiffHunkLine::added(format!("{added_text}\n")),
    ];
    diff_lines.extend(
        (3..=23).map(|index| vcs::DiffHunkLine::context(format!("diff_context_{index:02}\n"))),
    );

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(40, 20),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        file_lines.len(),
    )?;
    app.set_review_scope(ScopePreset::Commit {
        id: REVISION.to_string(),
        summary: "boundary fixture".to_string(),
    });
    app.preload_text_diff(
        "src/lib.rs",
        vec![vcs::DiffHunk {
            file_path: RepoPath::new("src/lib.rs")?,
            old_start: 1,
            new_start: 1,
            lines: diff_lines,
        }],
    )?;
    app.show_diff();
    let (runner, recorded_actions) = recording_mark_action_runner();
    app.install_mark_action_runner(runner);

    app.render()?;
    for _ in 0..2 {
        app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))?;
    }
    app.render()?;

    let screen = app.backend().screen_contents();
    assert!(
        app.backend()
            .rows()
            .iter()
            .any(|row| row.trim_start().starts_with('Z')),
        "expected the viewport to begin in the removed-row boundary continuation:\n{screen}"
    );
    assert!(
        screen.contains("+ ABOUNDARY_"),
        "expected the visible replacement diff row:\n{screen}"
    );
    assert!(
        !screen.contains("diff_context_09"),
        "did not expect a hidden diff row in the viewport:\n{screen}"
    );

    app.open_note_overlay()?;
    press_text(&mut app, "boundary diff note")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    let action = only_recorded_mark_action(&recorded_actions)?;
    assert_eq!(action.fingerprint, expected_fingerprint);
    assert_eq!(action.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(
        action.comment_scope,
        Some(CommentScope {
            start_line: 1,
            end_line: 8,
        })
    );
    let expected_context = [
        format!("- {removed_text}"),
        format!("+ {added_text}"),
        "  diff_context_03".to_string(),
        "  diff_context_04".to_string(),
        "  diff_context_05".to_string(),
        "  diff_context_06".to_string(),
        "  diff_context_07".to_string(),
        "  diff_context_08".to_string(),
    ]
    .join("\n");
    assert_eq!(
        action.comment_context.as_deref(),
        Some(expected_context.as_str())
    );

    let Some(CommentAnchor::Diff(anchor)) = action.comment_anchor else {
        panic!("expected a diff comment anchor");
    };
    assert_eq!(anchor.revision.as_str(), REVISION);
    assert_eq!(anchor.path, RepoPath::new("src/lib.rs")?);
    assert_eq!(
        anchor.rows,
        vec![
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Removed,
                old_line: Some(2),
                new_line: None,
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Added,
                old_line: None,
                new_line: Some(2),
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Context,
                old_line: Some(3),
                new_line: Some(3),
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Context,
                old_line: Some(4),
                new_line: Some(4),
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Context,
                old_line: Some(5),
                new_line: Some(5),
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Context,
                old_line: Some(6),
                new_line: Some(6),
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Context,
                old_line: Some(7),
                new_line: Some(7),
            },
            DiffCommentAnchorRow {
                kind: CommentAnchorDiffLineKind::Context,
                old_line: Some(8),
                new_line: Some(8),
            },
        ]
    );

    Ok(())
}

#[test]
fn no_scroll_source_note_anchors_full_rendered_target() -> Result<()> {
    const REVISION: &str = "3333333333333333333333333333333333333333";

    let mut source_lines = (0..24)
        .map(|index| format!("source_line_{index:02}"))
        .collect::<Vec<_>>();
    let boundary_line = format!("SBOUNDARY_{}Z", "x".repeat(29));
    source_lines[6] = boundary_line.clone();
    let file_content = format!("{}\n", source_lines.join("\n"));
    let expected_fingerprint = full_block_fingerprint(&file_content);
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(40, 50),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        source_lines.len(),
    )?;
    app.set_review_scope(ScopePreset::Commit {
        id: REVISION.to_string(),
        summary: "no-scroll fixture".to_string(),
    });
    let (runner, recorded_actions) = recording_mark_action_runner();
    app.install_mark_action_runner(runner);

    app.render()?;
    let screen = app.backend().screen_contents();
    assert!(
        screen.contains(&boundary_line),
        "expected the full-width boundary source line without a scrollbar:\n{screen}"
    );

    app.open_note_overlay()?;
    press_text(&mut app, "full source note")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    let action = only_recorded_mark_action(&recorded_actions)?;
    assert_eq!(action.fingerprint, expected_fingerprint);
    assert_eq!(action.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(action.comment_scope, None);
    assert_eq!(action.comment_context, None);

    let Some(CommentAnchor::Source(anchor)) = action.comment_anchor else {
        panic!("expected a full source comment anchor");
    };
    assert_eq!(anchor.revision.as_str(), REVISION);
    assert_eq!(anchor.path, RepoPath::new("src/lib.rs")?);
    assert_eq!(anchor.start_line, 0);
    assert_eq!(anchor.end_line, 24);

    Ok(())
}

#[test]
fn no_scroll_diff_note_anchors_all_rendered_rows() -> Result<()> {
    const REVISION: &str = "4444444444444444444444444444444444444444";

    let removed_text = format!("RBOUNDARY_{}Z", "x".repeat(27));
    let added_text = format!("ABOUNDARY_{}Y", "y".repeat(27));
    let mut file_lines = vec!["diff_context_01".to_string(), added_text.clone()];
    file_lines.extend((3..=23).map(|index| format!("diff_context_{index:02}")));
    let file_content = format!("{}\n", file_lines.join("\n"));
    let expected_fingerprint = full_block_fingerprint(&file_content);
    let mut diff_lines = vec![
        vcs::DiffHunkLine::context("diff_context_01\n"),
        vcs::DiffHunkLine::removed(format!("{removed_text}\n")),
        vcs::DiffHunkLine::added(format!("{added_text}\n")),
    ];
    diff_lines.extend(
        (3..=23).map(|index| vcs::DiffHunkLine::context(format!("diff_context_{index:02}\n"))),
    );
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(40, 50),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        file_lines.len(),
    )?;
    app.set_review_scope(ScopePreset::Commit {
        id: REVISION.to_string(),
        summary: "no-scroll fixture".to_string(),
    });
    app.preload_text_diff(
        "src/lib.rs",
        vec![vcs::DiffHunk {
            file_path: RepoPath::new("src/lib.rs")?,
            old_start: 1,
            new_start: 1,
            lines: diff_lines,
        }],
    )?;
    app.show_diff();
    let (runner, recorded_actions) = recording_mark_action_runner();
    app.install_mark_action_runner(runner);

    app.render()?;
    let screen = app.backend().screen_contents();
    assert!(
        screen.contains(&format!("- {removed_text}")),
        "expected the full-width removed row without a scrollbar:\n{screen}"
    );
    assert!(
        screen.contains(&format!("+ {added_text}")),
        "expected the full-width added row without a scrollbar:\n{screen}"
    );

    app.open_note_overlay()?;
    press_text(&mut app, "full diff note")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    let action = only_recorded_mark_action(&recorded_actions)?;
    assert_eq!(action.fingerprint, expected_fingerprint);
    assert_eq!(action.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(action.comment_scope, None);
    assert_eq!(action.comment_context, None);

    let Some(CommentAnchor::Diff(anchor)) = action.comment_anchor else {
        panic!("expected a full diff comment anchor");
    };
    assert_eq!(anchor.revision.as_str(), REVISION);
    assert_eq!(anchor.path, RepoPath::new("src/lib.rs")?);
    let expected_rows = std::iter::once(DiffCommentAnchorRow {
        kind: CommentAnchorDiffLineKind::Context,
        old_line: Some(1),
        new_line: Some(1),
    })
    .chain(std::iter::once(DiffCommentAnchorRow {
        kind: CommentAnchorDiffLineKind::Removed,
        old_line: Some(2),
        new_line: None,
    }))
    .chain(std::iter::once(DiffCommentAnchorRow {
        kind: CommentAnchorDiffLineKind::Added,
        old_line: None,
        new_line: Some(2),
    }))
    .chain((3..=23).map(|line| DiffCommentAnchorRow {
        kind: CommentAnchorDiffLineKind::Context,
        old_line: Some(line),
        new_line: Some(line),
    }))
    .collect::<Vec<_>>();
    assert_eq!(anchor.rows, expected_rows);

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
fn left_arrow_inserts_text_in_the_middle_of_a_note() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(120, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;

    app.open_note_overlay()?;
    press_text(&mut app, "ab")?;
    app.send_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))?;
    app.send_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))?;
    app.render()?;

    let screen = app.backend().screen_contents();
    assert_eq!(app.input_buffer(), "axb");
    assert!(
        screen.contains("axb"),
        "expected rendered note buffer to show middle insertion:\n{screen}"
    );

    Ok(())
}

#[test]
fn note_overlay_cell_wraps_content_instead_of_word_wrapping() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(10, 12),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;

    app.open_note_overlay()?;
    press_text(&mut app, "hello world")?;
    app.render()?;

    let rows = app.backend().rows();
    assert!(
        rows.iter().any(|row| row.contains("hello wo")),
        "expected note overlay to cell-wrap the first visual row: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("rld")),
        "expected note overlay tail to continue on the next visual row: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("world")),
        "did not expect word-wrapped second-row content: {rows:?}"
    );

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

    let result = app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Err(error) = result else {
        panic!("expected injected mark failure to bubble out");
    };
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

#[test]
fn paged_note_submit_preserves_scroll_position() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_scroll_preserve")?;
    let file_lines = (1..=20)
        .map(|index| format!("scroll_line_{index:02}"))
        .collect::<Vec<_>>();
    let file_content = format!("{}\n", file_lines.join("\n"));
    repo.write("src/lib.rs", &file_content)?;

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(80, 10),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        file_lines.len(),
    )?;
    app.install_mark_action_runner(
        trueflow::commands::tui::test_support::CliMarkActionRunner::new(
            env!("CARGO_BIN_EXE_trueflow"),
            &repo.path,
        ),
    );

    app.render()?;
    for _ in 0..3 {
        app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))?;
    }
    app.render()?;

    let scroll_before_submit = app.scroll_offset();
    assert!(
        scroll_before_submit > 0,
        "expected long block to be scrolled before opening note"
    );

    app.open_note_overlay()?;
    press_text(&mut app, "sticky scroll")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;
    app.render()?;

    assert_eq!(app.scroll_offset(), scroll_before_submit);

    let screen = app.backend().screen_contents();
    assert!(
        !screen.contains("scroll_line_01"),
        "expected submit to preserve scrolled viewport instead of jumping to the top:\n{screen}"
    );

    let records = read_review_records(&repo.path.join(".trueflow").join("reviews.jsonl"))?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].note.as_deref(), Some("sticky scroll"));

    Ok(())
}

#[test]
fn paged_note_submit_persists_comment_scope_and_scoped_feedback_context() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_scope")?;
    let file_lines = (1..=20)
        .map(|index| format!("scope_line_{index:02}"))
        .collect::<Vec<_>>();
    let file_content = format!("{}\n", file_lines.join("\n"));
    repo.write("src/lib.rs", &file_content)?;

    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(80, 10),
        "src/lib.rs",
        &file_content,
        &file_content,
        0,
        file_lines.len(),
    )?;
    app.install_mark_action_runner(
        trueflow::commands::tui::test_support::CliMarkActionRunner::new(
            env!("CARGO_BIN_EXE_trueflow"),
            &repo.path,
        ),
    );

    app.render()?;
    for _ in 0..3 {
        app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))?;
    }
    app.render()?;

    app.open_note_overlay()?;
    press_text(&mut app, "scoped note")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    let records = read_review_records(&repo.path.join(".trueflow").join("reviews.jsonl"))?;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let scope = record
        .comment_scope
        .as_ref()
        .unwrap_or_else(|| panic!("expected comment scope on paged note record"));
    assert!(
        scope.start_line > 0,
        "expected scrolled pane scope: {scope:?}"
    );
    assert!(
        usize::try_from(scope.end_line).unwrap_or(usize::MAX) < file_lines.len(),
        "expected scoped pane to exclude some trailing lines: {scope:?}"
    );
    let expected_context = file_lines[usize::try_from(scope.start_line).unwrap_or(0)
        ..usize::try_from(scope.end_line).unwrap_or(file_lines.len())]
        .join("\n");
    assert_eq!(
        record.comment_context.as_deref(),
        Some(expected_context.as_str())
    );

    let feedback = repo.run(&["feedback", "--format", "xml", "--since", "all"])?;
    assert!(feedback.contains("<comment>scoped note</comment>"));
    assert!(feedback.contains(&expected_context));
    assert!(
        !feedback.contains("scope_line_01"),
        "did not expect feedback context to include lines outside the scoped pane:\n{feedback}"
    );

    Ok(())
}

#[test]
fn multiline_note_submit_feedback_file_target_filters_to_the_requested_file() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_target_file")?;
    let file_content = "fn demo() {\n    work();\n}\n";
    repo.write("src/lib.rs", file_content)?;
    repo.write("src/other.rs", "fn other() {\n    noop();\n}\n")?;

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

    let keep_feedback = repo.run(&[
        "feedback",
        "--format",
        "xml",
        "--since",
        "all",
        "--target",
        "file:src/lib.rs",
    ])?;
    assert!(keep_feedback.contains("<comment>alpha\nbeta</comment>"));
    assert!(keep_feedback.contains("path=\"src/lib.rs\""));

    let other_feedback = repo.run(&[
        "feedback",
        "--format",
        "xml",
        "--since",
        "all",
        "--target",
        "file:src/other.rs",
    ])?;
    assert!(!other_feedback.contains("<comment>alpha\nbeta</comment>"));
    assert!(!other_feedback.contains("path=\"src/lib.rs\""));

    Ok(())
}

#[test]
fn multiline_note_submit_preserves_exact_whitespace() -> Result<()> {
    let repo = TestRepo::new("tui_vt100_comment_whitespace")?;
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
    press_text(&mut app, "  alpha")?;
    app.send_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))?;
    press_text(&mut app, "beta  ")?;
    app.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;

    assert!(
        !app.is_editing(),
        "expected successful submit to close the editor"
    );

    let records = read_review_records(&repo.path.join(".trueflow").join("reviews.jsonl"))?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].verdict.as_str(), "comment");
    assert_eq!(records[0].note.as_deref(), Some("  alpha\nbeta  "));

    let feedback = repo.run(&["feedback", "--format", "xml", "--since", "all"])?;
    assert!(feedback.contains("<comment>  alpha\nbeta  </comment>"));

    Ok(())
}

#[test]
fn source_view_expands_tabs_before_rendering_code_lines() -> Result<()> {
    let file_content = "xx\tfoo\nxx\tfoo界界\nxx\tfoo\n";
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(40, 20),
        "src/lib.rs",
        file_content,
        "xx\tfoo界界\n",
        1,
        2,
    )?;

    app.render()?;
    let rows = app
        .backend()
        .rows()
        .into_iter()
        .map(|row| row.replace(' ', "."))
        .collect::<Vec<_>>();

    assert!(
        rows.iter().any(|row| row.contains(".......xx......foo")),
        "expected context rows to preserve tab width with the gutter: {rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("xx......foo界界")),
        "expected focus rows to preserve tab width before wide characters: {rows:#?}"
    );

    Ok(())
}

#[test]
fn recap_done_requests_review_something_else_instead_of_exit() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(100, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;
    app.show_recap();

    app.render()?;
    let screen = app.backend().screen_contents();
    assert!(
        screen.contains("Press [d] review something else or [q/Esc] exit"),
        "expected recap footer to advertise the continue-review flow:\n{screen}"
    );

    app.send_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))?;

    assert_eq!(
        app.take_session_action(),
        Some(ScriptedSessionAction::ReviewSomethingElse)
    );

    Ok(())
}

#[test]
fn recap_quit_still_requests_exit() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(100, 20),
        "src/lib.rs",
        "fn demo() {\n    work();\n}\n",
        "fn demo() {\n    work();\n}\n",
        0,
        3,
    )?;
    app.show_recap();

    app.send_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))?;

    assert_eq!(app.take_session_action(), Some(ScriptedSessionAction::Exit));

    Ok(())
}

#[test]
fn speed_read_space_only_dispatches_while_current_mode_is_speed_read() -> Result<()> {
    let mut app = ScriptedTui::with_single_rust_block_file(
        VT100Backend::new(80, 18),
        "src/lib.rs",
        "alpha beta gamma delta\n",
        "alpha beta gamma delta\n",
        0,
        1,
    )?;

    app.send_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))?;
    app.render()?;
    assert!(
        app.is_speed_read_active(),
        "expected speed read mode after activation"
    );
    assert_eq!(app.speed_read_playback(), Some(PlaybackState::Paused));

    app.send_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))?;
    app.render()?;
    assert!(
        app.is_speed_read_active(),
        "expected to remain in speed read mode after play"
    );
    assert_eq!(app.speed_read_playback(), Some(PlaybackState::Playing));

    app.send_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))?;
    app.render()?;
    assert!(
        !app.is_speed_read_active(),
        "did not expect stale speed read mode at root"
    );
    assert!(
        app.is_at_root(),
        "expected root navigation after leaving the block"
    );

    app.send_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))?;
    app.render()?;
    assert!(
        !app.is_speed_read_active(),
        "did not expect space to revive speed read off the active block"
    );

    Ok(())
}
