use crate::block::BlockKind;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypeMemberSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) kind: BlockKind,
}

pub(crate) fn map_kind(node: Node<'_>, content: &str) -> BlockKind {
    match node.kind() {
        "import_declaration" => BlockKind::Import,
        "function_declaration" => BlockKind::Function,
        "class_declaration" => match declaration_keyword(node) {
            Some("struct") => BlockKind::Struct,
            Some("enum") => BlockKind::Enum,
            Some("extension") => BlockKind::Impl,
            Some("actor" | "class") => BlockKind::Class,
            _ => BlockKind::Code,
        },
        "protocol_declaration" => BlockKind::Interface,
        "typealias_declaration" | "associatedtype_declaration" => BlockKind::Type,
        "property_declaration" | "protocol_property_declaration" => {
            map_property_kind(node, content)
        }
        "init_declaration" | "deinit_declaration" | "subscript_declaration" => BlockKind::Method,
        "protocol_function_declaration" => BlockKind::FunctionSignature,
        _ => BlockKind::Code,
    }
}

pub(crate) fn is_attribute_node(kind: &str) -> bool {
    matches!(kind, "attribute" | "comment" | "multiline_comment")
}

pub(crate) fn body_is_non_trivial(body: Node<'_>, content: &str) -> bool {
    let mut cursor = body.walk();
    let member_count = body
        .children(&mut cursor)
        .filter(|child| map_member_kind(*child, content).is_some())
        .count();
    if member_count >= 2 {
        return true;
    }

    let body_content = &content[body.start_byte()..body.end_byte()];
    body_content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        >= 10
}

pub(crate) fn collect_type_member_spans(body: Node<'_>, content: &str) -> Vec<TypeMemberSpan> {
    let mut members = Vec::new();
    let mut cursor = body.walk();
    let mut pending_start: Option<usize> = None;
    let mut pending_end: usize = 0;

    for child in body.children(&mut cursor) {
        let ts_kind = child.kind();
        if matches!(ts_kind, "{" | "}") {
            continue;
        }

        let start_byte = child.start_byte();
        let end_byte = child.end_byte();
        if is_attribute_node(ts_kind) {
            if pending_start.is_none() {
                pending_start = Some(start_byte);
            }
            pending_end = end_byte;
            continue;
        }

        let Some(kind) = map_member_kind(child, content) else {
            continue;
        };

        members.push(TypeMemberSpan {
            start_byte: pending_start.unwrap_or(start_byte),
            end_byte,
            kind,
        });
        pending_start = None;
        pending_end = 0;
    }

    if let Some(start) = pending_start {
        let end = pending_end.max(start);
        if end > start {
            members.push(TypeMemberSpan {
                start_byte: start,
                end_byte: end,
                kind: BlockKind::Code,
            });
        }
    }

    members
}

fn map_member_kind(node: Node<'_>, content: &str) -> Option<BlockKind> {
    match node.kind() {
        "attribute" | "comment" | "multiline_comment" => None,
        "class_declaration" => Some(match declaration_keyword(node) {
            Some("struct") => BlockKind::Struct,
            Some("enum") => BlockKind::Enum,
            Some("extension") => BlockKind::Impl,
            Some("actor" | "class") => BlockKind::Class,
            _ => BlockKind::Code,
        }),
        "protocol_declaration" => Some(BlockKind::Interface),
        "function_declaration"
        | "init_declaration"
        | "deinit_declaration"
        | "subscript_declaration" => Some(BlockKind::Method),
        "protocol_function_declaration" => Some(BlockKind::FunctionSignature),
        "typealias_declaration" | "associatedtype_declaration" => Some(BlockKind::Type),
        "property_declaration" | "protocol_property_declaration" => {
            Some(map_property_kind(node, content))
        }
        "import_declaration" => Some(BlockKind::Import),
        _ => None,
    }
}

fn declaration_keyword(node: Node<'_>) -> Option<&str> {
    node.child_by_field_name("declaration_kind")
        .map(|child| child.kind())
}

fn map_property_kind(node: Node<'_>, content: &str) -> BlockKind {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "value_binding_pattern" {
            continue;
        }
        if let Some(mutability) = child.child_by_field_name("mutability") {
            return if mutability.kind() == "let" {
                BlockKind::Const
            } else {
                BlockKind::Variable
            };
        }
    }

    let node_content = &content[node.start_byte()..node.end_byte()];
    if node_content.contains("let ") {
        BlockKind::Const
    } else {
        BlockKind::Variable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    #[test]
    fn collect_type_member_spans_attach_attributes_and_comments() {
        let source = "extension Worker {\n    @MainActor\n    func run() {}\n\n    // UI\n    var body: some View { Text(\"hi\") }\n}\n";
        let tree = parse_tree(source);
        let body = find_type_body(tree.root_node());

        let members = collect_type_member_spans(body, source);

        assert_eq!(
            members.iter().map(|member| member.kind).collect::<Vec<_>>(),
            vec![BlockKind::Method, BlockKind::Variable]
        );
        assert!(
            source[members[0].start_byte..members[0].end_byte].starts_with("@MainActor"),
            "expected first member to include leading attribute"
        );
        assert!(
            source[members[1].start_byte..members[1].end_byte].starts_with("// UI"),
            "expected second member to include leading comment"
        );
    }

    fn parse_tree(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_swift::LANGUAGE.into())
            .unwrap_or_else(|error| panic!("load swift grammar: {error}"));
        parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("parse swift"))
    }

    fn find_type_body(root: Node<'_>) -> Node<'_> {
        let type_node = find_named_descendant(root, "class_declaration")
            .unwrap_or_else(|| panic!("expected swift class_declaration"));
        type_node
            .child_by_field_name("body")
            .unwrap_or_else(|| panic!("expected swift type body"))
    }

    fn find_named_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(found) = find_named_descendant(child, kind) {
                return Some(found);
            }
        }

        None
    }
}
