#![cfg(feature = "tui-test-support")]

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Context, Result, ensure};
use crossterm::event::KeyCode;
use trueflow::commands::tui::declaration::tui_test_support::{
    DeclarationPane, DeclarationTestApp, GraphSelection, TestDeclarationAnchor,
    TestDeclarationFixture, TestLayout, TestOutlineRow, TestRelationship,
    TestRelationshipGroup, TestRelationshipState, TestReviewActionKind,
};

const SNAPSHOT_ID: &str = "snapshot-head";
const PATH: &str = "src/config.rs";
const CONFIG_DOC: &str = "/// Runtime configuration.";
const CONFIG_HEADER: &str = "pub struct Config {";
const CONFIG_HOST: &str = "    host: String,";
const CONFIG_MODE: &str = "    mode: Mode,";
const MODE_HEADER: &str = "pub enum Mode {";
const MODE_FAST: &str = "    Fast,";
const MODE_SAFE: &str = "    Safe,";
const LOAD_SIGNATURE: &str = "pub fn load(config: &Config) -> Result<Config>";
const SAVE_SIGNATURE: &str = "pub fn save(config: &Config) -> Result<()>";
const VALIDATE_SIGNATURE: &str = "pub fn validate(config: &Config) -> bool";
const BODY_SENTINEL: &str = "EXECUTABLE BODY MUST STAY HIDDEN";

const CONFIG_DECLARATION: &str = r#"/// Runtime configuration.
pub struct Config {
    host: String,
    mode: Mode,
}"#;

const MODE_DECLARATION: &str = r#"pub enum Mode {
    Fast,
    Safe,
}"#;

const LOAD_DECLARATION: &str = r#"pub fn load(config: &Config) -> Result<Config> {
    let body_only = "EXECUTABLE BODY MUST STAY HIDDEN: Speed Read AI hint";
    Ok(config.clone())
}"#;

const SOURCE: &str = r#"/// Runtime configuration.
pub struct Config {
    host: String,
    mode: Mode,
}

pub enum Mode {
    Fast,
    Safe,
}

pub fn load(config: &Config) -> Result<Config> {
    let body_only = "EXECUTABLE BODY MUST STAY HIDDEN: Speed Read AI hint";
    Ok(config.clone())
}

pub fn save(config: &Config) -> Result<()> { todo!() }
pub fn validate(config: &Config) -> bool { todo!() }
pub fn helper_one() {}
pub fn helper_two() {}
pub fn helper_three() {}
pub fn helper_four() {}
pub fn helper_five() {}
pub fn helper_six() {}
"#;

fn exact_range(needle: &str) -> Result<Range<usize>> {
    let start = SOURCE
        .find(needle)
        .with_context(|| format!("fixture source is missing {needle:?}"))?;
    Ok(start..start + needle.len())
}

fn target_row(
    id: &str,
    declaration_text: &str,
    display_text: &str,
    anchor_texts: &[&str],
) -> Result<TestOutlineRow> {
    Ok(TestOutlineRow::review_target(
        id,
        exact_range(declaration_text)?,
        exact_range(display_text)?,
        anchor_texts
            .iter()
            .map(|text| exact_range(text))
            .collect::<Result<Vec<_>>>()?,
    ))
}

fn component_row(id: &str, owner: &str, text: &str) -> Result<TestOutlineRow> {
    Ok(TestOutlineRow::aggregate_component(
        id,
        owner,
        exact_range(text)?,
    ))
}

fn ready_load_relationships() -> TestRelationshipState {
    TestRelationshipState::Ready(vec![
        TestRelationshipGroup::new(
            "Called by",
            vec![
                TestRelationship::in_review("load.called_by.save", "crate::save", "save"),
                TestRelationship::external(
                    "load.called_by.external",
                    "external_runtime::start",
                    "registry dependency",
                ),
                TestRelationship::unresolved(
                    "load.called_by.unresolved",
                    "dynamic_dispatch_target",
                ),
            ],
        ),
        TestRelationshipGroup::new(
            "Calls",
            vec![
                // Deliberately opposite to canonical review order. This is the regression boundary.
                TestRelationship::in_review(
                    "load.calls.validate",
                    "crate::validate",
                    "validate",
                ),
                TestRelationship::in_review("load.calls.save", "crate::save", "save"),
            ],
        ),
    ])
}

fn review_fixture() -> Result<TestDeclarationFixture> {
    let helper_rows = [
        ("helper-one", "pub fn helper_one()"),
        ("helper-two", "pub fn helper_two()"),
        ("helper-three", "pub fn helper_three()"),
        ("helper-four", "pub fn helper_four()"),
        ("helper-five", "pub fn helper_five()"),
        ("helper-six", "pub fn helper_six()"),
    ]
    .into_iter()
    .map(|(id, signature)| target_row(id, signature, signature, &[signature]))
    .collect::<Result<Vec<_>>>()?;

    let mut rows = vec![
        target_row(
            "config",
            CONFIG_DECLARATION,
            CONFIG_HEADER,
            &[CONFIG_DOC, CONFIG_HEADER, CONFIG_HOST, CONFIG_MODE],
        )?,
        component_row("config.host", "config", CONFIG_HOST)?,
        component_row("config.mode", "config", CONFIG_MODE)?,
        target_row(
            "mode",
            MODE_DECLARATION,
            MODE_HEADER,
            &[MODE_HEADER, MODE_FAST, MODE_SAFE],
        )?,
        component_row("mode.fast", "mode", MODE_FAST)?,
        component_row("mode.safe", "mode", MODE_SAFE)?,
        target_row(
            "load",
            LOAD_DECLARATION,
            LOAD_SIGNATURE,
            &[LOAD_SIGNATURE],
        )?,
        target_row("save", SAVE_SIGNATURE, SAVE_SIGNATURE, &[SAVE_SIGNATURE])?,
        target_row(
            "validate",
            VALIDATE_SIGNATURE,
            VALIDATE_SIGNATURE,
            &[VALIDATE_SIGNATURE],
        )?,
    ];
    rows.extend(helper_rows);

    Ok(TestDeclarationFixture::new(SNAPSHOT_ID, PATH, SOURCE)
        .with_outline_rows(rows)
        .with_canonical_order([
            "config",
            "mode",
            "load",
            "save",
            "validate",
            "helper-one",
            "helper-two",
            "helper-three",
            "helper-four",
            "helper-five",
            "helper-six",
        ])
        .with_relationship_state("config", TestRelationshipState::NoRelationships)
        .with_relationship_state("load", ready_load_relationships())
        .with_relationship_state("save", TestRelationshipState::NoRelationships)
        .with_relationship_state("validate", TestRelationshipState::NoRelationships)
        .with_initial_outline_selection("load")
        .with_initial_expanded(["config", "mode"])
        .with_initial_graph_selection(GraphSelection::Relationship(
            "load.calls.validate".to_owned(),
        )))
}

fn app_with_fixture(
    fixture: TestDeclarationFixture,
    inner_width: u16,
    inner_height: u16,
) -> Result<DeclarationTestApp> {
    DeclarationTestApp::new(fixture, inner_width, inner_height)
}

fn comment_and_submit(app: &mut DeclarationTestApp, body: &str) -> Result<()> {
    app.press(KeyCode::Char('c'))?;
    ensure!(
        app.state_snapshot().comment_draft.is_some(),
        "source comment action did not open the declaration editor"
    );
    app.type_text(body)?;
    app.press(KeyCode::Enter)
}

fn only_action(app: &mut DeclarationTestApp) -> Result<trueflow::commands::tui::declaration::tui_test_support::TestReviewAction> {
    let mut actions = app.take_review_actions();
    ensure!(actions.len() == 1, "expected exactly one review action, got {actions:?}");
    Ok(actions.remove(0))
}

#[test]
fn wide_split_renders_exact_projection_rows_and_relationship_groups_with_one_active_selection(
) -> Result<()> {
    let mut app = app_with_fixture(review_fixture()?, 120, 18)?;
    let rendered = app.render()?;

    assert_eq!(
        rendered.layout,
        TestLayout::Split {
            outline_width: 69,
            divider_width: 1,
            relationship_width: 50,
        },
        "120 inner cells must use the locked 58/42 split around one divider"
    );

    let rows = rendered
        .outline_rows
        .iter()
        .map(|row| (row.id.as_str(), row.source_text.as_str()))
        .collect::<Vec<_>>();
    for expected in [
        ("config", CONFIG_HEADER),
        ("config.host", CONFIG_HOST),
        ("mode.fast", MODE_FAST),
        ("load", LOAD_SIGNATURE),
    ] {
        assert!(
            rows.contains(&expected),
            "outline must retain the exact reviewed source for {expected:?}: {rows:?}"
        );
    }

    assert_eq!(
        rendered
            .relationship_groups
            .iter()
            .map(|group| group.label.as_str())
            .collect::<Vec<_>>(),
        ["Called by", "Calls"]
    );
    assert_eq!(rendered.active_selections.len(), 1);
    assert_eq!(rendered.active_selections[0].pane, DeclarationPane::Outline);
    assert_eq!(rendered.active_selections[0].row_id, "load");
    Ok(())
}

#[test]
fn narrow_relationship_replacement_backspace_restores_outline_selection_expansion_and_scroll(
) -> Result<()> {
    for open_key in [KeyCode::Char('o'), KeyCode::Enter] {
        let mut app = app_with_fixture(review_fixture()?, 99, 5)?;
        assert_eq!(
            app.render()?.layout,
            TestLayout::Single {
                pane: DeclarationPane::Outline,
                width: 99,
            }
        );
        let outline_before = app.state_snapshot();
        assert!(outline_before.outline.scroll > 0, "fixture must exercise restoration below the fold");
        assert_eq!(
            outline_before.outline.expanded,
            BTreeSet::from(["config".to_owned(), "mode".to_owned()])
        );

        app.press(open_key)?;
        assert_eq!(
            app.render()?.layout,
            TestLayout::Single {
                pane: DeclarationPane::Relationships,
                width: 99,
            }
        );

        app.press(KeyCode::Backspace)?;
        assert_eq!(app.state_snapshot(), outline_before, "{open_key:?} replacement lost outline state");
        assert_eq!(
            app.render()?.layout,
            TestLayout::Single {
                pane: DeclarationPane::Outline,
                width: 99,
            }
        );
    }
    Ok(())
}

#[test]
fn crossing_100_columns_preserves_both_pane_cursors_expansion_scroll_and_comment_draft(
) -> Result<()> {
    let mut app = app_with_fixture(review_fixture()?, 100, 5)?;
    app.render()?;
    app.press(KeyCode::Char('c'))?;
    app.type_text("draft survives layout changes")?;

    let before = app.state_snapshot();
    assert!(before.outline.scroll > 0, "outline fixture must be scrolled");
    assert!(before.relationships.scroll > 0, "graph fixture must be scrolled");
    assert!(before.relationships.selection.is_some());
    assert_eq!(before.comment_draft.as_deref(), Some("draft survives layout changes"));
    assert_eq!(
        before.outline.expanded,
        BTreeSet::from(["config".to_owned(), "mode".to_owned()])
    );
    assert!(matches!(app.render()?.layout, TestLayout::Split { .. }));

    app.resize(99, 5)?;
    assert_eq!(app.state_snapshot(), before, "wide-to-narrow resize mutated reducer state");
    assert_eq!(
        app.render()?.layout,
        TestLayout::Single {
            pane: DeclarationPane::Outline,
            width: 99,
        }
    );

    app.resize(100, 5)?;
    assert_eq!(app.state_snapshot(), before, "narrow-to-wide resize mutated reducer state");
    assert!(matches!(app.render()?.layout, TestLayout::Split { .. }));
    Ok(())
}

#[test]
fn space_advances_canonical_declaration_order_even_when_graph_selection_points_elsewhere(
) -> Result<()> {
    let mut app = app_with_fixture(review_fixture()?, 120, 12)?;
    app.press(KeyCode::Tab)?;
    let before = app.state_snapshot();
    assert_eq!(before.active_declaration, "load");
    assert_eq!(
        before.relationships.selection,
        Some(GraphSelection::Relationship("load.calls.validate".to_owned()))
    );

    app.press(KeyCode::Char(' '))?;
    let after = app.state_snapshot();
    assert_eq!(
        after.active_declaration, "save",
        "canonical successor to load is save; graph order points to validate"
    );
    assert_eq!(after.back_stack_depth, 0, "Space is canonical advance, not a graph jump");
    Ok(())
}

#[test]
fn in_review_relationship_jump_round_trips_back_stack_but_external_and_unresolved_are_inspection_only(
) -> Result<()> {
    let mut jumping = app_with_fixture(review_fixture()?, 120, 12)?;
    jumping.press(KeyCode::Tab)?;
    let before_jump = jumping.state_snapshot();
    jumping.press(KeyCode::Enter)?;
    assert_eq!(jumping.state_snapshot().active_declaration, "validate");
    assert_eq!(jumping.state_snapshot().back_stack_depth, 1);
    jumping.press(KeyCode::Backspace)?;
    assert_eq!(jumping.state_snapshot(), before_jump, "Backspace must restore the complete pre-jump view");

    for relationship_id in ["load.called_by.external", "load.called_by.unresolved"] {
        let fixture = review_fixture()?.with_initial_graph_selection(
            GraphSelection::Relationship(relationship_id.to_owned()),
        );
        let mut app = app_with_fixture(fixture, 120, 12)?;
        app.press(KeyCode::Tab)?;
        app.press(KeyCode::Enter)?;

        let inspected = app.state_snapshot();
        assert_eq!(inspected.active_declaration, "load");
        assert_eq!(inspected.back_stack_depth, 0);
        assert_eq!(inspected.inspected_relationship.as_deref(), Some(relationship_id));

        app.press(KeyCode::Char('a'))?;
        app.press(KeyCode::Char('c'))?;
        assert!(app.take_review_actions().is_empty(), "{relationship_id} became actionable");
        assert!(app.state_snapshot().comment_draft.is_none(), "{relationship_id} created a source editor");
    }
    Ok(())
}

#[test]
fn field_approval_and_variant_comment_resolve_to_the_aggregate_review_owner() -> Result<()> {
    let field_fixture = review_fixture()?.with_initial_outline_selection("config.host");
    let mut field_app = app_with_fixture(field_fixture, 120, 12)?;
    field_app.press(KeyCode::Char('a'))?;
    let field_action = only_action(&mut field_app)?;
    assert_eq!(field_action.kind, TestReviewActionKind::Approve);
    assert_eq!(field_action.owner_id, "config");

    let variant_fixture = review_fixture()?.with_initial_outline_selection("mode.fast");
    let mut variant_app = app_with_fixture(variant_fixture, 120, 12)?;
    comment_and_submit(&mut variant_app, "variant contract")?;
    let variant_action = only_action(&mut variant_app)?;
    assert_eq!(variant_action.kind, TestReviewActionKind::Comment);
    assert_eq!(variant_action.owner_id, "mode");
    assert_eq!(variant_action.comment_body.as_deref(), Some("variant contract"));

    let anchor = variant_action.anchor.context("variant comment anchor")?;
    assert_eq!(anchor.snapshot_id, SNAPSHOT_ID);
    assert_eq!(anchor.path, PATH);
    assert_eq!(anchor.ranges.len(), 1);
    assert_eq!(anchor.ranges[0].source_range, exact_range(MODE_FAST)?);
    assert_eq!(anchor.ranges[0].exact_text, MODE_FAST);
    Ok(())
}

#[test]
fn full_projection_comment_retains_all_noncontiguous_exact_source_ranges() -> Result<()> {
    let fixture = review_fixture()?.with_initial_outline_selection("config");
    let mut app = app_with_fixture(fixture, 120, 12)?;
    comment_and_submit(&mut app, "whole shape")?;
    let action = only_action(&mut app)?;
    assert_eq!(action.kind, TestReviewActionKind::Comment);
    assert_eq!(action.owner_id, "config");
    assert_eq!(action.comment_body.as_deref(), Some("whole shape"));

    let anchor = action.anchor.context("full projection anchor")?;
    assert_eq!(
        anchor,
        TestDeclarationAnchor::new(
            SNAPSHOT_ID,
            PATH,
            [CONFIG_DOC, CONFIG_HEADER, CONFIG_HOST, CONFIG_MODE]
                .into_iter()
                .map(|text| Ok((exact_range(text)?, text.to_owned())))
                .collect::<Result<Vec<_>>>()?,
        )
    );
    Ok(())
}

#[test]
fn relationship_and_capability_rows_cannot_approve_or_open_source_comments() -> Result<()> {
    let mut graph_app = app_with_fixture(review_fixture()?, 120, 12)?;
    graph_app.press(KeyCode::Tab)?;
    graph_app.press(KeyCode::Char('c'))?;
    graph_app.press(KeyCode::Char('a'))?;
    assert!(graph_app.take_review_actions().is_empty());
    assert!(graph_app.state_snapshot().comment_draft.is_none());

    let partial = review_fixture()?
        .with_relationship_state(
            "load",
            TestRelationshipState::Partial {
                reason: "references truncated at configured bound".to_owned(),
                groups: Vec::new(),
            },
        )
        .with_initial_graph_selection(GraphSelection::Status);
    let mut capability_app = app_with_fixture(partial, 120, 12)?;
    capability_app.press(KeyCode::Tab)?;
    capability_app.press(KeyCode::Char('c'))?;
    capability_app.press(KeyCode::Char('a'))?;
    assert!(capability_app.take_review_actions().is_empty());
    assert!(capability_app.state_snapshot().comment_draft.is_none());

    let mut source_app = app_with_fixture(review_fixture()?, 120, 12)?;
    source_app.press(KeyCode::Char('c'))?;
    assert!(
        source_app.state_snapshot().comment_draft.is_some(),
        "the same key must remain enabled for an exact source row"
    );
    Ok(())
}

#[test]
fn relationship_loading_empty_partial_and_unavailable_states_have_distinct_semantic_output(
) -> Result<()> {
    let cases = [
        (
            "checking",
            TestRelationshipState::Checking,
            "Checking…",
        ),
        (
            "successful empty",
            TestRelationshipState::NoRelationships,
            "No relationships found",
        ),
        (
            "bounded partial",
            TestRelationshipState::Partial {
                reason: "result limit reached".to_owned(),
                groups: Vec::new(),
            },
            "Partial — result limit reached",
        ),
        (
            "historical unavailable",
            TestRelationshipState::Unavailable {
                reason: "historical LSP workspace not enabled".to_owned(),
            },
            "Unavailable — historical LSP workspace not enabled",
        ),
    ];

    for (case, state, expected) in cases {
        let fixture = review_fixture()?.with_relationship_state("load", state);
        let mut app = app_with_fixture(fixture, 120, 10)?;
        let rendered = app.render()?;
        assert_eq!(rendered.relationship_status.as_deref(), Some(expected), "{case}");
        assert!(rendered.visible_text.contains(expected), "{case} was not rendered");
    }
    Ok(())
}

#[test]
fn declaration_renderer_never_exposes_body_text_speed_read_or_ai_hints() -> Result<()> {
    let mut app = app_with_fixture(review_fixture()?, 120, 24)?;
    let rendered = app.render()?;

    assert!(rendered.visible_text.contains(LOAD_SIGNATURE));
    for forbidden in [BODY_SENTINEL, "Speed Read", "AI hint"] {
        assert!(
            !rendered.visible_text.contains(forbidden),
            "declaration renderer leaked forbidden block-only content {forbidden:?}"
        );
    }
    Ok(())
}
