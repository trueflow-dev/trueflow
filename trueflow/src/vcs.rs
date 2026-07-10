use crate::analysis::Language;
use crate::block::{Block, FileState};
use crate::block_splitter;
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::store::CommitId;
use anyhow::{Context, Result};
use gix::bstr::ByteSlice;
use gix::object::tree::{EntryKind, EntryMode};
use gix::status::UntrackedFiles;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct RepoSnapshot {
    pub repo_ref_revision: Option<CommitId>,
    repo: Option<gix::Repository>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunkLine {
    pub kind: DiffLineKind,
    /// Original line content without the leading unified-diff prefix.
    /// Trailing newlines are preserved if present.
    pub text: String,
}

impl DiffHunkLine {
    pub fn context(text: impl Into<String>) -> Self {
        Self {
            kind: DiffLineKind::Context,
            text: text.into(),
        }
    }

    pub fn added(text: impl Into<String>) -> Self {
        Self {
            kind: DiffLineKind::Added,
            text: text.into(),
        }
    }

    pub fn removed(text: impl Into<String>) -> Self {
        Self {
            kind: DiffLineKind::Removed,
            text: text.into(),
        }
    }

    fn from_unified_line(line: &str) -> Option<Self> {
        if let Some(text) = line.strip_prefix(' ') {
            return Some(Self::context(format!("{text}\n")));
        }
        if let Some(text) = line.strip_prefix('+') {
            return Some(Self::added(format!("{text}\n")));
        }
        if let Some(text) = line.strip_prefix('-') {
            return Some(Self::removed(format!("{text}\n")));
        }
        None
    }

    fn display_text(&self) -> &str {
        self.text.trim_end_matches('\n')
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub file_path: RepoPath,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffHunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
    pub is_focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileDiffUnavailableReason {
    Binary,
    External,
}

/// A confirmed tree change's old-tree lookup and new-tree display locations.
///
/// `location` is always the destination/current path. `source_location` differs
/// only for rewrites emitted by gix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangedPath {
    pub source_location: RepoPath,
    pub location: RepoPath,
}

impl ChangedPath {
    pub fn identity(location: RepoPath) -> Self {
        Self {
            source_location: location.clone(),
            location,
        }
    }

    fn from_change(change: &gix::diff::tree_with_rewrites::ChangeRef<'_>) -> Result<Self> {
        Ok(Self {
            source_location: RepoPath::new(change.source_location().to_str_lossy().as_ref())?,
            location: RepoPath::new(change.location().to_str_lossy().as_ref())?,
        })
    }

    fn from_change_with_location(
        change: &gix::diff::tree_with_rewrites::ChangeRef<'_>,
        location: RepoPath,
    ) -> Result<Self> {
        Ok(Self {
            source_location: RepoPath::new(change.source_location().to_str_lossy().as_ref())?,
            location,
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct SelectedFileDiffs {
    by_destination: HashMap<RepoPath, FileDiff>,
}

impl SelectedFileDiffs {
    pub(crate) fn take(&mut self, destination: &RepoPath) -> FileDiff {
        self.by_destination
            .remove(destination)
            .unwrap_or_else(|| FileDiff::NoTextChanges {
                changed_path: ChangedPath::identity(destination.clone()),
            })
    }
}

#[cfg(test)]
thread_local! {
    static FILE_DIFF_TREE_TRAVERSALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FILE_DIFF_INSPECTED_CHANGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_file_diff_test_counters() {
    FILE_DIFF_TREE_TRAVERSALS.with(|counter| counter.set(0));
    FILE_DIFF_INSPECTED_CHANGES.with(|counter| counter.set(0));
}

#[cfg(test)]
pub(crate) fn file_diff_test_counters() -> (usize, usize) {
    (
        FILE_DIFF_TREE_TRAVERSALS.with(std::cell::Cell::get),
        FILE_DIFF_INSPECTED_CHANGES.with(std::cell::Cell::get),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileDiff {
    Text {
        changed_path: ChangedPath,
        hunks: Vec<DiffHunk>,
    },
    NoTextChanges {
        changed_path: ChangedPath,
    },
    Unavailable {
        changed_path: ChangedPath,
        reason: FileDiffUnavailableReason,
    },
}

impl FileDiff {
    pub(crate) fn changed_path(&self) -> &ChangedPath {
        match self {
            Self::Text { changed_path, .. }
            | Self::NoTextChanges { changed_path }
            | Self::Unavailable { changed_path, .. } => changed_path,
        }
    }
    pub(crate) fn hunks(&self) -> &[DiffHunk] {
        match self {
            Self::Text { hunks, .. } => hunks,
            Self::NoTextChanges { .. } | Self::Unavailable { .. } => &[],
        }
    }

    pub(crate) fn into_hunks(self) -> Vec<DiffHunk> {
        match self {
            Self::Text { hunks, .. } => hunks,
            Self::NoTextChanges { .. } | Self::Unavailable { .. } => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockDiffFocusMode {
    WholeBlock,
    ChangedWithContext { context_lines: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockDiffChangeKind {
    NoTextChanges,
    OnlyNonreviewableChurn,
    ReviewableChanges,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DiffBlockOwnership<'a> {
    BaseOnly(&'a Block),
    HeadOnly(&'a Block),
    Matched { base: &'a Block, head: &'a Block },
}

#[derive(Debug, Clone)]
pub(crate) struct DiffChangedLineIndex {
    changed_lines: Vec<IndexedChangedDiffLine>,
    base_changed_line_indices: Vec<usize>,
    head_changed_line_indices: Vec<usize>,
}

#[derive(Debug, Clone)]
struct IndexedChangedDiffLine {
    diff_order: usize,
    line: DiffLine,
}

impl DiffChangedLineIndex {
    pub(crate) fn from_hunks(hunks: &[DiffHunk]) -> Self {
        let mut changed_lines = Vec::new();
        for hunk in hunks {
            push_indexed_changed_lines_for_hunk(&mut changed_lines, hunk);
        }

        let mut base_changed_line_indices = changed_lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| line.line.old_line.is_some().then_some(index))
            .collect::<Vec<_>>();
        base_changed_line_indices.sort_by_key(|index| changed_lines[*index].line.old_line);

        let mut head_changed_line_indices = changed_lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| line.line.new_line.is_some().then_some(index))
            .collect::<Vec<_>>();
        head_changed_line_indices.sort_by_key(|index| changed_lines[*index].line.new_line);

        Self {
            changed_lines,
            base_changed_line_indices,
            head_changed_line_indices,
        }
    }

    pub(crate) fn change_kind_for_block(
        &self,
        ownership: DiffBlockOwnership<'_>,
    ) -> BlockDiffChangeKind {
        let mut changed_lines = match ownership {
            DiffBlockOwnership::BaseOnly(base) => self.changed_lines_in_base_range(base),
            DiffBlockOwnership::HeadOnly(head) => self.changed_lines_in_head_range(head),
            DiffBlockOwnership::Matched { base, head } => {
                let mut changed_lines = self.changed_lines_in_base_range(base);
                changed_lines.extend(self.changed_lines_in_head_range(head));
                changed_lines
            }
        };
        changed_lines.sort_unstable_by_key(|line| line.diff_order);

        if changed_lines.is_empty() {
            return BlockDiffChangeKind::NoTextChanges;
        }

        if all_changed_lines_are_nonreviewable(&changed_lines) {
            BlockDiffChangeKind::OnlyNonreviewableChurn
        } else {
            BlockDiffChangeKind::ReviewableChanges
        }
    }

    fn changed_lines_in_base_range(&self, block: &Block) -> Vec<&IndexedChangedDiffLine> {
        self.changed_lines_in_range(
            &self.base_changed_line_indices,
            block_diff_line_range(block),
            |line| line.old_line,
        )
    }

    fn changed_lines_in_head_range(&self, block: &Block) -> Vec<&IndexedChangedDiffLine> {
        self.changed_lines_in_range(
            &self.head_changed_line_indices,
            block_diff_line_range(block),
            |line| line.new_line,
        )
    }

    fn changed_lines_in_range(
        &self,
        line_indices: &[usize],
        range: std::ops::Range<u32>,
        line_number: impl Fn(&DiffLine) -> Option<u32>,
    ) -> Vec<&IndexedChangedDiffLine> {
        let first = line_indices.partition_point(|index| {
            line_number(&self.changed_lines[*index].line).is_some_and(|line| line < range.start)
        });
        let end = line_indices[first..].partition_point(|index| {
            line_number(&self.changed_lines[*index].line).is_some_and(|line| line < range.end)
        }) + first;
        line_indices[first..end]
            .iter()
            .map(|index| &self.changed_lines[*index])
            .collect()
    }
}

fn block_diff_line_range(block: &Block) -> std::ops::Range<u32> {
    let start = usize_to_u32_saturating(block.start_line).saturating_add(1);
    let end = usize_to_u32_saturating(block.end_line).saturating_add(1);
    start..end
}

pub struct GitConfig {
    pub email: String,
    pub signing_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: CommitId,
    pub summary: String,
}

pub fn repo_from_workdir() -> Result<gix::Repository> {
    Ok(gix::discover(".")?)
}

pub fn git_root_from_workdir() -> Result<Option<PathBuf>> {
    let repo = repo_from_workdir()?;
    Ok(repo.workdir().map(Path::to_path_buf))
}

pub fn snapshot_from_workdir() -> RepoSnapshot {
    let repo = repo_from_workdir().ok();
    let repo_ref_revision = repo
        .as_ref()
        .and_then(|repo| repo.head_id().ok())
        .and_then(|id| CommitId::new(id.detach().to_string()).ok());
    RepoSnapshot {
        repo_ref_revision,
        repo,
    }
}

pub fn resolve_commit_id_in_repo(repo: &gix::Repository, revision: &str) -> Result<CommitId> {
    let object = repo
        .rev_parse_single(revision)
        .with_context(|| format!("revision `{revision}` could not be resolved"))?;
    let commit = object
        .object()?
        .peel_to_commit()
        .with_context(|| format!("revision `{revision}` must resolve to a commit"))?;
    CommitId::new(commit.id().detach().to_string())
}

pub fn resolve_commit_id_from_workdir(revision: &str) -> Result<CommitId> {
    let repo = repo_from_workdir()?;
    resolve_commit_id_in_repo(&repo, revision)
}

pub fn git_config_from_workdir() -> Result<GitConfig> {
    let repo = repo_from_workdir()?;
    let config = repo.config_snapshot();
    let email = config.string("user.email").map_or_else(
        || "unknown@localhost".to_string(),
        |value| value.to_string(),
    );
    let signing_key = config
        .string("user.signingkey")
        .map(|value| value.to_string());
    Ok(GitConfig { email, signing_key })
}

pub fn dirty_files_from_workdir() -> Result<HashSet<RepoPath>> {
    let repo = repo_from_workdir()?;
    dirty_files(&repo)
}

pub fn dirty_files(repo: &gix::Repository) -> Result<HashSet<RepoPath>> {
    let mut dirty = HashSet::new();
    let iter = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        .into_iter(Vec::new())?;
    for entry in iter {
        match entry? {
            gix::status::Item::IndexWorktree(item) => {
                if item.summary().is_some() {
                    insert_repo_path(&mut dirty, item.rela_path())?;
                }
            }
            gix::status::Item::TreeIndex(change) => {
                insert_tree_index_change_paths(&mut dirty, change)?;
            }
        }
    }
    Ok(dirty)
}

fn insert_tree_index_change_paths(
    dirty: &mut HashSet<RepoPath>,
    change: gix::diff::index::Change,
) -> Result<()> {
    insert_repo_path(dirty, change.location())?;
    if let gix::diff::index::ChangeRef::Rewrite {
        source_location,
        copy: false,
        ..
    } = change
    {
        insert_repo_path(dirty, source_location.as_ref())?;
    }
    Ok(())
}

fn insert_repo_path(dirty: &mut HashSet<RepoPath>, path: &gix::bstr::BStr) -> Result<()> {
    dirty.insert(RepoPath::new(path.to_str_lossy().as_ref())?);
    Ok(())
}

pub fn block_state_for_path(
    repo_snapshot: &RepoSnapshot,
    path_hint: Option<&str>,
    fingerprint: &str,
) -> BlockStateResult {
    let Some(repo) = &repo_snapshot.repo else {
        return BlockStateResult::Unknown;
    };
    let Some(path) = path_hint else {
        return BlockStateResult::Unknown;
    };
    let workdir_prefix = repo
        .workdir()
        .and_then(path_utils::current_workdir_prefix_for_repo_root);
    let candidate_paths =
        path_utils::repo_path_candidates(path, workdir_prefix.as_deref(), repo.workdir());
    tracing::debug!(
        path_hint = %path,
        ?candidate_paths,
        "resolving block state path candidates"
    );

    for candidate in &candidate_paths {
        let Ok(candidate_path) = RepoPath::new(candidate) else {
            continue;
        };
        if let Ok(blocks) = head_blocks_for_path(repo, &candidate_path)
            && blocks
                .iter()
                .any(|block| block.hash.as_str() == fingerprint)
        {
            tracing::debug!(
                path_hint = %path,
                resolved_path = %candidate,
                "block state resolved as committed"
            );
            return BlockStateResult::Committed;
        }
    }

    if let Ok(dirty) = dirty_files(repo)
        && candidate_paths
            .iter()
            .filter_map(|candidate| RepoPath::new(candidate).ok())
            .any(|candidate| dirty.contains(&candidate))
    {
        tracing::debug!(
            path_hint = %path,
            ?candidate_paths,
            "block state resolved as uncommitted"
        );
        return BlockStateResult::Uncommitted;
    }

    tracing::debug!(
        path_hint = %path,
        ?candidate_paths,
        "block state resolution fell back to unknown"
    );
    BlockStateResult::Unknown
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStateResult {
    Committed,
    Uncommitted,
    Unknown,
}

pub fn head_blocks_for_path(repo: &gix::Repository, path: &RepoPath) -> Result<Vec<Block>> {
    let head_tree = repo.head_tree()?;
    let tree_path = Path::new(path.as_str());
    let entry = head_tree
        .lookup_entry_by_path(tree_path)?
        .context("path not found in head tree")?;
    if entry.mode().kind() == EntryKind::Tree {
        return Ok(Vec::new());
    }
    let blob = entry.object()?.try_into_blob()?;
    let content = std::str::from_utf8(&blob.data).context("utf8")?;
    let extension = tree_path.extension().and_then(|ext| ext.to_str());
    let language = extension
        .and_then(Language::from_extension)
        .unwrap_or(Language::Unknown);
    Ok(split_blocks(content, language))
}

pub fn file_states_for_paths_in_revision(
    repo: &gix::Repository,
    revision: &str,
    paths: &HashSet<RepoPath>,
    workdir_prefix: Option<&str>,
) -> Result<Vec<FileState>> {
    let tree = tree_for_revision(repo, revision)?;
    file_states_for_paths_in_tree(&tree, paths, workdir_prefix)
}

pub fn file_states_in_revision(
    repo: &gix::Repository,
    revision: &str,
    workdir_prefix: Option<&str>,
) -> Result<Vec<FileState>> {
    let tree = tree_for_revision(repo, revision)?;
    let mut files = Vec::new();
    collect_file_states_in_tree(&tree, "", workdir_prefix, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

pub(crate) fn file_state_for_path_in_main_base(
    repo: &gix::Repository,
    repo_relative_path: &RepoPath,
    output_path: &RepoPath,
) -> Result<Option<FileState>> {
    let (base_tree, _) = main_and_head_trees(repo)?;
    file_state_for_path_in_tree(&base_tree, repo_relative_path.as_str(), output_path)
}

pub(crate) fn file_state_for_path_in_revision(
    repo: &gix::Repository,
    revision: &str,
    repo_relative_path: &RepoPath,
    output_path: &RepoPath,
) -> Result<Option<FileState>> {
    let tree = tree_for_revision(repo, revision)?;
    file_state_for_path_in_tree(&tree, repo_relative_path.as_str(), output_path)
}

pub(crate) fn file_text_for_path_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<Option<String>> {
    let tree = tree_for_revision(repo, revision)?;
    for candidate in path_utils::tree_path_candidates_for_repo_path(path.as_str(), workdir_prefix) {
        if let Some(text) = file_text_for_path_in_tree(&tree, &candidate)? {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

pub(crate) fn file_state_for_path_in_revision_base(
    repo: &gix::Repository,
    revision: &str,
    repo_relative_path: &RepoPath,
    output_path: &RepoPath,
) -> Result<Option<FileState>> {
    let object = repo.rev_parse_single(revision)?;
    let commit = object
        .object()?
        .peel_to_commit()
        .context("revision must resolve to a commit")?;
    let base_tree = if let Some(parent_id) = commit.parent_ids().next() {
        repo.find_commit(parent_id)?.tree()?
    } else {
        repo.empty_tree()
    };
    file_state_for_path_in_tree(&base_tree, repo_relative_path.as_str(), output_path)
}

fn tree_for_revision<'repo>(
    repo: &'repo gix::Repository,
    revision: &str,
) -> Result<gix::Tree<'repo>> {
    let object = repo.rev_parse_single(revision)?;
    let commit = object
        .object()?
        .peel_to_commit()
        .context("revision must resolve to a commit")?;
    commit.tree().map_err(Into::into)
}

fn file_states_for_paths_in_tree(
    tree: &gix::Tree<'_>,
    paths: &HashSet<RepoPath>,
    workdir_prefix: Option<&str>,
) -> Result<Vec<FileState>> {
    let mut ordered_paths = paths.iter().collect::<Vec<_>>();
    ordered_paths.sort();

    let mut files = Vec::new();
    for requested_path in ordered_paths {
        let candidates =
            path_utils::tree_path_candidates_for_repo_path(requested_path.as_str(), workdir_prefix);
        for candidate in candidates {
            if let Some(file_state) = file_state_for_path_in_tree(tree, &candidate, requested_path)?
            {
                files.push(file_state);
                break;
            }
        }
    }

    Ok(files)
}

fn file_state_for_path_in_tree(
    tree: &gix::Tree<'_>,
    tree_path_str: &str,
    output_path: &RepoPath,
) -> Result<Option<FileState>> {
    let tree_path = Path::new(tree_path_str);
    let Some(entry) = tree.lookup_entry_by_path(tree_path)? else {
        return Ok(None);
    };
    if entry.mode().kind() == EntryKind::Tree {
        return Ok(None);
    }

    let blob = entry.object()?.try_into_blob()?;
    Ok(Some(file_state_from_blob(
        &blob,
        tree_path,
        output_path.clone(),
    )))
}

fn file_text_for_path_in_tree(tree: &gix::Tree<'_>, tree_path_str: &str) -> Result<Option<String>> {
    let tree_path = Path::new(tree_path_str);
    let Some(entry) = tree.lookup_entry_by_path(tree_path)? else {
        return Ok(None);
    };
    if entry.mode().kind() == EntryKind::Tree {
        return Ok(None);
    }

    let blob = entry.object()?.try_into_blob()?;
    let Ok(content) = std::str::from_utf8(&blob.data) else {
        return Ok(None);
    };
    Ok(Some(content.to_string()))
}

fn collect_file_states_in_tree(
    tree: &gix::Tree<'_>,
    prefix: &str,
    workdir_prefix: Option<&str>,
    files: &mut Vec<FileState>,
) -> Result<()> {
    let mut pending = vec![(tree.clone(), prefix.to_string())];

    while let Some((tree, prefix)) = pending.pop() {
        for entry in tree.iter() {
            let entry = entry?;
            let file_name = entry.filename().to_str_lossy();
            let full_path = if prefix.is_empty() {
                file_name.to_string()
            } else {
                format!("{prefix}/{file_name}")
            };

            match entry.kind() {
                EntryKind::Tree => {
                    let child_tree = entry.object()?.try_into_tree()?;
                    pending.push((child_tree, full_path));
                }
                EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                    if let Some(prefix) = workdir_prefix
                        && !path_utils::path_matches_workdir_prefix(&full_path, prefix)
                    {
                        continue;
                    }

                    let output_path = display_path_for_tree_entry(&full_path, workdir_prefix)?;
                    let blob = entry.object()?.try_into_blob()?;
                    files.push(file_state_from_blob(
                        &blob,
                        Path::new(&full_path),
                        output_path,
                    ));
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn display_path_for_tree_entry(path: &str, _workdir_prefix: Option<&str>) -> Result<RepoPath> {
    let normalized_path = path_utils::normalize_path_str(path);
    RepoPath::new(normalized_path)
}

fn file_state_from_blob(
    blob: &gix::Blob<'_>,
    tree_path: &Path,
    output_path: RepoPath,
) -> FileState {
    let content = match std::str::from_utf8(&blob.data) {
        Ok(content) => content,
        Err(_) => return FileState::from_binary(output_path, &blob.data),
    };

    let language = tree_path
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(Language::from_extension)
        .unwrap_or(Language::Unknown);
    let blocks = split_blocks(content, language);

    FileState::from_text(output_path, language, &blob.data, blocks)
}

pub fn diff_hunks_for_file(repo: &gix::Repository, path: &RepoPath) -> Result<Vec<DiffHunk>> {
    Ok(diff_for_file(repo, path)?.into_hunks())
}

pub(crate) fn diff_for_file(repo: &gix::Repository, path: &RepoPath) -> Result<FileDiff> {
    let mut diffs = file_diffs_for_main_to_head(repo, std::slice::from_ref(path))?;
    Ok(diffs.take(path))
}

pub(crate) fn file_diffs_for_main_to_head(
    repo: &gix::Repository,
    selected_destinations: &[RepoPath],
) -> Result<SelectedFileDiffs> {
    let (base_tree, head_tree) = main_and_head_trees(repo)?;
    diffs_for_paths_between_trees(repo, &base_tree, &head_tree, selected_destinations)
}

pub fn diff_hunks_for_file_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &RepoPath,
) -> Result<Vec<DiffHunk>> {
    Ok(diff_for_file_in_revision(repo, revision, path)?.into_hunks())
}

pub(crate) fn diff_for_file_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &RepoPath,
) -> Result<FileDiff> {
    let mut diffs = file_diffs_for_revision(repo, revision, std::slice::from_ref(path))?;
    Ok(diffs.take(path))
}

pub(crate) fn file_diffs_for_revision(
    repo: &gix::Repository,
    revision: &str,
    selected_destinations: &[RepoPath],
) -> Result<SelectedFileDiffs> {
    let object = repo.rev_parse_single(revision)?;
    let commit = object
        .object()?
        .peel_to_commit()
        .context("revision must resolve to a commit")?;
    let head_tree = commit.tree()?;
    let base_tree = if let Some(parent_id) = commit.parent_ids().next() {
        repo.find_commit(parent_id)?.tree()?
    } else {
        repo.empty_tree()
    };
    diffs_for_paths_between_trees(repo, &base_tree, &head_tree, selected_destinations)
}

pub fn diff_hunks_for_file_in_range(
    repo: &gix::Repository,
    start: &str,
    end: &str,
    path: &RepoPath,
) -> Result<Vec<DiffHunk>> {
    Ok(diff_for_file_in_range(repo, start, end, path)?.into_hunks())
}

pub(crate) fn diff_for_file_in_range(
    repo: &gix::Repository,
    start: &str,
    end: &str,
    path: &RepoPath,
) -> Result<FileDiff> {
    let mut diffs = file_diffs_for_range(repo, start, end, std::slice::from_ref(path))?;
    Ok(diffs.take(path))
}

pub(crate) fn file_diffs_for_range(
    repo: &gix::Repository,
    start: &str,
    end: &str,
    selected_destinations: &[RepoPath],
) -> Result<SelectedFileDiffs> {
    let start_obj = repo.rev_parse_single(start)?;
    let end_obj = repo.rev_parse_single(end)?;
    let start_commit = start_obj
        .object()?
        .peel_to_commit()
        .context("start revision must resolve to a commit")?;
    let end_commit = end_obj
        .object()?
        .peel_to_commit()
        .context("end revision must resolve to a commit")?;
    let start_tree = start_commit.tree()?;
    let end_tree = end_commit.tree()?;
    diffs_for_paths_between_trees(repo, &start_tree, &end_tree, selected_destinations)
}

fn diffs_for_paths_between_trees(
    repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    head_tree: &gix::Tree<'_>,
    selected_destinations: &[RepoPath],
) -> Result<SelectedFileDiffs> {
    if selected_destinations.is_empty() {
        return Ok(SelectedFileDiffs::default());
    }
    let mut diff_cache = repo.diff_resource_cache_for_tree_diff()?;
    diffs_for_paths_between_trees_with_cache(
        repo,
        base_tree,
        head_tree,
        selected_destinations,
        &mut diff_cache,
    )
}

fn diffs_for_paths_between_trees_with_cache(
    repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    head_tree: &gix::Tree<'_>,
    selected_destinations: &[RepoPath],
    diff_cache: &mut gix::diff::blob::Platform,
) -> Result<SelectedFileDiffs> {
    if selected_destinations.is_empty() {
        return Ok(SelectedFileDiffs::default());
    }

    let selected_by_location = selected_destinations
        .iter()
        .map(|path| (path.as_str(), path))
        .collect::<HashMap<_, _>>();
    let mut selected_diffs = SelectedFileDiffs {
        by_destination: HashMap::with_capacity(selected_by_location.len()),
    };

    #[cfg(test)]
    FILE_DIFF_TREE_TRAVERSALS.with(|counter| counter.set(counter.get() + 1));
    let changes = repo.diff_tree_to_tree(Some(base_tree), Some(head_tree), None)?;
    for change in changes {
        let change_ref = change.to_ref();
        if !is_blob_change(&change_ref) {
            continue;
        }
        #[cfg(test)]
        FILE_DIFF_INSPECTED_CHANGES.with(|counter| counter.set(counter.get() + 1));

        let Some(destination) =
            selected_destination_for_location(change_ref.location(), &selected_by_location)
        else {
            continue;
        };
        if selected_diffs.by_destination.contains_key(destination) {
            continue;
        }

        let changed_path =
            ChangedPath::from_change_with_location(&change_ref, destination.clone())?;
        diff_cache.set_resource_by_change(change_ref, &repo.objects)?;
        let file_diff = file_diff_from_change(diff_cache, changed_path)?;
        diff_cache.clear_resource_cache_keep_allocation();
        selected_diffs
            .by_destination
            .insert(destination.clone(), file_diff);
    }

    Ok(selected_diffs)
}

fn selected_destination_for_location<'a>(
    location: &gix::bstr::BStr,
    selected_by_location: &HashMap<&str, &'a RepoPath>,
) -> Option<&'a RepoPath> {
    match std::str::from_utf8(location.as_ref()) {
        Ok(location) => selected_by_location.get(location).copied(),
        Err(_) => {
            let location = location.to_str_lossy();
            selected_by_location.get(location.as_ref()).copied()
        }
    }
}

fn push_indexed_changed_lines_for_hunk(
    changed_lines: &mut Vec<IndexedChangedDiffLine>,
    hunk: &DiffHunk,
) {
    let mut old_line = hunk.old_start;
    let mut new_line = hunk.new_start;

    for line in &hunk.lines {
        match line.kind {
            DiffLineKind::Context => {
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
            DiffLineKind::Added => {
                let diff_order = changed_lines.len();
                changed_lines.push(IndexedChangedDiffLine {
                    diff_order,
                    line: DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(new_line),
                        text: line.display_text().to_string(),
                        is_focus: true,
                    },
                });
                new_line = new_line.saturating_add(1);
            }
            DiffLineKind::Removed => {
                let diff_order = changed_lines.len();
                changed_lines.push(IndexedChangedDiffLine {
                    diff_order,
                    line: DiffLine {
                        kind: DiffLineKind::Removed,
                        old_line: Some(old_line),
                        new_line: None,
                        text: line.display_text().to_string(),
                        is_focus: true,
                    },
                });
                old_line = old_line.saturating_add(1);
            }
        }
    }
}

fn all_changed_lines_are_nonreviewable(changed_lines: &[&IndexedChangedDiffLine]) -> bool {
    let mut index = 0;
    while index < changed_lines.len() {
        let line = &changed_lines[index].line;
        if is_trivial_closing_brace_addition(line) || is_trivial_whitespace_only_change(line) {
            index += 1;
            continue;
        }

        if let Some(consumed) = trivial_formatting_only_replacement_run(&changed_lines[index..]) {
            index += consumed;
            continue;
        }

        return false;
    }

    true
}

fn trivial_formatting_only_replacement_run(
    changed_lines: &[&IndexedChangedDiffLine],
) -> Option<usize> {
    trivial_formatting_only_replacement_run_for_order(
        changed_lines,
        DiffLineKind::Removed,
        DiffLineKind::Added,
    )
    .or_else(|| {
        trivial_formatting_only_replacement_run_for_order(
            changed_lines,
            DiffLineKind::Added,
            DiffLineKind::Removed,
        )
    })
}

fn trivial_formatting_only_replacement_run_for_order(
    changed_lines: &[&IndexedChangedDiffLine],
    first_kind: DiffLineKind,
    second_kind: DiffLineKind,
) -> Option<usize> {
    let mut first_len = 0;
    while changed_lines
        .get(first_len)
        .is_some_and(|line| line.line.kind == first_kind)
    {
        first_len += 1;
    }

    let mut second_len = 0;
    while changed_lines
        .get(first_len + second_len)
        .is_some_and(|line| line.line.kind == second_kind)
    {
        second_len += 1;
    }

    if first_len == 0 || first_len != second_len {
        return None;
    }

    let mut saw_nonempty_line = false;
    for index in 0..first_len {
        let first_trimmed = changed_lines[index].line.text.trim();
        let second_trimmed = changed_lines[first_len + index].line.text.trim();
        if first_trimmed != second_trimmed {
            return None;
        }
        saw_nonempty_line |= !first_trimmed.is_empty();
    }

    saw_nonempty_line.then_some(first_len + second_len)
}

fn is_trivial_closing_brace_addition(line: &DiffLine) -> bool {
    if line.kind != DiffLineKind::Added {
        return false;
    }

    let trimmed = line.text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let mut chars = trimmed.chars().peekable();
    let mut closing_brace_count = 0usize;
    while matches!(chars.peek(), Some('}')) {
        chars.next();
        closing_brace_count += 1;
    }
    if closing_brace_count == 0 {
        return false;
    }

    match chars.next() {
        None => true,
        Some(ch) if (ch == ';' || ch == ',') && chars.next().is_none() => true,
        _ => false,
    }
}

fn is_trivial_whitespace_only_change(line: &DiffLine) -> bool {
    matches!(line.kind, DiffLineKind::Added | DiffLineKind::Removed) && line.text.trim().is_empty()
}

pub fn files_changed_main_to_head() -> Result<HashSet<ChangedPath>> {
    let repo = repo_from_workdir()?;
    files_changed_main_to_head_in_repo(&repo)
}

pub fn files_changed_main_to_head_in_repo(repo: &gix::Repository) -> Result<HashSet<ChangedPath>> {
    let (base_tree, head_tree) = main_and_head_trees(repo)?;
    collect_changed_paths(repo, Some(&base_tree), Some(&head_tree))
}

pub fn recent_commits(limit: usize) -> Result<Vec<CommitInfo>> {
    let repo = repo_from_workdir()?;
    recent_commits_in_repo(&repo, limit)
}

pub fn recent_commits_in_repo(repo: &gix::Repository, limit: usize) -> Result<Vec<CommitInfo>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let Ok(head_commit) = repo.head_commit() else {
        return Ok(Vec::new());
    };

    let mut commits = Vec::new();
    let mut current = head_commit;

    loop {
        let summary = current.message().map_or_else(
            |_| "(no message)".to_string(),
            |message| message.summary().to_str_lossy().to_string(),
        );
        commits.push(CommitInfo {
            id: CommitId::new(current.id().detach().to_string())?,
            summary,
        });

        if commits.len() >= limit {
            break;
        }

        let Some(parent_id) = current.parent_ids().next() else {
            break;
        };
        current = repo.find_commit(parent_id)?;
    }

    Ok(commits)
}

pub fn files_changed_in_revision(revision: &str) -> Result<HashSet<ChangedPath>> {
    let repo = repo_from_workdir()?;
    files_changed_in_revision_in_repo(&repo, revision)
}

pub fn files_changed_in_revision_in_repo(
    repo: &gix::Repository,
    revision: &str,
) -> Result<HashSet<ChangedPath>> {
    let object = repo.rev_parse_single(revision)?;
    let commit = object
        .object()?
        .peel_to_commit()
        .context("revision must resolve to a commit")?;
    let commit_tree = commit.tree()?;
    let parent_tree = if let Some(parent_id) = commit.parent_ids().next() {
        repo.find_commit(parent_id)?.tree()?
    } else {
        repo.empty_tree()
    };
    collect_changed_paths(repo, Some(&parent_tree), Some(&commit_tree))
}

pub fn files_changed_in_range(start: &str, end: &str) -> Result<HashSet<ChangedPath>> {
    let repo = repo_from_workdir()?;
    files_changed_in_range_in_repo(&repo, start, end)
}

pub fn files_changed_in_range_in_repo(
    repo: &gix::Repository,
    start: &str,
    end: &str,
) -> Result<HashSet<ChangedPath>> {
    let start_obj = repo.rev_parse_single(start)?;
    let end_obj = repo.rev_parse_single(end)?;
    let start_commit = start_obj
        .object()?
        .peel_to_commit()
        .context("start revision must resolve to a commit")?;
    let end_commit = end_obj
        .object()?
        .peel_to_commit()
        .context("end revision must resolve to a commit")?;
    let start_tree = start_commit.tree()?;
    let end_tree = end_commit.tree()?;
    collect_changed_paths(repo, Some(&start_tree), Some(&end_tree))
}

fn file_diff_from_change(
    diff_cache: &mut gix::diff::blob::Platform,
    changed_path: ChangedPath,
) -> Result<FileDiff> {
    let prep = diff_cache.prepare_diff()?;
    match prep.operation {
        gix::diff::blob::platform::prepare_diff::Operation::SourceOrDestinationIsBinary => {
            Ok(FileDiff::Unavailable {
                changed_path,
                reason: FileDiffUnavailableReason::Binary,
            })
        }
        gix::diff::blob::platform::prepare_diff::Operation::ExternalCommand { .. } => {
            Ok(FileDiff::Unavailable {
                changed_path,
                reason: FileDiffUnavailableReason::External,
            })
        }
        gix::diff::blob::platform::prepare_diff::Operation::InternalDiff { algorithm } => {
            let input = prep.interned_input();
            let sink = gix::diff::blob::UnifiedDiff::new(
                &input,
                gix::diff::blob::unified_diff::ConsumeBinaryHunk::new(String::new(), "\n"),
                gix::diff::blob::unified_diff::ContextSize::symmetrical(3),
            );
            let unified = gix::diff::blob::diff(algorithm, &input, sink)?;
            let mut hunks = Vec::new();
            collect_hunks(&mut hunks, &changed_path.location, &unified)?;
            if hunks.is_empty() {
                Ok(FileDiff::NoTextChanges { changed_path })
            } else {
                Ok(FileDiff::Text {
                    changed_path,
                    hunks,
                })
            }
        }
    }
}

fn main_and_head_trees<'repo>(
    repo: &'repo gix::Repository,
) -> Result<(gix::Tree<'repo>, gix::Tree<'repo>)> {
    let head_commit = repo.head_commit()?;
    let head_tree = head_commit.tree()?;

    let mut main_ref = repo
        .find_reference("main")
        .or_else(|_| repo.find_reference("master"))
        .context("Could not find main or master branch")?;
    let main_commit = main_ref.peel_to_commit()?;
    let main_id = main_commit.id().detach();

    let base_tree = match repo.merge_base(head_commit.id().detach(), main_id) {
        Ok(base_id) => repo.find_commit(base_id.detach())?.tree()?,
        Err(_) => main_commit.tree()?,
    };

    Ok((base_tree, head_tree))
}

fn collect_changed_paths(
    repo: &gix::Repository,
    base_tree: Option<&gix::Tree<'_>>,
    head_tree: Option<&gix::Tree<'_>>,
) -> Result<HashSet<ChangedPath>> {
    let changes = repo.diff_tree_to_tree(base_tree, head_tree, None)?;
    let mut paths = HashSet::new();
    for change in changes {
        let change_ref = change.to_ref();
        if change_ref.location().is_empty() || !is_blob_change(&change_ref) {
            continue;
        }
        paths.insert(ChangedPath::from_change(&change_ref)?);
    }
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

fn collect_hunks(hunks: &mut Vec<DiffHunk>, path: &RepoPath, unified: &str) -> Result<()> {
    let mut current: Option<DiffHunk> = None;
    for line in unified.lines() {
        if let Some(header) = parse_hunk_header(line) {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(DiffHunk {
                file_path: path.clone(),
                old_start: header.before_start,
                new_start: header.after_start,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(hunk) = &mut current
            && let Some(line) = DiffHunkLine::from_unified_line(line)
        {
            hunk.lines.push(line);
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    Ok(())
}

fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let line = line.strip_prefix("@@ -")?;
    let (before, rest) = line.split_once(' ')?;
    let rest = rest.strip_prefix('+')?;
    let (after, _) = rest.split_once(" @@")?;
    Some(HunkHeader {
        before_start: parse_hunk_start(before)?,
        after_start: parse_hunk_start(after)?,
    })
}

fn parse_hunk_start(range: &str) -> Option<u32> {
    let (start, _) = range.split_once(',').unwrap_or((range, ""));
    start.parse().ok()
}

struct HunkHeader {
    before_start: u32,
    after_start: u32,
}

fn split_blocks(content: &str, language: Language) -> Vec<Block> {
    block_splitter::split(content, language).into_review_blocks(content)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockKind;

    fn unified(line: &str) -> DiffHunkLine {
        let line = line.trim_end_matches('\n');
        DiffHunkLine::from_unified_line(line)
            .unwrap_or_else(|| panic!("expected valid unified diff line: {line:?}"))
    }

    /// Builds a line-range fixture from a hypothetical source with one newline
    /// byte per preceding line, keeping byte and line coordinates coherent.
    fn line_only_block(start_line: usize, end_line: usize) -> Block {
        let content = "\n".repeat(end_line - start_line);
        Block::new(
            content,
            BlockKind::Code,
            crate::block::LineSpan::new(start_line, end_line),
            crate::block::ByteSpan::new(start_line, end_line),
        )
    }

    fn local_source_block(content: &str, start_line: usize, end_line: usize) -> Block {
        Block::new(
            content.to_string(),
            BlockKind::Code,
            crate::block::LineSpan::new(start_line, end_line),
            crate::block::ByteSpan::new(0, content.len()),
        )
    }

    fn block_has_changed_lines_in_diff(
        block: &crate::block::Block,
        hunks: &[DiffHunk],
    ) -> BlockDiffChangeKind {
        DiffChangedLineIndex::from_hunks(hunks).change_kind_for_block(DiffBlockOwnership::Matched {
            base: block,
            head: block,
        })
    }

    #[test]
    fn selected_destination_lookup_preserves_lossy_non_utf8_locations() {
        let selected_path = RepoPath::new("src/\u{fffd}.rs").unwrap();
        let selected_by_location = HashMap::from([(selected_path.as_str(), &selected_path)]);

        let found = selected_destination_for_location(
            gix::bstr::BStr::new(b"src/\xff.rs"),
            &selected_by_location,
        );

        assert_eq!(found, Some(&selected_path));
    }

    #[test]
    fn diffs_for_paths_between_trees_preserve_changed_paths_and_file_diff_states() {
        use crate::test_git::{run_git, temp_git_repo};
        use std::fs;

        let repo_path = temp_git_repo("batch_file_diffs");
        run_git(&repo_path, &["config", "diff.renames", "true"]);
        fs::create_dir_all(repo_path.join("src")).unwrap();
        fs::write(
            repo_path.join("src/text.rs"),
            "pub fn text() { println!(\"before\"); }\n",
        )
        .unwrap();
        fs::write(
            repo_path.join("src/old.rs"),
            "pub fn retained_alpha() {}\npub fn retained_beta() {}\npub fn retained_gamma() {}\n",
        )
        .unwrap();
        fs::write(repo_path.join("src/binary.bin"), [0_u8, 255, 1]).unwrap();
        fs::write(
            repo_path.join("src/external.external"),
            "pub fn external() { println!(\"before\"); }\n",
        )
        .unwrap();
        fs::write(
            repo_path.join(".gitattributes"),
            "*.external diff=external\n",
        )
        .unwrap();
        run_git(&repo_path, &["add", "."]);
        run_git(&repo_path, &["commit", "-m", "base"]);
        run_git(&repo_path, &["branch", "-M", "main"]);
        run_git(&repo_path, &["checkout", "-b", "feature"]);
        run_git(&repo_path, &["config", "diff.external.command", "cat"]);

        fs::write(
            repo_path.join("src/text.rs"),
            "pub fn text() { println!(\"after\"); }\n",
        )
        .unwrap();
        run_git(&repo_path, &["mv", "src/old.rs", "src/new.rs"]);
        fs::write(repo_path.join("src/binary.bin"), [0_u8, 255, 2]).unwrap();
        fs::write(
            repo_path.join("src/external.external"),
            "pub fn external() { println!(\"after\"); }\n",
        )
        .unwrap();
        run_git(&repo_path, &["add", "."]);
        run_git(&repo_path, &["commit", "-m", "feature changes"]);

        let repo = gix::open(&repo_path).unwrap();
        let (base_tree, head_tree) = main_and_head_trees(&repo).unwrap();
        let selected = [
            RepoPath::new("src/text.rs").unwrap(),
            RepoPath::new("src/new.rs").unwrap(),
            RepoPath::new("src/binary.bin").unwrap(),
            RepoPath::new("src/external.external").unwrap(),
        ];
        let mut cache = repo.diff_resource_cache_for_tree_diff().unwrap();
        cache.options.skip_internal_diff_if_external_is_configured = true;
        let mut diffs = diffs_for_paths_between_trees_with_cache(
            &repo, &base_tree, &head_tree, &selected, &mut cache,
        )
        .unwrap();

        let text = diffs.take(&selected[0]);
        assert!(matches!(text, FileDiff::Text { .. }));
        assert_eq!(
            text.changed_path(),
            &ChangedPath::identity(selected[0].clone())
        );

        let renamed = diffs.take(&selected[1]);
        assert!(matches!(renamed, FileDiff::NoTextChanges { .. }));
        assert_eq!(
            renamed.changed_path(),
            &ChangedPath {
                source_location: RepoPath::new("src/old.rs").unwrap(),
                location: selected[1].clone(),
            }
        );

        let binary = diffs.take(&selected[2]);
        assert!(matches!(
            binary,
            FileDiff::Unavailable {
                reason: FileDiffUnavailableReason::Binary,
                ..
            }
        ));
        assert_eq!(
            binary.changed_path(),
            &ChangedPath::identity(selected[2].clone())
        );

        let external = diffs.take(&selected[3]);
        assert!(matches!(
            external,
            FileDiff::Unavailable {
                reason: FileDiffUnavailableReason::External,
                ..
            }
        ));
        assert_eq!(
            external.changed_path(),
            &ChangedPath::identity(selected[3].clone())
        );
    }

    #[test]
    fn tree_index_rewrite_paths_distinguish_rename_from_copy() {
        use std::borrow::Cow;

        fn rewrite(copy: bool) -> gix::diff::index::Change {
            gix::diff::index::ChangeRef::Rewrite {
                source_location: Cow::Borrowed(gix::bstr::BStr::new(b"src/old.rs")),
                source_index: 0,
                source_entry_mode: gix::index::entry::Mode::FILE,
                source_id: Cow::Owned(gix::ObjectId::null(gix::hash::Kind::Sha1)),
                location: Cow::Borrowed(gix::bstr::BStr::new(b"src/new.rs")),
                index: 0,
                entry_mode: gix::index::entry::Mode::FILE,
                id: Cow::Owned(gix::ObjectId::null(gix::hash::Kind::Sha1)),
                copy,
            }
        }

        let mut rename_paths = HashSet::new();
        insert_tree_index_change_paths(&mut rename_paths, rewrite(false)).unwrap();
        assert_eq!(
            rename_paths,
            [
                RepoPath::new("src/old.rs").unwrap(),
                RepoPath::new("src/new.rs").unwrap(),
            ]
            .into_iter()
            .collect()
        );

        let mut copy_paths = HashSet::new();
        insert_tree_index_change_paths(&mut copy_paths, rewrite(true)).unwrap();
        assert_eq!(
            copy_paths,
            [RepoPath::new("src/new.rs").unwrap()].into_iter().collect()
        );
    }

    #[test]
    fn parse_hunk_header_extracts_positions() {
        let header = "@@ -10,2 +12,4 @@";
        let parsed = parse_hunk_header(header).unwrap();
        assert_eq!(parsed.before_start, 10);
        assert_eq!(parsed.after_start, 12);
    }

    #[test]
    fn collect_hunks_groups_lines_by_header() {
        let diff = "@@ -1,1 +1,2 @@\n-foo\n+foo\n+bar\n@@ -5,1 +6,1 @@\n-baz\n+qux\n";
        let mut hunks = Vec::new();
        collect_hunks(&mut hunks, &RepoPath::new("src/main.rs").unwrap(), diff).unwrap();

        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file_path, RepoPath::new("src/main.rs").unwrap());
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(
            hunks[0].lines,
            vec![unified("-foo\n"), unified("+foo\n"), unified("+bar\n")]
        );
        assert_eq!(hunks[1].old_start, 5);
        assert_eq!(hunks[1].new_start, 6);
        assert_eq!(hunks[1].lines, vec![unified("-baz\n"), unified("+qux\n")]);
    }

    #[test]
    fn block_has_changed_lines_reports_overlap_and_non_overlap() {
        let block = line_only_block(10, 20);

        let hunk_inside = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 12,
            new_start: 12,
            lines: vec![unified("+line12\n")],
        };
        let hunk_before = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 5,
            new_start: 5,
            lines: vec![unified("+line5\n")],
        };
        let hunk_after = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 25,
            new_start: 25,
            lines: vec![unified("+line25\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, std::slice::from_ref(&hunk_inside)),
            BlockDiffChangeKind::ReviewableChanges
        );
        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk_before.clone(), hunk_after.clone()]),
            BlockDiffChangeKind::NoTextChanges
        );
        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk_before, hunk_inside, hunk_after]),
            BlockDiffChangeKind::ReviewableChanges
        );
    }

    #[test]
    fn block_has_changed_lines_respects_exclusive_end_line() {
        let block = line_only_block(10, 20);

        let hunk_at_exclusive_end = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 21,
            new_start: 21,
            lines: vec![unified("+line21\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk_at_exclusive_end]),
            BlockDiffChangeKind::NoTextChanges,
            "exclusive end_line must not overlap"
        );
    }

    #[test]
    fn block_has_changed_lines_counts_removed_lines_from_overlapping_hunks() {
        let block = line_only_block(10, 12);

        let hunk = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 10,
            new_start: 10,
            lines: vec![
                unified(" before\n"),
                unified("-removed\n"),
                unified("+added\n"),
                unified(" after\n"),
            ],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::ReviewableChanges,
            "overlapping hunks with removed lines must still count as reviewable changes"
        );
    }

    #[test]
    fn head_changed_line_index_does_not_attribute_deletion_to_following_block() {
        let following_head_block = line_only_block(10, 12);
        let following_base_block = line_only_block(11, 13);
        let hunk = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 11,
            new_start: 11,
            lines: vec![
                unified("-deleted before following block\n"),
                unified(" following line 1\n"),
                unified(" following line 2\n"),
            ],
        };

        let index = DiffChangedLineIndex::from_hunks(&[hunk]);

        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::Matched {
                base: &following_base_block,
                head: &following_head_block,
            }),
            BlockDiffChangeKind::NoTextChanges
        );
    }

    #[test]
    fn diff_changed_line_index_matches_block_change_detection() {
        let block = line_only_block(10, 11);
        let hunks = vec![DiffHunk {
            file_path: RepoPath::root(),
            old_start: 11,
            new_start: 11,
            lines: vec![unified("-    value\n"), unified("+value\n")],
        }];

        let index = DiffChangedLineIndex::from_hunks(&hunks);
        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::Matched {
                base: &block,
                head: &block,
            }),
            BlockDiffChangeKind::OnlyNonreviewableChurn
        );

        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::BaseOnly(&block)),
            BlockDiffChangeKind::ReviewableChanges
        );
    }

    #[test]
    fn diff_changed_line_index_block_start_whitespace_replacement_is_nonreviewable() {
        let base = local_source_block("    value\n", 0, 1);
        let head = local_source_block("value\n", 0, 1);
        let index = DiffChangedLineIndex::from_hunks(&[DiffHunk {
            file_path: RepoPath::root(),
            old_start: 1,
            new_start: 1,
            lines: vec![unified("-    value\n"), unified("+value\n")],
        }]);

        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::Matched {
                base: &base,
                head: &head,
            }),
            BlockDiffChangeKind::OnlyNonreviewableChurn
        );
    }

    #[test]
    fn diff_changed_line_index_block_start_whitespace_only_changes_are_nonreviewable() {
        let base = local_source_block(" \n", 0, 1);
        let head = local_source_block("\t\n", 0, 1);
        let index = DiffChangedLineIndex::from_hunks(&[DiffHunk {
            file_path: RepoPath::root(),
            old_start: 1,
            new_start: 1,
            lines: vec![unified("- \n"), unified("+\t\n")],
        }]);

        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::Matched {
                base: &base,
                head: &head,
            }),
            BlockDiffChangeKind::OnlyNonreviewableChurn
        );
    }

    #[test]
    fn diff_changed_line_index_block_start_whitespace_mixed_with_removal_is_reviewable() {
        let base = local_source_block(" \nremoved\n", 0, 2);
        let head = line_only_block(0, 1);
        let index = DiffChangedLineIndex::from_hunks(&[DiffHunk {
            file_path: RepoPath::root(),
            old_start: 1,
            new_start: 1,
            lines: vec![unified("- \n"), unified("-removed\n")],
        }]);

        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::Matched {
                base: &base,
                head: &head,
            }),
            BlockDiffChangeKind::ReviewableChanges
        );
    }

    #[test]
    fn diff_changed_line_index_handles_unsorted_hunks() {
        let block = line_only_block(10, 20);
        let hunk_after = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 25,
            new_start: 25,
            lines: vec![unified("+line25\n")],
        };
        let hunk_inside = DiffHunk {
            file_path: RepoPath::root(),
            old_start: 12,
            new_start: 12,
            lines: vec![unified("+line12\n")],
        };

        let index = DiffChangedLineIndex::from_hunks(&[hunk_after, hunk_inside]);

        assert_eq!(
            index.change_kind_for_block(DiffBlockOwnership::HeadOnly(&block)),
            BlockDiffChangeKind::ReviewableChanges
        );
    }

    #[test]
    fn collect_hunks_ignores_no_newline_metadata_lines() {
        let unified_text =
            "@@ -1 +1 @@\n-let value = 42;\n\\ No newline at end of file\n+let value = 42;\n";
        let mut hunks = Vec::new();

        collect_hunks(
            &mut hunks,
            &RepoPath::new("src/lib.rs").unwrap(),
            unified_text,
        )
        .unwrap_or_else(|error| panic!("collect hunks should succeed: {error}"));

        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].lines,
            vec![unified("-let value = 42;\n"), unified("+let value = 42;\n")]
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_closing_brace_only_additions() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![unified("+}\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "brace-only additions should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_whitespace_only_additions() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![unified("+    \n"), unified("+\t\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "whitespace-only additions should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_whitespace_only_removals() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![unified("-    \n"), unified("-\t\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "whitespace-only removals should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_keeps_mixed_whitespace_and_real_changes() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![unified("+    \n"), unified("+let value = 42;\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::ReviewableChanges,
            "mixed whitespace and non-whitespace changes must remain reviewable"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_indentation_only_replacements() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![
                unified("-let value = 42;\n"),
                unified("+    let value = 42;\n"),
            ],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "indentation-only replacements should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_trailing_whitespace_only_replacements() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![
                unified("-let value = 42;\n"),
                unified("+let value = 42;   \n"),
            ],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "trailing-whitespace-only replacements should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_keeps_internal_spacing_replacements_reviewable() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![
                unified("-let value = compute(a, b);\n"),
                unified("+let value = compute(a,  b);\n"),
            ],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::ReviewableChanges,
            "internal spacing replacements should remain reviewable under conservative filtering"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_crlf_to_lf_only_replacements() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![
                unified("-let value = 42;\r\n"),
                unified("+let value = 42;\n"),
            ],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "CRLF/LF-only replacements should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_missing_final_newline_only_replacements() {
        let block = line_only_block(2, 5);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![unified("-let value = 42;\n"), unified("+let value = 42;\n")],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "missing-final-newline-only replacements should not mark a block as changed for review"
        );
    }

    #[test]
    fn block_has_changed_lines_ignores_multiline_indentation_only_replacements() {
        let block = line_only_block(2, 8);

        let hunk = DiffHunk {
            file_path: RepoPath::new("src/lib.rs").unwrap(),
            old_start: 3,
            new_start: 3,
            lines: vec![
                unified("-let value = 42;\n"),
                unified("-return value;\n"),
                unified("+    let value = 42;\n"),
                unified("+    return value;\n"),
            ],
        };

        assert_eq!(
            block_has_changed_lines_in_diff(&block, &[hunk]),
            BlockDiffChangeKind::OnlyNonreviewableChurn,
            "multiline indentation-only replacements should not mark a block as changed for review"
        );
    }
}
