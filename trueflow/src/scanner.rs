use crate::analysis::{self, FileType, Language};
use crate::block::FileState;
use crate::block_splitter;
use crate::hashing::hash_str;
use crate::repo_path::RepoPath;
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
const SCAN_CACHE_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCacheMode {
    Disabled,
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

impl ScanCacheMode {
    pub fn from_flags(use_cache: bool, write_cache: bool) -> Self {
        match (use_cache, write_cache) {
            (false, false) => Self::Disabled,
            (true, false) => Self::ReadOnly,
            (false, true) => Self::WriteOnly,
            (true, true) => Self::ReadWrite,
        }
    }

    fn reads_enabled(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    fn writes_enabled(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    pub cache_mode: ScanCacheMode,
    pub cache_dir: Option<PathBuf>,
    pub ignore_names: Vec<String>,
    pub ignore_globs: Vec<String>,
    pub ignore_path_prefixes: Vec<RepoPath>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            cache_mode: ScanCacheMode::ReadWrite,
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
    pub reused_files: usize,
    pub rescanned_files: usize,
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
        reused_files: 0,
        rescanned_files: 0,
    };

    let cached_entry = if options.cache_mode.reads_enabled() {
        match load_cache_entry(root, options) {
            Ok(Some(entry)) => {
                cache.read = ScanCacheReadStatus::Hit;
                Some(index_cached_files(entry))
            }
            Ok(None) => {
                cache.read = ScanCacheReadStatus::Miss;
                None
            }
            Err(err) => {
                cache.read = ScanCacheReadStatus::Error;
                diagnostics.push(ScanDiagnostic::new(
                    None,
                    format!("failed to load scan cache: {err}"),
                ));
                debug!("scan cache unavailable, continuing without cache: {err}");
                None
            }
        }
    } else {
        None
    };

    let inventory = collect_scan_inventory(root, options, &mut diagnostics)?;

    let mut files = Vec::new();
    let mut cache_files = Vec::with_capacity(inventory.len());

    for scan_input in inventory {
        let reused_entry = cached_entry
            .as_ref()
            .and_then(|entries| entries.get(&scan_input.path))
            .filter(|entry| entry.stamp == scan_input.stamp);

        let cache_file = match reused_entry {
            Some(entry) => {
                cache.reused_files += 1;
                entry.clone()
            }
            None => {
                cache.rescanned_files += 1;
                scan_file(root, &scan_input)
            }
        };

        append_cached_outcome(&cache_file.outcome, &mut files, &mut diagnostics);
        cache_files.push(cache_file);
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    sort_diagnostics(&mut diagnostics);

    if options.cache_mode.writes_enabled() {
        match write_cache(root, options, cache_files) {
            Ok(()) => cache.write = ScanCacheWriteStatus::Wrote,
            Err(err) => {
                cache.write = ScanCacheWriteStatus::Error;
                diagnostics.push(ScanDiagnostic::new(
                    None,
                    format!("failed to write scan cache: {err}"),
                ));
                sort_diagnostics(&mut diagnostics);
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
    format_version: u32,
    root_hash: String,
    options_fingerprint: String,
    files: Vec<CachedFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFileEntry {
    path: RepoPath,
    stamp: FileStamp,
    outcome: CachedFileOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CachedFileOutcome {
    Included {
        file_state: FileState,
        diagnostics: Vec<ScanDiagnostic>,
    },
    Skipped {
        diagnostics: Vec<ScanDiagnostic>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct FileStamp {
    modified_at: u64,
    size: u64,
}

#[derive(Debug, Clone)]
struct ScanInput {
    path: RepoPath,
    full_path: PathBuf,
    stamp: FileStamp,
}

fn load_cache_entry(root: &Path, options: &ScanOptions) -> Result<Option<CacheEntry>> {
    let cache_path = cache_path(root, options)?;
    let contents = match fs::read_to_string(&cache_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let entry: CacheEntry = serde_json::from_str(&contents)?;
    if entry.format_version != SCAN_CACHE_FORMAT_VERSION {
        return Ok(None);
    }
    if entry.root_hash != cache_root_hash(root) {
        return Ok(None);
    }
    if entry.options_fingerprint != scan_options_fingerprint(options) {
        return Ok(None);
    }

    Ok(Some(entry))
}

fn index_cached_files(entry: CacheEntry) -> HashMap<RepoPath, CachedFileEntry> {
    entry
        .files
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect()
}

fn write_cache(root: &Path, options: &ScanOptions, files: Vec<CachedFileEntry>) -> Result<()> {
    let cache_path = cache_path(root, options)?;
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut files = files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let entry = CacheEntry {
        format_version: SCAN_CACHE_FORMAT_VERSION,
        root_hash: cache_root_hash(root),
        options_fingerprint: scan_options_fingerprint(options),
        files,
    };

    let contents = serde_json::to_string(&entry)?;
    fs::write(cache_path, contents)?;
    Ok(())
}

fn scan_options_fingerprint(options: &ScanOptions) -> String {
    #[derive(Serialize)]
    struct CacheOptionsFingerprint<'a> {
        ignore_names: &'a [String],
        ignore_globs: &'a [String],
        ignore_path_prefixes: Vec<&'a str>,
    }

    let mut ignore_names = options.ignore_names.clone();
    ignore_names.sort();
    ignore_names.dedup();

    let mut ignore_globs = options.ignore_globs.clone();
    ignore_globs.sort();
    ignore_globs.dedup();

    let mut ignore_path_prefixes: Vec<_> = options
        .ignore_path_prefixes
        .iter()
        .map(|path| path.as_str())
        .collect();
    ignore_path_prefixes.sort();
    ignore_path_prefixes.dedup();

    let fingerprint = CacheOptionsFingerprint {
        ignore_names: &ignore_names,
        ignore_globs: &ignore_globs,
        ignore_path_prefixes,
    };

    let json = match serde_json::to_string(&fingerprint) {
        Ok(json) => json,
        Err(err) => panic!("serializing scan cache fingerprint should succeed: {err}"),
    };
    hash_str(&json)
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

fn collect_scan_inventory(
    root: &Path,
    options: &ScanOptions,
    diagnostics: &mut Vec<ScanDiagnostic>,
) -> Result<Vec<ScanInput>> {
    let mut inputs = Vec::new();

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
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        let path = normalize_cache_key(root, entry.path())?;
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(err) => {
                let err = anyhow::Error::from(err);
                let diagnostic = diagnostic_for_process_file_error(root, entry.path(), &err);
                log_process_file_error(&diagnostic, &err);
                diagnostics.push(diagnostic);
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(err) => {
                let err = anyhow::Error::from(err);
                let diagnostic = diagnostic_for_process_file_error(root, entry.path(), &err);
                log_process_file_error(&diagnostic, &err);
                diagnostics.push(diagnostic);
                continue;
            }
        };

        inputs.push(ScanInput {
            path,
            full_path: entry.into_path(),
            stamp: FileStamp {
                modified_at: system_time_to_epoch(modified),
                size: metadata.len(),
            },
        });
    }

    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inputs)
}

fn scan_file(root: &Path, input: &ScanInput) -> CachedFileEntry {
    let outcome = match process_file(root, &input.full_path) {
        Ok(file_scan) => CachedFileOutcome::Included {
            file_state: file_scan.file_state,
            diagnostics: file_scan.diagnostics,
        },
        Err(err) => {
            let diagnostic = diagnostic_for_process_file_error(root, &input.full_path, &err);
            log_process_file_error(&diagnostic, &err);
            CachedFileOutcome::Skipped {
                diagnostics: vec![diagnostic],
            }
        }
    };

    CachedFileEntry {
        path: input.path.clone(),
        stamp: input.stamp,
        outcome,
    }
}

fn append_cached_outcome(
    outcome: &CachedFileOutcome,
    files: &mut Vec<FileState>,
    diagnostics: &mut Vec<ScanDiagnostic>,
) {
    match outcome {
        CachedFileOutcome::Included {
            file_state,
            diagnostics: file_diagnostics,
        } => {
            files.push(file_state.clone());
            diagnostics.extend(file_diagnostics.iter().cloned());
        }
        CachedFileOutcome::Skipped {
            diagnostics: file_diagnostics,
        } => diagnostics.extend(file_diagnostics.iter().cloned()),
    }
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

fn sort_diagnostics(diagnostics: &mut [ScanDiagnostic]) {
    diagnostics.sort_by(|a, b| {
        let a_path = a.path.as_ref().map(RepoPath::as_str);
        let b_path = b.path.as_ref().map(RepoPath::as_str);
        (a_path, a.reason.as_str()).cmp(&(b_path, b.reason.as_str()))
    });
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

#[derive(Debug, Clone)]
struct ProcessedFile {
    file_state: FileState,
    diagnostics: Vec<ScanDiagnostic>,
}

fn process_file(root: &Path, path: &Path) -> Result<ProcessedFile> {
    let file_type = analysis::analyze_file(path);
    let relative_path = path.strip_prefix(root).unwrap_or(path);
    let normalized_path = RepoPath::from_relative_path(relative_path)?;

    if matches!(file_type, FileType::Binary) {
        return Ok(ProcessedFile {
            file_state: FileState::from_binary(normalized_path, &fs::read(path)?),
            diagnostics: Vec::new(),
        });
    }

    let bytes = fs::read(path)?;
    let content = std::str::from_utf8(&bytes)?;

    let language = match file_type {
        FileType::Code(code_file) => code_file.language,
        FileType::Text => Language::Text,
        _ => Language::Unknown,
    };
    let split_result = block_splitter::split(content, language);
    let diagnostics = split_result
        .diagnostics
        .iter()
        .map(|diagnostic| {
            ScanDiagnostic::new(Some(normalized_path.clone()), diagnostic.reason.clone())
        })
        .collect();

    Ok(ProcessedFile {
        file_state: FileState::from_text(
            normalized_path,
            language,
            &bytes,
            split_result.into_review_blocks(),
        ),
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    #[test]
    fn scan_cache_mode_maps_bool_pairs_to_explicit_modes() {
        assert_eq!(
            ScanCacheMode::from_flags(false, false),
            ScanCacheMode::Disabled
        );
        assert_eq!(
            ScanCacheMode::from_flags(true, false),
            ScanCacheMode::ReadOnly
        );
        assert_eq!(
            ScanCacheMode::from_flags(false, true),
            ScanCacheMode::WriteOnly
        );
        assert_eq!(
            ScanCacheMode::from_flags(true, true),
            ScanCacheMode::ReadWrite
        );
    }

    #[test]
    fn scan_options_fingerprint_ignores_order_and_duplicates() {
        let mut a = ScanOptions::default();
        a.ignore_names
            .extend(["dist".to_string(), "dist".to_string()]);
        a.ignore_globs
            .extend(["*.snap".to_string(), "*.tmp".to_string()]);
        a.ignore_path_prefixes.extend([
            RepoPath::new("generated").unwrap(),
            RepoPath::new("vendor").unwrap(),
        ]);

        let mut b = ScanOptions::default();
        b.ignore_names.extend(["dist".to_string()]);
        b.ignore_globs
            .extend(["*.tmp".to_string(), "*.snap".to_string()]);
        b.ignore_path_prefixes.extend([
            RepoPath::new("vendor").unwrap(),
            RepoPath::new("generated").unwrap(),
            RepoPath::new("vendor").unwrap(),
        ]);

        assert_eq!(scan_options_fingerprint(&a), scan_options_fingerprint(&b));
    }
}
