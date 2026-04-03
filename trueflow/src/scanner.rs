use crate::analysis::{self, FileType, Language};
use crate::block::{Block, BlockKind, FileState};
use crate::block_splitter;
use crate::hashing::{TreeHash, hash_str};
use crate::optimizer;
use crate::repo_path::RepoPath;
use crate::text_split::split_by_paragraph_breaks;
use anyhow::Result;
use dirs::home_dir;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, WalkBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

const DEFAULT_IGNORE_NAMES: &[&str] =
    &[".git", ".trueflow", "target", "node_modules", "mutants.out"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    pub use_cache: bool,
    pub write_cache: bool,
    pub cache_dir: Option<PathBuf>,
    pub ignore_names: Vec<String>,
    pub ignore_globs: Vec<String>,
    pub ignore_path_prefixes: Vec<RepoPath>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            use_cache: true,
            write_cache: true,
            cache_dir: None,
            ignore_names: DEFAULT_IGNORE_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            ignore_globs: Vec::new(),
            ignore_path_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanDiagnostic {
    pub path: Option<RepoPath>,
    pub reason: String,
}

impl ScanDiagnostic {
    pub fn new(path: Option<RepoPath>, reason: impl Into<String>) -> Self {
        Self {
            path,
            reason: reason.into(),
        }
    }

    pub fn display_message(&self) -> String {
        match &self.path {
            Some(path) => format!("{path}: {}", self.reason),
            None => self.reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanCacheReadStatus {
    Disabled,
    Hit,
    Miss,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanCacheWriteStatus {
    Disabled,
    Wrote,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanCacheReport {
    pub read: ScanCacheReadStatus,
    pub write: ScanCacheWriteStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub files: Vec<FileState>,
    pub diagnostics: Vec<ScanDiagnostic>,
    pub cache: ScanCacheReport,
}

pub fn scan_directory<P: AsRef<Path>>(root: P, options: &ScanOptions) -> Result<ScanResult> {
    let root = root.as_ref();
    let mut diagnostics = Vec::new();
    let mut cache = ScanCacheReport {
        read: ScanCacheReadStatus::Disabled,
        write: ScanCacheWriteStatus::Disabled,
    };

    if options.use_cache {
        match load_cache(root, options) {
            Ok(Some(mut cached)) => {
                cached.sort_by(|a, b| a.path.cmp(&b.path));
                cache.read = ScanCacheReadStatus::Hit;
                cache.write = ScanCacheWriteStatus::Skipped;
                return Ok(ScanResult {
                    files: cached,
                    diagnostics,
                    cache,
                });
            }
            Ok(None) => {
                cache.read = ScanCacheReadStatus::Miss;
            }
            Err(err) => {
                cache.read = ScanCacheReadStatus::Error;
                diagnostics.push(ScanDiagnostic::new(
                    None,
                    format!("failed to load scan cache: {err}"),
                ));
                debug!("scan cache unavailable, continuing without cache: {err}");
            }
        }
    }

    let mut files = Vec::new();

    for entry in build_walker(root, options)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let diagnostic = diagnostic_for_walk_error(root, &err);
                log_walk_scan_error(&diagnostic, &err);
                diagnostics.push(diagnostic);
                continue;
            }
        };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            match process_file(root, entry.path(), &mut diagnostics) {
                Ok(file_state) => files.push(file_state),
                Err(err) => {
                    let diagnostic = diagnostic_for_process_file_error(root, entry.path(), &err);
                    log_process_file_error(&diagnostic, &err);
                    diagnostics.push(diagnostic);
                }
            }
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    if options.write_cache {
        match write_cache(root, options, &files) {
            Ok(()) => cache.write = ScanCacheWriteStatus::Wrote,
            Err(err) => {
                cache.write = ScanCacheWriteStatus::Error;
                diagnostics.push(ScanDiagnostic::new(
                    None,
                    format!("failed to write scan cache: {err}"),
                ));
                debug!("failed to write scan cache, continuing: {err}");
            }
        }
    }

    Ok(ScanResult {
        files,
        diagnostics,
        cache,
    })
}

fn build_walker(root: &Path, options: &ScanOptions) -> Result<ignore::Walk> {
    let matcher = IgnoreMatcher::new(root, options)?;
    let root = root.to_path_buf();
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .require_git(false);
    builder.filter_entry(move |entry| !matcher.matches(&root, entry));
    Ok(builder.build())
}

struct IgnoreMatcher {
    names: HashSet<String>,
    path_prefixes: Vec<RepoPath>,
    glob_matcher: Gitignore,
}

impl IgnoreMatcher {
    fn new(root: &Path, options: &ScanOptions) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(root);
        builder.allow_unclosed_class(false);
        for pattern in &options.ignore_globs {
            builder.add_line(None, pattern)?;
        }

        Ok(Self {
            names: options.ignore_names.iter().cloned().collect(),
            path_prefixes: options.ignore_path_prefixes.clone(),
            glob_matcher: if options.ignore_globs.is_empty() {
                Gitignore::empty()
            } else {
                builder.build()?
            },
        })
    }

    fn matches(&self, root: &Path, entry: &DirEntry) -> bool {
        let Some(name) = entry.path().file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if self.names.contains(name) {
            return true;
        }

        let is_dir = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());

        if !self.path_prefixes.is_empty()
            && let Ok(repo_path) = RepoPath::from_relative_path(relative)
            && self
                .path_prefixes
                .iter()
                .any(|prefix| repo_path_matches_prefix(&repo_path, prefix))
        {
            return true;
        }

        self.glob_matcher
            .matched_path_or_any_parents(relative, is_dir)
            .is_ignore()
    }
}

fn repo_path_matches_prefix(path: &RepoPath, prefix: &RepoPath) -> bool {
    if prefix.is_root() {
        return true;
    }
    path == prefix
        || path
            .as_str()
            .strip_prefix(prefix.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    files: Vec<CachedFile>,
    root_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    path: RepoPath,
    modified_at: u64,
    size: u64,
    file_state: FileState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified_at: u64,
    size: u64,
}

fn load_cache(root: &Path, options: &ScanOptions) -> Result<Option<Vec<FileState>>> {
    let cache_path = cache_path(root, options)?;
    let contents = match fs::read_to_string(&cache_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let entry: CacheEntry = serde_json::from_str(&contents)?;

    let root_hash = cache_root_hash(root);
    if entry.root_hash != root_hash {
        return Ok(None);
    }

    let current_stamps = collect_current_file_stamps(root, options)?;
    if current_stamps.len() != entry.files.len() {
        return Ok(None);
    }

    let mut files = Vec::new();
    for cached in entry.files {
        let Some(current) = current_stamps.get(&cached.path) else {
            return Ok(None);
        };
        if current.modified_at != cached.modified_at || current.size != cached.size {
            return Ok(None);
        }
        files.push(cached.file_state);
    }

    Ok(Some(files))
}

fn write_cache(root: &Path, options: &ScanOptions, files: &[FileState]) -> Result<()> {
    let cache_path = cache_path(root, options)?;
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut cached_files = Vec::new();
    for file in files {
        let full_path = root.join(file.path.as_str());
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
        root_hash: cache_root_hash(root),
    };

    let contents = serde_json::to_string(&entry)?;
    fs::write(cache_path, contents)?;
    Ok(())
}

fn cache_path(root: &Path, options: &ScanOptions) -> Result<PathBuf> {
    let identity = cache_identity(root);
    let repo_name = identity
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let cache_root = options.cache_dir.clone().unwrap_or_else(|| {
        home_dir()
            .unwrap_or_else(|| root.to_path_buf())
            .join(".trueflow")
            .join("cache")
    });
    let root_hash = hash_str(identity.to_string_lossy().as_ref());
    Ok(cache_root.join(format!("scan-{repo_name}-{root_hash}.json")))
}

fn cache_identity(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn cache_root_hash(root: &Path) -> String {
    let identity = cache_identity(root);
    hash_str(identity.to_string_lossy().as_ref())
}

fn collect_current_file_stamps(
    root: &Path,
    options: &ScanOptions,
) -> Result<HashMap<RepoPath, FileStamp>> {
    let mut stamps = HashMap::new();
    for entry in build_walker(root, options)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                if is_permission_denied_walk_error(&err) {
                    debug!("Skipping unreadable entry during cache validation: {err}");
                    continue;
                }
                return Err(err.into());
            }
        };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(err) if is_permission_denied_io(&err) => {
                debug!(
                    "Skipping unreadable file during cache validation {:?}: {}",
                    entry.path(),
                    err
                );
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(err) if is_permission_denied_io(&err) => {
                debug!(
                    "Skipping unreadable metadata during cache validation {:?}: {}",
                    entry.path(),
                    err
                );
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let path = normalize_cache_key(root, entry.path())?;
        stamps.insert(
            path,
            FileStamp {
                modified_at: system_time_to_epoch(modified),
                size: metadata.len(),
            },
        );
    }
    Ok(stamps)
}

fn normalize_cache_key(root: &Path, path: &Path) -> Result<RepoPath> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    RepoPath::from_relative_path(relative)
}

fn system_time_to_epoch(time: SystemTime) -> u64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

fn is_permission_denied_io(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
}

fn is_permission_denied_walk_error(err: &ignore::Error) -> bool {
    err.io_error().is_some_and(is_permission_denied_io)
}

fn log_walk_scan_error(diagnostic: &ScanDiagnostic, err: &ignore::Error) {
    if is_permission_denied_walk_error(err) {
        debug!("{}", diagnostic.display_message());
    } else {
        warn!("{}", diagnostic.display_message());
    }
}

fn log_process_file_error(diagnostic: &ScanDiagnostic, err: &anyhow::Error) {
    if err
        .downcast_ref::<std::io::Error>()
        .is_some_and(is_permission_denied_io)
    {
        debug!("{}", diagnostic.display_message());
    } else {
        warn!("{}", diagnostic.display_message());
    }
}

fn diagnostic_for_walk_error(_root: &Path, err: &ignore::Error) -> ScanDiagnostic {
    let reason = if is_permission_denied_walk_error(err) {
        format!("skipped unreadable entry: {err}")
    } else {
        format!("skipped entry: {err}")
    };
    ScanDiagnostic::new(None, reason)
}

fn diagnostic_for_process_file_error(
    root: &Path,
    path: &Path,
    err: &anyhow::Error,
) -> ScanDiagnostic {
    let path = normalize_cache_key(root, path).ok();
    let reason = if err.downcast_ref::<std::str::Utf8Error>().is_some() {
        "skipped invalid UTF-8 file".to_string()
    } else if err
        .downcast_ref::<std::io::Error>()
        .is_some_and(is_permission_denied_io)
    {
        format!("skipped unreadable file: {err}")
    } else {
        format!("skipped file: {err}")
    };
    ScanDiagnostic::new(path, reason)
}

// TODO: Investigate whether salsa can help incremental review caching.

fn process_file(
    root: &Path,
    path: &Path,
    diagnostics: &mut Vec<ScanDiagnostic>,
) -> Result<FileState> {
    let file_type = analysis::analyze_file(path);
    let relative_path = path.strip_prefix(root).unwrap_or(path);
    let normalized_path = RepoPath::from_relative_path(relative_path)?;

    if matches!(file_type, FileType::Binary) {
        return Ok(FileState::from_binary(normalized_path, &fs::read(path)?));
    }

    let bytes = fs::read(path)?;
    let content = std::str::from_utf8(&bytes)?;

    let (language, blocks) = match file_type {
        FileType::Code(code_file) => {
            let language = code_file.language;
            let blocks = block_splitter::split(content, language);

            match blocks {
                Ok(b) if !b.is_empty() => (language, optimizer::optimize(b)),
                Ok(_) => {
                    diagnostics.push(ScanDiagnostic::new(
                        Some(normalized_path.clone()),
                        "block splitter returned no blocks; used fallback splitter",
                    ));
                    (language, fallback_split_blocks(content, FallbackMode::Code))
                }
                Err(e) => {
                    diagnostics.push(ScanDiagnostic::new(
                        Some(normalized_path.clone()),
                        format!("block splitter failed; used fallback splitter: {e}"),
                    ));
                    warn!("Failed to parse file {path:?}: {e}, falling back to paragraphs");
                    (language, fallback_split_blocks(content, FallbackMode::Code))
                }
            }
        }
        FileType::Text => (
            Language::Text,
            fallback_split_blocks(content, FallbackMode::Text),
        ),
        _ => (
            Language::Unknown,
            fallback_split_blocks(content, FallbackMode::Text),
        ),
    };

    Ok(FileState::from_text(
        normalized_path,
        language,
        &bytes,
        blocks,
    ))
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
        hash: TreeHash::from_content(chunk),
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
    use std::time::Duration;

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

    #[test]
    fn cache_timestamp_preserves_subsecond_precision() {
        let a = UNIX_EPOCH + Duration::from_secs(123) + Duration::from_nanos(1);
        let b = UNIX_EPOCH + Duration::from_secs(123) + Duration::from_nanos(2);
        assert_ne!(system_time_to_epoch(a), system_time_to_epoch(b));
    }

    #[test]
    fn repo_path_matches_prefix_checks_exact_or_descendant() {
        let prefix = RepoPath::new("src/generated").unwrap();
        assert!(repo_path_matches_prefix(
            &RepoPath::new("src/generated").unwrap(),
            &prefix,
        ));
        assert!(repo_path_matches_prefix(
            &RepoPath::new("src/generated/out.rs").unwrap(),
            &prefix,
        ));
        assert!(!repo_path_matches_prefix(
            &RepoPath::new("src/generate.rs").unwrap(),
            &prefix,
        ));
    }
}
