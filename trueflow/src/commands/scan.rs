use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::scanner;
use crate::tree;
use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutputMode {
    Text,
    JsonFiles,
    JsonTree,
}

impl ScanOutputMode {
    pub fn from_flags(json: bool, tree_output: bool) -> Result<Self> {
        match (json, tree_output) {
            (false, false) => Ok(Self::Text),
            (true, false) => Ok(Self::JsonFiles),
            (true, true) => Ok(Self::JsonTree),
            (false, true) => bail!("Tree output requires --json"),
        }
    }
}

pub fn run(_context: &TrueflowContext, output_mode: ScanOutputMode) -> Result<()> {
    let config = load_config()?;
    let scan_options = config.scan.resolve_options()?;
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
            ScanOutputMode::from_flags(false, false).unwrap(),
            ScanOutputMode::Text
        );
        assert_eq!(
            ScanOutputMode::from_flags(true, false).unwrap(),
            ScanOutputMode::JsonFiles
        );
        assert_eq!(
            ScanOutputMode::from_flags(true, true).unwrap(),
            ScanOutputMode::JsonTree
        );
    }

    #[test]
    fn scan_output_mode_rejects_tree_without_json() {
        let error = ScanOutputMode::from_flags(false, true).unwrap_err();
        assert!(error.to_string().contains("Tree output requires --json"));
    }
}
