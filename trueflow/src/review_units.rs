use crate::block::Block;

pub const MAX_REVIEW_UNIT_SPAN_LINES: usize = 32;

pub fn block_line_span(block: &Block) -> usize {
    block.end_line.saturating_sub(block.start_line)
}
