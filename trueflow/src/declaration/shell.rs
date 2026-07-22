use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::analysis::Language;

use super::projection::{declaration_id, declaration_key};
use super::{
    DeclarationKind, DeclarationNode, ProjectionDiagnostic, SourceComponent, SourceComponentRole,
    Visibility, projection_hash,
};

pub(super) fn project(
    path: &Path,
    source: &str,
) -> Result<(Vec<DeclarationNode>, Vec<ProjectionDiagnostic>)> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .context("failed to load the shell grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a shell syntax tree")?;

    let mut projector = Projector {
        path,
        source,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
    };
    projector.collect(tree.root_node());
    if tree.root_node().has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "shell source contains syntax errors; declarations with invalid signatures were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

struct Projector<'a> {
    path: &'a Path,
    source: &'a str,
    declarations: Vec<DeclarationNode>,
    diagnostics: Vec<ProjectionDiagnostic>,
}

impl Projector<'_> {
    fn collect(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "function_definition" {
                self.add_function(child);
            } else {
                self.collect(child);
            }
        }
    }

    fn add_function(&mut self, node: Node<'_>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted shell function at byte {} because it has no reliable name",
                node.start_byte()
            )));
            return;
        };
        let Some(body) = node.child_by_field_name("body") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted shell function at byte {} because it has no body boundary",
                node.start_byte()
            )));
            return;
        };
        let Some(name) = self.source.get(name_node.byte_range()) else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted shell function at byte {} because its name is not on a UTF-8 boundary",
                node.start_byte()
            )));
            return;
        };

        let signature_range =
            trimmed_signature_range(self.source, node.start_byte()..body.start_byte());
        if signature_range.is_empty() || has_error_before(node, body.start_byte()) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted shell function {name} because its signature contains a syntax error"
            )));
            return;
        }

        let mut components = Vec::with_capacity(3);
        if let Some(documentation_range) = leading_comment_range(self.source, node.start_byte()) {
            components.push(SourceComponent {
                text: self.source[documentation_range.clone()].to_owned(),
                source_range: documentation_range.clone(),
                role: SourceComponentRole::Documentation,
            });
            if documentation_range.end < signature_range.start {
                let layout_range = documentation_range.end..signature_range.start;
                components.push(SourceComponent {
                    text: self.source[layout_range.clone()].to_owned(),
                    source_range: layout_range,
                    role: SourceComponentRole::Layout,
                });
            }
        }
        components.push(SourceComponent {
            text: self.source[signature_range.clone()].to_owned(),
            source_range: signature_range,
            role: SourceComponentRole::Signature,
        });

        let kind = DeclarationKind::Function;
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let projection_hash = projection_hash(Language::Shell, kind, &components);
        let source_ordinal = self.declarations.len();
        let source_start = components
            .first()
            .map_or(node.start_byte(), |component| component.source_range.start);
        let source_span = source_start..node.end_byte();
        let id = declaration_id(
            self.path,
            kind,
            name,
            source_ordinal,
            source_span.start,
            &projection_hash,
        );
        let key = declaration_key(Language::Shell, kind, name, None, "");
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name: name.to_owned(),
            kind,
            visibility: Visibility::Implicit,
            parent_part: None,
            source_ordinal,
            source_span,
            components,
            projection_text,
            projection_hash,
            review_owner: id,
            children: Vec::new(),
            type_use_sites: Vec::new(),
        });
    }
}

fn trimmed_signature_range(source: &str, range: Range<usize>) -> Range<usize> {
    let Some(signature) = source.get(range.clone()) else {
        return range.start..range.start;
    };
    range.start..range.start + signature.trim_end().len()
}

fn leading_comment_range(source: &str, declaration_start: usize) -> Option<Range<usize>> {
    let prefix = source.get(..declaration_start)?;
    let mut end = prefix.trim_end_matches([' ', '\t', '\r', '\n']).len();
    if end == 0 || prefix[end..].matches('\n').count() > 1 {
        return None;
    }

    let mut start = end;
    loop {
        let line_start = prefix[..start].rfind('\n').map_or(0, |newline| newline + 1);
        let line = &prefix[line_start..start];
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') || trimmed.starts_with("#!") {
            break;
        }
        start = line_start;
        if start == 0 {
            break;
        }
        end = end.max(start);
        let previous_end = start - 1;
        if prefix[..previous_end].ends_with('\n') {
            break;
        }
        start = previous_end;
    }

    (start < end).then_some(start..end)
}

fn has_error_before(node: Node<'_>, end: usize) -> bool {
    if node.start_byte() < end && (node.is_error() || node.is_missing()) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .take_while(|child| child.start_byte() < end)
        .any(|child| has_error_before(child, end))
}
