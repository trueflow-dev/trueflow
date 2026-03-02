use crate::block::{Block, BlockKind};
use std::mem;

const MAX_IMPORT_SPAN_LINES: usize = 24;
const MAX_IMPORT_GAP_LINES: usize = 3;
const SMALL_FILE_MAX_SPAN_LINES: usize = 64;
const SMALL_FILE_MAX_NON_TRIVIAL_BLOCKS: usize = 12;

pub fn optimize(blocks: Vec<Block>) -> Vec<Block> {
    let blocks = optimize_imports(blocks);
    let blocks = optimize_modules(blocks);
    let blocks = optimize_code_paragraphs(blocks);
    optimize_small_files(blocks)
}

fn optimize_imports(blocks: Vec<Block>) -> Vec<Block> {
    optimize_sequence(
        blocks,
        |block, buffer| {
            if block.kind == BlockKind::Import {
                let start_line = buffer
                    .iter()
                    .find(|candidate| candidate.kind == BlockKind::Import)
                    .map(|candidate| candidate.start_line)
                    .unwrap_or(block.start_line);
                let span = block.end_line.saturating_sub(start_line);
                if span > MAX_IMPORT_SPAN_LINES {
                    return Decision::FlushAndBuffer;
                }
                return Decision::Buffer;
            }

            if block.kind == BlockKind::Gap {
                if buffer.is_empty() {
                    return Decision::FlushAndEmit;
                }
                if line_span(block) > MAX_IMPORT_GAP_LINES {
                    return Decision::FlushAndEmit;
                }
                return Decision::Buffer;
            }

            if block.kind == BlockKind::Comment && !buffer.is_empty() {
                return Decision::Buffer;
            }

            Decision::FlushAndEmit
        },
        |buffer| flush_blocks(buffer, BlockKind::Import, BlockKind::Imports, Some("\n")),
    )
}

fn optimize_code_paragraphs(blocks: Vec<Block>) -> Vec<Block> {
    optimize_sequence(
        blocks,
        |block, buffer| {
            if !matches!(block.kind, BlockKind::CodeParagraph | BlockKind::Gap) {
                return Decision::FlushAndEmit;
            }

            if block.kind == BlockKind::Gap {
                return Decision::Buffer;
            }

            // It is CodeParagraph. Check if adding it would exceed the limit.
            let start_line = buffer
                .iter()
                .find(|b| b.kind == BlockKind::CodeParagraph)
                .map(|b| b.start_line)
                .unwrap_or(block.start_line);
            let end_line = block.end_line;
            let size = end_line.saturating_sub(start_line);

            if size > 8 {
                Decision::FlushAndBuffer
            } else {
                Decision::Buffer
            }
        },
        |buffer| {
            flush_blocks(
                buffer,
                BlockKind::CodeParagraph,
                BlockKind::CodeParagraph,
                None,
            )
        },
    )
}

fn optimize_modules(blocks: Vec<Block>) -> Vec<Block> {
    optimize_sequence(
        blocks,
        |block, buffer| {
            if !matches!(
                block.kind,
                BlockKind::Module | BlockKind::Gap | BlockKind::Comment
            ) {
                return Decision::FlushAndEmit;
            }

            if matches!(block.kind, BlockKind::Gap | BlockKind::Comment) {
                return Decision::Buffer;
            }

            let start_line = buffer
                .iter()
                .find(|b| b.kind == BlockKind::Module)
                .map(|b| b.start_line)
                .unwrap_or(block.start_line);
            let end_line = block.end_line;
            let size = end_line.saturating_sub(start_line);

            if size > 48 {
                Decision::FlushAndBuffer
            } else {
                Decision::Buffer
            }
        },
        |buffer| flush_blocks(buffer, BlockKind::Module, BlockKind::Modules, None),
    )
}

fn optimize_small_files(blocks: Vec<Block>) -> Vec<Block> {
    if !should_merge_small_file(&blocks) {
        return blocks;
    }

    let mut content = String::new();
    for block in &blocks {
        content.push_str(&block.content);
    }

    let start_line = blocks.first().map_or(0, |block| block.start_line);
    let end_line = blocks.last().map_or(start_line, |block| block.end_line);
    let merged_kind = merged_small_file_kind(&blocks);
    let mut merged = Block::new(content, merged_kind, start_line, end_line);
    let (tags, complexity) = merged_metadata(&blocks);
    merged.tags = tags;
    merged.complexity = complexity;
    vec![merged]
}

enum Decision {
    Buffer,
    FlushAndBuffer,
    FlushAndEmit,
}

fn optimize_sequence<F>(
    blocks: Vec<Block>,
    mut decider: F,
    flusher: impl Fn(Vec<Block>) -> Vec<Block>,
) -> Vec<Block>
where
    F: FnMut(&Block, &Vec<Block>) -> Decision,
{
    let mut optimized = Vec::with_capacity(blocks.len());
    let mut buffer = Vec::new();

    for block in blocks {
        match decider(&block, &buffer) {
            Decision::Buffer => buffer.push(block),
            Decision::FlushAndBuffer => {
                if !buffer.is_empty() {
                    optimized.extend(flusher(mem::take(&mut buffer)));
                }
                buffer.push(block);
            }
            Decision::FlushAndEmit => {
                if !buffer.is_empty() {
                    optimized.extend(flusher(mem::take(&mut buffer)));
                }
                optimized.push(block);
            }
        }
    }

    if !buffer.is_empty() {
        optimized.extend(flusher(buffer));
    }

    optimized
}

fn flush_blocks(
    buffer: Vec<Block>,
    target_kind: BlockKind,
    merged_kind: BlockKind,
    separator: Option<&str>,
) -> Vec<Block> {
    let target_count = buffer.iter().filter(|b| b.kind == target_kind).count();
    if target_count < 2 {
        return buffer;
    }

    let Some(first_idx) = buffer.iter().position(|b| b.kind == target_kind) else {
        return buffer;
    };
    let Some(last_idx) = buffer.iter().rposition(|b| b.kind == target_kind) else {
        return buffer;
    };

    let mut result = Vec::with_capacity(buffer.len() - (last_idx - first_idx));

    // Emit leading gaps
    result.extend(buffer.iter().take(first_idx).cloned());

    // Merge range
    let range = &buffer[first_idx..=last_idx];
    let start_line = buffer[first_idx].start_line;
    let end_line = buffer[last_idx].end_line;

    let mut content = String::new();
    let mut prev_was_target = false;

    for block in range {
        if let Some(sep) = separator
            && prev_was_target
            && block.kind == target_kind
        {
            content.push_str(sep);
        }
        content.push_str(&block.content);
        prev_was_target = block.kind == target_kind;
    }

    let mut merged_block = Block::new(content, merged_kind, start_line, end_line);
    let (merged_tags, merged_complexity) = merged_metadata(range);
    merged_block.tags = merged_tags;
    merged_block.complexity = merged_complexity;
    result.push(merged_block);

    // Emit trailing gaps
    result.extend(buffer.iter().skip(last_idx + 1).cloned());

    result
}

fn merged_metadata(blocks: &[Block]) -> (Vec<String>, u32) {
    let mut tags = Vec::new();
    let mut complexity = 0_u32;

    for block in blocks {
        for tag in &block.tags {
            if !tags.iter().any(|existing| existing == tag) {
                tags.push(tag.clone());
            }
        }
        complexity = complexity.saturating_add(block.complexity);
    }

    (tags, complexity)
}

fn line_span(block: &Block) -> usize {
    block.end_line.saturating_sub(block.start_line)
}

fn should_merge_small_file(blocks: &[Block]) -> bool {
    if blocks.len() < 2 {
        return false;
    }

    let non_trivial_blocks: Vec<&Block> = blocks
        .iter()
        .filter(|block| !matches!(block.kind, BlockKind::Gap | BlockKind::Comment))
        .collect();

    if non_trivial_blocks.len() < 2 || non_trivial_blocks.len() > SMALL_FILE_MAX_NON_TRIVIAL_BLOCKS
    {
        return false;
    }

    if non_trivial_blocks
        .iter()
        .any(|block| is_non_collapsible_small_file_kind(block.kind))
    {
        return false;
    }

    if non_trivial_blocks.iter().all(|block| {
        matches!(
            block.kind,
            BlockKind::Import | BlockKind::Imports | BlockKind::Module | BlockKind::Modules
        )
    }) {
        return false;
    }

    let start_line = non_trivial_blocks.first().map_or(0, |block| block.start_line);
    let end_line = non_trivial_blocks
        .last()
        .map_or(start_line, |block| block.end_line);
    let span = end_line.saturating_sub(start_line);
    span <= SMALL_FILE_MAX_SPAN_LINES
}

fn is_non_collapsible_small_file_kind(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::TextBlock
            | BlockKind::CodeParagraph
            | BlockKind::Header
            | BlockKind::Paragraph
            | BlockKind::CodeBlock
            | BlockKind::List
            | BlockKind::ListItem
            | BlockKind::Quote
            | BlockKind::Element
            | BlockKind::Content
            | BlockKind::Sentence
            | BlockKind::Section
            | BlockKind::Preamble
    )
}

fn merged_small_file_kind(blocks: &[Block]) -> BlockKind {
    let non_trivial: Vec<BlockKind> = blocks
        .iter()
        .filter_map(|block| {
            if matches!(block.kind, BlockKind::Gap | BlockKind::Comment) {
                None
            } else {
                Some(block.kind)
            }
        })
        .collect();

    let Some(first_kind) = non_trivial.first().copied() else {
        return BlockKind::Code;
    };

    if non_trivial.iter().all(|kind| *kind == first_kind) {
        first_kind
    } else {
        BlockKind::Code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;

    fn make_block(kind: BlockKind, content: &str, start: usize, end: usize) -> Block {
        Block::new(content.to_string(), kind, start, end)
    }

    #[test]
    fn test_merge_small_paragraphs() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 2), // 2 lines
            make_block(BlockKind::Gap, "\n", 2, 3),             // 1 line
            make_block(BlockKind::CodeParagraph, "P2\n", 3, 5), // 2 lines
        ];
        // Total span: 5 - 0 = 5 lines. Should merge.

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[0].content, "P1\n\nP2\n");
    }

    #[test]
    fn test_dont_merge_large_paragraphs() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\nP1\nP1\nP1\n", 0, 4), // 4 lines
            make_block(BlockKind::Gap, "\n", 4, 5),                         // 1 line
            make_block(BlockKind::CodeParagraph, "P2\nP2\nP2\nP2\n", 5, 9), // 4 lines
        ];
        // Total span: 9 - 0 = 9 lines. Should NOT merge.

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::CodeParagraph);
    }

    #[test]
    fn test_merge_sequence_greedy() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 1), // 1 line
            make_block(BlockKind::Gap, "\n", 1, 2),             // 1 line
            make_block(BlockKind::CodeParagraph, "P2\n", 2, 3), // 1 line
            // Span 0..3 = 3 lines. Merge P1+Gap+P2.
            make_block(BlockKind::Gap, "\n\n\n\n\n\n", 3, 9), // 6 lines
            make_block(BlockKind::CodeParagraph, "P3\n", 9, 10), // 1 line
        ];
        // Adding P3: Span 0..10 = 10 lines. Too big.
        // Should flush P1+Gap+P2. Then emit Gap(6). Then buffer P3.

        let optimized = optimize(blocks);
        // P1+Gap+P2 merged = 1 block.
        // Gap(6) = 1 block.
        // P3 = 1 block.
        // Total 3 blocks.
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[0].content, "P1\n\nP2\n");
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[2].content, "P3\n");
    }

    #[test]
    fn test_merge_preserves_union_tags_and_complexity() {
        let mut module_a = make_block(BlockKind::Module, "mod tests {\n}\n", 0, 2);
        module_a.tags = vec!["test".to_string()];
        module_a.complexity = 2;

        let mut module_b = make_block(BlockKind::Module, "mod helper {\n}\n", 3, 5);
        module_b.tags = vec!["test".to_string(), "integration".to_string()];
        module_b.complexity = 3;

        let blocks = vec![module_a, make_block(BlockKind::Gap, "\n", 2, 3), module_b];

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::Modules);
        assert_eq!(
            optimized[0].tags,
            vec!["test".to_string(), "integration".to_string()]
        );
        assert_eq!(optimized[0].complexity, 5);
    }

    #[test]
    fn test_merge_imports_with_inline_comment() {
        let blocks = vec![
            make_block(BlockKind::Import, "use a;", 0, 1),
            make_block(BlockKind::Comment, "\n// grouped import note\n", 1, 3),
            make_block(BlockKind::Import, "use b;", 3, 4),
        ];

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::Imports);
        assert_eq!(optimized[0].content, "use a;\n// grouped import note\nuse b;");
    }

    #[test]
    fn test_import_merge_respects_large_gap_boundary() {
        let blocks = vec![
            make_block(BlockKind::Import, "use a;", 0, 1),
            make_block(BlockKind::Gap, "\n", 1, 2),
            make_block(BlockKind::Import, "use b;", 2, 3),
            make_block(BlockKind::Gap, "\n\n\n\n\n", 3, 8),
            make_block(BlockKind::Import, "use c;", 8, 9),
        ];

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::Imports);
        assert_eq!(optimized[0].content, "use a;\nuse b;");
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::Import);
        assert_eq!(optimized[2].content, "use c;");
    }

    #[test]
    fn test_small_file_collapses_mixed_semantic_blocks() {
        let blocks = vec![
            make_block(BlockKind::Import, "use std::fmt;\n", 0, 1),
            make_block(BlockKind::Gap, "\n", 1, 2),
            make_block(BlockKind::Function, "fn run() {}\n", 2, 3),
            make_block(BlockKind::Gap, "\n", 3, 4),
            make_block(BlockKind::Const, "const LIMIT: usize = 3;\n", 4, 5),
        ];

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::Code);
        assert_eq!(
            optimized[0].content,
            "use std::fmt;\n\nfn run() {}\n\nconst LIMIT: usize = 3;\n"
        );
    }

    #[test]
    fn test_small_file_pass_does_not_collapse_large_span() {
        let blocks = vec![
            make_block(BlockKind::Function, "fn a() {}\n", 0, 1),
            make_block(BlockKind::Gap, "\n", 1, 2),
            make_block(BlockKind::Function, "fn b() {}\n", 70, 71),
        ];

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::Function);
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::Function);
    }

    #[test]
    fn test_small_file_pass_does_not_override_code_paragraph_strategy() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 1),
            make_block(BlockKind::Gap, "\n", 1, 2),
            make_block(BlockKind::CodeParagraph, "P2\n", 2, 3),
            make_block(BlockKind::Gap, "\n\n\n\n\n\n", 3, 9),
            make_block(BlockKind::CodeParagraph, "P3\n", 9, 10),
        ];

        let optimized = optimize(blocks);
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::CodeParagraph);
    }
}
