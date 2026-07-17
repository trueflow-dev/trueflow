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
    range: Range<usize>,
    documentation: bool,
}

#[derive(Debug, Clone, Copy)]
enum ScopeKind {
    File,
    Impl,
    Trait,
    Module,
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
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("failed to load the Rust grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a Rust syntax tree")?;

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
    projector.collect_scope(tree.root_node(), ScopeKind::File, None, None)?;
    if tree.root_node().has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "Rust source contains syntax errors; declarations with errors in projected surfaces were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect_scope(
        &mut self,
        scope: Node<'_>,
        scope_kind: ScopeKind,
        parent_id: Option<&DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<()> {
        let mut pending = Vec::<SemanticRange>::new();
        let mut cursor = scope.walk();
        for child in scope.named_children(&mut cursor) {
            match child.kind() {
                "line_comment" | "block_comment" => {
                    if is_outer_documentation(child, self.source) {
                        pending.push(SemanticRange {
                            range: child.byte_range(),
                            role: SourceComponentRole::Documentation,
                        });
                    }
                }
                "attribute_item" => {
                    let role = if is_doc_attribute(child, self.source) {
                        SourceComponentRole::Documentation
                    } else {
                        SourceComponentRole::Attribute
                    };
                    pending.push(SemanticRange {
                        range: child.byte_range(),
                        role,
                    });
                }
                "inner_attribute_item" => {
                    pending.clear();
                }
                "function_item" | "function_signature_item" => {
                    let kind = if matches!(scope_kind, ScopeKind::File | ScopeKind::Module) {
                        DeclarationKind::Function
                    } else {
                        DeclarationKind::Method
                    };
                    self.add_declaration(
                        child,
                        kind,
                        std::mem::take(&mut pending),
                        parent_id,
                        parent_name,
                    )?;
                }
                "struct_item" => {
                    self.add_declaration(
                        child,
                        DeclarationKind::Struct,
                        std::mem::take(&mut pending),
                        parent_id,
                        parent_name,
                    )?;
                }
                "enum_item" => {
                    self.add_declaration(
                        child,
                        DeclarationKind::Enum,
                        std::mem::take(&mut pending),
                        parent_id,
                        parent_name,
                    )?;
                }
                "trait_item" => {
                    let attachments = std::mem::take(&mut pending);
                    let parent_index = self.declarations.len();
                    let added = self.add_declaration(
                        child,
                        DeclarationKind::Trait,
                        attachments,
                        parent_id,
                        parent_name,
                    )?;
                    if added {
                        let trait_id = self.declarations[parent_index].id.clone();
                        let trait_name = self.declarations[parent_index].name.clone();
                        let children_start = self.declarations.len();
                        if let Some(body) = child.child_by_field_name("body") {
                            self.collect_scope(
                                body,
                                ScopeKind::Trait,
                                Some(&trait_id),
                                Some(&trait_name),
                            )?;
                        }
                        let child_ids = self.declarations[children_start..]
                            .iter()
                            .map(|declaration| declaration.id.clone())
                            .collect();
                        self.declarations[parent_index].children = child_ids;
                    }
                }
                "impl_item" => {
                    pending.clear();
                    if let Some(body) = child.child_by_field_name("body") {
                        let impl_name = impl_target_name(child, self.source);
                        self.collect_scope(body, ScopeKind::Impl, None, impl_name.as_deref())?;
                    }
                }
                "mod_item" => {
                    let attachments = std::mem::take(&mut pending);
                    let parent_index = self.declarations.len();
                    let added = self.add_declaration(
                        child,
                        DeclarationKind::Module,
                        attachments,
                        parent_id,
                        parent_name,
                    )?;
                    if added {
                        let module_id = self.declarations[parent_index].id.clone();
                        let module_name = self.declarations[parent_index].name.clone();
                        let children_start = self.declarations.len();
                        if let Some(body) = child.child_by_field_name("body") {
                            self.collect_scope(
                                body,
                                ScopeKind::Module,
                                Some(&module_id),
                                Some(&module_name),
                            )?;
                        }
                        let child_ids = self.declarations[children_start..]
                            .iter()
                            .map(|declaration| declaration.id.clone())
                            .collect();
                        self.declarations[parent_index].children = child_ids;
                    }
                }
                "type_item" | "associated_type" => {
                    let kind = if matches!(scope_kind, ScopeKind::Trait | ScopeKind::Impl) {
                        DeclarationKind::AssociatedType
                    } else {
                        DeclarationKind::TypeAlias
                    };
                    self.add_declaration(
                        child,
                        kind,
                        std::mem::take(&mut pending),
                        parent_id,
                        parent_name,
                    )?;
                }
                "const_item" => {
                    self.add_declaration(
                        child,
                        DeclarationKind::Constant,
                        std::mem::take(&mut pending),
                        parent_id,
                        parent_name,
                    )?;
                }
                "static_item" => {
                    self.add_declaration(
                        child,
                        DeclarationKind::Static,
                        std::mem::take(&mut pending),
                        parent_id,
                        parent_name,
                    )?;
                }
                _ => pending.clear(),
            }
        }
        Ok(())
    }

    fn add_declaration(
        &mut self,
        item: Node<'_>,
        kind: DeclarationKind,
        mut attachments: Vec<SemanticRange>,
        parent_id: Option<&DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<bool> {
        let Some(name_node) = item.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} at byte {} because it has no declared name",
                item.start_byte()
            )));
            return Ok(false);
        };
        let name = self
            .source
            .get(name_node.byte_range())
            .context("Rust declaration name was not on UTF-8 boundaries")?
            .to_owned();

        let Some(item_projection_range) = projected_item_range(item, kind, self.source) else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} {name} because its projected surface is incomplete"
            )));
            return Ok(false);
        };
        if has_syntax_error_in_range(item, &item_projection_range) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} {name} because its projected surface contains a syntax error"
            )));
            return Ok(false);
        }

        let item_role = match kind {
            DeclarationKind::Function
            | DeclarationKind::Method
            | DeclarationKind::Trait
            | DeclarationKind::Module => SourceComponentRole::Signature,
            DeclarationKind::Struct | DeclarationKind::Enum => SourceComponentRole::AggregateShape,
            DeclarationKind::TypeAlias | DeclarationKind::AssociatedType => {
                SourceComponentRole::TypeAlias
            }
            DeclarationKind::Constant | DeclarationKind::Static => SourceComponentRole::Value,
            _ => SourceComponentRole::Signature,
        };
        attachments.push(SemanticRange {
            range: item_projection_range.clone(),
            role: item_role,
        });
        attachments.sort_by_key(|component| component.range.start);

        let components = build_components(self.source, &attachments, &self.comments)?;
        if components.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} {name} because it has no projectable source components"
            )));
            return Ok(false);
        }
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let hash = projection_hash(Language::Rust, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let source_start = attachments
            .first()
            .map_or(item.start_byte(), |attachment| attachment.range.start);
        let source_span = source_start..item.end_byte();
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_span.start,
            &hash,
        );
        let item_projection = self
            .source
            .get(item_projection_range)
            .context("Rust projection was not on UTF-8 boundaries")?;
        let key = declaration_key(Language::Rust, kind, &name, parent_name, item_projection);
        let implicit_visibility = parent_id.is_some()
            && matches!(
                kind,
                DeclarationKind::Method
                    | DeclarationKind::AssociatedType
                    | DeclarationKind::Constant
            );
        let visibility = rust_visibility(item, self.source, implicit_visibility)?;
        let type_use_sites =
            collect_type_use_sites(item, self.source, &components, name_node.byte_range(), kind)?;

        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name,
            kind,
            visibility,
            parent_part: parent_id.cloned(),
            source_ordinal,
            source_span,
            components,
            projection_text,
            projection_hash: hash,
            review_owner: id,
            children: Vec::new(),
            type_use_sites,
        });
        Ok(true)
    }
}

fn projected_item_range(
    item: Node<'_>,
    kind: DeclarationKind,
    source: &str,
) -> Option<Range<usize>> {
    let mut end = match kind {
        DeclarationKind::Function | DeclarationKind::Method => item
            .child_by_field_name("body")
            .map_or(item.end_byte(), |body| body.start_byte()),
        DeclarationKind::Trait | DeclarationKind::Module => item
            .child_by_field_name("body")
            .map_or(item.end_byte(), |body| body.start_byte()),
        _ => item.end_byte(),
    };
    while end > item.start_byte()
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    (end > item.start_byte()).then_some(item.start_byte()..end)
}

fn build_components(
    source: &str,
    semantic_ranges: &[SemanticRange],
    comments: &[CommentRange],
) -> Result<Vec<SourceComponent>> {
    let (Some(first), Some(last)) = (semantic_ranges.first(), semantic_ranges.last()) else {
        return Ok(Vec::new());
    };
    let surface = first.range.start..last.range.end;
    let excluded = comments
        .iter()
        .filter(|comment| {
            !comment.documentation
                && comment.range.start < surface.end
                && comment.range.end > surface.start
        })
        .map(|comment| comment.range.clone())
        .collect::<Vec<_>>();

    let mut boundaries = vec![surface.start, surface.end];
    for semantic in semantic_ranges {
        boundaries.push(semantic.range.start);
        boundaries.push(semantic.range.end);
    }
    for comment in &excluded {
        boundaries.push(comment.start.max(surface.start));
        boundaries.push(comment.end.min(surface.end));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut components = Vec::new();
    for boundary in boundaries.windows(2) {
        let range = boundary[0]..boundary[1];
        if range.is_empty()
            || excluded
                .iter()
                .any(|comment| comment.start < range.end && comment.end > range.start)
        {
            continue;
        }
        let role = semantic_ranges
            .iter()
            .find(|semantic| semantic.range.start <= range.start && semantic.range.end >= range.end)
            .map_or(SourceComponentRole::Layout, |semantic| semantic.role);
        let text = source
            .get(range.clone())
            .context("Rust declaration component was not on UTF-8 boundaries")?
            .to_owned();
        if text.is_empty() {
            continue;
        }
        components.push(SourceComponent {
            role,
            source_range: range,
            text,
        });
    }
    Ok(components)
}

fn collect_comments(node: Node<'_>, source: &str, comments: &mut Vec<CommentRange>) {
    if matches!(node.kind(), "line_comment" | "block_comment") {
        let range = node.byte_range();
        let documentation = is_documentation_text(node_text(node, source).unwrap_or_default());
        comments.push(CommentRange {
            range: if documentation {
                range
            } else {
                whole_comment_line(source, range)
            },
            documentation,
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

fn is_outer_documentation(node: Node<'_>, source: &str) -> bool {
    let text = node_text(node, source).unwrap_or_default().trim_start();
    (text.starts_with("///") && !text.starts_with("////"))
        || (text.starts_with("/**") && !text.starts_with("/***"))
}

fn is_documentation_text(text: &str) -> bool {
    let text = text.trim_start();
    (text.starts_with("///") && !text.starts_with("////"))
        || text.starts_with("//!")
        || (text.starts_with("/**") && !text.starts_with("/***"))
        || text.starts_with("/*!")
}

fn is_doc_attribute(node: Node<'_>, source: &str) -> bool {
    let Some(text) = node_text(node, source)
        .map(str::trim_start)
        .and_then(|text| text.strip_prefix("#["))
    else {
        return false;
    };
    let Some(after_doc) = text.trim_start().strip_prefix("doc") else {
        return false;
    };
    after_doc
        .chars()
        .next()
        .is_some_and(|character| character.is_whitespace() || matches!(character, '=' | '(' | ']'))
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}

fn rust_visibility(item: Node<'_>, source: &str, implicit_visibility: bool) -> Result<Visibility> {
    let mut cursor = item.walk();
    let visibility = item
        .named_children(&mut cursor)
        .find(|child| child.kind() == "visibility_modifier");
    let Some(visibility) = visibility else {
        return Ok(if implicit_visibility {
            Visibility::Implicit
        } else {
            Visibility::Private
        });
    };
    let text = source
        .get(visibility.byte_range())
        .context("Rust visibility was not on UTF-8 boundaries")?;
    Ok(match text.trim() {
        "pub" => Visibility::Public,
        "pub(crate)" => Visibility::Crate,
        other => Visibility::Restricted(other.to_owned()),
    })
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

fn impl_target_name(item: Node<'_>, source: &str) -> Option<String> {
    item.child_by_field_name("type")
        .and_then(|node| source.get(node.byte_range()))
        .map(str::to_owned)
}

fn collect_type_use_sites(
    item: Node<'_>,
    source: &str,
    components: &[SourceComponent],
    declaration_name: Range<usize>,
    kind: DeclarationKind,
) -> Result<Vec<TypeUseSite>> {
    let mut sites = Vec::new();
    collect_type_identifiers(
        item,
        source,
        components,
        &declaration_name,
        kind,
        &mut sites,
    )?;
    Ok(sites)
}

fn collect_type_identifiers(
    node: Node<'_>,
    source: &str,
    components: &[SourceComponent],
    declaration_name: &Range<usize>,
    declaration_kind: DeclarationKind,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    let range = node.byte_range();
    if node.kind() == "type_identifier"
        && range != *declaration_name
        && components.iter().any(|component| {
            component.source_range.start <= range.start && component.source_range.end >= range.end
        })
    {
        let role = if matches!(
            declaration_kind,
            DeclarationKind::TypeAlias | DeclarationKind::AssociatedType
        ) {
            TypeUseRole::AliasTarget
        } else {
            TypeUseRole::Other
        };
        sites.push(TypeUseSite {
            name: source
                .get(range.clone())
                .context("Rust type-use site was not on UTF-8 boundaries")?
                .to_owned(),
            role,
            source_range: range,
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_identifiers(
            child,
            source,
            components,
            declaration_name,
            declaration_kind,
            sites,
        )?;
    }
    Ok(())
}
