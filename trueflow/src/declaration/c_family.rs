use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tree_sitter::{Node, Parser};

use crate::analysis::Language;

use super::projection::{declaration_id, declaration_key};
use super::{
    DeclarationId, DeclarationKind, DeclarationNode, ProjectionDiagnostic, SourceComponent,
    SourceComponentRole, TypeUseRole, TypeUseSite, Visibility, projection_hash,
};

#[derive(Debug, Clone)]
struct Part {
    range: Range<usize>,
    role: SourceComponentRole,
}

#[derive(Debug, Clone)]
struct Comment {
    range: Range<usize>,
}

#[derive(Debug, Clone)]
struct NameInfo {
    name: String,
    range: Range<usize>,
    qualifier: Option<String>,
}

struct Projector<'a> {
    path: &'a Path,
    language: Language,
    source: &'a str,
    comments: Vec<Comment>,
    next_ordinal: usize,
    declarations: Vec<DeclarationNode>,
    diagnostics: Vec<ProjectionDiagnostic>,
}

pub(super) fn project(
    path: &Path,
    language: Language,
    source: &str,
) -> Result<(Vec<DeclarationNode>, Vec<ProjectionDiagnostic>)> {
    let grammar = match language {
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        other => bail!("the C-family declaration projector cannot parse {other:?}"),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .context("failed to load the C/C++ grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a C/C++ syntax tree")?;

    let mut comments = Vec::new();
    collect_comments(tree.root_node(), source, &mut comments);
    let mut projector = Projector {
        path,
        language,
        source,
        comments,
        next_ordinal: 0,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
    };
    projector.collect_scope(tree.root_node(), None, None, &Visibility::Public)?;
    if tree.root_node().has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "C/C++ source contains syntax errors; declarations with errors in projected surfaces were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect_scope(
        &mut self,
        scope: Node<'_>,
        parent_id: Option<&DeclarationId>,
        parent_name: Option<&str>,
        default_visibility: &Visibility,
    ) -> Result<()> {
        let mut pending = Vec::<Part>::new();
        let mut cursor = scope.walk();
        for child in scope.named_children(&mut cursor) {
            if child.kind() == "comment" {
                self.update_pending_comment(child, &mut pending);
                continue;
            }
            if is_preprocessor_container(child.kind()) {
                pending.clear();
                self.collect_scope(child, parent_id, parent_name, default_visibility)?;
                continue;
            }
            if child.kind() == "namespace_definition" {
                pending.clear();
                if let Some(body) = child.child_by_field_name("body") {
                    let namespace = child
                        .child_by_field_name("name")
                        .and_then(|node| node_text(node, self.source))
                        .filter(|name| !name.is_empty());
                    let qualified = qualify(parent_name, namespace);
                    if namespace.is_some() {
                        self.collect_scope(
                            body,
                            parent_id,
                            qualified.as_deref(),
                            default_visibility,
                        )?;
                    } else {
                        self.collect_scope(
                            body,
                            parent_id,
                            qualified.as_deref(),
                            &Visibility::Private,
                        )?;
                    }
                }
                continue;
            }

            self.expire_detached_docs(child.start_byte(), &mut pending);
            if let Some(specifier) = aggregate_specifier(child) {
                if is_aggregate_declaration(child, specifier) {
                    let attachments = std::mem::take(&mut pending);
                    self.add_aggregate(
                        child,
                        specifier,
                        attachments,
                        parent_id.cloned(),
                        parent_name,
                        default_visibility.clone(),
                    )?;
                } else {
                    pending.clear();
                }
                continue;
            }
            if let Some(declarators) = callable_declarators(child) {
                let attachments = std::mem::take(&mut pending);
                for (index, declarator) in declarators.into_iter().enumerate() {
                    self.add_callable(
                        child,
                        declarator,
                        if index == 0 {
                            attachments.clone()
                        } else {
                            Vec::new()
                        },
                        parent_id.cloned(),
                        parent_name,
                        default_visibility.clone(),
                        false,
                    )?;
                }
                continue;
            }
            pending.clear();
        }
        Ok(())
    }

    fn add_aggregate(
        &mut self,
        item: Node<'_>,
        specifier: Node<'_>,
        attachments: Vec<Part>,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
        visibility: Visibility,
    ) -> Result<()> {
        let Some(name_info) = aggregate_name(item, specifier, self.source) else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted anonymous C/C++ aggregate at byte {} because it has no typedef or declared name",
                item.start_byte()
            )));
            return Ok(());
        };
        let kind = match specifier.kind() {
            "enum_specifier" => DeclarationKind::Enum,
            "class_specifier" => DeclarationKind::Class,
            "struct_specifier" | "union_specifier" => DeclarationKind::Struct,
            _ => return Ok(()),
        };
        if specifier.kind() == "union_specifier" {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "C/C++ union {} is represented as Struct because the declaration model has no union kind",
                name_info.name
            )));
        }

        let mut parts = attachments;
        parts.extend(aggregate_parts(item, specifier, self.source));
        let item_end = aggregate_item_end(item, self.source);
        if self.parts_have_errors(item, &parts) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted C/C++ {kind:?} {} because its projected surface contains a syntax error",
                name_info.name
            )));
            return Ok(());
        }
        let components = self.components(item, &parts)?;
        if components.is_empty() {
            return Ok(());
        }
        let projection_text = component_text(&components);
        let hash = projection_hash(self.language, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let source_start = parts
            .iter()
            .map(|part| part.range.start)
            .min()
            .unwrap_or(item.start_byte());
        let source_span = source_start..item_end;
        let id = declaration_id(
            self.path,
            kind,
            &name_info.name,
            source_ordinal,
            source_span.start,
            &hash,
        );
        let key = declaration_key(
            self.language,
            kind,
            &name_info.name,
            parent_name,
            &projection_text,
        );
        let type_use_sites =
            collect_type_use_sites(item, self.source, &components, &name_info.range, kind)?;
        let aggregate_index = self.declarations.len();
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name: name_info.name.clone(),
            kind,
            visibility,
            parent_part: parent_id,
            source_ordinal,
            source_span,
            components,
            projection_text,
            projection_hash: hash,
            review_owner: id.clone(),
            children: Vec::new(),
            type_use_sites,
        });

        if let Some(body) = specifier.child_by_field_name("body") {
            let children_start = self.declarations.len();
            let member_visibility = if specifier.kind() == "class_specifier" {
                Visibility::Private
            } else {
                Visibility::Public
            };
            self.collect_member_scope(body, &id, &name_info.name, member_visibility)?;
            self.declarations[aggregate_index].children = self.declarations[children_start..]
                .iter()
                .map(|declaration| declaration.id.clone())
                .collect();
        }
        Ok(())
    }

    fn collect_member_scope(
        &mut self,
        body: Node<'_>,
        parent_id: &DeclarationId,
        parent_name: &str,
        mut visibility: Visibility,
    ) -> Result<()> {
        let mut pending = Vec::<Part>::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "comment" {
                self.update_pending_comment(child, &mut pending);
                continue;
            }
            if child.kind() == "access_specifier" {
                pending.clear();
                visibility = access_visibility(child, self.source).unwrap_or(visibility);
                continue;
            }
            self.expire_detached_docs(child.start_byte(), &mut pending);
            if let Some(specifier) = aggregate_specifier(child) {
                if is_aggregate_declaration(child, specifier) {
                    self.add_aggregate(
                        child,
                        specifier,
                        std::mem::take(&mut pending),
                        Some(parent_id.clone()),
                        Some(parent_name),
                        visibility.clone(),
                    )?;
                } else {
                    pending.clear();
                }
                continue;
            }
            if let Some(declarators) = callable_declarators(child) {
                let attachments = std::mem::take(&mut pending);
                let friend = child.kind() == "friend_declaration";
                for (index, declarator) in declarators.into_iter().enumerate() {
                    self.add_callable(
                        child,
                        declarator,
                        if index == 0 {
                            attachments.clone()
                        } else {
                            Vec::new()
                        },
                        (!friend).then(|| parent_id.clone()),
                        (!friend).then_some(parent_name),
                        if friend {
                            Visibility::Public
                        } else {
                            visibility.clone()
                        },
                        !friend,
                    )?;
                }
                continue;
            }
            pending.clear();
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_callable(
        &mut self,
        item: Node<'_>,
        declarator: Node<'_>,
        mut attachments: Vec<Part>,
        parent_id: Option<DeclarationId>,
        lexical_parent_name: Option<&str>,
        visibility: Visibility,
        member_scope: bool,
    ) -> Result<()> {
        let Some(name_info) = callable_name(declarator, self.source) else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted C/C++ callable at byte {} because its declarator has no reliable name",
                item.start_byte()
            )));
            return Ok(());
        };
        let visibility = if name_info.qualifier.is_some() && !member_scope {
            Visibility::Implicit
        } else if !member_scope && has_static_storage(item, self.source) {
            Visibility::Private
        } else {
            visibility
        };
        let type_node = function_node(item).and_then(|node| node.child_by_field_name("type"));
        let effective_parent = name_info.qualifier.as_deref().or(lexical_parent_name);
        let kind = callable_kind(
            &name_info.name,
            effective_parent,
            type_node.is_none(),
            member_scope || name_info.qualifier.is_some(),
        );
        let Some(signature_range) = callable_signature_range(item, self.source) else {
            return Ok(());
        };
        attachments.push(Part {
            range: signature_range,
            role: SourceComponentRole::Signature,
        });
        if self.parts_have_errors(item, &attachments) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted C/C++ {kind:?} {} because its signature contains a syntax error",
                name_info.name
            )));
            return Ok(());
        }
        let components = self.components(item, &attachments)?;
        if components.is_empty() {
            return Ok(());
        }
        let projection_text = component_text(&components);
        let hash = projection_hash(self.language, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let source_start = attachments
            .iter()
            .map(|part| part.range.start)
            .min()
            .unwrap_or(item.start_byte());
        let source_span = source_start..item.end_byte();
        let id = declaration_id(
            self.path,
            kind,
            &name_info.name,
            source_ordinal,
            source_span.start,
            &hash,
        );
        let key = declaration_key(
            self.language,
            kind,
            &name_info.name,
            effective_parent,
            &projection_text,
        );
        let type_use_sites =
            collect_type_use_sites(item, self.source, &components, &name_info.range, kind)?;
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name: name_info.name,
            kind,
            visibility,
            parent_part: parent_id,
            source_ordinal,
            source_span,
            components,
            projection_text,
            projection_hash: hash,
            review_owner: id,
            children: Vec::new(),
            type_use_sites,
        });
        Ok(())
    }

    fn components(&self, item: Node<'_>, parts: &[Part]) -> Result<Vec<SourceComponent>> {
        let attributes = attribute_ranges(item);
        materialize_components(self.source, parts, &attributes, &self.comments)
    }

    fn update_pending_comment(&self, comment: Node<'_>, pending: &mut Vec<Part>) {
        let Some(text) = node_text(comment, self.source) else {
            pending.clear();
            return;
        };
        if !is_doxygen(text)
            || pending.last().is_some_and(|previous| {
                !immediately_precedes(self.source, previous.range.end, comment.start_byte())
            })
        {
            pending.clear();
        }
        if is_doxygen(text) {
            pending.push(Part {
                range: whole_comment_line(self.source, comment.byte_range()),
                role: SourceComponentRole::Documentation,
            });
        }
    }

    fn expire_detached_docs(&self, next_start: usize, pending: &mut Vec<Part>) {
        if pending
            .last()
            .is_some_and(|part| !immediately_precedes(self.source, part.range.end, next_start))
        {
            pending.clear();
        }
    }

    fn parts_have_errors(&self, item: Node<'_>, parts: &[Part]) -> bool {
        parts
            .iter()
            .any(|part| has_syntax_error_in_range(item, &part.range))
    }
}

fn aggregate_parts(item: Node<'_>, specifier: Node<'_>, source: &str) -> Vec<Part> {
    let item_end = aggregate_item_end(item, source);
    let Some(body) = specifier.child_by_field_name("body") else {
        return vec![Part {
            range: item.start_byte()..item_end,
            role: SourceComponentRole::AggregateShape,
        }];
    };
    if specifier.kind() == "enum_specifier" {
        return vec![Part {
            range: item.start_byte()..item_end,
            role: SourceComponentRole::AggregateShape,
        }];
    }

    let open_end = body.start_byte().saturating_add(1).min(body.end_byte());
    let mut parts = vec![Part {
        range: item.start_byte()..open_end,
        role: SourceComponentRole::AggregateShape,
    }];
    let mut previous_end = open_end;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let prefix_start = whitespace_suffix_start(source, previous_end, member.start_byte());
        let member_end = if member.kind() == "access_specifier" {
            access_specifier_end(member, source)
        } else {
            member.end_byte()
        };
        match member.kind() {
            "access_specifier" => parts.push(Part {
                range: prefix_start..member_end,
                role: SourceComponentRole::AggregateShape,
            }),
            "field_declaration" => {
                if contains_callable_declarator(member) {
                    parts.push(Part {
                        range: prefix_start..member.end_byte(),
                        role: SourceComponentRole::AggregateShape,
                    });
                } else {
                    if prefix_start < member.start_byte() {
                        parts.push(Part {
                            range: prefix_start..member.start_byte(),
                            role: SourceComponentRole::AggregateShape,
                        });
                    }
                    parts.extend(field_shape_parts(member));
                }
            }
            "declaration" => {
                if contains_callable_declarator(member) {
                    parts.push(Part {
                        range: prefix_start..member.end_byte(),
                        role: SourceComponentRole::AggregateShape,
                    });
                } else if aggregate_specifier(member).is_none() {
                    if prefix_start < member.start_byte() {
                        parts.push(Part {
                            range: prefix_start..member.start_byte(),
                            role: SourceComponentRole::AggregateShape,
                        });
                    }
                    parts.extend(field_shape_parts(member));
                }
            }
            "function_definition" => {
                if member.child_by_field_name("body").is_none() {
                    parts.push(Part {
                        range: prefix_start..member.end_byte(),
                        role: SourceComponentRole::AggregateShape,
                    });
                }
            }
            "template_declaration" => {
                if let Some(function) = function_node(member)
                    && function.child_by_field_name("body").is_none()
                {
                    parts.push(Part {
                        range: prefix_start..member.end_byte(),
                        role: SourceComponentRole::AggregateShape,
                    });
                }
            }
            _ => {}
        }
        previous_end = member_end;
    }
    let close_start =
        whitespace_suffix_start(source, previous_end, body.end_byte().saturating_sub(1));
    if close_start < body.end_byte() {
        parts.push(Part {
            range: close_start..body.end_byte(),
            role: SourceComponentRole::AggregateShape,
        });
    }
    if body.end_byte() < item_end {
        parts.push(Part {
            range: body.end_byte()..item_end,
            role: SourceComponentRole::AggregateShape,
        });
    }
    parts
}

fn field_shape_parts(field: Node<'_>) -> Vec<Part> {
    let mut removals = Vec::<Range<usize>>::new();
    collect_initializer_removals(field, &mut removals);
    removals.sort_by_key(|range| range.start);
    let mut parts = Vec::new();
    let mut start = field.start_byte();
    for removal in removals {
        if removal.start > start {
            parts.push(Part {
                range: start..removal.start,
                role: SourceComponentRole::AggregateShape,
            });
        }
        start = start.max(removal.end);
    }
    if start < field.end_byte() {
        parts.push(Part {
            range: start..field.end_byte(),
            role: SourceComponentRole::AggregateShape,
        });
    }
    parts
}

fn collect_initializer_removals(node: Node<'_>, removals: &mut Vec<Range<usize>>) {
    if node.kind() == "init_declarator"
        && let (Some(declarator), Some(value)) = (
            node.child_by_field_name("declarator"),
            node.child_by_field_name("value"),
        )
    {
        removals.push(initializer_removal(declarator.end_byte(), value));
        return;
    }
    let mut cursor = node.walk();
    let mut last_declarator_end = None;
    for (index, child) in node.children(&mut cursor).enumerate() {
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        let field_name = node.field_name_for_child(index);
        if field_name == Some("declarator") {
            last_declarator_end = Some(child.end_byte());
        }
        if field_name == Some("default_value") {
            removals.push(initializer_removal(
                last_declarator_end.unwrap_or(node.start_byte()),
                child,
            ));
        } else if child.is_named() {
            collect_initializer_removals(child, removals);
        }
    }
}

fn initializer_removal(declarator_end: usize, value: Node<'_>) -> Range<usize> {
    declarator_end..value.end_byte()
}

fn callable_signature_range(item: Node<'_>, source: &str) -> Option<Range<usize>> {
    let function = function_node(item).unwrap_or(item);
    let mut end = function
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or(item.end_byte());
    if let Some(initializers) = direct_named_child(function, "field_initializer_list") {
        end = end.min(initializers.start_byte());
    }
    if item.kind() == "template_declaration" {
        end = end.min(
            function
                .child_by_field_name("body")
                .map(|body| body.start_byte())
                .unwrap_or(item.end_byte()),
        );
    }
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

fn callable_declarators(node: Node<'_>) -> Option<Vec<Node<'_>>> {
    let function = function_node(node)?;
    let mut found = Vec::new();
    collect_entity_declarators(function, &mut found);
    (!found.is_empty()).then_some(found)
}

fn collect_entity_declarators<'a>(node: Node<'a>, found: &mut Vec<Node<'a>>) {
    if matches!(node.kind(), "function_declarator" | "operator_cast") {
        if node.kind() == "operator_cast" || !is_function_pointer_variable(node) {
            found.push(node);
        }
        return;
    }
    if matches!(
        node.kind(),
        "parameter_declaration" | "optional_parameter_declaration" | "compound_statement"
    ) {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_entity_declarators(child, found);
    }
}

fn is_function_pointer_variable(function: Node<'_>) -> bool {
    let Some(mut declarator) = function.child_by_field_name("declarator") else {
        return false;
    };
    if declarator.kind() != "parenthesized_declarator" {
        return false;
    }
    while let Some(child) = declarator.child_by_field_name("declarator") {
        if child.kind() == "pointer_declarator" {
            return true;
        }
        declarator = child;
    }
    false
}

fn contains_callable_declarator(node: Node<'_>) -> bool {
    callable_declarators(node).is_some()
}

fn function_node(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "function_definition" | "declaration" | "field_declaration"
    ) {
        return contains_callable_syntax(node).then_some(node);
    }
    if matches!(node.kind(), "template_declaration" | "friend_declaration") {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(function) = function_node(child) {
                return Some(function);
            }
        }
    }
    None
}

fn contains_callable_syntax(node: Node<'_>) -> bool {
    if matches!(node.kind(), "function_declarator" | "operator_cast") {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(contains_callable_syntax)
}

fn callable_name(declarator: Node<'_>, source: &str) -> Option<NameInfo> {
    if declarator.kind() == "operator_cast" {
        let ty = declarator.child_by_field_name("type")?;
        let range = declarator.start_byte()..ty.end_byte();
        return Some(NameInfo {
            name: source.get(range.clone())?.to_owned(),
            range,
            qualifier: None,
        });
    }
    let mut current = declarator.child_by_field_name("declarator")?;
    loop {
        match current.kind() {
            "identifier" | "field_identifier" | "operator_name" | "destructor_name" => {
                let range = current.byte_range();
                return Some(NameInfo {
                    name: source.get(range.clone())?.to_owned(),
                    range,
                    qualifier: None,
                });
            }
            "qualified_identifier" => {
                let whole = node_text(current, source)?;
                let name_node = current.child_by_field_name("name")?;
                let mut info = callable_name_from_name_node(name_node, source)?;
                info.qualifier = whole
                    .rsplit_once("::")
                    .map(|(qualifier, _)| qualifier.to_owned());
                return Some(info);
            }
            "operator_cast" => return callable_name(current, source),
            _ => current = current.child_by_field_name("declarator")?,
        }
    }
}

fn callable_name_from_name_node(node: Node<'_>, source: &str) -> Option<NameInfo> {
    if node.kind() == "operator_cast" {
        return callable_name(node, source);
    }
    let range = node.byte_range();
    Some(NameInfo {
        name: source.get(range.clone())?.to_owned(),
        range,
        qualifier: None,
    })
}

fn callable_kind(
    name: &str,
    parent_name: Option<&str>,
    missing_return_type: bool,
    member: bool,
) -> DeclarationKind {
    if name.starts_with("operator") {
        return DeclarationKind::Operator;
    }
    if name.starts_with('~') {
        return DeclarationKind::Destructor;
    }
    if missing_return_type
        && parent_name.is_some_and(|parent| {
            parent
                .rsplit("::")
                .next()
                .and_then(|tail| tail.split('<').next())
                .is_some_and(|tail| tail == name)
        })
    {
        return DeclarationKind::Constructor;
    }
    if member {
        DeclarationKind::Method
    } else {
        DeclarationKind::Function
    }
}
fn is_callable_kind(kind: DeclarationKind) -> bool {
    matches!(
        kind,
        DeclarationKind::Function
            | DeclarationKind::Method
            | DeclarationKind::Constructor
            | DeclarationKind::Destructor
            | DeclarationKind::Operator
    )
}

fn aggregate_specifier(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "struct_specifier" | "union_specifier" | "class_specifier" | "enum_specifier"
    ) {
        return Some(node);
    }
    if matches!(
        node.kind(),
        "declaration" | "type_definition" | "template_declaration" | "friend_declaration"
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(specifier) = aggregate_specifier(child) {
                return Some(specifier);
            }
        }
    }
    None
}

fn is_aggregate_declaration(item: Node<'_>, specifier: Node<'_>) -> bool {
    if specifier.child_by_field_name("body").is_some() || item.id() == specifier.id() {
        return true;
    }
    let mut cursor = item.walk();
    !item.named_children(&mut cursor).any(|child| {
        child.id() != specifier.id()
            && matches!(
                child.kind(),
                "identifier"
                    | "field_identifier"
                    | "pointer_declarator"
                    | "array_declarator"
                    | "init_declarator"
            )
    })
}

fn aggregate_name(item: Node<'_>, specifier: Node<'_>, source: &str) -> Option<NameInfo> {
    if item.kind() == "type_definition"
        && let Some(alias) = item.child_by_field_name("declarator")
    {
        let name = terminal_declarator_name(alias)?;
        let range = name.byte_range();
        return Some(NameInfo {
            name: source.get(range.clone())?.to_owned(),
            range,
            qualifier: None,
        });
    }
    let name = specifier.child_by_field_name("name")?;
    let range = name.byte_range();
    let text = source.get(range.clone())?;
    Some(NameInfo {
        name: text.rsplit("::").next().unwrap_or(text).to_owned(),
        range,
        qualifier: text.rsplit_once("::").map(|(prefix, _)| prefix.to_owned()),
    })
}

fn terminal_declarator_name(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if matches!(node.kind(), "type_identifier" | "identifier") {
            return Some(node);
        }
        node = node.child_by_field_name("declarator")?;
    }
}

fn aggregate_item_end(item: Node<'_>, source: &str) -> usize {
    if !matches!(
        item.kind(),
        "struct_specifier" | "union_specifier" | "class_specifier" | "enum_specifier"
    ) {
        return item.end_byte();
    }
    let bytes = source.as_bytes();
    let mut end = item.end_byte();
    while bytes
        .get(end)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        end += 1;
    }
    if bytes.get(end) == Some(&b';') {
        end + 1
    } else {
        item.end_byte()
    }
}

fn access_specifier_end(node: Node<'_>, source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut end = node.end_byte();
    while bytes
        .get(end)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        end += 1;
    }
    if bytes.get(end) == Some(&b':') {
        end + 1
    } else {
        node.end_byte()
    }
}

fn access_visibility(node: Node<'_>, source: &str) -> Option<Visibility> {
    match node_text(node, source)?.trim().trim_end_matches(':') {
        "public" => Some(Visibility::Public),
        "protected" => Some(Visibility::Protected),
        "private" => Some(Visibility::Private),
        _ => None,
    }
}

fn materialize_components(
    source: &str,
    parts: &[Part],
    attributes: &[Range<usize>],
    comments: &[Comment],
) -> Result<Vec<SourceComponent>> {
    let mut parts = parts.to_vec();
    parts.sort_by_key(|part| part.range.start);
    let mut boundaries = Vec::new();
    for part in &parts {
        boundaries.push(part.range.start);
        boundaries.push(part.range.end);
        for attribute in attributes {
            if attribute.start < part.range.end && attribute.end > part.range.start {
                boundaries.push(attribute.start.max(part.range.start));
                boundaries.push(attribute.end.min(part.range.end));
            }
        }
        for comment in comments {
            if comment.range.start < part.range.end && comment.range.end > part.range.start {
                boundaries.push(comment.range.start.max(part.range.start));
                boundaries.push(comment.range.end.min(part.range.end));
            }
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut components = Vec::new();
    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        let Some(part) = parts
            .iter()
            .find(|part| part.range.start <= range.start && part.range.end >= range.end)
        else {
            continue;
        };
        if part.role != SourceComponentRole::Documentation
            && comments
                .iter()
                .any(|comment| comment.range.start < range.end && comment.range.end > range.start)
        {
            continue;
        }
        let role = if attributes
            .iter()
            .any(|attribute| attribute.start <= range.start && attribute.end >= range.end)
        {
            SourceComponentRole::Attribute
        } else {
            part.role
        };
        let text = source
            .get(range.clone())
            .context("C/C++ declaration component was not on UTF-8 boundaries")?;
        if !text.is_empty() {
            components.push(SourceComponent {
                role,
                source_range: range,
                text: text.to_owned(),
            });
        }
    }
    Ok(components)
}

fn attribute_ranges(item: Node<'_>) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    collect_attribute_ranges(item, &mut ranges);
    ranges
}

fn collect_attribute_ranges(node: Node<'_>, ranges: &mut Vec<Range<usize>>) {
    if matches!(
        node.kind(),
        "attribute_declaration" | "attribute_specifier" | "ms_declspec_modifier"
    ) {
        ranges.push(node.byte_range());
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_attribute_ranges(child, ranges);
    }
}

fn collect_comments(node: Node<'_>, source: &str, comments: &mut Vec<Comment>) {
    if node.kind() == "comment" {
        let range = whole_comment_line(source, node.byte_range());
        comments.push(Comment { range });
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_comments(child, source, comments);
    }
}

fn whole_comment_line(source: &str, range: Range<usize>) -> Range<usize> {
    let bytes = source.as_bytes();
    let mut end = range.end;
    while bytes
        .get(end)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        end += 1;
    }
    if bytes.get(end) == Some(&b'\r') {
        end += 1;
    }
    if bytes.get(end) == Some(&b'\n') {
        end += 1;
    }
    range.start..end
}

fn is_doxygen(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with("///<")
        || trimmed.starts_with("//!<")
        || trimmed.starts_with("/**<")
        || trimmed.starts_with("/*!<")
    {
        return false;
    }
    trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/**")
        || trimmed.starts_with("/*!")
}

fn immediately_precedes(source: &str, previous_end: usize, next_start: usize) -> bool {
    source
        .get(previous_end..next_start)
        .is_some_and(|gap| gap.chars().all(|character| matches!(character, ' ' | '\t')))
}

fn whitespace_suffix_start(source: &str, lower: usize, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut start = end;
    while start > lower && bytes.get(start - 1).is_some_and(u8::is_ascii_whitespace) {
        start -= 1;
    }
    start
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

fn collect_type_use_sites(
    item: Node<'_>,
    source: &str,
    components: &[SourceComponent],
    declaration_name: &Range<usize>,
    declaration_kind: DeclarationKind,
) -> Result<Vec<TypeUseSite>> {
    let mut sites = Vec::new();
    collect_type_identifiers(
        item,
        source,
        components,
        declaration_name,
        declaration_kind,
        &mut sites,
    )?;
    sites.sort_by_key(|site| site.source_range.start);
    sites.dedup_by(|left, right| left.source_range == right.source_range);
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
        && !is_template_parameter_name(node)
        && components.iter().any(|component| {
            component.source_range.start <= range.start && component.source_range.end >= range.end
        })
    {
        sites.push(TypeUseSite {
            name: source
                .get(range.clone())
                .context("C/C++ type-use site was not on UTF-8 boundaries")?
                .to_owned(),
            role: type_use_role(node, declaration_kind),
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

fn type_use_role(mut node: Node<'_>, declaration_kind: DeclarationKind) -> TypeUseRole {
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "parameter_declaration" | "optional_parameter_declaration" => {
                return TypeUseRole::Parameter;
            }
            "trailing_return_type" => return TypeUseRole::Return,
            "base_class_clause"
            | "requires_clause"
            | "constraint_type"
            | "template_parameter_list"
            | "enum_specifier" => return TypeUseRole::Bound,
            "field_declaration" => {
                return if is_callable_kind(declaration_kind) {
                    TypeUseRole::Return
                } else {
                    TypeUseRole::Field
                };
            }
            "type_definition" | "alias_declaration" => return TypeUseRole::AliasTarget,
            "function_definition" | "declaration" => break,
            _ => node = parent,
        }
    }
    if is_callable_kind(declaration_kind) {
        TypeUseRole::Return
    } else {
        TypeUseRole::Other
    }
}

fn is_template_parameter_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "type_parameter_declaration" | "variadic_type_parameter_declaration" => true,
        "optional_type_parameter_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.byte_range() == node.byte_range()),
        _ => false,
    }
}

fn direct_named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn has_static_storage(node: Node<'_>, source: &str) -> bool {
    if matches!(node.kind(), "compound_statement" | "try_statement") {
        return false;
    }
    if node.kind() == "storage_class_specifier"
        && node_text(node, source).is_some_and(|text| text.trim() == "static")
    {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| has_static_storage(child, source))
}

fn is_preprocessor_container(kind: &str) -> bool {
    matches!(kind, "preproc_if" | "preproc_ifdef" | "preproc_else")
}

fn qualify(parent: Option<&str>, child: Option<&str>) -> Option<String> {
    match (parent, child) {
        (Some(parent), Some(child)) => Some(format!("{parent}::{child}")),
        (Some(parent), None) => Some(parent.to_owned()),
        (None, Some(child)) => Some(child.to_owned()),
        (None, None) => None,
    }
}

fn component_text(components: &[SourceComponent]) -> String {
    components
        .iter()
        .map(|component| component.text.as_str())
        .collect()
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}
