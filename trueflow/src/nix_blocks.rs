use crate::block::BlockKind;
use anyhow::Result;
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NixSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: BlockKind,
}

impl NixSpan {
    fn new(start: usize, end: usize, kind: BlockKind) -> Self {
        Self { start, end, kind }
    }
}

pub(crate) fn split_structural_children(
    source: &str,
    block_kind: BlockKind,
) -> Result<Option<Vec<NixSpan>>> {
    let spans = match block_kind {
        BlockKind::Variable => split_binding_children(source)?,
        BlockKind::Section | BlockKind::List | BlockKind::Code | BlockKind::Function => {
            split_expression_children(source)?
        }
        _ => None,
    };

    Ok(spans.map(normalize_spans))
}

fn split_binding_children(source: &str) -> Result<Option<Vec<NixSpan>>> {
    let wrapped = format!("{{{source}}}");
    let Some(tree) = parse_expression_tree(&wrapped)? else {
        return Ok(None);
    };
    let Some(expression) = tree.root_node().child_by_field_name("expression") else {
        return Ok(None);
    };
    let Some(binding_set) = first_child_of_kind(expression, "binding_set") else {
        return Ok(None);
    };

    let mut cursor = binding_set.walk();
    let Some(item) = binding_set
        .children(&mut cursor)
        .find(|child| matches!(child.kind(), "binding" | "inherit" | "inherit_from"))
    else {
        return Ok(None);
    };

    Ok(match item.kind() {
        "binding" => split_binding_node(item, source, 1),
        _ => None,
    })
}

fn split_expression_children(source: &str) -> Result<Option<Vec<NixSpan>>> {
    let Some(tree) = parse_expression_tree(source)? else {
        return Ok(None);
    };
    let Some(expression) = tree.root_node().child_by_field_name("expression") else {
        return Ok(None);
    };

    Ok(should_structurally_split_expression(expression)
        .then(|| collect_expression_children(expression, source, 0)))
}

fn parse_expression_tree(source: &str) -> Result<Option<Tree>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_nix::LANGUAGE.into())?;
    Ok(parser.parse(source, None))
}

fn should_structurally_split_expression(node: Node<'_>) -> bool {
    let semantic = semantic_nix_node(node);
    match semantic.kind() {
        "function_expression"
        | "let_expression"
        | "with_expression"
        | "assert_expression"
        | "attrset_expression"
        | "let_attrset_expression"
        | "rec_attrset_expression"
        | "list_expression" => true,
        "if_expression" => if_expression_has_structural_branch(semantic),
        _ => false,
    }
}

fn split_binding_node(node: Node<'_>, source: &str, offset: usize) -> Option<Vec<NixSpan>> {
    let expression = semantic_nix_node(node.child_by_field_name("expression")?);
    if !should_structurally_split_expression(expression) {
        return None;
    }

    let mut spans = Vec::new();
    let start = node.start_byte().saturating_sub(offset);

    if expression.kind() == "function_expression" {
        let body = semantic_nix_node(expression.child_by_field_name("body")?);
        push_range(
            &mut spans,
            start,
            body.start_byte().saturating_sub(offset),
            BlockKind::FunctionSignature,
        );
        if should_structurally_split_expression(body) {
            spans.extend(collect_expression_children(body, source, offset));
        } else {
            spans.push(expression_child_span(body, offset));
        }
    } else {
        push_range(
            &mut spans,
            start,
            expression.start_byte().saturating_sub(offset),
            BlockKind::Preamble,
        );
        spans.extend(collect_expression_children(expression, source, offset));
    }

    push_interstitial(
        &mut spans,
        source,
        expression.end_byte().saturating_sub(offset),
        node.end_byte().saturating_sub(offset),
    );
    Some(spans)
}

fn collect_expression_children(node: Node<'_>, source: &str, offset: usize) -> Vec<NixSpan> {
    let semantic = semantic_nix_node(node);
    match semantic.kind() {
        "function_expression" => collect_function_children(semantic, source, offset),
        "let_expression" => collect_let_children(semantic, source, offset),
        "with_expression" | "assert_expression" => {
            collect_prefix_and_body_children(semantic, source, offset)
        }
        "attrset_expression" | "let_attrset_expression" | "rec_attrset_expression" => {
            collect_attrset_children(semantic, source, offset)
        }
        "list_expression" => collect_list_children(semantic, source, offset),
        "if_expression" => collect_if_children(semantic, source, offset)
            .unwrap_or_else(|| vec![expression_child_span(semantic, offset)]),
        _ => vec![expression_child_span(semantic, offset)],
    }
}

fn collect_function_children(node: Node<'_>, source: &str, offset: usize) -> Vec<NixSpan> {
    let Some(body) = node.child_by_field_name("body").map(semantic_nix_node) else {
        return vec![expression_child_span(node, offset)];
    };

    let mut spans = Vec::new();
    push_range(
        &mut spans,
        node.start_byte().saturating_sub(offset),
        body.start_byte().saturating_sub(offset),
        BlockKind::FunctionSignature,
    );
    if should_structurally_split_expression(body) {
        spans.extend(collect_expression_children(body, source, offset));
    } else {
        spans.push(expression_child_span(body, offset));
    }
    push_interstitial(
        &mut spans,
        source,
        body.end_byte().saturating_sub(offset),
        node.end_byte().saturating_sub(offset),
    );
    spans
}

fn collect_let_children(node: Node<'_>, source: &str, offset: usize) -> Vec<NixSpan> {
    let body = node.child_by_field_name("body").map(semantic_nix_node);
    let binding_set = first_child_of_kind(node, "binding_set");

    let mut spans = Vec::new();
    let first_child_start = binding_set
        .map(|binding_set| binding_set.start_byte())
        .or_else(|| body.map(|body| body.start_byte()))
        .unwrap_or_else(|| node.end_byte());
    push_interstitial(
        &mut spans,
        source,
        node.start_byte().saturating_sub(offset),
        first_child_start.saturating_sub(offset),
    );

    if let Some(binding_set) = binding_set {
        collect_binding_set_children(binding_set, source, offset, &mut spans);
        if let Some(body) = body {
            push_interstitial(
                &mut spans,
                source,
                binding_set.end_byte().saturating_sub(offset),
                body.start_byte().saturating_sub(offset),
            );
        }
    }

    if let Some(body) = body {
        spans.push(expression_child_span(body, offset));
        push_interstitial(
            &mut spans,
            source,
            body.end_byte().saturating_sub(offset),
            node.end_byte().saturating_sub(offset),
        );
    }

    spans
}

fn collect_prefix_and_body_children(node: Node<'_>, source: &str, offset: usize) -> Vec<NixSpan> {
    let Some(body) = node.child_by_field_name("body").map(semantic_nix_node) else {
        return vec![expression_child_span(node, offset)];
    };

    let mut spans = Vec::new();
    push_interstitial(
        &mut spans,
        source,
        node.start_byte().saturating_sub(offset),
        body.start_byte().saturating_sub(offset),
    );
    spans.push(expression_child_span(body, offset));
    push_interstitial(
        &mut spans,
        source,
        body.end_byte().saturating_sub(offset),
        node.end_byte().saturating_sub(offset),
    );
    spans
}

fn collect_attrset_children(node: Node<'_>, source: &str, offset: usize) -> Vec<NixSpan> {
    let Some(binding_set) = first_child_of_kind(node, "binding_set") else {
        return vec![expression_child_span(node, offset)];
    };

    let mut spans = Vec::new();
    push_interstitial(
        &mut spans,
        source,
        node.start_byte().saturating_sub(offset),
        binding_set.start_byte().saturating_sub(offset),
    );
    collect_binding_set_children(binding_set, source, offset, &mut spans);
    push_interstitial(
        &mut spans,
        source,
        binding_set.end_byte().saturating_sub(offset),
        node.end_byte().saturating_sub(offset),
    );
    spans
}

fn collect_list_children(node: Node<'_>, source: &str, offset: usize) -> Vec<NixSpan> {
    let mut cursor = node.walk();
    let elements = node
        .children_by_field_name("element", &mut cursor)
        .map(semantic_nix_node)
        .collect::<Vec<_>>();

    if elements.is_empty() {
        return vec![expression_child_span(node, offset)];
    }

    let mut spans = Vec::new();
    let mut last_end = node.start_byte().saturating_sub(offset);
    for element in elements {
        push_interstitial(
            &mut spans,
            source,
            last_end,
            element.start_byte().saturating_sub(offset),
        );
        spans.push(expression_child_span(element, offset));
        last_end = element.end_byte().saturating_sub(offset);
    }
    push_interstitial(
        &mut spans,
        source,
        last_end,
        node.end_byte().saturating_sub(offset),
    );
    spans
}

fn collect_if_children(node: Node<'_>, source: &str, offset: usize) -> Option<Vec<NixSpan>> {
    let consequence = semantic_nix_node(node.child_by_field_name("consequence")?);
    let alternative = semantic_nix_node(node.child_by_field_name("alternative")?);
    if !if_expression_has_structural_branch(node) {
        return None;
    }

    let mut spans = Vec::new();
    push_interstitial(
        &mut spans,
        source,
        node.start_byte().saturating_sub(offset),
        consequence.start_byte().saturating_sub(offset),
    );
    spans.push(expression_child_span(consequence, offset));
    push_interstitial(
        &mut spans,
        source,
        consequence.end_byte().saturating_sub(offset),
        alternative.start_byte().saturating_sub(offset),
    );
    spans.push(expression_child_span(alternative, offset));
    push_interstitial(
        &mut spans,
        source,
        alternative.end_byte().saturating_sub(offset),
        node.end_byte().saturating_sub(offset),
    );
    Some(spans)
}

fn collect_binding_set_children(
    binding_set: Node<'_>,
    source: &str,
    offset: usize,
    spans: &mut Vec<NixSpan>,
) {
    let mut cursor = binding_set.walk();
    let items = binding_set
        .children(&mut cursor)
        .filter(|child| matches!(child.kind(), "binding" | "inherit" | "inherit_from"))
        .collect::<Vec<_>>();

    let mut last_end = binding_set.start_byte().saturating_sub(offset);
    for item in items {
        push_interstitial(
            spans,
            source,
            last_end,
            item.start_byte().saturating_sub(offset),
        );
        let kind = match item.kind() {
            "binding" => BlockKind::Variable,
            "inherit" | "inherit_from" => BlockKind::Import,
            _ => BlockKind::Code,
        };
        push_range(
            spans,
            item.start_byte().saturating_sub(offset),
            item.end_byte().saturating_sub(offset),
            kind,
        );
        last_end = item.end_byte().saturating_sub(offset);
    }
}

fn expression_child_span(node: Node<'_>, offset: usize) -> NixSpan {
    let semantic = semantic_nix_node(node);
    NixSpan::new(
        semantic.start_byte().saturating_sub(offset),
        semantic.end_byte().saturating_sub(offset),
        classify_expression_child_kind(semantic),
    )
}

fn classify_expression_child_kind(node: Node<'_>) -> BlockKind {
    match semantic_nix_node(node).kind() {
        "attrset_expression" | "let_attrset_expression" | "rec_attrset_expression" => {
            BlockKind::Section
        }
        "list_expression" => BlockKind::List,
        "function_expression" => BlockKind::Function,
        "integer_expression"
        | "float_expression"
        | "string_expression"
        | "indented_string_expression"
        | "path_expression"
        | "hpath_expression"
        | "spath_expression"
        | "uri_expression"
        | "variable_expression"
        | "select_expression" => BlockKind::Content,
        _ => BlockKind::Code,
    }
}

fn if_expression_has_structural_branch(node: Node<'_>) -> bool {
    let Some(consequence) = node
        .child_by_field_name("consequence")
        .map(semantic_nix_node)
    else {
        return false;
    };
    let Some(alternative) = node
        .child_by_field_name("alternative")
        .map(semantic_nix_node)
    else {
        return false;
    };
    branch_is_structural(consequence) || branch_is_structural(alternative)
}

fn branch_is_structural(node: Node<'_>) -> bool {
    matches!(
        semantic_nix_node(node).kind(),
        "function_expression"
            | "let_expression"
            | "with_expression"
            | "assert_expression"
            | "attrset_expression"
            | "let_attrset_expression"
            | "rec_attrset_expression"
            | "list_expression"
    )
}

fn semantic_nix_node(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        let mut cursor = node.walk();
        let Some(child) = node.children(&mut cursor).find(|child| child.is_named()) else {
            break;
        };
        node = child;
    }
    node
}

fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn normalize_spans(spans: Vec<NixSpan>) -> Vec<NixSpan> {
    let mut normalized: Vec<NixSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        if let Some(previous) = normalized.last_mut()
            && previous.kind == span.kind
            && previous.end == span.start
        {
            previous.end = span.end;
        } else {
            normalized.push(span);
        }
    }
    normalized
}

fn push_range(spans: &mut Vec<NixSpan>, start: usize, end: usize, kind: BlockKind) {
    if end > start {
        spans.push(NixSpan::new(start, end, kind));
    }
}

fn push_interstitial(spans: &mut Vec<NixSpan>, source: &str, start: usize, end: usize) {
    if end <= start {
        return;
    }

    let chunk = &source[start..end];
    if chunk.is_empty() {
        return;
    }

    if chunk.trim().is_empty() || is_nix_structural_noise(chunk) {
        spans.push(NixSpan::new(start, end, BlockKind::Gap));
        return;
    }

    let mut segment_start = start;
    let mut line_start = start;
    let mut current_kind = None;

    while line_start < end {
        let line_end = source[line_start..end]
            .find('\n')
            .map(|offset| line_start + offset + 1)
            .unwrap_or(end);
        let line = &source[line_start..line_end];
        let kind = classify_interstitial_line(line);

        if let Some(previous_kind) = current_kind {
            if previous_kind != kind {
                spans.push(NixSpan::new(segment_start, line_start, previous_kind));
                segment_start = line_start;
            }
        } else {
            segment_start = line_start;
        }
        current_kind = Some(kind);
        line_start = line_end;
    }

    if let Some(kind) = current_kind {
        spans.push(NixSpan::new(segment_start, end, kind));
    }
}

fn classify_interstitial_line(line: &str) -> BlockKind {
    let trimmed = line.trim();
    if trimmed.is_empty() || is_nix_structural_noise(line) {
        BlockKind::Gap
    } else if line_is_nix_comment(trimmed) {
        BlockKind::Comment
    } else {
        BlockKind::Preamble
    }
}

fn is_nix_structural_noise(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '{' | '}' | '[' | ']' | ';'))
}

fn line_is_nix_comment(trimmed_line: &str) -> bool {
    trimmed_line.starts_with('#') || trimmed_line.starts_with("/*")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge_spans(source: &str, spans: &[NixSpan]) -> String {
        spans
            .iter()
            .map(|span| source[span.start..span.end].to_string())
            .collect::<String>()
    }

    #[test]
    fn split_binding_children_exposes_attrset_members() {
        let source = "defaults = {\n  retries = 3;\n};";
        let spans = split_binding_children(source)
            .unwrap()
            .expect("expected structural split");
        assert!(spans.iter().any(|span| {
            span.kind == BlockKind::Preamble && &source[span.start..span.end] == "defaults = "
        }));
        assert!(spans.iter().any(|span| {
            span.kind == BlockKind::Variable
                && source[span.start..span.end].contains("retries = 3;")
        }));
        assert_eq!(merge_spans(source, &spans), source);
    }

    #[test]
    fn split_expression_children_exposes_attrset_bindings() {
        let source = "{\n  retries = 3;\n  labels = { tier = \"backend\"; };\n}";
        let spans = split_expression_children(source)
            .unwrap()
            .expect("expected structural split");
        assert!(spans.iter().any(|span| span.kind == BlockKind::Variable));
        assert_eq!(merge_spans(source, &spans), source);
    }

    #[test]
    fn split_expression_children_exposes_if_branches_when_structural() {
        let source = "if enabled then { system = \"linux\"; } else { system = \"other\"; }";
        let spans = split_expression_children(source)
            .unwrap()
            .expect("expected structural split");
        assert!(spans.iter().any(|span| span.kind == BlockKind::Section));
        assert_eq!(merge_spans(source, &spans), source);
    }

    #[test]
    fn split_binding_children_keeps_whitespace_between_list_items_as_gap() {
        let source = "packages = [\n  pkgs.git\n  { name = \"helper\"; }\n];";
        let spans = split_structural_children(source, BlockKind::Variable)
            .unwrap()
            .expect("expected structural split");
        let preambles = spans
            .iter()
            .filter(|span| span.kind == BlockKind::Preamble)
            .collect::<Vec<_>>();
        assert_eq!(preambles.len(), 1, "unexpected preambles: {spans:#?}");
        assert_eq!(&source[preambles[0].start..preambles[0].end], "packages = ");
        assert_eq!(merge_spans(source, &spans), source);
    }

    #[test]
    fn split_binding_children_merges_adjacent_preamble_segments() {
        let source =
            "selected = if enabled then { system = \"linux\"; } else { system = \"other\"; };";
        let spans = split_structural_children(source, BlockKind::Variable)
            .unwrap()
            .expect("expected structural split");
        let preambles = spans
            .iter()
            .filter(|span| span.kind == BlockKind::Preamble)
            .collect::<Vec<_>>();
        assert_eq!(preambles.len(), 2, "unexpected preambles: {spans:#?}");
        assert_eq!(
            &source[preambles[0].start..preambles[0].end],
            "selected = if enabled then "
        );
        assert_eq!(&source[preambles[1].start..preambles[1].end], " else ");
        assert_eq!(merge_spans(source, &spans), source);
    }
}
