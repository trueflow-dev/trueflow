use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
use crate::hashing::TreeHash;
use serde::{Deserialize, Serialize};

pub const MAX_REVIEW_UNIT_SPAN_LINES: usize = 32;
pub const MAX_MARKDOWN_REVIEW_UNIT_SPAN_LINES: usize = 50;

/// Compact representation of a generated review unit without duplicating the
/// parent block's source text or tag vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewUnitSpan {
    pub hash: TreeHash,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: BlockKind,
    pub is_blank: bool,
}

impl ReviewUnitSpan {
    pub fn from_block(block: &Block) -> Self {
        Self {
            hash: block.hash.clone(),
            start_byte: block.start_byte,
            end_byte: block.end_byte,
            start_line: block.start_line,
            end_line: block.end_line,
            kind: block.kind,
            is_blank: block.content.trim().is_empty(),
        }
    }

    pub fn byte_span(&self) -> ByteSpan {
        ByteSpan::new(self.start_byte, self.end_byte)
    }

    pub fn line_span(&self) -> LineSpan {
        LineSpan::new(self.start_line, self.end_line)
    }

    pub fn to_block_from_parent(&self, parent: &Block) -> Option<Block> {
        let rel_start = self.start_byte.checked_sub(parent.start_byte)?;
        let rel_end = self.end_byte.checked_sub(parent.start_byte)?;
        let content = parent.content.get(rel_start..rel_end)?;
        Some(Block {
            hash: self.hash.clone(),
            content: content.to_owned(),
            kind: self.kind,
            tags: parent.tags.clone(),
            complexity: None,
            start_line: self.start_line,
            end_line: self.end_line,
            start_byte: self.start_byte,
            end_byte: self.end_byte,
        })
    }
}
