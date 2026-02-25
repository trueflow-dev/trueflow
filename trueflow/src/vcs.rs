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
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
    pub is_focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockDiffFocusMode {
    WholeBlock,
    ChangedWithContext { context_lines: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockDiffView {
    pub lines: Vec<DiffLine>,
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
    Ok(repo.workdir().map(Path::to_path_buf))
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
    let email = config.string("user.email").map_or_else(
        || "unknown@localhost".to_string(),
        |value| value.to_string(),
    );
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
    let candidate_paths = candidate_repo_paths_for_hint(repo, path);
    tracing::debug!(
        path_hint = %path,
        ?candidate_paths,
        "resolving block state path candidates"
    );

    for candidate in &candidate_paths {
        if let Ok(blocks) = head_blocks_for_path(repo, candidate)
            && blocks.iter().any(|block| block.hash == fingerprint)
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
            .any(|candidate| dirty.contains(candidate.as_str()))
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

fn candidate_repo_paths_for_hint(repo: &gix::Repository, path_hint: &str) -> Vec<String> {
    let normalized = normalize_repo_path(path_hint);
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![normalized.clone()];
    if let Some(prefix) = workdir_prefix_for_repo(repo)
        && normalized != prefix
        && !normalized.starts_with(&format!("{prefix}/"))
    {
        candidates.push(format!("{prefix}/{normalized}"));
    }

    if let Some(workdir) = repo.workdir() {
        let hinted_path = Path::new(path_hint);
        if hinted_path.is_absolute()
            && let Ok(relative) = hinted_path.strip_prefix(workdir)
        {
            let relative = normalize_repo_path(relative.to_string_lossy().as_ref());
            if !relative.is_empty() && !candidates.contains(&relative) {
                candidates.push(relative);
            }
        }
    }

    candidates
}

fn workdir_prefix_for_repo(repo: &gix::Repository) -> Option<String> {
    let repo_root = repo.workdir()?;
    let cwd = std::env::current_dir().ok()?;
    let repo_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let relative = cwd.strip_prefix(&repo_root).ok()?;
    let relative = normalize_repo_path(relative.to_string_lossy().as_ref());
    if relative.is_empty() || relative == "." {
        None
    } else {
        Some(relative)
    }
}

fn normalize_repo_path(path: &str) -> String {
    path.trim_start_matches("./").replace('\\', "/")
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

pub fn diff_hunks_for_file(repo: &gix::Repository, path: &str) -> Result<Vec<DiffHunk>> {
    let (base_tree, head_tree) = main_and_head_trees(repo)?;
    diff_hunks_for_file_between_trees(repo, &base_tree, &head_tree, path)
}

pub fn diff_hunks_for_file_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &str,
) -> Result<Vec<DiffHunk>> {
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
    diff_hunks_for_file_between_trees(repo, &base_tree, &head_tree, path)
}

pub fn diff_hunks_for_file_in_range(
    repo: &gix::Repository,
    start: &str,
    end: &str,
    path: &str,
) -> Result<Vec<DiffHunk>> {
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
    diff_hunks_for_file_between_trees(repo, &start_tree, &end_tree, path)
}

fn diff_hunks_for_file_between_trees(
    repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    head_tree: &gix::Tree<'_>,
    path: &str,
) -> Result<Vec<DiffHunk>> {
    let mut hunks = Vec::new();
    let mut diff_cache = repo.diff_resource_cache_for_tree_diff()?;

    let changes = repo.diff_tree_to_tree(Some(base_tree), Some(head_tree), None)?;
    for change in changes {
        let change_ref = change.to_ref();
        let location = change_ref.location();
        if location.to_str_lossy() != path {
            continue;
        }

        diff_cache.set_resource_by_change(change_ref, &repo.objects)?;
        let prep = diff_cache.prepare_diff()?;
        if let gix::diff::blob::platform::prepare_diff::Operation::InternalDiff { algorithm } =
            prep.operation
        {
            let input = prep.interned_input();
            let sink = gix::diff::blob::UnifiedDiff::new(
                &input,
                gix::diff::blob::unified_diff::ConsumeBinaryHunk::new(String::new(), "\n"),
                gix::diff::blob::unified_diff::ContextSize::symmetrical(3),
            );
            let unified = gix::diff::blob::diff(algorithm, &input, sink)?;
            collect_hunks(&mut hunks, path, &unified)?;
        }
        break; // Found our file
    }

    Ok(hunks)
}

pub fn extract_block_diff_view_for_block(
    block: &Block,
    hunks: &[DiffHunk],
    focus_mode: BlockDiffFocusMode,
) -> Option<BlockDiffView> {
    let start = usize_to_u32_saturating(block.start_line).saturating_add(1); // 1-based for diff
    let end_exclusive = usize_to_u32_saturating(block.end_line).saturating_add(1);

    let mut lines = Vec::new();
    for hunk in hunks {
        for line in positioned_hunk_lines(hunk) {
            if line.anchor_new_line < start || line.anchor_new_line >= end_exclusive {
                continue;
            }

            lines.push(DiffLine {
                kind: line.kind,
                old_line: line.old_line,
                new_line: line.new_line,
                text: line.text,
                is_focus: line.kind != DiffLineKind::Context,
            });
        }
    }

    if lines.is_empty() {
        return None;
    }

    if let BlockDiffFocusMode::ChangedWithContext { context_lines } = focus_mode {
        lines = keep_changed_with_context(lines, context_lines);
        if lines.is_empty() {
            return None;
        }
    }

    Some(BlockDiffView { lines })
}

pub fn block_has_changed_lines_in_diff(block: &Block, hunks: &[DiffHunk]) -> bool {
    extract_block_diff_view_for_block(block, hunks, BlockDiffFocusMode::WholeBlock).is_some_and(
        |view| {
            view.lines
                .iter()
                .any(|line| line.kind != DiffLineKind::Context)
        },
    )
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
                old_start: header.before_start,
                new_start: header.after_start,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(hunk) = &mut current
            && (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
        {
            hunk.lines.push(format!("{line}\n"));
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
        if let Some(text) = line.strip_prefix(' ') {
            lines.push(PositionedDiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(old_line),
                new_line: Some(new_line),
                anchor_new_line: new_line,
                text: text.trim_end_matches('\n').to_string(),
            });
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
            continue;
        }

        if let Some(text) = line.strip_prefix('+') {
            lines.push(PositionedDiffLine {
                kind: DiffLineKind::Added,
                old_line: None,
                new_line: Some(new_line),
                anchor_new_line: new_line,
                text: text.trim_end_matches('\n').to_string(),
            });
            new_line = new_line.saturating_add(1);
            continue;
        }

        if let Some(text) = line.strip_prefix('-') {
            lines.push(PositionedDiffLine {
                kind: DiffLineKind::Removed,
                old_line: Some(old_line),
                new_line: None,
                anchor_new_line: new_line,
                text: text.trim_end_matches('\n').to_string(),
            });
            old_line = old_line.saturating_add(1);
        }
    }

    lines
}

fn keep_changed_with_context(lines: Vec<DiffLine>, context_lines: usize) -> Vec<DiffLine> {
    let changed_indices = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| (line.kind != DiffLineKind::Context).then_some(idx))
        .collect::<Vec<_>>();

    if changed_indices.is_empty() {
        return lines;
    }

    let mut keep = vec![false; lines.len()];
    for index in changed_indices {
        let start = index.saturating_sub(context_lines);
        let end = (index.saturating_add(context_lines).saturating_add(1)).min(lines.len());
        for slot in &mut keep[start..end] {
            *slot = true;
        }
    }

    lines
        .into_iter()
        .zip(keep)
        .filter_map(|(line, keep)| keep.then_some(line))
        .collect()
}

fn split_blocks(content: &str, language: Language) -> Vec<Block> {
    if language != Language::Unknown
        && let Ok(blocks) = block_splitter::split(content, language)
        && !blocks.is_empty()
    {
        return crate::optimizer::optimize(blocks);
    }

    scanner::fallback_split_blocks(content, scanner::FallbackMode::Text)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        collect_hunks(&mut hunks, "src/main.rs", diff).unwrap();

        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file_path, "src/main.rs");
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].lines, vec!["-foo\n", "+foo\n", "+bar\n"]);
        assert_eq!(hunks[1].old_start, 5);
        assert_eq!(hunks[1].new_start, 6);
        assert_eq!(hunks[1].lines, vec!["-baz\n", "+qux\n"]);
    }

    #[test]
    fn test_extract_block_diff_view_overlap() {
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: String::new(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: 0,
            start_line: 10, // 0-based, so lines 11-20
            end_line: 20,
        };

        let hunk_inside = DiffHunk {
            file_path: String::new(),
            old_start: 12,
            new_start: 12, // Inside block
            lines: vec!["+line12\n".to_string()],
        };

        let hunk_before = DiffHunk {
            file_path: String::new(),
            old_start: 5,
            new_start: 5,
            lines: vec!["+line5\n".to_string()],
        };

        let hunk_after = DiffHunk {
            file_path: String::new(),
            old_start: 25,
            new_start: 25,
            lines: vec!["+line25\n".to_string()],
        };

        // Case 1: Overlap
        let result = extract_block_diff_view_for_block(
            &block,
            std::slice::from_ref(&hunk_inside),
            BlockDiffFocusMode::WholeBlock,
        );
        assert!(result.is_some());
        let lines = match result {
            Some(view) => view.lines,
            None => panic!("expected overlap"),
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, DiffLineKind::Added);
        assert_eq!(lines[0].text, "line12".to_string());

        // Case 2: No Overlap
        let result = extract_block_diff_view_for_block(
            &block,
            &[hunk_before.clone(), hunk_after.clone()],
            BlockDiffFocusMode::WholeBlock,
        );
        assert!(result.is_none());

        // Case 3: Mixed
        let result = extract_block_diff_view_for_block(
            &block,
            &[hunk_before, hunk_inside, hunk_after],
            BlockDiffFocusMode::WholeBlock,
        );
        assert!(result.is_some());
        let lines = match result {
            Some(view) => view.lines,
            None => panic!("expected overlap"),
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, DiffLineKind::Added);
        assert_eq!(lines[0].text, "line12".to_string());
    }

    #[test]
    fn test_extract_block_diff_view_respects_exclusive_end_line() {
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: String::new(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: 0,
            start_line: 10, // 0-based, so lines 11-20
            end_line: 20,   // exclusive
        };

        let hunk_at_exclusive_end = DiffHunk {
            file_path: String::new(),
            old_start: 21,
            new_start: 21, // outside the block range
            lines: vec!["+line21\n".to_string()],
        };

        let result = extract_block_diff_view_for_block(
            &block,
            &[hunk_at_exclusive_end],
            BlockDiffFocusMode::WholeBlock,
        );
        assert!(result.is_none(), "exclusive end_line must not overlap");
    }

    #[test]
    fn extract_block_diff_view_reports_kinds_focus_and_line_numbers() {
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: String::new(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: 0,
            start_line: 9, // 0-based, so lines 10-14
            end_line: 14,  // exclusive
        };

        let hunk = DiffHunk {
            file_path: String::new(),
            old_start: 10,
            new_start: 10,
            lines: vec![
                " keep_before\n".to_string(),
                "-old_value\n".to_string(),
                "+new_value\n".to_string(),
                " keep_after\n".to_string(),
            ],
        };

        let view = match extract_block_diff_view_for_block(
            &block,
            &[hunk],
            BlockDiffFocusMode::WholeBlock,
        ) {
            Some(view) => view,
            None => panic!("expected overlap"),
        };

        assert_eq!(view.lines.len(), 4);
        assert_eq!(view.lines[0].kind, DiffLineKind::Context);
        assert_eq!(view.lines[1].kind, DiffLineKind::Removed);
        assert_eq!(view.lines[2].kind, DiffLineKind::Added);
        assert_eq!(view.lines[3].kind, DiffLineKind::Context);

        assert_eq!(view.lines[0].old_line, Some(10));
        assert_eq!(view.lines[0].new_line, Some(10));
        assert_eq!(view.lines[1].old_line, Some(11));
        assert_eq!(view.lines[1].new_line, None);
        assert_eq!(view.lines[2].old_line, None);
        assert_eq!(view.lines[2].new_line, Some(11));

        assert!(!view.lines[0].is_focus);
        assert!(view.lines[1].is_focus);
        assert!(view.lines[2].is_focus);
        assert!(!view.lines[3].is_focus);
    }

    #[test]
    fn changed_with_context_focus_mode_trims_distant_context() {
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: String::new(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: 0,
            start_line: 10, // 0-based, so lines 11-16
            end_line: 16,
        };

        let hunk = DiffHunk {
            file_path: String::new(),
            old_start: 11,
            new_start: 11,
            lines: vec![
                " ctx1\n".to_string(),
                " ctx2\n".to_string(),
                "-old\n".to_string(),
                "+new\n".to_string(),
                " ctx3\n".to_string(),
                " ctx4\n".to_string(),
            ],
        };

        let view = match extract_block_diff_view_for_block(
            &block,
            &[hunk],
            BlockDiffFocusMode::ChangedWithContext { context_lines: 1 },
        ) {
            Some(view) => view,
            None => panic!("expected overlap"),
        };

        let rendered = view
            .lines
            .iter()
            .map(|line| (line.kind, line.text.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                (DiffLineKind::Context, "ctx2".to_string()),
                (DiffLineKind::Removed, "old".to_string()),
                (DiffLineKind::Added, "new".to_string()),
                (DiffLineKind::Context, "ctx3".to_string()),
            ]
        );
    }

    #[test]
    fn overlapping_hunk_keeps_removed_lines() {
        use crate::block::{Block, BlockKind};

        let block = Block {
            hash: String::new(),
            content: String::new(),
            kind: BlockKind::Code,
            tags: vec![],
            complexity: 0,
            start_line: 10, // 0-based, so lines 11-12
            end_line: 12,
        };

        let hunk = DiffHunk {
            file_path: String::new(),
            old_start: 10,
            new_start: 10,
            lines: vec![
                " before\n".to_string(),
                "-removed\n".to_string(),
                "+added\n".to_string(),
                " after\n".to_string(),
            ],
        };

        let view = match extract_block_diff_view_for_block(
            &block,
            &[hunk],
            BlockDiffFocusMode::WholeBlock,
        ) {
            Some(view) => view,
            None => panic!("expected overlap"),
        };
        assert!(
            view.lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Removed),
            "removed lines should be retained when an overlapping hunk is included"
        );
    }
}
