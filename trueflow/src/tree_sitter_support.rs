use crate::block::BlockKind;
use tree_sitter::Node;

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
