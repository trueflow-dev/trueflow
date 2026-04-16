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
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::hashing::TreeHash;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileDiff {
    Text {
        path: RepoPath,
        hunks: Vec<DiffHunk>,
    },
    NoTextChanges {
        path: RepoPath,
    },
    Unavailable {
        path: RepoPath,
        reason: FileDiffUnavailableReason,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffBlockSide {
    Base,
    Head,
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
        .into_index_worktree_iter(Vec::new())?;
    for entry in iter {
        let item = entry?;
        let summary = item.summary();
        if summary.is_none() {
            continue;
        }
        dirty.insert(RepoPath::new(item.rela_path().to_str_lossy().as_ref())?);
    }
    Ok(dirty)
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
    let mut ordered_paths = paths.iter().cloned().collect::<Vec<_>>();
    ordered_paths.sort();

    let mut files = Vec::new();
    for requested_path in ordered_paths {
        let candidates =
            path_utils::tree_path_candidates_for_repo_path(requested_path.as_str(), workdir_prefix);
        for candidate in candidates {
            if let Some(file_state) =
                file_state_for_path_in_tree(tree, &candidate, &requested_path)?
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

fn collect_file_states_in_tree(
    tree: &gix::Tree<'_>,
    prefix: &str,
    workdir_prefix: Option<&str>,
    files: &mut Vec<FileState>,
) -> Result<()> {
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
                collect_file_states_in_tree(&child_tree, &full_path, workdir_prefix, files)?;
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
    Ok(match diff_for_file(repo, path)? {
        FileDiff::Text { hunks, .. } => hunks,
        FileDiff::NoTextChanges { .. } | FileDiff::Unavailable { .. } => Vec::new(),
    })
}

pub(crate) fn diff_for_file(repo: &gix::Repository, path: &RepoPath) -> Result<FileDiff> {
    let (base_tree, head_tree) = main_and_head_trees(repo)?;
    diff_for_file_between_trees(repo, &base_tree, &head_tree, path)
}

pub fn diff_hunks_for_file_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &RepoPath,
) -> Result<Vec<DiffHunk>> {
    Ok(match diff_for_file_in_revision(repo, revision, path)? {
        FileDiff::Text { hunks, .. } => hunks,
        FileDiff::NoTextChanges { .. } | FileDiff::Unavailable { .. } => Vec::new(),
    })
}

pub(crate) fn diff_for_file_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &RepoPath,
) -> Result<FileDiff> {
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
    diff_for_file_between_trees(repo, &base_tree, &head_tree, path)
}

pub fn diff_hunks_for_file_in_range(
    repo: &gix::Repository,
    start: &str,
    end: &str,
    path: &RepoPath,
) -> Result<Vec<DiffHunk>> {
    Ok(match diff_for_file_in_range(repo, start, end, path)? {
        FileDiff::Text { hunks, .. } => hunks,
        FileDiff::NoTextChanges { .. } | FileDiff::Unavailable { .. } => Vec::new(),
    })
}

pub(crate) fn diff_for_file_in_range(
    repo: &gix::Repository,
    start: &str,
    end: &str,
    path: &RepoPath,
) -> Result<FileDiff> {
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
    diff_for_file_between_trees(repo, &start_tree, &end_tree, path)
}

fn diff_for_file_between_trees(
    repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    head_tree: &gix::Tree<'_>,
    path: &RepoPath,
) -> Result<FileDiff> {
    let mut diff_cache = repo.diff_resource_cache_for_tree_diff()?;

    let changes = repo.diff_tree_to_tree(Some(base_tree), Some(head_tree), None)?;
    for change in changes {
        let change_ref = change.to_ref();
        let location = change_ref.location();
        if location.to_str_lossy() != path.as_str() {
            continue;
        }

        diff_cache.set_resource_by_change(change_ref, &repo.objects)?;
        let file_diff = file_diff_from_change(&mut diff_cache, path.clone())?;
        diff_cache.clear_resource_cache_keep_allocation();
        return Ok(file_diff);
    }

    Ok(FileDiff::NoTextChanges { path: path.clone() })
}

pub(crate) fn block_has_changed_lines_in_diff(
    block: &Block,
    hunks: &[DiffHunk],
) -> BlockDiffChangeKind {
    analyze_block_diff_hunks_for_side(block, hunks, DiffBlockSide::Head)
}

pub(crate) fn block_has_changed_lines_in_diff_for_side(
    block: &Block,
    hunks: &[DiffHunk],
    side: DiffBlockSide,
) -> BlockDiffChangeKind {
    analyze_block_diff_hunks_for_side(block, hunks, side)
}

fn analyze_block_diff_hunks_for_side(
    block: &Block,
    hunks: &[DiffHunk],
    side: DiffBlockSide,
) -> BlockDiffChangeKind {
    let start = usize_to_u32_saturating(block.start_line).saturating_add(1); // 1-based for diff
    let end_exclusive = usize_to_u32_saturating(block.end_line).saturating_add(1);

    let changed_lines = hunks
        .iter()
        .flat_map(positioned_hunk_lines)
        .filter(|line| {
            let anchor_line = match side {
                DiffBlockSide::Base => line.old_line,
                DiffBlockSide::Head => Some(line.new_line.unwrap_or(line.anchor_new_line)),
            };
            anchor_line.is_some_and(|line_number| {
                line_number >= start
                    && line_number < end_exclusive
                    && line.kind != DiffLineKind::Context
            })
        })
        .map(|line| DiffLine {
            kind: line.kind,
            old_line: line.old_line,
            new_line: line.new_line,
            text: line.text,
            is_focus: true,
        })
        .collect::<Vec<_>>();

    if changed_lines.is_empty() {
        return BlockDiffChangeKind::NoTextChanges;
    }

    let changed_line_refs = changed_lines.iter().collect::<Vec<_>>();
    if all_changed_lines_are_nonreviewable(&changed_line_refs) {
        BlockDiffChangeKind::OnlyNonreviewableChurn
    } else {
        BlockDiffChangeKind::ReviewableChanges
    }
}

fn all_changed_lines_are_nonreviewable(changed_lines: &[&DiffLine]) -> bool {
    let mut index = 0;
    while index < changed_lines.len() {
        let line = changed_lines[index];
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

fn trivial_formatting_only_replacement_run(changed_lines: &[&DiffLine]) -> Option<usize> {
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
    changed_lines: &[&DiffLine],
    first_kind: DiffLineKind,
    second_kind: DiffLineKind,
) -> Option<usize> {
    let mut first_run = Vec::new();
    let mut second_run = Vec::new();
    let mut index = 0;

    while let Some(line) = changed_lines.get(index).copied() {
        if line.kind != first_kind {
            break;
        }
        first_run.push(line);
        index += 1;
    }

    while let Some(line) = changed_lines.get(index).copied() {
        if line.kind != second_kind {
            break;
        }
        second_run.push(line);
        index += 1;
    }

    if first_run.is_empty() || second_run.is_empty() {
        return None;
    }

    let first_trimmed = first_run
        .iter()
        .map(|line| line.text.trim())
        .collect::<Vec<_>>();
    let second_trimmed = second_run
        .iter()
        .map(|line| line.text.trim())
        .collect::<Vec<_>>();

    if first_trimmed == second_trimmed && first_trimmed.iter().any(|line| !line.is_empty()) {
        Some(index)
    } else {
        None
    }
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

pub fn files_changed_main_to_head() -> Result<HashSet<RepoPath>> {
    let repo = repo_from_workdir()?;
    files_changed_main_to_head_in_repo(&repo)
}

pub fn files_changed_main_to_head_in_repo(repo: &gix::Repository) -> Result<HashSet<RepoPath>> {
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

pub fn files_changed_in_revision(revision: &str) -> Result<HashSet<RepoPath>> {
    let repo = repo_from_workdir()?;
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
    collect_changed_paths(&repo, Some(&parent_tree), Some(&commit_tree))
}

pub fn files_changed_in_range(start: &str, end: &str) -> Result<HashSet<RepoPath>> {
    let repo = repo_from_workdir()?;
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
    collect_changed_paths(&repo, Some(&start_tree), Some(&end_tree))
}

fn file_diff_from_change(
    diff_cache: &mut gix::diff::blob::Platform,
    path: RepoPath,
) -> Result<FileDiff> {
    let prep = diff_cache.prepare_diff()?;
    match prep.operation {
        gix::diff::blob::platform::prepare_diff::Operation::SourceOrDestinationIsBinary => {
            Ok(FileDiff::Unavailable {
                path,
                reason: FileDiffUnavailableReason::Binary,
            })
        }
        gix::diff::blob::platform::prepare_diff::Operation::ExternalCommand { .. } => {
            Ok(FileDiff::Unavailable {
                path,
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
            collect_hunks(&mut hunks, &path, &unified)?;
            if hunks.is_empty() {
                Ok(FileDiff::NoTextChanges { path })
            } else {
                Ok(FileDiff::Text { path, hunks })
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
) -> Result<HashSet<RepoPath>> {
    let changes = repo.diff_tree_to_tree(base_tree, head_tree, None)?;
    let mut paths = HashSet::new();
    for change in changes {
        let change_ref = change.to_ref();
        let location = change_ref.location();
        if location.is_empty() {
            continue;
        }
        if !is_blob_change(&change_ref) {
            continue;
        }
        paths.insert(RepoPath::new(location.to_str_lossy().as_ref())?);
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

#[derive(Debug, Clone)]
struct PositionedDiffLine {
    kind: DiffLineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
    anchor_new_line: u32,
    text: String,
}

fn positioned_hunk_lines(hunk: &DiffHunk) -> Vec<PositionedDiffLine> {
    let mut old_line = hunk.old_start;
    let mut new_line = hunk.new_start;
    let mut lines = Vec::new();

    for line in &hunk.lines {
        match line.kind {
            DiffLineKind::Context => {
                lines.push(PositionedDiffLine {
                    kind: DiffLineKind::Context,
                    old_line: Some(old_line),
                    new_line: Some(new_line),
                    anchor_new_line: new_line,
                    text: line.display_text().to_string(),
                });
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
            DiffLineKind::Added => {
                lines.push(PositionedDiffLine {
                    kind: DiffLineKind::Added,
                    old_line: None,
                    new_line: Some(new_line),
                    anchor_new_line: new_line,
                    text: line.display_text().to_string(),
                });
                new_line = new_line.saturating_add(1);
            }
            DiffLineKind::Removed => {
                lines.push(PositionedDiffLine {
                    kind: DiffLineKind::Removed,
                    old_line: Some(old_line),
                    new_line: None,
                    anchor_new_line: new_line,
                    text: line.display_text().to_string(),
                });
                old_line = old_line.saturating_add(1);
            }
        }
    }

    lines
}

fn split_blocks(content: &str, language: Language) -> Vec<Block> {
    block_splitter::split(content, language).into_review_blocks()
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unified(line: &str) -> DiffHunkLine {
        let line = line.trim_end_matches('\n');
        DiffHunkLine::from_unified_line(line)
            .unwrap_or_else(|| panic!("expected valid unified diff line: {line:?}"))
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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 10, // 0-based, so lines 11-20
            end_line: 20,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 10, // 0-based, so lines 11-20
            end_line: 20,   // exclusive
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 10, // 0-based, so lines 11-12
            end_line: 12,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2, // 0-based, lines 3..=5
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 5,
        };

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
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: TreeHash::default(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: None,
            start_line: 2,
            end_line: 8,
        };

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
