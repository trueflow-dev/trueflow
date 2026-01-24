use crate::context::TrueflowContext;
use crate::scanner;
use anyhow::Result;

pub fn run(_context: &TrueflowContext, json: bool) -> Result<()> {
    let files = scanner::scan_directory(".")?;
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
