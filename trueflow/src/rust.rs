use crate::block::BlockKind;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImplMemberSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) kind: BlockKind,
}

pub(crate) fn collect_impl_member_spans(container_node: Node<'_>) -> Vec<ImplMemberSpan> {
    let Some(body) = container_node.child_by_field_name("body") else {
        return Vec::new();
    };

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

        let Some(kind) = map_impl_member_kind(ts_kind) else {
            continue;
        };

        members.push(ImplMemberSpan {
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
            members.push(ImplMemberSpan {
                start_byte: start,
                end_byte: end,
                kind: BlockKind::Code,
            });
        }
    }

    members
}

fn map_impl_member_kind(kind: &str) -> Option<BlockKind> {
    match kind {
        "function_item" => Some(BlockKind::Method),
        "function_signature_item" => Some(BlockKind::FunctionSignature),
        "const_item" => Some(BlockKind::Const),
        "static_item" => Some(BlockKind::Static),
        "type_item" | "associated_type" => Some(BlockKind::Type),
        "macro_invocation" | "macro_definition" => Some(BlockKind::Macro),
        _ => None,
    }
}

fn is_attribute_node(kind: &str) -> bool {
    matches!(
        kind,
        "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    #[test]
    fn collect_impl_member_spans_attach_attributes_and_comments() {
        let source = "impl Worker {\n    #[cfg(test)]\n    fn run(&self) {}\n\n    // limit\n    const MAX: usize = 1;\n}\n";
        let tree = parse_tree(source);
        let impl_node = find_named_descendant(tree.root_node(), "impl_item")
            .unwrap_or_else(|| panic!("expected rust impl_item"));

        let members = collect_impl_member_spans(impl_node);

        assert_eq!(
            members.iter().map(|member| member.kind).collect::<Vec<_>>(),
            vec![BlockKind::Method, BlockKind::Const]
        );
        assert!(
            source[members[0].start_byte..members[0].end_byte].starts_with("#[cfg(test)]"),
            "expected method to include leading attribute"
        );
        assert!(
            source[members[1].start_byte..members[1].end_byte].starts_with("// limit"),
            "expected const to include leading comment"
        );
    }

    #[test]
    fn collect_impl_member_spans_keep_trailing_attributes_as_code() {
        let source = "impl Worker {\n    #[cfg(test)]\n}\n";
        let tree = parse_tree(source);
        let impl_node = find_named_descendant(tree.root_node(), "impl_item")
            .unwrap_or_else(|| panic!("expected rust impl_item"));

        let members = collect_impl_member_spans(impl_node);

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].kind, BlockKind::Code);
        assert_eq!(
            &source[members[0].start_byte..members[0].end_byte],
            "#[cfg(test)]"
        );
    }

    fn parse_tree(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap_or_else(|error| panic!("load rust grammar: {error}"));
        parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("parse rust"))
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
