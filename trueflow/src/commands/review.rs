use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::config::{BlockFilters, load as load_config};
use crate::context::TrueflowContext;
use crate::coverage::{CoverageBuildOptions, CoverageIndex};
use crate::path_utils;
use crate::policy::{should_skip_container_by_default, should_skip_imports_by_default};
use crate::repo_path::RepoPath;
use crate::review_metadata;
use crate::review_scope::CliSemanticReviewScope;
use crate::scanner::{self, ScanDiagnostic, ScanOptions};
use crate::store::{FileStore, ReviewCheck, ReviewStore, Verdict};
use crate::sub_splitter;
use crate::targets::{
    ResolvedTargets, ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget,
    ReviewPathSelection, resolve_targets, workdir_prefix_from_git_root,
};
use crate::tree;
use crate::vcs;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tracing::info;

pub use crate::targets::{ReviewTarget, RevisionRangeSpec, RevisionSpec};

#[derive(Serialize)]
pub struct UnreviewedFile {
    pub path: RepoPath,
    pub language: Language,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRequest {
    AllFiles,
    Targets(Vec<ReviewTarget>),
}

#[derive(Debug, Clone)]
pub struct ResolvedReviewQuery {
    pub filters: BlockFilters,
    pub scan_options: ScanOptions,
    pub content_source: ReviewContentSource,
    pub path_selection: ReviewPathSelection,
    pub diff_selection: ReviewDiffSelection,
}

impl ResolvedReviewQuery {
    fn requires_repo(&self) -> bool {
        self.diff_selection.requires_repo()
            || matches!(self.content_source, ReviewContentSource::Revision(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiagnostic {
    Scan(ScanDiagnostic),
}

impl ReviewDiagnostic {
    pub fn display_message(&self) -> String {
        match self {
            Self::Scan(diagnostic) => diagnostic.display_message(),
        }
    }
}

impl From<ScanDiagnostic> for ReviewDiagnostic {
    fn from(value: ScanDiagnostic) -> Self {
        Self::Scan(value)
    }
}

pub struct ReviewSummary {
    pub files: Vec<UnreviewedFile>,
    pub total_blocks: usize,
    pub diagnostics: Vec<ReviewDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Deleted,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockChangeKind {
    Added,
    Deleted,
    Changed,
}

pub struct CollectedReview {
    pub summary: ReviewSummary,
    pub tree: tree::Tree,
    pub unreviewed_block_nodes: HashSet<tree::TreeNodeId>,
    pub diff_block_sides: HashMap<tree::TreeNodeId, DiffBlockSides>,
    pub file_change_kinds: HashMap<tree::TreeNodeId, FileChangeKind>,
    pub block_change_kinds: HashMap<tree::TreeNodeId, BlockChangeKind>,
}

#[derive(Debug, Clone)]
pub struct DiffBlockSides {
    pub base: Option<Block>,
    pub head: Option<Block>,
}

impl DiffBlockSides {
    fn display_block(&self) -> &Block {
        self.head
            .as_ref()
            .or(self.base.as_ref())
            .unwrap_or_else(|| panic!("diff block sides must include at least one side"))
    }

    pub(crate) fn is_base_only(&self) -> bool {
        self.base.is_some() && self.head.is_none()
    }
}

#[derive(Debug, Clone)]
struct DiffReviewBlock {
    sides: DiffBlockSides,
    change_kind: BlockChangeKind,
}

impl DiffReviewBlock {
    fn display_block(&self) -> &Block {
        self.sides.display_block()
    }
}

#[derive(Debug, Clone)]
struct DiffReviewFile {
    path: RepoPath,
    language: Language,
    file_hash: crate::hashing::TreeHash,
    change_kind: FileChangeKind,
    blocks: Vec<DiffReviewBlock>,
}

struct DiffReviewContext<'a> {
    repo: &'a gix::Repository,
    database: &'a crate::store::ReviewDatabase,
    diff_targets: &'a [ReviewDiffTarget],
    workdir_prefix: Option<&'a str>,
    review_check: &'a ReviewCheck,
}

pub fn parse_review_request(
    all: bool,
    values: &[ReviewTarget],
    since: Option<&str>,
) -> Result<ReviewRequest> {
    Ok(resolve_cli_review_scope(all, values, since)?.review_request())
}

pub fn resolve_cli_review_scope(
    all: bool,
    values: &[ReviewTarget],
    since: Option<&str>,
) -> Result<CliSemanticReviewScope> {
    resolve_cli_review_scope_with(all, values, since, validate_revision_exists_str)
}

pub fn expand_cli_review_targets(
    values: &[ReviewTarget],
    since: Option<&str>,
) -> Result<Vec<ReviewTarget>> {
    expand_cli_review_targets_with(values, since, &validate_revision_exists_str)
}

pub(crate) fn resolve_cli_review_scope_with<F>(
    all: bool,
    values: &[ReviewTarget],
    since: Option<&str>,
    validate_revision: F,
) -> Result<CliSemanticReviewScope>
where
    F: Fn(&str) -> Result<()>,
{
    let targets = expand_cli_review_targets_with(values, since, &validate_revision)?;
    CliSemanticReviewScope::from_cli(all, &targets)
}

pub(crate) fn expand_cli_review_targets_with<F>(
    values: &[ReviewTarget],
    since: Option<&str>,
    validate_revision: &F,
) -> Result<Vec<ReviewTarget>>
where
    F: Fn(&str) -> Result<()>,
{
    let Some(since) = since else {
        return Ok(values.to_vec());
    };

    let mut targets = values.to_vec();
    targets.push(since_review_target_with(since, validate_revision)?);
    Ok(targets)
}

pub fn since_review_target(since: &str) -> Result<ReviewTarget> {
    since_review_target_with(since, &validate_revision_exists_str)
}

fn since_review_target_with<F>(since: &str, validate_revision: &F) -> Result<ReviewTarget>
where
    F: Fn(&str) -> Result<()>,
{
    let start = RevisionSpec::new(since)?;
    validate_revision(start.as_str())?;
    validate_revision("HEAD")?;
    Ok(ReviewTarget::RevisionRange(RevisionRangeSpec::new(
        start.as_str(),
        "HEAD",
    )?))
}

fn validate_revision_exists_str(revision: &str) -> Result<()> {
    let repo = vcs::repo_from_workdir().context("git repository required for revision targets")?;
    repo.rev_parse_single(revision)
        .with_context(|| format!("revision `{revision}` could not be resolved"))?;
    Ok(())
}

pub fn resolve_review_request(
    request: ReviewRequest,
    filters: BlockFilters,
    scan_options: ScanOptions,
) -> Result<ResolvedReviewQuery> {
    let resolved_targets = match request {
        ReviewRequest::AllFiles => ResolvedTargets::new(
            ReviewContentSource::Workdir,
            ReviewDiffSelection::None,
            HashSet::new(),
            Vec::new(),
            HashSet::new(),
        ),
        ReviewRequest::Targets(targets) if targets.is_empty() => {
            return Err(anyhow!(
                "review target list cannot be empty; use AllFiles or an explicit target"
            ));
        }
        ReviewRequest::Targets(targets) => resolve_targets(&targets)?,
    };

    let path_selection = resolved_targets.path_selection();
    let content_source = resolved_targets.content_source;
    let diff_selection = resolved_targets.diff_selection;

    Ok(ResolvedReviewQuery {
        filters,
        scan_options,
        content_source,
        path_selection,
        diff_selection,
    })
}

pub fn collect_review(query: &ResolvedReviewQuery) -> Result<CollectedReview> {
    info!(
        "review collect (content_source={:?}, path_selection={:?}, diff_selection={:?})",
        query.content_source, query.path_selection, query.diff_selection
    );
    let review_repo = if query.requires_repo() {
        Some(vcs::repo_from_workdir()?)
    } else {
        None
    };
    let workdir_prefix = workdir_prefix_from_git_root();

    let store = FileStore::new()?;
    let database = store.load_database()?;
    info!("loaded {} review records", database.records().len());

    let review_check = ReviewCheck::review();

    let (files, diagnostics) = match &query.content_source {
        ReviewContentSource::Workdir => {
            let scan_result = scanner::scan_directory(".", &query.scan_options)?;
            (
                scan_result.files,
                scan_result
                    .diagnostics
                    .into_iter()
                    .map(ReviewDiagnostic::from)
                    .collect(),
            )
        }
        ReviewContentSource::Revision(revision) => {
            let Some(repo) = review_repo.as_ref() else {
                return Err(anyhow!("review repo unavailable for revision target"));
            };
            (
                vcs::file_states_in_revision(repo, revision.as_str(), workdir_prefix.as_deref())?,
                Vec::new(),
            )
        }
    };
    info!("scanned {} files", files.len());
    if let (Some(repo), Some(diff_targets)) = (review_repo.as_ref(), query.diff_selection.targets())
    {
        let diff_context = DiffReviewContext {
            repo,
            database: &database,
            diff_targets,
            workdir_prefix: workdir_prefix.as_deref(),
            review_check: &review_check,
        };
        return collect_diff_scoped_review(query, &diff_context, files, diagnostics);
    }

    let tree = tree::build_tree_from_files(&files);
    let coverage = CoverageIndex::build(
        &tree,
        &database,
        &CoverageBuildOptions {
            workdir_prefix: workdir_prefix.clone(),
        },
    )?;

    let mut unreviewed_files = Vec::new();
    let mut total_blocks = 0;
    let mut unreviewed_block_nodes = HashSet::new();

    for file in files {
        if !query
            .path_selection
            .includes(&file.path, workdir_prefix.as_deref())?
        {
            continue;
        }

        let language = file.language;
        let file_diff_hunks = if let (Some(repo), Some(diff_targets)) =
            (review_repo.as_ref(), query.diff_selection.targets())
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
            if !query.filters.allows_block(block.kind) {
                continue;
            }
            if should_skip_imports_by_default(file.path.as_str(), &block, &query.filters) {
                continue;
            }
            let node_id = tree.find_block_node(&file.path, &block);
            if should_skip_container_by_default(
                node_id.is_some_and(|node_id| tree.is_container_block(node_id)),
                &block,
                &query.filters,
            ) {
                continue;
            }
            if let Some(hunks) = file_diff_hunks.as_deref()
                && vcs::block_has_changed_lines_in_diff(&block, hunks)
                    != vcs::BlockDiffChangeKind::ReviewableChanges
            {
                continue;
            }
            reviewable_blocks.push((block, node_id));
        }
        total_blocks += reviewable_blocks.len();

        let Some(file_node_id) = tree.find_by_path(file.path.as_str()) else {
            continue;
        };
        if coverage
            .node(file_node_id)
            .effective_latest_verdict_for(&review_check)
            == Some(&Verdict::Approved)
        {
            continue;
        }

        let mut unreviewed_blocks = Vec::new();
        for (block, node_id) in reviewable_blocks {
            let block_coverage = coverage.block(&file.path, &block);
            if block_coverage.effective_latest_verdict_for(&review_check)
                == Some(&Verdict::Approved)
            {
                continue;
            }

            if block_coverage
                .direct_latest_verdict_for(&review_check)
                .is_none()
                && is_subblock_covered(
                    &block,
                    language,
                    query,
                    &coverage,
                    &review_check,
                    &file.path,
                )
            {
                continue;
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

    for file in &mut unreviewed_files {
        file.blocks
            .sort_by_key(|block| (kind_rank(block), block.start_line));
    }

    unreviewed_files.sort_by(|a, b| {
        let rank_fn = |file: &UnreviewedFile| file.blocks.first().map_or(100, kind_rank);
        (rank_fn(a), &a.path).cmp(&(rank_fn(b), &b.path))
    });

    Ok(CollectedReview {
        summary: ReviewSummary {
            files: unreviewed_files,
            total_blocks,
            diagnostics,
        },
        tree,
        unreviewed_block_nodes,
        diff_block_sides: HashMap::new(),
        file_change_kinds: HashMap::new(),
        block_change_kinds: HashMap::new(),
    })
}

fn collect_diff_scoped_review(
    query: &ResolvedReviewQuery,
    diff_context: &DiffReviewContext<'_>,
    files: Vec<crate::block::FileState>,
    diagnostics: Vec<ReviewDiagnostic>,
) -> Result<CollectedReview> {
    let review_files = collect_diff_review_files(
        query,
        diff_context.repo,
        files,
        diff_context.diff_targets,
        diff_context.workdir_prefix,
    )?;
    let (tree, diff_block_sides, file_change_kinds, block_change_kinds) =
        build_tree_from_diff_review_files(&review_files)?;
    let coverage = CoverageIndex::build(
        &tree,
        diff_context.database,
        &CoverageBuildOptions {
            workdir_prefix: diff_context.workdir_prefix.map(ToOwned::to_owned),
        },
    )?;

    let mut unreviewed_files = Vec::new();
    let mut total_blocks = 0usize;
    let mut unreviewed_block_nodes = HashSet::new();

    for file in review_files {
        let Some(file_node_id) = tree.find_by_path(file.path.as_str()) else {
            continue;
        };
        if coverage
            .node(file_node_id)
            .effective_latest_verdict_for(diff_context.review_check)
            == Some(&Verdict::Approved)
        {
            continue;
        }

        let mut unreviewed_blocks = Vec::new();
        for review_block in file.blocks {
            let display_block = review_block.display_block().clone();
            let Some(node_id) = tree.find_block_node(&file.path, &display_block) else {
                continue;
            };
            if should_skip_container_by_default(
                tree.is_container_block(node_id),
                &display_block,
                &query.filters,
            ) {
                continue;
            }
            total_blocks += 1;
            let block_coverage = coverage.block(&file.path, &display_block);
            if block_coverage.effective_latest_verdict_for(diff_context.review_check)
                == Some(&Verdict::Approved)
            {
                continue;
            }

            if block_coverage
                .direct_latest_verdict_for(diff_context.review_check)
                .is_none()
                && is_subblock_covered(
                    &display_block,
                    file.language,
                    query,
                    &coverage,
                    diff_context.review_check,
                    &file.path,
                )
            {
                continue;
            }

            unreviewed_block_nodes.insert(node_id);
            unreviewed_blocks.push(display_block);
        }

        if !unreviewed_blocks.is_empty() {
            unreviewed_files.push(UnreviewedFile {
                path: file.path,
                language: file.language,
                blocks: unreviewed_blocks,
            });
        }
    }

    for file in &mut unreviewed_files {
        file.blocks
            .sort_by_key(|block| (kind_rank(block), block.start_line));
    }

    unreviewed_files.sort_by(|a, b| {
        let rank_fn = |file: &UnreviewedFile| file.blocks.first().map_or(100, kind_rank);
        (rank_fn(a), &a.path).cmp(&(rank_fn(b), &b.path))
    });

    Ok(CollectedReview {
        summary: ReviewSummary {
            files: unreviewed_files,
            total_blocks,
            diagnostics,
        },
        tree,
        unreviewed_block_nodes,
        diff_block_sides,
        file_change_kinds,
        block_change_kinds,
    })
}

fn collect_diff_review_files(
    query: &ResolvedReviewQuery,
    repo: &gix::Repository,
    files: Vec<crate::block::FileState>,
    diff_targets: &[ReviewDiffTarget],
    workdir_prefix: Option<&str>,
) -> Result<Vec<DiffReviewFile>> {
    let mut head_files_by_path = files
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut selected_paths =
        selected_review_paths(&query.path_selection, &head_files_by_path, workdir_prefix)?;
    selected_paths.sort();

    let mut review_files = Vec::new();
    for path in selected_paths {
        let display_path = display_path_for_workdir_prefix(&path, workdir_prefix)?;
        let repo_relative_path = RepoPath::new(path_utils::repo_relative_path_for_diff(
            display_path.as_str(),
            workdir_prefix,
        ))?;
        let head_file = head_files_by_path
            .remove(&path)
            .or_else(|| head_files_by_path.remove(&display_path));
        let base_files = base_file_states_for_diff_targets(
            repo,
            diff_targets,
            &display_path,
            &repo_relative_path,
        )?;
        let hunks = diff_hunks_for_file_targets(repo, diff_targets, &display_path, workdir_prefix)?;

        let mut base_blocks = dedupe_blocks(
            base_files
                .iter()
                .flat_map(|file| file.blocks.clone())
                .collect::<Vec<_>>(),
        );
        let mut head_blocks = head_file
            .as_ref()
            .map(|file| file.blocks.clone())
            .unwrap_or_default();
        base_blocks.sort_by_key(|block| (block.start_line, block.end_line, block.kind.as_str()));
        head_blocks.sort_by_key(|block| (block.start_line, block.end_line, block.kind.as_str()));

        let blocks = collect_diff_review_blocks_for_file(&base_blocks, &head_blocks, &hunks)
            .into_iter()
            .filter(|review_block| {
                let display_block = review_block.display_block();
                query.filters.allows_block(display_block.kind)
                    && !should_skip_imports_by_default(
                        display_path.as_str(),
                        display_block,
                        &query.filters,
                    )
            })
            .collect::<Vec<_>>();
        if blocks.is_empty() {
            continue;
        }

        let language = head_file
            .as_ref()
            .map(|file| file.language)
            .or_else(|| base_files.first().map(|file| file.language))
            .unwrap_or(Language::Unknown);
        let file_hash = head_file
            .as_ref()
            .map(|file| file.tree_hash.clone())
            .or_else(|| base_files.first().map(|file| file.tree_hash.clone()))
            .unwrap_or_default();

        let Some(change_kind) =
            classify_file_change_kind(!base_files.is_empty(), head_file.is_some())
        else {
            continue;
        };

        review_files.push(DiffReviewFile {
            path: display_path,
            language,
            file_hash,
            change_kind,
            blocks,
        });
    }

    Ok(review_files)
}

fn selected_review_paths(
    path_selection: &ReviewPathSelection,
    head_files_by_path: &HashMap<RepoPath, crate::block::FileState>,
    workdir_prefix: Option<&str>,
) -> Result<Vec<RepoPath>> {
    match path_selection {
        ReviewPathSelection::All => Ok(head_files_by_path.keys().cloned().collect()),
        ReviewPathSelection::Scoped {
            files,
            dirs,
            changed,
        } => {
            let candidate_paths = if let Some(changed_paths) = changed {
                files
                    .iter()
                    .chain(changed_paths.iter())
                    .cloned()
                    .collect::<HashSet<_>>()
            } else if dirs.is_empty() {
                files.iter().cloned().collect::<HashSet<_>>()
            } else {
                head_files_by_path.keys().cloned().collect::<HashSet<_>>()
            };

            let mut selected = Vec::new();
            for path in candidate_paths {
                if path_selection.includes(&path, workdir_prefix)? {
                    selected.push(path);
                }
            }
            Ok(selected)
        }
    }
}

fn display_path_for_workdir_prefix(
    path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<RepoPath> {
    let Some(prefix) = workdir_prefix
        .map(path_utils::normalize_path_str)
        .filter(|prefix| !prefix.is_empty())
    else {
        return Ok(path.clone());
    };

    let normalized = path_utils::normalize_path_str(path.as_str());
    let prefixed_root = format!("{prefix}/");
    if let Some(stripped) = normalized.strip_prefix(&prefixed_root) {
        return RepoPath::new(stripped);
    }
    if normalized == prefix {
        return RepoPath::new(path.as_str());
    }
    RepoPath::new(path.as_str())
}

fn base_file_states_for_diff_targets(
    repo: &gix::Repository,
    diff_targets: &[ReviewDiffTarget],
    display_path: &RepoPath,
    repo_relative_path: &RepoPath,
) -> Result<Vec<crate::block::FileState>> {
    let mut files = Vec::new();
    for target in diff_targets {
        let file = match target {
            ReviewDiffTarget::MainDiff => {
                vcs::file_state_for_path_in_main_base(repo, repo_relative_path, display_path)?
            }
            ReviewDiffTarget::Revision(revision) => vcs::file_state_for_path_in_revision_base(
                repo,
                revision.as_str(),
                repo_relative_path,
                display_path,
            )?,
            ReviewDiffTarget::RevisionRange(range) => vcs::file_state_for_path_in_revision(
                repo,
                range.start.as_str(),
                repo_relative_path,
                display_path,
            )?,
        };
        if let Some(file) = file {
            files.push(file);
        }
    }
    Ok(files)
}

fn dedupe_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut unique = HashMap::new();
    for block in blocks {
        unique
            .entry((block.hash.clone(), block.start_line, block.end_line))
            .or_insert(block);
    }
    unique.into_values().collect()
}

fn classify_file_change_kind(has_base: bool, has_head: bool) -> Option<FileChangeKind> {
    match (has_base, has_head) {
        (true, true) => Some(FileChangeKind::Changed),
        (true, false) => Some(FileChangeKind::Deleted),
        (false, true) => Some(FileChangeKind::Added),
        (false, false) => None,
    }
}

fn classify_block_change_kind(sides: &DiffBlockSides) -> Option<BlockChangeKind> {
    match (&sides.base, &sides.head) {
        (Some(_), Some(_)) => Some(BlockChangeKind::Changed),
        (Some(_), None) => Some(BlockChangeKind::Deleted),
        (None, Some(_)) => Some(BlockChangeKind::Added),
        (None, None) => None,
    }
}

type DiffReviewTreeBuild = (
    tree::Tree,
    HashMap<tree::TreeNodeId, DiffBlockSides>,
    HashMap<tree::TreeNodeId, FileChangeKind>,
    HashMap<tree::TreeNodeId, BlockChangeKind>,
);

fn build_tree_from_diff_review_files(files: &[DiffReviewFile]) -> Result<DiffReviewTreeBuild> {
    let mut builder = tree::TreeBuilder::new();
    let root = builder.root();
    let mut directories = HashMap::from([(RepoPath::root(), root)]);
    let mut diff_block_sides = HashMap::new();
    let mut file_change_kinds = HashMap::new();
    let mut block_change_kinds = HashMap::new();

    for file in files {
        let parts = file.path.as_str().split('/').collect::<Vec<_>>();
        let mut current_path = RepoPath::root();
        let mut parent = root;

        for (index, part) in parts.iter().enumerate() {
            let is_file = index == parts.len().saturating_sub(1);
            let next_path = current_path.join(part)?;
            if is_file {
                let file_id = builder.add_file(
                    parent,
                    (*part).to_string(),
                    next_path.clone(),
                    file.file_hash.clone(),
                    file.language,
                );
                file_change_kinds.insert(file_id, file.change_kind);
                let mut container_stack: Vec<(tree::TreeNodeId, usize, usize)> = Vec::new();
                for review_block in &file.blocks {
                    let display_block = review_block.display_block().clone();
                    while let Some((_, _, end_line)) = container_stack.last() {
                        if display_block.start_line > *end_line {
                            container_stack.pop();
                        } else {
                            break;
                        }
                    }

                    let parent = container_stack
                        .iter()
                        .rev()
                        .find(|(_, start, end)| {
                            display_block.start_line >= *start && display_block.end_line <= *end
                        })
                        .map(|(id, _, _)| *id)
                        .unwrap_or(file_id);
                    let start_line = display_block.start_line;
                    let end_line = display_block.end_line;
                    let node_id = builder.add_block(
                        parent,
                        diff_block_label(&display_block),
                        next_path.clone(),
                        display_block.clone(),
                        file.language,
                    );
                    diff_block_sides.insert(node_id, review_block.sides.clone());
                    block_change_kinds.insert(node_id, review_block.change_kind);
                    if display_block.kind.can_contain_review_children() {
                        container_stack.push((node_id, start_line, end_line));
                    }
                }
            } else {
                let dir_id = *directories.entry(next_path.clone()).or_insert_with(|| {
                    builder.add_dir(parent, (*part).to_string(), next_path.clone())
                });
                parent = dir_id;
                current_path = next_path;
            }
        }
    }

    Ok((
        builder.finalize(),
        diff_block_sides,
        file_change_kinds,
        block_change_kinds,
    ))
}

fn diff_block_label(block: &Block) -> String {
    let start = block.start_line + 1;
    let end = block.end_line.max(start);
    format!("{}:L{}-L{}", block.kind.as_str(), start, end)
}

pub fn collect_review_summary(query: &ResolvedReviewQuery) -> Result<ReviewSummary> {
    let collected = collect_review(query)?;
    Ok(collected.summary)
}

fn diff_hunks_for_file_targets(
    repo: &gix::Repository,
    targets: &[ReviewDiffTarget],
    file_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<Vec<vcs::DiffHunk>> {
    let repo_relative_path = RepoPath::new(path_utils::repo_relative_path_for_diff(
        file_path.as_str(),
        workdir_prefix,
    ))?;
    let mut hunks = Vec::new();

    for target in targets {
        let target_hunks = match target {
            ReviewDiffTarget::MainDiff => vcs::diff_hunks_for_file(repo, &repo_relative_path)?,
            ReviewDiffTarget::Revision(revision) => {
                vcs::diff_hunks_for_file_in_revision(repo, revision.as_str(), &repo_relative_path)?
            }
            ReviewDiffTarget::RevisionRange(range) => vcs::diff_hunks_for_file_in_range(
                repo,
                range.start.as_str(),
                range.end.as_str(),
                &repo_relative_path,
            )?,
        };
        hunks.extend(target_hunks);
    }

    Ok(hunks)
}

pub fn collect_main_diff_summary() -> Result<ReviewSummary> {
    let config = load_config()?;
    let filters = config.review.resolve_filters(&[], &[]);
    let scan_options = config.scan.resolve_options()?;
    let query = resolve_review_request(
        ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
        filters,
        scan_options,
    )?;
    collect_review_summary(&query)
}

pub(crate) fn run_request(
    json: bool,
    request: ReviewRequest,
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    let config = load_config()?;
    let filters = config.review.resolve_filters(only, exclude);
    let scan_options = config.scan.resolve_options()?;
    let query = resolve_review_request(request, filters, scan_options)?;
    let summary = collect_review_summary(&query)?;
    print_review_summary(summary, json)
}

pub fn print_review_summary(summary: ReviewSummary, json: bool) -> Result<()> {
    for diagnostic in &summary.diagnostics {
        eprintln!("warning: {}", diagnostic.display_message());
    }
    let unreviewed_files = summary.files;

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

pub fn run(
    _context: &TrueflowContext,
    json: bool,
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    info!(
        "review start (json={json}, all={all}, target={target:?}, since={since:?}, only={only:?}, exclude={exclude:?})"
    );
    let request = parse_review_request(all, target, since)?;
    run_request(json, request, only, exclude)
}

fn kind_rank(block: &Block) -> u8 {
    if block.tags.iter().any(|tag| tag == "test") {
        return 10;
    }
    block.kind.default_review_priority()
}

fn is_subblock_covered(
    block: &Block,
    language: Language,
    query: &ResolvedReviewQuery,
    coverage: &CoverageIndex<'_>,
    review_check: &ReviewCheck,
    file_path: &RepoPath,
) -> bool {
    if !query.filters.allows_subblock(block.kind) {
        return true;
    }

    if coverage
        .block(file_path, block)
        .direct_latest_verdict_for(review_check)
        == Some(&Verdict::Approved)
    {
        return true;
    }

    let Ok(sub_split) = sub_splitter::split_result(block, language) else {
        return false;
    };
    if sub_split.blocks.is_empty() {
        return false;
    }

    match sub_split.semantics {
        sub_splitter::SubSplitSemantics::ReviewUnits => sub_split.blocks.iter().all(|sub_block| {
            !query.filters.allows_subblock(sub_block.kind)
                || coverage
                    .block(file_path, sub_block)
                    .direct_latest_verdict_for(review_check)
                    == Some(&Verdict::Approved)
        }),
        sub_splitter::SubSplitSemantics::StructuralChildren => {
            sub_split.blocks.iter().all(|sub_block| {
                is_subblock_covered(
                    sub_block,
                    language,
                    query,
                    coverage,
                    review_check,
                    file_path,
                )
            })
        }
    }
}

fn collect_diff_review_blocks_for_file(
    base_blocks: &[Block],
    head_blocks: &[Block],
    hunks: &[vcs::DiffHunk],
) -> Vec<DiffReviewBlock> {
    let changed_base_blocks = base_blocks
        .iter()
        .filter(|block| {
            vcs::block_has_changed_lines_in_diff_for_side(block, hunks, vcs::DiffBlockSide::Base)
                == vcs::BlockDiffChangeKind::ReviewableChanges
        })
        .cloned()
        .collect::<Vec<_>>();
    let changed_head_blocks = head_blocks
        .iter()
        .filter(|block| {
            vcs::block_has_changed_lines_in_diff_for_side(block, hunks, vcs::DiffBlockSide::Head)
                == vcs::BlockDiffChangeKind::ReviewableChanges
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut unmatched_head_blocks = changed_head_blocks
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    let mut diff_blocks = Vec::new();

    for base_block in changed_base_blocks {
        if let Some(head_index) =
            find_matching_head_block(&base_block, &unmatched_head_blocks, hunks)
        {
            let head_block = unmatched_head_blocks[head_index]
                .take()
                .unwrap_or_else(|| panic!("matched head block should still be present"));
            let sides = DiffBlockSides {
                base: Some(base_block),
                head: Some(head_block),
            };
            diff_blocks.push(DiffReviewBlock {
                change_kind: classify_block_change_kind(&sides)
                    .unwrap_or_else(|| panic!("paired diff block should have a change kind")),
                sides,
            });
        } else {
            let sides = DiffBlockSides {
                base: Some(base_block),
                head: None,
            };
            diff_blocks.push(DiffReviewBlock {
                change_kind: classify_block_change_kind(&sides)
                    .unwrap_or_else(|| panic!("base-only diff block should have a change kind")),
                sides,
            });
        }
    }

    diff_blocks.extend(
        unmatched_head_blocks
            .into_iter()
            .flatten()
            .map(|head_block| {
                let sides = DiffBlockSides {
                    base: None,
                    head: Some(head_block),
                };
                DiffReviewBlock {
                    change_kind: classify_block_change_kind(&sides).unwrap_or_else(|| {
                        panic!("head-only diff block should have a change kind")
                    }),
                    sides,
                }
            }),
    );

    diff_blocks.sort_by_key(|block| {
        let display = block.display_block();
        (
            display.start_line,
            display.end_line,
            display.kind.default_review_priority(),
        )
    });
    diff_blocks
}

fn find_matching_head_block(
    base_block: &Block,
    head_blocks: &[Option<Block>],
    hunks: &[vcs::DiffHunk],
) -> Option<usize> {
    let base_identifier = review_metadata::semantic_block_identifier(base_block);
    if let Some(identifier) = base_identifier.as_deref() {
        let identifier_matches = head_blocks
            .iter()
            .enumerate()
            .filter_map(|(index, head_block)| {
                let head_block = head_block.as_ref()?;
                if head_block.kind != base_block.kind {
                    return None;
                }
                (review_metadata::semantic_block_identifier(head_block).as_deref()
                    == Some(identifier))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if identifier_matches.len() == 1 {
            return identifier_matches.into_iter().next();
        }
    }

    let mapped_base_range = mapped_head_range_for_base_block(base_block, hunks);
    head_blocks
        .iter()
        .enumerate()
        .filter_map(|(index, head_block)| {
            let head_block = head_block.as_ref()?;
            if head_block.kind != base_block.kind {
                return None;
            }
            let head_range = head_block_line_range(head_block);
            let overlap = line_range_overlap(&mapped_base_range, &head_range);
            (overlap > 0).then_some((index, overlap))
        })
        .max_by_key(|(_, overlap)| *overlap)
        .map(|(index, _)| index)
}

fn mapped_head_range_for_base_block(
    base_block: &Block,
    hunks: &[vcs::DiffHunk],
) -> std::ops::Range<u32> {
    let start = map_base_line_to_head_anchor(
        u32::try_from(base_block.start_line.saturating_add(1)).unwrap_or(u32::MAX),
        hunks,
    );
    let end_inclusive = u32::try_from(base_block.end_line).unwrap_or(u32::MAX);
    let end_anchor = map_base_line_to_head_anchor(end_inclusive.max(start), hunks);
    start..end_anchor.saturating_add(1)
}

fn map_base_line_to_head_anchor(old_line: u32, hunks: &[vcs::DiffHunk]) -> u32 {
    let mut old_cursor = 1u32;
    let mut new_cursor = 1u32;

    for hunk in hunks {
        while old_cursor < hunk.old_start {
            if old_line == old_cursor {
                return new_cursor;
            }
            old_cursor = old_cursor.saturating_add(1);
            new_cursor = new_cursor.saturating_add(1);
        }

        for line in &hunk.lines {
            match line.kind {
                vcs::DiffLineKind::Context => {
                    if old_line == old_cursor {
                        return new_cursor;
                    }
                    old_cursor = old_cursor.saturating_add(1);
                    new_cursor = new_cursor.saturating_add(1);
                }
                vcs::DiffLineKind::Removed => {
                    if old_line == old_cursor {
                        return new_cursor;
                    }
                    old_cursor = old_cursor.saturating_add(1);
                }
                vcs::DiffLineKind::Added => {
                    new_cursor = new_cursor.saturating_add(1);
                }
            }
        }
    }

    let delta = i64::from(new_cursor) - i64::from(old_cursor);
    let mapped = i64::from(old_line) + delta;
    u32::try_from(mapped.max(1).min(i64::from(u32::MAX))).unwrap_or(u32::MAX)
}

fn head_block_line_range(block: &Block) -> std::ops::Range<u32> {
    let start = u32::try_from(block.start_line.saturating_add(1)).unwrap_or(u32::MAX);
    let end = u32::try_from(block.end_line.saturating_add(1)).unwrap_or(u32::MAX);
    start..end
}

fn line_range_overlap(a: &std::ops::Range<u32>, b: &std::ops::Range<u32>) -> u32 {
    let start = a.start.max(b.start);
    let end = a.end.min(b.end);
    end.saturating_sub(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;
    use crate::test_git::{CurrentDirGuard, run_git, temp_git_repo};
    use std::fs;
    use std::path::Path;

    fn make_block(kind: BlockKind, tags: &[&str]) -> Block {
        Block {
            hash: TreeHash::new("hash"),
            content: "content".to_string(),
            kind,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            complexity: None,
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
    fn parse_review_request_all_rejects_explicit_targets() {
        let err = parse_review_request(
            true,
            &[ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap())],
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Explicit review targets cannot be combined with --all")
        );
    }

    #[test]
    fn parse_review_request_defaults_to_dirty_worktree() {
        let request = parse_review_request(false, &[], None)
            .unwrap_or_else(|error| panic!("expected default request: {error}"));
        assert_eq!(
            request,
            ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree])
        );
    }

    #[test]
    fn parse_review_request_parses_typed_file_and_revision_targets() {
        let request = parse_review_request(
            false,
            &[
                ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
                ReviewTarget::RevisionRange(RevisionRangeSpec::new("abc1234", "def5678").unwrap()),
            ],
            None,
        )
        .unwrap_or_else(|error| panic!("expected typed targets: {error}"));

        assert_eq!(
            request,
            ReviewRequest::Targets(vec![
                ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
                ReviewTarget::RevisionRange(RevisionRangeSpec::new("abc1234", "def5678").unwrap()),
            ])
        );
    }

    #[test]
    fn review_target_from_str_supports_dirty_and_main() {
        assert_eq!(
            ReviewTarget::from_cli("dirty").unwrap(),
            ReviewTarget::DirtyWorktree
        );
        assert_eq!(
            ReviewTarget::from_cli("main").unwrap(),
            ReviewTarget::MainDiff
        );
    }

    #[test]
    fn parse_review_request_expands_since_to_head_range() {
        let scope = resolve_cli_review_scope_with(false, &[], Some("HEAD"), |_| Ok(()))
            .unwrap_or_else(|error| panic!("expected since scope: {error}"));
        let request = scope.review_request();

        assert_eq!(
            request,
            ReviewRequest::Targets(vec![ReviewTarget::RevisionRange(
                RevisionRangeSpec::new("HEAD", "HEAD").unwrap()
            )])
        );
    }

    #[test]
    fn expand_cli_review_targets_combines_since_with_explicit_targets() {
        let targets = expand_cli_review_targets_with(
            &[ReviewTarget::Dir(RepoPath::new("src").unwrap())],
            Some("abc1234"),
            &|_| Ok(()),
        )
        .unwrap_or_else(|error| panic!("expected combined since+target expansion: {error}"));

        assert_eq!(
            targets,
            vec![
                ReviewTarget::Dir(RepoPath::new("src").unwrap()),
                ReviewTarget::RevisionRange(RevisionRangeSpec::new("abc1234", "HEAD").unwrap()),
            ]
        );
    }

    #[test]
    fn since_review_target_rejects_unknown_revision_early() {
        let err = since_review_target_with("definitely-not-a-real-revision", &|revision| {
            Err(anyhow!("revision `{revision}` could not be resolved"))
        })
        .unwrap_err();
        assert!(err.to_string().contains("could not be resolved"));
    }

    #[test]
    fn resolve_review_request_rejects_mixed_historical_and_worktree_content_sources() {
        let targets = vec![
            ReviewTarget::MainDiff,
            ReviewTarget::Revision(RevisionSpec::new("abc1234").unwrap()),
        ];

        let err = resolve_review_request(
            ReviewRequest::Targets(targets),
            BlockFilters::default(),
            ScanOptions::default(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Historical targets cannot be mixed with worktree-based targets")
        );
    }

    fn make_diff_block(
        hash: &str,
        kind: BlockKind,
        start_line: usize,
        end_line: usize,
        content: &str,
    ) -> Block {
        Block {
            hash: TreeHash::new(hash),
            content: content.to_string(),
            kind,
            tags: Vec::new(),
            complexity: None,
            start_line,
            end_line,
        }
    }

    fn diff_review_blocks(blocks: &[DiffReviewBlock]) -> Vec<&DiffBlockSides> {
        blocks.iter().map(|block| &block.sides).collect()
    }

    #[test]
    fn classify_file_change_kind_marks_head_and_base_as_changed() {
        assert_eq!(
            classify_file_change_kind(true, true),
            Some(FileChangeKind::Changed)
        );
    }

    #[test]
    fn classify_file_change_kind_marks_base_only_as_deleted() {
        assert_eq!(
            classify_file_change_kind(true, false),
            Some(FileChangeKind::Deleted)
        );
    }

    #[test]
    fn classify_file_change_kind_marks_head_only_as_added() {
        assert_eq!(
            classify_file_change_kind(false, true),
            Some(FileChangeKind::Added)
        );
    }

    #[test]
    fn classify_file_change_kind_returns_none_when_neither_side_exists() {
        assert_eq!(classify_file_change_kind(false, false), None);
    }

    #[test]
    fn classify_block_change_kind_marks_head_only_as_added() {
        let head_block = make_diff_block("head-only", BlockKind::Function, 0, 1, "fn add() {}\n");
        assert_eq!(
            classify_block_change_kind(&DiffBlockSides {
                base: None,
                head: Some(head_block),
            }),
            Some(BlockChangeKind::Added)
        );
    }

    #[test]
    fn classify_block_change_kind_marks_base_only_as_deleted() {
        let base_block = make_diff_block("base-only", BlockKind::Function, 0, 1, "fn gone() {}\n");
        assert_eq!(
            classify_block_change_kind(&DiffBlockSides {
                base: Some(base_block),
                head: None,
            }),
            Some(BlockChangeKind::Deleted)
        );
    }

    #[test]
    fn classify_block_change_kind_marks_paired_block_as_changed() {
        let base_block = make_diff_block(
            "base",
            BlockKind::Function,
            0,
            2,
            "fn demo() {\n    old();\n}\n",
        );
        let head_block = make_diff_block(
            "head",
            BlockKind::Function,
            0,
            2,
            "fn demo() {\n    new();\n}\n",
        );
        assert_eq!(
            classify_block_change_kind(&DiffBlockSides {
                base: Some(base_block),
                head: Some(head_block),
            }),
            Some(BlockChangeKind::Changed)
        );
    }

    #[test]
    fn classify_block_change_kind_returns_none_when_neither_side_exists() {
        assert_eq!(
            classify_block_change_kind(&DiffBlockSides {
                base: None,
                head: None,
            }),
            None
        );
    }

    #[test]
    fn collect_diff_review_blocks_includes_deleted_single_line_module() {
        let base_module = make_diff_block("base-module", BlockKind::Module, 0, 1, "mod common;\n");
        let blocks = collect_diff_review_blocks_for_file(
            std::slice::from_ref(&base_module),
            &[],
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![vcs::DiffHunkLine::removed("mod common;\n")],
            }],
        );

        let sides = diff_review_blocks(&blocks);
        assert_eq!(sides.len(), 1, "expected deleted module review unit");
        assert_eq!(
            sides[0].base.as_ref().map(|block| block.hash.as_str()),
            Some(base_module.hash.as_str())
        );
        assert!(sides[0].head.is_none());
        assert!(sides[0].is_base_only());
    }

    #[test]
    fn collect_diff_review_blocks_includes_deleted_function() {
        let base_function = make_diff_block(
            "base-function",
            BlockKind::Function,
            0,
            3,
            "fn removed() {\n    old_body();\n}\n",
        );
        let blocks = collect_diff_review_blocks_for_file(
            std::slice::from_ref(&base_function),
            &[],
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("fn removed() {\n"),
                    vcs::DiffHunkLine::removed("    old_body();\n"),
                    vcs::DiffHunkLine::removed("}\n"),
                ],
            }],
        );

        let sides = diff_review_blocks(&blocks);
        assert_eq!(sides.len(), 1, "expected deleted function review unit");
        assert_eq!(
            sides[0].base.as_ref().map(|block| block.hash.as_str()),
            Some(base_function.hash.as_str())
        );
        assert!(sides[0].head.is_none());
    }

    #[test]
    fn collect_diff_review_blocks_keeps_modified_function_and_deleted_code_paragraph() {
        let base_function = make_diff_block(
            "base-function",
            BlockKind::Function,
            0,
            4,
            "fn keep() {\n    first();\n    second();\n}\n",
        );
        let deleted_paragraph = make_diff_block(
            "deleted-paragraph",
            BlockKind::CodeParagraph,
            2,
            3,
            "    second();\n",
        );
        let head_function = make_diff_block(
            "head-function",
            BlockKind::Function,
            0,
            3,
            "fn keep() {\n    first();\n}\n",
        );

        let blocks = collect_diff_review_blocks_for_file(
            &[base_function.clone(), deleted_paragraph.clone()],
            std::slice::from_ref(&head_function),
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::context("fn keep() {\n"),
                    vcs::DiffHunkLine::context("    first();\n"),
                    vcs::DiffHunkLine::removed("    second();\n"),
                    vcs::DiffHunkLine::context("}\n"),
                ],
            }],
        );

        let sides = diff_review_blocks(&blocks);
        assert_eq!(
            sides.len(),
            2,
            "expected one modified function review unit and one deleted paragraph"
        );
        assert!(
            sides.iter().any(|side| {
                side.base.as_ref().map(|block| block.hash.as_str())
                    == Some(base_function.hash.as_str())
                    && side.head.as_ref().map(|block| block.hash.as_str())
                        == Some(head_function.hash.as_str())
            }),
            "expected paired function review unit: {sides:?}"
        );
        assert!(
            sides.iter().any(|side| {
                side.base.as_ref().map(|block| block.hash.as_str())
                    == Some(deleted_paragraph.hash.as_str())
                    && side.head.is_none()
            }),
            "expected deleted code paragraph review unit: {sides:?}"
        );
    }

    #[test]
    fn collect_review_main_diff_includes_deleted_base_only_module_block() {
        let repo_root = temp_git_repo("review_deleted_module_collect");
        let file_path = repo_root.join("src/lib.rs");
        fs::create_dir_all(file_path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_else(|error| panic!("failed to create fixture directory: {error}"));
        fs::write(
            &file_path,
            concat!(
                "mod common;\n",
                "\n",
                "fn kept() {\n",
                "    body();\n",
                "}\n"
            ),
        )
        .unwrap_or_else(|error| panic!("failed to write initial fixture file: {error}"));

        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Initial"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        run_git(&repo_root, &["switch", "-c", "feature"]);

        fs::write(&file_path, concat!("fn kept() {\n", "    body();\n", "}\n"))
            .unwrap_or_else(|error| panic!("failed to write updated fixture file: {error}"));
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "Delete module"]);

        let _guard = CurrentDirGuard::push(&repo_root);
        let query = resolve_review_request(
            ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
            BlockFilters::default(),
            ScanOptions::default(),
        )
        .unwrap_or_else(|error| panic!("expected main diff query: {error}"));
        let collected = collect_review(&query)
            .unwrap_or_else(|error| panic!("expected collected review: {error}"));

        let deleted_block = collected
            .summary
            .files
            .iter()
            .flat_map(|file| file.blocks.iter())
            .find(|block| block.content.contains("mod common;"))
            .unwrap_or_else(|| panic!("expected deleted module block in summary"));
        assert!(
            !deleted_block.content.trim().is_empty(),
            "expected deleted block to keep original content"
        );

        let node_id = collected
            .tree
            .find_block_node("src/lib.rs", deleted_block)
            .unwrap_or_else(|| panic!("expected deleted module node in tree"));
        let sides = collected
            .diff_block_sides
            .get(&node_id)
            .unwrap_or_else(|| panic!("expected diff sides for deleted module node"));
        assert!(sides.is_base_only());
        assert!(
            sides
                .base
                .as_ref()
                .is_some_and(|block| block.content.contains("mod common;"))
        );
        assert!(sides.head.is_none());
    }
}
