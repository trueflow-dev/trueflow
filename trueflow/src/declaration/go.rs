use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::analysis::Language;

use super::projection::{declaration_id, declaration_key};
use super::{
    DeclarationKind, DeclarationNode, ProjectionDiagnostic, SourceComponent, SourceComponentRole,
    TypeUseRole, TypeUseSite, Visibility, projection_hash,
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
    };
    projector.collect_file(tree.root_node())?;
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
        let Some(body) = item.child_by_field_name("body") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} at byte {} because its signature has no body boundary",
                item.start_byte()
            )));
            return Ok(());
        };
        let Some(surface) = trim_range_end(self.source, item.start_byte()..body.start_byte())
        else {
            return Ok(());
        };

        let mut semantic_ranges = documentation_ranges(documentation);
        semantic_ranges.push(SemanticRange {
            range: surface,
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
            .and_then(first_type_identifier)
            .and_then(|receiver| self.source.get(receiver.byte_range()))
            .map(str::to_owned);
        self.finish_declaration(
            item,
            name_node,
            kind,
            semantic_ranges,
            item.end_byte(),
            parent_name.as_deref(),
            type_use_sites,
        )
    }

    fn add_type_declaration(
        &mut self,
        declaration: Node<'_>,
        declaration_documentation: &[Range<usize>],
    ) -> Result<()> {
        let grouped = is_grouped_declaration(declaration);
        let specs = collect_specs(declaration, &["type_spec", "type_alias"], self.source);
        let ungrouped_single = !grouped && specs.len() == 1;

        for entry in specs {
            let spec = entry.node;
            let Some(name_node) = spec.child_by_field_name("name") else {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Go type declaration at byte {} because it has no declared name",
                    spec.start_byte()
                )));
                continue;
            };
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
            self.finish_declaration(
                item,
                name_node,
                kind,
                semantic_ranges,
                item.end_byte(),
                None,
                type_use_sites,
            )?;
        }
        Ok(())
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
            let mut name_cursor = spec.walk();
            let names = spec
                .children_by_field_name("name", &mut name_cursor)
                .filter(|name| name.is_named())
                .collect::<Vec<_>>();
            if names.is_empty() {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted Go {kind:?} at byte {} because it has no declared name",
                    spec.start_byte()
                )));
                continue;
            }
            for name_node in names {
                self.finish_declaration(
                    item,
                    name_node,
                    kind,
                    semantic_ranges.clone(),
                    item.end_byte(),
                    None,
                    type_use_sites.clone(),
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_declaration(
        &mut self,
        syntax_node: Node<'_>,
        name_node: Node<'_>,
        kind: DeclarationKind,
        mut semantic_ranges: Vec<SemanticRange>,
        source_span_end: usize,
        parent_name: Option<&str>,
        type_use_sites: Vec<TypeUseSite>,
    ) -> Result<()> {
        let name = self
            .source
            .get(name_node.byte_range())
            .context("Go declaration name was not on UTF-8 boundaries")?
            .to_owned();
        semantic_ranges.sort_by_key(|semantic| (semantic.range.start, semantic.range.end));
        semantic_ranges
            .dedup_by(|left, right| left.range == right.range && left.role == right.role);
        let Some(projected_surface) = semantic_ranges
            .iter()
            .filter(|semantic| semantic.role != SourceComponentRole::Documentation)
            .map(|semantic| semantic.range.clone())
            .reduce(|left, right| left.start.min(right.start)..left.end.max(right.end))
        else {
            return Ok(());
        };
        if has_syntax_error_in_range(syntax_node, &projected_surface) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} {name} because its projected surface contains a syntax error"
            )));
            return Ok(());
        }

        let components = build_components(self.source, &semantic_ranges, &self.comments)?;
        if components.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Go {kind:?} {name} because it has no projectable source components"
            )));
            return Ok(());
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
        let key = declaration_key(Language::Go, kind, &name, parent_name, &projection_text);
        let visibility = go_visibility(&name);
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name,
            kind,
            visibility,
            parent_part: None,
            source_ordinal,
            source_span,
            components,
            projection_text,
            projection_hash,
            review_owner: id,
            children: Vec::new(),
            type_use_sites,
        });
        Ok(())
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
        &["method_elem", "type_elem"]
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
    let excluded = comments
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

fn has_syntax_error_in_range(node: Node<'_>, range: &Range<usize>) -> bool {
    if (node.is_error() || node.is_missing())
        && node.start_byte() < range.end
        && node.end_byte() >= range.start
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| has_syntax_error_in_range(child, range))
}

fn go_visibility(name: &str) -> Visibility {
    if name.chars().next().is_some_and(char::is_uppercase) {
        Visibility::Public
    } else {
        Visibility::Package
    }
}

fn first_type_identifier(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "type_identifier" {
        return Some(node);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(first_type_identifier)
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
        match member.kind() {
            "method_elem" => {
                if let Some(parameters) = member.child_by_field_name("parameters") {
                    collect_type_identifiers(parameters, source, TypeUseRole::Parameter, sites)?;
                }
                if let Some(result) = member.child_by_field_name("result") {
                    collect_type_identifiers(result, source, TypeUseRole::Return, sites)?;
                }
            }
            "type_elem" => {
                collect_type_identifiers(member, source, TypeUseRole::Bound, sites)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn normalize_type_use_sites(sites: &mut Vec<TypeUseSite>) {
    sites.sort_by_key(|site| (site.source_range.start, site.source_range.end));
    sites.dedup_by(|left, right| left.source_range == right.source_range);
}
