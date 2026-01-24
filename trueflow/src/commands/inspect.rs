use crate::context::TrueflowContext;
use crate::scanner;
use crate::sub_splitter;
use anyhow::{Context, Result, bail};

pub fn run(_context: &TrueflowContext, fingerprint: &str, split: bool) -> Result<()> {
    let files = scanner::scan_directory(".")?;
    let mut matches = Vec::new();

    for file in &files {
        for block in &file.blocks {
            if block.hash.starts_with(fingerprint) {
                matches.push((block.clone(), file.language.clone()));
            }
        }
    }

    if matches.is_empty() {
        for file in &files {
            for block in &file.blocks {
                if let Ok(sub_blocks) = sub_splitter::split(block, file.language.clone()) {
                    for sub_block in sub_blocks {
                        if sub_block.hash.starts_with(fingerprint) {
                            matches.push((sub_block, file.language.clone()));
                        }
                    }
                }
            }
        }
    }

    if matches.is_empty() {
        bail!("Block not found");
    }
    if matches.len() > 1 {
        bail!(
            "Multiple blocks matched fingerprint ({} matches). Use a longer prefix.",
            matches.len()
        );
    }

    let (block, lang) = matches.pop().context("Block not found")?;
    if split {
        let sub_blocks = sub_splitter::split(&block, lang)?;
        println!("{}", serde_json::to_string_pretty(&sub_blocks)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&block)?);
    }

    Ok(())
}
