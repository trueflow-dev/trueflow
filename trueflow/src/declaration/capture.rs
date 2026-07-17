use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use gix::bstr::ByteSlice;
use gix::object::tree::{EntryKind, EntryMode};
use ignore::WalkBuilder;
use ignore::gitignore::GitignoreBuilder;

use crate::analysis::Language;
use crate::commands::review::ResolvedReviewQuery;
use crate::declaration::snapshot::{
    PathPairEvidence, SnapshotId, SnapshotPair, SnapshotPairId, SourceSnapshot,
};
use crate::hashing::{BytesHash, hash_bytes};
use crate::repo_path::RepoPath;
use crate::store::{BlockState, CommitId, RepoRef, VcsSystem};
use crate::targets::{ReviewDiffSelection, ReviewDiffTarget, ReviewPathSelection};
use crate::vcs::{self, ChangedPath};

const CAPTURE_DRIFT_ERROR: &str = "worktree changed during declaration capture; retry";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureEndpointProvenance {
    pub repo_ref: RepoRef,
    pub block_state: BlockState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureProvenance {
    pub base: Option<CaptureEndpointProvenance>,
    pub head: CaptureEndpointProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureDiagnostic {
    pub path: Option<PathBuf>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureBatch {
    pub pairs: Vec<SnapshotPair>,
    pub provenance: CaptureProvenance,
    pub diagnostics: Vec<CaptureDiagnostic>,
}

pub fn capture_declaration_sources(
    repo_root: &Path,
    query: &ResolvedReviewQuery,
) -> Result<Vec<CaptureBatch>> {
    capture_declaration_sources_with_hook(repo_root, query, || Ok(()))
}

pub fn capture_declaration_sources_with_hook<F>(
    repo_root: &Path,
    query: &ResolvedReviewQuery,
    hook: F,
) -> Result<Vec<CaptureBatch>>
where
    F: FnOnce() -> Result<()>,
{
    let repo = gix::discover(repo_root)
        .with_context(|| format!("git repository required at {}", repo_root.display()))?;
    let workdir = repo
        .workdir()
        .context("declaration capture requires a non-bare repository")?
        .to_path_buf();

    match &query.diff_selection {
        ReviewDiffSelection::Targets(targets) => {
            let mut batches = Vec::with_capacity(targets.len());
            for target in targets {
                batches.push(capture_immutable_target(&repo, query, target)?);
            }
            hook()?;
            Ok(batches)
        }
        ReviewDiffSelection::None => match dirty_changed_paths(&query.path_selection) {
            Some(changed) => {
                capture_dirty(&repo, &workdir, query, changed, hook).map(|batch| vec![batch])
            }
            None => {
                capture_worktree_inventory(&repo, &workdir, query, hook).map(|batch| vec![batch])
            }
        },
    }
}

fn capture_immutable_target(
    repo: &gix::Repository,
    query: &ResolvedReviewQuery,
    target: &ReviewDiffTarget,
) -> Result<CaptureBatch> {
    let (base_commit, head_commit) = immutable_endpoints(repo, target)?;
    let base_tree = base_commit.as_ref().map(gix::Commit::tree).transpose()?;
    let head_tree = head_commit.tree()?;
    let changed = changed_paths(repo, base_tree.as_ref(), Some(&head_tree))?;
    let base_id = base_commit.as_ref().map(commit_id).transpose()?;
    let head_id = commit_id(&head_commit)?;
    let target_key = format!(
        "{}..{}",
        base_id.as_ref().map_or("empty", CommitId::as_str),
        head_id.as_str()
    );

    let mut pairs = Vec::new();
    for changed_path in changed {
        if !changed_pair_selected(&query.path_selection, &changed_path) {
            continue;
        }
        let Some(language) = declaration_language(&changed_path.location) else {
            continue;
        };
        let base = base_tree
            .as_ref()
            .map(|tree| {
                snapshot_from_tree(
                    tree,
                    &changed_path.source_location,
                    language,
                    "base",
                    &target_key,
                )
            })
            .transpose()?
            .flatten();
        let head = snapshot_from_tree(
            &head_tree,
            &changed_path.location,
            language,
            "head",
            &target_key,
        )?;
        if base.is_none() && head.is_none() {
            continue;
        }
        pairs.push(snapshot_pair(
            target_key.as_str(),
            &changed_path,
            base,
            head,
        ));
    }

    Ok(CaptureBatch {
        pairs,
        provenance: CaptureProvenance {
            base: base_id.map(|revision| endpoint(revision, BlockState::Committed)),
            head: endpoint(head_id, BlockState::Committed),
        },
        diagnostics: Vec::new(),
    })
}

fn immutable_endpoints<'repo>(
    repo: &'repo gix::Repository,
    target: &ReviewDiffTarget,
) -> Result<(Option<gix::Commit<'repo>>, gix::Commit<'repo>)> {
    match target {
        ReviewDiffTarget::MainDiff => {
            let head = repo.head_commit()?;
            let main = vcs::mainline_commit(repo)?;
            let base = match repo.merge_base(head.id().detach(), main.id().detach()) {
                Ok(id) => repo.find_commit(id.detach())?,
                Err(_) => main,
            };
            Ok((Some(base), head))
        }
        ReviewDiffTarget::Revision(revision) => {
            let head = resolved_commit(repo, revision, "revision")?;
            let base = head
                .parent_ids()
                .next()
                .map(|id| repo.find_commit(id))
                .transpose()?;
            Ok((base, head))
        }
        ReviewDiffTarget::RevisionRange(range) => Ok((
            Some(resolved_commit(repo, &range.start, "start revision")?),
            resolved_commit(repo, &range.end, "end revision")?,
        )),
    }
}

fn resolved_commit<'repo>(
    repo: &'repo gix::Repository,
    revision: &CommitId,
    description: &str,
) -> Result<gix::Commit<'repo>> {
    let object = repo
        .rev_parse_single(revision.as_str())
        .with_context(|| format!("{description} `{revision}` could not be resolved"))?
        .object()?;
    object
        .peel_to_commit()
        .with_context(|| format!("{description} `{revision}` must resolve to a commit"))
}

fn commit_id(commit: &gix::Commit<'_>) -> Result<CommitId> {
    CommitId::new(commit.id().detach().to_string())
}

fn changed_paths(
    repo: &gix::Repository,
    base_tree: Option<&gix::Tree<'_>>,
    head_tree: Option<&gix::Tree<'_>>,
) -> Result<Vec<ChangedPath>> {
    let changes = repo.diff_tree_to_tree(base_tree, head_tree, None)?;
    let mut paths = Vec::new();
    for change in changes {
        let change = change.to_ref();
        if change.location().is_empty() || !is_blob_change(&change) {
            continue;
        }
        paths.push(ChangedPath {
            source_location: RepoPath::new(change.source_location().to_str_lossy().as_ref())?,
            location: RepoPath::new(change.location().to_str_lossy().as_ref())?,
        });
    }
    paths.sort_by(|left, right| {
        left.location
            .cmp(&right.location)
            .then_with(|| left.source_location.cmp(&right.source_location))
    });
    Ok(paths)
}

fn is_blob_change(change: &gix::diff::tree_with_rewrites::ChangeRef<'_>) -> bool {
    let is_blob = |mode: EntryMode| {
        matches!(
            mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
        )
    };
    let (mode, _) = change.entry_mode_and_id();
    let (source_mode, _) = change.source_entry_mode_and_id();
    is_blob(mode) || is_blob(source_mode)
}

fn capture_dirty<F>(
    repo: &gix::Repository,
    workdir: &Path,
    query: &ResolvedReviewQuery,
    changed: &HashSet<ChangedPath>,
    hook: F,
) -> Result<CaptureBatch>
where
    F: FnOnce() -> Result<()>,
{
    let head = repo.head_commit()?;
    let head_id = commit_id(&head)?;
    let head_tree = head.tree()?;
    let status_before = vcs::dirty_files(repo)?;
    let mut selected = changed
        .iter()
        .filter(|path| changed_pair_selected(&query.path_selection, path))
        .filter_map(|path| {
            declaration_language(&path.location).map(|language| (path.clone(), language))
        })
        .collect::<Vec<_>>();
    selected.sort_by(|(left, _), (right, _)| {
        left.location
            .cmp(&right.location)
            .then_with(|| left.source_location.cmp(&right.source_location))
    });

    let mut pairs = Vec::with_capacity(selected.len());
    let mut fingerprints = Vec::with_capacity(selected.len());
    let target_key = format!("dirty:{}", head_id.as_str());
    for (changed_path, language) in &selected {
        let base = snapshot_from_tree(
            &head_tree,
            &changed_path.source_location,
            *language,
            "base",
            &target_key,
        )?;
        let (head_snapshot, fingerprint) = snapshot_from_worktree(
            workdir,
            &changed_path.location,
            *language,
            "head",
            &target_key,
        )?;
        fingerprints.push((changed_path.location.clone(), fingerprint));
        if base.is_none() && head_snapshot.is_none() {
            continue;
        }
        pairs.push(snapshot_pair(
            &target_key,
            changed_path,
            base,
            head_snapshot,
        ));
    }

    hook()?;
    validate_worktree_generation(repo, workdir, &head_id, &status_before, &fingerprints)?;

    Ok(CaptureBatch {
        pairs,
        provenance: CaptureProvenance {
            base: Some(endpoint(head_id.clone(), BlockState::Committed)),
            head: endpoint(head_id, BlockState::Uncommitted),
        },
        diagnostics: Vec::new(),
    })
}

fn capture_worktree_inventory<F>(
    repo: &gix::Repository,
    workdir: &Path,
    query: &ResolvedReviewQuery,
    hook: F,
) -> Result<CaptureBatch>
where
    F: FnOnce() -> Result<()>,
{
    let head_id = commit_id(&repo.head_commit()?)?;
    let status_before = vcs::dirty_files(repo)?;
    let paths = worktree_source_paths(workdir, query)?;
    let mut pairs = Vec::with_capacity(paths.len());
    let mut fingerprints = Vec::with_capacity(paths.len());
    let target_key = format!("worktree:{}", head_id.as_str());
    for (path, language) in paths {
        let (head, fingerprint) =
            snapshot_from_worktree(workdir, &path, language, "head", &target_key)?;
        fingerprints.push((path.clone(), fingerprint));
        if let Some(head) = head {
            pairs.push(snapshot_pair(
                &target_key,
                &ChangedPath::identity(path),
                None,
                Some(head),
            ));
        }
    }

    hook()?;
    validate_worktree_generation(repo, workdir, &head_id, &status_before, &fingerprints)?;
    let uncommitted = status_before.iter().any(|path| {
        declaration_language(path).is_some() && worktree_path_selected(&query.path_selection, path)
    });

    Ok(CaptureBatch {
        pairs,
        provenance: CaptureProvenance {
            base: None,
            head: endpoint(
                head_id,
                if uncommitted {
                    BlockState::Uncommitted
                } else {
                    BlockState::Committed
                },
            ),
        },
        diagnostics: Vec::new(),
    })
}

fn worktree_source_paths(
    workdir: &Path,
    query: &ResolvedReviewQuery,
) -> Result<Vec<(RepoPath, Language)>> {
    let mut glob_builder = GitignoreBuilder::new(workdir);
    glob_builder.allow_unclosed_class(false);
    for pattern in &query.scan_options.ignore_globs {
        glob_builder.add_line(None, pattern)?;
    }
    let glob_matcher = glob_builder.build()?;
    let ignore_names = query
        .scan_options
        .ignore_names
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let ignore_prefixes = query.scan_options.ignore_path_prefixes.clone();
    let root = workdir.to_path_buf();

    let mut builder = WalkBuilder::new(workdir);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    builder.filter_entry(move |entry| {
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return true;
        }
        if entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| ignore_names.contains(name))
        {
            return false;
        }
        let Ok(repo_path) = RepoPath::from_relative_path(relative) else {
            return false;
        };
        if ignore_prefixes
            .iter()
            .any(|prefix| repo_path.is_under(prefix))
        {
            return false;
        }
        !glob_matcher
            .matched_path_or_any_parents(
                entry.path(),
                entry.file_type().is_some_and(|kind| kind.is_dir()),
            )
            .is_ignore()
    });

    let mut paths = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(workdir)
            .context("worktree walker returned a path outside the repository")?;
        let path = RepoPath::from_relative_path(relative)?;
        if !worktree_path_selected(&query.path_selection, &path) {
            continue;
        }
        if let Some(language) = declaration_language(&path) {
            paths.push((path, language));
        }
    }
    paths.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(paths)
}

fn snapshot_from_tree(
    tree: &gix::Tree<'_>,
    path: &RepoPath,
    language: Language,
    endpoint_name: &str,
    target_key: &str,
) -> Result<Option<SourceSnapshot>> {
    let Some(entry) = tree.lookup_entry_by_path(Path::new(path.as_str()))? else {
        return Ok(None);
    };
    if !matches!(
        entry.mode().kind(),
        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
    ) {
        return Ok(None);
    }
    let blob = entry.object()?.try_into_blob()?;
    let source = std::str::from_utf8(&blob.data)
        .with_context(|| format!("{} contains invalid UTF-8", path.as_str()))?;
    Ok(Some(source_snapshot(
        path,
        language,
        source,
        endpoint_name,
        target_key,
    )))
}

fn snapshot_from_worktree(
    workdir: &Path,
    path: &RepoPath,
    language: Language,
    endpoint_name: &str,
    target_key: &str,
) -> Result<(Option<SourceSnapshot>, FileFingerprint)> {
    let absolute = workdir.join(path.as_str());
    let before = file_metadata(&absolute)?;
    let Some(before) = before else {
        return Ok((None, FileFingerprint::missing()));
    };
    if !before.is_file {
        return Ok((None, FileFingerprint::from_metadata(before, None)));
    }
    let bytes = fs::read(&absolute).with_context(|| format!("failed to read {}", path.as_str()))?;
    let after = file_metadata(&absolute)?.ok_or_else(|| anyhow!(CAPTURE_DRIFT_ERROR))?;
    if before != after {
        return Err(anyhow!(CAPTURE_DRIFT_ERROR));
    }
    let source = std::str::from_utf8(&bytes)
        .with_context(|| format!("{} contains invalid UTF-8", path.as_str()))?;
    let fingerprint = FileFingerprint::from_metadata(after, Some(BytesHash::from_bytes(&bytes)));
    Ok((
        Some(source_snapshot(
            path,
            language,
            source,
            endpoint_name,
            target_key,
        )),
        fingerprint,
    ))
}

fn source_snapshot(
    path: &RepoPath,
    language: Language,
    source: &str,
    endpoint_name: &str,
    target_key: &str,
) -> SourceSnapshot {
    let bytes_hash = BytesHash::from_bytes(source.as_bytes());
    let identity = hash_bytes(
        format!(
            "{target_key}\0{endpoint_name}\0{}\0{}",
            path.as_str(),
            bytes_hash.as_str()
        )
        .as_bytes(),
    );
    SourceSnapshot::new(
        SnapshotId::new(format!("source:{identity}")),
        Path::new(path.as_str()),
        language,
        source,
    )
}

fn snapshot_pair(
    target_key: &str,
    changed_path: &ChangedPath,
    base: Option<SourceSnapshot>,
    head: Option<SourceSnapshot>,
) -> SnapshotPair {
    let identity = hash_bytes(
        format!(
            "{target_key}\0{}\0{}\0{}\0{}",
            changed_path.source_location.as_str(),
            changed_path.location.as_str(),
            base.as_ref()
                .map_or("missing", |snapshot| snapshot.id.as_str()),
            head.as_ref()
                .map_or("missing", |snapshot| snapshot.id.as_str())
        )
        .as_bytes(),
    );
    let evidence = if changed_path.source_location == changed_path.location {
        PathPairEvidence::SamePath
    } else {
        PathPairEvidence::ExplicitRename
    };
    SnapshotPair::new(
        SnapshotPairId::new(format!("pair:{identity}")),
        base,
        head,
        evidence,
    )
}

fn endpoint(revision: CommitId, block_state: BlockState) -> CaptureEndpointProvenance {
    CaptureEndpointProvenance {
        repo_ref: RepoRef::Vcs {
            system: VcsSystem::Git,
            revision,
        },
        block_state,
    }
}

fn declaration_language(path: &RepoPath) -> Option<Language> {
    let path = Path::new(path.as_str());
    let language = path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(Language::from_extension)
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(Language::from_file_name)
        })?;
    matches!(
        language,
        Language::Rust
            | Language::Swift
            | Language::Elisp
            | Language::JavaScript
            | Language::TypeScript
            | Language::Java
            | Language::Kotlin
            | Language::CSharp
            | Language::Python
            | Language::Ruby
            | Language::Php
            | Language::Go
            | Language::C
            | Language::Cpp
            | Language::Zig
            | Language::Lua
            | Language::Dart
            | Language::Scala
            | Language::Haskell
            | Language::OCaml
            | Language::Elixir
            | Language::Clojure
            | Language::Sql
            | Language::Shell
            | Language::Nix
            | Language::Just
    )
    .then_some(language)
}

fn dirty_changed_paths(selection: &ReviewPathSelection) -> Option<&HashSet<ChangedPath>> {
    match selection {
        ReviewPathSelection::Scoped {
            changed: Some(changed),
            ..
        } => Some(changed),
        ReviewPathSelection::All
        | ReviewPathSelection::Empty
        | ReviewPathSelection::Scoped { changed: None, .. } => None,
    }
}

fn changed_pair_selected(selection: &ReviewPathSelection, changed_path: &ChangedPath) -> bool {
    match selection {
        ReviewPathSelection::All => true,
        ReviewPathSelection::Empty => false,
        ReviewPathSelection::Scoped {
            files,
            dirs,
            changed,
        } => {
            let explicitly_selected = files.is_empty() && dirs.is_empty()
                || explicit_path_selected(files, dirs, &changed_path.source_location)
                || explicit_path_selected(files, dirs, &changed_path.location);
            explicitly_selected
                && changed.as_ref().is_none_or(|selected| {
                    selected.iter().any(|candidate| {
                        candidate.source_location == changed_path.source_location
                            && candidate.location == changed_path.location
                    })
                })
        }
    }
}

fn worktree_path_selected(selection: &ReviewPathSelection, path: &RepoPath) -> bool {
    match selection {
        ReviewPathSelection::All => true,
        ReviewPathSelection::Empty => false,
        ReviewPathSelection::Scoped {
            files,
            dirs,
            changed,
        } => changed.as_ref().map_or_else(
            || files.is_empty() && dirs.is_empty() || explicit_path_selected(files, dirs, path),
            |changed| {
                changed.iter().any(|candidate| {
                    candidate.location == *path
                        && (files.is_empty() && dirs.is_empty()
                            || explicit_path_selected(files, dirs, &candidate.source_location)
                            || explicit_path_selected(files, dirs, &candidate.location))
                })
            },
        ),
    }
}

fn explicit_path_selected(files: &HashSet<RepoPath>, dirs: &[RepoPath], path: &RepoPath) -> bool {
    files.contains(path) || dirs.iter().any(|directory| path.is_under(directory))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMetadata {
    is_file: bool,
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    metadata: Option<FileMetadata>,
    bytes_hash: Option<BytesHash>,
}

impl FileFingerprint {
    fn missing() -> Self {
        Self {
            metadata: None,
            bytes_hash: None,
        }
    }

    fn from_metadata(metadata: FileMetadata, bytes_hash: Option<BytesHash>) -> Self {
        Self {
            metadata: Some(metadata),
            bytes_hash,
        }
    }
}

fn file_metadata(path: &Path) -> Result<Option<FileMetadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(FileMetadata {
            is_file: metadata.file_type().is_file(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn fingerprint_file(workdir: &Path, path: &RepoPath) -> Result<FileFingerprint> {
    let absolute = workdir.join(path.as_str());
    let before = file_metadata(&absolute)?;
    let Some(before) = before else {
        return Ok(FileFingerprint::missing());
    };
    if !before.is_file {
        return Ok(FileFingerprint::from_metadata(before, None));
    }
    let bytes = fs::read(&absolute)?;
    let after = file_metadata(&absolute)?.ok_or_else(|| anyhow!(CAPTURE_DRIFT_ERROR))?;
    if before != after {
        return Err(anyhow!(CAPTURE_DRIFT_ERROR));
    }
    Ok(FileFingerprint::from_metadata(
        after,
        Some(BytesHash::from_bytes(&bytes)),
    ))
}

fn validate_worktree_generation(
    repo: &gix::Repository,
    workdir: &Path,
    expected_head: &CommitId,
    expected_status: &HashSet<RepoPath>,
    fingerprints: &[(RepoPath, FileFingerprint)],
) -> Result<()> {
    let current_head = repo
        .head_commit()
        .ok()
        .and_then(|commit| commit_id(&commit).ok());
    if current_head.as_ref() != Some(expected_head)
        || vcs::dirty_files(repo).as_ref().ok() != Some(expected_status)
    {
        return Err(anyhow!(CAPTURE_DRIFT_ERROR));
    }
    for (path, expected) in fingerprints {
        if fingerprint_file(workdir, path).as_ref().ok() != Some(expected) {
            return Err(anyhow!(CAPTURE_DRIFT_ERROR));
        }
    }
    Ok(())
}
