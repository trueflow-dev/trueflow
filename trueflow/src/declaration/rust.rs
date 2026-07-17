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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    File,
    Impl,
    TraitImpl,
    Trait,
    Module,
    Foreign,
}

#[derive(Debug)]
struct ImplLink {
    child_index: usize,
    target_name: String,
    target_scope: String,
}

struct Projector<'a> {
    path: &'a Path,
    source: &'a str,
    comments: Vec<CommentRange>,
    next_ordinal: usize,
    declarations: Vec<DeclarationNode>,
    declaration_qualifiers: Vec<String>,
    impl_links: Vec<ImplLink>,
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
        declaration_qualifiers: Vec::new(),
        impl_links: Vec::new(),
        diagnostics: Vec::new(),
    };
    projector.collect_scope(tree.root_node(), ScopeKind::File, None, "")?;
    projector.resolve_impl_lineage();
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
        scope_qualifier: &str,
    ) -> Result<Vec<usize>> {
        let mut direct_children = Vec::new();
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
                "inner_attribute_item" => pending.clear(),
                "function_item" | "function_signature_item" => {
                    let kind = if matches!(
                        scope_kind,
                        ScopeKind::File | ScopeKind::Module | ScopeKind::Foreign
                    ) {
                        DeclarationKind::Function
                    } else {
                        DeclarationKind::Method
                    };
                    if let Some(index) = self.add_declaration(
                        child,
                        kind,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                "struct_item" | "union_item" => {
                    if let Some(index) = self.add_declaration(
                        child,
                        DeclarationKind::Struct,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                "enum_item" => {
                    if let Some(index) = self.add_declaration(
                        child,
                        DeclarationKind::Enum,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                "trait_item" => {
                    let Some(parent_index) = self.add_declaration(
                        child,
                        DeclarationKind::Trait,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )?
                    else {
                        continue;
                    };
                    direct_children.push(parent_index);
                    let trait_id = self.declarations[parent_index].id.clone();
                    let trait_qualifier = self.declaration_qualifiers[parent_index].clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let children = self.collect_scope(
                            body,
                            ScopeKind::Trait,
                            Some(&trait_id),
                            &trait_qualifier,
                        )?;
                        self.declarations[parent_index].children = children
                            .into_iter()
                            .map(|index| self.declarations[index].id.clone())
                            .collect();
                    }
                }
                "impl_item" => {
                    pending.clear();
                    let Some(body) = child.child_by_field_name("body") else {
                        self.diagnostics.push(ProjectionDiagnostic::new(format!(
                            "Rust impl block at byte {} has no body; no associated declarations were projected",
                            child.start_byte()
                        )));
                        continue;
                    };
                    let impl_qualifier = qualify(
                        scope_qualifier,
                        &key_tokens(child, self.source, impl_header_range(child))?,
                    );
                    let impl_scope = if child.child_by_field_name("trait").is_some() {
                        ScopeKind::TraitImpl
                    } else {
                        ScopeKind::Impl
                    };
                    let children = self.collect_scope(body, impl_scope, None, &impl_qualifier)?;
                    if let Some(target_name) = impl_target_name(child, self.source) {
                        self.impl_links
                            .extend(children.into_iter().map(|child_index| ImplLink {
                                child_index,
                                target_name: target_name.clone(),
                                target_scope: scope_qualifier.to_owned(),
                            }));
                    }
                }
                "mod_item" => {
                    let Some(parent_index) = self.add_declaration(
                        child,
                        DeclarationKind::Module,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )?
                    else {
                        continue;
                    };
                    direct_children.push(parent_index);
                    let module_id = self.declarations[parent_index].id.clone();
                    let module_qualifier = self.declaration_qualifiers[parent_index].clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let children = self.collect_scope(
                            body,
                            ScopeKind::Module,
                            Some(&module_id),
                            &module_qualifier,
                        )?;
                        self.declarations[parent_index].children = children
                            .into_iter()
                            .map(|index| self.declarations[index].id.clone())
                            .collect();
                    }
                }
                "foreign_mod_item" => {
                    pending.clear();
                    if let Some(body) = child.child_by_field_name("body") {
                        let foreign_qualifier = qualify(
                            scope_qualifier,
                            &key_tokens(child, self.source, impl_header_range(child))?,
                        );
                        direct_children.extend(self.collect_scope(
                            body,
                            ScopeKind::Foreign,
                            parent_id,
                            &foreign_qualifier,
                        )?);
                    }
                }
                "type_item" | "associated_type" => {
                    let kind = if matches!(
                        scope_kind,
                        ScopeKind::Trait | ScopeKind::Impl | ScopeKind::TraitImpl
                    ) {
                        DeclarationKind::AssociatedType
                    } else {
                        DeclarationKind::TypeAlias
                    };
                    if let Some(index) = self.add_declaration(
                        child,
                        kind,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                "const_item" => {
                    if let Some(index) = self.add_declaration(
                        child,
                        DeclarationKind::Constant,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                "static_item" => {
                    if let Some(index) = self.add_declaration(
                        child,
                        DeclarationKind::Static,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                "macro_definition" => {
                    if let Some(index) = self.add_declaration(
                        child,
                        DeclarationKind::Macro,
                        std::mem::take(&mut pending),
                        parent_id,
                        scope_qualifier,
                        scope_kind,
                    )? {
                        direct_children.push(index);
                    }
                }
                _ => pending.clear(),
            }
        }
        Ok(direct_children)
    }

    fn add_declaration(
        &mut self,
        item: Node<'_>,
        kind: DeclarationKind,
        mut attachments: Vec<SemanticRange>,
        parent_id: Option<&DeclarationId>,
        parent_qualifier: &str,
        scope_kind: ScopeKind,
    ) -> Result<Option<usize>> {
        let Some(name_node) = item.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} at byte {} because it has no declared name",
                item.start_byte()
            )));
            return Ok(None);
        };
        let name = self
            .source
            .get(name_node.byte_range())
            .context("Rust declaration name was not on UTF-8 boundaries")?
            .to_owned();

        attachments.sort_by_key(|component| component.range.start);
        let item_ranges = projected_item_ranges(item, kind, self.source);
        if item_ranges.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} {name} because its projected surface is incomplete"
            )));
            return Ok(None);
        }
        if has_syntax_error_in_ranges(item, &item_ranges) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} {name} because its projected surface contains a syntax error"
            )));
            return Ok(None);
        }

        let item_role = match kind {
            DeclarationKind::Function | DeclarationKind::Method | DeclarationKind::Module => {
                SourceComponentRole::Signature
            }
            DeclarationKind::Struct | DeclarationKind::Enum | DeclarationKind::Trait => {
                SourceComponentRole::AggregateShape
            }
            DeclarationKind::TypeAlias | DeclarationKind::AssociatedType => {
                SourceComponentRole::TypeAlias
            }
            DeclarationKind::Constant | DeclarationKind::Static => SourceComponentRole::Value,
            DeclarationKind::Macro => SourceComponentRole::Signature,
            _ => SourceComponentRole::Signature,
        };
        let source_start = attachments
            .first()
            .map_or(item.start_byte(), |attachment| attachment.range.start);
        let mut includes = item_ranges.clone();
        if source_start < item_ranges[0].start {
            includes.push(source_start..item_ranges[0].end);
        }
        normalize_ranges(&mut includes);
        self.comments
            .iter()
            .filter(|comment| {
                comment.documentation
                    && includes.iter().any(|included| {
                        included.start <= comment.range.start && included.end >= comment.range.end
                    })
            })
            .for_each(|comment| {
                attachments.push(SemanticRange {
                    range: comment.range.clone(),
                    role: SourceComponentRole::Documentation,
                });
            });
        collect_attribute_semantics(item, self.source, &includes, &mut attachments);
        attachments.extend(item_ranges.iter().cloned().map(|range| SemanticRange {
            range,
            role: item_role,
        }));
        attachments.sort_by_key(|component| component.range.start);

        let components = build_components(self.source, &includes, &attachments, &self.comments)?;
        if components.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Rust {kind:?} {name} because it has no projectable source components"
            )));
            return Ok(None);
        }
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let hash = projection_hash(Language::Rust, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let source_span = source_start..item.end_byte();
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_span.start,
            &hash,
        );
        let discriminator = key_tokens(item, self.source, key_identity_range(item, kind))?;
        let key = declaration_key(
            Language::Rust,
            kind,
            &name,
            (!parent_qualifier.is_empty()).then_some(parent_qualifier),
            &discriminator,
        );
        let implicit_visibility = matches!(scope_kind, ScopeKind::Trait | ScopeKind::TraitImpl)
            && matches!(
                kind,
                DeclarationKind::Method
                    | DeclarationKind::AssociatedType
                    | DeclarationKind::Constant
                    | DeclarationKind::Macro
            );
        let visibility = rust_visibility(item, self.source, implicit_visibility)?;
        let type_use_sites =
            collect_type_use_sites(item, self.source, &components, name_node.byte_range(), kind)?;

        let index = self.declarations.len();
        self.declaration_qualifiers
            .push(qualify(parent_qualifier, &name));
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
        Ok(Some(index))
    }

    fn resolve_impl_lineage(&mut self) {
        for link in &self.impl_links {
            let target_qualifier = qualify(&link.target_scope, &link.target_name);
            let Some(parent_index) = self
                .declaration_qualifiers
                .iter()
                .enumerate()
                .find(|(index, qualifier)| {
                    **qualifier == target_qualifier
                        && matches!(
                            self.declarations[*index].kind,
                            DeclarationKind::Struct | DeclarationKind::Enum
                        )
                })
                .map(|(index, _)| index)
            else {
                continue;
            };
            let parent_id = self.declarations[parent_index].id.clone();
            let child_id = self.declarations[link.child_index].id.clone();
            self.declarations[link.child_index].parent_part = Some(parent_id);
            self.declarations[parent_index].children.push(child_id);
        }
    }
}

fn projected_item_ranges(item: Node<'_>, kind: DeclarationKind, source: &str) -> Vec<Range<usize>> {
    if matches!(kind, DeclarationKind::Trait | DeclarationKind::Module)
        && let Some(body) = item.child_by_field_name("body")
        && let Some(header_end) = opening_delimiter_end(body, source)
    {
        let mut ranges = vec![item.start_byte()..header_end];
        if let Some(close_start) = closing_delimiter_start(body, source) {
            let close_with_layout = leading_whitespace_start(source, close_start, header_end);
            if close_with_layout < item.end_byte() {
                ranges.push(close_with_layout..item.end_byte());
            }
        }
        normalize_ranges(&mut ranges);
        return ranges;
    }

    let mut end = match kind {
        DeclarationKind::Function | DeclarationKind::Method => item
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
    (end > item.start_byte())
        .then_some(item.start_byte()..end)
        .into_iter()
        .collect()
}

fn opening_delimiter_end(body: Node<'_>, source: &str) -> Option<usize> {
    source.as_bytes()[body.byte_range()]
        .iter()
        .position(|byte| *byte == b'{')
        .map(|offset| body.start_byte() + offset + 1)
}

fn closing_delimiter_start(body: Node<'_>, source: &str) -> Option<usize> {
    source.as_bytes()[body.byte_range()]
        .iter()
        .rposition(|byte| *byte == b'}')
        .map(|offset| body.start_byte() + offset)
}

fn leading_whitespace_start(source: &str, offset: usize, minimum: usize) -> usize {
    let mut start = offset;
    while start > minimum
        && source
            .as_bytes()
            .get(start - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        start -= 1;
    }
    start
}

fn normalize_ranges(ranges: &mut Vec<Range<usize>>) {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized = Vec::<Range<usize>>::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = normalized.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            normalized.push(range);
        }
    }
    *ranges = normalized;
}

fn build_components(
    source: &str,
    included_ranges: &[Range<usize>],
    semantic_ranges: &[SemanticRange],
    comments: &[CommentRange],
) -> Result<Vec<SourceComponent>> {
    let (Some(first), Some(last)) = (included_ranges.first(), included_ranges.last()) else {
        return Ok(Vec::new());
    };
    let surface = first.start..last.end;
    let excluded = comments
        .iter()
        .filter(|comment| {
            !comment.documentation
                && included_ranges.iter().any(|included| {
                    comment.range.start < included.end && comment.range.end > included.start
                })
        })
        .map(|comment| comment.range.clone())
        .collect::<Vec<_>>();

    let mut boundaries = vec![surface.start, surface.end];
    for included in included_ranges {
        boundaries.push(included.start);
        boundaries.push(included.end);
    }
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
            || !included_ranges
                .iter()
                .any(|included| included.start <= range.start && included.end >= range.end)
            || excluded
                .iter()
                .any(|comment| comment.start < range.end && comment.end > range.start)
        {
            continue;
        }
        let role = semantic_ranges
            .iter()
            .filter(|semantic| {
                semantic.range.start <= range.start && semantic.range.end >= range.end
            })
            .min_by_key(|semantic| semantic.range.end - semantic.range.start)
            .map_or(SourceComponentRole::Layout, |semantic| semantic.role);
        let text = source
            .get(range.clone())
            .context("Rust declaration component was not on UTF-8 boundaries")?
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
fn collect_attribute_semantics(
    node: Node<'_>,
    source: &str,
    includes: &[Range<usize>],
    semantics: &mut Vec<SemanticRange>,
) {
    if node.kind() == "attribute_item"
        && includes
            .iter()
            .any(|included| included.start <= node.start_byte() && included.end >= node.end_byte())
    {
        semantics.push(SemanticRange {
            range: node.byte_range(),
            role: if is_doc_attribute(node, source) {
                SourceComponentRole::Documentation
            } else {
                SourceComponentRole::Attribute
            },
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_attribute_semantics(child, source, includes, semantics);
    }
}

fn key_identity_range(item: Node<'_>, kind: DeclarationKind) -> Range<usize> {
    match kind {
        DeclarationKind::Function
        | DeclarationKind::Method
        | DeclarationKind::Trait
        | DeclarationKind::Module => impl_header_range(item),
        DeclarationKind::Constant | DeclarationKind::Static => {
            item.child_by_field_name("value").map_or_else(
                || item.byte_range(),
                |value| item.start_byte()..value.start_byte(),
            )
        }
        DeclarationKind::Macro => {
            let mut cursor = item.walk();
            item.named_children(&mut cursor)
                .find(|child| child.kind() == "macro_rule")
                .map_or_else(
                    || item.byte_range(),
                    |rule| item.start_byte()..rule.start_byte(),
                )
        }
        _ => item.byte_range(),
    }
}

fn impl_header_range(item: Node<'_>) -> Range<usize> {
    item.child_by_field_name("body").map_or_else(
        || item.byte_range(),
        |body| item.start_byte()..body.start_byte(),
    )
}

fn key_tokens(item: Node<'_>, source: &str, range: Range<usize>) -> Result<String> {
    fn collect(
        node: Node<'_>,
        source: &str,
        range: &Range<usize>,
        output: &mut String,
    ) -> Result<()> {
        if node.start_byte() >= range.end || node.end_byte() <= range.start {
            return Ok(());
        }
        if matches!(node.kind(), "line_comment" | "block_comment")
            || (node.kind() == "attribute_item" && is_doc_attribute(node, source))
        {
            return Ok(());
        }
        if node.child_count() == 0 {
            if node.start_byte() >= range.start && node.end_byte() <= range.end {
                let token = node_text(node, source)
                    .context("Rust identity token was not on UTF-8 boundaries")?;
                output.push_str(&token.len().to_string());
                output.push(':');
                output.push_str(token);
                output.push(';');
            }
            return Ok(());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect(child, source, range, output)?;
        }
        Ok(())
    }

    let mut output = String::new();
    collect(item, source, &range, &mut output)?;
    Ok(output)
}

fn qualify(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}::{name}")
    }
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

fn has_syntax_error_in_ranges(node: Node<'_>, ranges: &[Range<usize>]) -> bool {
    ranges
        .iter()
        .any(|range| has_syntax_error_in_range(node, range))
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
    fn first_type_identifier(node: Node<'_>) -> Option<Node<'_>> {
        if node.kind() == "type_identifier" {
            return Some(node);
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find_map(first_type_identifier)
    }

    item.child_by_field_name("type")
        .and_then(first_type_identifier)
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
    item: Node<'_>,
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
        && !is_declared_type_parameter(node)
        && components.iter().any(|component| {
            component.source_range.start <= range.start && component.source_range.end >= range.end
        })
    {
        sites.push(TypeUseSite {
            name: source
                .get(range.clone())
                .context("Rust type-use site was not on UTF-8 boundaries")?
                .to_owned(),
            role: rust_type_use_role(item, node, declaration_kind),
            source_range: range,
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_identifiers(
            item,
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

fn rust_type_use_role(
    item: Node<'_>,
    node: Node<'_>,
    declaration_kind: DeclarationKind,
) -> TypeUseRole {
    let range = node.byte_range();
    if matches!(
        declaration_kind,
        DeclarationKind::TypeAlias | DeclarationKind::AssociatedType
    ) && item
        .child_by_field_name("type")
        .is_some_and(|target| contains_range(target.byte_range(), &range))
    {
        return TypeUseRole::AliasTarget;
    }
    if item
        .child_by_field_name("parameters")
        .is_some_and(|parameters| contains_range(parameters.byte_range(), &range))
    {
        return TypeUseRole::Parameter;
    }
    if item
        .child_by_field_name("return_type")
        .is_some_and(|return_type| contains_range(return_type.byte_range(), &range))
    {
        return TypeUseRole::Return;
    }
    if has_ancestor_kind(node, item, "enum_variant") {
        return TypeUseRole::Variant;
    }
    if declaration_kind == DeclarationKind::Struct
        && item
            .child_by_field_name("body")
            .is_some_and(|body| contains_range(body.byte_range(), &range))
    {
        return TypeUseRole::Field;
    }
    if has_any_ancestor_kind(
        node,
        item,
        &["trait_bounds", "where_clause", "type_parameter"],
    ) {
        return TypeUseRole::Bound;
    }
    TypeUseRole::Other
}

fn is_declared_type_parameter(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "type_parameter"
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.byte_range() == node.byte_range())
    })
}

fn has_ancestor_kind(node: Node<'_>, item: Node<'_>, kind: &str) -> bool {
    has_any_ancestor_kind(node, item, &[kind])
}

fn has_any_ancestor_kind(mut node: Node<'_>, item: Node<'_>, kinds: &[&str]) -> bool {
    while let Some(parent) = node.parent() {
        if parent == item {
            return false;
        }
        if kinds.contains(&parent.kind()) {
            return true;
        }
        node = parent;
    }
    false
}

fn contains_range(container: Range<usize>, range: &Range<usize>) -> bool {
    container.start <= range.start && container.end >= range.end
}
