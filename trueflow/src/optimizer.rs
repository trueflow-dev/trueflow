use crate::block::{Block, BlockKind};
use crate::hashing::hash_str;

pub fn optimize(blocks: Vec<Block>) -> Vec<Block> {
    let mut optimized = Vec::new();
    let mut import_buffer: Vec<Block> = Vec::new();

    for block in blocks {
        if block.kind == BlockKind::Import
            || (block.kind == BlockKind::Gap && !import_buffer.is_empty())
        {
            import_buffer.push(block);
        } else {
            // Flush imports
            if !import_buffer.is_empty() {
                optimized.extend(flush_imports(import_buffer));
                import_buffer = Vec::new();
            }
            optimized.push(block);
        }
    }

    // Final flush
    if !import_buffer.is_empty() {
        optimized.extend(flush_imports(import_buffer));
    }

    optimized
}

fn flush_imports(buffer: Vec<Block>) -> Vec<Block> {
    let import_count = buffer
        .iter()
        .filter(|b| b.kind == BlockKind::Import)
        .count();

    if import_count < 2 {
        return buffer;
    }

    let first_idx = buffer
        .iter()
        .position(|b| b.kind == BlockKind::Import)
        .unwrap();
    let last_idx = buffer
        .iter()
        .rposition(|b| b.kind == BlockKind::Import)
        .unwrap();

    let mut result = Vec::new();

    // Emit leading gaps
    result.extend(buffer.iter().take(first_idx).cloned());

    // Merge range
    let merged_slice = &buffer[first_idx..=last_idx];
    let start_line = merged_slice[0].start_line;
    let end_line = merged_slice.last().unwrap().end_line;

    let mut merged_content = String::new();
    let mut prev_was_import = false;
    for block in merged_slice.iter() {
        if prev_was_import && block.kind == BlockKind::Import {
            merged_content.push('\n');
        }
        merged_content.push_str(&block.content);
        prev_was_import = block.kind == BlockKind::Import;
    }

    let merged_block = Block {
        hash: hash_str(&merged_content),
        content: merged_content,
        kind: BlockKind::Imports,
        start_line,
        end_line,
    };
    result.push(merged_block);

    // Emit trailing gaps
    result.extend(buffer.iter().skip(last_idx + 1).cloned());

    result
}
