use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::analysis::Language;

use super::projection::{declaration_id, declaration_key};
use super::{
    DeclarationId, DeclarationKind, DeclarationNode, ProjectionDiagnostic, SourceComponent,
    SourceComponentRole, TypeUseRole, TypeUseSite, Visibility, projection_hash,
};

#[derive(Debug, Clone)]
struct SemanticRange {
    range: Range<usize>,
    role: SourceComponentRole,
}

#[derive(Debug, Clone)]
struct CommentRange {
    raw: Range<usize>,
    exclusion: Range<usize>,
}

#[derive(Debug)]
struct SpecEntry<'tree> {
    node: Node<'tree>,
    documentation: Vec<Range<usize>>,
}

struct Projector<'a> {
    path: &'a Path,
    source: &'a str,
    comments: Vec<CommentRange>,
    next_ordinal: usize,
    declarations: Vec<DeclarationNode>,
    diagnostics: Vec<ProjectionDiagnostic>,
    pending_method_parents: Vec<(usize, String)>,
}

pub(super) fn project(
    path: &Path,
    source: &str,
) -> Result<(Vec<DeclarationNode>, Vec<ProjectionDiagnostic>)> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .context("failed to load the Go grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a Go syntax tree")?;

    let mut comments = Vec::new();
    collect_comments(tree.root_node(), source, &mut comments);
    let mut projector = Projector {
        path,
        source,
        comments,
        next_ordinal: 0,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
        pending_method_parents: Vec::new(),
    };
    projector.collect_file(tree.root_node())?;
    projector.resolve_method_lineage();
    if tree.root_node().has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "Go source contains syntax errors; declarations with errors in projected surfaces were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect_file(&mut self, root: Node<'_>) -> Result<()> {
        let mut pending_comments = Vec::<Range<usize>>::new();
        let mut previous_item_end_row = None;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "comment" {
                if previous_item_end_row == Some(child.start_position().row) {
                    pending_comments.clear();
                } else {
                    pending_comments.push(child.byte_range());
                }
                continue;
            }

            let documentation =
                trailing_comment_group(self.source, &pending_comments, child.start_byte());
            pending_comments.clear();
            match child.kind() {
                "function_declaration" => {
                    self.add_callable(child, DeclarationKind::Function, documentation)?;
                }
                "method_declaration" => {
                    self.add_callable(child, DeclarationKind::Method, documentation)?;
                }
                "type_declaration" => {
                    self.add_type_declaration(child, &documentation)?;
                }
                "const_declaration" => {
                    self.add_value_declaration(child, DeclarationKind::Constant, &documentation)?;
                }
                "var_declaration" => {
                    self.add_value_declaration(child, DeclarationKind::Static, &documentation)?;
                }
                _ => {}
            }
            previous_item_end_row = Some(child.end_position().row);
        }
        Ok(())
    }

    fn add_callable(
        &mut self,
        item: Node<'_>,
        kind: DeclarationKind,
        documentation: Vec<Range<usize>>,
    ) -> Result<()> {
        let Some(name_node) = item.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} at byte {} because it has no declared name",
                item.start_byte()
            )));
            return Ok(());
        };
        let name = node_text(name_node, self.source)
            .context("Go callable name was not on UTF-8 boundaries")?
            .to_owned();
        let signature_end = item
            .child_by_field_name("body")
            .map_or(item.end_byte(), |body| body.start_byte());
        let Some(surface) = trim_range_end(self.source, item.start_byte()..signature_end) else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} {name} because its projected signature is empty"
            )));
            return Ok(());
        };

        let mut semantic_ranges = documentation_ranges(documentation);
        semantic_ranges.push(SemanticRange {
            range: surface.clone(),
            role: SourceComponentRole::Signature,
        });
        let mut type_use_sites = Vec::new();
        if let Some(type_parameters) = item.child_by_field_name("type_parameters") {
            collect_type_identifiers(
                type_parameters,
                self.source,
                TypeUseRole::Bound,
                &mut type_use_sites,
            )?;
        }
        if let Some(receiver) = item.child_by_field_name("receiver") {
            collect_type_identifiers(
                receiver,
                self.source,
                TypeUseRole::Parameter,
                &mut type_use_sites,
            )?;
        }
        if let Some(parameters) = item.child_by_field_name("parameters") {
            collect_type_identifiers(
                parameters,
                self.source,
                TypeUseRole::Parameter,
                &mut type_use_sites,
            )?;
        }
        if let Some(result) = item.child_by_field_name("result") {
            collect_type_identifiers(
                result,
                self.source,
                TypeUseRole::Return,
                &mut type_use_sites,
            )?;
        }
        normalize_type_use_sites(&mut type_use_sites);

        let parent_name = item
            .child_by_field_name("receiver")
            .and_then(receiver_base_type_identifier)
            .and_then(|receiver| node_text(receiver, self.source))
            .map(str::to_owned);
        if kind == DeclarationKind::Method && parent_name.is_none() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "Go Method {name} at byte {} has no representable local receiver base type; aggregate lineage was omitted",
                item.start_byte()
            )));
        }
        let index = self.finish_declaration(
            item,
            name,
            kind,
            semantic_ranges,
            item.end_byte(),
            None,
            parent_name.as_deref(),
            type_use_sites,
            surface,
            &[],
        )?;
        if let (Some(index), Some(parent_name)) = (index, parent_name) {
            self.pending_method_parents.push((index, parent_name));
        }
        Ok(())
    }

    fn add_type_declaration(
        &mut self,
        declaration: Node<'_>,
        declaration_documentation: &[Range<usize>],
    ) -> Result<()> {
        let grouped = is_grouped_declaration(declaration);
        let specs = collect_specs(declaration, &["type_spec", "type_alias"], self.source);
        let ungrouped_single = !grouped && specs.len() == 1;
        if grouped && !declaration_documentation.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(
                "Go grouped type declaration documentation cannot be assigned to one spec without ambiguous ownership",
            ));
        }

        for entry in specs {
            let spec = entry.node;
            let Some(name_node) = spec.child_by_field_name("name") else {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Go type declaration at byte {} because it has no declared name",
                    spec.start_byte()
                )));
                continue;
            };
            let name = node_text(name_node, self.source)
                .context("Go type declaration name was not on UTF-8 boundaries")?
                .to_owned();
            let Some(target) = spec.child_by_field_name("type") else {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Go type declaration at byte {} because it has no target type",
                    spec.start_byte()
                )));
                continue;
            };
            let kind = if spec.kind() == "type_alias" {
                DeclarationKind::TypeAlias
            } else {
                match target.kind() {
                    "struct_type" => DeclarationKind::Struct,
                    "interface_type" => DeclarationKind::Interface,
                    _ => DeclarationKind::TypeAlias,
                }
            };
            let item = if ungrouped_single { declaration } else { spec };
            let mut semantic_ranges = if ungrouped_single {
                documentation_ranges(declaration_documentation.iter().cloned())
            } else {
                documentation_ranges(entry.documentation)
            };
            let role = match kind {
                DeclarationKind::Struct | DeclarationKind::Interface => {
                    SourceComponentRole::AggregateShape
                }
                _ => SourceComponentRole::TypeAlias,
            };
            semantic_ranges.push(SemanticRange {
                range: item.byte_range(),
                role,
            });
            if matches!(kind, DeclarationKind::Struct | DeclarationKind::Interface) {
                semantic_ranges.extend(
                    collect_aggregate_documentation(target, self.source)
                        .into_iter()
                        .map(|range| SemanticRange {
                            range,
                            role: SourceComponentRole::Documentation,
                        }),
                );
            }

            let interface_methods = if kind == DeclarationKind::Interface {
                collect_interface_methods(target, self.source)
            } else {
                Vec::new()
            };
            let exclusions = interface_methods
                .iter()
                .map(|entry| {
                    let start = entry
                        .documentation
                        .first()
                        .map_or(entry.node.start_byte(), |range| range.start);
                    member_line_exclusion(self.source, start..entry.node.end_byte())
                })
                .collect::<Vec<_>>();

            let mut type_use_sites = Vec::new();
            if let Some(type_parameters) = spec.child_by_field_name("type_parameters") {
                collect_type_identifiers(
                    type_parameters,
                    self.source,
                    TypeUseRole::Bound,
                    &mut type_use_sites,
                )?;
            }
            match kind {
                DeclarationKind::Struct => {
                    collect_struct_type_uses(target, self.source, &mut type_use_sites)?;
                }
                DeclarationKind::Interface => {
                    collect_interface_type_uses(target, self.source, &mut type_use_sites)?;
                }
                _ => {
                    collect_type_identifiers(
                        target,
                        self.source,
                        TypeUseRole::AliasTarget,
                        &mut type_use_sites,
                    )?;
                }
            }
            normalize_type_use_sites(&mut type_use_sites);
            let Some(parent_index) = self.finish_declaration(
                item,
                name.clone(),
                kind,
                semantic_ranges,
                item.end_byte(),
                None,
                None,
                type_use_sites,
                item.byte_range(),
                &exclusions,
            )?
            else {
                continue;
            };

            if !interface_methods.is_empty() {
                let parent_id = self.declarations[parent_index].id.clone();
                let mut children = Vec::new();
                for method in interface_methods {
                    if let Some(child_index) =
                        self.add_interface_method(method, &parent_id, &name)?
                    {
                        children.push(self.declarations[child_index].id.clone());
                    }
                }
                self.declarations[parent_index].children = children;
            }
        }
        Ok(())
    }

    fn add_interface_method(
        &mut self,
        entry: SpecEntry<'_>,
        parent_id: &DeclarationId,
        parent_name: &str,
    ) -> Result<Option<usize>> {
        let method = entry.node;
        let Some(name_node) = method.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go interface method at byte {} because it has no declared name",
                method.start_byte()
            )));
            return Ok(None);
        };
        let name = node_text(name_node, self.source)
            .context("Go interface method name was not on UTF-8 boundaries")?
            .to_owned();
        let mut semantic_ranges = documentation_ranges(entry.documentation);
        semantic_ranges.push(SemanticRange {
            range: method.byte_range(),
            role: SourceComponentRole::Signature,
        });
        let mut type_use_sites = Vec::new();
        if let Some(parameters) = method.child_by_field_name("parameters") {
            collect_type_identifiers(
                parameters,
                self.source,
                TypeUseRole::Parameter,
                &mut type_use_sites,
            )?;
        }
        if let Some(result) = method.child_by_field_name("result") {
            collect_type_identifiers(
                result,
                self.source,
                TypeUseRole::Return,
                &mut type_use_sites,
            )?;
        }
        normalize_type_use_sites(&mut type_use_sites);
        self.finish_declaration(
            method,
            name,
            DeclarationKind::Method,
            semantic_ranges,
            method.end_byte(),
            Some(parent_id.clone()),
            Some(parent_name),
            type_use_sites,
            method.byte_range(),
            &[],
        )
    }

    fn add_value_declaration(
        &mut self,
        declaration: Node<'_>,
        kind: DeclarationKind,
        declaration_documentation: &[Range<usize>],
    ) -> Result<()> {
        let spec_kind = if kind == DeclarationKind::Constant {
            "const_spec"
        } else {
            "var_spec"
        };
        let grouped = is_grouped_declaration(declaration);
        let specs = collect_specs(declaration, &[spec_kind], self.source);
        let ungrouped_single = !grouped && specs.len() == 1;
        if grouped && !declaration_documentation.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "Go grouped {kind:?} declaration documentation cannot be assigned to one spec without ambiguous ownership"
            )));
        }

        for entry in specs {
            let spec = entry.node;
            if kind == DeclarationKind::Constant
                && spec.child_by_field_name("type").is_none()
                && spec.child_by_field_name("value").is_none()
            {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Go const spec at byte {} because it inherits an earlier expression and has no exact standalone projection",
                    spec.start_byte()
                )));
                continue;
            }

            let mut name_cursor = spec.walk();
            let name_nodes = spec
                .children_by_field_name("name", &mut name_cursor)
                .filter(|name| name.is_named())
                .collect::<Vec<_>>();
            let mut names = Vec::new();
            for name_node in &name_nodes {
                let name = node_text(*name_node, self.source)
                    .context("Go value declaration name was not on UTF-8 boundaries")?;
                if name != "_" {
                    names.push(name.to_owned());
                }
            }
            if names.is_empty() {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Go {kind:?} at byte {} because it declares no named binding",
                    spec.start_byte()
                )));
                continue;
            }
            let visibility = go_visibility(&names[0]);
            if names
                .iter()
                .skip(1)
                .any(|name| go_visibility(name) != visibility)
            {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted indivisible Go {kind:?} group {} because its names have different visibility and cannot have one exact review owner",
                    names.join(", ")
                )));
                continue;
            }
            let name = names.join(", ");
            let item = if ungrouped_single { declaration } else { spec };
            let mut semantic_ranges = if ungrouped_single {
                documentation_ranges(declaration_documentation.iter().cloned())
            } else {
                documentation_ranges(entry.documentation)
            };
            semantic_ranges.push(SemanticRange {
                range: item.byte_range(),
                role: SourceComponentRole::Value,
            });

            let mut type_use_sites = Vec::new();
            if let Some(type_node) = spec.child_by_field_name("type") {
                collect_type_identifiers(
                    type_node,
                    self.source,
                    TypeUseRole::Other,
                    &mut type_use_sites,
                )?;
            }
            normalize_type_use_sites(&mut type_use_sites);
            self.finish_declaration(
                item,
                name,
                kind,
                semantic_ranges,
                item.end_byte(),
                None,
                None,
                type_use_sites,
                item.byte_range(),
                &[],
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_declaration(
        &mut self,
        syntax_node: Node<'_>,
        name: String,
        kind: DeclarationKind,
        mut semantic_ranges: Vec<SemanticRange>,
        source_span_end: usize,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
        type_use_sites: Vec<TypeUseSite>,
        key_surface: Range<usize>,
        ownership_exclusions: &[Range<usize>],
    ) -> Result<Option<usize>> {
        semantic_ranges.sort_by_key(|semantic| (semantic.range.start, semantic.range.end));
        semantic_ranges
            .dedup_by(|left, right| left.range == right.range && left.role == right.role);
        let Some(projected_surface) = semantic_ranges
            .iter()
            .filter(|semantic| semantic.role != SourceComponentRole::Documentation)
            .map(|semantic| semantic.range.clone())
            .reduce(|left, right| left.start.min(right.start)..left.end.max(right.end))
        else {
            return Ok(None);
        };
        if has_syntax_error_outside_exclusions(
            syntax_node,
            &projected_surface,
            ownership_exclusions,
        ) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} {name} because its projected surface contains a syntax error"
            )));
            return Ok(None);
        }

        let components = build_components(
            self.source,
            &semantic_ranges,
            &self.comments,
            ownership_exclusions,
        )?;
        if components.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} {name} because it has no projectable source components"
            )));
            return Ok(None);
        }
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let projection_hash = projection_hash(Language::Go, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let source_start = semantic_ranges
            .first()
            .map_or(syntax_node.start_byte(), |semantic| semantic.range.start);
        let source_span = source_start..source_span_end;
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_span.start,
            &projection_hash,
        );
        let key_discriminator =
            syntax_discriminator(syntax_node, self.source, &key_surface, ownership_exclusions)?;
        let key = declaration_key(Language::Go, kind, &name, parent_name, &key_discriminator);
        let visibility = go_visibility(&name);
        let index = self.declarations.len();
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name,
            kind,
            visibility,
            parent_part: parent_id,
            source_ordinal,
            source_span,
            components,
            projection_text,
            projection_hash,
            review_owner: id,
            children: Vec::new(),
            type_use_sites,
        });
        Ok(Some(index))
    }

    fn resolve_method_lineage(&mut self) {
        for (method_index, parent_name) in std::mem::take(&mut self.pending_method_parents) {
            let candidates = self
                .declarations
                .iter()
                .enumerate()
                .filter(|(index, declaration)| {
                    *index != method_index
                        && declaration.name == parent_name
                        && matches!(
                            declaration.kind,
                            DeclarationKind::Struct
                                | DeclarationKind::Interface
                                | DeclarationKind::TypeAlias
                        )
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let [parent_index] = candidates.as_slice() else {
                let method = &self.declarations[method_index];
                let reason = if candidates.is_empty() {
                    "no matching type declaration occurs in this file"
                } else {
                    "more than one matching type declaration occurs in this file"
                };
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "Go Method {} receiver {parent_name} has no exact aggregate lineage because {reason}",
                    method.name
                )));
                continue;
            };
            let parent_index = *parent_index;
            let parent_id = self.declarations[parent_index].id.clone();
            let method_id = self.declarations[method_index].id.clone();
            self.declarations[method_index].parent_part = Some(parent_id);
            self.declarations[parent_index].children.push(method_id);
        }

        let source_ordinals = self
            .declarations
            .iter()
            .map(|declaration| (declaration.id.clone(), declaration.source_ordinal))
            .collect::<HashMap<_, _>>();
        for declaration in &mut self.declarations {
            declaration
                .children
                .sort_by_key(|child| source_ordinals.get(child).copied().unwrap_or(usize::MAX));
            declaration.children.dedup();
        }
    }
}

fn documentation_ranges(ranges: impl IntoIterator<Item = Range<usize>>) -> Vec<SemanticRange> {
    ranges
        .into_iter()
        .map(|range| SemanticRange {
            range,
            role: SourceComponentRole::Documentation,
        })
        .collect()
}

fn collect_specs<'tree>(
    declaration: Node<'tree>,
    spec_kinds: &[&str],
    source: &str,
) -> Vec<SpecEntry<'tree>> {
    let mut entries = Vec::new();
    collect_specs_from_container(declaration, spec_kinds, source, &mut entries);
    entries
}

fn collect_specs_from_container<'tree>(
    container: Node<'tree>,
    spec_kinds: &[&str],
    source: &str,
    entries: &mut Vec<SpecEntry<'tree>>,
) {
    let mut pending_comments = Vec::<Range<usize>>::new();
    let mut previous_item_end_row = Some(container.start_position().row);
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        if child.kind() == "comment" {
            if previous_item_end_row == Some(child.start_position().row) {
                pending_comments.clear();
            } else {
                pending_comments.push(child.byte_range());
            }
            continue;
        }
        if spec_kinds.iter().any(|kind| child.kind() == *kind) {
            entries.push(SpecEntry {
                node: child,
                documentation: trailing_comment_group(
                    source,
                    &pending_comments,
                    child.start_byte(),
                ),
            });
            pending_comments.clear();
            previous_item_end_row = Some(child.end_position().row);
        } else if child.kind() == "var_spec_list" {
            pending_comments.clear();
            collect_specs_from_container(child, spec_kinds, source, entries);
            previous_item_end_row = Some(child.end_position().row);
        } else {
            pending_comments.clear();
            previous_item_end_row = Some(child.end_position().row);
        }
    }
}

fn collect_aggregate_documentation(type_node: Node<'_>, source: &str) -> Vec<Range<usize>> {
    let container = if type_node.kind() == "struct_type" {
        let mut cursor = type_node.walk();
        type_node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "field_declaration_list")
    } else {
        Some(type_node)
    };
    let Some(container) = container else {
        return Vec::new();
    };
    let member_kinds: &[&str] = if type_node.kind() == "struct_type" {
        &["field_declaration"]
    } else {
        &["type_elem"]
    };
    let mut documentation = Vec::new();
    let mut pending_comments = Vec::<Range<usize>>::new();
    let mut previous_item_end_row = Some(container.start_position().row);
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        if child.kind() == "comment" {
            if previous_item_end_row == Some(child.start_position().row) {
                pending_comments.clear();
            } else {
                pending_comments.push(child.byte_range());
            }
            continue;
        }
        if member_kinds.iter().any(|kind| child.kind() == *kind) {
            documentation.extend(trailing_comment_group(
                source,
                &pending_comments,
                child.start_byte(),
            ));
        }
        pending_comments.clear();
        previous_item_end_row = Some(child.end_position().row);
    }
    documentation
}

fn collect_interface_methods<'tree>(
    interface_type: Node<'tree>,
    source: &str,
) -> Vec<SpecEntry<'tree>> {
    let mut entries = Vec::new();
    let mut pending_comments = Vec::<Range<usize>>::new();
    let mut previous_item_end_row = Some(interface_type.start_position().row);
    let mut cursor = interface_type.walk();
    for child in interface_type.named_children(&mut cursor) {
        if child.kind() == "comment" {
            if previous_item_end_row == Some(child.start_position().row) {
                pending_comments.clear();
            } else {
                pending_comments.push(child.byte_range());
            }
            continue;
        }
        if child.kind() == "method_elem" {
            entries.push(SpecEntry {
                node: child,
                documentation: trailing_comment_group(
                    source,
                    &pending_comments,
                    child.start_byte(),
                ),
            });
        }
        pending_comments.clear();
        previous_item_end_row = Some(child.end_position().row);
    }
    entries
}

fn trailing_comment_group(
    source: &str,
    comments: &[Range<usize>],
    declaration_start: usize,
) -> Vec<Range<usize>> {
    let mut next_start = declaration_start;
    let mut group = Vec::new();
    for comment in comments.iter().rev() {
        if !is_immediately_before(source, comment.end, next_start) {
            break;
        }
        group.push(comment.clone());
        next_start = comment.start;
    }
    group.reverse();
    group
}

fn is_immediately_before(source: &str, previous_end: usize, next_start: usize) -> bool {
    let Some(gap) = source.get(previous_end..next_start) else {
        return false;
    };
    gap.chars().all(char::is_whitespace) && gap.bytes().filter(|byte| *byte == b'\n').count() <= 1
}

fn is_grouped_declaration(declaration: Node<'_>) -> bool {
    let mut cursor = declaration.walk();
    declaration
        .children(&mut cursor)
        .any(|child| child.kind() == "(")
}

fn trim_range_end(source: &str, mut range: Range<usize>) -> Option<Range<usize>> {
    while range.end > range.start
        && source
            .as_bytes()
            .get(range.end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        range.end -= 1;
    }
    (range.end > range.start).then_some(range)
}

fn build_components(
    source: &str,
    semantic_ranges: &[SemanticRange],
    comments: &[CommentRange],
    ownership_exclusions: &[Range<usize>],
) -> Result<Vec<SourceComponent>> {
    let Some(surface) = semantic_ranges
        .iter()
        .map(|semantic| semantic.range.clone())
        .reduce(|left, right| left.start.min(right.start)..left.end.max(right.end))
    else {
        return Ok(Vec::new());
    };
    let documentation = semantic_ranges
        .iter()
        .filter(|semantic| semantic.role == SourceComponentRole::Documentation)
        .map(|semantic| semantic.range.clone())
        .collect::<Vec<_>>();
    let mut excluded = comments
        .iter()
        .filter(|comment| {
            !documentation.contains(&comment.raw)
                && comment.exclusion.start < surface.end
                && comment.exclusion.end > surface.start
        })
        .map(|comment| {
            comment.exclusion.start.max(surface.start)..comment.exclusion.end.min(surface.end)
        })
        .collect::<Vec<_>>();
    excluded.extend(
        ownership_exclusions
            .iter()
            .filter(|range| range.start < surface.end && range.end > surface.start)
            .map(|range| range.start.max(surface.start)..range.end.min(surface.end)),
    );
    excluded.sort_by_key(|range| (range.start, range.end));
    excluded.dedup();

    let mut boundaries = vec![surface.start, surface.end];
    for semantic in semantic_ranges {
        boundaries.push(semantic.range.start);
        boundaries.push(semantic.range.end);
    }
    for exclusion in &excluded {
        boundaries.push(exclusion.start);
        boundaries.push(exclusion.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut components = Vec::new();
    for boundary in boundaries.windows(2) {
        let range = boundary[0]..boundary[1];
        if range.is_empty()
            || excluded
                .iter()
                .any(|exclusion| exclusion.start < range.end && exclusion.end > range.start)
        {
            continue;
        }
        let role = semantic_ranges
            .iter()
            .filter(|semantic| {
                semantic.range.start <= range.start && semantic.range.end >= range.end
            })
            .min_by_key(|semantic| {
                (
                    semantic.range.end - semantic.range.start,
                    if semantic.role == SourceComponentRole::Documentation {
                        0
                    } else {
                        1
                    },
                )
            })
            .map_or(SourceComponentRole::Layout, |semantic| semantic.role);
        let text = source
            .get(range.clone())
            .context("Go declaration component was not on UTF-8 boundaries")?
            .to_owned();
        if !text.is_empty() {
            components.push(SourceComponent {
                role,
                source_range: range,
                text,
            });
        }
    }
    Ok(components)
}

fn collect_comments(node: Node<'_>, source: &str, comments: &mut Vec<CommentRange>) {
    if node.kind() == "comment" {
        let raw = node.byte_range();
        comments.push(CommentRange {
            exclusion: whole_comment_line(source, raw.clone()),
            raw,
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_comments(child, source, comments);
    }
}

fn whole_comment_line(source: &str, range: Range<usize>) -> Range<usize> {
    let line_start = source.as_bytes()[..range.start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let line_content_end = source.as_bytes()[range.end..]
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))
        .map_or(source.len(), |offset| range.end + offset);
    if !source[line_start..range.start].trim().is_empty()
        || !source[range.end..line_content_end].trim().is_empty()
    {
        return range;
    }

    let mut line_end = line_content_end;
    if source.as_bytes().get(line_end) == Some(&b'\r') {
        line_end += 1;
    }
    if source.as_bytes().get(line_end) == Some(&b'\n') {
        line_end += 1;
    }
    line_start..line_end
}

fn member_line_exclusion(source: &str, range: Range<usize>) -> Range<usize> {
    let line_start = source.as_bytes()[..range.start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let start = if source[line_start..range.start].trim().is_empty() {
        line_start
    } else {
        range.start
    };
    let line_content_end = source.as_bytes()[range.end..]
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))
        .map_or(source.len(), |offset| range.end + offset);
    if !source[range.end..line_content_end].trim().is_empty() {
        return start..range.end;
    }
    let mut end = line_content_end;
    if source.as_bytes().get(end) == Some(&b'\r') {
        end += 1;
    }
    if source.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }
    start..end
}

fn syntax_discriminator(
    node: Node<'_>,
    source: &str,
    surface: &Range<usize>,
    exclusions: &[Range<usize>],
) -> Result<String> {
    fn collect(
        node: Node<'_>,
        source: &str,
        surface: &Range<usize>,
        exclusions: &[Range<usize>],
        output: &mut String,
    ) -> Result<()> {
        let range = node.byte_range();
        if node.kind() == "comment"
            || range.end <= surface.start
            || range.start >= surface.end
            || exclusions
                .iter()
                .any(|excluded| excluded.start <= range.start && excluded.end >= range.end)
        {
            return Ok(());
        }
        if node.child_count() == 0 {
            if range.start < surface.start || range.end > surface.end || range.is_empty() {
                return Ok(());
            }
            let text =
                node_text(node, source).context("Go identity token was not on UTF-8 boundaries")?;
            write!(
                output,
                "{}:{}{}:{}",
                node.kind().len(),
                node.kind(),
                text.len(),
                text
            )
            .expect("writing to a String cannot fail");
            return Ok(());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect(child, source, surface, exclusions, output)?;
        }
        Ok(())
    }

    let mut output = String::new();
    collect(node, source, surface, exclusions, &mut output)?;
    Ok(output)
}

fn has_syntax_error_outside_exclusions(
    node: Node<'_>,
    range: &Range<usize>,
    exclusions: &[Range<usize>],
) -> bool {
    let node_range = node.byte_range();
    if (node.is_error() || node.is_missing())
        && node_range.start < range.end
        && node_range.end >= range.start
        && !exclusions
            .iter()
            .any(|exclusion| exclusion.start <= node_range.start && exclusion.end >= node_range.end)
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| has_syntax_error_outside_exclusions(child, range, exclusions))
}

fn go_visibility(name: &str) -> Visibility {
    if name.chars().next().is_some_and(char::is_uppercase) {
        Visibility::Public
    } else {
        Visibility::Package
    }
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}

fn receiver_base_type_identifier(receiver: Node<'_>) -> Option<Node<'_>> {
    fn unwrap_type(node: Node<'_>) -> Option<Node<'_>> {
        match node.kind() {
            "type_identifier" => Some(node),
            "generic_type" => node.child_by_field_name("type").and_then(unwrap_type),
            "pointer_type" | "parenthesized_type" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor).find_map(unwrap_type)
            }
            _ => None,
        }
    }

    let mut cursor = receiver.walk();
    receiver
        .named_children(&mut cursor)
        .find(|child| child.kind() == "parameter_declaration")
        .and_then(|parameter| parameter.child_by_field_name("type"))
        .and_then(unwrap_type)
}

fn collect_type_identifiers(
    node: Node<'_>,
    source: &str,
    role: TypeUseRole,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    if node.kind() == "type_identifier" {
        let range = node.byte_range();
        sites.push(TypeUseSite {
            name: source
                .get(range.clone())
                .context("Go type-use site was not on UTF-8 boundaries")?
                .to_owned(),
            role,
            source_range: range,
        });
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_identifiers(child, source, role, sites)?;
    }
    Ok(())
}

fn collect_struct_type_uses(
    struct_type: Node<'_>,
    source: &str,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    let mut stack = vec![struct_type];
    while let Some(node) = stack.pop() {
        if node.kind() == "field_declaration" {
            if let Some(field_type) = node.child_by_field_name("type") {
                collect_type_identifiers(field_type, source, TypeUseRole::Field, sites)?;
            }
            continue;
        }
        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    Ok(())
}

fn collect_interface_type_uses(
    interface_type: Node<'_>,
    source: &str,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    let mut cursor = interface_type.walk();
    for member in interface_type.named_children(&mut cursor) {
        if member.kind() == "type_elem" {
            collect_type_identifiers(member, source, TypeUseRole::Bound, sites)?;
        }
    }
    Ok(())
}

fn normalize_type_use_sites(sites: &mut Vec<TypeUseSite>) {
    sites.sort_by_key(|site| (site.source_range.start, site.source_range.end));
    sites.dedup_by(|left, right| left.source_range == right.source_range);
}
