use crate::analysis::{self, FileType, Language};
use crate::block::{Block, BlockKind, FileState};
use crate::block_splitter;
use crate::hashing::hash_str;
use crate::optimizer;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub fn scan_directory<P: AsRef<Path>>(root: P) -> Result<Vec<FileState>> {
    let mut files = Vec::new();

    let walker = WalkDir::new(root).into_iter();

    for entry in walker.filter_entry(|e| !is_ignored(e)) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("Skipping unreadable entry: {}", err);
                continue;
            }
        };
        if entry.file_type().is_file() {
            match process_file(entry.path()) {
                Ok(file_state) => files.push(file_state),
                Err(e) => eprintln!("Skipping file {:?}: {}", entry.path(), e),
            }
        }
    }

    Ok(files)
}

fn is_ignored(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();

    // Don't ignore the root "."
    if name == "." {
        return false;
    }

    // Basic ignore rules
    name.starts_with('.') || // .git, .trueflow, .env
    name == "target" ||      // rust build
    name == "node_modules" // js dependencies
}

fn process_file(path: &Path) -> Result<FileState> {
    let file_type = analysis::analyze_file(path);

    // Skip binary files
    if matches!(file_type, FileType::Binary) {
        // Return empty block list or handle specifically?
        // For now, let's treat them as empty/skipped to avoid polluting output with garbage.
        // Or create a single block "Binary Content".
        return Ok(FileState {
            path: path.to_string_lossy().to_string(),
            language: Language::Unknown,
            file_hash: "binary_skipped".to_string(),
            blocks: Vec::new(),
        });
    }

    let content = fs::read_to_string(path)?;

    // Choose chunker based on analysis
    let (language, blocks) = match file_type {
        FileType::Code(code_file) => {
            // Check if we have a splitter for this language
            let language = code_file.language.clone();
            let blocks = block_splitter::split(&content, language.clone());

            match blocks {
                Ok(b) if !b.is_empty() => (language, optimizer::optimize(b)),
                Ok(_) => (language, chunk_content(&content)), // Fallback if splitter returns empty (not implemented or empty file)
                Err(e) => {
                    // Only log if it's a supported language that failed
                    if matches!(
                        language,
                        Language::Rust
                            | Language::Python
                            | Language::JavaScript
                            | Language::TypeScript
                            | Language::Shell
                    ) || language.uses_text_fallback() {
                        eprintln!(
                            "Failed to parse file {:?}: {}, falling back to lines",
                            path, e
                        );
                    }
                    (language, chunk_content(&content))
                }
            }
        }
        _ => (Language::Unknown, chunk_content(&content)), // Fallback for non-code files
    };

    // Compute file hash (Merkle root of block hashes)
    let mut hasher = Sha256::new();
    for block in &blocks {
        hasher.update(&block.hash);
    }
    let file_hash = format!("{:x}", hasher.finalize());

    Ok(FileState {
        path: path.to_string_lossy().to_string(),
        language,
        file_hash,
        blocks,
    })
}

fn chunk_content(content: &str) -> Vec<Block> {
    // Strategy: Fixed line chunks (e.g. 20 lines)
    // This is the MVP strategy. Later we can do tree-sitter or rolling hash.
    const CHUNK_SIZE: usize = 20;

    let lines: Vec<&str> = content.lines().collect();
    let mut blocks = Vec::new();

    if lines.is_empty() {
        return blocks;
    }

    for (i, chunk) in lines.chunks(CHUNK_SIZE).enumerate() {
        let start_line = i * CHUNK_SIZE;
        let end_line = start_line + chunk.len();

        let block_content = chunk.join("\n");
        let hash = hash_str(&block_content);

        blocks.push(Block {
            hash,
            content: block_content,
            kind: BlockKind::TextBlock,
            start_line,
            end_line,
        });
    }

    blocks
}
