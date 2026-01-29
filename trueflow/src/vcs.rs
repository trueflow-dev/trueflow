use crate::analysis::Language;
use crate::block::Block;
use crate::block_splitter;
use crate::scanner;
use anyhow::{Context, Result};
use gix::bstr::ByteSlice;
use gix::object::tree::{EntryKind, EntryMode};
use gix::status::UntrackedFiles;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct RepoSnapshot {
    pub repo_ref_revision: Option<String>,
    repo: Option<gix::Repository>,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub file_path: String,
    pub new_start: u32,
    pub lines: Vec<String>,
}

pub struct GitConfig {
    pub email: String,
    pub signing_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: String,
    pub summary: String,
}

pub fn repo_from_workdir() -> Result<gix::Repository> {
    Ok(gix::discover(".")?)
}

pub fn git_root_from_workdir() -> Result<Option<PathBuf>> {
    let repo = repo_from_workdir()?;
    Ok(repo.workdir().map(|path| path.to_path_buf()))
}

pub fn snapshot_from_workdir() -> RepoSnapshot {
    let repo = repo_from_workdir().ok();
    let repo_ref_revision = repo
        .as_ref()
        .and_then(|repo| repo.head_id().ok())
        .map(|id| id.detach().to_string());
    RepoSnapshot {
        repo_ref_revision,
        repo,
    }
}

pub fn git_config_from_workdir() -> Result<GitConfig> {
    let repo = repo_from_workdir()?;
    let config = repo.config_snapshot();
    let email = config
        .string("user.email")
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown@localhost".to_string());
    let signing_key = config
        .string("user.signingkey")
        .map(|value| value.to_string());
    Ok(GitConfig { email, signing_key })
}

pub fn dirty_files_from_workdir() -> Result<HashSet<String>> {
    let repo = repo_from_workdir()?;
    dirty_files(&repo)
}

pub fn dirty_files(repo: &gix::Repository) -> Result<HashSet<String>> {
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
        dirty.insert(item.rela_path().to_str_lossy().to_string());
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
    let normalized = path.trim_start_matches("./");

    if let Ok(blocks) = head_blocks_for_path(repo, normalized)
        && blocks.iter().any(|block| block.hash == fingerprint)
    {
        return BlockStateResult::Committed;
    }

    if let Ok(dirty) = dirty_files(repo)
        && (dirty.contains(normalized) || dirty.contains(path))
    {
        return BlockStateResult::Uncommitted;
    }

    BlockStateResult::Unknown
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStateResult {
    Committed,
    Uncommitted,
    Unknown,
}

pub fn head_blocks_for_path(repo: &gix::Repository, path: &str) -> Result<Vec<Block>> {
    let head_tree = repo.head_tree()?;
    let tree_path = Path::new(path);
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

pub fn diff_main_to_head() -> Result<Vec<DiffHunk>> {
    let repo = repo_from_workdir()?;
    let (base_tree, head_tree) = main_and_head_trees(&repo)?;
    diff_trees(&repo, &base_tree, &head_tree)
}

pub fn files_changed_main_to_head() -> Result<HashSet<String>> {
    let repo = repo_from_workdir()?;
    files_changed_main_to_head_in_repo(&repo)
}

pub fn files_changed_main_to_head_in_repo(repo: &gix::Repository) -> Result<HashSet<String>> {
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

    let head_commit = match repo.head_commit() {
        Ok(commit) => commit,
        Err(_) => return Ok(Vec::new()),
    };

    let mut commits = Vec::new();
    let mut current = head_commit;

    loop {
        let summary = current
            .message()
            .map(|message| message.summary().to_str_lossy().to_string())
            .unwrap_or_else(|_| "(no message)".to_string());
        commits.push(CommitInfo {
            id: current.id().detach().to_string(),
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

pub fn files_changed_in_revision(revision: &str) -> Result<HashSet<String>> {
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

pub fn files_changed_in_range(start: &str, end: &str) -> Result<HashSet<String>> {
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

fn diff_trees(
    repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    head_tree: &gix::Tree<'_>,
) -> Result<Vec<DiffHunk>> {
    let mut hunks = Vec::new();
    let mut diff_cache = repo.diff_resource_cache_for_tree_diff()?;
    let changes = repo.diff_tree_to_tree(Some(base_tree), Some(head_tree), None)?;

    for change in changes {
        let change_ref = change.to_ref();
        let location = change_ref.location();
        if location.is_empty() {
            continue;
        }
        if !is_blob_change(&change_ref) {
            continue;
        }

        diff_cache.set_resource_by_change(change_ref, &repo.objects)?;
        let prep = diff_cache.prepare_diff()?;
        match prep.operation {
            gix::diff::blob::platform::prepare_diff::Operation::SourceOrDestinationIsBinary
            | gix::diff::blob::platform::prepare_diff::Operation::ExternalCommand { .. } => {
                diff_cache.clear_resource_cache_keep_allocation();
                continue;
            }
            gix::diff::blob::platform::prepare_diff::Operation::InternalDiff { algorithm } => {
                let input = prep.interned_input();
                let sink = gix::diff::blob::UnifiedDiff::new(
                    &input,
                    gix::diff::blob::unified_diff::ConsumeBinaryHunk::new(String::new(), "\n"),
                    gix::diff::blob::unified_diff::ContextSize::symmetrical(3),
                );
                let unified = gix::diff::blob::diff(algorithm, &input, sink)?;
                let path = location.to_str_lossy();
                collect_hunks(&mut hunks, path.as_ref(), &unified)?;
            }
        }

        diff_cache.clear_resource_cache_keep_allocation();
    }

    Ok(hunks)
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
) -> Result<HashSet<String>> {
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
        paths.insert(location.to_str_lossy().to_string());
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

fn collect_hunks(hunks: &mut Vec<DiffHunk>, path: &str, unified: &str) -> Result<()> {
    let mut current: Option<DiffHunk> = None;
    for line in unified.lines() {
        if let Some(header) = parse_hunk_header(line) {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(DiffHunk {
                file_path: path.to_string(),
                new_start: header.after_start,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(hunk) = &mut current
            && (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
        {
            hunk.lines.push(format!("{}\n", line));
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
    #[allow(dead_code)]
    before_start: u32,
    after_start: u32,
}

fn split_blocks(content: &str, language: Language) -> Vec<Block> {
    if language != Language::Unknown
        && let Ok(blocks) = block_splitter::split(content, language.clone())
        && !blocks.is_empty()
    {
        return crate::optimizer::optimize(blocks);
    }

    scanner::fallback_split_blocks(content, scanner::FallbackMode::Text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hunk_header_extracts_positions() {
        let header = "@@ -10,2 +12,4 @@";
        let parsed = parse_hunk_header(header).expect("header");
        assert_eq!(parsed.before_start, 10);
        assert_eq!(parsed.after_start, 12);
    }

    #[test]
    fn collect_hunks_groups_lines_by_header() {
        let diff = "@@ -1,1 +1,2 @@\n-foo\n+foo\n+bar\n@@ -5,1 +6,1 @@\n-baz\n+qux\n";
        let mut hunks = Vec::new();
        collect_hunks(&mut hunks, "src/main.rs", diff).expect("collect");

        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file_path, "src/main.rs");
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].lines, vec!["-foo\n", "+foo\n", "+bar\n"]);
        assert_eq!(hunks[1].new_start, 6);
        assert_eq!(hunks[1].lines, vec!["-baz\n", "+qux\n"]);
    }
}
