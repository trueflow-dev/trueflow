use std::collections::BTreeSet;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

/// Stable selection in the advisory relationship pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphSelection {
    Status,
    Group(String),
    Relationship(String),
}

/// Where a relationship points. Only `InReview` can navigate to another review target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationshipDestination {
    InReview { declaration_id: String },
    External { context: String },
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    pub label: String,
    pub destination: RelationshipDestination,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipGroup {
    pub label: String,
    pub relationships: Vec<Relationship>,
}

/// A normalized relationship-provider result consumed by the presentation controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationshipState {
    Checking,
    NoRelationships,
    Ready(Vec<RelationshipGroup>),
    Partial {
        reason: String,
        groups: Vec<RelationshipGroup>,
    },
    Unavailable {
        reason: String,
    },
}

impl RelationshipState {
    pub fn status_text(&self) -> Option<String> {
        match self {
            Self::Checking => Some("Checking…".to_owned()),
            Self::NoRelationships => Some("No relationships found".to_owned()),
            Self::Ready(_) => None,
            Self::Partial { reason, .. } => Some(format!("Partial — {reason}")),
            Self::Unavailable { reason } => Some(format!("Unavailable — {reason}")),
        }
    }

    pub fn groups(&self) -> &[RelationshipGroup] {
        match self {
            Self::Ready(groups) | Self::Partial { groups, .. } => groups,
            Self::Checking | Self::NoRelationships | Self::Unavailable { .. } => &[],
        }
    }

    pub fn relationship(&self, id: &str) -> Option<&Relationship> {
        self.groups()
            .iter()
            .flat_map(|group| &group.relationships)
            .find(|relationship| relationship.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPaneState {
    pub selection: Option<GraphSelection>,
    pub scroll: usize,
    pub collapsed_groups: BTreeSet<String>,
}

impl Default for GraphPaneState {
    fn default() -> Self {
        Self {
            selection: Some(GraphSelection::Status),
            scroll: 0,
            collapsed_groups: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipRenderRow {
    pub id: String,
    pub text: String,
    pub is_group: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipRenderGroup {
    pub label: String,
    pub relationships: Vec<RelationshipRenderRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphRow<'a> {
    Status(String),
    Group(&'a RelationshipGroup),
    Relationship {
        group: &'a RelationshipGroup,
        relationship: &'a Relationship,
    },
}

impl GraphRow<'_> {
    pub(crate) fn selection(&self) -> GraphSelection {
        match self {
            Self::Status(_) => GraphSelection::Status,
            Self::Group(group) => GraphSelection::Group(group.label.clone()),
            Self::Relationship { relationship, .. } => {
                GraphSelection::Relationship(relationship.id.clone())
            }
        }
    }
}

pub(crate) fn graph_rows<'a>(
    state: &'a RelationshipState,
    collapsed_groups: &BTreeSet<String>,
) -> Vec<GraphRow<'a>> {
    let mut rows = Vec::new();
    if let Some(status) = state.status_text() {
        rows.push(GraphRow::Status(status));
    }
    for group in state.groups() {
        rows.push(GraphRow::Group(group));
        if !collapsed_groups.contains(&group.label) {
            rows.extend(
                group
                    .relationships
                    .iter()
                    .map(|relationship| GraphRow::Relationship {
                        group,
                        relationship,
                    }),
            );
        }
    }
    rows
}

pub(crate) fn selected_index(rows: &[GraphRow<'_>], selection: &GraphSelection) -> Option<usize> {
    rows.iter().position(|row| row.selection() == *selection)
}

pub(crate) fn normalize_selection(
    pane: &mut GraphPaneState,
    relationship_state: &RelationshipState,
) {
    let rows = graph_rows(relationship_state, &pane.collapsed_groups);
    if rows.is_empty() {
        pane.selection = None;
        pane.scroll = 0;
        return;
    }
    if pane
        .selection
        .as_ref()
        .and_then(|selection| selected_index(&rows, selection))
        .is_none()
    {
        pane.selection = Some(rows[0].selection());
    }
}

pub(crate) fn move_selection(
    pane: &mut GraphPaneState,
    relationship_state: &RelationshipState,
    delta: isize,
    height: usize,
) {
    let rows = graph_rows(relationship_state, &pane.collapsed_groups);
    if rows.is_empty() {
        pane.selection = None;
        pane.scroll = 0;
        return;
    }
    let current = pane
        .selection
        .as_ref()
        .and_then(|selection| selected_index(&rows, selection))
        .unwrap_or(0);
    let last = rows.len().saturating_sub(1);
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(last)
    };
    pane.selection = Some(rows[next].selection());
    ensure_visible(pane, next, rows.len(), height);
}

pub(crate) fn ensure_selection_visible(
    pane: &mut GraphPaneState,
    relationship_state: &RelationshipState,
    height: usize,
) {
    let rows = graph_rows(relationship_state, &pane.collapsed_groups);
    normalize_selection(pane, relationship_state);
    let Some(index) = pane
        .selection
        .as_ref()
        .and_then(|selection| selected_index(&rows, selection))
    else {
        pane.scroll = 0;
        return;
    };
    ensure_visible(pane, index, rows.len(), height);
}

fn ensure_visible(pane: &mut GraphPaneState, index: usize, row_count: usize, height: usize) {
    let height = height.max(1);
    if index < pane.scroll {
        pane.scroll = index;
    } else if index >= pane.scroll.saturating_add(height) {
        pane.scroll = index + 1 - height;
    }
    pane.scroll = pane.scroll.min(row_count.saturating_sub(height));
}

pub(crate) fn semantic_groups(
    relationship_state: &RelationshipState,
    selection: Option<&GraphSelection>,
    active: bool,
) -> Vec<RelationshipRenderGroup> {
    relationship_state
        .groups()
        .iter()
        .map(|group| RelationshipRenderGroup {
            label: group.label.clone(),
            relationships: group
                .relationships
                .iter()
                .map(|relationship| RelationshipRenderRow {
                    id: relationship.id.clone(),
                    text: relationship.label.clone(),
                    is_group: false,
                    selected: active
                        && selection
                            == Some(&GraphSelection::Relationship(relationship.id.clone())),
                })
                .collect(),
        })
        .collect()
}

/// Renders a relationship pane from its already-normalized semantic rows.
pub fn render_relationship_pane(
    area: Rect,
    buffer: &mut Buffer,
    rows: &[RelationshipRenderRow],
    active: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let heading_style = if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
    };
    Paragraph::new("RELATIONSHIPS")
        .style(heading_style)
        .render(Rect::new(area.x, area.y, area.width, 1), buffer);
    let content = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    let inactive = Style::default().fg(Color::DarkGray);
    let normal = Style::default().fg(Color::Gray);
    let selected = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let lines = rows
        .iter()
        .map(|row| {
            let prefix = if row.selected && active { "> " } else { "  " };
            let style = if row.selected && active {
                selected
            } else if !active {
                inactive
            } else if row.is_group {
                normal.add_modifier(Modifier::BOLD)
            } else {
                normal
            };
            Line::from(vec![Span::styled(prefix, style), Span::styled(&row.text, style)])
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines).render(content, buffer);
}
