use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::scanner;
use crate::tree;
use anyhow::{Result, bail};

pub fn run(_context: &TrueflowContext, json: bool, tree_output: bool) -> Result<()> {
    let config = load_config()?;
    let scan_options = config.scan.resolve_options()?;
    let result = scanner::scan_directory(".", &scan_options)?;
    if tree_output {
        if !json {
            bail!("Tree output requires --json");
        }
        let tree = tree::build_tree_from_files(&result.files);
        println!("{}", serde_json::to_string_pretty(&tree.view_json())?);
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let scanner::ScanResult {
            files, diagnostics, ..
        } = result;
        for file in files {
            println!(
                "File: {} (Tree Hash: {}, Bytes Hash: {})",
                file.path, file.tree_hash, file.bytes_hash
            );
            for block in file.blocks {
                println!(
                    "  Block [L{}-L{}]: {}",
                    block.start_line, block.end_line, block.hash
                );
            }
        }
        for diagnostic in diagnostics {
            eprintln!("warning: {}", diagnostic.display_message());
        }
    }
    Ok(())
}
