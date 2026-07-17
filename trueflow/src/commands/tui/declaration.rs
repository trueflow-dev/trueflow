//! Dedicated Declaration Review controller and renderer.
//!
//! This module deliberately consumes only explicit projection fragments. It never derives
//! display or comment text from the enclosing declaration span, which may include a body.

pub mod graph;
mod relationship_bridge;
mod runtime;
pub(super) mod terminal;

pub use relationship_bridge::{
    BackgroundRelationshipProvider, DeclarationRelationshipProvider,
    DeclarationRelationshipRequest, DeclarationRelationshipResults, RelationshipBridge,
    RelationshipUpdate,
};
pub use runtime::{
    DeclarationAppRuntime, DeclarationRecordAppender, PreparedDeclarationLaunch,
    PreparedDeclarationTarget, prepare_declaration_launch,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

pub use graph::{
    GraphPaneState, GraphSelection, Relationship, RelationshipDestination, RelationshipGroup,
    RelationshipRenderGroup, RelationshipRenderRow, RelationshipState,
};
use graph::{
    GraphRow, ensure_selection_visible as ensure_graph_selection_visible, graph_rows,
    move_selection as move_graph_selection, normalize_selection as normalize_graph_selection,
    semantic_groups,
};

pub const WIDE_LAYOUT_MIN_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationPane {
    Outline,
    Relationships,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationReviewStatus {
    Pending,
    Approved,
    Commented,
    Rejected,
}

impl DeclarationReviewStatus {
    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "",
            Self::Approved => " [approved]",
            Self::Commented => " [commented]",
            Self::Rejected => " [rejected]",
        }
    }
}

/// One exact-source outline row. `declaration_range` is identity/navigation metadata only;
/// rendering is always sourced from `display_range` and comments from `anchor_ranges`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineRow {
    pub id: String,
    pub review_owner: String,
    pub declaration_range: Range<usize>,
    pub display_range: Range<usize>,
    pub anchor_ranges: Vec<Range<usize>>,
    pub parent: Option<String>,
    pub review_target: bool,
    pub status: DeclarationReviewStatus,
}

impl OutlineRow {
    pub fn review_target(
        id: impl Into<String>,
        declaration_range: Range<usize>,
        display_range: Range<usize>,
        anchor_ranges: Vec<Range<usize>>,
    ) -> Self {
        let id = id.into();
        Self {
            review_owner: id.clone(),
            id,
            declaration_range,
            display_range,
            anchor_ranges,
            parent: None,
            review_target: true,
            status: DeclarationReviewStatus::Pending,
        }
    }

    pub fn aggregate_component(
        id: impl Into<String>,
        review_owner: impl Into<String>,
        source_range: Range<usize>,
    ) -> Self {
        let review_owner = review_owner.into();
        Self {
            id: id.into(),
            review_owner: review_owner.clone(),
            declaration_range: source_range.clone(),
            display_range: source_range.clone(),
            anchor_ranges: vec![source_range],
            parent: Some(review_owner),
            review_target: false,
            status: DeclarationReviewStatus::Pending,
        }
    }
}

/// Snapshot-keyed input to the declaration controller.
#[derive(Debug, Clone)]
pub struct DeclarationDocument {
    pub snapshot_id: String,
    pub path: String,
    pub exact_source: String,
    pub outline_rows: Vec<OutlineRow>,
    pub canonical_order: Vec<String>,
    pub relationships: BTreeMap<String, RelationshipState>,
    pub initial_outline_selection: Option<String>,
    pub initial_expanded: BTreeSet<String>,
    pub initial_graph_selection: Option<GraphSelection>,
}

impl DeclarationDocument {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.snapshot_id.is_empty(),
            "declaration snapshot id cannot be empty"
        );
        ensure!(
            !self.path.is_empty(),
            "declaration snapshot path cannot be empty"
        );
        let mut ids = BTreeSet::new();
        for row in &self.outline_rows {
            ensure!(!row.id.is_empty(), "outline row id cannot be empty");
            ensure!(
                ids.insert(row.id.as_str()),
                "duplicate outline row id {}",
                row.id
            );
            validate_range(
                &self.exact_source,
                &row.declaration_range,
                &row.id,
                "declaration",
            )?;
            validate_range(&self.exact_source, &row.display_range, &row.id, "display")?;
            ensure!(
                !row.anchor_ranges.is_empty(),
                "outline row {} has no source anchors",
                row.id
            );
            let mut previous_end = 0;
            for range in &row.anchor_ranges {
                validate_range(&self.exact_source, range, &row.id, "anchor")?;
                ensure!(
                    range.start >= previous_end,
                    "outline row {} has unordered/overlapping anchors",
                    row.id
                );
                previous_end = range.end;
            }
        }
        for row in &self.outline_rows {
            ensure!(
                ids.contains(row.review_owner.as_str()),
                "outline row {} has missing review owner {}",
                row.id,
                row.review_owner
            );
            if let Some(parent) = &row.parent {
                ensure!(
                    ids.contains(parent.as_str()),
                    "outline row {} has missing parent {parent}",
                    row.id
                );
            }
        }
        let mut canonical = BTreeSet::new();
        for id in &self.canonical_order {
            ensure!(
                canonical.insert(id.as_str()),
                "duplicate canonical declaration {id}"
            );
            let row = self.outline_rows.iter().find(|row| row.id == *id);
            ensure!(
                row.is_some_and(|row| row.review_target),
                "canonical declaration {id} is not a review target"
            );
        }
        ensure!(
            !self.canonical_order.is_empty(),
            "canonical declaration order cannot be empty"
        );
        if let Some(selection) = &self.initial_outline_selection {
            ensure!(
                ids.contains(selection.as_str()),
                "initial outline selection {selection} does not exist"
            );
        }
        for expanded in &self.initial_expanded {
            ensure!(
                ids.contains(expanded.as_str()),
                "initial expanded row {expanded} does not exist"
            );
        }
        for (source_id, state) in &self.relationships {
            ensure!(
                canonical.contains(source_id.as_str()),
                "relationship state source {source_id} is not a review target"
            );
            let mut group_labels = BTreeSet::new();
            let mut relationship_ids = BTreeSet::new();
            for group in state.groups() {
                ensure!(
                    !group.label.is_empty(),
                    "relationship group label cannot be empty"
                );
                ensure!(
                    group_labels.insert(group.label.as_str()),
                    "duplicate relationship group label {} for {source_id}",
                    group.label
                );
                for relationship in &group.relationships {
                    ensure!(
                        !relationship.id.is_empty(),
                        "relationship id cannot be empty"
                    );
                    ensure!(
                        relationship_ids.insert(relationship.id.as_str()),
                        "duplicate relationship id {} for {source_id}",
                        relationship.id
                    );
                    ensure!(
                        !relationship.label.is_empty(),
                        "relationship label cannot be empty"
                    );
                    if let RelationshipDestination::InReview { declaration_id } =
                        &relationship.destination
                    {
                        ensure!(
                            canonical.contains(declaration_id.as_str()),
                            "relationship {} points to non-review declaration {declaration_id}",
                            relationship.id
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn source_text(&self, range: &Range<usize>) -> &str {
        &self.exact_source[range.clone()]
    }

    fn row(&self, id: &str) -> Option<&OutlineRow> {
        self.outline_rows.iter().find(|row| row.id == id)
    }

    fn relationships_for(&self, declaration_id: &str) -> RelationshipState {
        self.relationships
            .get(declaration_id)
            .cloned()
            .unwrap_or_else(|| RelationshipState::Unavailable {
                reason: "relationship provider has no result for this declaration".to_owned(),
            })
    }
}

fn validate_range(source: &str, range: &Range<usize>, id: &str, role: &str) -> Result<()> {
    ensure!(
        range.start < range.end,
        "outline row {id} has an empty {role} range"
    );
    ensure!(
        range.end <= source.len(),
        "outline row {id} has an out-of-bounds {role} range"
    );
    ensure!(
        source.is_char_boundary(range.start) && source.is_char_boundary(range.end),
        "outline row {id} has a non-UTF-8-boundary {role} range"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlinePaneState {
    pub selection: String,
    pub scroll: usize,
    pub expanded: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationStateSnapshot {
    pub active_declaration: String,
    pub active_pane: DeclarationPane,
    pub outline: OutlinePaneState,
    pub relationships: GraphPaneState,
    pub comment_draft: Option<String>,
    pub back_stack_depth: usize,
    pub inspected_relationship: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentIdentity {
    snapshot_id: String,
    path: String,
}

#[derive(Debug, Clone)]
struct NavigationSnapshot {
    document: DocumentIdentity,
    outline_scope: Option<String>,
    active_declaration: String,
    active_pane: DeclarationPane,
    outline: OutlinePaneState,
    relationships: GraphPaneState,
    comment_draft: Option<String>,
    rejection_draft: bool,
    replacement_restore: Option<Box<NavigationSnapshot>>,
    inspected_relationship: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationAnchorRange {
    pub source_range: Range<usize>,
    pub exact_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationAnchor {
    pub snapshot_id: String,
    pub path: String,
    pub ranges: Vec<DeclarationAnchorRange>,
}

impl DeclarationAnchor {
    pub fn new(
        snapshot_id: impl Into<String>,
        path: impl Into<String>,
        ranges: impl IntoIterator<Item = (Range<usize>, String)>,
    ) -> Self {
        Self {
            snapshot_id: snapshot_id.into(),
            path: path.into(),
            ranges: ranges
                .into_iter()
                .map(|(source_range, exact_text)| DeclarationAnchorRange {
                    source_range,
                    exact_text,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationReviewActionKind {
    Approve,
    Comment,
    Reject,
}

/// Structured intent emitted by the reducer. Persistence remains the launch integration's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationReviewAction {
    pub kind: DeclarationReviewActionKind,
    pub owner_id: String,
    pub comment_body: Option<String>,
    pub anchor: Option<DeclarationAnchor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationLayout {
    Split {
        outline_width: u16,
        divider_width: u16,
        relationship_width: u16,
    },
    Single {
        pane: DeclarationPane,
        width: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineRenderRow {
    pub id: String,
    pub source_text: String,
    pub selected: bool,
    pub depth: usize,
    pub status: DeclarationReviewStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSelection {
    pub pane: DeclarationPane,
    pub row_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationRenderModel {
    pub layout: DeclarationLayout,
    pub outline_rows: Vec<OutlineRenderRow>,
    pub relationship_groups: Vec<RelationshipRenderGroup>,
    pub relationship_status: Option<String>,
    pub relationship_rows: Vec<RelationshipRenderRow>,
    pub title: String,
    pub footer: String,
    pub banner: String,
    pub active_selections: Vec<ActiveSelection>,
    pub visible_text: String,
}

#[derive(Debug, Clone)]
pub struct DeclarationController {
    documents: Vec<DeclarationDocument>,
    active_document: usize,
    outline_scope: Option<String>,
    active_declaration: String,
    active_pane: DeclarationPane,
    outline: OutlinePaneState,
    relationships: GraphPaneState,
    comment_draft: Option<String>,
    rejection_draft: bool,
    back_stack: Vec<NavigationSnapshot>,
    replacement_restore: Option<Box<NavigationSnapshot>>,
    inspected_relationship: Option<String>,
    pending_actions: Vec<DeclarationReviewAction>,
    inner_width: u16,
    inner_height: u16,
}

impl DeclarationController {
    pub fn new(document: DeclarationDocument, inner_width: u16, inner_height: u16) -> Result<Self> {
        document.validate()?;
        let outline_selection = document
            .initial_outline_selection
            .clone()
            .unwrap_or_else(|| document.canonical_order[0].clone());
        let active_declaration = document
            .row(&outline_selection)
            .map(|row| row.review_owner.clone())
            .unwrap_or_else(|| document.canonical_order[0].clone());
        Self::from_document_catalog(
            vec![document],
            active_declaration,
            outline_selection,
            None,
            inner_width,
            inner_height,
        )
    }

    pub(crate) fn new_with_document_catalog(
        documents: Vec<DeclarationDocument>,
        active_declaration: String,
        inner_width: u16,
        inner_height: u16,
    ) -> Result<Self> {
        let active_document = documents
            .iter()
            .position(|document| {
                document
                    .canonical_order
                    .iter()
                    .any(|id| id == &active_declaration)
            })
            .context("active declaration has no exact-source document")?;
        let outline_selection = documents[active_document]
            .row(&active_declaration)
            .map(|row| row.id.clone())
            .context("active declaration has no review-target outline row")?;
        Self::from_document_catalog(
            documents,
            active_declaration.clone(),
            outline_selection,
            Some(active_declaration),
            inner_width,
            inner_height,
        )
    }

    fn from_document_catalog(
        documents: Vec<DeclarationDocument>,
        active_declaration: String,
        outline_selection: String,
        outline_scope: Option<String>,
        inner_width: u16,
        inner_height: u16,
    ) -> Result<Self> {
        ensure!(
            !documents.is_empty(),
            "declaration document catalog cannot be empty"
        );
        let mut identities = BTreeSet::new();
        let mut declarations = BTreeSet::new();
        for document in &documents {
            document.validate()?;
            ensure!(
                identities.insert((document.snapshot_id.as_str(), document.path.as_str())),
                "duplicate declaration document {} at {}",
                document.snapshot_id,
                document.path
            );
            for declaration_id in &document.canonical_order {
                ensure!(
                    declarations.insert(declaration_id.as_str()),
                    "declaration {declaration_id} appears in multiple documents"
                );
            }
        }
        let active_document = documents
            .iter()
            .position(|document| {
                document
                    .canonical_order
                    .iter()
                    .any(|id| id == &active_declaration)
            })
            .context("active declaration has no exact-source document")?;
        let document = &documents[active_document];
        let mut controller = Self {
            outline: OutlinePaneState {
                selection: outline_selection,
                scroll: 0,
                expanded: document.initial_expanded.clone(),
            },
            relationships: GraphPaneState {
                selection: document
                    .initial_graph_selection
                    .clone()
                    .or(Some(GraphSelection::Status)),
                scroll: 0,
                collapsed_groups: BTreeSet::new(),
            },
            documents,
            active_document,
            outline_scope,
            active_declaration,
            active_pane: DeclarationPane::Outline,
            comment_draft: None,
            rejection_draft: false,
            back_stack: Vec::new(),
            replacement_restore: None,
            inspected_relationship: None,
            pending_actions: Vec::new(),
            inner_width,
            inner_height,
        };
        controller.normalize_view_state();
        Ok(controller)
    }
    fn document(&self) -> &DeclarationDocument {
        &self.documents[self.active_document]
    }

    fn document_identity(&self) -> DocumentIdentity {
        let document = self.document();
        DocumentIdentity {
            snapshot_id: document.snapshot_id.clone(),
            path: document.path.clone(),
        }
    }

    fn document_index(&self, identity: &DocumentIdentity) -> Option<usize> {
        self.documents.iter().position(|document| {
            document.snapshot_id == identity.snapshot_id && document.path == identity.path
        })
    }

    fn declaration_document_index(&self, declaration_id: &str) -> Option<usize> {
        self.documents.iter().position(|document| {
            document
                .canonical_order
                .iter()
                .any(|id| id == declaration_id)
        })
    }

    pub(crate) fn begin_review(&mut self, declaration_id: &str) -> Result<()> {
        let document_index = self
            .declaration_document_index(declaration_id)
            .with_context(|| format!("review declaration {declaration_id} has no document"))?;
        let document = &self.documents[document_index];
        let outline_selection = document
            .row(declaration_id)
            .map(|row| row.id.clone())
            .with_context(|| format!("review declaration {declaration_id} has no outline row"))?;
        self.active_document = document_index;
        self.outline_scope = Some(declaration_id.to_owned());
        self.active_declaration = declaration_id.to_owned();
        self.active_pane = DeclarationPane::Outline;
        self.outline = OutlinePaneState {
            selection: outline_selection,
            scroll: 0,
            expanded: document.initial_expanded.clone(),
        };
        self.relationships = GraphPaneState {
            selection: document
                .initial_graph_selection
                .clone()
                .or(Some(GraphSelection::Status)),
            scroll: 0,
            collapsed_groups: BTreeSet::new(),
        };
        self.comment_draft = None;
        self.rejection_draft = false;
        self.back_stack.clear();
        self.replacement_restore = None;
        self.inspected_relationship = None;
        self.pending_actions.clear();
        self.normalize_view_state();
        Ok(())
    }

    pub(crate) fn apply_review_status(
        &mut self,
        declaration_id: &str,
        status: DeclarationReviewStatus,
    ) {
        for document in &mut self.documents {
            for row in &mut document.outline_rows {
                if row.review_owner == declaration_id {
                    row.status = status;
                }
            }
        }
    }

    pub fn state_snapshot(&self) -> DeclarationStateSnapshot {
        DeclarationStateSnapshot {
            active_declaration: self.active_declaration.clone(),
            active_pane: self.active_pane,
            outline: self.outline.clone(),
            relationships: self.relationships.clone(),
            comment_draft: self.comment_draft.clone(),
            back_stack_depth: self.back_stack.len(),
            inspected_relationship: self.inspected_relationship.clone(),
        }
    }

    pub fn resize(&mut self, inner_width: u16, inner_height: u16) {
        self.inner_width = inner_width;
        self.inner_height = inner_height;
    }

    pub fn is_editing(&self) -> bool {
        self.comment_draft.is_some()
    }

    pub fn apply_relationship_state(
        &mut self,
        declaration_id: &str,
        state: RelationshipState,
    ) -> Result<()> {
        let document_index = self
            .declaration_document_index(declaration_id)
            .with_context(|| {
                format!("relationship update targets an unknown declaration {declaration_id}")
            })?;
        self.documents[document_index]
            .relationships
            .insert(declaration_id.to_owned(), state);
        if self.active_declaration == declaration_id {
            let state = self.current_relationship_state();
            normalize_graph_selection(&mut self.relationships, &state);
            ensure_graph_selection_visible(
                &mut self.relationships,
                &state,
                usize::from(self.inner_height),
            );
        }
        Ok(())
    }

    pub fn take_actions(&mut self) -> Vec<DeclarationReviewAction> {
        std::mem::take(&mut self.pending_actions)
    }

    pub fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        if self.comment_draft.is_some() {
            return self.handle_editor_key(key);
        }
        match key {
            KeyCode::Char('j') | KeyCode::Down => self.move_active_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_active_selection(-1),
            KeyCode::PageDown => self.move_active_selection(
                isize::try_from(self.inner_height.max(1)).unwrap_or(isize::MAX),
            ),
            KeyCode::PageUp => self.move_active_selection(
                -isize::try_from(self.inner_height.max(1)).unwrap_or(isize::MAX),
            ),
            KeyCode::Home => self.move_to_edge(false),
            KeyCode::End => self.move_to_edge(true),
            KeyCode::Tab | KeyCode::BackTab if self.is_wide() => self.toggle_pane(),
            KeyCode::Char('o')
                if !self.is_wide() && self.active_pane == DeclarationPane::Outline =>
            {
                self.open_relationship_replacement();
            }
            KeyCode::Enter if !self.is_wide() && self.active_pane == DeclarationPane::Outline => {
                self.open_relationship_replacement();
            }
            KeyCode::Enter if self.active_pane == DeclarationPane::Relationships => {
                self.activate_graph_selection();
            }
            KeyCode::Backspace | KeyCode::Esc => self.go_back()?,
            KeyCode::Char(' ') => self.advance_canonical(),
            KeyCode::Char('a') => self.emit_approval(),
            KeyCode::Char('c') => self.open_comment(),
            KeyCode::Char('r') => self.open_rejection(),
            KeyCode::Char('h') | KeyCode::Left if self.active_pane == DeclarationPane::Outline => {
                self.collapse_selected();
            }
            KeyCode::Char('l') | KeyCode::Right if self.active_pane == DeclarationPane::Outline => {
                self.expand_selected();
            }
            KeyCode::Char('[') if self.active_pane == DeclarationPane::Relationships => {
                self.move_graph_group(-1);
            }
            KeyCode::Char(']') if self.active_pane == DeclarationPane::Relationships => {
                self.move_graph_group(1);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn insert_text(&mut self, text: &str) {
        if let Some(draft) = &mut self.comment_draft {
            draft.push_str(text);
        }
    }

    pub fn render_model(&self) -> DeclarationRenderModel {
        self.render_model_for(self.inner_width, self.inner_height)
    }

    fn render_model_for(&self, inner_width: u16, row_height: u16) -> DeclarationRenderModel {
        let layout = layout_for(inner_width, self.active_pane);
        let row_height = usize::from(row_height);
        let visible_outline = self.visible_outline_rows();
        let outline_selection = visible_outline
            .iter()
            .position(|row| row.id == self.outline.selection);
        let outline_start = effective_window_start(
            self.outline.scroll,
            outline_selection,
            visible_outline.len(),
            row_height,
        );
        let outline_rows = visible_outline
            .iter()
            .skip(outline_start)
            .take(row_height)
            .map(|row| OutlineRenderRow {
                id: row.id.clone(),
                source_text: self.document().source_text(&row.display_range).to_owned(),
                selected: self.active_pane == DeclarationPane::Outline
                    && row.id == self.outline.selection,
                depth: usize::from(row.parent.is_some()),
                status: row.status,
            })
            .collect::<Vec<_>>();
        let relationship_state = self.current_relationship_state();
        let relationship_groups = semantic_groups(
            &relationship_state,
            self.relationships.selection.as_ref(),
            self.active_pane == DeclarationPane::Relationships,
        );
        let relationship_status = relationship_state.status_text();
        let graph_active = self.active_pane == DeclarationPane::Relationships;
        let graph_rows = graph_rows(&relationship_state, &self.relationships.collapsed_groups);
        let graph_selection = self.relationships.selection.as_ref().and_then(|selection| {
            graph_rows
                .iter()
                .position(|row| row.selection() == *selection)
        });
        let graph_start = effective_window_start(
            self.relationships.scroll,
            graph_selection,
            graph_rows.len(),
            row_height,
        );
        let relationship_rows = graph_rows
            .into_iter()
            .skip(graph_start)
            .take(row_height)
            .map(|row| match row {
                GraphRow::Status(status) => RelationshipRenderRow {
                    id: "status".to_owned(),
                    text: status,
                    is_group: false,
                    selected: graph_active
                        && self.relationships.selection == Some(GraphSelection::Status),
                },
                GraphRow::Group(group) => RelationshipRenderRow {
                    id: group.label.clone(),
                    text: group.label.clone(),
                    is_group: true,
                    selected: graph_active
                        && self.relationships.selection
                            == Some(GraphSelection::Group(group.label.clone())),
                },
                GraphRow::Relationship { relationship, .. } => RelationshipRenderRow {
                    id: relationship.id.clone(),
                    text: relationship.label.clone(),
                    is_group: false,
                    selected: graph_active
                        && self.relationships.selection
                            == Some(GraphSelection::Relationship(relationship.id.clone())),
                },
            })
            .collect::<Vec<_>>();
        let mut active_selections = Vec::with_capacity(1);
        match self.active_pane {
            DeclarationPane::Outline => active_selections.push(ActiveSelection {
                pane: DeclarationPane::Outline,
                row_id: self.outline.selection.clone(),
            }),
            DeclarationPane::Relationships => {
                if let Some(selection) = &self.relationships.selection {
                    let row_id = match selection {
                        GraphSelection::Status => "status".to_owned(),
                        GraphSelection::Group(label) => label.clone(),
                        GraphSelection::Relationship(id) => id.clone(),
                    };
                    active_selections.push(ActiveSelection {
                        pane: DeclarationPane::Relationships,
                        row_id,
                    });
                }
            }
        }
        let visible_text = self.visible_text(&layout, &outline_rows, &relationship_rows);
        DeclarationRenderModel {
            layout,
            outline_rows,
            relationship_groups,
            relationship_status,
            relationship_rows,
            title: format!("Declaration Review · {}", self.document().path),
            footer: match &self.comment_draft {
                Some(draft) if self.rejection_draft => format!("Reject: {draft}"),
                Some(draft) => format!("Comment: {draft}"),
                None => {
                    "[a]pprove [c]omment [r]eject [Tab]pane [o]relations [Backspace]back".to_owned()
                }
            },
            banner: if self.comment_draft.is_some() {
                "[Enter] submit · [Esc] cancel".to_owned()
            } else {
                "Declaration Review".to_owned()
            },
            active_selections,
            visible_text,
        }
    }

    fn is_wide(&self) -> bool {
        self.inner_width >= WIDE_LAYOUT_MIN_WIDTH
    }

    fn current_relationship_state(&self) -> RelationshipState {
        self.document().relationships_for(&self.active_declaration)
    }

    fn visible_outline_rows(&self) -> Vec<&OutlineRow> {
        self.document()
            .outline_rows
            .iter()
            .filter(|row| {
                self.outline_scope
                    .as_ref()
                    .is_none_or(|owner| &row.review_owner == owner)
                    && row
                        .parent
                        .as_ref()
                        .is_none_or(|parent| self.outline.expanded.contains(parent))
            })
            .collect()
    }

    fn normalize_view_state(&mut self) {
        self.ensure_outline_visible();
        let relationship_state = self.current_relationship_state();
        normalize_graph_selection(&mut self.relationships, &relationship_state);
        ensure_graph_selection_visible(
            &mut self.relationships,
            &relationship_state,
            usize::from(self.inner_height),
        );
    }

    fn ensure_outline_visible(&mut self) {
        let (selected_index, visible_len, first_id) = {
            let visible = self.visible_outline_rows();
            (
                visible
                    .iter()
                    .position(|row| row.id == self.outline.selection),
                visible.len(),
                visible.first().map(|row| row.id.clone()),
            )
        };
        let Some(index) = selected_index else {
            if let Some(first) = first_id {
                self.outline.selection = first;
                self.outline.scroll = 0;
            }
            return;
        };
        let height = usize::from(self.inner_height).max(1);
        if index < self.outline.scroll {
            self.outline.scroll = index;
        } else if index >= self.outline.scroll.saturating_add(height) {
            self.outline.scroll = index + 1 - height;
        }
        self.outline.scroll = self.outline.scroll.min(visible_len.saturating_sub(height));
    }

    fn select_outline_index(&mut self, index: usize) {
        let (id, owner) = {
            let visible = self.visible_outline_rows();
            let Some(row) = visible.get(index) else {
                return;
            };
            (row.id.clone(), row.review_owner.clone())
        };
        self.outline.selection = id;
        if self.active_declaration != owner {
            self.active_declaration = owner;
            self.relationships.scroll = 0;
            self.inspected_relationship = None;
            let state = self.current_relationship_state();
            normalize_graph_selection(&mut self.relationships, &state);
        }
        self.ensure_outline_visible();
        let state = self.current_relationship_state();
        ensure_graph_selection_visible(
            &mut self.relationships,
            &state,
            usize::from(self.inner_height),
        );
    }

    fn move_active_selection(&mut self, delta: isize) {
        if self.active_pane == DeclarationPane::Relationships {
            let state = self.current_relationship_state();
            move_graph_selection(
                &mut self.relationships,
                &state,
                delta,
                usize::from(self.inner_height),
            );
            self.inspected_relationship = None;
            return;
        }
        let visible = self.visible_outline_rows();
        let current = visible
            .iter()
            .position(|row| row.id == self.outline.selection)
            .unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta.unsigned_abs())
                .min(visible.len().saturating_sub(1))
        };
        self.select_outline_index(next);
    }

    fn move_to_edge(&mut self, end: bool) {
        if self.active_pane == DeclarationPane::Relationships {
            let state = self.current_relationship_state();
            move_graph_selection(
                &mut self.relationships,
                &state,
                if end { isize::MAX } else { isize::MIN },
                usize::from(self.inner_height),
            );
        } else {
            let index = if end {
                self.visible_outline_rows().len().saturating_sub(1)
            } else {
                0
            };
            self.select_outline_index(index);
        }
    }

    fn toggle_pane(&mut self) {
        self.active_pane = match self.active_pane {
            DeclarationPane::Outline => DeclarationPane::Relationships,
            DeclarationPane::Relationships => DeclarationPane::Outline,
        };
        self.inspected_relationship = None;
    }

    fn navigation_snapshot(&self) -> NavigationSnapshot {
        NavigationSnapshot {
            document: self.document_identity(),
            outline_scope: self.outline_scope.clone(),
            active_declaration: self.active_declaration.clone(),
            active_pane: self.active_pane,
            outline: self.outline.clone(),
            relationships: self.relationships.clone(),
            comment_draft: self.comment_draft.clone(),
            inspected_relationship: self.inspected_relationship.clone(),
            rejection_draft: self.rejection_draft,
            replacement_restore: self.replacement_restore.clone(),
        }
    }

    fn restore_navigation(&mut self, snapshot: NavigationSnapshot) -> Result<()> {
        self.active_document = self
            .document_index(&snapshot.document)
            .context("navigation snapshot document disappeared from the launch catalog")?;
        self.outline_scope = snapshot.outline_scope;
        self.active_declaration = snapshot.active_declaration;
        self.active_pane = snapshot.active_pane;
        self.outline = snapshot.outline;
        self.relationships = snapshot.relationships;
        self.comment_draft = snapshot.comment_draft;
        self.inspected_relationship = snapshot.inspected_relationship;
        self.rejection_draft = snapshot.rejection_draft;
        self.replacement_restore = snapshot.replacement_restore;
        Ok(())
    }

    fn open_relationship_replacement(&mut self) {
        if self.replacement_restore.is_none() {
            self.replacement_restore = Some(Box::new(self.navigation_snapshot()));
        }
        self.active_pane = DeclarationPane::Relationships;
        self.inspected_relationship = None;
    }

    fn go_back(&mut self) -> Result<()> {
        if let Some(snapshot) = self.back_stack.pop() {
            self.restore_navigation(snapshot)?;
        } else if let Some(snapshot) = self.replacement_restore.take() {
            self.restore_navigation(*snapshot)?;
        }
        Ok(())
    }

    fn activate_graph_selection(&mut self) {
        let Some(GraphSelection::Relationship(id)) = self.relationships.selection.clone() else {
            return;
        };
        let state = self.current_relationship_state();
        let Some(relationship) = state.relationship(&id) else {
            return;
        };
        match &relationship.destination {
            RelationshipDestination::InReview { declaration_id } => {
                let Some(document_index) = self.declaration_document_index(declaration_id) else {
                    return;
                };
                let Some(target) = self.documents[document_index].row(declaration_id) else {
                    return;
                };
                let owner = target.review_owner.clone();
                let selected_id = target.id.clone();
                let expanded = self.documents[document_index].initial_expanded.clone();
                let graph_selection = self.documents[document_index]
                    .initial_graph_selection
                    .clone()
                    .or(Some(GraphSelection::Status));
                self.back_stack.push(self.navigation_snapshot());
                self.active_document = document_index;
                if self.outline_scope.is_some() {
                    self.outline_scope = Some(owner.clone());
                }
                self.active_declaration = owner;
                self.outline.selection = selected_id;
                self.outline.scroll = 0;
                self.outline.expanded = expanded;
                self.inspected_relationship = None;
                self.relationships.selection = graph_selection;
                self.relationships.scroll = 0;
                self.relationships.collapsed_groups.clear();
                let target_state = self.current_relationship_state();
                normalize_graph_selection(&mut self.relationships, &target_state);
                self.ensure_outline_visible();
                ensure_graph_selection_visible(
                    &mut self.relationships,
                    &target_state,
                    usize::from(self.inner_height),
                );
            }
            RelationshipDestination::External { .. } | RelationshipDestination::Unresolved => {
                self.inspected_relationship = Some(id);
            }
        }
    }

    fn advance_canonical(&mut self) {
        let document = self.document();
        let current = document
            .canonical_order
            .iter()
            .position(|id| id == &self.active_declaration)
            .unwrap_or(0);
        let next = (current + 1).min(document.canonical_order.len().saturating_sub(1));
        let id = document.canonical_order[next].clone();
        self.active_declaration = id.clone();
        self.outline.selection = id;
        self.inspected_relationship = None;
        self.replacement_restore = None;
        self.ensure_outline_visible();
        let state = self.current_relationship_state();
        normalize_graph_selection(&mut self.relationships, &state);
        ensure_graph_selection_visible(
            &mut self.relationships,
            &state,
            usize::from(self.inner_height),
        );
    }

    fn selected_source_row(&self) -> Option<&OutlineRow> {
        if self.active_pane != DeclarationPane::Outline {
            return None;
        }
        self.document().row(&self.outline.selection)
    }

    fn anchor_for_row(&self, row: &OutlineRow) -> DeclarationAnchor {
        let document = self.document();
        DeclarationAnchor::new(
            document.snapshot_id.clone(),
            document.path.clone(),
            row.anchor_ranges
                .iter()
                .map(|range| (range.clone(), document.source_text(range).to_owned())),
        )
    }

    fn emit_approval(&mut self) {
        let Some(row) = self.selected_source_row() else {
            return;
        };
        let owner_id = row.review_owner.clone();
        let anchor = self.anchor_for_row(row);
        self.pending_actions.push(DeclarationReviewAction {
            kind: DeclarationReviewActionKind::Approve,
            owner_id,
            comment_body: None,
            anchor: Some(anchor),
        });
    }

    fn open_comment(&mut self) {
        if self.selected_source_row().is_some() {
            self.rejection_draft = false;
            self.comment_draft = Some(String::new());
        }
    }

    fn open_rejection(&mut self) {
        if self.selected_source_row().is_some() {
            self.rejection_draft = true;
            self.comment_draft = Some(String::new());
        }
    }

    fn handle_editor_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Enter => {
                let body = self.comment_draft.take().unwrap_or_default();
                if body.trim().is_empty() {
                    return Ok(());
                }
                let Some(row) = self.document().row(&self.outline.selection).cloned() else {
                    bail!("comment source row disappeared")
                };
                let anchor = self.anchor_for_row(&row);
                let kind = if self.rejection_draft {
                    DeclarationReviewActionKind::Reject
                } else {
                    DeclarationReviewActionKind::Comment
                };
                self.rejection_draft = false;
                self.pending_actions.push(DeclarationReviewAction {
                    kind,
                    owner_id: row.review_owner,
                    comment_body: Some(body),
                    anchor: Some(anchor),
                });
            }
            KeyCode::Esc => {
                self.comment_draft = None;
                self.rejection_draft = false;
            }
            KeyCode::Backspace => {
                if let Some(draft) = &mut self.comment_draft {
                    draft.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(draft) = &mut self.comment_draft {
                    draft.push(character);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn collapse_selected(&mut self) {
        let selected = self.outline.selection.clone();
        if self.outline.expanded.remove(&selected) {
            self.ensure_outline_visible();
        } else if let Some(parent) = self
            .document()
            .row(&selected)
            .and_then(|row| row.parent.clone())
            && let Some(index) = self
                .visible_outline_rows()
                .iter()
                .position(|row| row.id == parent)
        {
            self.select_outline_index(index);
        }
    }

    fn expand_selected(&mut self) {
        let selected = self.outline.selection.clone();
        if self
            .document()
            .outline_rows
            .iter()
            .any(|row| row.parent.as_deref() == Some(&selected))
        {
            self.outline.expanded.insert(selected);
            self.ensure_outline_visible();
        } else if !self.is_wide() {
            self.open_relationship_replacement();
        }
    }

    fn move_graph_group(&mut self, delta: isize) {
        let state = self.current_relationship_state();
        let rows = graph_rows(&state, &self.relationships.collapsed_groups);
        let groups = rows
            .iter()
            .filter_map(|row| match row {
                GraphRow::Group(group) => Some(GraphSelection::Group(group.label.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if groups.is_empty() {
            return;
        }
        let current = self
            .relationships
            .selection
            .as_ref()
            .and_then(|selection| groups.iter().position(|group| group == selection))
            .unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta.unsigned_abs())
                .min(groups.len() - 1)
        };
        self.relationships.selection = Some(groups[next].clone());
        ensure_graph_selection_visible(
            &mut self.relationships,
            &state,
            usize::from(self.inner_height),
        );
    }

    fn visible_text(
        &self,
        layout: &DeclarationLayout,
        outline_rows: &[OutlineRenderRow],
        relationship_rows: &[RelationshipRenderRow],
    ) -> String {
        let mut lines = vec![format!("Declaration Review · {}", self.document().path)];
        let show_outline = matches!(layout, DeclarationLayout::Split { .. })
            || matches!(
                layout,
                DeclarationLayout::Single {
                    pane: DeclarationPane::Outline,
                    ..
                }
            );
        let show_graph = matches!(layout, DeclarationLayout::Split { .. })
            || matches!(
                layout,
                DeclarationLayout::Single {
                    pane: DeclarationPane::Relationships,
                    ..
                }
            );
        if show_outline {
            lines.push("OUTLINE".to_owned());
            lines.extend(
                outline_rows
                    .iter()
                    .map(|row| format!("{}{}", row.source_text, row.status.marker())),
            );
        }
        if show_graph {
            lines.push("RELATIONSHIPS".to_owned());
            lines.extend(relationship_rows.iter().map(|row| row.text.clone()));
        }
        if let Some(draft) = &self.comment_draft {
            let label = if self.rejection_draft {
                "Reject"
            } else {
                "Comment"
            };
            lines.push(format!("{label}: {draft}"));
        }
        lines
            .push("[a]pprove [c]omment [r]eject [Tab]pane [o]relations [Backspace]back".to_owned());
        lines.push("Declaration Review".to_owned());
        lines.join("\n")
    }
}

fn effective_window_start(
    saved_scroll: usize,
    selection: Option<usize>,
    row_count: usize,
    height: usize,
) -> usize {
    if height == 0 {
        return saved_scroll.min(row_count);
    }
    let mut start = saved_scroll.min(row_count.saturating_sub(height));
    if let Some(selection) = selection {
        if selection < start {
            start = selection;
        } else if selection >= start.saturating_add(height) {
            start = selection + 1 - height;
        }
    }
    start.min(row_count.saturating_sub(height))
}

pub fn layout_for(inner_width: u16, active_pane: DeclarationPane) -> DeclarationLayout {
    if inner_width < WIDE_LAYOUT_MIN_WIDTH {
        return DeclarationLayout::Single {
            pane: active_pane,
            width: inner_width,
        };
    }
    let available = inner_width.saturating_sub(1);
    let outline_width = u16::try_from((u32::from(available) * 58) / 100).unwrap_or(available);
    DeclarationLayout::Split {
        outline_width,
        divider_width: 1,
        relationship_width: available.saturating_sub(outline_width),
    }
}

/// Renders the complete declaration surface, including its body-free title, footer, and banner.
pub fn render_declaration_review(
    frame: &mut Frame<'_>,
    area: Rect,
    controller: &DeclarationController,
) {
    let row_height = if area.height >= 4 {
        area.height - 4
    } else {
        area.height.saturating_sub(1)
    };
    let model = controller.render_model_for(area.width, row_height);
    if area.height < 4 {
        render_model(area, frame.buffer_mut(), &model);
        return;
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let panes = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height - 3,
    );
    let footer = Rect::new(
        area.x,
        area.y.saturating_add(area.height - 2),
        area.width,
        1,
    );
    let banner = Rect::new(
        area.x,
        area.y.saturating_add(area.height - 1),
        area.width,
        1,
    );
    frame.render_widget(
        Paragraph::new(model.title.as_str()).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    render_model(panes, frame.buffer_mut(), &model);
    frame.render_widget(
        Paragraph::new(model.footer.as_str()).style(Style::default().fg(Color::Gray)),
        footer,
    );
    frame.render_widget(
        Paragraph::new(model.banner.as_str())
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Cyan)),
        banner,
    );
}

pub fn render_model(area: Rect, buffer: &mut Buffer, model: &DeclarationRenderModel) {
    match model.layout {
        DeclarationLayout::Split {
            outline_width,
            divider_width,
            relationship_width,
        } => {
            let outline = Rect::new(area.x, area.y, outline_width.min(area.width), area.height);
            let divider_x = area.x.saturating_add(outline.width);
            let divider = Rect::new(
                divider_x,
                area.y,
                divider_width.min(area.width.saturating_sub(outline.width)),
                area.height,
            );
            let relationship = Rect::new(
                divider_x.saturating_add(divider.width),
                area.y,
                relationship_width.min(
                    area.width
                        .saturating_sub(outline.width)
                        .saturating_sub(divider.width),
                ),
                area.height,
            );
            render_outline_pane(
                outline,
                buffer,
                &model.outline_rows,
                true_if_active(model, DeclarationPane::Outline),
            );
            if divider.width > 0 {
                Paragraph::new(vec![Line::from("│"); usize::from(area.height)])
                    .style(Style::default().fg(Color::DarkGray))
                    .render(divider, buffer);
            }
            graph::render_relationship_pane(
                relationship,
                buffer,
                &model.relationship_rows,
                true_if_active(model, DeclarationPane::Relationships),
            );
        }
        DeclarationLayout::Single { pane, .. } => match pane {
            DeclarationPane::Outline => {
                render_outline_pane(area, buffer, &model.outline_rows, true);
            }
            DeclarationPane::Relationships => {
                graph::render_relationship_pane(area, buffer, &model.relationship_rows, true);
            }
        },
    }
}

fn true_if_active(model: &DeclarationRenderModel, pane: DeclarationPane) -> bool {
    model
        .active_selections
        .first()
        .is_some_and(|selection| selection.pane == pane)
}

pub fn render_outline_pane(
    area: Rect,
    buffer: &mut Buffer,
    rows: &[OutlineRenderRow],
    active: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let heading_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    Paragraph::new("OUTLINE")
        .style(heading_style)
        .render(Rect::new(area.x, area.y, area.width, 1), buffer);
    let content = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    let normal = if active {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let selected = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let lines = rows
        .iter()
        .map(|row| {
            let row_style = if active && row.selected {
                selected
            } else {
                normal
            };
            let prefix = if active && row.selected { "> " } else { "  " };
            let indent = "  ".repeat(row.depth);
            Line::from(vec![
                Span::styled(prefix, row_style),
                Span::styled(indent, row_style),
                Span::styled(&row.source_text, row_style),
                Span::styled(row.status.marker(), normal),
            ])
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines).render(content, buffer);
}

#[cfg(feature = "tui-test-support")]
#[doc(hidden)]
pub mod tui_test_support {
    use super::{
        BTreeMap, BTreeSet, DeclarationAnchor, DeclarationController, DeclarationDocument,
        DeclarationLayout, DeclarationRenderModel, DeclarationReviewAction,
        DeclarationReviewActionKind, DeclarationStateSnapshot, KeyCode, OutlineRow, Relationship,
        RelationshipDestination, RelationshipGroup, RelationshipState, Result, ensure,
    };
    pub use super::{DeclarationPane, GraphSelection};

    pub type TestOutlineRow = OutlineRow;
    pub type TestRelationship = Relationship;
    pub type TestRelationshipGroup = RelationshipGroup;
    pub type TestRelationshipState = RelationshipState;
    pub type TestDeclarationAnchor = DeclarationAnchor;
    pub type TestReviewAction = DeclarationReviewAction;
    pub type TestReviewActionKind = DeclarationReviewActionKind;
    pub type TestLayout = DeclarationLayout;
    pub type TestDeclarationState = DeclarationStateSnapshot;
    pub type TestRenderedDeclaration = DeclarationRenderModel;

    impl Relationship {
        pub fn in_review(
            id: impl Into<String>,
            label: impl Into<String>,
            declaration_id: impl Into<String>,
        ) -> Self {
            Self {
                id: id.into(),
                label: label.into(),
                destination: RelationshipDestination::InReview {
                    declaration_id: declaration_id.into(),
                },
            }
        }

        pub fn external(
            id: impl Into<String>,
            label: impl Into<String>,
            context: impl Into<String>,
        ) -> Self {
            Self {
                id: id.into(),
                label: label.into(),
                destination: RelationshipDestination::External {
                    context: context.into(),
                },
            }
        }

        pub fn unresolved(id: impl Into<String>, label: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                label: label.into(),
                destination: RelationshipDestination::Unresolved,
            }
        }
    }

    impl RelationshipGroup {
        pub fn new(label: impl Into<String>, relationships: Vec<Relationship>) -> Self {
            Self {
                label: label.into(),
                relationships,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct TestDeclarationFixture {
        document: DeclarationDocument,
    }

    impl TestDeclarationFixture {
        pub fn new(
            snapshot_id: impl Into<String>,
            path: impl Into<String>,
            exact_source: impl Into<String>,
        ) -> Self {
            Self {
                document: DeclarationDocument {
                    snapshot_id: snapshot_id.into(),
                    path: path.into(),
                    exact_source: exact_source.into(),
                    outline_rows: Vec::new(),
                    canonical_order: Vec::new(),
                    relationships: BTreeMap::new(),
                    initial_outline_selection: None,
                    initial_expanded: BTreeSet::new(),
                    initial_graph_selection: None,
                },
            }
        }

        pub fn with_outline_rows(mut self, rows: Vec<TestOutlineRow>) -> Self {
            self.document.outline_rows = rows;
            self
        }

        pub fn with_canonical_order(
            mut self,
            order: impl IntoIterator<Item = impl Into<String>>,
        ) -> Self {
            self.document.canonical_order = order.into_iter().map(Into::into).collect();
            self
        }

        pub fn with_relationship_state(
            mut self,
            declaration_id: impl Into<String>,
            state: TestRelationshipState,
        ) -> Self {
            self.document
                .relationships
                .insert(declaration_id.into(), state);
            self
        }

        pub fn with_initial_outline_selection(mut self, id: impl Into<String>) -> Self {
            self.document.initial_outline_selection = Some(id.into());
            self
        }

        pub fn with_initial_expanded(
            mut self,
            ids: impl IntoIterator<Item = impl Into<String>>,
        ) -> Self {
            self.document.initial_expanded = ids.into_iter().map(Into::into).collect();
            self
        }

        pub fn with_initial_graph_selection(mut self, selection: GraphSelection) -> Self {
            self.document.initial_graph_selection = Some(selection);
            self
        }
    }

    #[derive(Debug, Clone)]
    pub struct DeclarationTestApp {
        controller: DeclarationController,
    }

    impl DeclarationTestApp {
        pub fn new(
            fixture: TestDeclarationFixture,
            inner_width: u16,
            inner_height: u16,
        ) -> Result<Self> {
            Ok(Self {
                controller: DeclarationController::new(
                    fixture.document,
                    inner_width,
                    inner_height,
                )?,
            })
        }

        pub fn press(&mut self, key: KeyCode) -> Result<()> {
            self.controller.handle_key(key)
        }

        pub fn type_text(&mut self, text: &str) -> Result<()> {
            ensure!(
                self.controller.comment_draft.is_some(),
                "comment editor is not open"
            );
            self.controller.insert_text(text);
            Ok(())
        }

        pub fn resize(&mut self, inner_width: u16, inner_height: u16) -> Result<()> {
            self.controller.resize(inner_width, inner_height);
            Ok(())
        }

        pub fn render(&mut self) -> Result<TestRenderedDeclaration> {
            Ok(self.controller.render_model())
        }

        pub fn state_snapshot(&self) -> TestDeclarationState {
            self.controller.state_snapshot()
        }

        pub fn take_review_actions(&mut self) -> Vec<TestReviewAction> {
            self.controller.take_actions()
        }
    }
}
