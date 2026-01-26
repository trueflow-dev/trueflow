use crate::context::TrueflowContext;
use crate::scanner;
use crate::tree;
use anyhow::{Result, bail};

pub fn run(_context: &TrueflowContext, json: bool, tree_output: bool) -> Result<()> {
    let files = scanner::scan_directory(".")?;
    if tree_output {
        if !json {
            bail!("Tree output requires --json");
        }
        let tree = tree::build_tree_from_files(&files);
        println!("{}", serde_json::to_string_pretty(&tree.view_json())?);
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&files)?);
    } else {
        for file in files {
            println!("File: {} (Hash: {})", file.path, file.file_hash);
            for block in file.blocks {
                println!(
                    "  Block [L{}-L{}]: {}",
                    block.start_line, block.end_line, block.hash
                );
            }
        }
    }
    Ok(())
}
