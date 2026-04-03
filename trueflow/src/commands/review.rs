use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::config::{BlockFilters, load as load_config};
use crate::context::TrueflowContext;
use crate::path_utils;
use crate::policy::{should_skip_impl_by_default, should_skip_imports_by_default};
use crate::repo_path::RepoPath;
use crate::scanner;
use crate::store::{
    FileStore, ReviewStore, Verdict, approved_hashes_from_verdicts, latest_review_verdicts,
};
use crate::sub_splitter;
use crate::tree;
use crate::vcs;
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tracing::info;

#[derive(Serialize)]
pub struct UnreviewedFile {
    pub path: RepoPath,
    pub language: Language,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOptions {
    pub all: bool,
    pub targets: Vec<ReviewTarget>,
    pub only: Vec<BlockKind>,
    pub exclude: Vec<BlockKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    DirtyWorktree,
    MainDiff,
    All,
    File(String),
    Revision(String),
    RevisionRange { start: String, end: String },
}

pub struct ReviewSummary {
    pub files: Vec<UnreviewedFile>,
    pub total_blocks: usize,
    #[allow(dead_code)]
    pub review_state: HashMap<String, Verdict>,
    pub tree: tree::Tree,
    pub unreviewed_block_nodes: HashSet<tree::TreeNodeId>,
}

enum ReviewContentSource {
    Workdir,
    Revision(String),
}

enum ResolvedTargetPaths {
    All,
    Specific(HashSet<RepoPath>),
}

pub fn collect_review_summary(
    _context: &TrueflowContext,
    options: &ReviewOptions,
    filters: &BlockFilters,
) -> Result<ReviewSummary> {
    info!(
        "review collect (all={}, only={:?}, exclude={:?})",
        options.all, options.only, options.exclude
    );
    validate_review_options(options)?;
    let normalized_targets = normalize_targets(options);
    let content_source = review_content_source(&normalized_targets)?;
    let target_paths = resolve_review_targets_from_targets(&normalized_targets)?;
    let diff_overlap_targets = diff_overlap_targets(&normalized_targets);
    let review_repo = if diff_overlap_targets.is_some()
        || matches!(content_source, ReviewContentSource::Revision(_))
    {
        Some(vcs::repo_from_workdir()?)
    } else {
        None
    };
    let workdir_prefix = workdir_prefix_from_git_root();

    // 1. Load Approved Hashes
    let store = FileStore::new()?;
    let history = store.read_history()?;
    info!("loaded {} review records", history.len());

    let fingerprint_status = latest_review_verdicts(&history);
    let approved_hashes = approved_hashes_from_verdicts(&fingerprint_status);

    // 2. Load review content
    let files = match &content_source {
        ReviewContentSource::Workdir => scanner::scan_directory(".")?,
        ReviewContentSource::Revision(revision) => {
            let Some(repo) = review_repo.as_ref() else {
                return Err(anyhow!("review repo unavailable for revision target"));
            };
            let paths = match &target_paths {
                ResolvedTargetPaths::Specific(paths) => paths,
                ResolvedTargetPaths::All => {
                    return Err(anyhow!(
                        "historical review targets must resolve to explicit paths"
                    ));
                }
            };
            vcs::file_states_for_paths_in_revision(
                repo,
                revision,
                paths,
                workdir_prefix.as_deref(),
            )?
        }
    };
    info!("scanned {} files", files.len());
    let tree = tree::build_tree_from_files(&files);

    // 3. Subtraction (Tree Traversal)
    let mut unreviewed_files = Vec::new();
    let mut total_blocks = 0;
    let mut unreviewed_block_nodes = HashSet::new();

    for file in files {
        if let ResolvedTargetPaths::Specific(targets) = &target_paths {
            let file_path = file.path.clone();
            let mut matches = targets.contains(&file_path);
            if !matches && let Some(prefix) = &workdir_prefix {
                let repo_path = RepoPath::new(format!("{prefix}/{file_path}"))?;
                matches = targets.contains(&repo_path);
            }
            if !matches {
                continue;
            }
        }

        let language = file.language;
        let file_diff_hunks = if let (Some(repo), Some(diff_targets)) =
            (review_repo.as_ref(), diff_overlap_targets.as_ref())
        {
            Some(diff_hunks_for_file_targets(
                repo,
                diff_targets,
                &file.path,
                workdir_prefix.as_deref(),
            )?)
        } else {
            None
        };
        let mut reviewable_blocks = Vec::new();
        for block in file.blocks {
            if !filters.allows_block(block.kind) {
                continue;
            }
            if should_skip_imports_by_default(file.path.as_str(), &block, filters) {
                continue;
            }
            if should_skip_impl_by_default(&block, filters) {
                continue;
            }
            if let Some(hunks) = file_diff_hunks.as_deref()
                && !vcs::block_has_changed_lines_in_diff(&block, hunks)
            {
                continue;
            }
            reviewable_blocks.push(block);
        }
        total_blocks += reviewable_blocks.len();

        // Optimization: If the FILE hash is approved, everything inside is approved.
        if fingerprint_status.get(file.tree_hash.as_str()) == Some(&Verdict::Approved) {
            continue;
        }

        let mut unreviewed_blocks = Vec::new();
        for block in reviewable_blocks {
            let node_id = tree.find_block_node(&file.path, &block);
            if let Some(node_id) = node_id
                && tree.is_node_covered(node_id, &approved_hashes)
            {
                continue;
            }

            // Check status
            if fingerprint_status.get(block.hash.as_str()) == Some(&Verdict::Approved) {
                continue;
            }

            if !fingerprint_status.contains_key(block.hash.as_str()) {
                // Not explicitly approved. Check implicit approval via sub-blocks.
                if let Ok(sub_blocks) = sub_splitter::split(&block, language)
                    && !sub_blocks.is_empty()
                {
                    let all_approved = sub_blocks.iter().all(|sb| {
                        if !filters.allows_subblock(sb.kind) {
                            return true;
                        }
                        fingerprint_status.get(sb.hash.as_str()) == Some(&Verdict::Approved)
                    });

                    if all_approved {
                        continue;
                    }
                }
            }

            if let Some(node_id) = node_id {
                unreviewed_block_nodes.insert(node_id);
            }
            unreviewed_blocks.push(block);
        }

        if !unreviewed_blocks.is_empty() {
            unreviewed_files.push(UnreviewedFile {
                path: file.path,
                language,
                blocks: unreviewed_blocks,
            });
        }
    }

    // 1. Sort blocks within files
    for file in &mut unreviewed_files {
        file.blocks
            .sort_by_key(|block| (kind_rank(block), block.start_line));
    }

    // 2. Sort files (Files with higher priority blocks come first)
    unreviewed_files.sort_by(|a, b| {
        let rank_fn = |file: &UnreviewedFile| file.blocks.first().map_or(100, kind_rank);
        (rank_fn(a), &a.path).cmp(&(rank_fn(b), &b.path))
    });

    Ok(ReviewSummary {
        files: unreviewed_files,
        total_blocks,
        review_state: fingerprint_status,
        tree,
        unreviewed_block_nodes,
    })
}

pub fn collect_unreviewed(
    context: &TrueflowContext,
    options: &ReviewOptions,
    filters: &BlockFilters,
) -> Result<Vec<UnreviewedFile>> {
    Ok(collect_review_summary(context, options, filters)?.files)
}

fn review_content_source(targets: &[ReviewTarget]) -> Result<ReviewContentSource> {
    let mut revision = None;
    let mut saw_workdir_target = false;

    for target in targets {
        match target {
            ReviewTarget::Revision(candidate) => {
                if saw_workdir_target {
                    return Err(anyhow!(
                        "Historical targets cannot be mixed with worktree-based targets"
                    ));
                }

                match &revision {
                    Some(existing) if existing != candidate => {
                        return Err(anyhow!(
                            "Multiple historical targets with different content revisions are not supported"
                        ));
                    }
                    Some(_) => {}
                    None => revision = Some(candidate.clone()),
                }
            }
            ReviewTarget::RevisionRange { end, .. } => {
                if saw_workdir_target {
                    return Err(anyhow!(
                        "Historical targets cannot be mixed with worktree-based targets"
                    ));
                }

                match &revision {
                    Some(existing) if existing != end => {
                        return Err(anyhow!(
                            "Multiple historical targets with different content revisions are not supported"
                        ));
                    }
                    Some(_) => {}
                    None => revision = Some(end.clone()),
                }
            }
            ReviewTarget::File(_) => {}
            ReviewTarget::DirtyWorktree | ReviewTarget::MainDiff | ReviewTarget::All => {
                if revision.is_some() {
                    return Err(anyhow!(
                        "Historical targets cannot be mixed with worktree-based targets"
                    ));
                }
                saw_workdir_target = true;
            }
        }
    }

    Ok(revision
        .map(ReviewContentSource::Revision)
        .unwrap_or(ReviewContentSource::Workdir))
}

fn validate_review_options(options: &ReviewOptions) -> Result<()> {
    if options.all && !options.targets.is_empty() {
        return Err(anyhow!(
            "Explicit review targets cannot be combined with --all"
        ));
    }

    Ok(())
}

fn resolve_review_targets_from_targets(targets: &[ReviewTarget]) -> Result<ResolvedTargetPaths> {
    if targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::All))
    {
        return Ok(ResolvedTargetPaths::All);
    }

    let mut paths = HashSet::new();
    for target in targets {
        match target {
            ReviewTarget::DirtyWorktree => {
                if let Ok(dirty) = get_dirty_files() {
                    paths.extend(dirty);
                }
            }
            ReviewTarget::MainDiff => {
                paths.extend(vcs::files_changed_main_to_head()?);
            }
            ReviewTarget::File(path) => {
                paths.insert(RepoPath::new(path)?);
            }
            ReviewTarget::Revision(revision) => {
                paths.extend(vcs::files_changed_in_revision(revision)?);
            }
            ReviewTarget::RevisionRange { start, end } => {
                paths.extend(vcs::files_changed_in_range(start, end)?);
            }
            ReviewTarget::All => {}
        }
    }

    Ok(ResolvedTargetPaths::Specific(paths))
}

fn normalize_targets(options: &ReviewOptions) -> Vec<ReviewTarget> {
    if options.all {
        return vec![ReviewTarget::All];
    }
    if options.targets.is_empty() {
        return vec![ReviewTarget::DirtyWorktree];
    }
    options.targets.clone()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DiffOverlapTarget {
    MainDiff,
    Revision(String),
    RevisionRange { start: String, end: String },
}

fn diff_overlap_targets(targets: &[ReviewTarget]) -> Option<Vec<DiffOverlapTarget>> {
    let mut diff_targets = Vec::new();

    for target in targets {
        match target {
            ReviewTarget::MainDiff => diff_targets.push(DiffOverlapTarget::MainDiff),
            ReviewTarget::Revision(revision) => {
                diff_targets.push(DiffOverlapTarget::Revision(revision.clone()));
            }
            ReviewTarget::RevisionRange { start, end } => {
                diff_targets.push(DiffOverlapTarget::RevisionRange {
                    start: start.clone(),
                    end: end.clone(),
                });
            }
            ReviewTarget::DirtyWorktree | ReviewTarget::All | ReviewTarget::File(_) => {
                return None;
            }
        }
    }

    if diff_targets.is_empty() {
        None
    } else {
        Some(diff_targets)
    }
}

fn diff_hunks_for_file_targets(
    repo: &gix::Repository,
    targets: &[DiffOverlapTarget],
    file_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<Vec<vcs::DiffHunk>> {
    let repo_relative_path = repo_relative_path_for_diff(file_path, workdir_prefix)?;
    let mut hunks = Vec::new();

    for target in targets {
        let target_hunks = match target {
            DiffOverlapTarget::MainDiff => vcs::diff_hunks_for_file(repo, &repo_relative_path)?,
            DiffOverlapTarget::Revision(revision) => {
                vcs::diff_hunks_for_file_in_revision(repo, revision, &repo_relative_path)?
            }
            DiffOverlapTarget::RevisionRange { start, end } => {
                vcs::diff_hunks_for_file_in_range(repo, start, end, &repo_relative_path)?
            }
        };
        hunks.extend(target_hunks);
    }

    Ok(hunks)
}

fn repo_relative_path_for_diff(
    file_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<RepoPath> {
    RepoPath::new(path_utils::repo_relative_path_for_diff(
        file_path.as_str(),
        workdir_prefix,
    ))
}

fn workdir_prefix_from_git_root() -> Option<String> {
    let repo_root = vcs::git_root_from_workdir().ok().flatten()?;
    path_utils::current_workdir_prefix_for_repo_root(&repo_root)
}

pub(crate) fn parse_review_targets(values: &[String]) -> Result<Vec<ReviewTarget>> {
    let mut targets = Vec::new();
    for raw in values {
        if let Some(rest) = raw.strip_prefix("file:") {
            targets.push(ReviewTarget::File(rest.to_string()));
            continue;
        }
        if let Some(rest) = raw.strip_prefix("rev:") {
            if let Some((start, end)) = rest.split_once("..") {
                targets.push(ReviewTarget::RevisionRange {
                    start: start.to_string(),
                    end: end.to_string(),
                });
            } else {
                targets.push(ReviewTarget::Revision(rest.to_string()));
            }
            continue;
        }
        return Err(anyhow!("Unknown review target: {raw}"));
    }
    Ok(targets)
}

pub fn run(
    context: &TrueflowContext,
    json: bool,
    all: bool,
    target: &[String],
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    info!(
        "review start (json={json}, all={all}, target={target:?}, only={only:?}, exclude={exclude:?})"
    );
    let config = load_config()?;
    let filters = config.review.resolve_filters(only, exclude);
    let options = ReviewOptions {
        all,
        targets: parse_review_targets(target)?,
        only: only.to_vec(),
        exclude: exclude.to_vec(),
    };
    let unreviewed_files = collect_unreviewed(context, &options, &filters)?;

    let total_blocks: usize = unreviewed_files.iter().map(|file| file.blocks.len()).sum();
    info!(
        "unreviewed summary (files={}, blocks={})",
        unreviewed_files.len(),
        total_blocks
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&unreviewed_files)?);
    } else if unreviewed_files.is_empty() {
        println!("All clear! No unreviewed blocks found.");
    } else {
        for file in unreviewed_files {
            println!("File: {}", file.path);
            for block in file.blocks {
                println!(
                    "  [Unreviewed] L{}-L{} (Hash: {}) Kind: {}",
                    block.start_line, block.end_line, block.hash, block.kind
                );
                if let Some(first_line) = block.content.lines().next() {
                    println!("    > {}", first_line.trim());
                }
            }
        }
    }

    Ok(())
}

fn get_dirty_files() -> Result<HashSet<RepoPath>> {
    vcs::dirty_files_from_workdir()
}

fn kind_rank(block: &Block) -> u8 {
    if block.tags.iter().any(|tag| tag == "test") {
        return 10;
    }
    block.kind.default_review_priority()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;

    fn make_block(kind: BlockKind, tags: &[&str]) -> Block {
        Block {
            hash: TreeHash::new("hash"),
            content: "content".to_string(),
            kind,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            complexity: 0,
            start_line: 0,
            end_line: 1,
        }
    }

    #[test]
    fn test_review_priority_ordering() {
        let test_block = make_block(BlockKind::Function, &["test"]);
        let signature_block = make_block(BlockKind::FunctionSignature, &[]);
        let function_block = make_block(BlockKind::Function, &[]);

        let ordered = [
            make_block(BlockKind::Struct, &[]),
            test_block,
            signature_block,
            function_block,
        ];

        for window in ordered.windows(2) {
            let first = kind_rank(&window[0]);
            let second = kind_rank(&window[1]);
            assert!(
                first < second,
                "expected {:?} (rank {}) before {:?} (rank {})",
                window[0].kind,
                first,
                window[1].kind,
                second
            );
        }

        let data_rank = kind_rank(&make_block(BlockKind::Struct, &[]));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Enum, &[])));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Type, &[])));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Interface, &[])));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Class, &[])));
    }

    #[test]
    fn validate_review_options_allows_all_without_explicit_targets() {
        let options = ReviewOptions {
            all: true,
            targets: Vec::new(),
            only: Vec::new(),
            exclude: Vec::new(),
        };

        assert!(validate_review_options(&options).is_ok());
    }

    #[test]
    fn validate_review_options_rejects_all_with_explicit_targets() {
        let options = ReviewOptions {
            all: true,
            targets: vec![ReviewTarget::All],
            only: Vec::new(),
            exclude: Vec::new(),
        };

        let err = validate_review_options(&options).unwrap_err();
        assert!(
            err.to_string()
                .contains("Explicit review targets cannot be combined with --all")
        );
    }
}
