use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::{block_splitter, optimizer};

pub fn fuzzy_find_block(path: &Path, fuzzy_ident: &str) -> Result<Block> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;
    let file_type = crate::analysis::analyze_file(path);

    let language = match file_type {
        crate::analysis::FileType::Code(code_file) => code_file.language,
        _ => Language::Unknown,
    };

    let blocks = match block_splitter::split(&content, language.clone()) {
        Ok(blocks) if !blocks.is_empty() => optimizer::optimize(blocks),
        Ok(_) => Vec::new(),
        Err(err) => bail!("Failed to split file {}: {}", path.display(), err),
    };

    let mut matches = blocks
        .iter()
        .filter(|block| {
            matches!(block.kind, BlockKind::Function | BlockKind::Method)
                && block.content.contains(fuzzy_ident)
        })
        .cloned()
        .collect::<Vec<_>>();

    if matches.is_empty() {
        matches = blocks
            .iter()
            .filter(|block| block.content.contains(fuzzy_ident))
            .cloned()
            .collect();
    }

    if matches.is_empty() {
        bail!("No block matched '{}' in {}", fuzzy_ident, path.display());
    }
    if matches.len() > 1 {
        bail!(
            "Multiple blocks matched '{}' in {}",
            fuzzy_ident,
            path.display()
        );
    }

    Ok(matches.remove(0))
}
