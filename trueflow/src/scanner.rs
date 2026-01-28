use crate::analysis::{self, FileType, Language};
use crate::block::{Block, BlockKind, FileState};
use crate::block_splitter;
use crate::hashing::hash_str;
use crate::optimizer;
use crate::text_split::split_by_paragraph_breaks;
use crate::vcs;
use anyhow::Result;
use dirs::home_dir;
use log::warn;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub fn scan_directory<P: AsRef<Path>>(root: P) -> Result<Vec<FileState>> {
    let root = root.as_ref();
    if let Some(cached) = load_cache(root)? {
        return Ok(cached);
    }

    let mut files = Vec::new();

    let walker = WalkDir::new(root).into_iter();

    for entry in walker.filter_entry(|e| !is_ignored(e)) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warn!("Skipping unreadable entry: {}", err);
                continue;
            }
        };
        if entry.file_type().is_file() {
            match process_file(entry.path()) {
                Ok(file_state) => files.push(file_state),
                Err(e) => warn!("Skipping file {:?}: {}", entry.path(), e),
            }
        }
    }

    write_cache(root, &files)?;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    files: Vec<CachedFile>,
    repo_revision: Option<String>,
    root_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    path: String,
    modified_at: u64,
    size: u64,
    file_state: FileState,
}

fn load_cache(root: &Path) -> Result<Option<Vec<FileState>>> {
    let cache_path = cache_path(root)?;
    let contents = match fs::read_to_string(&cache_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let entry: CacheEntry = serde_json::from_str(&contents)?;
    if entry.repo_revision != vcs::snapshot_from_workdir().repo_ref_revision {
        return Ok(None);
    }

    let root_hash = hash_str(root.to_string_lossy().as_ref());
    if entry.root_hash != root_hash {
        return Ok(None);
    }

    let mut files = Vec::new();
    for cached in entry.files {
        let full_path = root.join(&cached.path);
        let metadata = match fs::metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(None),
        };
        let modified = match metadata.modified() {
            Ok(time) => time,
            Err(_) => return Ok(None),
        };
        let modified_at = system_time_to_epoch(modified);
        if modified_at != cached.modified_at || metadata.len() != cached.size {
            return Ok(None);
        }
        files.push(cached.file_state);
    }

    Ok(Some(files))
}

fn write_cache(root: &Path, files: &[FileState]) -> Result<()> {
    let cache_path = cache_path(root)?;
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut cached_files = Vec::new();
    for file in files {
        let full_path = root.join(&file.path);
        let metadata = fs::metadata(&full_path)?;
        let modified = metadata.modified()?;
        cached_files.push(CachedFile {
            path: file.path.clone(),
            modified_at: system_time_to_epoch(modified),
            size: metadata.len(),
            file_state: file.clone(),
        });
    }

    let entry = CacheEntry {
        files: cached_files,
        repo_revision: vcs::snapshot_from_workdir().repo_ref_revision,
        root_hash: hash_str(root.to_string_lossy().as_ref()),
    };

    let contents = serde_json::to_string(&entry)?;
    fs::write(cache_path, contents)?;
    Ok(())
}

fn cache_path(root: &Path) -> Result<PathBuf> {
    let repo_name = root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let cache_root = home_dir().unwrap_or_else(|| root.to_path_buf());
    let root_hash = hash_str(root.to_string_lossy().as_ref());
    Ok(cache_root
        .join(".trueflow")
        .join("cache")
        .join(format!("scan-{}-{}.json", repo_name, root_hash)))
}

fn system_time_to_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// TODO: Investigate whether salsa can help incremental review caching.

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
                Ok(_) => (
                    language,
                    fallback_split_blocks(&content, FallbackMode::Code),
                ), // Fallback if splitter returns empty (not implemented or empty file)
                Err(e) => {
                    warn!(
                        "Failed to parse file {:?}: {}, falling back to paragraphs",
                        path, e
                    );
                    (
                        language,
                        fallback_split_blocks(&content, FallbackMode::Code),
                    )
                }
            }
        }
        FileType::Text => (
            Language::Text,
            fallback_split_blocks(&content, FallbackMode::Text),
        ),
        _ => (
            Language::Unknown,
            fallback_split_blocks(&content, FallbackMode::Text),
        ), // Fallback for non-code files
    };

    // Compute file hash (Merkle root of block hashes)
    let mut hasher = Sha256::new();
    for block in &blocks {
        hasher.update(&block.hash);
    }
    let file_hash = format!("{:x}", hasher.finalize());

    Ok(FileState {
        path: path.to_string_lossy().trim_start_matches("./").to_string(),
        language,
        file_hash,
        blocks,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FallbackMode {
    Code,
    Text,
}

pub(crate) fn fallback_split_blocks(content: &str, mode: FallbackMode) -> Vec<Block> {
    let fallback = split_by_paragraph_breaks(content, |chunk, start, end, is_gap| {
        let kind = classify_fallback_chunk(chunk, mode, is_gap);
        create_fallback_block(content, chunk, kind, start, end)
    });

    if fallback.is_empty() {
        return Vec::new();
    }

    fallback
}

fn classify_fallback_chunk(chunk: &str, mode: FallbackMode, is_gap: bool) -> BlockKind {
    if is_gap {
        return BlockKind::Gap;
    }

    if chunk.trim().is_empty() {
        return BlockKind::Gap;
    }

    match mode {
        FallbackMode::Code => classify_code_paragraph(chunk),
        FallbackMode::Text => BlockKind::Paragraph,
    }
}

fn classify_code_paragraph(chunk: &str) -> BlockKind {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return BlockKind::Gap;
    }

    let is_comment = trimmed.lines().all(|line| {
        let line = line.trim_start();
        line.starts_with("//")
            || line.starts_with('#')
            || line.starts_with("/*")
            || line.starts_with('*')
    });

    if is_comment {
        BlockKind::Comment
    } else {
        BlockKind::CodeParagraph
    }
}

fn create_fallback_block(
    full_source: &str,
    chunk: &str,
    kind: BlockKind,
    start: usize,
    end: usize,
) -> Block {
    let (start_line, end_line) = byte_range_to_lines(full_source, start, end);
    Block {
        hash: hash_str(chunk),
        content: chunk.to_string(),
        kind,
        tags: Vec::new(),
        complexity: 0,
        start_line,
        end_line,
    }
}

fn byte_range_to_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let pre = &source[..start];
    let start_line = pre.lines().count();
    let start_line = if start > 0 && pre.ends_with('\n') {
        start_line
    } else {
        start_line.saturating_sub(1)
    };

    let mid = &source[start..end];
    let new_lines = mid.chars().filter(|&c| c == '\n').count();
    let end_line = start_line + new_lines + if mid.ends_with('\n') { 0 } else { 1 };

    (start_line, end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_merged_blocks(blocks: Vec<Block>, expected: &str) {
        let merged = blocks
            .into_iter()
            .map(|block| block.content)
            .collect::<String>();
        assert_eq!(merged, expected);
    }

    #[test]
    fn fallback_split_text_paragraphs() {
        let content = "Para 1.\n\nPara 2.";
        let blocks = fallback_split_blocks(content, FallbackMode::Text);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(blocks[1].kind, BlockKind::Gap);
        assert_eq!(blocks[2].kind, BlockKind::Paragraph);
        assert_merged_blocks(blocks, content);
    }

    #[test]
    fn fallback_split_code_paragraphs() {
        let content = "fn main() {}\n\n// comment";
        let blocks = fallback_split_blocks(content, FallbackMode::Code);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, BlockKind::CodeParagraph);
        assert_eq!(blocks[1].kind, BlockKind::Gap);
        assert_eq!(blocks[2].kind, BlockKind::Comment);
        assert_merged_blocks(blocks, content);
    }
}
