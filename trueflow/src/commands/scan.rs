use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::scanner;
use crate::tree;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutputMode {
    Text,
    JsonFiles,
    JsonTree,
}

impl ScanOutputMode {
    pub fn from_flags(json: bool, tree_output: bool) -> Self {
        match (json, tree_output) {
            (false, false) => Self::Text,
            (true, false) => Self::JsonFiles,
            (true, true) => Self::JsonTree,
            (false, true) => panic!("clap invariant violated: --tree requires --json"),
        }
    }
}

pub fn run(_context: &TrueflowContext, output_mode: ScanOutputMode) -> Result<()> {
    let config = load_config()?;
    let scan_options = config.scan.resolve_options();
    let result = scanner::scan_directory(".", &scan_options)?;
    match output_mode {
        ScanOutputMode::JsonTree => {
            let tree = tree::build_tree_from_files(&result.files);
            println!("{}", serde_json::to_string_pretty(&tree.view_json())?);
        }
        ScanOutputMode::JsonFiles => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        ScanOutputMode::Text => {
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_output_mode_maps_cli_flags_to_text_and_json_variants() {
        assert_eq!(
            ScanOutputMode::from_flags(false, false),
            ScanOutputMode::Text
        );
        assert_eq!(
            ScanOutputMode::from_flags(true, false),
            ScanOutputMode::JsonFiles
        );
        assert_eq!(
            ScanOutputMode::from_flags(true, true),
            ScanOutputMode::JsonTree
        );
    }
}
