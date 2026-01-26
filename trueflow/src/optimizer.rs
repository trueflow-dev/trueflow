use crate::block::{Block, BlockKind};
use std::mem;

pub fn optimize(blocks: Vec<Block>) -> Vec<Block> {
    let blocks = optimize_imports(blocks);
    optimize_code_paragraphs(blocks)
}

fn optimize_imports(blocks: Vec<Block>) -> Vec<Block> {
    optimize_sequence(
        blocks,
        |block, buffer| {
            if block.kind == BlockKind::Import
                || (block.kind == BlockKind::Gap && !buffer.is_empty())
            {
                Decision::Buffer
            } else {
                Decision::FlushAndEmit
            }
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

    let first_idx = buffer.iter().position(|b| b.kind == target_kind).unwrap();
    let last_idx = buffer.iter().rposition(|b| b.kind == target_kind).unwrap();

    let mut result = Vec::with_capacity(buffer.len() - (last_idx - first_idx));

    // Emit leading gaps
    result.extend(buffer.iter().take(first_idx).cloned());

    // Merge range
    let range = &buffer[first_idx..=last_idx];
    let start_line = range[0].start_line;
    let end_line = range.last().unwrap().end_line;

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

    let merged_block = Block::new(content, merged_kind, start_line, end_line);
    result.push(merged_block);

    // Emit trailing gaps
    result.extend(buffer.iter().skip(last_idx + 1).cloned());

    result
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
}
