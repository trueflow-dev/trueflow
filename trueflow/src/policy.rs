use crate::block::Block;
use crate::config::BlockFilters;

pub fn should_skip_whitespace_only_by_default(block: &Block, filters: &BlockFilters) -> bool {
    block.content.trim().is_empty() && !filters.only_contains(block.kind)
}

pub fn should_skip_imports_by_default(path: &str, block: &Block, filters: &BlockFilters) -> bool {
    if block.kind.is_import_like() && !is_lib_rs(path) && !filters.only_contains(block.kind) {
        return true;
    }
    false
}

pub fn should_skip_container_by_default(
    is_container: bool,
    block: &Block,
    filters: &BlockFilters,
) -> bool {
    is_container && !filters.only_contains(block.kind)
}

fn is_lib_rs(path: &str) -> bool {
    path.ends_with("/lib.rs") || path == "lib.rs"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
    use crate::config::BlockFilters;

    fn block(kind: BlockKind, content: &str) -> Block {
        let line_span = LineSpan::new(0, content.lines().count());
        let byte_span = ByteSpan::new(0, content.len());
        Block::new(content.to_string(), kind, line_span, byte_span)
    }

    #[test]
    fn whitespace_only_blocks_are_skipped_by_default() {
        let filters = BlockFilters::default();

        assert!(should_skip_whitespace_only_by_default(
            &block(BlockKind::Paragraph, "\n  \t\n"),
            &filters
        ));
        assert!(!should_skip_whitespace_only_by_default(
            &block(BlockKind::Paragraph, "real content"),
            &filters
        ));
    }

    #[test]
    fn explicit_only_filter_can_review_whitespace_blocks() {
        let filters = BlockFilters::from_lists(&[BlockKind::Paragraph], &[]);

        assert!(!should_skip_whitespace_only_by_default(
            &block(BlockKind::Paragraph, "\n\n"),
            &filters
        ));
    }
}
