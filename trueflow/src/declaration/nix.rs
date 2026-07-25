use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::analysis::Language;

use super::projection::{declaration_id, declaration_key};
use super::{
    DeclarationId, DeclarationKind, DeclarationNode, ProjectionDiagnostic, SourceComponent,
    SourceComponentRole, Visibility, projection_hash,
};

#[derive(Debug, Clone, Copy)]
enum BindingScope {
    Let,
    Output,
}

impl BindingScope {
    const fn visibility(self) -> Visibility {
        match self {
            Self::Let => Visibility::Private,
            Self::Output => Visibility::Public,
        }
    }

    const fn qualifier(self) -> &'static str {
        match self {
            Self::Let => "let",
            Self::Output => "output",
        }
    }
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
        .set_language(&tree_sitter_nix::LANGUAGE.into())
        .context("failed to load the Nix grammar for declaration projection")?;
    let tree = parser
        .parse(source, None)
        .context("tree-sitter did not produce a Nix syntax tree")?;

    let mut projector = Projector {
        path,
        source,
        next_ordinal: 0,
        declarations: Vec::new(),
        diagnostics: Vec::new(),
    };
    let root = tree.root_node();
    if let Some(expression) = root.child_by_field_name("expression").or_else(|| {
        let mut cursor = root.walk();
        root.named_children(&mut cursor).next()
    }) {
        projector.collect_root_expression(expression)?;
    }
    if root.has_error() {
        projector.diagnostics.push(ProjectionDiagnostic::new(
            "Nix source contains syntax errors; bindings with malformed projected surfaces were omitted",
        ));
    }
    Ok((projector.declarations, projector.diagnostics))
}

impl Projector<'_> {
    fn collect_root_expression(&mut self, expression: Node<'_>) -> Result<()> {
        match expression.kind() {
            "function_expression" => {
                if let Some(body) = expression.child_by_field_name("body") {
                    self.collect_root_expression(body)?;
                }
            }
            "let_expression" => {
                if let Some(bindings) = binding_set(expression) {
                    self.collect_binding_set(bindings, BindingScope::Let)?;
                }
                if let Some(body) = expression.child_by_field_name("body") {
                    self.collect_root_expression(body)?;
                }
            }
            "attrset_expression" | "rec_attrset_expression" => {
                if let Some(bindings) = binding_set(expression) {
                    self.collect_binding_set(bindings, BindingScope::Output)?;
                }
            }
            "let_attrset_expression" => {
                if let Some(bindings) = binding_set(expression) {
                    self.collect_binding_set(bindings, BindingScope::Let)?;
                }
            }
            "parenthesized_expression" => {
                if let Some(inner) = expression.child_by_field_name("expression") {
                    self.collect_root_expression(inner)?;
                }
            }
            "assert_expression" | "with_expression" => {
                if let Some(body) = expression.child_by_field_name("body") {
                    self.collect_root_expression(body)?;
                }
            }
            "if_expression" => self.diagnostics.push(ProjectionDiagnostic::new(
                "Nix root conditionals may select different declaration sets; branch bindings were not inventoried",
            )),
            _ => {}
        }
        Ok(())
    }

    fn collect_binding_set(&mut self, bindings: Node<'_>, scope: BindingScope) -> Result<()> {
        let mut pending_comments = Vec::<Range<usize>>::new();
        let mut cursor = bindings.walk();
        for child in bindings.named_children(&mut cursor) {
            match child.kind() {
                "comment" => {
                    if pending_comments.last().is_some_and(|previous| {
                        !comments_are_contiguous(self.source, previous.end, child.start_byte())
                    }) {
                        pending_comments.clear();
                    }
                    pending_comments.push(child.byte_range());
                }
                "binding" => {
                    let documentation = take_attached_comments(
                        self.source,
                        &mut pending_comments,
                        child.start_byte(),
                    );
                    self.add_binding(child, &documentation, scope)?;
                }
                "inherit" | "inherit_from" => {
                    let documentation = take_attached_comments(
                        self.source,
                        &mut pending_comments,
                        child.start_byte(),
                    );
                    self.add_inherit(child, &documentation, scope)?;
                }
                _ => pending_comments.clear(),
            }
        }
        Ok(())
    }

    fn add_binding(
        &mut self,
        binding: Node<'_>,
        documentation: &[Range<usize>],
        scope: BindingScope,
    ) -> Result<()> {
        let Some(attrpath) = binding.child_by_field_name("attrpath") else {
            self.omitted(binding, "binding without an attribute path");
            return Ok(());
        };
        let Some(name) = static_attribute_name(attrpath, self.source) else {
            self.omitted(binding, "binding with a nested or dynamic attribute path");
            return Ok(());
        };
        let Some(expression) = binding.child_by_field_name("expression") else {
            self.omitted(binding, &format!("binding {name} without a value"));
            return Ok(());
        };

        let (kind, role, projection_end) = if expression.kind() == "function_expression" {
            (
                DeclarationKind::Function,
                SourceComponentRole::Signature,
                function_signature_end(expression, self.source),
            )
        } else {
            (
                DeclarationKind::Constant,
                SourceComponentRole::Value,
                trim_ascii_end(self.source, binding.start_byte(), binding.end_byte()),
            )
        };
        if projection_end <= binding.start_byte() || binding.has_error() || expression.is_missing()
        {
            self.omitted(binding, &format!("malformed binding {name}"));
            return Ok(());
        }

        self.push_declaration(
            binding,
            name,
            kind,
            scope,
            documentation,
            binding.start_byte()..projection_end,
            role,
        )
    }

    fn add_inherit(
        &mut self,
        inherit: Node<'_>,
        documentation: &[Range<usize>],
        scope: BindingScope,
    ) -> Result<()> {
        let Some(attrs) = inherit.child_by_field_name("attrs") else {
            self.omitted(inherit, "inherit statement without attributes");
            return Ok(());
        };
        let mut cursor = attrs.walk();
        let names = attrs
            .named_children(&mut cursor)
            .filter_map(|attr| static_attribute_name(attr, self.source))
            .collect::<Vec<_>>();
        let all_attr_count = {
            let mut cursor = attrs.walk();
            attrs.named_children(&mut cursor).count()
        };
        if names.len() != 1 || all_attr_count != 1 {
            let statement = node_text(inherit, self.source).unwrap_or("inherit").trim();
            self.diagnostics.push(ProjectionDiagnostic::new(format!(
                "omitted Nix `{statement}` because a multi-name or dynamic inherit statement has no exclusive single declaration owner"
            )));
            return Ok(());
        }
        let Some(name) = names.into_iter().next() else {
            return Ok(());
        };
        let end = trim_ascii_end(self.source, inherit.start_byte(), inherit.end_byte());
        self.push_declaration(
            inherit,
            name,
            DeclarationKind::Constant,
            scope,
            documentation,
            inherit.start_byte()..end,
            SourceComponentRole::Value,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push_declaration(
        &mut self,
        syntax: Node<'_>,
        name: String,
        kind: DeclarationKind,
        scope: BindingScope,
        documentation: &[Range<usize>],
        projected_item: Range<usize>,
        item_role: SourceComponentRole,
    ) -> Result<()> {
        let components =
            declaration_components(self.source, documentation, projected_item, item_role)?;
        let projection_text = components
            .iter()
            .map(|component| component.text.as_str())
            .collect::<String>();
        let projection_hash = projection_hash(Language::Nix, kind, &components);
        let source_start = documentation
            .first()
            .map_or(syntax.start_byte(), |range| range.start);
        let source_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let id = declaration_id(
            self.path,
            kind,
            &name,
            source_ordinal,
            source_start,
            &projection_hash,
        );
        let key = declaration_key(Language::Nix, kind, &name, Some(scope.qualifier()), "");
        self.declarations.push(DeclarationNode {
            id: id.clone(),
            key,
            name,
            kind,
            visibility: scope.visibility(),
            parent_part: None,
            source_ordinal,
            source_span: source_start..syntax.end_byte(),
            components,
            projection_text,
            projection_hash,
            review_owner: id,
            children: Vec::<DeclarationId>::new(),
            type_use_sites: Vec::new(),
        });
        Ok(())
    }

    fn omitted(&mut self, node: Node<'_>, reason: &str) {
        self.diagnostics.push(ProjectionDiagnostic::new(format!(
            "omitted Nix declaration at byte {}: {reason}",
            node.start_byte()
        )));
    }
}

fn binding_set(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "binding_set")
}

fn static_attribute_name(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "interpolation" || has_descendant_kind(node, "interpolation") {
        return None;
    }
    if node.kind() == "attrpath" {
        let mut cursor = node.walk();
        let attrs = node.named_children(&mut cursor).collect::<Vec<_>>();
        if attrs.len() != 1 {
            return None;
        }
    }
    let text = node_text(node, source)?.trim();
    if text.is_empty() {
        return None;
    }
    Some(
        text.strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            .unwrap_or(text)
            .to_owned(),
    )
}

fn has_descendant_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| child.kind() == kind || has_descendant_kind(child, kind))
}

fn function_signature_end(function: Node<'_>, source: &str) -> usize {
    let Some(body) = function.child_by_field_name("body") else {
        return trim_ascii_end(source, function.start_byte(), function.end_byte());
    };
    if body.kind() == "function_expression" {
        function_signature_end(body, source)
    } else {
        trim_ascii_end(source, function.start_byte(), body.start_byte())
    }
}

fn trim_ascii_end(source: &str, start: usize, mut end: usize) -> usize {
    while end > start
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    end
}

fn take_attached_comments(
    source: &str,
    pending: &mut Vec<Range<usize>>,
    item_start: usize,
) -> Vec<Range<usize>> {
    let attached = pending
        .last()
        .is_some_and(|comment| comments_are_contiguous(source, comment.end, item_start));
    if attached {
        std::mem::take(pending)
    } else {
        pending.clear();
        Vec::new()
    }
}

fn comments_are_contiguous(source: &str, start: usize, end: usize) -> bool {
    let Some(gap) = source.get(start..end) else {
        return false;
    };
    gap.bytes().all(|byte| byte.is_ascii_whitespace())
        && gap.bytes().filter(|byte| *byte == b'\n').count() <= 1
}

fn declaration_components(
    source: &str,
    documentation: &[Range<usize>],
    item: Range<usize>,
    item_role: SourceComponentRole,
) -> Result<Vec<SourceComponent>> {
    let mut components = Vec::with_capacity(documentation.len().saturating_mul(2) + 2);
    let mut cursor = documentation
        .first()
        .map_or(item.start, |range| range.start);
    for range in documentation {
        push_component(
            &mut components,
            source,
            cursor..range.start,
            SourceComponentRole::Layout,
        )?;
        push_component(
            &mut components,
            source,
            range.clone(),
            SourceComponentRole::Documentation,
        )?;
        cursor = range.end;
    }
    push_component(
        &mut components,
        source,
        cursor..item.start,
        SourceComponentRole::Layout,
    )?;
    push_component(&mut components, source, item, item_role)?;
    Ok(components)
}

fn push_component(
    components: &mut Vec<SourceComponent>,
    source: &str,
    range: Range<usize>,
    role: SourceComponentRole,
) -> Result<()> {
    if range.is_empty() {
        return Ok(());
    }
    let text = source
        .get(range.clone())
        .context("Nix declaration component was not on UTF-8 boundaries")?;
    components.push(SourceComponent {
        role,
        source_range: range,
        text: text.to_owned(),
    });
    Ok(())
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}
