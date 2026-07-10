use crate::block::{Block, BlockKind, ByteSpan};
use crate::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use std::mem;

const MAX_IMPORT_SPAN_LINES: usize = 24;
const MAX_IMPORT_GAP_LINES: usize = 3;
const MAX_CODE_PARAGRAPH_SPAN_LINES: usize = MAX_REVIEW_UNIT_SPAN_LINES;
const SMALL_FILE_MAX_SPAN_LINES: usize = MAX_REVIEW_UNIT_SPAN_LINES;
const SMALL_FILE_MAX_NON_TRIVIAL_BLOCKS: usize = 12;

pub fn optimize(blocks: Vec<Block>, source: &str) -> Vec<Block> {
    let blocks = optimize_imports(blocks, source);
    let blocks = optimize_modules(blocks, source);
    let blocks = optimize_code_paragraphs(blocks, source);
    optimize_small_files(blocks, source)
}

fn optimize_imports(blocks: Vec<Block>, source: &str) -> Vec<Block> {
    optimize_sequence(
        blocks,
        |block, buffer| {
            if block.kind == BlockKind::Import {
                let previous_import_end = buffer
                    .iter()
                    .rev()
                    .find(|candidate| candidate.kind == BlockKind::Import)
                    .map(|candidate| candidate.end_line)
                    .unwrap_or(block.start_line);
                let line_gap = block.start_line.saturating_sub(previous_import_end);
                if line_gap > MAX_IMPORT_GAP_LINES {
                    return Decision::FlushAndBuffer;
                }

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
                if block.line_span().len() > MAX_IMPORT_GAP_LINES {
                    return Decision::FlushAndEmit;
                }
                return Decision::Buffer;
            }

            if block.kind == BlockKind::Comment && !buffer.is_empty() {
                return Decision::Buffer;
            }

            Decision::FlushAndEmit
        },
        |buffer| flush_blocks(buffer, BlockKind::Import, BlockKind::Imports, source),
    )
}

fn optimize_code_paragraphs(blocks: Vec<Block>, source: &str) -> Vec<Block> {
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

            if size > MAX_CODE_PARAGRAPH_SPAN_LINES {
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
                source,
            )
        },
    )
}

fn optimize_modules(blocks: Vec<Block>, source: &str) -> Vec<Block> {
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
        |buffer| flush_blocks(buffer, BlockKind::Module, BlockKind::Modules, source),
    )
}

fn optimize_small_files(blocks: Vec<Block>, source: &str) -> Vec<Block> {
    if !should_merge_small_file(&blocks) {
        return blocks;
    }

    let merged_kind = merged_small_file_kind(&blocks);
    exact_source_merge(source, &blocks, merged_kind).map_or(blocks, |merged| vec![merged])
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
    source: &str,
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

    let range = &buffer[first_idx..=last_idx];
    let Some(merged_block) = exact_source_merge(source, range, merged_kind) else {
        return buffer;
    };

    let mut result = Vec::with_capacity(buffer.len() - (last_idx - first_idx));
    result.extend(buffer.iter().take(first_idx).cloned());
    result.push(merged_block);
    result.extend(buffer.iter().skip(last_idx + 1).cloned());
    result
}

/// Merges a source-ordered, pairwise-disjoint sequence only when every input is
/// proven to be an exact UTF-8 slice of `source`.
fn exact_source_merge(source: &str, blocks: &[Block], merged_kind: BlockKind) -> Option<Block> {
    let first = blocks.first()?;
    let mut previous_end = None;

    for block in blocks {
        let start = block.start_byte;
        let end = block.end_byte;
        if start > end
            || end > source.len()
            || !source.is_char_boundary(start)
            || !source.is_char_boundary(end)
            || (!block.content.is_empty() && start == end)
            || source.get(start..end)? != block.content.as_str()
        {
            return None;
        }
        if previous_end.is_some_and(|previous_end| previous_end > start) {
            return None;
        }
        previous_end = Some(end);
    }

    let end = previous_end?;
    let mut merged =
        Block::from_file_range(source, merged_kind, ByteSpan::new(first.start_byte, end)).ok()?;
    let (tags, complexity) = merged_metadata(blocks);
    merged.tags = tags;
    merged.complexity = complexity;
    Some(merged)
}

fn merged_metadata(blocks: &[Block]) -> (Vec<String>, Option<u32>) {
    let mut tags = Vec::new();
    let mut complexity = None;

    for block in blocks {
        for tag in &block.tags {
            if !tags.iter().any(|existing| existing == tag) {
                tags.push(tag.clone());
            }
        }

        if let Some(block_complexity) = block.complexity {
            complexity = Some(complexity.unwrap_or(0_u32).saturating_add(block_complexity));
        }
    }

    (tags, complexity)
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

    if non_trivial_blocks.len() >= 3
        && non_trivial_blocks
            .iter()
            .all(|block| is_small_file_declarative_kind(block.kind))
    {
        return false;
    }

    let logic_block_count = non_trivial_blocks
        .iter()
        .filter(|block| is_small_file_logic_kind(block.kind))
        .count();
    if logic_block_count > 1 {
        return false;
    }

    let start_line = non_trivial_blocks
        .first()
        .map_or(0, |block| block.start_line);
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

fn is_small_file_declarative_kind(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Import
            | BlockKind::Imports
            | BlockKind::Variable
            | BlockKind::Const
            | BlockKind::Static
            | BlockKind::FunctionSignature
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

fn is_small_file_logic_kind(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Function
            | BlockKind::Method
            | BlockKind::Impl
            | BlockKind::Macro
            | BlockKind::Command
            | BlockKind::Code
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, ByteSpan, LineSpan};

    fn make_block(kind: BlockKind, content: &str, start: usize, end: usize) -> Block {
        Block::new(
            content.to_string(),
            kind,
            LineSpan::new(start, end),
            ByteSpan::new(0, content.len()),
        )
    }

    fn optimize_test_blocks(mut blocks: Vec<Block>) -> Vec<Block> {
        let capacity = blocks.iter().map(|block| block.content.len()).sum();
        let mut source = String::with_capacity(capacity);
        for block in &mut blocks {
            block.start_byte = source.len();
            source.push_str(&block.content);
            block.end_byte = source.len();
        }
        optimize(blocks, &source)
    }

    fn source_block(source: &str, kind: BlockKind, start_byte: usize, end_byte: usize) -> Block {
        Block::from_file_range(source, kind, ByteSpan::new(start_byte, end_byte))
            .unwrap_or_else(|error| panic!("valid source block: {error}"))
    }

    fn assert_blocks_unchanged(actual: &[Block], expected: &[Block]) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.hash, expected.hash);
            assert_eq!(actual.content, expected.content);
            assert_eq!(actual.kind, expected.kind);
            assert_eq!(actual.tags, expected.tags);
            assert_eq!(actual.complexity, expected.complexity);
            assert_eq!(
                (actual.start_line, actual.end_line),
                (expected.start_line, expected.end_line)
            );
            assert_eq!(
                (actual.start_byte, actual.end_byte),
                (expected.start_byte, expected.end_byte)
            );
        }
    }

    #[test]
    fn test_merge_small_paragraphs() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 2), // 2 lines
            make_block(BlockKind::Gap, "\n", 2, 3),             // 1 line
            make_block(BlockKind::CodeParagraph, "P2\n", 3, 5), // 2 lines
        ];
        // Total span: 5 - 0 = 5 lines. Should merge.

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[0].content, "P1\n\nP2\n");
    }

    #[test]
    fn test_merge_code_paragraphs_up_to_small_file_threshold() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 16),
            make_block(BlockKind::Gap, "\n", 16, 17),
            make_block(BlockKind::CodeParagraph, "P2\n", 17, 32),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[0].content, "P1\n\nP2\n");
    }

    #[test]
    fn test_dont_merge_large_paragraphs() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 16),
            make_block(BlockKind::Gap, "\n", 16, 17),
            make_block(BlockKind::CodeParagraph, "P2\n", 17, 33),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::CodeParagraph);
    }

    #[test]
    fn test_small_paragraph_files_collapse_to_single_review_block() {
        let blocks = vec![
            make_block(BlockKind::Paragraph, "P1\n", 0, 10),
            make_block(BlockKind::Gap, "\n", 10, 11),
            make_block(BlockKind::Paragraph, "P2\n", 11, 20),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::Paragraph);
        assert_eq!(optimized[0].content, "P1\n\nP2\n");
    }

    #[test]
    fn test_merge_sequence_greedy() {
        let blocks = vec![
            make_block(BlockKind::CodeParagraph, "P1\n", 0, 1), // 1 line
            make_block(BlockKind::Gap, "\n", 1, 2),             // 1 line
            make_block(BlockKind::CodeParagraph, "P2\n", 2, 3), // 1 line
            // Span 0..3 = 3 lines. Merge P1+Gap+P2.
            make_block(BlockKind::Gap, "\n", 3, 70),
            make_block(BlockKind::CodeParagraph, "P3\n", 70, 71), // 1 line
        ];
        // Adding P3: Span 0..71 = 71 lines. Too big.
        // Should flush P1+Gap+P2. Then emit the large gap. Then buffer P3.

        let optimized = optimize_test_blocks(blocks);
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
        module_a.complexity = Some(2);

        let mut module_b = make_block(BlockKind::Module, "mod helper {\n}\n", 3, 5);
        module_b.tags = vec!["test".to_string(), "integration".to_string()];
        module_b.complexity = Some(3);

        let blocks = vec![module_a, make_block(BlockKind::Gap, "\n", 2, 3), module_b];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::Modules);
        assert_eq!(
            optimized[0].tags,
            vec!["test".to_string(), "integration".to_string()]
        );
        assert_eq!(optimized[0].complexity, Some(5));
    }

    #[test]
    fn test_merge_keeps_complexity_unknown_when_no_inputs_have_scores() {
        let blocks = vec![
            make_block(BlockKind::Module, "mod tests {\n}\n", 0, 2),
            make_block(BlockKind::Gap, "\n", 2, 3),
            make_block(BlockKind::Module, "mod helper {\n}\n", 3, 5),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].complexity, None);
    }

    #[test]
    fn test_merge_imports_with_inline_comment() {
        let blocks = vec![
            make_block(BlockKind::Import, "use a;", 0, 1),
            make_block(BlockKind::Comment, "\n// grouped import note\n", 1, 3),
            make_block(BlockKind::Import, "use b;", 3, 4),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::Imports);
        assert_eq!(
            optimized[0].content,
            "use a;\n// grouped import note\nuse b;"
        );
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

        let optimized = optimize_test_blocks(blocks);
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

        let optimized = optimize_test_blocks(blocks);
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

        let optimized = optimize_test_blocks(blocks);
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
            make_block(BlockKind::Gap, "\n", 3, 70),
            make_block(BlockKind::CodeParagraph, "P3\n", 70, 71),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 3);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[1].kind, BlockKind::Gap);
        assert_eq!(optimized[2].kind, BlockKind::CodeParagraph);
    }

    #[test]
    fn test_small_file_pass_does_not_merge_multiple_logic_blocks() {
        let blocks = vec![
            make_block(BlockKind::Import, "use std::fmt;\n", 0, 1),
            make_block(BlockKind::Gap, "\n", 1, 2),
            make_block(BlockKind::Function, "fn a() {}\n", 2, 3),
            make_block(BlockKind::Gap, "\n", 3, 4),
            make_block(BlockKind::Function, "fn b() {}\n", 4, 5),
        ];

        let optimized = optimize_test_blocks(blocks);
        assert_eq!(optimized.len(), 5);
        assert_eq!(optimized[0].kind, BlockKind::Import);
        assert_eq!(optimized[2].kind, BlockKind::Function);
        assert_eq!(optimized[4].kind, BlockKind::Function);
    }
    #[test]
    fn test_small_file_pass_does_not_merge_declarative_block_sets() {
        let blocks = vec![
            make_block(BlockKind::FunctionSignature, "{ pkgs }:\n", 0, 1),
            make_block(BlockKind::Variable, "let\n  foo = 1;\n", 1, 3),
            make_block(BlockKind::Gap, "\n", 3, 4),
            make_block(BlockKind::Variable, "  bar = 2;\n", 4, 5),
            make_block(BlockKind::Import, "in { inherit foo bar; }\n", 5, 6),
        ];

        let optimized = optimize_test_blocks(blocks.clone());
        assert_eq!(optimized.len(), blocks.len());
        assert_eq!(optimized[0].kind, BlockKind::FunctionSignature);
        assert_eq!(optimized[1].kind, BlockKind::Variable);
        assert_eq!(optimized[3].kind, BlockKind::Variable);
        assert_eq!(optimized[4].kind, BlockKind::Import);
    }

    #[test]
    fn test_source_integrity_merge_slices_disjoint_source_and_metadata() {
        let source = "use first;\n\nuse second;";
        let first_end = source.find('\n').unwrap();
        let second_start = source.find("use second;").unwrap();
        let mut first = source_block(source, BlockKind::Import, 0, first_end);
        first.tags = vec!["test".to_string(), "shared".to_string()];
        first.complexity = Some(2);
        let mut second = source_block(
            source,
            BlockKind::Import,
            second_start,
            second_start + "use second;".len(),
        );
        second.tags = vec!["shared".to_string(), "integration".to_string()];
        second.complexity = Some(5);

        let Some(merged) = exact_source_merge(source, &[first, second], BlockKind::Imports) else {
            panic!("disjoint source blocks should merge");
        };

        assert_eq!(merged.content, source);
        assert_eq!(merged.hash, crate::hashing::TreeHash::from_content(source));
        assert_eq!((merged.start_byte, merged.end_byte), (0, source.len()));
        assert_eq!((merged.start_line, merged.end_line), (0, 3));
        assert_eq!(
            merged.tags,
            vec![
                "test".to_string(),
                "shared".to_string(),
                "integration".to_string()
            ]
        );
        assert_eq!(merged.complexity, Some(7));
    }

    #[test]
    fn test_source_integrity_merge_refuses_unprovable_candidate() {
        let source = "abcd";
        let first = source_block(source, BlockKind::Import, 0, 2);
        let second = source_block(source, BlockKind::Import, 2, 4);

        let mut reversed_first = first.clone();
        let mut reversed_second = second.clone();
        reversed_first.start_byte = 2;
        reversed_first.end_byte = 4;
        reversed_first.content = source[2..4].to_string();
        reversed_first.hash = crate::hashing::TreeHash::from_content(&reversed_first.content);
        reversed_second.start_byte = 0;
        reversed_second.end_byte = 2;
        reversed_second.content = source[0..2].to_string();
        reversed_second.hash = crate::hashing::TreeHash::from_content(&reversed_second.content);

        let mut overlapping = second.clone();
        overlapping.start_byte = 1;
        overlapping.end_byte = 3;
        overlapping.content = source[1..3].to_string();
        overlapping.hash = crate::hashing::TreeHash::from_content(&overlapping.content);

        let mut backwards = first.clone();
        backwards.start_byte = 3;
        backwards.end_byte = 2;

        let mut out_of_bounds = second.clone();
        out_of_bounds.end_byte = source.len() + 1;

        let unicode_source = "éx";
        let mut non_boundary = source_block(unicode_source, BlockKind::Import, 0, 2);
        non_boundary.end_byte = 1;
        let unicode_tail = source_block(unicode_source, BlockKind::Import, 2, 3);

        let mut empty_non_empty = first.clone();
        empty_non_empty.end_byte = empty_non_empty.start_byte;

        let cases = vec![
            (source, vec![reversed_first, reversed_second]),
            (source, vec![first.clone(), overlapping]),
            (source, vec![backwards, second.clone()]),
            (source, vec![first, out_of_bounds]),
            (unicode_source, vec![non_boundary, unicode_tail]),
            (source, vec![empty_non_empty, second]),
        ];
        for (case_source, blocks) in cases {
            let result = flush_blocks(
                blocks.clone(),
                BlockKind::Import,
                BlockKind::Imports,
                case_source,
            );
            assert_blocks_unchanged(&result, &blocks);
        }
    }

    #[test]
    fn test_source_integrity_flush_refuses_overlapping_whole_buffer() {
        let source = "// lead\nuse a;\nuse b;\n// tail";
        let first_import_start = source.find("use a;").unwrap();
        let second_import_start = source.find("use b;").unwrap();
        let mut second = source_block(
            source,
            BlockKind::Import,
            second_import_start,
            second_import_start + "use b;".len(),
        );
        second.start_byte = first_import_start + "use a".len();
        let blocks = vec![
            source_block(source, BlockKind::Comment, 0, first_import_start),
            source_block(
                source,
                BlockKind::Import,
                first_import_start,
                first_import_start + "use a;".len(),
            ),
            second,
            source_block(
                source,
                BlockKind::Comment,
                source.find("// tail").unwrap(),
                source.len(),
            ),
        ];

        let result = flush_blocks(
            blocks.clone(),
            BlockKind::Import,
            BlockKind::Imports,
            source,
        );
        assert_blocks_unchanged(&result, &blocks);
    }

    #[test]
    fn test_source_integrity_small_file_refuses_overlapping_whole_vector() {
        let source = "import os\nx = 1";
        let mut code = source_block(source, BlockKind::Code, 10, source.len());
        code.start_byte = 8;
        let blocks = vec![source_block(source, BlockKind::Import, 0, 9), code];

        let result = optimize_small_files(blocks.clone(), source);
        assert_blocks_unchanged(&result, &blocks);
    }

    #[test]
    fn test_source_integrity_code_paragraph_merge_slices_source() {
        let source = "first\n\nsecond";
        let second_start = source.find("second").unwrap();
        let blocks = vec![
            source_block(source, BlockKind::CodeParagraph, 0, "first".len()),
            source_block(source, BlockKind::CodeParagraph, second_start, source.len()),
        ];

        let optimized = optimize(blocks, source);
        assert_eq!(optimized.len(), 1);
        assert_eq!(optimized[0].kind, BlockKind::CodeParagraph);
        assert_eq!(optimized[0].content, source);
        assert_eq!(
            (optimized[0].start_byte, optimized[0].end_byte),
            (0, source.len())
        );
        assert_eq!(
            optimized[0].hash,
            crate::hashing::TreeHash::from_content(source)
        );
    }
}
