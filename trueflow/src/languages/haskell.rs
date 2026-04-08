use super::{
    LanguageRegistration, LanguageSubSplitSemantics, SubSplitRegistration, TopLevelRegistration,
    default_code_sub_split, default_map_kind, no_attribute_nodes, no_nested_blocks, no_test_ranges,
};
use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::complexity;
use crate::hashing::TreeHash;
use crate::text_split::paragraph_break_regex;
use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Parser, Tree};

#[derive(Debug, Clone)]
struct SemanticSpan {
    start_byte: usize,
    end_byte: usize,
    kind: BlockKind,
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
struct CollectedItem {
    span: SemanticSpan,
}

struct DeclEntry<'tree> {
    node: Node<'tree>,
    span: SemanticSpan,
    name: Option<String>,
}

pub(crate) fn registration() -> LanguageRegistration {
    LanguageRegistration {
        top_level: TopLevelRegistration {
            parser_language,
            map_kind: default_map_kind,
            is_attribute_node: no_attribute_nodes,
            collect_nested_blocks: no_nested_blocks,
            collect_test_ranges: no_test_ranges,
            custom_splitter: Some(split_top_level),
        },
        sub_split: sub_split_registration,
    }
}

fn parser_language(_content: &str) -> TsLanguage {
    tree_sitter_haskell::LANGUAGE.into()
}

fn split_top_level(root: Node<'_>, content: &str, lang: Language) -> Result<Vec<Block>> {
    let mut items = collect_top_level_items(root, content);
    items.sort_by_key(|item| item.span.start_byte);

    let mut blocks = Vec::new();
    let mut last_end = 0usize;

    for item in items {
        push_top_level_interstitial(&mut blocks, content, last_end, item.span.start_byte, lang);
        blocks.push(create_file_block(
            &content[item.span.start_byte..item.span.end_byte],
            item.span.kind,
            item.span.tags,
            content,
            item.span.start_byte,
            item.span.end_byte,
            lang,
        ));
        last_end = item.span.end_byte;
    }

    push_top_level_interstitial(&mut blocks, content, last_end, content.len(), lang);
    Ok(blocks)
}

fn collect_top_level_items(root: Node<'_>, content: &str) -> Vec<CollectedItem> {
    let mut items = Vec::new();
    let mut cursor = root.walk();

    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "header" => items.push(CollectedItem {
                span: SemanticSpan {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    kind: BlockKind::Module,
                    tags: Vec::new(),
                },
            }),
            "imports" => items.extend(collect_import_items(child, content)),
            "declarations" => items.extend(collect_declaration_items(child, content, true)),
            "import" => items.push(CollectedItem {
                span: SemanticSpan {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    kind: BlockKind::Import,
                    tags: Vec::new(),
                },
            }),
            _ => {
                if let Some(entry) = classify_declaration_entry(child, content) {
                    items.push(CollectedItem { span: entry.span });
                }
            }
        }
    }

    items
}

fn collect_import_items(container: Node<'_>, _content: &str) -> Vec<CollectedItem> {
    let mut items = Vec::new();
    let mut cursor = container.walk();

    for child in container.named_children(&mut cursor) {
        if child.kind() != "import" {
            continue;
        }

        items.push(CollectedItem {
            span: SemanticSpan {
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                kind: BlockKind::Import,
                tags: Vec::new(),
            },
        });
    }

    items
}

fn collect_declaration_items(
    container: Node<'_>,
    content: &str,
    include_nested_children: bool,
) -> Vec<CollectedItem> {
    let entries = collect_declaration_entries(container, content);
    let mut items = Vec::new();
    let mut index = 0usize;

    while index < entries.len() {
        let entry = &entries[index];
        if entry.span.kind == BlockKind::FunctionSignature
            && let Some(name) = entry.name.as_deref()
        {
            let mut next_index = index + 1;
            let mut end_byte = None;
            let mut tags = entry.span.tags.clone();

            while let Some(next) = entries.get(next_index) {
                if next.span.kind != BlockKind::Function || next.name.as_deref() != Some(name) {
                    break;
                }
                end_byte = Some(next.span.end_byte);
                extend_tags(&mut tags, &next.span.tags);
                next_index += 1;
            }

            if let Some(end_byte) = end_byte {
                items.push(CollectedItem {
                    span: SemanticSpan {
                        start_byte: entry.span.start_byte,
                        end_byte,
                        kind: BlockKind::Function,
                        tags,
                    },
                });
                index = next_index;
                continue;
            }
        }

        if entry.span.kind == BlockKind::Function
            && let Some(name) = entry.name.as_deref()
        {
            let mut next_index = index + 1;
            let mut end_byte = entry.span.end_byte;
            let mut saw_additional_clause = false;
            let mut tags = entry.span.tags.clone();

            while let Some(next) = entries.get(next_index) {
                if next.span.kind != BlockKind::Function || next.name.as_deref() != Some(name) {
                    break;
                }
                end_byte = next.span.end_byte;
                saw_additional_clause = true;
                extend_tags(&mut tags, &next.span.tags);
                next_index += 1;
            }

            if saw_additional_clause {
                items.push(CollectedItem {
                    span: SemanticSpan {
                        start_byte: entry.span.start_byte,
                        end_byte,
                        kind: BlockKind::Function,
                        tags,
                    },
                });
                index = next_index;
                continue;
            }
        }

        let _ = include_nested_children;
        let _ = content;
        let _ = entry.node;

        items.push(CollectedItem {
            span: entry.span.clone(),
        });
        index += 1;
    }

    items
}

fn collect_declaration_entries<'tree>(
    container: Node<'tree>,
    content: &str,
) -> Vec<DeclEntry<'tree>> {
    let mut entries = Vec::new();
    let mut cursor = container.walk();

    for child in container.named_children(&mut cursor) {
        if let Some(entry) = classify_declaration_entry(child, content) {
            entries.push(entry);
        }
    }

    entries
}

fn classify_declaration_entry<'tree>(node: Node<'tree>, content: &str) -> Option<DeclEntry<'tree>> {
    let node = unwrap_declaration_wrapper(node);
    let kind = match node.kind() {
        "comment" | "haddock" | "pragma" | "cpp" => return None,
        "header" => BlockKind::Module,
        "import" => BlockKind::Import,
        "signature" | "default_signature" => BlockKind::FunctionSignature,
        "function" | "bind" => BlockKind::Function,
        "class" => BlockKind::Class,
        "instance" | "deriving_instance" => BlockKind::Impl,
        "data_type" | "newtype" | "type_synomym" | "type_family" | "data_family"
        | "data_instance" | "type_instance" => BlockKind::Type,
        "default_types" | "fixity" | "foreign_export" | "foreign_import" | "kind_signature"
        | "pattern_synonym" | "role_annotation" | "top_splice" => BlockKind::Code,
        _ => return None,
    };

    let name = declaration_name(node, content);
    let mut tags = Vec::new();
    if matches!(kind, BlockKind::Function | BlockKind::FunctionSignature)
        && name.as_deref().is_some_and(is_test_like_name)
    {
        tags.push("test".to_string());
    }

    Some(DeclEntry {
        node,
        span: SemanticSpan {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind,
            tags,
        },
        name,
    })
}

fn unwrap_declaration_wrapper(mut node: Node<'_>) -> Node<'_> {
    loop {
        if !matches!(
            node.kind(),
            "declaration" | "decl" | "class_decl" | "instance_decl"
        ) {
            return node;
        }

        let Some(child) = first_named_child(node) else {
            return node;
        };
        node = child;
    }
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn collect_group_member_spans(group_node: Node<'_>, content: &str) -> Vec<SemanticSpan> {
    let Some(declarations) = group_node.child_by_field_name("declarations") else {
        return Vec::new();
    };

    collect_declaration_items(declarations, content, false)
        .into_iter()
        .map(|item| item.span)
        .collect()
}

fn declaration_name(node: Node<'_>, content: &str) -> Option<String> {
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|name| node_text(name, content))
    {
        return Some(name.to_string());
    }

    if let Some(module) = node
        .child_by_field_name("module")
        .and_then(|module| node_text(module, content))
    {
        return Some(module.to_string());
    }

    if let Some(names) = node.child_by_field_name("names")
        && let Some(name) = first_named_descendant_text(names, content)
    {
        return Some(name);
    }

    if let Some(signature) = node.child_by_field_name("signature") {
        return declaration_name(signature, content);
    }

    if let Some(synonym) = node.child_by_field_name("synonym")
        && let Some(name) = first_named_descendant_text(synonym, content)
    {
        return Some(name);
    }

    None
}

fn node_text<'a>(node: Node<'_>, content: &'a str) -> Option<&'a str> {
    node.utf8_text(content.as_bytes()).ok()
}

fn first_named_descendant_text(node: Node<'_>, content: &str) -> Option<String> {
    if let Some(text) = node_text(node, content)
        && matches!(
            node.kind(),
            "variable" | "prefix_id" | "constructor" | "module_id"
        )
    {
        return Some(text.to_string());
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(text) = first_named_descendant_text(child, content) {
            return Some(text);
        }
    }

    None
}

fn is_test_like_name(name: &str) -> bool {
    has_test_like_prefix(name, "test")
        || has_test_like_prefix(name, "spec")
        || has_test_like_prefix(name, "prop")
        || has_test_like_prefix(name, "case")
}

fn has_test_like_prefix(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.is_empty()
            || suffix.starts_with('_')
            || suffix.starts_with('"')
            || suffix.chars().next().is_some_and(char::is_uppercase)
    })
}

fn sub_split_registration(kind: BlockKind) -> SubSplitRegistration {
    match kind {
        BlockKind::Function => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::ReviewUnits,
            splitter: split_function_like_review_units,
        },
        BlockKind::Class | BlockKind::Impl => SubSplitRegistration {
            semantics: LanguageSubSplitSemantics::StructuralChildren,
            splitter: split_declaration_group_children,
        },
        _ => default_code_sub_split(kind),
    }
}

fn split_function_like_review_units(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Haskell function-like block")?;
    let root = tree.root_node();
    let entries = collect_top_level_declaration_entries(root, &block.content);

    let Some(function_index) = entries
        .iter()
        .position(|entry| entry.span.kind == BlockKind::Function)
    else {
        return crate::sub_splitter::split_code_review_units(block);
    };
    let function_entry = &entries[function_index];
    let function_name = function_entry.name.as_deref();

    let signature_prefix_end = entries[..function_index]
        .iter()
        .rev()
        .take_while(|entry| {
            entry.span.kind == BlockKind::FunctionSignature
                && entry.name.as_deref() == function_name
        })
        .last()
        .map_or(0, |entry| entry.span.end_byte);

    let Some(head_end) = first_substantive_line_end(
        &block.content[function_entry.span.start_byte..function_entry.span.end_byte],
    ) else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let signature_end = signature_prefix_end.max(function_entry.span.start_byte + head_end);
    if signature_end == 0 || signature_end > block.content.len() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    let header_kind = if signature_prefix_end > 0 {
        BlockKind::FunctionSignature
    } else {
        BlockKind::CodeParagraph
    };

    let mut blocks = vec![create_sub_block(
        block,
        &block.content[..signature_end],
        0,
        header_kind,
        Vec::new(),
    )];
    blocks.extend(split_review_tail(block, signature_end));
    Ok(blocks)
}

fn split_declaration_group_children(block: &Block) -> Result<Vec<Block>> {
    let tree = parse_tree(&block.content).context("Failed to parse Haskell declaration group")?;
    let root = tree.root_node();
    let Some(group_node) =
        find_named_descendant_any(root, &["class", "instance", "deriving_instance"])
    else {
        return crate::sub_splitter::split_code_review_units(block);
    };

    let spans = collect_group_member_spans(group_node, &block.content);
    if spans.is_empty() {
        return crate::sub_splitter::split_code_review_units(block);
    }

    Ok(spans
        .into_iter()
        .map(|span| {
            create_sub_block(
                block,
                &block.content[span.start_byte..span.end_byte],
                span.start_byte,
                span.kind,
                span.tags,
            )
        })
        .collect())
}

fn collect_top_level_declaration_entries<'tree>(
    root: Node<'tree>,
    content: &str,
) -> Vec<DeclEntry<'tree>> {
    let mut entries = Vec::new();
    let mut cursor = root.walk();

    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "declarations" => entries.extend(collect_declaration_entries(child, content)),
            "header" | "imports" => {}
            _ => {
                if let Some(entry) = classify_declaration_entry(child, content) {
                    entries.push(entry);
                }
            }
        }
    }

    entries
}

fn split_review_tail(parent: &Block, start_offset: usize) -> Vec<Block> {
    let rest = &parent.content[start_offset..];
    let re = paragraph_break_regex();
    let mut blocks = Vec::new();
    let mut start = 0usize;

    for gap in re.find_iter(rest) {
        if start < gap.start() {
            let chunk = &rest[start..gap.start()];
            if !chunk.is_empty() {
                push_review_chunk(
                    parent,
                    start_offset + start,
                    start_offset + gap.start(),
                    &mut blocks,
                );
            }
        }

        blocks.push(create_sub_block(
            parent,
            &rest[gap.start()..gap.end()],
            start_offset + gap.start(),
            BlockKind::Gap,
            Vec::new(),
        ));
        start = gap.end();
    }

    if start < rest.len() {
        let chunk = &rest[start..];
        if !chunk.is_empty() {
            push_review_chunk(
                parent,
                start_offset + start,
                start_offset + rest.len(),
                &mut blocks,
            );
        }
    }

    blocks
}

fn push_review_chunk(parent: &Block, start: usize, end: usize, blocks: &mut Vec<Block>) {
    if start >= end {
        return;
    }

    let chunk = &parent.content[start..end];
    if let Some(comment_end) = leading_haskell_comment_prefix_len(chunk) {
        let comment = &chunk[..comment_end];
        blocks.push(create_sub_block(
            parent,
            comment,
            start,
            BlockKind::Comment,
            Vec::new(),
        ));

        let remainder = &chunk[comment_end..];
        if !remainder.trim().is_empty() {
            blocks.push(create_sub_block(
                parent,
                remainder,
                start + comment_end,
                BlockKind::CodeParagraph,
                Vec::new(),
            ));
        }
        return;
    }

    let kind = classify_review_chunk(chunk);
    if matches!(kind, BlockKind::Gap) {
        return;
    }

    blocks.push(create_sub_block(parent, chunk, start, kind, Vec::new()));
}

fn classify_review_chunk(chunk: &str) -> BlockKind {
    if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if chunk_is_haskell_comment_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn push_top_level_interstitial(
    blocks: &mut Vec<Block>,
    content: &str,
    start: usize,
    end: usize,
    lang: Language,
) {
    if end <= start {
        return;
    }

    let chunk = &content[start..end];
    let kind = if chunk.trim().is_empty() {
        BlockKind::Gap
    } else if chunk_is_haskell_trivia_only(chunk) {
        BlockKind::Comment
    } else {
        BlockKind::Code
    };

    if matches!(kind, BlockKind::Gap) && chunk.trim().is_empty() {
        return;
    }

    blocks.push(create_file_block(
        chunk,
        kind,
        Vec::new(),
        content,
        start,
        end,
        lang,
    ));
}

fn chunk_is_haskell_comment_only(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return false;
    }

    let mut block_comment_depth = 0usize;
    let mut saw_comment = false;

    for line in chunk.lines() {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            continue;
        }

        if block_comment_depth > 0 {
            saw_comment = true;
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        if trimmed.starts_with("--")
            || ((trimmed.starts_with("{-") && !trimmed.starts_with("{-#"))
                || trimmed.starts_with("-}"))
        {
            saw_comment = true;
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        return false;
    }

    saw_comment && block_comment_depth == 0
}

fn chunk_is_haskell_trivia_only(chunk: &str) -> bool {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return false;
    }

    let mut block_comment_depth = 0usize;
    let mut saw_trivia = false;

    for line in chunk.lines() {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            continue;
        }

        if block_comment_depth > 0 {
            saw_trivia = true;
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        if trimmed.starts_with("{-#") || trimmed.starts_with('#') {
            saw_trivia = true;
            continue;
        }

        if trimmed.starts_with("--")
            || ((trimmed.starts_with("{-") && !trimmed.starts_with("{-#"))
                || trimmed.starts_with("-}"))
        {
            saw_trivia = true;
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        return false;
    }

    saw_trivia && block_comment_depth == 0
}

fn leading_haskell_comment_prefix_len(chunk: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut saw_comment = false;
    let mut block_comment_depth = 0usize;

    for line in chunk.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            offset += line.len();
            continue;
        }

        if block_comment_depth > 0 {
            saw_comment = true;
            offset += line.len();
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        if trimmed.starts_with("--")
            || ((trimmed.starts_with("{-") && !trimmed.starts_with("{-#"))
                || trimmed.starts_with("-}"))
        {
            saw_comment = true;
            offset += line.len();
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        return (saw_comment && block_comment_depth == 0).then_some(offset);
    }

    (saw_comment && block_comment_depth == 0).then_some(offset)
}

fn first_substantive_line_end(content: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut block_comment_depth = 0usize;

    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.trim().is_empty() {
            offset += line.len();
            continue;
        }

        if block_comment_depth > 0 {
            offset += line.len();
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        if (trimmed.starts_with("{-") && !trimmed.starts_with("{-#")) || trimmed.starts_with("-}") {
            offset += line.len();
            block_comment_depth = block_comment_depth_after(block_comment_depth, trimmed);
            continue;
        }

        if trimmed.starts_with("--") {
            offset += line.len();
            continue;
        }

        offset += line.len();
        return Some(offset.min(content.len()));
    }

    if offset > 0 {
        Some(offset.min(content.len()))
    } else {
        None
    }
}

fn block_comment_depth_after(mut depth: usize, text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut index = 0usize;

    while index + 1 < bytes.len() {
        if bytes[index..].starts_with(b"{-#") || bytes[index..].starts_with(b"#-}") {
            index += 3;
            continue;
        }

        match (bytes[index], bytes[index + 1]) {
            (b'{', b'-') => {
                depth += 1;
                index += 2;
            }
            (b'-', b'}') => {
                depth = depth.saturating_sub(1);
                index += 2;
            }
            _ => index += 1,
        }
    }

    depth
}

fn parse_tree(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_haskell::LANGUAGE.into())
        .context("Failed to load Haskell grammar")?;
    parser
        .parse(source, None)
        .context("Failed to parse Haskell")
}

fn find_named_descendant_any<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
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

fn create_file_block(
    text: &str,
    kind: BlockKind,
    tags: Vec<String>,
    full_source: &str,
    start_byte: usize,
    end_byte: usize,
    lang: Language,
) -> Block {
    let (start_line, end_line) = byte_range_to_lines(full_source, start_byte, end_byte);
    Block {
        hash: TreeHash::from_content(text),
        content: text.to_string(),
        kind,
        tags,
        complexity: complexity::calculate(text, lang),
        start_line,
        end_line,
    }
}

fn create_sub_block(
    parent: &Block,
    text: &str,
    start_offset: usize,
    kind: BlockKind,
    tags: Vec<String>,
) -> Block {
    let pre_chunk = &parent.content[..start_offset];
    let offset_newlines = pre_chunk.chars().filter(|&ch| ch == '\n').count();
    let chunk_newlines = text.chars().filter(|&ch| ch == '\n').count();

    let start_line = parent.start_line + offset_newlines;
    let end_line = start_line + chunk_newlines + usize::from(!text.ends_with('\n'));

    let mut combined_tags = parent.tags.clone();
    extend_tags(&mut combined_tags, &tags);

    Block {
        hash: TreeHash::from_content(text),
        content: text.to_string(),
        kind,
        tags: combined_tags,
        complexity: None,
        start_line,
        end_line,
    }
}

fn extend_tags(target: &mut Vec<String>, extra: &[String]) {
    for tag in extra {
        if !target.iter().any(|existing| existing == tag) {
            target.push(tag.clone());
        }
    }
}

fn byte_range_to_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let pre = &source[..start];
    let start_line = pre.lines().count();
    let start_line = if start > 0 && pre.ends_with('\n') {
        start_line
    } else {
        start_line.saturating_sub(1)
    };

    let mid = &source[start..end];
    let new_lines = mid.chars().filter(|&ch| ch == '\n').count();
    let end_line = start_line + new_lines + usize::from(!mid.ends_with('\n'));

    (start_line, end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_top_level_maps_haskell_declarations() {
        let source = "module Demo where\n\nimport Data.List (intercalate)\n\ndata Mode = Fast | Safe\nnewtype WorkerId = WorkerId Int\ntype Rendered = String\n\nclass Renderable a where\n  render :: a -> Rendered\n  render value = show value\n\ninstance Renderable WorkerId where\n  render (WorkerId value) = show value\n\nformatWorker :: WorkerId -> [Int] -> Rendered\nformatWorker workerId values = intercalate \",\" (map show values)\n";
        let tree = parse_tree(source).unwrap();
        let root = tree.root_node();

        let blocks = split_top_level(root, source, Language::Haskell).unwrap();
        let kinds = blocks.iter().map(|block| block.kind).collect::<Vec<_>>();

        assert!(kinds.contains(&BlockKind::Module));
        assert!(kinds.contains(&BlockKind::Import));
        assert!(kinds.contains(&BlockKind::Type));
        assert!(kinds.contains(&BlockKind::Class));
        assert!(kinds.contains(&BlockKind::Impl));
        assert!(kinds.contains(&BlockKind::Function));
        assert!(
            !blocks.iter().any(|block| {
                block.kind == BlockKind::Function
                    && block.content.contains("render value = show value")
            }),
            "did not expect overlapping nested class member blocks at file level: {blocks:#?}"
        );
    }

    #[test]
    fn haskell_test_like_names_cover_specs_and_props() {
        for name in [
            "spec",
            "testWorker",
            "test_worker",
            "prop_roundTrip",
            "case_render",
        ] {
            assert!(is_test_like_name(name), "expected {name} to be test-like");
        }
        for name in ["specialCase", "helper", "property"] {
            assert!(
                !is_test_like_name(name),
                "did not expect {name} to be test-like"
            );
        }
    }

    #[test]
    fn split_top_level_pairs_signature_with_haddock_followed_function() {
        let source = "module Spec where\n\nspec :: Int\n-- | docs for spec\nspec = 1\n";
        let tree = parse_tree(source).unwrap();
        let root = tree.root_node();

        let blocks = split_top_level(root, source, Language::Haskell).unwrap();
        let spec_block = blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("spec :: Int")
            })
            .unwrap_or_else(|| panic!("expected grouped spec block: {blocks:#?}"));

        assert!(spec_block.content.contains("-- | docs for spec"));
        assert!(spec_block.content.contains("spec = 1"));
    }

    #[test]
    fn split_top_level_keeps_type_synonym_and_multi_clause_functions_grouped() {
        let source = "module Demo where\n\ntype Rendered = String\n\nfoo :: Int -> Int\nfoo 0 = 0\nfoo n = n + 1\n";
        let tree = parse_tree(source).unwrap();
        let root = tree.root_node();
        let blocks = split_top_level(root, source, Language::Haskell).unwrap();

        assert!(
            blocks.iter().any(|block| {
                block.kind == BlockKind::Type && block.content.contains("type Rendered = String")
            }),
            "expected type synonym block: {blocks:#?}"
        );

        let foo = blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("foo :: Int -> Int")
            })
            .unwrap_or_else(|| panic!("expected grouped multi-clause function block: {blocks:#?}"));
        assert!(foo.content.contains("foo 0 = 0"));
        assert!(foo.content.contains("foo n = n + 1"));
    }

    #[test]
    fn nested_block_comments_are_treated_as_comment_prefixes() {
        let chunk = "{- outer\n   {- inner -}\n-}\nbody = 1\n";
        assert!(!chunk_is_haskell_comment_only(chunk));
        let comment_end = leading_haskell_comment_prefix_len(chunk)
            .unwrap_or_else(|| panic!("expected comment prefix in {chunk:?}"));
        assert_eq!(&chunk[comment_end..], "body = 1\n");

        let signature_end = first_substantive_line_end(chunk)
            .unwrap_or_else(|| panic!("expected first substantive line in {chunk:?}"));
        assert_eq!(&chunk[signature_end..], "");
    }

    #[test]
    fn pragma_lines_do_not_break_top_level_haskell_splitting() {
        let source = "{-# LANGUAGE OverloadedStrings #-}\nmodule Demo where\n\nimport Data.Text (Text)\n\nrender :: Text\n{-# INLINE render #-}\nrender = \"demo\"\n";
        let tree = parse_tree(source).unwrap();
        let root = tree.root_node();

        let blocks = split_top_level(root, source, Language::Haskell).unwrap();
        let kinds = blocks.iter().map(|block| block.kind).collect::<Vec<_>>();

        assert!(kinds.contains(&BlockKind::Module));
        assert!(kinds.contains(&BlockKind::Import));
        assert!(kinds.contains(&BlockKind::Function));
        assert_eq!(
            blocks.first().map(|block| block.kind),
            Some(BlockKind::Comment)
        );

        let render = blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("render :: Text")
            })
            .unwrap_or_else(|| panic!("missing pragma-adjacent render block: {blocks:#?}"));
        let split = split_function_like_review_units(render)
            .unwrap_or_else(|error| panic!("split render with pragma: {error}"));
        assert_eq!(
            split.first().map(|block| block.kind),
            Some(BlockKind::FunctionSignature)
        );
    }

    #[test]
    fn byte_ranges_use_zero_based_line_spans() {
        let source = "module Demo where\n\nrender :: Int\nrender = 1\n";
        let tree = parse_tree(source).unwrap();
        let root = tree.root_node();
        let blocks = split_top_level(root, source, Language::Haskell).unwrap();

        let module_block = blocks
            .iter()
            .find(|block| block.kind == BlockKind::Module)
            .unwrap_or_else(|| panic!("missing module block: {blocks:#?}"));
        assert_eq!((module_block.start_line, module_block.end_line), (0, 1));

        let render_block = blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("render :: Int")
            })
            .unwrap_or_else(|| panic!("missing render block: {blocks:#?}"));
        assert_eq!((render_block.start_line, render_block.end_line), (2, 4));
    }

    #[test]
    fn function_review_units_keep_inline_pragmas_with_the_header_block() {
        let block = Block {
            hash: TreeHash::from_content(
                "render :: Int\n{-# INLINE render #-}\nrender = 1\n\nnext = 2\n",
            ),
            content: "render :: Int\n{-# INLINE render #-}\nrender = 1\n\nnext = 2\n".to_string(),
            kind: BlockKind::Function,
            tags: Vec::new(),
            complexity: None,
            start_line: 0,
            end_line: 8,
        };

        let split = split_function_like_review_units(&block)
            .unwrap_or_else(|error| panic!("split pragma-bearing function block: {error}"));
        assert_eq!(
            split.first().map(|block| block.kind),
            Some(BlockKind::FunctionSignature)
        );
        assert!(
            split
                .first()
                .is_some_and(|header| header.content.contains("{-# INLINE render #-}")),
            "expected pragma to stay with the header block: {split:#?}"
        );
    }

    #[test]
    fn function_review_units_without_type_signature_use_code_header() {
        let block = Block {
            hash: TreeHash::from_content("helper value =\n  value + 1\n\nhelperAgain = value\n"),
            content: "helper value =\n  value + 1\n\nhelperAgain = value\n".to_string(),
            kind: BlockKind::Function,
            tags: Vec::new(),
            complexity: None,
            start_line: 0,
            end_line: 8,
        };

        let split = split_function_like_review_units(&block)
            .unwrap_or_else(|error| panic!("split no-signature function block: {error}"));
        assert_eq!(
            split.first().map(|block| block.kind),
            Some(BlockKind::CodeParagraph)
        );
    }
}
