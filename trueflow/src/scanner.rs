use crate::analysis::{self, FileType, Language};
use crate::block::FileState;
use crate::block_splitter;
use crate::hashing::hash_str;
use crate::repo_path::RepoPath;
use anyhow::{Result, anyhow};
use dirs::home_dir;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, WalkBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

const DEFAULT_IGNORE_NAMES: &[&str] =
    &[".git", ".trueflow", "target", "node_modules", "mutants.out"];
const SCAN_CACHE_FORMAT_VERSION: u32 = 3;

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

    let cached_entry = load_scan_cache_for_read(&root, options, &mut cache, &mut diagnostics);
    let cached_entry_count = cached_entry.as_ref().map(HashMap::len);

    let inventory = collect_scan_inventory(&root, &repo_path_base, options, &mut diagnostics)?;

    let mut files = Vec::new();
    let mut cache_files = Vec::with_capacity(inventory.len());

    for scan_input in inventory {
        let reused_entry = cached_entry
            .as_ref()
            .and_then(|entries| entries.get(&scan_input.path))
            .filter(|entry| entry.stamp == scan_input.stamp);

        let attempt = match reused_entry {
            Some(entry) => {
                cache.reused_files += 1;
                ScanAttempt::Cache(entry.clone())
            }
            None => {
                cache.rescanned_files += 1;
                scan_file(&repo_path_base, &scan_input)
            }
        };

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
    if should_write_scan_cache(options, cache, cached_entry_count, cache_files.len()) {
        match write_cache(root, options, cache_files) {
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
    } else if options.cache_mode.writes_enabled() {
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
    let ignore_matcher = DirectScanPathIgnoreMatcher::new(&repo_path_base, options)?;

    for path in ordered_paths {
        if path.is_root() || ignore_matcher.matches(path) {
            cache_files.remove(path);
            continue;
        }

        let input = match admit_scan_path(&repo_path_base, path) {
            Ok(ScanPathAdmission::Regular { full_path, stamp }) => ScanInput {
                path: path.clone(),
                full_path,
                stamp,
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
        let attempt = match reused_entry {
            Some(entry) => {
                cache.reused_files += 1;
                ScanAttempt::Cache(entry)
            }
            None => {
                cache.rescanned_files += 1;
                scan_file(&repo_path_base, &input)
            }
        };
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

struct DirectScanPathIgnoreMatcher<'a> {
    path_prefixes: &'a [RepoPath],
    ignored_names: HashSet<&'a str>,
    glob_matcher: Gitignore,
}

impl<'a> DirectScanPathIgnoreMatcher<'a> {
    fn new(repo_path_base: &Path, options: &'a ScanOptions) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(repo_path_base);
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

    fn matches(&self, path: &RepoPath) -> bool {
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
}

/// These pathname observations exclude stable links and detect ordinary drift that they see.
/// They do not provide adversarial race containment between separate filesystem operations.
#[derive(Debug)]
enum ScanPathAdmission {
    Regular {
        full_path: PathBuf,
        stamp: FileStamp,
    },
    Link {
        component: RepoPath,
    },
    MissingOrNonFile,
}

#[derive(Debug)]
enum ScanAttempt {
    Cache(CachedFileEntry),
    Reject { diagnostics: Vec<ScanDiagnostic> },
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
        match admit_scan_path(repo_path_base, &path) {
            Ok(ScanPathAdmission::Regular { full_path, stamp }) => {
                inputs.push(ScanInput {
                    path,
                    full_path,
                    stamp,
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

    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inputs)
}

fn scan_file(repo_path_base: &Path, input: &ScanInput) -> ScanAttempt {
    let admission = match admit_scan_path(repo_path_base, &input.path) {
        Ok(admission) => admission,
        Err(err) => return rejected_scan_error(repo_path_base, input, err),
    };
    if let Some(rejection) = rejected_admission(input, admission) {
        return rejection;
    }

    let mut file = match File::open(&input.full_path) {
        Ok(file) => file,
        Err(err) => return cacheable_scan_error(repo_path_base, input, err.into()),
    };
    match handle_matches_input(&file, input) {
        Ok(true) => {}
        Ok(false) => return rejected_changed_scan(input),
        Err(err) => return rejected_scan_error(repo_path_base, input, err),
    }

    let admission = match admit_scan_path(repo_path_base, &input.path) {
        Ok(admission) => admission,
        Err(err) => return rejected_scan_error(repo_path_base, input, err),
    };
    if let Some(rejection) = rejected_admission(input, admission) {
        return rejection;
    }

    let mut bytes = Vec::new();
    if let Err(err) = file.read_to_end(&mut bytes) {
        return cacheable_scan_error(repo_path_base, input, err.into());
    }

    match handle_matches_input(&file, input) {
        Ok(true) => {}
        Ok(false) => return rejected_changed_scan(input),
        Err(err) => return rejected_scan_error(repo_path_base, input, err),
    }
    let admission = match admit_scan_path(repo_path_base, &input.path) {
        Ok(admission) => admission,
        Err(err) => return rejected_scan_error(repo_path_base, input, err),
    };
    if let Some(rejection) = rejected_admission(input, admission) {
        return rejection;
    }

    let outcome = match process_file(&input.path, &bytes) {
        Ok(file_scan) => CachedFileOutcome::Included {
            file_state: file_scan.file_state,
            diagnostics: file_scan.diagnostics,
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
    let components = path.as_str().split('/').collect::<Vec<_>>();
    let mut full_path = repo_path_base.to_path_buf();

    for (index, component) in components.iter().enumerate() {
        full_path.push(component);
        let metadata = match fs::symlink_metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ScanPathAdmission::MissingOrNonFile);
            }
            Err(err) => return Err(err.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(ScanPathAdmission::Link {
                component: RepoPath::new(components[..=index].join("/"))?,
            });
        }

        let is_final_component = index + 1 == components.len();
        if is_final_component {
            return if metadata.file_type().is_file() {
                Ok(ScanPathAdmission::Regular {
                    full_path,
                    stamp: file_stamp(&metadata)?,
                })
            } else {
                Ok(ScanPathAdmission::MissingOrNonFile)
            };
        }
        if !metadata.file_type().is_dir() {
            return Ok(ScanPathAdmission::MissingOrNonFile);
        }
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
    Ok(metadata.file_type().is_file() && file_stamp(&metadata)? == input.stamp)
}

fn cacheable_scan_error(
    repo_path_base: &Path,
    input: &ScanInput,
    err: anyhow::Error,
) -> ScanAttempt {
    let diagnostic = diagnostic_for_process_file_error(repo_path_base, &input.full_path, &err);
    log_process_file_error(&diagnostic, &err);
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
    err: anyhow::Error,
) -> ScanAttempt {
    let diagnostic = diagnostic_for_process_file_error(repo_path_base, &input.full_path, &err);
    log_process_file_error(&diagnostic, &err);
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

fn symbolic_link_diagnostic(path: &RepoPath, component: &RepoPath) -> ScanDiagnostic {
    ScanDiagnostic::new(
        Some(path.clone()),
        format!("skipped symbolic link at {component}"),
    )
}

fn changed_during_scan_diagnostic(path: &RepoPath) -> ScanDiagnostic {
    ScanDiagnostic::new(Some(path.clone()), "skipped file changed during scan")
}

#[derive(Debug, Clone)]
struct ProcessedFile {
    file_state: FileState,
    diagnostics: Vec<ScanDiagnostic>,
}

fn process_file(normalized_path: &RepoPath, bytes: &[u8]) -> Result<ProcessedFile> {
    let file_type = analysis::analyze_file(Path::new(normalized_path.as_str()), bytes);

    if matches!(file_type, FileType::Binary) {
        return Ok(ProcessedFile {
            file_state: FileState::from_binary(normalized_path.clone(), bytes),
            diagnostics: Vec::new(),
        });
    }

    let content = std::str::from_utf8(bytes)?;
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
            normalized_path.clone(),
            language,
            bytes,
            split_result.into_review_blocks(),
        ),
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;
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
            vec![Block::new(
                "fn other() {}\n".to_string(),
                crate::block::BlockKind::Function,
                1,
                2,
            )],
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
            ScanPathAdmission::Regular { full_path, stamp } => ScanInput {
                path: path.clone(),
                full_path,
                stamp,
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
        const PRE_FIX_SCAN_CACHE_FORMAT_VERSION: u32 = 2;
        const POISON: &[u8] = b"pub fn evil() {}\n";
        const SAFE: &[u8] = b"pub fn safe() {}\n";
        assert_eq!(POISON.len(), SAFE.len());
        assert_eq!(
            PRE_FIX_SCAN_CACHE_FORMAT_VERSION,
            SCAN_CACHE_FORMAT_VERSION - 1
        );

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
            vec![Block::new(
                String::from_utf8(POISON.to_vec()).unwrap(),
                crate::block::BlockKind::Function,
                1,
                2,
            )],
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

        let result = scan_paths(&repo, &HashSet::from([path.clone()]), &options).unwrap();
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
