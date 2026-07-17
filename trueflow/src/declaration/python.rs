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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignmentDeclarationKind {
    Constant,
    Static,
    TypeAlias,
}

struct Projector<'a> {
    path: &'a Path,
    source: &'a str,
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
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("failed to load the Python grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a Python syntax tree")?;

    let mut projector = Projector {
        path,
        source,
        next_ordinal: 0,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
    };
    projector.collect_module(tree.root_node())?;
    if tree.root_node().has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "Python source contains syntax errors; malformed projected surfaces were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect_module(&mut self, module: Node<'_>) -> Result<()> {
        self.collect_module_scope(module)
    }

    fn collect_module_scope(&mut self, scope: Node<'_>) -> Result<()> {
        let mut cursor = scope.walk();
        for child in scope.named_children(&mut cursor) {
            if let Some((definition, decorators, outer)) = unpack_definition(child) {
                match definition.kind() {
                    "function_definition" => {
                        self.add_callable(definition, &decorators, outer, None, None)?;
                    }
                    "class_definition" => {
                        self.add_class(definition, &decorators, outer, None, None)?;
                    }
                    _ => {}
                }
            } else if child.kind() == "type_alias_statement" {
                self.add_type_alias(child, None, None)?;
            } else if let Some(assignment) = statement_assignment(child)
                && explicit_assignment_kind(assignment, false, self.source).is_some()
            {
                self.add_assignment_declaration(assignment, child, None, None)?;
            } else if let Some(assignment) = statement_assignment(child) {
                self.omitted(
                    "module variable",
                    assignment.start_byte(),
                    "the declaration model has no general Python variable kind and the syntax has no explicit Final or TypeAlias marker",
                );
            } else if is_scope_preserving_compound(child) {
                self.collect_module_scope(child)?;
            }
        }
        Ok(())
    }

    fn add_callable(
        &mut self,
        function: Node<'_>,
        decorators: &[Node<'_>],
        outer: Node<'_>,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<Option<DeclarationId>> {
        let Some(name_node) = function.child_by_field_name("name") else {
            self.omitted("callable", function.start_byte(), "it has no declared name");
            return Ok(None);
        };
        let name = self
            .node_text(name_node, "Python callable name")?
            .to_owned();
        let Some(header) = definition_header_range(function) else {
            self.omitted_named("callable", &name, "its terminating colon is missing");
            return Ok(None);
        };

        let mut semantic = decorators
            .iter()
            .map(|decorator| SemanticRange {
                range: decorator.byte_range(),
                role: SourceComponentRole::Attribute,
            })
            .collect::<Vec<_>>();
        semantic.push(SemanticRange {
            range: header.clone(),
            role: SourceComponentRole::Signature,
        });
        if let Some(docstring) = first_docstring(function, self.source) {
            semantic.push(SemanticRange {
                range: docstring.byte_range(),
                role: SourceComponentRole::Documentation,
            });
        }
        semantic.sort_by_key(|component| component.range.start);

        if semantic
            .iter()
            .any(|component| has_syntax_error_in_range(outer, &component.range))
        {
            self.omitted_named(
                "callable",
                &name,
                "its projected surface contains a syntax error",
            );
            return Ok(None);
        }
        let components = build_sparse_components(self.source, &semantic)?;
        let kind = callable_kind(&name, parent_id.is_some(), decorators, self.source);
        let type_use_sites = callable_type_uses(function, self.source)?;
        let key_discriminator = semantic_key_discriminator(
            self.source,
            semantic
                .iter()
                .filter(|component| component.role != SourceComponentRole::Documentation)
                .map(|component| component.range.clone()),
        )?;
        let id = self.push_declaration(
            outer,
            name,
            kind,
            parent_id,
            parent_name,
            components,
            &key_discriminator,
            type_use_sites,
        );
        Ok(Some(id))
    }

    fn add_class(
        &mut self,
        class: Node<'_>,
        decorators: &[Node<'_>],
        outer: Node<'_>,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<Option<DeclarationId>> {
        let Some(name_node) = class.child_by_field_name("name") else {
            self.omitted("class", class.start_byte(), "it has no declared name");
            return Ok(None);
        };
        let name = self.node_text(name_node, "Python class name")?.to_owned();
        let Some(header) = definition_header_range(class) else {
            self.omitted_named("class", &name, "its terminating colon is missing");
            return Ok(None);
        };

        let mut semantic = decorators
            .iter()
            .map(|decorator| SemanticRange {
                range: decorator.byte_range(),
                role: SourceComponentRole::Attribute,
            })
            .collect::<Vec<_>>();
        semantic.push(SemanticRange {
            range: header.clone(),
            role: SourceComponentRole::AggregateShape,
        });
        if let Some(docstring) = first_docstring(class, self.source) {
            semantic.push(SemanticRange {
                range: docstring.byte_range(),
                role: SourceComponentRole::Documentation,
            });
        }
        self.collect_class_shape(class, &name, &mut semantic);
        semantic.sort_by_key(|component| component.range.start);

        if has_syntax_error_in_range(class, &header) {
            self.omitted_named("class", &name, "its header contains a syntax error");
            return Ok(None);
        }
        semantic.retain(|component| {
            component.range == header || !has_syntax_error_in_range(class, &component.range)
        });
        let components = build_sparse_components(self.source, &semantic)?;
        let type_use_sites = class_type_uses(class, self.source, &semantic)?;
        let key_discriminator = semantic_key_discriminator(
            self.source,
            decorators
                .iter()
                .map(Node::byte_range)
                .chain(std::iter::once(header.clone())),
        )?;
        let class_id = self.push_declaration(
            outer,
            name.clone(),
            DeclarationKind::Class,
            parent_id,
            parent_name,
            components,
            &key_discriminator,
            type_use_sites,
        );
        let class_index = self.declarations.len() - 1;

        let qualified_name = parent_name.map_or_else(
            || name.clone(),
            |parent_name| format!("{parent_name}.{name}"),
        );
        let children = if let Some(body) = class.child_by_field_name("body") {
            self.collect_class_members(body, &class_id, &qualified_name)?
        } else {
            Vec::new()
        };
        self.declarations[class_index].children = children;
        Ok(Some(class_id))
    }

    fn collect_class_members(
        &mut self,
        scope: Node<'_>,
        class_id: &DeclarationId,
        class_name: &str,
    ) -> Result<Vec<DeclarationId>> {
        let mut children = Vec::new();
        let mut cursor = scope.walk();
        for child in scope.named_children(&mut cursor) {
            if let Some((definition, decorators, outer)) = unpack_definition(child) {
                let child_id = match definition.kind() {
                    "function_definition"
                        if is_signature_only_property(definition, &decorators, self.source) =>
                    {
                        None
                    }
                    "function_definition" => self.add_callable(
                        definition,
                        &decorators,
                        outer,
                        Some(class_id.clone()),
                        Some(class_name),
                    )?,
                    "class_definition" => self.add_class(
                        definition,
                        &decorators,
                        outer,
                        Some(class_id.clone()),
                        Some(class_name),
                    )?,
                    _ => None,
                };
                if let Some(child_id) = child_id {
                    children.push(child_id);
                }
            } else if child.kind() == "type_alias_statement" {
                if let Some(child_id) =
                    self.add_type_alias(child, Some(class_id.clone()), Some(class_name))?
                {
                    children.push(child_id);
                }
            } else if let Some(assignment) = statement_assignment(child)
                && explicit_assignment_kind(assignment, true, self.source).is_some()
            {
                if let Some(child_id) = self.add_assignment_declaration(
                    assignment,
                    child,
                    Some(class_id.clone()),
                    Some(class_name),
                )? {
                    children.push(child_id);
                }
            } else if is_scope_preserving_compound(child) {
                children.extend(self.collect_class_members(child, class_id, class_name)?);
            }
        }
        Ok(children)
    }

    fn collect_class_shape(
        &mut self,
        class: Node<'_>,
        class_name: &str,
        semantic: &mut Vec<SemanticRange>,
    ) {
        let Some(body) = class.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for statement in body.named_children(&mut cursor) {
            self.collect_class_shape_statement(statement, class_name, semantic);
        }
    }

    fn collect_class_shape_statement(
        &mut self,
        statement: Node<'_>,
        class_name: &str,
        semantic: &mut Vec<SemanticRange>,
    ) {
        if statement.kind() == "expression_statement" {
            if is_docstring_statement(statement, self.source) {
                return;
            }
            let Some(assignment) = statement_assignment(statement) else {
                return;
            };
            if explicit_assignment_kind(assignment, true, self.source).is_some() {
                return;
            }
            match class_attribute_range(assignment) {
                Some(range) => semantic.push(SemanticRange {
                    range,
                    role: SourceComponentRole::AggregateShape,
                }),
                None => self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted a computed Python class attribute in {class_name} at byte {} because the declaration model cannot represent its target exactly",
                    assignment.start_byte()
                ))),
            }
            return;
        }

        if let Some((definition, decorators, _)) = unpack_definition(statement) {
            if definition.kind() != "function_definition"
                || !is_signature_only_property(definition, &decorators, self.source)
            {
                return;
            }
            if let Some(range) = definition_header_range(definition) {
                semantic.extend(decorators.iter().map(|decorator| SemanticRange {
                    range: decorator.byte_range(),
                    role: SourceComponentRole::Attribute,
                }));
                semantic.push(SemanticRange {
                    range,
                    role: SourceComponentRole::AggregateShape,
                });
                if let Some(docstring) = first_docstring(definition, self.source) {
                    semantic.push(SemanticRange {
                        range: docstring.byte_range(),
                        role: SourceComponentRole::Documentation,
                    });
                }
            } else {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted a malformed abstract Python property in {class_name} at byte {}",
                    definition.start_byte()
                )));
            }
            return;
        }

        if is_scope_preserving_compound(statement) {
            let mut cursor = statement.walk();
            for child in statement.named_children(&mut cursor) {
                self.collect_class_shape_statement(child, class_name, semantic);
            }
        }
    }

    fn add_type_alias(
        &mut self,
        statement: Node<'_>,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<Option<DeclarationId>> {
        let Some(left) = statement.child_by_field_name("left") else {
            self.omitted(
                "type alias",
                statement.start_byte(),
                "it has no declared name",
            );
            return Ok(None);
        };
        let Some(name_node) = type_alias_name(left) else {
            self.omitted(
                "type alias",
                statement.start_byte(),
                "its parameterized name cannot be represented exactly",
            );
            return Ok(None);
        };
        let name = self
            .node_text(name_node, "Python type-alias name")?
            .to_owned();
        let Some(target) = statement.child_by_field_name("right") else {
            self.omitted_named("type alias", &name, "it has no target type");
            return Ok(None);
        };
        if has_syntax_error_in_range(statement, &statement.byte_range()) {
            self.omitted_named("type alias", &name, "its surface contains a syntax error");
            return Ok(None);
        }

        let mut components = Vec::new();
        push_component(
            self.source,
            &mut components,
            SourceComponentRole::TypeAlias,
            statement.byte_range(),
        )?;
        let mut type_use_sites = Vec::new();
        collect_type_names(
            target,
            self.source,
            TypeUseRole::AliasTarget,
            &mut type_use_sites,
        )?;
        collect_type_parameter_bounds(left, self.source, &mut type_use_sites)?;
        normalize_type_uses(&mut type_use_sites);
        let key_discriminator = semantic_key_discriminator(
            self.source,
            [left.byte_range(), target.byte_range()].into_iter(),
        )?;
        Ok(Some(self.push_declaration(
            statement,
            name,
            DeclarationKind::TypeAlias,
            parent_id,
            parent_name,
            components,
            &key_discriminator,
            type_use_sites,
        )))
    }

    fn add_assignment_declaration(
        &mut self,
        assignment: Node<'_>,
        statement: Node<'_>,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<Option<DeclarationId>> {
        let Some((assignment_kind, marker)) =
            explicit_assignment_marker(assignment, parent_id.is_some(), self.source)
        else {
            return Ok(None);
        };
        let Some(name_node) = assignment
            .child_by_field_name("left")
            .filter(|left| left.kind() == "identifier")
        else {
            self.omitted(
                "typed assignment",
                assignment.start_byte(),
                "its explicit typing marker has a non-identifier target",
            );
            return Ok(None);
        };
        let name = self
            .node_text(name_node, "Python typed-assignment name")?
            .to_owned();
        let Some(annotation) = assignment.child_by_field_name("type") else {
            self.omitted_named("typed assignment", &name, "its annotation is missing");
            return Ok(None);
        };
        let value = assignment.child_by_field_name("right");
        if assignment_kind == AssignmentDeclarationKind::TypeAlias && value.is_none() {
            self.omitted_named("type alias", &name, "its target type is missing");
            return Ok(None);
        }
        if has_syntax_error_in_range(statement, &statement.byte_range()) {
            self.omitted_named(
                "typed assignment",
                &name,
                "its surface contains a syntax error",
            );
            return Ok(None);
        }

        let (kind, role) = match assignment_kind {
            AssignmentDeclarationKind::Constant => {
                (DeclarationKind::Constant, SourceComponentRole::Value)
            }
            AssignmentDeclarationKind::Static => {
                (DeclarationKind::Static, SourceComponentRole::Value)
            }
            AssignmentDeclarationKind::TypeAlias => {
                (DeclarationKind::TypeAlias, SourceComponentRole::TypeAlias)
            }
        };
        let mut components = Vec::new();
        push_component(self.source, &mut components, role, statement.byte_range())?;

        let mut type_use_sites = Vec::new();
        if assignment_kind == AssignmentDeclarationKind::TypeAlias {
            collect_type_names(
                value.expect("a legacy type alias target was checked above"),
                self.source,
                TypeUseRole::AliasTarget,
                &mut type_use_sites,
            )?;
        } else {
            collect_type_names(
                annotation,
                self.source,
                TypeUseRole::Other,
                &mut type_use_sites,
            )?;
            type_use_sites.retain(|site| site.source_range != marker.byte_range());
        }
        normalize_type_uses(&mut type_use_sites);

        let key_ranges = if let Some(value) = value
            && assignment_kind == AssignmentDeclarationKind::TypeAlias
        {
            vec![annotation.byte_range(), value.byte_range()]
        } else {
            vec![annotation.byte_range()]
        };
        let key_discriminator = semantic_key_discriminator(self.source, key_ranges.into_iter())?;
        Ok(Some(self.push_declaration(
            statement,
            name,
            kind,
            parent_id,
            parent_name,
            components,
            &key_discriminator,
            type_use_sites,
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn push_declaration(
        &mut self,
        outer: Node<'_>,
        name: String,
        kind: DeclarationKind,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
        components: Vec<SourceComponent>,
        key_discriminator: &str,
        type_use_sites: Vec<TypeUseSite>,
    ) -> DeclarationId {
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let hash = projection_hash(Language::Python, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let source_start = components
            .first()
            .map_or(outer.start_byte(), |component| component.source_range.start);
        let source_end = components
            .last()
            .map_or(outer.end_byte(), |component| component.source_range.end);
        let source_span = source_start..source_end;
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_span.start,
            &hash,
        );
        let key = declaration_key(
            Language::Python,
            kind,
            &name,
            parent_name,
            key_discriminator,
        );
        let visibility = python_visibility(&name);
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
            projection_hash: hash,
            review_owner: id.clone(),
            children: Vec::new(),
            type_use_sites,
        });
        id
    }

    fn node_text<'a>(&'a self, node: Node<'_>, context: &'static str) -> Result<&'a str> {
        self.source.get(node.byte_range()).context(context)
    }

    fn omitted(&mut self, construct: &str, byte: usize, reason: &str) {
        self.diagnostics.push(ProjectionDiagnostic::new(format!(
            "omitted Python {construct} at byte {byte} because {reason}"
        )));
    }

    fn omitted_named(&mut self, construct: &str, name: &str, reason: &str) {
        self.diagnostics.push(ProjectionDiagnostic::new(format!(
            "omitted Python {construct} {name} because {reason}"
        )));
    }
}

fn unpack_definition(node: Node<'_>) -> Option<(Node<'_>, Vec<Node<'_>>, Node<'_>)> {
    if matches!(node.kind(), "function_definition" | "class_definition") {
        return Some((node, Vec::new(), node));
    }
    if node.kind() != "decorated_definition" {
        return None;
    }
    let definition = node.child_by_field_name("definition")?;
    let mut cursor = node.walk();
    let decorators = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "decorator")
        .collect();
    Some((definition, decorators, node))
}

fn statement_assignment(statement: Node<'_>) -> Option<Node<'_>> {
    (statement.kind() == "expression_statement")
        .then(|| statement.named_child(0))
        .flatten()
        .filter(|node| node.kind() == "assignment")
}

fn explicit_assignment_kind(
    assignment: Node<'_>,
    member: bool,
    source: &str,
) -> Option<AssignmentDeclarationKind> {
    explicit_assignment_marker(assignment, member, source).map(|(kind, _)| kind)
}

fn explicit_assignment_marker<'tree>(
    assignment: Node<'tree>,
    member: bool,
    source: &str,
) -> Option<(AssignmentDeclarationKind, Node<'tree>)> {
    let annotation = assignment.child_by_field_name("type")?;
    let marker = annotation_marker(annotation)?;
    let marker_name = source.get(marker.byte_range())?;
    let kind = match marker_name {
        "Final" => AssignmentDeclarationKind::Constant,
        "ClassVar" if member => AssignmentDeclarationKind::Static,
        "TypeAlias" => AssignmentDeclarationKind::TypeAlias,
        _ => return None,
    };
    Some((kind, marker))
}

fn annotation_marker(mut annotation: Node<'_>) -> Option<Node<'_>> {
    while annotation.kind() == "type" && annotation.named_child_count() == 1 {
        annotation = annotation.named_child(0)?;
    }
    if annotation.kind() == "generic_type" {
        annotation = annotation.named_child(0)?;
    }
    match annotation.kind() {
        "identifier" => Some(annotation),
        "attribute" | "member_type" | "dotted_name" => terminal_identifier(annotation),
        _ => None,
    }
}

fn type_alias_name(mut left: Node<'_>) -> Option<Node<'_>> {
    while left.kind() == "type" && left.named_child_count() == 1 {
        left = left.named_child(0)?;
    }
    if left.kind() == "generic_type" {
        left = left.named_child(0)?;
        while left.kind() == "type" && left.named_child_count() == 1 {
            left = left.named_child(0)?;
        }
    }
    (left.kind() == "identifier").then_some(left)
}

fn semantic_key_discriminator(
    source: &str,
    ranges: impl IntoIterator<Item = Range<usize>>,
) -> Result<String> {
    let mut discriminator = String::new();
    for range in ranges {
        let text = source
            .get(range)
            .context("Python semantic key surface was not on UTF-8 boundaries")?;
        let canonical = canonicalize_python_surface(text);
        discriminator.push_str(&canonical.len().to_string());
        discriminator.push(':');
        discriminator.push_str(&canonical);
    }
    Ok(discriminator)
}

fn canonicalize_python_surface(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut canonical = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut quote = None;
    let mut triple_quoted = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            canonical.push(byte);
            if triple_quoted {
                if byte == delimiter
                    && bytes.get(index + 1) == Some(&delimiter)
                    && bytes.get(index + 2) == Some(&delimiter)
                {
                    canonical.extend_from_slice(&bytes[index + 1..=index + 2]);
                    index += 3;
                    quote = None;
                    triple_quoted = false;
                    continue;
                }
            } else if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }

        if byte == b'#' {
            index += 1;
            while index < bytes.len() && !matches!(bytes[index], b'\r' | b'\n') {
                index += 1;
            }
            continue;
        }
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        canonical.push(byte);
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            if bytes.get(index + 1) == Some(&byte) && bytes.get(index + 2) == Some(&byte) {
                canonical.extend_from_slice(&bytes[index + 1..=index + 2]);
                index += 2;
                triple_quoted = true;
            }
        }
        index += 1;
    }

    String::from_utf8(canonical).expect("removing ASCII layout preserves UTF-8")
}

fn definition_header_range(definition: Node<'_>) -> Option<Range<usize>> {
    let body = definition.child_by_field_name("body")?;
    let mut colon = None;
    for index in 0..definition.child_count() {
        let child = definition.child(u32::try_from(index).ok()?)?;
        if child.kind() == ":" && child.end_byte() <= body.start_byte() {
            colon = Some(child);
        }
    }
    colon.map(|colon| definition.start_byte()..colon.end_byte())
}

fn first_docstring<'tree>(definition: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    let body = definition.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first_statement = body
        .named_children(&mut cursor)
        .find(|child| child.kind() != "comment")?;
    is_docstring_statement(first_statement, source).then_some(first_statement)
}

fn is_scope_preserving_compound(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "if_statement"
            | "elif_clause"
            | "else_clause"
            | "for_statement"
            | "while_statement"
            | "try_statement"
            | "except_clause"
            | "finally_clause"
            | "with_statement"
            | "match_statement"
            | "case_clause"
            | "block"
    )
}

fn is_docstring_statement(statement: Node<'_>, source: &str) -> bool {
    if statement.kind() != "expression_statement" || statement.named_child_count() != 1 {
        return false;
    }
    statement
        .named_child(0)
        .is_some_and(|expression| is_unicode_string_expression(expression, source))
}

fn is_unicode_string_expression(expression: Node<'_>, source: &str) -> bool {
    if expression.kind() == "parenthesized_expression" && expression.named_child_count() == 1 {
        return expression
            .named_child(0)
            .is_some_and(|inner| is_unicode_string_expression(inner, source));
    }
    if expression.kind() == "concatenated_string" {
        let mut cursor = expression.walk();
        let strings = expression.named_children(&mut cursor).collect::<Vec<_>>();
        return !strings.is_empty()
            && strings
                .into_iter()
                .all(|string| is_unicode_string_expression(string, source));
    }
    if expression.kind() != "string" {
        return false;
    }
    let Some(text) = source.get(expression.byte_range()) else {
        return false;
    };
    let prefix_end = text.find(['\'', '"']).unwrap_or(text.len());
    !text[..prefix_end]
        .bytes()
        .any(|byte| matches!(byte.to_ascii_lowercase(), b'b' | b'f'))
}

fn class_attribute_range(assignment: Node<'_>) -> Option<Range<usize>> {
    let left = assignment.child_by_field_name("left")?;
    if !is_attribute_target(left) {
        return None;
    }
    if let Some(annotation) = assignment.child_by_field_name("type") {
        return Some(assignment.start_byte()..annotation.end_byte());
    }

    let mut end = left.end_byte();
    let mut current = assignment;
    while let Some(right) = current
        .child_by_field_name("right")
        .filter(|right| right.kind() == "assignment")
    {
        let chained_left = right.child_by_field_name("left")?;
        if !is_attribute_target(chained_left) {
            return None;
        }
        end = chained_left.end_byte();
        current = right;
    }
    Some(assignment.start_byte()..end)
}

fn is_attribute_target(node: Node<'_>) -> bool {
    if node.kind() == "identifier" {
        return true;
    }
    if !matches!(
        node.kind(),
        "pattern_list" | "tuple_pattern" | "list_pattern"
    ) {
        return false;
    }
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    !children.is_empty() && children.into_iter().all(is_attribute_target)
}

fn is_signature_only_property(function: Node<'_>, decorators: &[Node<'_>], source: &str) -> bool {
    let property = decorators.iter().any(|decorator| {
        decorator_name(*decorator, source).is_some_and(is_property_decorator_name)
    });
    if !property {
        return false;
    }
    let explicitly_abstract = decorators.iter().any(|decorator| {
        decorator_name(*decorator, source).is_some_and(|name| {
            matches!(
                name.rsplit('.').next(),
                Some("abstractmethod" | "abstractproperty")
            )
        })
    });
    explicitly_abstract || signature_only_body(function, source)
}

fn decorator_name<'a>(decorator: Node<'_>, source: &'a str) -> Option<&'a str> {
    let expression = decorator.named_child(0)?;
    let expression = if expression.kind() == "call" {
        expression.child_by_field_name("function")?
    } else {
        expression
    };
    source.get(expression.byte_range()).map(str::trim)
}

fn signature_only_body(function: Node<'_>, source: &str) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    let docstring = first_docstring(function, source);
    let mut cursor = body.walk();
    let statements = body
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment" && Some(*child) != docstring)
        .collect::<Vec<_>>();
    if statements.len() != 1 {
        return false;
    }
    let statement = statements[0];
    statement.kind() == "pass_statement"
        || statement.kind() == "expression_statement"
            && source
                .get(statement.byte_range())
                .is_some_and(|text| text.trim() == "...")
}
fn callable_kind(
    name: &str,
    member: bool,
    decorators: &[Node<'_>],
    source: &str,
) -> DeclarationKind {
    if !member {
        return DeclarationKind::Function;
    }
    if decorators
        .iter()
        .any(|decorator| decorator_name(*decorator, source).is_some_and(is_property_decorator_name))
    {
        return DeclarationKind::Property;
    }
    match name {
        "__init__" | "__new__" => DeclarationKind::Constructor,
        "__del__" => DeclarationKind::Destructor,
        _ => DeclarationKind::Method,
    }
}

fn is_property_decorator_name(name: &str) -> bool {
    matches!(
        name.rsplit('.').next(),
        Some("property" | "cached_property" | "abstractproperty" | "setter" | "deleter" | "getter")
    )
}

fn python_visibility(name: &str) -> Visibility {
    if name.starts_with("__") && !name.ends_with("__") {
        Visibility::Private
    } else if name.starts_with('_') && !name.starts_with("__") {
        Visibility::Protected
    } else {
        Visibility::Public
    }
}

fn build_sparse_components(
    source: &str,
    semantic_ranges: &[SemanticRange],
) -> Result<Vec<SourceComponent>> {
    let mut components = Vec::new();
    let mut previous_end = None;
    for semantic in semantic_ranges {
        if let Some(end) = previous_end
            && end < semantic.range.start
            && let Some(layout) = sparse_layout_range(source, end..semantic.range.start)
        {
            push_component(source, &mut components, SourceComponentRole::Layout, layout)?;
        }
        push_component(
            source,
            &mut components,
            semantic.role,
            semantic.range.clone(),
        )?;
        previous_end = Some(semantic.range.end);
    }
    Ok(components)
}
fn sparse_layout_range(source: &str, gap: Range<usize>) -> Option<Range<usize>> {
    let text = source.get(gap.clone())?;
    if text.chars().all(char::is_whitespace) {
        return Some(gap);
    }

    let bytes = source.as_bytes();
    let separator_offset = bytes[gap.clone()]
        .iter()
        .rposition(|byte| matches!(*byte, b'\n' | b';'))?;
    let separator = gap.start + separator_offset;
    let after_separator = separator + 1;
    if !source
        .get(after_separator..gap.end)?
        .chars()
        .all(char::is_whitespace)
    {
        return None;
    }

    let start =
        if bytes[separator] == b'\n' && separator > gap.start && bytes[separator - 1] == b'\r' {
            separator - 1
        } else {
            separator
        };
    Some(start..gap.end)
}

fn push_component(
    source: &str,
    components: &mut Vec<SourceComponent>,
    role: SourceComponentRole,
    source_range: Range<usize>,
) -> Result<()> {
    let text = source
        .get(source_range.clone())
        .context("Python declaration component was not on UTF-8 boundaries")?;
    if !text.is_empty() {
        components.push(SourceComponent {
            role,
            source_range,
            text: text.to_owned(),
        });
    }
    Ok(())
}

fn callable_type_uses(function: Node<'_>, source: &str) -> Result<Vec<TypeUseSite>> {
    let mut sites = Vec::new();
    if let Some(parameters) = function.child_by_field_name("parameters") {
        collect_parameter_types(parameters, source, &mut sites)?;
    }
    if let Some(return_type) = function.child_by_field_name("return_type") {
        collect_type_names(return_type, source, TypeUseRole::Return, &mut sites)?;
    }
    if let Some(type_parameters) = function.child_by_field_name("type_parameters") {
        collect_type_parameter_bounds(type_parameters, source, &mut sites)?;
    }
    normalize_type_uses(&mut sites);
    Ok(sites)
}

fn class_type_uses(
    class: Node<'_>,
    source: &str,
    semantic: &[SemanticRange],
) -> Result<Vec<TypeUseSite>> {
    let mut sites = Vec::new();
    if let Some(superclasses) = class.child_by_field_name("superclasses") {
        collect_type_names(superclasses, source, TypeUseRole::Bound, &mut sites)?;
    }
    if let Some(type_parameters) = class.child_by_field_name("type_parameters") {
        collect_type_parameter_bounds(type_parameters, source, &mut sites)?;
    }

    if let Some(body) = class.child_by_field_name("body") {
        collect_class_owned_types(body, source, semantic, &mut sites)?;
    }
    normalize_type_uses(&mut sites);
    Ok(sites)
}

fn collect_class_owned_types(
    node: Node<'_>,
    source: &str,
    semantic: &[SemanticRange],
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    if node.kind() == "assignment" {
        if let Some(annotation) = node.child_by_field_name("type")
            && is_range_owned_by_aggregate(annotation.byte_range(), semantic)
        {
            collect_type_names(annotation, source, TypeUseRole::Field, sites)?;
        }
        return Ok(());
    }
    if node.kind() == "function_definition" {
        if definition_header_range(node).is_some_and(|range| {
            semantic.iter().any(|component| {
                component.role == SourceComponentRole::AggregateShape
                    && component.range.start <= range.start
                    && component.range.end >= range.end
            })
        }) {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                collect_parameter_types_with_role(parameters, source, TypeUseRole::Field, sites)?;
            }
            if let Some(return_type) = node.child_by_field_name("return_type") {
                collect_type_names(return_type, source, TypeUseRole::Field, sites)?;
            }
            if let Some(type_parameters) = node.child_by_field_name("type_parameters") {
                collect_type_parameter_bounds(type_parameters, source, sites)?;
            }
        }
        return Ok(());
    }
    if node.kind() == "class_definition" {
        return Ok(());
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_class_owned_types(child, source, semantic, sites)?;
    }
    Ok(())
}

fn is_range_owned_by_aggregate(range: Range<usize>, semantic: &[SemanticRange]) -> bool {
    semantic.iter().any(|component| {
        component.role == SourceComponentRole::AggregateShape
            && component.range.start <= range.start
            && component.range.end >= range.end
    })
}

fn collect_parameter_types(
    node: Node<'_>,
    source: &str,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    collect_parameter_types_with_role(node, source, TypeUseRole::Parameter, sites)
}

fn collect_parameter_types_with_role(
    node: Node<'_>,
    source: &str,
    role: TypeUseRole,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    if matches!(node.kind(), "typed_parameter" | "typed_default_parameter") {
        if let Some(annotation) = node.child_by_field_name("type") {
            collect_type_names(annotation, source, role, sites)?;
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_parameter_types_with_role(child, source, role, sites)?;
    }
    Ok(())
}

fn collect_type_parameter_bounds(
    node: Node<'_>,
    source: &str,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    if matches!(node.kind(), "constrained_type" | "assignment") {
        let mut cursor = node.walk();
        for bound in node.named_children(&mut cursor).skip(1) {
            collect_type_names(bound, source, TypeUseRole::Bound, sites)?;
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_parameter_bounds(child, source, sites)?;
    }
    Ok(())
}

fn collect_type_names(
    node: Node<'_>,
    source: &str,
    role: TypeUseRole,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    match node.kind() {
        "identifier" => {
            sites.push(TypeUseSite {
                name: source
                    .get(node.byte_range())
                    .context("Python type-use name was not on UTF-8 boundaries")?
                    .to_owned(),
                role,
                source_range: node.byte_range(),
            });
            return Ok(());
        }
        "attribute" | "member_type" | "dotted_name" => {
            if let Some(identifier) = terminal_identifier(node) {
                return collect_type_names(identifier, source, role, sites);
            }
        }
        "keyword_argument" => {
            if let Some(value) = node.child_by_field_name("value") {
                return collect_type_names(value, source, role, sites);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_names(child, source, role, sites)?;
    }
    Ok(())
}

fn normalize_type_uses(sites: &mut Vec<TypeUseSite>) {
    sites.sort_by_key(|site| (site.source_range.start, site.source_range.end));
    sites
        .dedup_by(|left, right| left.source_range == right.source_range && left.role == right.role);
}

fn terminal_identifier(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(attribute) = node.child_by_field_name("attribute") {
        return Some(attribute);
    }
    let mut cursor = node.walk();

    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "identifier")
        .last()
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
