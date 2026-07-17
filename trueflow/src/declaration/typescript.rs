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
    jsdoc: bool,
}

#[derive(Debug, Clone, Copy)]
struct TopLevelItem<'tree> {
    outer: Node<'tree>,
    declaration: Node<'tree>,
    kind: DeclarationKind,
}

#[derive(Debug, Clone)]
struct MemberCallable<'tree> {
    item: Node<'tree>,
    documentation: Vec<Range<usize>>,
    leading_start: usize,
}

struct Projector<'a> {
    path: &'a Path,
    source: &'a str,
    comments: Vec<CommentRange>,
    decorators: Vec<Range<usize>>,
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
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .context("failed to load the TypeScript grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a TypeScript syntax tree")?;

    let mut comments = Vec::new();
    let mut decorators = Vec::new();
    collect_syntax_ranges(tree.root_node(), source, &mut comments, &mut decorators);
    let mut projector = Projector {
        path,
        source,
        comments,
        decorators,
        next_ordinal: 0,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
    };
    projector.collect_program(tree.root_node())?;
    if tree.root_node().has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "TypeScript source contains syntax errors; declarations with errors in projected surfaces were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect_program(&mut self, program: Node<'_>) -> Result<()> {
        self.collect_scope(program, None, None)
    }

    fn collect_scope(
        &mut self,
        scope: Node<'_>,
        parent_id: Option<DeclarationId>,
        parent_lineage: Option<&str>,
    ) -> Result<()> {
        let mut pending_documentation = Vec::new();
        let mut cursor = scope.walk();
        for child in scope.named_children(&mut cursor) {
            if child.kind() == "comment" {
                if is_jsdoc(child, self.source) {
                    pending_documentation.push(child.byte_range());
                } else {
                    pending_documentation.clear();
                }
                continue;
            }

            let Some(item) = top_level_item(child) else {
                pending_documentation.clear();
                continue;
            };
            let documentation = std::mem::take(&mut pending_documentation);
            match item.kind {
                DeclarationKind::Function => {
                    self.add_callable(
                        item.outer,
                        item.declaration,
                        item.kind,
                        &documentation,
                        None,
                        parent_id.clone(),
                        parent_lineage,
                    )?;
                }
                DeclarationKind::Class | DeclarationKind::Interface => {
                    self.add_class_like(item, &documentation, parent_id.clone(), parent_lineage)?;
                }
                DeclarationKind::TypeAlias
                    if item
                        .declaration
                        .child_by_field_name("value")
                        .is_some_and(|value| value.kind() == "object_type") =>
                {
                    self.add_class_like(item, &documentation, parent_id.clone(), parent_lineage)?;
                }
                DeclarationKind::TypeAlias | DeclarationKind::Enum => {
                    self.add_contiguous_aggregate(
                        item,
                        &documentation,
                        parent_id.clone(),
                        parent_lineage,
                    )?;
                }
                DeclarationKind::Module => {
                    self.add_module(item, &documentation, parent_id.clone(), parent_lineage)?;
                }
                DeclarationKind::Constant | DeclarationKind::Static => {
                    self.add_value(item, &documentation, parent_id.clone(), parent_lineage)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn add_class_like(
        &mut self,
        item: TopLevelItem<'_>,
        documentation: &[Range<usize>],
        parent_id: Option<DeclarationId>,
        parent_lineage: Option<&str>,
    ) -> Result<()> {
        let body = if item.kind == DeclarationKind::TypeAlias {
            item.declaration.child_by_field_name("value")
        } else {
            item.declaration.child_by_field_name("body")
        };
        let Some(body) = body else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} at byte {} because it has no aggregate body",
                item.kind,
                item.declaration.start_byte()
            )));
            return Ok(());
        };
        let Some(name_node) = item.declaration.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} at byte {} because it has no declared name",
                item.kind,
                item.declaration.start_byte()
            )));
            return Ok(());
        };
        let name = node_text(name_node, self.source)
            .context("TypeScript aggregate name was not on UTF-8 boundaries")?
            .to_owned();

        let aggregate_role = if item.kind == DeclarationKind::TypeAlias {
            SourceComponentRole::TypeAlias
        } else {
            SourceComponentRole::AggregateShape
        };
        let mut includes = Vec::<Range<usize>>::new();
        let mut semantics = Vec::<SemanticRange>::new();
        let surface_start = documentation
            .first()
            .map_or(item.outer.start_byte(), |range| range.start);
        let header_end = opening_delimiter_end(body, self.source).unwrap_or(body.start_byte());
        includes.push(surface_start..header_end);
        semantics.push(SemanticRange {
            range: item.outer.start_byte()..header_end,
            role: aggregate_role,
        });
        add_documentation_semantics(&mut semantics, documentation);

        let mut callables = Vec::<MemberCallable<'_>>::new();
        let mut pending_documentation = Vec::new();
        let mut pending_decorators = Vec::<Range<usize>>::new();
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            if member.kind() == "comment" {
                if is_jsdoc(member, self.source) {
                    pending_documentation.push(member.byte_range());
                } else {
                    pending_documentation.clear();
                    pending_decorators.clear();
                }
                continue;
            }
            if member.kind() == "decorator" {
                pending_decorators.push(member.byte_range());
                continue;
            }

            let member_documentation = std::mem::take(&mut pending_documentation);
            let member_decorators = std::mem::take(&mut pending_decorators);
            let leading_start = member_documentation
                .iter()
                .chain(&member_decorators)
                .map(|range| range.start)
                .min()
                .unwrap_or(member.start_byte());
            if is_callable_member(member) {
                callables.push(MemberCallable {
                    item: member,
                    documentation: member_documentation,
                    leading_start,
                });
                continue;
            }

            if member.kind() == "call_signature" {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "TypeScript call signature at byte {} remains aggregate-owned because it has no stable declared name for an independent callable target",
                    member.start_byte()
                )));
            }

            match member.kind() {
                "property_signature" | "index_signature" | "call_signature" => {
                    let include_start = leading_whitespace_start(self.source, leading_start);
                    includes.push(include_start..member.end_byte());
                    semantics.push(SemanticRange {
                        range: member.byte_range(),
                        role: aggregate_role,
                    });
                    add_documentation_semantics(&mut semantics, &member_documentation);
                    if let Some(terminator) = declaration_terminator(member, self.source) {
                        includes.push(terminator.clone());
                        semantics.push(SemanticRange {
                            range: terminator,
                            role: SourceComponentRole::Terminator,
                        });
                    }
                }
                "public_field_definition" => {
                    let include_start = leading_whitespace_start(self.source, leading_start);
                    let signature_end = field_signature_end(member, self.source);
                    includes.push(include_start..signature_end);
                    semantics.push(SemanticRange {
                        range: member.start_byte()..signature_end,
                        role: aggregate_role,
                    });
                    add_documentation_semantics(&mut semantics, &member_documentation);
                    if let Some(terminator) = declaration_terminator(member, self.source) {
                        includes.push(terminator.clone());
                        semantics.push(SemanticRange {
                            range: terminator,
                            role: SourceComponentRole::Terminator,
                        });
                    }
                }
                "class_static_block" => {
                    self.diagnostics.push(ProjectionDiagnostic::new(format!(
                        "TypeScript class static block at byte {} is intentionally excluded because executable bodies are not declaration review surfaces",
                        member.start_byte()
                    )));
                }
                _ => {}
            }
        }

        let close_start = body.end_byte().saturating_sub(1);
        if close_start >= header_end {
            let include_start = leading_whitespace_start(self.source, close_start);
            includes.push(include_start..item.outer.end_byte());
            semantics.push(SemanticRange {
                range: close_start..item.outer.end_byte(),
                role: aggregate_role,
            });
        }
        normalize_ranges(&mut includes);
        if has_syntax_error_in_ranges(item.outer, &includes) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} {name} because its projected surface contains a syntax error",
                item.kind
            )));
            return Ok(());
        }

        let components = build_components(
            self.source,
            &includes,
            &semantics,
            &self.comments,
            &self.decorators,
        )?;
        let overload_discriminator = key_discriminator(item.outer, &components, self.source);
        let source_start = surface_start;
        let parent_index = self.declarations.len();
        let id = self.push_declaration(
            item.outer,
            name.clone(),
            name_node.byte_range(),
            item.kind,
            top_level_visibility(item.outer),
            parent_id,
            parent_lineage,
            source_start..item.outer.end_byte(),
            components,
            &overload_discriminator,
        )?;

        let lineage = declaration_lineage(parent_lineage, item.kind, &name);
        let children_start = self.declarations.len();
        for callable in callables {
            let callable_kind = callable_kind(callable.item, self.source);
            self.add_callable(
                callable.item,
                callable.item,
                callable_kind,
                &callable.documentation,
                Some(callable.leading_start),
                Some(id.clone()),
                Some(&lineage),
            )?;
        }
        self.declarations[parent_index].children = self.declarations[children_start..]
            .iter()
            .filter(|declaration| declaration.parent_part.as_ref() == Some(&id))
            .map(|declaration| declaration.id.clone())
            .collect();
        Ok(())
    }

    fn add_contiguous_aggregate(
        &mut self,
        item: TopLevelItem<'_>,
        documentation: &[Range<usize>],
        parent_id: Option<DeclarationId>,
        parent_lineage: Option<&str>,
    ) -> Result<()> {
        let Some(name_node) = item.declaration.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} at byte {} because it has no declared name",
                item.kind,
                item.declaration.start_byte()
            )));
            return Ok(());
        };
        let name = node_text(name_node, self.source)
            .context("TypeScript aggregate name was not on UTF-8 boundaries")?
            .to_owned();
        let surface_start = documentation
            .first()
            .map_or(item.outer.start_byte(), |range| range.start);
        let include = surface_start..item.outer.end_byte();
        let includes = std::slice::from_ref(&include);
        if has_syntax_error_in_ranges(item.outer, includes) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} {name} because its projected surface contains a syntax error",
                item.kind
            )));
            return Ok(());
        }
        let role = if item.kind == DeclarationKind::TypeAlias {
            SourceComponentRole::TypeAlias
        } else {
            SourceComponentRole::AggregateShape
        };
        let mut semantics = vec![SemanticRange {
            range: item.outer.byte_range(),
            role,
        }];
        add_documentation_semantics(&mut semantics, documentation);
        semantics.extend(
            self.comments
                .iter()
                .filter(|comment| {
                    comment.jsdoc && contains_range(&item.outer.byte_range(), &comment.range)
                })
                .map(|comment| SemanticRange {
                    range: comment.range.clone(),
                    role: SourceComponentRole::Documentation,
                }),
        );
        let components = build_components(
            self.source,
            includes,
            &semantics,
            &self.comments,
            &self.decorators,
        )?;
        let overload_discriminator = key_discriminator(item.outer, &components, self.source);
        self.push_declaration(
            item.outer,
            name,
            name_node.byte_range(),
            item.kind,
            top_level_visibility(item.outer),
            parent_id,
            parent_lineage,
            surface_start..item.outer.end_byte(),
            components,
            &overload_discriminator,
        )?;
        Ok(())
    }

    fn add_module(
        &mut self,
        item: TopLevelItem<'_>,
        documentation: &[Range<usize>],
        parent_id: Option<DeclarationId>,
        parent_lineage: Option<&str>,
    ) -> Result<()> {
        let name_node = item
            .declaration
            .child_by_field_name("name")
            .or_else(|| direct_child_by_kind(item.declaration, "global"));
        let Some(name_node) = name_node else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript Module at byte {} because it has no declared name",
                item.declaration.start_byte()
            )));
            return Ok(());
        };
        let name = node_text(name_node, self.source)
            .context("TypeScript module name was not on UTF-8 boundaries")?
            .to_owned();
        let surface_start = documentation
            .first()
            .map_or(item.outer.start_byte(), |range| range.start);
        let mut includes = Vec::new();
        let mut semantics = Vec::new();
        add_documentation_semantics(&mut semantics, documentation);
        let body = item
            .declaration
            .child_by_field_name("body")
            .or_else(|| direct_child_by_kind(item.declaration, "statement_block"));
        if let Some(body) = body {
            let header_end = opening_delimiter_end(body, self.source).unwrap_or(body.start_byte());
            includes.push(surface_start..header_end);
            semantics.push(SemanticRange {
                range: item.outer.start_byte()..header_end,
                role: SourceComponentRole::AggregateShape,
            });
            let close_start = body.end_byte().saturating_sub(1);
            if close_start >= header_end {
                includes.push(
                    leading_whitespace_start(self.source, close_start)..item.outer.end_byte(),
                );
                semantics.push(SemanticRange {
                    range: close_start..item.outer.end_byte(),
                    role: SourceComponentRole::AggregateShape,
                });
            }
        } else {
            includes.push(surface_start..item.outer.end_byte());
            semantics.push(SemanticRange {
                range: item.outer.byte_range(),
                role: SourceComponentRole::AggregateShape,
            });
        }
        normalize_ranges(&mut includes);
        if has_syntax_error_in_ranges(item.outer, &includes) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript Module {name} because its projected surface contains a syntax error"
            )));
            return Ok(());
        }
        let components = build_components(
            self.source,
            &includes,
            &semantics,
            &self.comments,
            &self.decorators,
        )?;
        let discriminator = key_discriminator(item.outer, &components, self.source);
        let parent_index = self.declarations.len();
        let id = self.push_declaration(
            item.outer,
            name.clone(),
            name_node.byte_range(),
            DeclarationKind::Module,
            top_level_visibility(item.outer),
            parent_id,
            parent_lineage,
            surface_start..item.outer.end_byte(),
            components,
            &discriminator,
        )?;
        if let Some(body) = body {
            let lineage = declaration_lineage(parent_lineage, DeclarationKind::Module, &name);
            let children_start = self.declarations.len();
            self.collect_scope(body, Some(id.clone()), Some(&lineage))?;
            self.declarations[parent_index].children = self.declarations[children_start..]
                .iter()
                .filter(|declaration| declaration.parent_part.as_ref() == Some(&id))
                .map(|declaration| declaration.id.clone())
                .collect();
        }
        Ok(())
    }

    fn add_value(
        &mut self,
        item: TopLevelItem<'_>,
        documentation: &[Range<usize>],
        parent_id: Option<DeclarationId>,
        parent_lineage: Option<&str>,
    ) -> Result<()> {
        let mut cursor = item.declaration.walk();
        let declarators = item
            .declaration
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .collect::<Vec<_>>();
        let [declarator] = declarators.as_slice() else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} declaration at byte {} because a multi-declarator statement has no exclusive standalone source surface",
                item.kind,
                item.declaration.start_byte()
            )));
            return Ok(());
        };
        let Some(name_node) = declarator.child_by_field_name("name") else {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} at byte {} because it has no declared name",
                item.kind,
                declarator.start_byte()
            )));
            return Ok(());
        };
        if name_node.kind() != "identifier" {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} at byte {} because destructuring has no single lossless declaration identity",
                item.kind,
                declarator.start_byte()
            )));
            return Ok(());
        }
        let name = node_text(name_node, self.source)
            .context("TypeScript value name was not on UTF-8 boundaries")?
            .to_owned();
        let surface_start = documentation
            .first()
            .map_or(item.outer.start_byte(), |range| range.start);
        let include = surface_start..item.outer.end_byte();
        let includes = std::slice::from_ref(&include);
        if has_syntax_error_in_ranges(item.outer, includes) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {:?} {name} because its projected surface contains a syntax error",
                item.kind
            )));
            return Ok(());
        }
        let mut semantics = vec![SemanticRange {
            range: item.outer.byte_range(),
            role: SourceComponentRole::Value,
        }];
        add_documentation_semantics(&mut semantics, documentation);
        let components = build_components(
            self.source,
            includes,
            &semantics,
            &self.comments,
            &self.decorators,
        )?;
        let discriminator = value_key_discriminator(item.outer, *declarator, self.source);
        self.push_declaration(
            item.outer,
            name,
            name_node.byte_range(),
            item.kind,
            top_level_visibility(item.outer),
            parent_id,
            parent_lineage,
            surface_start..item.outer.end_byte(),
            components,
            &discriminator,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_callable(
        &mut self,
        outer: Node<'_>,
        declaration: Node<'_>,
        kind: DeclarationKind,
        documentation: &[Range<usize>],
        leading_start: Option<usize>,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
    ) -> Result<()> {
        let name_node = declaration.child_by_field_name("name");
        let (name, name_range) = if kind == DeclarationKind::Constructor {
            if let Some(name_node) = name_node {
                (
                    node_text(name_node, self.source)
                        .context("TypeScript constructor name was not on UTF-8 boundaries")?
                        .to_owned(),
                    name_node.byte_range(),
                )
            } else {
                (
                    "constructor".to_owned(),
                    declaration.start_byte()..declaration.start_byte(),
                )
            }
        } else {
            let Some(name_node) = name_node else {
                self.diagnostics.push(ProjectionDiagnostic::new(format!(
                    "omitted TypeScript {kind:?} at byte {} because it has no declared name",
                    declaration.start_byte()
                )));
                return Ok(());
            };
            (
                node_text(name_node, self.source)
                    .context("TypeScript callable name was not on UTF-8 boundaries")?
                    .to_owned(),
                name_node.byte_range(),
            )
        };

        let signature_end = callable_signature_end(declaration, self.source);
        if signature_end <= outer.start_byte() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {kind:?} {name} because its projected surface is incomplete"
            )));
            return Ok(());
        }
        let surface_start = leading_start
            .into_iter()
            .chain(documentation.first().map(|range| range.start))
            .min()
            .unwrap_or(outer.start_byte());
        let include = surface_start..signature_end;
        let includes = std::slice::from_ref(&include);
        if has_syntax_error_in_ranges(outer, includes) {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {kind:?} {name} because its projected surface contains a syntax error"
            )));
            return Ok(());
        }
        let mut semantics = vec![SemanticRange {
            range: outer.start_byte()..signature_end,
            role: SourceComponentRole::Signature,
        }];
        add_documentation_semantics(&mut semantics, documentation);
        let components = build_components(
            self.source,
            includes,
            &semantics,
            &self.comments,
            &self.decorators,
        )?;
        let overload_discriminator = key_discriminator(outer, &components, self.source);
        let visibility = if parent_id.is_some() {
            member_visibility(declaration, self.source)
        } else {
            top_level_visibility(outer)
        };
        let source_span = surface_start..outer.end_byte().max(signature_end);
        self.push_declaration(
            outer,
            name,
            name_range,
            kind,
            visibility,
            parent_id,
            parent_name,
            source_span,
            components,
            &overload_discriminator,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_declaration(
        &mut self,
        syntax: Node<'_>,
        name: String,
        name_range: Range<usize>,
        kind: DeclarationKind,
        visibility: Visibility,
        parent_id: Option<DeclarationId>,
        parent_name: Option<&str>,
        source_span: Range<usize>,
        components: Vec<SourceComponent>,
        overload_discriminator: &str,
    ) -> Result<DeclarationId> {
        if components.is_empty() {
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted TypeScript {kind:?} {name} because it has no projectable source components"
            )));
            anyhow::bail!("TypeScript declaration unexpectedly had no projectable components");
        }
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let hash = projection_hash(Language::TypeScript, kind, &components);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_span.start,
            &hash,
        );
        let key = declaration_key(
            Language::TypeScript,
            kind,
            &name,
            parent_name,
            overload_discriminator,
        );
        let type_use_sites =
            collect_type_use_sites(syntax, self.source, &components, &name_range, kind)?;
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
        Ok(id)
    }
}

fn top_level_item(node: Node<'_>) -> Option<TopLevelItem<'_>> {
    let outer = node;
    let mut declaration = if node.kind() == "export_statement" {
        node.child_by_field_name("declaration")?
    } else {
        node
    };
    if declaration.kind() == "ambient_declaration"
        && direct_child_by_kind(declaration, "global").is_none()
    {
        let mut cursor = declaration.walk();
        declaration = declaration.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "function_signature"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "internal_module"
                    | "module"
                    | "lexical_declaration"
                    | "variable_declaration"
            )
        })?;
    }
    let kind = match declaration.kind() {
        "function_declaration" | "function_signature" => DeclarationKind::Function,
        "class_declaration" | "abstract_class_declaration" => DeclarationKind::Class,
        "interface_declaration" => DeclarationKind::Interface,
        "type_alias_declaration" => DeclarationKind::TypeAlias,
        "enum_declaration" => DeclarationKind::Enum,
        "internal_module" | "module" | "ambient_declaration" => DeclarationKind::Module,
        "lexical_declaration" => {
            if declaration
                .child_by_field_name("kind")
                .is_some_and(|kind| kind.kind() == "const")
            {
                DeclarationKind::Constant
            } else {
                DeclarationKind::Static
            }
        }
        "variable_declaration" => DeclarationKind::Static,
        _ => return None,
    };
    Some(TopLevelItem {
        outer,
        declaration,
        kind,
    })
}

fn direct_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let child = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    child
}

fn is_callable_member(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "method_definition"
            | "method_signature"
            | "abstract_method_signature"
            | "construct_signature"
    )
}

fn callable_kind(node: Node<'_>, source: &str) -> DeclarationKind {
    if node.kind() == "construct_signature"
        || node
            .child_by_field_name("name")
            .and_then(|name| node_text(name, source))
            .is_some_and(|name| name == "constructor")
    {
        return DeclarationKind::Constructor;
    }
    let mut cursor = node.walk();
    if node
        .children(&mut cursor)
        .any(|child| matches!(child.kind(), "get" | "set"))
    {
        DeclarationKind::Property
    } else {
        DeclarationKind::Method
    }
}

fn callable_signature_end(declaration: Node<'_>, source: &str) -> usize {
    let body = declaration.child_by_field_name("body");
    let mut end = body.map_or(declaration.end_byte(), |body| body.start_byte());
    while end > declaration.start_byte()
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    if body.is_none()
        && let Some(terminator) = declaration_terminator(declaration, source)
    {
        end = terminator.end;
    }
    end
}

fn field_signature_end(field: Node<'_>, source: &str) -> usize {
    let Some(value) = field.child_by_field_name("value") else {
        return field.end_byte();
    };
    let bytes = source.as_bytes();
    let mut end = value.start_byte();
    while end > field.start_byte() && bytes.get(end - 1).is_some_and(u8::is_ascii_whitespace) {
        end -= 1;
    }
    if bytes.get(end - 1) == Some(&b'=') {
        end -= 1;
        while end > field.start_byte() && bytes.get(end - 1).is_some_and(u8::is_ascii_whitespace) {
            end -= 1;
        }
    }
    end
}

fn declaration_terminator(declaration: Node<'_>, source: &str) -> Option<Range<usize>> {
    let bytes = source.as_bytes();
    let mut start = declaration.end_byte();
    while bytes
        .get(start)
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        start += 1;
    }
    (bytes.get(start) == Some(&b';')).then_some(start..start + 1)
}

fn opening_delimiter_end(body: Node<'_>, source: &str) -> Option<usize> {
    source
        .get(body.byte_range())?
        .find('{')
        .map(|offset| body.start_byte() + offset + 1)
}

fn leading_whitespace_start(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start;
    while cursor > 0 && bytes.get(cursor - 1).is_some_and(u8::is_ascii_whitespace) {
        cursor -= 1;
    }
    cursor
}

fn top_level_visibility(outer: Node<'_>) -> Visibility {
    if outer.kind() == "export_statement" {
        Visibility::Public
    } else {
        Visibility::Private
    }
}

fn member_visibility(member: Node<'_>, source: &str) -> Visibility {
    if member
        .child_by_field_name("name")
        .and_then(|name| node_text(name, source))
        .is_some_and(|name| name.starts_with('#'))
    {
        return Visibility::Private;
    }
    let mut cursor = member.walk();
    let modifier = member
        .named_children(&mut cursor)
        .find(|child| child.kind() == "accessibility_modifier")
        .and_then(|node| node_text(node, source));
    match modifier.map(str::trim) {
        Some("public") => Visibility::Public,
        Some("protected") => Visibility::Protected,
        Some("private") => Visibility::Private,
        Some(other) => Visibility::Restricted(other.to_owned()),
        None => Visibility::Implicit,
    }
}

fn add_documentation_semantics(semantics: &mut Vec<SemanticRange>, documentation: &[Range<usize>]) {
    semantics.extend(documentation.iter().cloned().map(|range| SemanticRange {
        range,
        role: SourceComponentRole::Documentation,
    }));
}

fn normalize_ranges(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized = Vec::<Range<usize>>::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if range.is_empty() {
            continue;
        }
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

fn key_discriminator(syntax: Node<'_>, components: &[SourceComponent], source: &str) -> String {
    let identity_ranges = components
        .iter()
        .filter(|component| {
            !matches!(
                component.role,
                SourceComponentRole::Documentation
                    | SourceComponentRole::Attribute
                    | SourceComponentRole::Layout
            )
        })
        .map(|component| component.source_range.clone())
        .collect::<Vec<_>>();
    let mut discriminator = String::new();
    collect_identity_tokens(syntax, &identity_ranges, source, &mut discriminator);
    discriminator
}

fn collect_identity_tokens(
    node: Node<'_>,
    identity_ranges: &[Range<usize>],
    source: &str,
    discriminator: &mut String,
) {
    let node_range = node.byte_range();
    if node.kind() == "comment"
        || !identity_ranges
            .iter()
            .any(|range| ranges_overlap(range, &node_range))
    {
        return;
    }
    if node.child_count() == 0 {
        if identity_ranges
            .iter()
            .any(|range| contains_range(range, &node_range))
            && let Some(text) = node_text(node, source)
        {
            discriminator.push_str(&text.len().to_string());
            discriminator.push(':');
            discriminator.push_str(text);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identity_tokens(child, identity_ranges, source, discriminator);
    }
}

fn declaration_lineage(parent_lineage: Option<&str>, kind: DeclarationKind, name: &str) -> String {
    let parent = parent_lineage.unwrap_or_default();
    format!(
        "{}:{}{}:{}{}:{}",
        parent.len(),
        parent,
        kind.protocol_tag().len(),
        kind.protocol_tag(),
        name.len(),
        name
    )
}

fn value_key_discriminator(outer: Node<'_>, declarator: Node<'_>, source: &str) -> String {
    let mut identity_end = declarator.end_byte();
    if let Some(value) = declarator.child_by_field_name("value") {
        identity_end = value.start_byte();
        let bytes = source.as_bytes();
        while identity_end > declarator.start_byte()
            && bytes
                .get(identity_end - 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            identity_end -= 1;
        }
        if bytes.get(identity_end - 1) == Some(&b'=') {
            identity_end -= 1;
        }
    }
    let identity_ranges = [outer.start_byte()..identity_end];
    let mut discriminator = String::new();
    collect_identity_tokens(outer, &identity_ranges, source, &mut discriminator);
    discriminator
}

fn build_components(
    source: &str,
    includes: &[Range<usize>],
    semantics: &[SemanticRange],
    comments: &[CommentRange],
    decorators: &[Range<usize>],
) -> Result<Vec<SourceComponent>> {
    let mut boundaries = Vec::new();
    for include in includes {
        boundaries.push(include.start);
        boundaries.push(include.end);
        for semantic in semantics {
            if ranges_overlap(include, &semantic.range) {
                boundaries.push(semantic.range.start.max(include.start));
                boundaries.push(semantic.range.end.min(include.end));
            }
        }
        for comment in comments {
            if ranges_overlap(include, &comment.range) {
                boundaries.push(comment.range.start.max(include.start));
                boundaries.push(comment.range.end.min(include.end));
            }
        }
        for decorator in decorators {
            if ranges_overlap(include, decorator) {
                boundaries.push(decorator.start.max(include.start));
                boundaries.push(decorator.end.min(include.end));
            }
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut components = Vec::new();
    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        if range.is_empty()
            || !includes
                .iter()
                .any(|include| contains_range(include, &range))
        {
            continue;
        }
        let selected_doc = semantics.iter().any(|semantic| {
            semantic.role == SourceComponentRole::Documentation
                && contains_range(&semantic.range, &range)
        });
        if comments.iter().any(|comment| {
            contains_range(&comment.range, &range) && !(comment.jsdoc && selected_doc)
        }) {
            continue;
        }

        let role = if selected_doc {
            SourceComponentRole::Documentation
        } else if decorators
            .iter()
            .any(|decorator| contains_range(decorator, &range))
        {
            SourceComponentRole::Attribute
        } else {
            semantics
                .iter()
                .find(|semantic| contains_range(&semantic.range, &range))
                .map_or(SourceComponentRole::Layout, |semantic| semantic.role)
        };
        let text = source
            .get(range.clone())
            .context("TypeScript declaration component was not on UTF-8 boundaries")?
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

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn contains_range(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && outer.end >= inner.end
}

fn collect_syntax_ranges(
    node: Node<'_>,
    source: &str,
    comments: &mut Vec<CommentRange>,
    decorators: &mut Vec<Range<usize>>,
) {
    match node.kind() {
        "comment" => {
            comments.push(CommentRange {
                range: node.byte_range(),
                jsdoc: is_jsdoc(node, source),
            });
            return;
        }
        "decorator" => decorators.push(node.byte_range()),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_syntax_ranges(child, source, comments, decorators);
    }
}

fn is_jsdoc(node: Node<'_>, source: &str) -> bool {
    let text = node_text(node, source).unwrap_or_default().trim_start();
    text.starts_with("/**") && !text.starts_with("/***")
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}

fn has_syntax_error_in_ranges(node: Node<'_>, ranges: &[Range<usize>]) -> bool {
    if (node.is_error() || node.is_missing())
        && ranges
            .iter()
            .any(|range| node.start_byte() < range.end && node.end_byte() >= range.start)
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| has_syntax_error_in_ranges(child, ranges))
}

fn collect_type_use_sites(
    syntax: Node<'_>,
    source: &str,
    components: &[SourceComponent],
    declaration_name: &Range<usize>,
    declaration_kind: DeclarationKind,
) -> Result<Vec<TypeUseSite>> {
    let mut ignored_names = vec![declaration_name.clone()];
    collect_type_parameter_names(syntax, &mut ignored_names);
    let mut sites = Vec::new();
    collect_type_identifiers(
        syntax,
        syntax,
        source,
        components,
        &ignored_names,
        declaration_kind,
        &mut sites,
    )?;
    sites.sort_by_key(|site| (site.source_range.start, site.source_range.end));
    sites.dedup_by(|left, right| left.source_range == right.source_range);
    Ok(sites)
}

fn collect_type_parameter_names(node: Node<'_>, names: &mut Vec<Range<usize>>) {
    if node.kind() == "type_parameter"
        && let Some(name) = node.child_by_field_name("name")
    {
        names.push(name.byte_range());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_parameter_names(child, names);
    }
}

fn collect_type_identifiers(
    node: Node<'_>,
    declaration: Node<'_>,
    source: &str,
    components: &[SourceComponent],
    ignored_names: &[Range<usize>],
    declaration_kind: DeclarationKind,
    sites: &mut Vec<TypeUseSite>,
) -> Result<()> {
    let range = node.byte_range();
    if node.kind() == "type_identifier"
        && !ignored_names.contains(&range)
        && components
            .iter()
            .any(|component| contains_range(&component.source_range, &range))
    {
        sites.push(TypeUseSite {
            name: node_text(node, source)
                .context("TypeScript type-use site was not on UTF-8 boundaries")?
                .to_owned(),
            role: type_use_role(node, declaration, declaration_kind),
            source_range: range,
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_identifiers(
            child,
            declaration,
            source,
            components,
            ignored_names,
            declaration_kind,
            sites,
        )?;
    }
    Ok(())
}

fn type_use_role(
    node: Node<'_>,
    declaration: Node<'_>,
    declaration_kind: DeclarationKind,
) -> TypeUseRole {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        match current.kind() {
            "constraint"
            | "default_type"
            | "extends_clause"
            | "extends_type_clause"
            | "implements_clause" => return TypeUseRole::Bound,
            "required_parameter" | "optional_parameter" | "rest_pattern" => {
                return TypeUseRole::Parameter;
            }
            "type_annotation" => {
                if let Some(parent) = current.parent()
                    && is_return_annotation(parent, current)
                {
                    return TypeUseRole::Return;
                }
            }
            "public_field_definition" | "property_signature" | "index_signature" => {
                return TypeUseRole::Field;
            }
            "type_alias_declaration" => return TypeUseRole::AliasTarget,
            _ => {}
        }
        if current.id() == declaration.id() {
            break;
        }
        ancestor = current.parent();
    }
    if declaration_kind == DeclarationKind::TypeAlias {
        TypeUseRole::AliasTarget
    } else {
        TypeUseRole::Other
    }
}

fn is_return_annotation(parent: Node<'_>, annotation: Node<'_>) -> bool {
    parent
        .child_by_field_name("return_type")
        .is_some_and(|candidate| candidate.id() == annotation.id())
        || (parent.kind() == "construct_signature"
            && parent
                .child_by_field_name("type")
                .is_some_and(|candidate| candidate.id() == annotation.id()))
}
