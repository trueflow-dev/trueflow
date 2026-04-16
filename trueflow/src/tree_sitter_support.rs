use crate::block::BlockKind;
use tree_sitter::Node;

pub(crate) fn find_named_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    find_named_descendant_any(node, &[kind])
}

pub(crate) fn find_named_descendant_any<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    if kinds.iter().any(|kind| *kind == node.kind()) {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_named_descendant_any(child, kinds) {
            return Some(found);
        }
    }

    None
}

pub(crate) fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

pub(crate) fn elisp_list_head_symbol<'a>(node: Node<'a>, content: &'a str) -> Option<&'a str> {
    let head = node.named_child(0)?;
    (head.kind() == "symbol")
        .then(|| head.utf8_text(content.as_bytes()).ok())
        .flatten()
}

pub(crate) fn kotlin_type_body(node: Node<'_>) -> Option<Node<'_>> {
    first_child_of_kind(node, "class_body").or_else(|| first_child_of_kind(node, "enum_class_body"))
}

pub(crate) fn classify_kotlin_class_kind(node: Node<'_>, content: &str) -> BlockKind {
    let name_start = node
        .child_by_field_name("name")
        .map(|name| name.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let header = &content[node.start_byte()..name_start.min(node.end_byte())];

    if header.contains("interface") {
        BlockKind::Interface
    } else if header.contains("enum") {
        BlockKind::Enum
    } else {
        BlockKind::Class
    }
}

pub(crate) fn classify_kotlin_property_kind(text: &str) -> BlockKind {
    if text.contains("var ") {
        BlockKind::Variable
    } else {
        BlockKind::Const
    }
}

pub(crate) fn markdown_heading_level(kind: &str, start: usize, content: &str) -> Option<u8> {
    match kind {
        "atx_heading" => {
            let line = content.get(start..)?.lines().next()?;
            let level = line.chars().take_while(|ch| *ch == '#').count();
            if level > 0 {
                u8::try_from(level.min(6)).ok()
            } else {
                None
            }
        }
        "setext_heading" => {
            let line = content.get(start..)?.lines().next()?;
            if line.chars().all(|ch| ch == '=') {
                Some(1)
            } else if line.chars().all(|ch| ch == '-') {
                Some(2)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn ruby_call_method_name(node: Node<'_>, content: &str) -> Option<String> {
    node.child_by_field_name("method")
        .and_then(|method| method.utf8_text(content.as_bytes()).ok())
        .map(str::to_string)
}

pub(crate) fn ruby_assignment_targets_constant(node: Node<'_>) -> bool {
    let Some(left) = node.child_by_field_name("left") else {
        return false;
    };

    ruby_lhs_targets_constant(left)
}

fn ruby_lhs_targets_constant(node: Node<'_>) -> bool {
    match node.kind() {
        "constant" => true,
        "scope_resolution" => node
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "constant"),
        "left_assignment_list" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .all(ruby_lhs_targets_constant)
        }
        _ => false,
    }
}
