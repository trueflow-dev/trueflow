use crate::analysis;
use crate::block::FileState;
use crate::hashing::hash_str;
use crate::repo_path::RepoPath;
use anyhow::{Result, anyhow};
use dirs::cache_dir;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, WalkBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

const DEFAULT_IGNORE_NAMES: &[&str] =
    &[".git", ".trueflow", "target", "node_modules", "mutants.out"];
const SCAN_CACHE_FORMAT_VERSION: u32 = 4;
const SCAN_CACHE_DIR_ENV: &str = "TRUEFLOW_CACHE_DIR";
const MAX_SCAN_CACHE_FILES: usize = 128;
const MAX_SCAN_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const STALE_SCAN_CACHE_TMP_AGE: Duration = Duration::from_secs(60 * 60);
const SCAN_WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;
const SCAN_WORKER_BATCH_SIZE: usize = 4;

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
    let root = canonicalize_or_original(root.as_ref());
    let repo_path_base = repo_path_base_for_scan_root(&root);
    let mut diagnostics = Vec::new();
    let mut cache = ScanCacheReport {
        read: ScanCacheReadStatus::Disabled,
        write: ScanCacheWriteStatus::Disabled,
        reused_files: 0,
        rescanned_files: 0,
    };

    let mut cached_entry = load_scan_cache_for_read(&root, options, &mut cache, &mut diagnostics);
    let cached_entry_count = cached_entry.as_ref().map(HashMap::len);

    let inventory = collect_scan_inventory(&root, &repo_path_base, options, &mut diagnostics)?;

    let mut files = Vec::new();
    let mut cache_files = Vec::with_capacity(inventory.len());
    let mut misses = Vec::new();

    for scan_input in inventory {
        // Inventory paths are unique, so each cached entry is taken at most once.
        let reused_entry = cached_entry
            .as_mut()
            .and_then(|entries| entries.remove(&scan_input.path))
            .filter(|entry| entry.stamp == scan_input.stamp);

        match reused_entry {
            Some(entry) => {
                cache.reused_files += 1;
                append_cached_outcome(&entry.outcome, &mut files, &mut diagnostics);
                cache_files.push(entry);
            }
            None => {
                cache.rescanned_files += 1;
                misses.push(scan_input);
            }
        }
    }

    for attempt in scan_inputs_parallel(&repo_path_base, &misses) {
        match attempt {
            ScanAttempt::Cache(cache_file) => {
                append_cached_outcome(&cache_file.outcome, &mut files, &mut diagnostics);
                cache_files.push(cache_file);
            }
            ScanAttempt::Reject {
                diagnostics: rejection_diagnostics,
            } => diagnostics.extend(rejection_diagnostics),
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    sort_diagnostics(&mut diagnostics);

    finalize_scan_cache_write(
        &root,
        options,
        &mut cache,
        &mut diagnostics,
        cached_entry_count,
        cache_files,
    );

    Ok(ScanResult {
        files,
        diagnostics,
        cache,
    })
}

fn should_write_scan_cache(
    options: &ScanOptions,
    cache: &ScanCacheReport,
    cached_entry_count: Option<usize>,
    current_entry_count: usize,
) -> bool {
    if !options.cache_mode.writes_enabled() {
        return false;
    }
    if cache.read != ScanCacheReadStatus::Hit {
        return true;
    }
    if cache.rescanned_files > 0 {
        return true;
    }
    cached_entry_count != Some(current_entry_count)
}

fn finalize_scan_cache_write(
    root: &Path,
    options: &ScanOptions,
    cache: &mut ScanCacheReport,
    diagnostics: &mut Vec<ScanDiagnostic>,
    cached_entry_count: Option<usize>,
    cache_files: Vec<CachedFileEntry>,
) {
    if !options.cache_mode.writes_enabled() {
        return;
    }
    let Some(cache_file_path) = cache_path(root, options) else {
        return;
    };
    if should_write_scan_cache(options, cache, cached_entry_count, cache_files.len()) {
        match write_cache_to_path(&cache_file_path, root, options, cache_files) {
            Ok(()) => cache.write = ScanCacheWriteStatus::Wrote,
            Err(err) => {
                cache.write = ScanCacheWriteStatus::Error;
                diagnostics.push(ScanDiagnostic::new(
                    None,
                    format!("failed to write scan cache: {err}"),
                ));
                sort_diagnostics(diagnostics);
                debug!("failed to write scan cache, continuing: {err}");
            }
        }
    } else {
        cache.write = ScanCacheWriteStatus::Skipped;
    }
}

pub fn scan_paths<P: AsRef<Path>>(
    root: P,
    paths: &HashSet<RepoPath>,
    options: &ScanOptions,
) -> Result<ScanResult> {
    let root = canonicalize_or_original(root.as_ref());
    let repo_path_base = repo_path_base_for_scan_root(&root);
    let mut diagnostics = Vec::new();
    let mut cache = ScanCacheReport {
        read: ScanCacheReadStatus::Disabled,
        write: ScanCacheWriteStatus::Disabled,
        reused_files: 0,
        rescanned_files: 0,
    };
    let mut cache_files =
        load_scan_cache_for_read(&root, options, &mut cache, &mut diagnostics).unwrap_or_default();
    let cached_entry_count = (cache.read == ScanCacheReadStatus::Hit).then_some(cache_files.len());
    let mut files = Vec::new();
    let mut ordered_paths = paths.iter().collect::<Vec<_>>();
    ordered_paths.sort();
    let ignore_matcher = DirectScanPathIgnoreMatcher::new(options)?;
    let mut dir_cache = HashMap::new();
    let mut misses = Vec::new();

    for path in ordered_paths {
        if path.is_root() || ignore_matcher.matches(path) {
            cache_files.remove(path);
            continue;
        }

        let input = match admit_scan_path_cached(&repo_path_base, path, &mut dir_cache) {
            Ok(ScanPathAdmission::Regular {
                full_path,
                stamp,
                file_id,
            }) => ScanInput {
                path: path.clone(),
                full_path,
                stamp,
                file_id,
            },
            Ok(ScanPathAdmission::Link { component }) => {
                cache_files.remove(path);
                diagnostics.push(symbolic_link_diagnostic(path, &component));
                continue;
            }
            Ok(ScanPathAdmission::MissingOrNonFile) => {
                cache_files.remove(path);
                continue;
            }
            Err(err) => {
                cache_files.remove(path);
                let diagnostic = diagnostic_for_process_file_error(
                    &repo_path_base,
                    &repo_path_base.join(path.as_str()),
                    &err,
                );
                log_process_file_error(&diagnostic, &err);
                diagnostics.push(diagnostic);
                continue;
            }
        };

        let cached_file = cache_files.remove(&input.path);
        let reused_entry = cached_file.filter(|entry| entry.stamp == input.stamp);
        match reused_entry {
            Some(entry) => {
                cache.reused_files += 1;
                append_cached_outcome(&entry.outcome, &mut files, &mut diagnostics);
                cache_files.insert(input.path, entry);
            }
            None => {
                cache.rescanned_files += 1;
                misses.push(input);
            }
        }
    }

    let attempts = scan_inputs_parallel(&repo_path_base, &misses);
    for (input, attempt) in misses.into_iter().zip(attempts) {
        match attempt {
            ScanAttempt::Cache(cache_file) => {
                append_cached_outcome(&cache_file.outcome, &mut files, &mut diagnostics);
                cache_files.insert(input.path, cache_file);
            }
            ScanAttempt::Reject {
                diagnostics: rejection_diagnostics,
            } => diagnostics.extend(rejection_diagnostics),
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    sort_diagnostics(&mut diagnostics);

    finalize_scan_cache_write(
        &root,
        options,
        &mut cache,
        &mut diagnostics,
        cached_entry_count,
        cache_files.into_values().collect(),
    );

    Ok(ScanResult {
        files,
        diagnostics,
        cache,
    })
}

/// Matches repo-relative paths against the `[scan]` ignore rules and the
/// built-in ignored names, so every content source (worktree paths,
/// revision trees, changed-path sets) skips the same files.
pub struct DirectScanPathIgnoreMatcher<'a> {
    path_prefixes: &'a [RepoPath],
    ignored_names: HashSet<&'a str>,
    glob_matcher: Gitignore,
}

impl<'a> DirectScanPathIgnoreMatcher<'a> {
    pub fn new(options: &'a ScanOptions) -> Result<Self> {
        // Paths are repo-relative, so the glob root never strips anything.
        let mut builder = GitignoreBuilder::new(".");
        builder.allow_unclosed_class(false);
        for pattern in &options.ignore_globs {
            builder.add_line(None, pattern)?;
        }

        Ok(Self {
            path_prefixes: &options.ignore_path_prefixes,
            ignored_names: options.ignore_names.iter().map(String::as_str).collect(),
            glob_matcher: if options.ignore_globs.is_empty() {
                Gitignore::empty()
            } else {
                builder.build()?
            },
        })
    }

    pub fn matches(&self, path: &RepoPath) -> bool {
        if self
            .path_prefixes
            .iter()
            .any(|prefix| path.is_under(prefix))
        {
            return true;
        }

        path.as_str()
            .split('/')
            .any(|segment| self.ignored_names.contains(segment))
            || self
                .glob_matcher
                .matched_path_or_any_parents(Path::new(path.as_str()), false)
                .is_ignore()
    }
}

fn load_scan_cache_for_read(
    root: &Path,
    options: &ScanOptions,
    cache: &mut ScanCacheReport,
    diagnostics: &mut Vec<ScanDiagnostic>,
) -> Option<HashMap<RepoPath, CachedFileEntry>> {
    if !options.cache_mode.reads_enabled() {
        return None;
    }
    let cache_file_path = cache_path(root, options)?;

    match load_cache_entry_from_path(&cache_file_path, root, options) {
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
}

fn build_walker(root: &Path, repo_path_base: &Path, options: &ScanOptions) -> Result<ignore::Walk> {
    let matcher = IgnoreMatcher::new(root, repo_path_base, options)?;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    builder.filter_entry(move |entry| !matcher.matches(entry));
    Ok(builder.build())
}

struct IgnoreMatcher {
    walk_root: PathBuf,
    repo_path_base: PathBuf,
    names: HashSet<String>,
    path_prefixes: Vec<RepoPath>,
    glob_matcher: Gitignore,
}

impl IgnoreMatcher {
    fn new(root: &Path, repo_path_base: &Path, options: &ScanOptions) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(repo_path_base);
        builder.allow_unclosed_class(false);
        for pattern in &options.ignore_globs {
            builder.add_line(None, pattern)?;
        }

        Ok(Self {
            walk_root: root.to_path_buf(),
            repo_path_base: repo_path_base.to_path_buf(),
            names: options.ignore_names.iter().cloned().collect(),
            path_prefixes: options.ignore_path_prefixes.clone(),
            glob_matcher: if options.ignore_globs.is_empty() {
                Gitignore::empty()
            } else {
                builder.build()?
            },
        })
    }

    fn matches(&self, entry: &DirEntry) -> bool {
        let Some(name) = entry.path().file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if self.names.contains(name) {
            return true;
        }

        let is_dir = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        let relative_to_walk_root = entry
            .path()
            .strip_prefix(&self.walk_root)
            .unwrap_or(entry.path());

        if !self.path_prefixes.is_empty()
            && let Ok(repo_path) = normalize_cache_key(&self.repo_path_base, entry.path())
            && self
                .path_prefixes
                .iter()
                .any(|prefix| repo_path.is_under(prefix))
        {
            return true;
        }

        let relative_to_repo = entry
            .path()
            .strip_prefix(&self.repo_path_base)
            .unwrap_or(relative_to_walk_root);
        self.glob_matcher
            .matched_path_or_any_parents(relative_to_repo, is_dir)
            .is_ignore()
    }
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

fn file_stamp(metadata: &fs::Metadata) -> Result<FileStamp> {
    Ok(FileStamp {
        modified_at: system_time_to_epoch(metadata.modified()?),
        size: metadata.len(),
    })
}

#[derive(Debug, Clone)]
struct ScanInput {
    path: RepoPath,
    full_path: PathBuf,
    stamp: FileStamp,
    file_id: Option<(u64, u64)>,
}

/// These pathname observations exclude stable links and detect ordinary drift that they see.
/// They do not provide adversarial race containment between separate filesystem operations.
#[derive(Debug)]
enum ScanPathAdmission {
    Regular {
        full_path: PathBuf,
        stamp: FileStamp,
        file_id: Option<(u64, u64)>,
    },
    Link {
        component: RepoPath,
    },
    MissingOrNonFile,
}

#[derive(Debug, Clone)]
enum DirComponentAdmission {
    RegularDir,
    Link(RepoPath),
    MissingOrNonDir,
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn scan_inputs_parallel(repo_path_base: &Path, inputs: &[ScanInput]) -> Vec<ScanAttempt> {
    if inputs.is_empty() {
        return Vec::new();
    }
    let parallelism = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    if inputs.len() <= 2 || parallelism <= 1 {
        let attempts = inputs
            .iter()
            .map(|input| scan_file(repo_path_base, input))
            .collect();
        crate::tree_sitter_support::clear_thread_parsers();
        return attempts;
    }

    let worker_count = parallelism.min(inputs.len().div_ceil(2));
    let slots: Vec<OnceLock<ScanAttempt>> = (0..inputs.len()).map(|_| OnceLock::new()).collect();
    let next_index = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let slots = &slots;
            let next_index = &next_index;
            let _ = std::thread::Builder::new()
                .name("trueflow-scan".to_string())
                .stack_size(SCAN_WORKER_STACK_BYTES)
                .spawn_scoped(scope, move || {
                    loop {
                        let start = next_index.fetch_add(SCAN_WORKER_BATCH_SIZE, Ordering::Relaxed);
                        if start >= inputs.len() {
                            break;
                        }
                        let end = (start + SCAN_WORKER_BATCH_SIZE).min(inputs.len());
                        for idx in start..end {
                            let _ = slots[idx].set(scan_file(repo_path_base, &inputs[idx]));
                        }
                    }
                    crate::tree_sitter_support::clear_thread_parsers();
                });
        }
    });

    let mut attempts = Vec::with_capacity(inputs.len());
    let mut ran_fallback = false;
    for (idx, mut slot) in slots.into_iter().enumerate() {
        let attempt = match slot.take() {
            Some(attempt) => attempt,
            None => {
                ran_fallback = true;
                scan_file(repo_path_base, &inputs[idx])
            }
        };
        attempts.push(attempt);
    }
    if ran_fallback {
        crate::tree_sitter_support::clear_thread_parsers();
    }
    attempts
}

#[derive(Debug)]
enum ScanAttempt {
    Cache(CachedFileEntry),
    Reject { diagnostics: Vec<ScanDiagnostic> },
}

#[cfg(test)]
fn load_cache_entry(root: &Path, options: &ScanOptions) -> Result<Option<CacheEntry>> {
    let Some(cache_file_path) = cache_path(root, options) else {
        return Ok(None);
    };
    load_cache_entry_from_path(&cache_file_path, root, options)
}

#[cfg(test)]
fn write_cache(root: &Path, options: &ScanOptions, files: Vec<CachedFileEntry>) -> Result<()> {
    let cache_file_path =
        cache_path(root, options).ok_or_else(|| anyhow!("scan cache path disabled"))?;
    write_cache_to_path(&cache_file_path, root, options, files)
}

fn load_cache_entry_from_path(
    cache_path: &Path,
    root: &Path,
    options: &ScanOptions,
) -> Result<Option<CacheEntry>> {
    let contents = match fs::read_to_string(cache_path) {
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
    validate_cache_entry(&entry)?;

    Ok(Some(entry))
}

fn validate_cache_entry(entry: &CacheEntry) -> Result<()> {
    let mut seen_paths = HashSet::new();
    for file in &entry.files {
        if !seen_paths.insert(&file.path) {
            return Err(anyhow!(
                "scan cache contains duplicate entry for {}",
                file.path
            ));
        }
        if let CachedFileOutcome::Included { file_state, .. } = &file.outcome
            && file_state.path != file.path
        {
            return Err(anyhow!(
                "scan cache entry for {} contains file state for {}",
                file.path,
                file_state.path
            ));
        }
    }
    Ok(())
}

fn index_cached_files(entry: CacheEntry) -> HashMap<RepoPath, CachedFileEntry> {
    entry
        .files
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect()
}

fn write_cache_to_path(
    cache_path: &Path,
    root: &Path,
    options: &ScanOptions,
    files: Vec<CachedFileEntry>,
) -> Result<()> {
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
    // Readers and concurrent scans (other processes, TUI status workers) must
    // never observe a truncated or interleaved cache, so publish atomically.
    let file_name = cache_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary_path =
        cache_path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let result =
        fs::write(&temporary_path, contents).and_then(|()| fs::rename(&temporary_path, cache_path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result?;
    if let Some(parent) = cache_path.parent() {
        prune_scan_cache_dir(
            parent,
            cache_path,
            MAX_SCAN_CACHE_FILES,
            MAX_SCAN_CACHE_BYTES,
            SystemTime::now(),
        );
    }
    Ok(())
}

fn prune_scan_cache_dir(
    cache_dir: &Path,
    keep_file: &Path,
    max_files: usize,
    max_bytes: u64,
    now: SystemTime,
) {
    let Ok(read_dir) = fs::read_dir(cache_dir) else {
        return;
    };

    struct Candidate {
        path: PathBuf,
        size: u64,
        modified_epoch: u64,
        keep: bool,
    }

    let mut candidates = Vec::new();
    let mut total_bytes = 0_u64;

    for entry in read_dir.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        if name.starts_with(".scan-") && name.ends_with(".tmp") {
            let is_stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_SCAN_CACHE_TMP_AGE);
            if is_stale {
                let _ = fs::remove_file(&path);
            }
            continue;
        }

        if name.starts_with("scan-") && name.ends_with(".json") {
            let size = metadata.len();
            let modified_epoch = metadata.modified().map(system_time_to_epoch).unwrap_or(0);
            let keep = path == keep_file;
            total_bytes = total_bytes.saturating_add(size);
            candidates.push(Candidate {
                path,
                size,
                modified_epoch,
                keep,
            });
        }
    }

    let mut remaining_files = candidates.len();
    if remaining_files <= max_files && total_bytes <= max_bytes {
        return;
    }

    candidates.sort_by(|a, b| {
        a.modified_epoch
            .cmp(&b.modified_epoch)
            .then_with(|| a.path.cmp(&b.path))
    });

    for candidate in candidates {
        if remaining_files <= max_files && total_bytes <= max_bytes {
            break;
        }
        if candidate.keep {
            continue;
        }
        if fs::remove_file(&candidate.path).is_ok() {
            remaining_files = remaining_files.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(candidate.size);
        }
    }
}

/// The code that produced cached blocks. A cache written by any other
/// producer must miss, because unchanged bytes may now split differently.
#[derive(Debug, Clone, Copy, Serialize)]
struct CacheProducer {
    trueflow_version: &'static str,
    splitter_revision: u32,
}

impl CacheProducer {
    const CURRENT: Self = Self {
        trueflow_version: crate::build_info::VERSION,
        splitter_revision: analysis::SPLITTER_REVISION,
    };
}

fn scan_options_fingerprint(options: &ScanOptions) -> String {
    cache_fingerprint(options, CacheProducer::CURRENT)
}

fn cache_fingerprint(options: &ScanOptions, producer: CacheProducer) -> String {
    #[derive(Serialize)]
    struct CacheOptionsFingerprint<'a> {
        producer: CacheProducer,
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
        producer,
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

fn cache_path(root: &Path, options: &ScanOptions) -> Option<PathBuf> {
    let identity = cache_identity(root);
    let cache_root = resolve_cache_dir(&identity, options)?;
    let repo_name = identity
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let root_hash = hash_str(identity.to_string_lossy().as_ref());
    Some(cache_root.join(format!("scan-{repo_name}-{root_hash}.json")))
}

fn resolve_cache_dir(identity: &Path, options: &ScanOptions) -> Option<PathBuf> {
    if let Some(explicit) = &options.cache_dir {
        return Some(explicit.clone());
    }
    if let Some(env_dir) = std::env::var_os(SCAN_CACHE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(env_dir));
    }
    let default_dir = cache_dir()?.join("trueflow").join("scan");
    resolve_default_cache_dir_for_identity(identity, &default_dir, &std::env::temp_dir())
}

fn resolve_default_cache_dir_for_identity(
    identity: &Path,
    default_cache_dir: &Path,
    temp_dir: &Path,
) -> Option<PathBuf> {
    let canonical_cache = canonicalize_existing_prefix(default_cache_dir);
    // Never write a default global cache inside the scanned directory itself
    // (for example when HOME equals the repository root or when scanning $HOME).
    if canonical_cache.starts_with(identity) {
        return None;
    }
    let canonical_temp = canonicalize_existing_prefix(temp_dir);
    // Do not persist ephemeral OS temp-directory scans into a real user cache
    // outside temp_dir; isolated test homes under temp_dir remain cacheable.
    if identity.starts_with(&canonical_temp) && !canonical_cache.starts_with(&canonical_temp) {
        return None;
    }
    Some(default_cache_dir.to_path_buf())
}

fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut suffix = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            let mut resolved = canonical;
            for part in suffix.into_iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        let Some(name) = cursor.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        cursor = parent;
    }
}

fn cache_identity(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn cache_root_hash(root: &Path) -> String {
    let identity = cache_identity(root);
    hash_str(identity.to_string_lossy().as_ref())
}

fn repo_path_base_for_scan_root(root: &Path) -> PathBuf {
    gix::discover(root)
        .ok()
        .and_then(|repo| repo.workdir().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf())
}

fn canonicalize_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn collect_scan_inventory(
    root: &Path,
    repo_path_base: &Path,
    options: &ScanOptions,
    diagnostics: &mut Vec<ScanDiagnostic>,
) -> Result<Vec<ScanInput>> {
    let mut inputs = Vec::new();
    let mut dir_cache = HashMap::new();

    for entry in build_walker(root, repo_path_base, options)? {
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

        let path = normalize_cache_key(repo_path_base, entry.path())?;
        match admit_scan_path_cached(repo_path_base, &path, &mut dir_cache) {
            Ok(ScanPathAdmission::Regular {
                full_path,
                stamp,
                file_id,
            }) => {
                inputs.push(ScanInput {
                    path,
                    full_path,
                    stamp,
                    file_id,
                });
            }
            Ok(ScanPathAdmission::Link { component }) => {
                diagnostics.push(symbolic_link_diagnostic(&path, &component));
            }
            Ok(ScanPathAdmission::MissingOrNonFile) => {
                diagnostics.push(changed_during_scan_diagnostic(&path));
            }
            Err(err) => {
                let diagnostic =
                    diagnostic_for_process_file_error(repo_path_base, entry.path(), &err);
                log_process_file_error(&diagnostic, &err);
                diagnostics.push(diagnostic);
            }
        }
    }

    append_gitignored_tracked_inputs(
        root,
        repo_path_base,
        options,
        &mut inputs,
        diagnostics,
        &mut dir_cache,
    )?;
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inputs)
}

/// Git never ignores a tracked file, but the walker applies `.gitignore`
/// (and `.ignore`) rules to every path. Index-tracked regular files under
/// `root` that the walk left out are added back; trueflow's own `[scan]`
/// ignore rules still apply.
fn append_gitignored_tracked_inputs(
    root: &Path,
    repo_path_base: &Path,
    options: &ScanOptions,
    inputs: &mut Vec<ScanInput>,
    diagnostics: &mut Vec<ScanDiagnostic>,
    dir_cache: &mut HashMap<String, DirComponentAdmission>,
) -> Result<()> {
    let Ok(repo) = gix::discover(root) else {
        return Ok(());
    };
    let Ok(relative_root) = root.strip_prefix(repo_path_base) else {
        return Ok(());
    };
    if repo.workdir() != Some(repo_path_base) {
        return Ok(());
    }
    let index = match repo.index_or_empty() {
        Ok(index) => index,
        Err(err) => {
            diagnostics.push(ScanDiagnostic::new(
                None,
                format!("failed to read the git index; tracked files that .gitignore matches are not scanned: {err}"),
            ));
            return Ok(());
        }
    };
    let scope = RepoPath::from_relative_path(relative_root)?;
    let ignore = DirectScanPathIgnoreMatcher::new(options)?;
    let walked = inputs
        .iter()
        .map(|input| input.path.clone())
        .collect::<HashSet<_>>();
    let mut added = HashSet::new();

    for entry in index.entries() {
        let is_regular_file = entry.mode.to_tree_entry_mode().is_some_and(|mode| {
            matches!(
                mode.kind(),
                gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable
            )
        });
        if !is_regular_file {
            continue;
        }
        let Ok(path) = RepoPath::new(entry.path(&index).to_string()) else {
            continue;
        };
        if !path.is_under(&scope)
            || ignore.matches(&path)
            || walked.contains(&path)
            || !added.insert(path.clone())
        {
            continue;
        }
        match admit_scan_path_cached(repo_path_base, &path, dir_cache) {
            Ok(ScanPathAdmission::Regular {
                full_path,
                stamp,
                file_id,
            }) => inputs.push(ScanInput {
                path,
                full_path,
                stamp,
                file_id,
            }),
            // Deleted, replaced or reached through a link: the walk would not
            // have scanned it either.
            Ok(ScanPathAdmission::MissingOrNonFile | ScanPathAdmission::Link { .. }) => {}
            Err(err) => {
                let full_path = repo_path_base.join(path.as_str());
                let diagnostic =
                    diagnostic_for_process_file_error(repo_path_base, &full_path, &err);
                log_process_file_error(&diagnostic, &err);
                diagnostics.push(diagnostic);
            }
        }
    }
    Ok(())
}

fn scan_file(repo_path_base: &Path, input: &ScanInput) -> ScanAttempt {
    if input.file_id.is_none() {
        let admission = match admit_scan_path(repo_path_base, &input.path) {
            Ok(admission) => admission,
            Err(err) => return rejected_scan_error(repo_path_base, input, &err),
        };
        if let Some(rejection) = rejected_admission(input, admission) {
            return rejection;
        }
    }

    let mut file = match File::open(&input.full_path) {
        Ok(file) => file,
        Err(err) => {
            if let Ok(admission) = admit_scan_path(repo_path_base, &input.path)
                && let Some(rejection) = rejected_admission(input, admission)
            {
                return rejection;
            }
            let err = anyhow::Error::from(err);
            return cacheable_scan_error(repo_path_base, input, &err);
        }
    };
    match handle_matches_input(&file, input) {
        Ok(true) => {}
        Ok(false) => {
            if let Ok(admission) = admit_scan_path(repo_path_base, &input.path)
                && let Some(rejection) = rejected_admission(input, admission)
            {
                return rejection;
            }
            return rejected_changed_scan(input);
        }
        Err(err) => return rejected_scan_error(repo_path_base, input, &err),
    }

    if input.file_id.is_none() {
        let admission = match admit_scan_path(repo_path_base, &input.path) {
            Ok(admission) => admission,
            Err(err) => return rejected_scan_error(repo_path_base, input, &err),
        };
        if let Some(rejection) = rejected_admission(input, admission) {
            return rejection;
        }
    }

    let mut bytes = Vec::with_capacity(usize::try_from(input.stamp.size).unwrap_or(0));
    if let Err(err) = file.read_to_end(&mut bytes) {
        let err = anyhow::Error::from(err);
        return cacheable_scan_error(repo_path_base, input, &err);
    }

    match handle_matches_input(&file, input) {
        Ok(true) => {}
        Ok(false) => {
            if let Ok(admission) = admit_scan_path(repo_path_base, &input.path)
                && let Some(rejection) = rejected_admission(input, admission)
            {
                return rejection;
            }
            return rejected_changed_scan(input);
        }
        Err(err) => return rejected_scan_error(repo_path_base, input, &err),
    }
    if input.file_id.is_none() {
        let admission = match admit_scan_path(repo_path_base, &input.path) {
            Ok(admission) => admission,
            Err(err) => return rejected_scan_error(repo_path_base, input, &err),
        };
        if let Some(rejection) = rejected_admission(input, admission) {
            return rejection;
        }
    }

    let outcome = match analysis::parse_file(&input.path, &bytes) {
        Ok(file_scan) => CachedFileOutcome::Included {
            file_state: file_scan.file_state,
            diagnostics: file_scan
                .diagnostics
                .into_iter()
                .map(|diagnostic| ScanDiagnostic::new(Some(input.path.clone()), diagnostic.reason))
                .collect(),
        },
        Err(err) => {
            let diagnostic =
                diagnostic_for_process_file_error(repo_path_base, &input.full_path, &err);
            log_process_file_error(&diagnostic, &err);
            CachedFileOutcome::Skipped {
                diagnostics: vec![diagnostic],
            }
        }
    };

    ScanAttempt::Cache(CachedFileEntry {
        path: input.path.clone(),
        stamp: input.stamp,
        outcome,
    })
}

fn admit_scan_path(repo_path_base: &Path, path: &RepoPath) -> Result<ScanPathAdmission> {
    let mut dir_cache = HashMap::new();
    admit_scan_path_cached(repo_path_base, path, &mut dir_cache)
}

fn admit_scan_path_cached(
    repo_path_base: &Path,
    path: &RepoPath,
    dir_cache: &mut HashMap<String, DirComponentAdmission>,
) -> Result<ScanPathAdmission> {
    let components = path.as_str().split('/').collect::<Vec<_>>();
    let mut full_path = repo_path_base.to_path_buf();
    let mut prefix = String::new();

    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            prefix.push('/');
        }
        prefix.push_str(component);
        full_path.push(component);

        let is_final_component = index + 1 == components.len();
        if !is_final_component {
            if let Some(cached) = dir_cache.get(&prefix) {
                match cached {
                    DirComponentAdmission::RegularDir => continue,
                    DirComponentAdmission::Link(link) => {
                        return Ok(ScanPathAdmission::Link {
                            component: link.clone(),
                        });
                    }
                    DirComponentAdmission::MissingOrNonDir => {
                        return Ok(ScanPathAdmission::MissingOrNonFile);
                    }
                }
            }

            let metadata = match fs::symlink_metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    dir_cache.insert(prefix, DirComponentAdmission::MissingOrNonDir);
                    return Ok(ScanPathAdmission::MissingOrNonFile);
                }
                Err(err) => return Err(err.into()),
            };
            if metadata.file_type().is_symlink() {
                let link_component = RepoPath::new(prefix.clone())?;
                dir_cache.insert(prefix, DirComponentAdmission::Link(link_component.clone()));
                return Ok(ScanPathAdmission::Link {
                    component: link_component,
                });
            }
            if !metadata.file_type().is_dir() {
                dir_cache.insert(prefix, DirComponentAdmission::MissingOrNonDir);
                return Ok(ScanPathAdmission::MissingOrNonFile);
            }
            dir_cache.insert(prefix.clone(), DirComponentAdmission::RegularDir);
            continue;
        }

        let metadata = match fs::symlink_metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ScanPathAdmission::MissingOrNonFile);
            }
            Err(err) => return Err(err.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(ScanPathAdmission::Link {
                component: RepoPath::new(prefix)?,
            });
        }
        return if metadata.file_type().is_file() {
            Ok(ScanPathAdmission::Regular {
                full_path,
                stamp: file_stamp(&metadata)?,
                file_id: file_identity(&metadata),
            })
        } else {
            Ok(ScanPathAdmission::MissingOrNonFile)
        };
    }

    Ok(ScanPathAdmission::MissingOrNonFile)
}

fn rejected_admission(input: &ScanInput, admission: ScanPathAdmission) -> Option<ScanAttempt> {
    match admission {
        ScanPathAdmission::Regular { stamp, .. } if stamp == input.stamp => None,
        ScanPathAdmission::Regular { .. } | ScanPathAdmission::MissingOrNonFile => {
            Some(rejected_changed_scan(input))
        }
        ScanPathAdmission::Link { component } => Some(ScanAttempt::Reject {
            diagnostics: vec![symbolic_link_diagnostic(&input.path, &component)],
        }),
    }
}

fn handle_matches_input(file: &File, input: &ScanInput) -> Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.file_type().is_file()
        && (input.file_id.is_none() || file_identity(&metadata) == input.file_id)
        && file_stamp(&metadata)? == input.stamp)
}

fn cacheable_scan_error(
    repo_path_base: &Path,
    input: &ScanInput,
    err: &anyhow::Error,
) -> ScanAttempt {
    let diagnostic = diagnostic_for_process_file_error(repo_path_base, &input.full_path, err);
    log_process_file_error(&diagnostic, err);
    ScanAttempt::Cache(CachedFileEntry {
        path: input.path.clone(),
        stamp: input.stamp,
        outcome: CachedFileOutcome::Skipped {
            diagnostics: vec![diagnostic],
        },
    })
}

fn rejected_scan_error(
    repo_path_base: &Path,
    input: &ScanInput,
    err: &anyhow::Error,
) -> ScanAttempt {
    let diagnostic = diagnostic_for_process_file_error(repo_path_base, &input.full_path, err);
    log_process_file_error(&diagnostic, err);
    ScanAttempt::Reject {
        diagnostics: vec![diagnostic],
    }
}

fn rejected_changed_scan(input: &ScanInput) -> ScanAttempt {
    ScanAttempt::Reject {
        diagnostics: vec![changed_during_scan_diagnostic(&input.path)],
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

fn normalize_cache_key(repo_path_base: &Path, path: &Path) -> Result<RepoPath> {
    let relative = path.strip_prefix(repo_path_base).unwrap_or(path);
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
    repo_path_base: &Path,
    path: &Path,
    err: &anyhow::Error,
) -> ScanDiagnostic {
    let path = normalize_cache_key(repo_path_base, path).ok();
    ScanDiagnostic::new(path, skipped_file_reason(err))
}

/// Why a file that could not be read or parsed was left out of review.
pub(crate) fn skipped_file_reason(err: &anyhow::Error) -> String {
    if err.downcast_ref::<std::str::Utf8Error>().is_some() {
        "skipped invalid UTF-8 file".to_string()
    } else if err
        .downcast_ref::<std::io::Error>()
        .is_some_and(is_permission_denied_io)
    {
        format!("skipped unreadable file: {err}")
    } else {
        format!("skipped file: {err}")
    }
}

fn symbolic_link_diagnostic(path: &RepoPath, component: &RepoPath) -> ScanDiagnostic {
    ScanDiagnostic::new(
        Some(path.clone()),
        format!("skipped symbolic link at {component}"),
    )
}

fn changed_during_scan_diagnostic(path: &RepoPath) -> ScanDiagnostic {
    ScanDiagnostic::new(Some(path.clone()), "skipped file changed during scan")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;

    use crate::block::{Block, BlockKind, ByteSpan, LineSpan};
    use crate::test_git::temp_test_dir;
    use std::process::Command;
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
        assert!(RepoPath::new("src/generated").unwrap().is_under(&prefix));
        assert!(
            RepoPath::new("src/generated/out.rs")
                .unwrap()
                .is_under(&prefix)
        );
        assert!(!RepoPath::new("src/generate.rs").unwrap().is_under(&prefix));
    }

    #[test]
    fn scan_paths_scans_only_requested_repo_paths() {
        let repo = temp_test_dir("scanner_paths_only");
        let src = repo.join("src");
        fs::create_dir_all(&src).unwrap_or_else(|error| panic!("create src dir: {error}"));
        fs::write(src.join("keep.rs"), "fn keep() {}\n")
            .unwrap_or_else(|error| panic!("write keep file: {error}"));
        fs::write(src.join("skip.rs"), "fn skip() {}\n")
            .unwrap_or_else(|error| panic!("write skip file: {error}"));
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap_or_else(|error| panic!("git init failed: {error}"));

        let paths = HashSet::from([RepoPath::new("src/keep.rs").unwrap()]);
        let result = scan_paths(&repo, &paths, &ScanOptions::default())
            .unwrap_or_else(|error| panic!("scan paths failed: {error}"));

        let scanned_paths = result
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(scanned_paths, vec!["src/keep.rs"]);
    }

    #[test]
    fn scan_paths_keeps_default_ignored_directories_ignored() {
        let repo = temp_test_dir("scanner_paths_ignores_trueflow");
        let trueflow_dir = repo.join(".trueflow");
        fs::create_dir_all(&trueflow_dir)
            .unwrap_or_else(|error| panic!("create .trueflow dir: {error}"));
        fs::write(trueflow_dir.join("reviews.jsonl"), "review log text\n")
            .unwrap_or_else(|error| panic!("write review log: {error}"));
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap_or_else(|error| panic!("git init failed: {error}"));

        let paths = HashSet::from([RepoPath::new(".trueflow/reviews.jsonl").unwrap()]);
        let result = scan_paths(&repo, &paths, &ScanOptions::default())
            .unwrap_or_else(|error| panic!("scan paths failed: {error}"));

        assert!(result.files.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn scan_paths_respects_empty_ignore_names() {
        let repo = temp_test_dir("scanner_paths_empty_ignore_names");
        let trueflow_dir = repo.join(".trueflow");
        fs::create_dir_all(&trueflow_dir)
            .unwrap_or_else(|error| panic!("create .trueflow dir: {error}"));
        fs::write(trueflow_dir.join("reviews.jsonl"), "review log text\n")
            .unwrap_or_else(|error| panic!("write review log: {error}"));
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap_or_else(|error| panic!("git init failed: {error}"));
        let options = ScanOptions {
            ignore_names: Vec::new(),
            ..ScanOptions::default()
        };

        let paths = HashSet::from([RepoPath::new(".trueflow/reviews.jsonl").unwrap()]);
        let result = scan_paths(&repo, &paths, &options)
            .unwrap_or_else(|error| panic!("scan paths failed: {error}"));

        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec![".trueflow/reviews.jsonl"]
        );
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn scan_paths_keeps_custom_ignored_directories_ignored() {
        let repo = temp_test_dir("scanner_paths_ignores_custom_names");
        let generated_dir = repo.join("generated");
        fs::create_dir_all(&generated_dir)
            .unwrap_or_else(|error| panic!("create generated dir: {error}"));
        fs::write(generated_dir.join("fixture.rs"), "fn generated() {}\n")
            .unwrap_or_else(|error| panic!("write generated file: {error}"));
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap_or_else(|error| panic!("git init failed: {error}"));
        let mut options = ScanOptions::default();
        options.ignore_names.push("generated".to_string());

        let paths = HashSet::from([RepoPath::new("generated/fixture.rs").unwrap()]);
        let result = scan_paths(&repo, &paths, &options)
            .unwrap_or_else(|error| panic!("scan paths failed: {error}"));

        assert!(result.files.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn scan_paths_keeps_custom_globs_ignored() {
        let repo = temp_test_dir("scanner_paths_ignores_custom_globs");
        let src = repo.join("src");
        fs::create_dir_all(&src).unwrap_or_else(|error| panic!("create src dir: {error}"));
        fs::write(src.join("snapshot.snap"), "generated snapshot\n")
            .unwrap_or_else(|error| panic!("write snapshot file: {error}"));
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap_or_else(|error| panic!("git init failed: {error}"));
        let options = ScanOptions {
            ignore_globs: vec!["*.snap".to_string()],
            ..ScanOptions::default()
        };

        let paths = HashSet::from([RepoPath::new("src/snapshot.snap").unwrap()]);
        let result = scan_paths(&repo, &paths, &options)
            .unwrap_or_else(|error| panic!("scan paths failed: {error}"));

        assert!(result.files.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn scan_paths_writes_and_reuses_requested_file_cache() {
        let repo = temp_test_dir("scanner_paths_writes_cache");
        let cache_dir = temp_test_dir("scanner_paths_writes_cache_store");
        fs::create_dir_all(repo.join("src")).unwrap_or_else(|error| panic!("create src: {error}"));
        fs::write(repo.join("src/keep.rs"), "fn keep() {}\n")
            .unwrap_or_else(|error| panic!("write keep file: {error}"));
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };
        let paths = HashSet::from([RepoPath::new("src/keep.rs").unwrap()]);

        let initial = scan_paths(&repo, &paths, &options)
            .unwrap_or_else(|error| panic!("initial scan paths failed: {error}"));
        assert_eq!(initial.cache.read, ScanCacheReadStatus::Miss);
        assert_eq!(initial.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(initial.cache.reused_files, 0);
        assert_eq!(initial.cache.rescanned_files, 1);

        let warm = scan_paths(&repo, &paths, &options)
            .unwrap_or_else(|error| panic!("warm scan paths failed: {error}"));
        assert_eq!(warm.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(warm.cache.write, ScanCacheWriteStatus::Skipped);
        assert_eq!(warm.cache.reused_files, 1);
        assert_eq!(warm.cache.rescanned_files, 0);
        assert_eq!(
            warm.files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/keep.rs"]
        );
    }

    #[test]
    fn scan_cache_rejects_included_file_state_with_mismatched_path() {
        let repo = temp_test_dir("scanner_cache_mismatched_payload_path");
        let cache_dir = temp_test_dir("scanner_cache_mismatched_payload_path_store");
        fs::create_dir_all(repo.join("src")).unwrap_or_else(|error| panic!("create src: {error}"));
        let path = repo.join("src/lib.rs");
        fs::write(&path, "fn keep() {}\n")
            .unwrap_or_else(|error| panic!("write lib file: {error}"));
        let metadata = fs::metadata(&path).unwrap_or_else(|error| panic!("metadata: {error}"));
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };
        let cache_path = RepoPath::new("src/lib.rs").unwrap();
        let mismatched_file_state = FileState::from_text(
            RepoPath::new("other.rs").unwrap(),
            Language::Rust,
            b"fn other() {}\n",
            vec![
                Block::from_file_range(
                    "fn other() {}\n",
                    BlockKind::Function,
                    ByteSpan::new(0, "fn other() {}\n".len()),
                )
                .unwrap(),
            ],
        );
        write_cache(
            &repo,
            &options,
            vec![CachedFileEntry {
                path: cache_path,
                stamp: FileStamp {
                    modified_at: system_time_to_epoch(
                        metadata
                            .modified()
                            .unwrap_or_else(|error| panic!("modified time: {error}")),
                    ),
                    size: metadata.len(),
                },
                outcome: CachedFileOutcome::Included {
                    file_state: mismatched_file_state,
                    diagnostics: Vec::new(),
                },
            }],
        )
        .unwrap_or_else(|error| panic!("write mismatched cache: {error}"));

        let result = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("scan with mismatched cache: {error}"));

        assert_eq!(result.cache.read, ScanCacheReadStatus::Error);
        assert_eq!(result.cache.rescanned_files, 1);
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );
    }

    #[test]
    fn scan_paths_preserves_unrequested_cache_entries() {
        let repo = temp_test_dir("scanner_paths_preserves_cache");
        let cache_dir = temp_test_dir("scanner_paths_preserves_cache_store");
        fs::create_dir_all(repo.join("src")).unwrap_or_else(|error| panic!("create src: {error}"));
        fs::write(repo.join("src/a.rs"), "fn a() -> u32 { 1 }\n")
            .unwrap_or_else(|error| panic!("write a file: {error}"));
        fs::write(repo.join("src/b.rs"), "fn b() -> u32 { 2 }\n")
            .unwrap_or_else(|error| panic!("write b file: {error}"));
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };

        let full = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("initial directory scan failed: {error}"));
        assert_eq!(full.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(full.cache.rescanned_files, 2);

        fs::write(repo.join("src/a.rs"), "fn a() -> u32 { 3 }\n")
            .unwrap_or_else(|error| panic!("rewrite a file: {error}"));
        let paths = HashSet::from([RepoPath::new("src/a.rs").unwrap()]);
        let targeted = scan_paths(&repo, &paths, &options)
            .unwrap_or_else(|error| panic!("targeted scan failed: {error}"));
        assert_eq!(targeted.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(targeted.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(targeted.cache.reused_files, 0);
        assert_eq!(targeted.cache.rescanned_files, 1);

        let warm_full = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("warm directory scan failed: {error}"));
        assert_eq!(warm_full.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(warm_full.cache.write, ScanCacheWriteStatus::Skipped);
        assert_eq!(warm_full.cache.reused_files, 2);
        assert_eq!(warm_full.cache.rescanned_files, 0);
    }

    #[test]
    fn scan_directory_uses_repo_root_relative_paths_from_subdir_roots() {
        let repo_root = std::env::temp_dir().join("trueflow_tests").join(format!(
            "scanner_repo_root_relative_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(repo_root.join("src/nested")).unwrap();
        fs::write(repo_root.join("src/nested/lib.rs"), "fn demo() {}\n").unwrap();

        let status = Command::new("git")
            .arg("init")
            .current_dir(&repo_root)
            .status()
            .unwrap();
        assert!(status.success());

        let scan = scan_directory(repo_root.join("src"), &ScanOptions::default())
            .unwrap_or_else(|error| panic!("scan from subdir root: {error}"));
        let paths = scan
            .files
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![RepoPath::new("src/nested/lib.rs").unwrap()]);
    }

    #[test]
    fn scan_directory_matches_ignore_globs_against_repo_paths_from_subdir_roots() {
        let repo_root = std::env::temp_dir().join("trueflow_tests").join(format!(
            "scanner_subdir_ignore_glob_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(repo_root.join("src")).unwrap();
        fs::write(repo_root.join("src/lib.rs"), "fn demo() {}\n").unwrap();
        fs::write(repo_root.join("src/generated.snap"), "generated snapshot\n").unwrap();

        let status = Command::new("git")
            .arg("init")
            .current_dir(&repo_root)
            .status()
            .unwrap();
        assert!(status.success());

        let options = ScanOptions {
            ignore_globs: vec!["src/*.snap".to_string()],
            ..ScanOptions::default()
        };
        let scan = scan_directory(repo_root.join("src"), &options)
            .unwrap_or_else(|error| panic!("scan from subdir root: {error}"));
        let paths = scan
            .files
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![RepoPath::new("src/lib.rs").unwrap()]);
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
    fn scan_cache_skips_write_on_complete_cache_hit() {
        let repo = temp_test_dir("scanner_cache_complete_hit");
        let cache_dir = temp_test_dir("scanner_cache_complete_hit_cache");
        fs::create_dir_all(repo.join("src")).unwrap_or_else(|error| panic!("create src: {error}"));
        fs::write(repo.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n")
            .unwrap_or_else(|error| panic!("write source: {error}"));
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };

        let initial = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("initial scan failed: {error}"));
        assert_eq!(initial.cache.read, ScanCacheReadStatus::Miss);
        assert_eq!(initial.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(initial.cache.reused_files, 0);
        assert_eq!(initial.cache.rescanned_files, 1);

        let warm = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("warm scan failed: {error}"));
        assert_eq!(warm.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(warm.cache.write, ScanCacheWriteStatus::Skipped);
        assert_eq!(warm.cache.reused_files, 1);
        assert_eq!(warm.cache.rescanned_files, 0);
        assert_eq!(warm.files.len(), 1);
    }

    #[test]
    fn scan_cache_rewrites_when_cached_file_is_deleted() {
        let repo = temp_test_dir("scanner_cache_deleted_file");
        let cache_dir = temp_test_dir("scanner_cache_deleted_file_cache");
        fs::create_dir_all(repo.join("src")).unwrap_or_else(|error| panic!("create src: {error}"));
        fs::write(repo.join("src/a.rs"), "pub fn a() -> u32 { 1 }\n")
            .unwrap_or_else(|error| panic!("write a: {error}"));
        fs::write(repo.join("src/b.rs"), "pub fn b() -> u32 { 2 }\n")
            .unwrap_or_else(|error| panic!("write b: {error}"));
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };

        let initial = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("initial scan failed: {error}"));
        assert_eq!(initial.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(initial.cache.rescanned_files, 2);

        fs::remove_file(repo.join("src/b.rs")).unwrap_or_else(|error| panic!("delete b: {error}"));
        let pruned = scan_directory(&repo, &options)
            .unwrap_or_else(|error| panic!("pruned scan failed: {error}"));

        assert_eq!(pruned.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(pruned.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(pruned.cache.reused_files, 1);
        assert_eq!(pruned.cache.rescanned_files, 0);
        assert_eq!(
            pruned
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs"]
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

    #[cfg(unix)]
    fn symlink_external_file(link_path: &Path, contents: &str) -> PathBuf {
        use std::os::unix::fs::symlink;

        let outside = temp_test_dir("scanner_external_symlink");
        fs::create_dir_all(&outside)
            .unwrap_or_else(|error| panic!("create external directory: {error}"));
        let target = outside.join("sentinel.rs");
        fs::write(&target, contents).unwrap_or_else(|error| panic!("write sentinel: {error}"));
        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create link parent: {error}"));
        }
        symlink(&target, link_path).unwrap_or_else(|error| panic!("create symlink: {error}"));
        outside
    }

    #[cfg(unix)]
    fn scanner_cache_options(cache_dir: PathBuf) -> ScanOptions {
        ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_paths_rejects_external_symlink_and_evicts_cache() {
        const SENTINEL: &str = "TRUEFLOW_OUTSIDE_SYMLINK_SENTINEL_17";

        let repo = temp_test_dir("scanner_targeted_symlink");
        let cache_dir = temp_test_dir("scanner_targeted_symlink_cache");
        let path = RepoPath::new("src/link.rs").unwrap();
        let full_path = repo.join(path.as_str());
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        fs::write(&full_path, "pub fn local() {}\n").unwrap();
        let options = scanner_cache_options(cache_dir);
        let paths = HashSet::from([path.clone()]);

        let warmed = scan_paths(&repo, &paths, &options).unwrap();
        assert_eq!(warmed.cache.read, ScanCacheReadStatus::Miss);
        assert_eq!(warmed.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(warmed.cache.rescanned_files, 1);
        assert!(
            load_cache_entry(&repo, &options)
                .unwrap()
                .unwrap()
                .files
                .iter()
                .any(|entry| entry.path == path)
        );

        fs::remove_file(&full_path).unwrap();
        let _outside =
            symlink_external_file(&full_path, &format!("pub fn {SENTINEL}_escaped() {{}}\n"));
        let rejected = scan_paths(&repo, &paths, &options).unwrap();

        assert!(rejected.files.is_empty());
        assert!(
            rejected
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.as_ref() == Some(&path)
                    && diagnostic.reason.contains("symbolic link"))
        );
        assert_eq!(rejected.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(rejected.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(rejected.cache.reused_files, 0);
        assert_eq!(rejected.cache.rescanned_files, 0);
        let cached = load_cache_entry(&repo, &options).unwrap().unwrap();
        assert!(!cached.files.iter().any(|entry| entry.path == path));
        let serialized = fs::read_to_string(cache_path(&repo, &options).unwrap()).unwrap();
        assert!(!serialized.contains(SENTINEL));
        assert!(!serialized.contains("src/link.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn scan_directory_prunes_cached_path_replaced_by_symlink() {
        const SENTINEL: &str = "TRUEFLOW_OUTSIDE_SYMLINK_SENTINEL_17";

        let repo = temp_test_dir("scanner_directory_symlink");
        let cache_dir = temp_test_dir("scanner_directory_symlink_cache");
        let real_path = RepoPath::new("src/real.rs").unwrap();
        let link_path = RepoPath::new("src/link.rs").unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join(real_path.as_str()), "pub fn real() {}\n").unwrap();
        let link_full_path = repo.join(link_path.as_str());
        fs::write(&link_full_path, "pub fn local() {}\n").unwrap();
        let options = scanner_cache_options(cache_dir);

        let warmed = scan_directory(&repo, &options).unwrap();
        assert_eq!(warmed.cache.rescanned_files, 2);
        fs::remove_file(&link_full_path).unwrap();
        let _outside = symlink_external_file(
            &link_full_path,
            &format!("pub fn {SENTINEL}_escaped() {{}}\n"),
        );

        let pruned = scan_directory(&repo, &options).unwrap();
        assert_eq!(pruned.cache.read, ScanCacheReadStatus::Hit);
        assert_eq!(pruned.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(pruned.cache.reused_files, 1);
        assert_eq!(pruned.cache.rescanned_files, 0);
        assert_eq!(
            pruned
                .files
                .iter()
                .map(|file| &file.path)
                .collect::<Vec<_>>(),
            vec![&real_path]
        );
        let cached = load_cache_entry(&repo, &options).unwrap().unwrap();
        assert_eq!(cached.files.len(), 1);
        assert_eq!(cached.files[0].path, real_path);
        assert!(!cached.files.iter().any(|entry| entry.path == link_path));
    }

    #[cfg(unix)]
    #[test]
    fn scan_paths_rejects_file_beneath_symlinked_directory() {
        const SENTINEL: &str = "TRUEFLOW_OUTSIDE_SYMLINK_SENTINEL_17";
        use std::os::unix::fs::symlink;

        let repo = temp_test_dir("scanner_ancestor_symlink");
        let cache_dir = temp_test_dir("scanner_ancestor_symlink_cache");
        let outside = temp_test_dir("scanner_ancestor_symlink_outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("secret.rs"),
            format!("pub fn {SENTINEL}_escaped() {{}}\n"),
        )
        .unwrap();
        symlink(&outside, repo.join("linked")).unwrap();
        let path = RepoPath::new("linked/secret.rs").unwrap();
        let options = scanner_cache_options(cache_dir);
        let result = scan_paths(&repo, &HashSet::from([path.clone()]), &options).unwrap();

        assert!(result.files.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.as_ref() == Some(&path)
                    && diagnostic.reason.contains("symbolic link"))
        );
        assert!(
            load_cache_entry(&repo, &options)
                .unwrap()
                .unwrap()
                .files
                .is_empty()
        );
        assert!(!serde_json::to_string(&result).unwrap().contains(SENTINEL));
    }

    #[cfg(unix)]
    #[test]
    fn scan_paths_rejects_broken_final_symlink() {
        use std::os::unix::fs::symlink;

        let repo = temp_test_dir("scanner_broken_symlink");
        let cache_dir = temp_test_dir("scanner_broken_symlink_cache");
        let path = RepoPath::new("src/link.rs").unwrap();
        let full_path = repo.join(path.as_str());
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        symlink(
            "/trueflow-missing-symlink-target-for-scanner-test",
            &full_path,
        )
        .unwrap();
        let options = scanner_cache_options(cache_dir);

        let result = scan_paths(&repo, &HashSet::from([path.clone()]), &options).unwrap();
        assert!(result.files.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.as_ref() == Some(&path)
                    && diagnostic.reason.contains("symbolic link"))
        );
        assert!(
            load_cache_entry(&repo, &options)
                .unwrap()
                .unwrap()
                .files
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_file_discards_path_replaced_by_symlink_after_inventory() {
        const SENTINEL: &str = "TRUEFLOW_OUTSIDE_SYMLINK_SENTINEL_17";

        let repo = temp_test_dir("scanner_read_drift_symlink");
        let path = RepoPath::new("src/link.rs").unwrap();
        let full_path = repo.join(path.as_str());
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        fs::write(&full_path, "pub fn local() {}\n").unwrap();
        let input = match admit_scan_path(&repo, &path).unwrap() {
            ScanPathAdmission::Regular {
                full_path,
                stamp,
                file_id,
            } => ScanInput {
                path: path.clone(),
                full_path,
                stamp,
                file_id,
            },
            admission => panic!("expected regular admission, got {admission:?}"),
        };

        fs::remove_file(&full_path).unwrap();
        let _outside =
            symlink_external_file(&full_path, &format!("pub fn {SENTINEL}_escaped() {{}}\n"));
        let attempt = scan_file(&repo, &input);

        match attempt {
            ScanAttempt::Reject { diagnostics } => assert!(diagnostics.iter().any(|diagnostic| {
                diagnostic.path.as_ref() == Some(&path)
                    && diagnostic.reason.contains("symbolic link")
            })),
            ScanAttempt::Cache(entry) => {
                panic!("replaced symlink must not be cacheable: {entry:?}")
            }
        }
    }

    #[test]
    fn scan_cache_rejects_immediately_previous_format_entry_with_followed_link_bytes() {
        const PRE_FIX_SCAN_CACHE_FORMAT_VERSION: u32 = SCAN_CACHE_FORMAT_VERSION - 1;
        const POISON: &[u8] = b"pub fn evil() {}\n";
        const SAFE: &[u8] = b"pub fn safe() {}\n";
        assert_eq!(POISON.len(), SAFE.len());

        let repo = temp_test_dir("scanner_prefixed_cache");
        let cache_dir = temp_test_dir("scanner_prefixed_cache_store");
        let path = RepoPath::new("src/link.rs").unwrap();
        let full_path = repo.join(path.as_str());
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        fs::write(&full_path, SAFE).unwrap();
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };
        let metadata = fs::symlink_metadata(&full_path).unwrap();
        let poisoned_file_state = FileState::from_text(
            path.clone(),
            Language::Rust,
            POISON,
            vec![
                Block::from_file_range(
                    std::str::from_utf8(POISON).unwrap(),
                    BlockKind::Function,
                    ByteSpan::new(0, POISON.len()),
                )
                .unwrap(),
            ],
        );
        let cache_entry = CacheEntry {
            format_version: PRE_FIX_SCAN_CACHE_FORMAT_VERSION,
            root_hash: cache_root_hash(&repo),
            options_fingerprint: scan_options_fingerprint(&options),
            files: vec![CachedFileEntry {
                path: path.clone(),
                stamp: file_stamp(&metadata).unwrap(),
                outcome: CachedFileOutcome::Included {
                    file_state: poisoned_file_state,
                    diagnostics: Vec::new(),
                },
            }],
        };
        let cache_file = cache_path(&repo, &options).unwrap();
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, serde_json::to_string(&cache_entry).unwrap()).unwrap();

        let result = scan_paths(&repo, &HashSet::from([path]), &options).unwrap();
        assert_eq!(result.cache.read, ScanCacheReadStatus::Miss);
        assert_eq!(result.cache.rescanned_files, 1);
        assert_eq!(result.cache.write, ScanCacheWriteStatus::Wrote);
        let serialized_result = serde_json::to_string(&result).unwrap();
        assert!(!serialized_result.contains("evil"));
        assert!(serialized_result.contains("safe"));
        let rewritten = fs::read_to_string(cache_file).unwrap();
        assert!(!rewritten.contains("evil"));
        assert!(rewritten.contains("safe"));
    }

    #[test]
    fn scan_cache_rejects_pre_source_integrity_format() {
        const PRE_SOURCE_INTEGRITY_FORMAT_VERSION: u32 = SCAN_CACHE_FORMAT_VERSION - 1;
        let source = "import os\nx = 1\n";
        let stale_content = "import osx = 1";
        let repo = temp_test_dir("scanner_pre_source_integrity_cache");
        let cache_dir = temp_test_dir("scanner_pre_source_integrity_cache_store");
        let path = RepoPath::new("src/example.py").unwrap();
        let full_path = repo.join(path.as_str());
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        fs::write(&full_path, source).unwrap();
        let options = ScanOptions {
            cache_dir: Some(cache_dir),
            ..ScanOptions::default()
        };
        let metadata = fs::metadata(&full_path).unwrap();
        let stale_file_state = FileState::from_text(
            path.clone(),
            Language::Python,
            source.as_bytes(),
            vec![Block::new(
                stale_content.to_string(),
                BlockKind::Code,
                LineSpan::new(0, 2),
                ByteSpan::new(0, stale_content.len()),
            )],
        );
        let stale_entry = CacheEntry {
            format_version: PRE_SOURCE_INTEGRITY_FORMAT_VERSION,
            root_hash: cache_root_hash(&repo),
            options_fingerprint: scan_options_fingerprint(&options),
            files: vec![CachedFileEntry {
                path: path.clone(),
                stamp: file_stamp(&metadata).unwrap(),
                outcome: CachedFileOutcome::Included {
                    file_state: stale_file_state,
                    diagnostics: Vec::new(),
                },
            }],
        };
        let cache_file = cache_path(&repo, &options).unwrap();
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, serde_json::to_string(&stale_entry).unwrap()).unwrap();

        let result = scan_paths(&repo, &HashSet::from([path]), &options).unwrap();
        assert_eq!(result.cache.read, ScanCacheReadStatus::Miss);
        assert_eq!(result.cache.reused_files, 0);
        assert_eq!(result.cache.rescanned_files, 1);
        assert_eq!(result.cache.write, ScanCacheWriteStatus::Wrote);
        assert_eq!(result.files.len(), 1);
        let block = &result.files[0].blocks[0];
        assert_eq!(block.content, &source[0..15]);
        assert_eq!((block.start_byte, block.end_byte), (0, 15));
        assert_eq!(
            block.hash,
            crate::hashing::TreeHash::from_content(&source[0..15])
        );

        let rewritten = fs::read_to_string(cache_file).unwrap();
        assert!(!rewritten.contains(stale_content));
        let cache_entry = load_cache_entry(&repo, &options).unwrap();
        let Some(cache_entry) = cache_entry else {
            panic!("rescanned cache should be readable");
        };
        assert_eq!(cache_entry.format_version, SCAN_CACHE_FORMAT_VERSION);
    }

    #[test]
    fn scan_cache_rejects_entries_from_another_producer() {
        let source = "pub fn current() {}\n";
        let stale_content = "pub fn stale() {}\n";
        let path = RepoPath::new("src/lib.rs").unwrap();
        let other_producers = [
            CacheProducer {
                splitter_revision: analysis::SPLITTER_REVISION + 1,
                ..CacheProducer::CURRENT
            },
            CacheProducer {
                trueflow_version: "0.0.0-older",
                ..CacheProducer::CURRENT
            },
        ];

        for (index, producer) in other_producers.into_iter().enumerate() {
            let repo = temp_test_dir(&format!("scanner_other_producer_cache_{index}"));
            let cache_dir = temp_test_dir(&format!("scanner_other_producer_store_{index}"));
            let full_path = repo.join(path.as_str());
            fs::create_dir_all(full_path.parent().unwrap()).unwrap();
            fs::write(&full_path, source).unwrap();
            let options = ScanOptions {
                cache_dir: Some(cache_dir),
                ..ScanOptions::default()
            };
            let metadata = fs::metadata(&full_path).unwrap();
            let stale_entry = CacheEntry {
                format_version: SCAN_CACHE_FORMAT_VERSION,
                root_hash: cache_root_hash(&repo),
                options_fingerprint: cache_fingerprint(&options, producer),
                files: vec![CachedFileEntry {
                    path: path.clone(),
                    stamp: file_stamp(&metadata).unwrap(),
                    outcome: CachedFileOutcome::Included {
                        file_state: FileState::from_text(
                            path.clone(),
                            Language::Rust,
                            source.as_bytes(),
                            vec![Block::new(
                                stale_content.to_string(),
                                BlockKind::Function,
                                LineSpan::new(0, 1),
                                ByteSpan::new(0, stale_content.len()),
                            )],
                        ),
                        diagnostics: Vec::new(),
                    },
                }],
            };
            let cache_file = cache_path(&repo, &options).unwrap();
            fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
            fs::write(&cache_file, serde_json::to_string(&stale_entry).unwrap()).unwrap();

            let result = scan_paths(&repo, &HashSet::from([path.clone()]), &options).unwrap();

            assert_eq!(
                result.cache.read,
                ScanCacheReadStatus::Miss,
                "{producer:?} must not be reused"
            );
            assert_eq!(result.files[0].blocks[0].content, source.trim_end());
        }
    }

    #[test]
    fn resolve_default_cache_dir_skips_temp_roots_and_nested_caches() {
        let root_parent = temp_test_dir("scanner_default_cache_rules");
        let fake_temp = root_parent.join("tmp");
        let fake_home = root_parent.join("home");
        let real_cache = fake_home.join("Library/Caches/trueflow/scan");
        let proj = root_parent.join("src/proj");
        let temp_proj = fake_temp.join("test-repo");
        let temp_home_cache = fake_temp.join("isolated-home/Library/Caches/trueflow/scan");
        fs::create_dir_all(&fake_temp).unwrap();
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&temp_proj).unwrap();
        fs::create_dir_all(temp_home_cache.parent().unwrap()).unwrap();

        let proj_identity = cache_identity(&proj);
        let temp_proj_identity = cache_identity(&temp_proj);
        let home_identity = cache_identity(&fake_home);

        assert_eq!(
            resolve_default_cache_dir_for_identity(&proj_identity, &real_cache, &fake_temp),
            Some(real_cache.clone())
        );
        assert_eq!(
            resolve_default_cache_dir_for_identity(&temp_proj_identity, &real_cache, &fake_temp),
            None,
            "ephemeral temp roots must not pollute a user cache outside temp_dir"
        );
        assert_eq!(
            resolve_default_cache_dir_for_identity(
                &temp_proj_identity,
                &temp_home_cache,
                &fake_temp
            ),
            Some(temp_home_cache),
            "isolated temp homes used by CLI tests should still resolve default cache paths"
        );
        assert_eq!(
            resolve_default_cache_dir_for_identity(&home_identity, &real_cache, &fake_temp),
            None,
            "default cache must not be created inside the scanned root itself"
        );

        let _ = fs::remove_dir_all(&root_parent);
    }

    #[test]
    fn prune_scan_cache_dir_evicts_oldest_by_count_and_bytes_and_sweeps_stale_temps() {
        let cache_dir = temp_test_dir("scanner_prune_cache_dir");
        fs::create_dir_all(&cache_dir).unwrap();

        let stale_tmp = cache_dir.join(".scan-old-0000.json.1111.tmp");
        let fresh_tmp = cache_dir.join(".scan-fresh-0000.json.2222.tmp");
        fs::write(&stale_tmp, b"partial").unwrap();
        fs::write(&fresh_tmp, b"in-flight").unwrap();

        let a = cache_dir.join("scan-a-0001.json");
        let b = cache_dir.join("scan-b-0002.json");
        let c = cache_dir.join("scan-c-0003.json");
        let keep = cache_dir.join("scan-keep-0004.json");
        fs::write(&a, vec![b'a'; 100]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&b, vec![b'b'; 100]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&c, vec![b'c'; 100]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&keep, vec![b'k'; 100]).unwrap();

        prune_scan_cache_dir(&cache_dir, &keep, 4, 1000, SystemTime::now());
        assert!(fresh_tmp.exists(), "fresh .tmp file must not be swept");
        fs::remove_file(&fresh_tmp).unwrap();

        let future_for_stale =
            SystemTime::now() + STALE_SCAN_CACHE_TMP_AGE + Duration::from_secs(5);
        prune_scan_cache_dir(&cache_dir, &keep, 3, 250, future_for_stale);

        assert!(!stale_tmp.exists(), "stale .tmp file should be swept");
        assert!(!a.exists(), "oldest entry a should be evicted");
        assert!(
            !b.exists(),
            "second-oldest entry b should be evicted to meet 250-byte cap"
        );
        assert!(
            c.exists(),
            "newer entry c should survive within 2-file / 200-byte budget"
        );
        assert!(keep.exists(), "kept file must always survive");

        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn scan_directory_parallel_preserves_deterministic_order() {
        let repo = temp_test_dir("scanner_parallel_order");
        fs::create_dir_all(repo.join("src/sub")).unwrap();
        let mut expected = Vec::new();
        for idx in (0..24_u32).rev() {
            let rel = if idx % 2 == 0 {
                format!("src/file_{idx:02}.rs")
            } else {
                format!("src/sub/file_{idx:02}.rs")
            };
            fs::write(
                repo.join(&rel),
                format!(
                    "pub fn f_{idx:02}(x: i32) -> i32 {{ if x > 0 {{ x + {idx} }} else {{ 0 }} }}\n"
                ),
            )
            .unwrap();
            expected.push(rel);
        }
        expected.sort();

        let result = scan_directory(&repo, &ScanOptions::default()).unwrap();
        let actual: Vec<_> = result
            .files
            .iter()
            .map(|file| file.path.as_str().to_string())
            .collect();
        assert_eq!(actual, expected);

        let _ = fs::remove_dir_all(&repo);
    }

    #[cfg(windows)]
    #[test]
    fn scan_paths_rejects_windows_external_symlink() {
        use std::os::windows::fs::symlink_file;

        let repo = temp_test_dir("scanner_windows_symlink");
        let outside = temp_test_dir("scanner_windows_symlink_outside");
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join("sentinel.rs");
        let path = RepoPath::new("src/link.rs").unwrap();
        let link_path = repo.join(path.as_str());
        fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        fs::write(&target, "pub fn sentinel() {}\n").unwrap();
        match symlink_file(&target, &link_path) {
            Ok(()) => {}
            // Windows symlink creation may need Developer Mode or elevated privilege.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
            {
                return;
            }
            Err(error) => panic!("create Windows symlink: {error}"),
        }

        let result = scan_paths(
            &repo,
            &HashSet::from([path.clone()]),
            &ScanOptions::default(),
        )
        .unwrap();
        assert!(result.files.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.as_ref() == Some(&path)
                    && diagnostic.reason.contains("symbolic link"))
        );
    }
}
