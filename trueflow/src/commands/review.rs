use crate::analysis::Language;
use crate::block::{Block, BlockKind, ByteSpan};
use crate::config::{BlockFilters, load as load_config};
use crate::context::TrueflowContext;
use crate::coverage::{CoverageBuildOptions, CoverageIndex};
use crate::policy::{
    should_skip_container_by_default, should_skip_imports_by_default,
    should_skip_whitespace_only_by_default,
};
use crate::repo_path::RepoPath;
use crate::review_metadata;
use crate::scanner::{self, ScanDiagnostic, ScanOptions};
use crate::store::{FileStore, ReviewCheck, ReviewStore, Verdict};
use crate::sub_splitter;
use crate::targets::{
    ResolvedTargets, ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget,
    ReviewPathSelection, extract_pull_request_target, resolve_targets,
    workdir_prefix_from_git_root,
};
use crate::tree;
use crate::vcs;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use tracing::info;

pub use crate::targets::{ReviewTarget, RevisionExpr, RevisionRangeExpr};

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
    pub commented_block_nodes: HashSet<tree::TreeNodeId>,
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

struct DiffReviewPresentation {
    unreviewed_files: Vec<UnreviewedFile>,
    total_blocks: usize,
    unreviewed_block_nodes: HashSet<tree::TreeNodeId>,
    commented_block_nodes: HashSet<tree::TreeNodeId>,
}

pub fn parse_review_request(
    all: bool,
    values: &[ReviewTarget],
    since: Option<&str>,
) -> Result<ReviewRequest> {
    let targets = expand_cli_review_targets(values, since)?;
    review_request_from_cli_targets(all, &targets)
}

pub(crate) fn review_request_from_cli_targets(
    all: bool,
    targets: &[ReviewTarget],
) -> Result<ReviewRequest> {
    if all {
        if !targets.is_empty() {
            return Err(anyhow!(
                "Explicit review targets cannot be combined with --all"
            ));
        }
        return Ok(ReviewRequest::AllFiles);
    }

    if targets.is_empty() {
        Ok(ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree]))
    } else {
        Ok(ReviewRequest::Targets(targets.to_vec()))
    }
}

pub fn expand_cli_review_targets(
    values: &[ReviewTarget],
    since: Option<&str>,
) -> Result<Vec<ReviewTarget>> {
    expand_cli_review_targets_with(values, since, &validate_revision_exists_str)
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
    let start = RevisionExpr::new(since)?;
    validate_revision(start.as_str())?;
    validate_revision("HEAD")?;
    Ok(ReviewTarget::RevisionRange(RevisionRangeExpr::new(
        start.as_str(),
        "HEAD",
    )?))
}

fn validate_revision_exists_str(revision: &str) -> Result<()> {
    let repo = vcs::repo_from_workdir().context("git repository required for revision targets")?;
    vcs::resolve_commit_id_in_repo(&repo, revision)?;
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

    if matches!(query.path_selection, ReviewPathSelection::Empty) {
        return Ok(empty_collected_review());
    }

    let selected_paths = preselected_paths_for_review(&query.path_selection);
    let (files, diagnostics) = match &query.content_source {
        ReviewContentSource::Workdir => {
            let scan_result = if let Some(paths) = selected_paths.as_ref() {
                scanner::scan_paths(".", paths, &query.scan_options)?
            } else {
                scanner::scan_directory(".", &query.scan_options)?
            };
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
            let files = if let Some(paths) = selected_paths.as_ref() {
                vcs::file_states_for_paths_in_revision(
                    repo,
                    revision.as_str(),
                    paths,
                    workdir_prefix.as_deref(),
                )?
            } else {
                vcs::file_states_in_revision(repo, revision.as_str(), workdir_prefix.as_deref())?
            };
            (files, Vec::new())
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
    let coverage =
        CoverageIndex::build(&tree, &database, &CoverageBuildOptions { workdir_prefix })?;

    let mut unreviewed_files = Vec::new();
    let mut total_blocks = 0;
    let mut unreviewed_block_nodes = HashSet::new();
    let mut commented_block_nodes = HashSet::new();

    for file in files {
        if !query.path_selection.includes(&file.path) {
            continue;
        }

        let language = file.language;
        let mut reviewable_blocks = Vec::new();
        for block in file.blocks {
            if !query.filters.allows_block(block.kind) {
                continue;
            }
            if should_skip_whitespace_only_by_default(&block, &query.filters) {
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
            reviewable_blocks.push((block, node_id));
        }
        total_blocks += reviewable_blocks.len();

        let mut unreviewed_blocks = Vec::new();
        for (block, node_id) in reviewable_blocks {
            let block_coverage = coverage.block(&file.path, &block);
            let effective_verdict = block_coverage.effective_latest_verdict_for(&review_check);
            if effective_verdict == Some(&Verdict::Approved) {
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
                if effective_verdict == Some(&Verdict::Comment) {
                    commented_block_nodes.insert(node_id);
                }
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
        file.blocks.sort_by_key(|block| {
            (
                kind_rank(block),
                block.start_byte,
                block.end_byte,
                block.start_line,
                block.end_line,
            )
        });
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
        commented_block_nodes,
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
        &query.path_selection,
        diff_context.repo,
        files,
        diff_context.diff_targets,
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
    let DiffReviewPresentation {
        unreviewed_files,
        total_blocks,
        unreviewed_block_nodes,
        commented_block_nodes,
    } = collect_diff_review_presentation(
        query,
        &review_files,
        &tree,
        &coverage,
        diff_context.review_check,
    );

    Ok(CollectedReview {
        summary: ReviewSummary {
            files: unreviewed_files,
            total_blocks,
            diagnostics,
        },
        tree,
        unreviewed_block_nodes,
        commented_block_nodes,
        diff_block_sides,
        file_change_kinds,
        block_change_kinds,
    })
}

fn collect_diff_review_presentation(
    query: &ResolvedReviewQuery,
    review_files: &[DiffReviewFile],
    tree: &tree::Tree,
    coverage: &CoverageIndex<'_>,
    review_check: &ReviewCheck,
) -> DiffReviewPresentation {
    let mut unreviewed_files = Vec::new();
    let mut total_blocks = 0usize;
    let mut unreviewed_block_nodes = HashSet::new();
    let mut commented_block_nodes = HashSet::new();

    for file in review_files {
        let mut unreviewed_blocks = Vec::new();
        for review_block in &file.blocks {
            let display_block = review_block.display_block().clone();
            if !query.filters.allows_block(display_block.kind) {
                continue;
            }
            if should_skip_whitespace_only_by_default(&display_block, &query.filters) {
                continue;
            }
            if should_skip_imports_by_default(file.path.as_str(), &display_block, &query.filters) {
                continue;
            }
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
            let effective_verdict = block_coverage.effective_latest_verdict_for(review_check);
            if effective_verdict == Some(&Verdict::Approved) {
                continue;
            }

            if block_coverage
                .direct_latest_verdict_for(review_check)
                .is_none()
                && is_subblock_covered(
                    &display_block,
                    file.language,
                    query,
                    coverage,
                    review_check,
                    &file.path,
                )
            {
                continue;
            }

            unreviewed_block_nodes.insert(node_id);
            if effective_verdict == Some(&Verdict::Comment) {
                commented_block_nodes.insert(node_id);
            }
            unreviewed_blocks.push(display_block);
        }

        if !unreviewed_blocks.is_empty() {
            unreviewed_files.push(UnreviewedFile {
                path: file.path.clone(),
                language: file.language,
                blocks: unreviewed_blocks,
            });
        }
    }

    for file in &mut unreviewed_files {
        file.blocks.sort_by_key(|block| {
            (
                kind_rank(block),
                block.start_byte,
                block.end_byte,
                block.start_line,
                block.end_line,
            )
        });
    }
    unreviewed_files.sort_by(|a, b| {
        let rank_fn = |file: &UnreviewedFile| file.blocks.first().map_or(100, kind_rank);
        (rank_fn(a), &a.path).cmp(&(rank_fn(b), &b.path))
    });

    DiffReviewPresentation {
        unreviewed_files,
        total_blocks,
        unreviewed_block_nodes,
        commented_block_nodes,
    }
}

fn preselected_paths_for_review(path_selection: &ReviewPathSelection) -> Option<HashSet<RepoPath>> {
    match path_selection {
        ReviewPathSelection::All => None,
        ReviewPathSelection::Empty => Some(HashSet::new()),
        ReviewPathSelection::Scoped {
            changed: Some(changed),
            ..
        } => Some(
            changed
                .iter()
                .filter(|path| path_selection.includes(&path.location))
                .map(|path| path.location.clone())
                .collect(),
        ),
        ReviewPathSelection::Scoped {
            files,
            dirs,
            changed: None,
        } if dirs.is_empty() => Some(files.clone()),
        ReviewPathSelection::Scoped { .. } => None,
    }
}

fn empty_collected_review() -> CollectedReview {
    CollectedReview {
        summary: ReviewSummary {
            files: Vec::new(),
            total_blocks: 0,
            diagnostics: Vec::new(),
        },
        tree: tree::TreeBuilder::new().finalize(),
        unreviewed_block_nodes: HashSet::new(),
        commented_block_nodes: HashSet::new(),
        diff_block_sides: HashMap::new(),
        file_change_kinds: HashMap::new(),
        block_change_kinds: HashMap::new(),
    }
}

struct TargetDiffBatch<'a> {
    target: &'a ReviewDiffTarget,
    diffs: vcs::SelectedFileDiffs,
}

#[derive(Clone)]
struct TargetFileDiffInput {
    base: Option<crate::block::FileState>,
    file_diff: vcs::FileDiff,
}

fn collect_target_diff_batches<'a>(
    repo: &gix::Repository,
    diff_targets: &'a [ReviewDiffTarget],
    selected_destinations: &[RepoPath],
) -> Result<Vec<TargetDiffBatch<'a>>> {
    diff_targets
        .iter()
        .map(|target| {
            let diffs = match target {
                ReviewDiffTarget::MainDiff => {
                    vcs::file_diffs_for_main_to_head(repo, selected_destinations)?
                }
                ReviewDiffTarget::Revision(revision) => {
                    vcs::file_diffs_for_revision(repo, revision.as_str(), selected_destinations)?
                }
                ReviewDiffTarget::RevisionRange(range) => vcs::file_diffs_for_range(
                    repo,
                    range.start.as_str(),
                    range.end.as_str(),
                    selected_destinations,
                )?,
            };
            Ok(TargetDiffBatch { target, diffs })
        })
        .collect()
}

fn collect_diff_review_files(
    path_selection: &ReviewPathSelection,
    repo: &gix::Repository,
    files: Vec<crate::block::FileState>,
    diff_targets: &[ReviewDiffTarget],
) -> Result<Vec<DiffReviewFile>> {
    let mut head_files_by_path = files
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut selected_paths = selected_review_paths(path_selection, &head_files_by_path);
    selected_paths.sort();
    let mut target_batches = collect_target_diff_batches(repo, diff_targets, &selected_paths)?;

    let mut review_files = Vec::new();
    for display_path in selected_paths {
        let head_file = head_files_by_path.remove(&display_path);
        let mut target_inputs = Vec::with_capacity(target_batches.len());
        for target_batch in &mut target_batches {
            let file_diff = target_batch.diffs.take(&display_path);
            let base = match target_batch.target {
                ReviewDiffTarget::MainDiff => vcs::file_state_for_path_in_main_base(
                    repo,
                    &file_diff.changed_path().source_location,
                    &file_diff.changed_path().location,
                )?,
                ReviewDiffTarget::Revision(revision) => vcs::file_state_for_path_in_revision_base(
                    repo,
                    revision.as_str(),
                    &file_diff.changed_path().source_location,
                    &file_diff.changed_path().location,
                )?,
                ReviewDiffTarget::RevisionRange(range) => vcs::file_state_for_path_in_revision(
                    repo,
                    range.start.as_str(),
                    &file_diff.changed_path().source_location,
                    &file_diff.changed_path().location,
                )?,
            };
            target_inputs.push(TargetFileDiffInput { base, file_diff });
        }

        let has_base = target_inputs.iter().any(|input| input.base.is_some());
        let base_language = target_inputs
            .iter()
            .find_map(|input| input.base.as_ref().map(|file| file.language));
        let base_file_hash = target_inputs
            .iter()
            .find_map(|input| input.base.as_ref().map(|file| file.tree_hash.clone()));
        let mut head_blocks = head_file
            .as_ref()
            .map(|file| file.blocks.clone())
            .unwrap_or_default();
        head_blocks.sort_by_key(|block| {
            (
                block.start_byte,
                Reverse(block.end_byte),
                block.start_line,
                Reverse(block.end_line),
                block.kind.as_str(),
            )
        });

        let blocks = collect_diff_review_blocks_for_target_inputs(&target_inputs, &head_blocks);
        if blocks.is_empty() {
            continue;
        }

        let language = head_file
            .as_ref()
            .map(|file| file.language)
            .or(base_language)
            .unwrap_or(Language::Unknown);
        let file_hash = head_file
            .as_ref()
            .map(|file| file.tree_hash.clone())
            .or(base_file_hash)
            .unwrap_or_default();

        let Some(change_kind) = classify_file_change_kind(has_base, head_file.is_some()) else {
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
) -> Vec<RepoPath> {
    match path_selection {
        ReviewPathSelection::All => head_files_by_path.keys().cloned().collect(),
        ReviewPathSelection::Empty => Vec::new(),
        ReviewPathSelection::Scoped {
            files,
            dirs,
            changed,
        } => {
            let candidate_paths = if let Some(changed_paths) = changed {
                files
                    .iter()
                    .cloned()
                    .chain(changed_paths.iter().map(|changed| changed.location.clone()))
                    .collect::<HashSet<_>>()
            } else if dirs.is_empty() {
                files.iter().cloned().collect::<HashSet<_>>()
            } else {
                head_files_by_path.keys().cloned().collect::<HashSet<_>>()
            };

            let mut selected = Vec::new();
            for path in candidate_paths {
                if path_selection.includes(&path) {
                    selected.push(path);
                }
            }
            selected
        }
    }
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

                let (base_parents, head_parents) = diff_parent_maps(&file.blocks);
                let parents = file
                    .blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| {
                        if block.sides.head.is_some() {
                            head_parents[index]
                        } else {
                            base_parents[index]
                        }
                    })
                    .collect::<Vec<_>>();
                let depths = diff_parent_depths(&parents)?;
                let mut insertion_order = (0..file.blocks.len()).collect::<Vec<_>>();
                insertion_order.sort_by_key(|index| {
                    let block = file.blocks[*index].display_block();
                    (
                        depths[*index],
                        block.start_byte,
                        Reverse(block.end_byte),
                        *index,
                    )
                });

                let mut node_ids = vec![None; file.blocks.len()];
                for entry_index in insertion_order {
                    let review_block = &file.blocks[entry_index];
                    let parent = match parents[entry_index] {
                        Some(parent_index) => node_ids[parent_index].ok_or_else(|| {
                            anyhow!(
                                "diff parent must be inserted before child: parent={parent_index}, child={entry_index}"
                            )
                        })?,
                        None => file_id,
                    };
                    let display_block = review_block.display_block().clone();
                    let node_id = builder.add_block(
                        parent,
                        diff_block_label(&display_block),
                        next_path.clone(),
                        display_block,
                        file.language,
                    );
                    node_ids[entry_index] = Some(node_id);
                    diff_block_sides.insert(node_id, review_block.sides.clone());
                    block_change_kinds.insert(node_id, review_block.change_kind);
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

#[derive(Debug, Clone, Copy)]
enum DiffCoordinateSide {
    Base,
    Head,
}

fn diff_parent_maps(blocks: &[DiffReviewBlock]) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    (
        diff_parent_map_for_side(blocks, DiffCoordinateSide::Base),
        diff_parent_map_for_side(blocks, DiffCoordinateSide::Head),
    )
}

fn diff_parent_map_for_side(
    blocks: &[DiffReviewBlock],
    side: DiffCoordinateSide,
) -> Vec<Option<usize>> {
    let mut parents = vec![None; blocks.len()];
    let mut ordered = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, review_block)| {
            let block = match side {
                DiffCoordinateSide::Base => review_block.sides.base.as_ref(),
                DiffCoordinateSide::Head => review_block.sides.head.as_ref(),
            }?;
            Some((index, block))
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(index, block)| {
        (
            block.start_byte,
            Reverse(block.end_byte),
            block.start_line,
            Reverse(block.end_line),
            *index,
        )
    });

    let mut containers: Vec<(usize, ByteSpan)> = Vec::new();
    for (index, block) in ordered {
        let byte_span = block.byte_span();
        while let Some((_, container_span)) = containers.last() {
            if container_span.properly_contains(&byte_span) {
                break;
            }
            containers.pop();
        }

        parents[index] = containers.last().map(|(parent, _)| *parent);
        if block.kind.can_contain_review_children() {
            containers.push((index, byte_span));
        }
    }

    parents
}

fn diff_parent_depths(parents: &[Option<usize>]) -> Result<Vec<usize>> {
    parents
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let mut depth = 0usize;
            let mut seen = HashSet::new();
            let mut current = parents[index];
            while let Some(parent) = current {
                if !seen.insert(parent) {
                    return Err(anyhow!(
                        "diff block parent relation must be acyclic: entry={index}, parent={parent}"
                    ));
                }
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("diff block parent depth overflow"))?;
                current = parents[parent];
            }
            Ok(depth)
        })
        .collect()
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

pub fn collect_main_diff_summary() -> Result<ReviewSummary> {
    let config = load_config()?;
    let filters = config.review.resolve_filters(&[], &[]);
    let scan_options = config.scan.resolve_options();
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
    let scan_options = config.scan.resolve_options();
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
    let targets = resolve_review_command_targets(all, target, since)?;
    let request = review_request_from_cli_targets(all, &targets)?;
    run_request(json, request, only, exclude)
}

fn resolve_review_command_targets(
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
) -> Result<Vec<ReviewTarget>> {
    let targets = expand_cli_review_targets(target, since)?;
    let _ = review_request_from_cli_targets(all, &targets)?;

    if let Some(_pull_request) = extract_pull_request_target(&targets)? {
        return Err(anyhow!(
            "Pull request targets are only supported by `trueflow tui --target ...`"
        ));
    }

    Ok(targets)
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
    let mut pending = vec![block.clone()];

    while let Some(block) = pending.pop() {
        if !query.filters.allows_subblock(block.kind) {
            continue;
        }
        if should_skip_whitespace_only_by_default(&block, &query.filters) {
            continue;
        }

        if coverage
            .block(file_path, &block)
            .direct_latest_verdict_for(review_check)
            == Some(&Verdict::Approved)
        {
            continue;
        }

        let Ok(sub_split) = sub_splitter::split_result(&block, language) else {
            return false;
        };
        if sub_split.blocks.is_empty() {
            return false;
        }

        match sub_split.semantics {
            sub_splitter::SubSplitSemantics::ReviewUnits => {
                if sub_split.blocks.iter().any(|sub_block| {
                    query.filters.allows_subblock(sub_block.kind)
                        && !should_skip_whitespace_only_by_default(sub_block, &query.filters)
                        && coverage
                            .block(file_path, sub_block)
                            .direct_latest_verdict_for(review_check)
                            != Some(&Verdict::Approved)
                }) {
                    return false;
                }
            }
            sub_splitter::SubSplitSemantics::StructuralChildren => {
                return false;
            }
        }
    }

    true
}

fn collect_diff_review_blocks_for_target_inputs(
    target_inputs: &[TargetFileDiffInput],
    head_blocks: &[Block],
) -> Vec<DiffReviewBlock> {
    let mut candidates = Vec::new();
    for target_input in target_inputs {
        let base_blocks = target_input
            .base
            .as_ref()
            .map(|file| file.blocks.as_slice())
            .unwrap_or_default();
        candidates.extend(collect_diff_review_blocks_for_file(
            base_blocks,
            head_blocks,
            target_input.file_diff.hunks(),
        ));
    }
    dedupe_diff_review_blocks(candidates)
}

fn collect_diff_review_blocks_for_file(
    base_blocks: &[Block],
    head_blocks: &[Block],
    hunks: &[vcs::DiffHunk],
) -> Vec<DiffReviewBlock> {
    let changed_lines = vcs::DiffChangedLineIndex::from_hunks(hunks);
    let mut unmatched_base = vec![true; base_blocks.len()];
    let mut unmatched_head = vec![true; head_blocks.len()];
    // Reserve semantic survivors before positional evidence can consume them.
    let mut matched = reserve_unique_semantic_matches(
        base_blocks,
        head_blocks,
        &mut unmatched_base,
        &mut unmatched_head,
    );
    // Remaining survivors require real unchanged old/head coordinate overlap.
    let unchanged_lines = UnchangedLineIndex::from_hunks(hunks);
    matched.extend(reserve_unique_unchanged_line_matches(
        base_blocks,
        head_blocks,
        &mut unmatched_base,
        &mut unmatched_head,
        &unchanged_lines,
    ));
    // Positional anchors may pair only blocks independently changed on both sides.
    matched.extend(reserve_unique_positional_matches(
        base_blocks,
        head_blocks,
        &mut unmatched_base,
        &mut unmatched_head,
        &changed_lines,
        hunks,
    ));

    let mut diff_blocks = Vec::new();
    for (base_index, head_index) in matched {
        let base = &base_blocks[base_index];
        let head = &head_blocks[head_index];
        if changed_lines.change_kind_for_block(vcs::DiffBlockOwnership::Matched { base, head })
            == vcs::BlockDiffChangeKind::ReviewableChanges
        {
            diff_blocks.push(diff_review_block(Some(base.clone()), Some(head.clone())));
        }
    }

    for (index, base) in base_blocks.iter().enumerate() {
        if unmatched_base[index]
            && changed_lines.change_kind_for_block(vcs::DiffBlockOwnership::BaseOnly(base))
                == vcs::BlockDiffChangeKind::ReviewableChanges
        {
            diff_blocks.push(diff_review_block(Some(base.clone()), None));
        }
    }

    for (index, head) in head_blocks.iter().enumerate() {
        if unmatched_head[index]
            && changed_lines.change_kind_for_block(vcs::DiffBlockOwnership::HeadOnly(head))
                == vcs::BlockDiffChangeKind::ReviewableChanges
        {
            diff_blocks.push(diff_review_block(None, Some(head.clone())));
        }
    }

    sort_diff_review_blocks(&mut diff_blocks);
    diff_blocks
}

fn diff_review_block(base: Option<Block>, head: Option<Block>) -> DiffReviewBlock {
    let sides = DiffBlockSides { base, head };
    DiffReviewBlock {
        change_kind: classify_block_change_kind(&sides)
            .unwrap_or_else(|| panic!("diff review block must own at least one side")),
        sides,
    }
}

fn reserve_unique_semantic_matches(
    base_blocks: &[Block],
    head_blocks: &[Block],
    unmatched_base: &mut [bool],
    unmatched_head: &mut [bool],
) -> Vec<(usize, usize)> {
    let mut base_by_identifier = semantic_block_indices(base_blocks);
    let head_by_identifier = semantic_block_indices(head_blocks);
    let mut matches = Vec::new();

    for (identifier, base_indices) in base_by_identifier.drain() {
        let Some(head_indices) = head_by_identifier.get(&identifier) else {
            continue;
        };
        if base_indices.len() != 1 || head_indices.len() != 1 {
            continue;
        }

        let base_index = base_indices[0];
        let head_index = head_indices[0];
        if unmatched_base[base_index] && unmatched_head[head_index] {
            unmatched_base[base_index] = false;
            unmatched_head[head_index] = false;
            matches.push((base_index, head_index));
        }
    }
    matches.sort_unstable_by_key(|(base_index, _)| *base_index);
    matches
}

fn semantic_block_indices(blocks: &[Block]) -> HashMap<(BlockKind, String), Vec<usize>> {
    let mut by_identifier = HashMap::new();
    for (index, block) in blocks.iter().enumerate() {
        let Some(identifier) = review_metadata::semantic_block_identifier(block) else {
            continue;
        };
        by_identifier
            .entry((block.kind, identifier))
            .or_insert_with(Vec::new)
            .push(index);
    }
    by_identifier
}

fn reserve_unique_unchanged_line_matches(
    base_blocks: &[Block],
    head_blocks: &[Block],
    unmatched_base: &mut [bool],
    unmatched_head: &mut [bool],
    unchanged_lines: &UnchangedLineIndex,
) -> Vec<(usize, usize)> {
    let base_choices = base_blocks
        .iter()
        .enumerate()
        .map(|(base_index, base)| {
            unmatched_base[base_index].then(|| {
                unique_best_overlap(
                    head_blocks
                        .iter()
                        .enumerate()
                        .filter(|(head_index, head)| {
                            unmatched_head[*head_index] && head.kind == base.kind
                        })
                        .map(|(head_index, head)| {
                            (head_index, unchanged_lines.overlap_for_blocks(base, head))
                        }),
                )
            })
        })
        .collect::<Vec<_>>();
    let head_choices = head_blocks
        .iter()
        .enumerate()
        .map(|(head_index, head)| {
            unmatched_head[head_index].then(|| {
                unique_best_overlap(
                    base_blocks
                        .iter()
                        .enumerate()
                        .filter(|(base_index, base)| {
                            unmatched_base[*base_index] && base.kind == head.kind
                        })
                        .map(|(base_index, base)| {
                            (base_index, unchanged_lines.overlap_for_blocks(base, head))
                        }),
                )
            })
        })
        .collect::<Vec<_>>();

    let mut matches = Vec::new();
    for (base_index, head_index) in base_choices.into_iter().enumerate() {
        let Some(Some(head_index)) = head_index else {
            continue;
        };
        if head_choices[head_index] == Some(Some(base_index)) {
            unmatched_base[base_index] = false;
            unmatched_head[head_index] = false;
            matches.push((base_index, head_index));
        }
    }
    matches
}
fn reserve_unique_positional_matches(
    base_blocks: &[Block],
    head_blocks: &[Block],
    unmatched_base: &mut [bool],
    unmatched_head: &mut [bool],
    changed_lines: &vcs::DiffChangedLineIndex,
    hunks: &[vcs::DiffHunk],
) -> Vec<(usize, usize)> {
    let base_reviewable = base_blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            unmatched_base[index]
                && changed_lines.change_kind_for_block(vcs::DiffBlockOwnership::BaseOnly(block))
                    == vcs::BlockDiffChangeKind::ReviewableChanges
        })
        .collect::<Vec<_>>();
    let head_reviewable = head_blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            unmatched_head[index]
                && changed_lines.change_kind_for_block(vcs::DiffBlockOwnership::HeadOnly(block))
                    == vcs::BlockDiffChangeKind::ReviewableChanges
        })
        .collect::<Vec<_>>();
    let mapped_base_ranges = base_blocks
        .iter()
        .map(|block| mapped_head_range_for_base_block(block, hunks))
        .collect::<Vec<_>>();

    let base_choices = base_blocks
        .iter()
        .enumerate()
        .map(|(base_index, base)| {
            base_reviewable[base_index].then(|| {
                unique_best_overlap(
                    head_blocks
                        .iter()
                        .enumerate()
                        .filter(|(head_index, head)| {
                            head_reviewable[*head_index] && head.kind == base.kind
                        })
                        .map(|(head_index, head)| {
                            (
                                head_index,
                                line_range_overlap(
                                    &mapped_base_ranges[base_index],
                                    &block_line_range(head),
                                ),
                            )
                        }),
                )
            })
        })
        .collect::<Vec<_>>();
    let head_choices = head_blocks
        .iter()
        .enumerate()
        .map(|(head_index, head)| {
            head_reviewable[head_index].then(|| {
                unique_best_overlap(
                    base_blocks
                        .iter()
                        .enumerate()
                        .filter(|(base_index, base)| {
                            base_reviewable[*base_index] && base.kind == head.kind
                        })
                        .map(|(base_index, _)| {
                            (
                                base_index,
                                line_range_overlap(
                                    &mapped_base_ranges[base_index],
                                    &block_line_range(head),
                                ),
                            )
                        }),
                )
            })
        })
        .collect::<Vec<_>>();

    let mut matches = Vec::new();
    for (base_index, head_index) in base_choices.into_iter().enumerate() {
        let Some(Some(head_index)) = head_index else {
            continue;
        };
        if head_choices[head_index] == Some(Some(base_index)) {
            unmatched_base[base_index] = false;
            unmatched_head[head_index] = false;
            matches.push((base_index, head_index));
        }
    }
    matches
}

fn unique_best_overlap(overlaps: impl Iterator<Item = (usize, u32)>) -> Option<usize> {
    let mut best_index = None;
    let mut best_overlap = 0;
    let mut tied = false;
    for (index, overlap) in overlaps.filter(|(_, overlap)| *overlap > 0) {
        if overlap > best_overlap {
            best_index = Some(index);
            best_overlap = overlap;
            tied = false;
        } else if overlap == best_overlap {
            tied = true;
        }
    }
    (!tied).then_some(best_index).flatten()
}
fn mapped_head_range_for_base_block(
    base_block: &Block,
    hunks: &[vcs::DiffHunk],
) -> std::ops::Range<u32> {
    let old_start = u32::try_from(base_block.start_line.saturating_add(1)).unwrap_or(u32::MAX);
    let old_end_inclusive = u32::try_from(base_block.end_line)
        .unwrap_or(u32::MAX)
        .max(old_start);
    let start = map_base_line_to_head_anchor(old_start, hunks);
    let end_anchor = map_base_line_to_head_anchor(old_end_inclusive, hunks);
    start..end_anchor.saturating_add(1)
}

fn map_base_line_to_head_anchor(old_line: u32, hunks: &[vcs::DiffHunk]) -> u32 {
    let mut old_cursor = 1u32;
    let mut new_cursor = 1u32;
    let mut sorted_hunks = hunks.iter().collect::<Vec<_>>();
    sorted_hunks.sort_by_key(|hunk| (hunk.old_start, hunk.new_start));

    for hunk in sorted_hunks {
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
                vcs::DiffLineKind::Added => new_cursor = new_cursor.saturating_add(1),
            }
        }
    }

    let delta = i64::from(new_cursor) - i64::from(old_cursor);
    let mapped = i64::from(old_line) + delta;
    u32::try_from(mapped.max(1).min(i64::from(u32::MAX))).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, Copy)]
struct UnchangedLineSegment {
    base_start: u32,
    base_end: u32,
    head_start: u32,
}

#[derive(Debug, Default)]
struct UnchangedLineIndex {
    segments: Vec<UnchangedLineSegment>,
}

impl UnchangedLineIndex {
    fn from_hunks(hunks: &[vcs::DiffHunk]) -> Self {
        let mut sorted_hunks = hunks.iter().collect::<Vec<_>>();
        sorted_hunks.sort_by_key(|hunk| (hunk.old_start, hunk.new_start));

        let mut index = Self::default();
        let mut old_line = 1;
        let mut new_line = 1;
        for hunk in sorted_hunks {
            index.push_segment(old_line, hunk.old_start, new_line);
            old_line = hunk.old_start;
            new_line = hunk.new_start;

            for line in &hunk.lines {
                match line.kind {
                    vcs::DiffLineKind::Context => {
                        index.push_segment(old_line, old_line.saturating_add(1), new_line);
                        old_line = old_line.saturating_add(1);
                        new_line = new_line.saturating_add(1);
                    }
                    vcs::DiffLineKind::Removed => old_line = old_line.saturating_add(1),
                    vcs::DiffLineKind::Added => new_line = new_line.saturating_add(1),
                }
            }
        }
        index.push_segment(old_line, u32::MAX, new_line);
        index
    }

    fn overlap_for_blocks(&self, base: &Block, head: &Block) -> u32 {
        let base_range = block_line_range(base);
        let head_range = block_line_range(head);
        self.segments
            .iter()
            .map(|segment| {
                let base_overlap_start = base_range.start.max(segment.base_start);
                let base_overlap_end = base_range.end.min(segment.base_end);
                if base_overlap_start >= base_overlap_end {
                    return 0;
                }

                let offset = base_overlap_start.saturating_sub(segment.base_start);
                let mapped_head_start = segment.head_start.saturating_add(offset);
                let mapped_head_end =
                    mapped_head_start.saturating_add(base_overlap_end - base_overlap_start);
                line_range_overlap(&(mapped_head_start..mapped_head_end), &head_range)
            })
            .sum()
    }

    fn push_segment(&mut self, base_start: u32, base_end: u32, head_start: u32) {
        if base_start >= base_end {
            return;
        }
        if let Some(last) = self.segments.last_mut()
            && last.base_end == base_start
            && last
                .head_start
                .saturating_add(last.base_end - last.base_start)
                == head_start
        {
            last.base_end = base_end;
            return;
        }
        self.segments.push(UnchangedLineSegment {
            base_start,
            base_end,
            head_start,
        });
    }
}

fn dedupe_diff_review_blocks(blocks: Vec<DiffReviewBlock>) -> Vec<DiffReviewBlock> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(blocks.len());
    for block in blocks {
        let identity = (
            block
                .sides
                .base
                .as_ref()
                .map(|side| (side.hash.clone(), side.byte_span())),
            block
                .sides
                .head
                .as_ref()
                .map(|side| (side.hash.clone(), side.byte_span())),
        );
        if seen.insert(identity) {
            deduped.push(block);
        }
    }
    sort_diff_review_blocks(&mut deduped);
    deduped
}

fn sort_diff_review_blocks(blocks: &mut [DiffReviewBlock]) {
    blocks.sort_by(|left, right| {
        let left_display = left.display_block();
        let right_display = right.display_block();
        (
            left_display.start_byte,
            Reverse(left_display.end_byte),
            left_display.start_line,
            Reverse(left_display.end_line),
            left_display.kind.default_review_priority(),
            left_display.hash.as_str(),
            left.sides
                .base
                .as_ref()
                .map(|side| (side.hash.as_str(), side.start_byte, side.end_byte)),
            left.sides
                .head
                .as_ref()
                .map(|side| (side.hash.as_str(), side.start_byte, side.end_byte)),
        )
            .cmp(&(
                right_display.start_byte,
                Reverse(right_display.end_byte),
                right_display.start_line,
                Reverse(right_display.end_line),
                right_display.kind.default_review_priority(),
                right_display.hash.as_str(),
                right
                    .sides
                    .base
                    .as_ref()
                    .map(|side| (side.hash.as_str(), side.start_byte, side.end_byte)),
                right
                    .sides
                    .head
                    .as_ref()
                    .map(|side| (side.hash.as_str(), side.start_byte, side.end_byte)),
            ))
    });
}

fn block_line_range(block: &Block) -> std::ops::Range<u32> {
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
    use crate::block::ByteSpan;
    use crate::hashing::TreeHash;
    use crate::store::{CommitId, Record, ReviewDatabase};
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
            start_byte: 0,
            end_byte: "content".len(),
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

    fn approved_record_for_block(block: &Block, path: &str) -> Record {
        serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "version": crate::store::CURRENT_VERSION,
            "target": { "kind": "block", "hash": block.hash.as_str() },
            "check": "review",
            "verdict": "approved",
            "identity": { "type": "email", "email": "a@example.com" },
            "repo_ref": { "type": "vcs", "system": "git", "revision": "deadbeef" },
            "block_state": "committed",
            "timestamp": 1,
            "path_hint": path,
            "line_hint": block.start_line,
            "note": null,
            "comment_scope": null,
            "comment_context": null,
            "comment_anchor": null,
            "attestations": null
        }))
        .unwrap()
    }

    fn approved_hash_only_record_for_block(block: &Block) -> Record {
        serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "version": crate::store::CURRENT_VERSION,
            "target": { "kind": "block", "hash": block.hash.as_str() },
            "check": "review",
            "verdict": "approved",
            "identity": { "type": "email", "email": "a@example.com" },
            "repo_ref": { "type": "vcs", "system": "git", "revision": "deadbeef" },
            "block_state": "committed",
            "timestamp": 1,
            "path_hint": null,
            "line_hint": null,
            "note": null,
            "comment_scope": null,
            "comment_context": null,
            "comment_anchor": null,
            "attestations": null
        }))
        .unwrap()
    }

    #[test]
    fn structural_child_coverage_does_not_cover_container_header() {
        let source = "type User struct {\n\tName string\n\tAge int\n}\n";
        let container =
            Block::from_file_range(source, BlockKind::Struct, ByteSpan::new(0, source.len()))
                .unwrap_or_else(|error| panic!("container source range should be valid: {error}"));
        let children = match sub_splitter::split_result(&container, Language::Go) {
            Ok(children) => children,
            Err(error) => panic!("expected split result: {error:#}"),
        };
        assert_eq!(
            children.semantics,
            sub_splitter::SubSplitSemantics::StructuralChildren
        );
        assert!(!children.blocks.is_empty());

        let mut builder = crate::tree::TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(
            root,
            "model.go".to_string(),
            "src/model.go".to_string(),
            "file-hash".to_string(),
            Language::Go,
        );
        builder.add_block(
            file,
            "user".to_string(),
            "src/model.go".to_string(),
            container.clone(),
            Language::Go,
        );
        let tree = builder.finalize();
        let records = children
            .blocks
            .iter()
            .map(|block| approved_record_for_block(block, "src/model.go"))
            .collect::<Vec<_>>();
        let database = ReviewDatabase::from_records(records);
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();
        let query = ResolvedReviewQuery {
            filters: BlockFilters::default(),
            scan_options: ScanOptions::default(),
            content_source: ReviewContentSource::Workdir,
            path_selection: ReviewPathSelection::All,
            diff_selection: ReviewDiffSelection::None,
        };
        let review_check = ReviewCheck::new("review").unwrap();
        let file_path = RepoPath::new("src/model.go").unwrap();

        assert!(
            !is_subblock_covered(
                &container,
                Language::Go,
                &query,
                &coverage,
                &review_check,
                &file_path,
            ),
            "approved structural children must not cover omitted container header"
        );
    }

    #[test]
    fn diff_coverage_candidate_universe_precedes_display_filters() {
        let visible_content = std::iter::once("Shared review unit.\n".to_string())
            .chain((1..=50).map(|index| format!("Visible review unit {index}.\n")))
            .collect::<String>();
        let hidden_content = std::iter::once("Shared review unit.\n".to_string())
            .chain((1..=50).map(|index| format!("Hidden review unit {index}.\n")))
            .collect::<String>();
        let source = format!("{visible_content}{hidden_content}");
        let visible_parent = Block::from_file_range(
            &source,
            BlockKind::Paragraph,
            ByteSpan::new(0, visible_content.len()),
        )
        .unwrap_or_else(|error| panic!("visible parent should be source-backed: {error:#}"));
        let hidden_parent = Block::from_file_range(
            &source,
            BlockKind::Import,
            ByteSpan::new(visible_content.len(), source.len()),
        )
        .unwrap_or_else(|error| panic!("hidden parent should be source-backed: {error:#}"));
        let visible_units = sub_splitter::split_result(&visible_parent, Language::Text)
            .unwrap_or_else(|error| panic!("visible parent should split: {error:#}"));
        let hidden_units = sub_splitter::split_result(&hidden_parent, Language::Text)
            .unwrap_or_else(|error| panic!("hidden parent should split: {error:#}"));
        assert_eq!(
            visible_units.semantics,
            sub_splitter::SubSplitSemantics::ReviewUnits
        );
        assert_eq!(
            hidden_units.semantics,
            sub_splitter::SubSplitSemantics::ReviewUnits
        );
        assert!(visible_units.blocks.len() > 50);
        assert!(hidden_units.blocks.len() > 50);
        let visible_shared = &visible_units.blocks[0];
        let hidden_shared = &hidden_units.blocks[0];
        assert_eq!(visible_shared.hash, hidden_shared.hash);
        assert_ne!(visible_shared.byte_span(), hidden_shared.byte_span());

        let mut records = visible_units
            .blocks
            .iter()
            .skip(1)
            .chain(hidden_units.blocks.iter().skip(1))
            .map(|unit| approved_record_for_block(unit, "notes.txt"))
            .collect::<Vec<_>>();
        records.push(approved_hash_only_record_for_block(visible_shared));
        let database = ReviewDatabase::from_records(records);
        let review_files = vec![DiffReviewFile {
            path: RepoPath::new("notes.txt").unwrap(),
            language: Language::Text,
            file_hash: TreeHash::new("notes-tree"),
            change_kind: FileChangeKind::Changed,
            blocks: vec![
                DiffReviewBlock {
                    sides: DiffBlockSides {
                        base: Some(visible_parent.clone()),
                        head: Some(visible_parent.clone()),
                    },
                    change_kind: BlockChangeKind::Changed,
                },
                DiffReviewBlock {
                    sides: DiffBlockSides {
                        base: Some(hidden_parent.clone()),
                        head: Some(hidden_parent.clone()),
                    },
                    change_kind: BlockChangeKind::Changed,
                },
            ],
        }];
        let (tree, _, _, _) = build_tree_from_diff_review_files(&review_files)
            .unwrap_or_else(|error| panic!("complete changed diff tree should build: {error:#}"));
        let coverage =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions::default()).unwrap();
        let review_check = ReviewCheck::review();
        let file_path = RepoPath::new("notes.txt").unwrap();
        let modes = [
            (
                "only",
                BlockFilters::from_lists(&[BlockKind::Paragraph], &[]),
            ),
            (
                "exclude",
                BlockFilters::from_lists(&[], &[BlockKind::Import]),
            ),
            ("default", BlockFilters::default()),
        ];

        for (mode, filters) in modes {
            let query = ResolvedReviewQuery {
                filters,
                scan_options: ScanOptions::default(),
                content_source: ReviewContentSource::Workdir,
                path_selection: ReviewPathSelection::All,
                diff_selection: ReviewDiffSelection::None,
            };
            assert!(
                query.filters.allows_block(visible_parent.kind)
                    && !should_skip_whitespace_only_by_default(&visible_parent, &query.filters)
                    && !should_skip_imports_by_default(
                        file_path.as_str(),
                        &visible_parent,
                        &query.filters,
                    ),
                "{mode} must retain the visible changed parent"
            );
            assert!(
                !query.filters.allows_block(hidden_parent.kind)
                    || should_skip_whitespace_only_by_default(&hidden_parent, &query.filters)
                    || should_skip_imports_by_default(
                        file_path.as_str(),
                        &hidden_parent,
                        &query.filters,
                    ),
                "{mode} must hide the second changed parent from presentation"
            );
            assert_eq!(
                coverage
                    .block(&file_path, visible_shared)
                    .direct_latest_verdict_for(&review_check),
                None,
                "{mode} must leave the visible shared generated unit unapproved"
            );
            assert_eq!(
                coverage
                    .block(&file_path, hidden_shared)
                    .direct_latest_verdict_for(&review_check),
                None,
                "{mode} must retain the hidden shared generated unit as an ambiguous candidate"
            );
            assert!(
                !is_subblock_covered(
                    &visible_parent,
                    Language::Text,
                    &query,
                    &coverage,
                    &review_check,
                    &file_path,
                ),
                "{mode} must not clear the visible parent from the shared coarse approval"
            );
            let presentation = collect_diff_review_presentation(
                &query,
                &review_files,
                &tree,
                &coverage,
                &review_check,
            );
            let visible_node = tree
                .find_block_node(file_path.as_str(), &visible_parent)
                .unwrap_or_else(|| panic!("{mode} visible parent should remain in the diff tree"));
            let hidden_node = tree
                .find_block_node(file_path.as_str(), &hidden_parent)
                .unwrap_or_else(|| panic!("{mode} hidden parent should remain in the diff tree"));
            assert_eq!(
                presentation.total_blocks, 1,
                "{mode} must count only the visible changed parent"
            );
            assert_eq!(
                presentation.unreviewed_files.len(),
                1,
                "{mode} must present the visible parent as unreviewed"
            );
            assert_eq!(
                presentation.unreviewed_files[0].blocks.len(),
                1,
                "{mode} must not leak the hidden parent into the visible summary"
            );
            assert_eq!(
                (
                    presentation.unreviewed_files[0].blocks[0].hash.clone(),
                    presentation.unreviewed_files[0].blocks[0].byte_span(),
                ),
                (visible_parent.hash.clone(), visible_parent.byte_span()),
                "{mode} must present only the visible parent"
            );
            assert!(
                presentation.unreviewed_block_nodes.contains(&visible_node)
                    && !presentation.unreviewed_block_nodes.contains(&hidden_node)
                    && presentation.unreviewed_block_nodes.len() == 1,
                "{mode} must keep hidden diff nodes out of visible unreviewed state"
            );
            assert!(
                presentation.commented_block_nodes.is_empty(),
                "{mode} must keep hidden diff nodes out of visible comment state"
            );
        }
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
                ReviewTarget::RevisionRange(RevisionRangeExpr::new("abc1234", "def5678").unwrap()),
            ],
            None,
        )
        .unwrap_or_else(|error| panic!("expected typed targets: {error}"));

        assert_eq!(
            request,
            ReviewRequest::Targets(vec![
                ReviewTarget::File(RepoPath::new("src/lib.rs").unwrap()),
                ReviewTarget::RevisionRange(RevisionRangeExpr::new("abc1234", "def5678").unwrap()),
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
    fn resolve_review_command_targets_rejects_pull_request_target() {
        let err = resolve_review_command_targets(
            false,
            &[ReviewTarget::from_cli("pr:11").unwrap()],
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Pull request targets are only supported by `trueflow tui --target ...`")
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
                ReviewTarget::RevisionRange(RevisionRangeExpr::new("abc1234", "HEAD").unwrap()),
            ]
        );
    }

    #[test]
    fn collect_review_returns_empty_without_scanning_when_path_selection_is_empty() {
        let query = ResolvedReviewQuery {
            filters: BlockFilters::default(),
            scan_options: ScanOptions::default(),
            content_source: ReviewContentSource::Workdir,
            path_selection: ReviewPathSelection::Empty,
            diff_selection: ReviewDiffSelection::Targets(vec![ReviewDiffTarget::MainDiff]),
        };

        let collected = collect_review(&query)
            .unwrap_or_else(|error| panic!("expected empty review to collect: {error}"));

        assert!(collected.summary.files.is_empty());
        assert_eq!(collected.summary.total_blocks, 0);
        assert!(collected.unreviewed_block_nodes.is_empty());
    }

    #[test]
    fn resolve_review_request_rejects_mixed_historical_and_worktree_content_sources() {
        let targets = vec![
            ReviewTarget::MainDiff,
            ReviewTarget::Revision(RevisionExpr::new("abc1234").unwrap()),
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
        let start_byte = start_line;
        let end_byte = start_byte
            .checked_add(content.len())
            .unwrap_or_else(|| panic!("diff fixture byte end overflow"));
        Block {
            hash: TreeHash::new(hash),
            content: content.to_string(),
            kind,
            tags: Vec::new(),
            complexity: None,
            start_line,
            end_line,
            start_byte,
            end_byte,
        }
    }

    #[test]
    fn diff_tree_parenting_uses_shared_side_byte_containment_for_deleted_child() {
        let base_parent = make_diff_block("base-parent", BlockKind::Class, 0, 1, &"P".repeat(100));
        let head_parent = make_diff_block("head-parent", BlockKind::Class, 2, 3, &"H".repeat(100));
        let deleted_child = make_diff_block("deleted-child", BlockKind::Method, 0, 1, "child");
        let file = DiffReviewFile {
            path: RepoPath::new("src/Main.java").unwrap(),
            language: Language::Java,
            file_hash: TreeHash::new("file"),
            change_kind: FileChangeKind::Changed,
            blocks: vec![
                DiffReviewBlock {
                    sides: DiffBlockSides {
                        base: Some(deleted_child.clone()),
                        head: None,
                    },
                    change_kind: BlockChangeKind::Deleted,
                },
                DiffReviewBlock {
                    sides: DiffBlockSides {
                        base: Some(base_parent),
                        head: Some(head_parent.clone()),
                    },
                    change_kind: BlockChangeKind::Changed,
                },
            ],
        };

        let (tree, _, _, _) = build_tree_from_diff_review_files(&[file])
            .unwrap_or_else(|error| panic!("diff tree should build: {error}"));
        let parent_id = tree
            .find_block_node("src/Main.java", &head_parent)
            .unwrap_or_else(|| panic!("expected changed parent display node"));
        let child_id = tree
            .find_block_node("src/Main.java", &deleted_child)
            .unwrap_or_else(|| panic!("expected deleted child node"));

        assert_eq!(tree.parent(child_id), Some(parent_id));
    }

    fn diff_review_blocks(blocks: &[DiffReviewBlock]) -> Vec<&DiffBlockSides> {
        blocks.iter().map(|block| &block.sides).collect()
    }
    fn rust_review_blocks(content: &str) -> Vec<Block> {
        crate::block_splitter::split(content, Language::Rust).into_review_blocks(content)
    }
    fn target_file_diff_input(
        source_path: &str,
        base_content: &str,
        base_blocks: Vec<Block>,
        hunk: vcs::DiffHunk,
    ) -> TargetFileDiffInput {
        let source_location = RepoPath::new(source_path).unwrap();
        let location = RepoPath::new("src/dest.rs").unwrap();
        TargetFileDiffInput {
            base: Some(crate::block::FileState::from_text(
                source_location.clone(),
                Language::Rust,
                base_content.as_bytes(),
                base_blocks,
            )),
            file_diff: vcs::FileDiff::Text {
                changed_path: vcs::ChangedPath {
                    source_location,
                    location,
                },
                hunks: vec![hunk],
            },
        }
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
    fn collect_diff_review_blocks_does_not_extend_single_line_mapping_after_insertion() {
        let base = make_diff_block("base", BlockKind::CodeParagraph, 9, 10, "target\n");
        let shifted_target = make_diff_block(
            "head-target",
            BlockKind::CodeParagraph,
            10,
            11,
            "target changed\n",
        );
        let adjacent = make_diff_block(
            "head-adjacent",
            BlockKind::CodeParagraph,
            11,
            12,
            "adjacent changed\n",
        );

        let blocks = collect_diff_review_blocks_for_file(
            std::slice::from_ref(&base),
            &[shifted_target.clone(), adjacent.clone()],
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::added("inserted\n"),
                    vcs::DiffHunkLine::context("line1\n"),
                    vcs::DiffHunkLine::context("line2\n"),
                    vcs::DiffHunkLine::context("line3\n"),
                    vcs::DiffHunkLine::context("line4\n"),
                    vcs::DiffHunkLine::context("line5\n"),
                    vcs::DiffHunkLine::context("line6\n"),
                    vcs::DiffHunkLine::context("line7\n"),
                    vcs::DiffHunkLine::context("line8\n"),
                    vcs::DiffHunkLine::context("line9\n"),
                    vcs::DiffHunkLine::removed("target\n"),
                    vcs::DiffHunkLine::added("target changed\n"),
                    vcs::DiffHunkLine::removed("adjacent\n"),
                    vcs::DiffHunkLine::added("adjacent changed\n"),
                ],
            }],
        );

        let sides = diff_review_blocks(&blocks);
        assert_eq!(sides.len(), 2);
        assert_eq!(
            sides[0].head.as_ref().map(|block| block.hash.as_str()),
            Some(shifted_target.hash.as_str())
        );
        assert_eq!(
            sides[1].head.as_ref().map(|block| block.hash.as_str()),
            Some(adjacent.hash.as_str())
        );
        assert!(sides[1].base.is_none());
    }

    #[test]
    fn collect_diff_review_blocks_block_start_removal_marks_attached_attribute_as_changed() {
        let base_blocks = rust_review_blocks("#[inline]\npub fn retained() {}\n");
        let head_blocks = rust_review_blocks("pub fn retained() {}\n");
        assert_eq!(base_blocks.len(), 1);
        assert_eq!(head_blocks.len(), 1);

        let blocks = collect_diff_review_blocks_for_file(
            &base_blocks,
            &head_blocks,
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("#[inline]\n"),
                    vcs::DiffHunkLine::context("pub fn retained() {}\n"),
                ],
            }],
        );

        assert_eq!(
            blocks.len(),
            1,
            "expected one paired review unit: {blocks:?}"
        );
        assert_eq!(blocks[0].change_kind, BlockChangeKind::Changed);
        assert_eq!(
            blocks[0]
                .sides
                .base
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(base_blocks[0].hash.as_str())
        );
        assert_eq!(
            blocks[0]
                .sides
                .head
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(head_blocks[0].hash.as_str())
        );
        assert_eq!(blocks[0].display_block().hash, head_blocks[0].hash);
    }

    #[test]
    fn collect_diff_review_blocks_block_start_removal_marks_attached_doc_comment_as_changed() {
        let base_blocks = rust_review_blocks("/// Retained documentation.\npub fn retained() {}\n");
        let head_blocks = rust_review_blocks("pub fn retained() {}\n");
        assert_eq!(base_blocks.len(), 1);
        assert_eq!(head_blocks.len(), 1);

        let blocks = collect_diff_review_blocks_for_file(
            &base_blocks,
            &head_blocks,
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("/// Retained documentation.\n"),
                    vcs::DiffHunkLine::context("pub fn retained() {}\n"),
                ],
            }],
        );

        assert_eq!(
            blocks.len(),
            1,
            "expected one paired review unit: {blocks:?}"
        );
        assert_eq!(blocks[0].change_kind, BlockChangeKind::Changed);
        assert_eq!(
            blocks[0]
                .sides
                .base
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(base_blocks[0].hash.as_str())
        );
        assert_eq!(
            blocks[0]
                .sides
                .head
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(head_blocks[0].hash.as_str())
        );
        assert_eq!(blocks[0].display_block().hash, head_blocks[0].hash);
    }

    #[test]
    fn collect_diff_review_blocks_block_start_removal_does_not_attribute_predecessor_to_survivor() {
        let base_blocks = rust_review_blocks("pub fn removed() {}\npub fn retained() {}\n");
        let head_blocks = rust_review_blocks("pub fn retained() {}\n");
        assert_eq!(base_blocks.len(), 2);
        assert_eq!(head_blocks.len(), 1);

        let blocks = collect_diff_review_blocks_for_file(
            &base_blocks,
            &head_blocks,
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("pub fn removed() {}\n"),
                    vcs::DiffHunkLine::context("pub fn retained() {}\n"),
                ],
            }],
        );

        assert_eq!(
            blocks.len(),
            1,
            "unexpected survivor review unit: {blocks:?}"
        );
        assert_eq!(blocks[0].change_kind, BlockChangeKind::Deleted);
        assert_eq!(
            blocks[0]
                .sides
                .base
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(base_blocks[0].hash.as_str())
        );
        assert!(blocks[0].sides.head.is_none());
        assert!(
            blocks.iter().all(|block| {
                block
                    .sides
                    .head
                    .as_ref()
                    .is_none_or(|head| head.hash != head_blocks[0].hash)
            }),
            "retained block must not be paired or added: {blocks:?}"
        );
    }

    #[test]
    fn collect_diff_review_blocks_keeps_block_start_replacement_paired() {
        let base = make_diff_block(
            "base-retained",
            BlockKind::Function,
            0,
            1,
            "pub fn retained() {}\n",
        );
        let head = make_diff_block(
            "head-retained",
            BlockKind::Function,
            0,
            1,
            "fn retained() {}\n",
        );

        let blocks = collect_diff_review_blocks_for_file(
            std::slice::from_ref(&base),
            std::slice::from_ref(&head),
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("pub fn retained() {}\n"),
                    vcs::DiffHunkLine::added("fn retained() {}\n"),
                ],
            }],
        );

        assert_eq!(
            blocks.len(),
            1,
            "expected one paired review unit: {blocks:?}"
        );
        assert_eq!(blocks[0].change_kind, BlockChangeKind::Changed);
        assert_eq!(
            blocks[0]
                .sides
                .base
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(base.hash.as_str())
        );
        assert_eq!(
            blocks[0]
                .sides
                .head
                .as_ref()
                .map(|block| block.hash.as_str()),
            Some(head.hash.as_str())
        );
    }

    #[test]
    fn collect_diff_review_blocks_filters_block_start_whitespace_churn() {
        let base = make_diff_block("base-whitespace", BlockKind::Code, 0, 1, " \n");
        let head = make_diff_block("head-whitespace", BlockKind::Code, 0, 1, "\t\n");

        let blocks = collect_diff_review_blocks_for_file(
            std::slice::from_ref(&base),
            std::slice::from_ref(&head),
            &[vcs::DiffHunk {
                file_path: RepoPath::new("src/lib.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed(" \n"),
                    vcs::DiffHunkLine::added("\t\n"),
                ],
            }],
        );

        assert!(
            blocks.is_empty(),
            "whitespace churn must be absent: {blocks:?}"
        );
    }

    #[test]
    fn collect_diff_review_files_block_start_removal_scopes_ownership_by_target_source_path() {
        let head_blocks = rust_review_blocks("pub fn retained() {}\n");
        let base_a_blocks = rust_review_blocks("#[inline]\npub fn retained() {}\n");
        let base_b_blocks = rust_review_blocks("pub fn removed() {}\npub fn retained() {}\n");
        assert_eq!(head_blocks.len(), 1);
        assert_eq!(base_a_blocks.len(), 1);
        assert_eq!(base_b_blocks.len(), 2);

        let target_a = target_file_diff_input(
            "src/old-a.rs",
            "#[inline]\npub fn retained() {}\n",
            base_a_blocks.clone(),
            vcs::DiffHunk {
                file_path: RepoPath::new("src/dest.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("#[inline]\n"),
                    vcs::DiffHunkLine::context("pub fn retained() {}\n"),
                ],
            },
        );
        let target_b = target_file_diff_input(
            "src/old-b.rs",
            "pub fn removed() {}\npub fn retained() {}\n",
            base_b_blocks.clone(),
            vcs::DiffHunk {
                file_path: RepoPath::new("src/dest.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("pub fn removed() {}\n"),
                    vcs::DiffHunkLine::context("pub fn retained() {}\n"),
                ],
            },
        );

        let blocks = collect_diff_review_blocks_for_target_inputs(
            &[target_a.clone(), target_b.clone()],
            &head_blocks,
        );
        assert_eq!(
            blocks.len(),
            2,
            "expected one result per target: {blocks:?}"
        );
        assert!(blocks.iter().any(|block| {
            block.change_kind == BlockChangeKind::Changed
                && block
                    .sides
                    .base
                    .as_ref()
                    .is_some_and(|base| base.hash == base_a_blocks[0].hash)
                && block
                    .sides
                    .head
                    .as_ref()
                    .is_some_and(|head| head.hash == head_blocks[0].hash)
        }));
        assert!(blocks.iter().any(|block| {
            block.change_kind == BlockChangeKind::Deleted
                && block
                    .sides
                    .base
                    .as_ref()
                    .is_some_and(|base| base.hash == base_b_blocks[0].hash)
                && block.sides.head.is_none()
        }));
        let reversed =
            collect_diff_review_blocks_for_target_inputs(&[target_b, target_a], &head_blocks);
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.display_block().hash.clone())
                .collect::<Vec<_>>(),
            reversed
                .iter()
                .map(|block| block.display_block().hash.clone())
                .collect::<Vec<_>>(),
            "target processing order must not change final display ordering"
        );
    }

    #[test]
    fn collect_diff_review_files_block_start_removal_preserves_duplicate_target_boundaries() {
        let head_blocks = rust_review_blocks("pub fn retained() {}\n");
        let base_blocks = rust_review_blocks("#[inline]\npub fn retained() {}\n");
        let input = target_file_diff_input(
            "src/old.rs",
            "#[inline]\npub fn retained() {}\n",
            base_blocks.clone(),
            vcs::DiffHunk {
                file_path: RepoPath::new("src/dest.rs").unwrap(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    vcs::DiffHunkLine::removed("#[inline]\n"),
                    vcs::DiffHunkLine::context("pub fn retained() {}\n"),
                ],
            },
        );

        let per_target = collect_diff_review_blocks_for_file(
            input
                .base
                .as_ref()
                .map(|base| base.blocks.as_slice())
                .unwrap(),
            &head_blocks,
            input.file_diff.hunks(),
        );
        assert_eq!(per_target.len(), 1);
        assert_eq!(per_target[0].change_kind, BlockChangeKind::Changed);

        let blocks =
            collect_diff_review_blocks_for_target_inputs(&[input.clone(), input], &head_blocks);
        assert_eq!(
            blocks.len(),
            1,
            "duplicate inputs must survive mapping then deduplicate visibly: {blocks:?}"
        );
        assert_eq!(blocks[0].change_kind, BlockChangeKind::Changed);
        assert_eq!(
            blocks[0].sides.base.as_ref().map(|base| base.hash.as_str()),
            Some(base_blocks[0].hash.as_str())
        );
        assert_eq!(
            blocks[0].sides.head.as_ref().map(|head| head.hash.as_str()),
            Some(head_blocks[0].hash.as_str())
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

    #[test]
    fn collect_diff_review_files_traverses_and_inspects_once_per_diff_target() {
        let repo_root = temp_git_repo("review_target_first_batch");
        let source_dir = repo_root.join("src");
        fs::create_dir_all(&source_dir).unwrap();
        for index in 0..128 {
            fs::write(
                source_dir.join(format!("file_{index:03}.rs")),
                format!("pub fn file_{index:03}() {{ println!(\"before\"); }}\n"),
            )
            .unwrap();
        }
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "base"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        run_git(&repo_root, &["switch", "-c", "feature"]);
        for index in 0..128 {
            fs::write(
                source_dir.join(format!("file_{index:03}.rs")),
                format!("pub fn file_{index:03}() {{ println!(\"after\"); }}\n"),
            )
            .unwrap();
        }
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "change all files"]);

        let _guard = CurrentDirGuard::push(&repo_root);
        let query = resolve_review_request(
            ReviewRequest::Targets(vec![ReviewTarget::MainDiff, ReviewTarget::MainDiff]),
            BlockFilters::default(),
            ScanOptions::default(),
        )
        .unwrap();
        vcs::reset_file_diff_test_counters();

        let collected = collect_review(&query).unwrap();
        let paths = collected
            .summary
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 128);
        assert_eq!(paths.first(), Some(&"src/file_000.rs"));
        assert_eq!(paths.last(), Some(&"src/file_127.rs"));
        assert_eq!(vcs::file_diff_test_counters(), (2, 256));
    }

    #[test]
    fn target_diff_batches_preserve_order_duplicates_and_sources() {
        use crate::test_git::run_git_stdout;

        let repo_root = temp_git_repo("review_target_batch_order");
        let source_dir = repo_root.join("src");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("old_a.rs"),
            "pub fn retained_alpha() {}\npub fn retained_beta() {}\npub fn retained_gamma() {}\npub fn replaced_a() {}\n",
        )
        .unwrap();
        run_git(&repo_root, &["config", "diff.renames", "true"]);
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "base"]);
        run_git(&repo_root, &["branch", "-M", "main"]);
        run_git(&repo_root, &["switch", "-c", "feature"]);

        run_git(&repo_root, &["mv", "src/old_a.rs", "src/dest.rs"]);
        fs::write(
            source_dir.join("dest.rs"),
            "pub fn retained_alpha() {}\npub fn retained_beta() {}\npub fn retained_gamma() {}\npub fn first_marker() {}\n",
        )
        .unwrap();
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "first rename"]);
        let first = run_git_stdout(&repo_root, &["rev-parse", "HEAD"]);

        fs::remove_file(source_dir.join("dest.rs")).unwrap();
        fs::write(
            source_dir.join("old_b.rs"),
            "pub fn retained_alpha() {}\npub fn retained_beta() {}\npub fn retained_gamma() {}\npub fn replaced_b() {}\n",
        )
        .unwrap();
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "prepare second rename"]);
        run_git(&repo_root, &["mv", "src/old_b.rs", "src/dest.rs"]);
        fs::write(
            source_dir.join("dest.rs"),
            "pub fn retained_alpha() {}\npub fn retained_beta() {}\npub fn retained_gamma() {}\npub fn second_marker() {}\n",
        )
        .unwrap();
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-m", "second rename"]);
        let second = run_git_stdout(&repo_root, &["rev-parse", "HEAD"]);

        let repo = gix::open(&repo_root).unwrap();
        let first_target = ReviewDiffTarget::Revision(CommitId::new(first.trim()).unwrap());
        let second_target = ReviewDiffTarget::Revision(CommitId::new(second.trim()).unwrap());
        let targets = vec![first_target.clone(), second_target, first_target];
        let destination = RepoPath::new("src/dest.rs").unwrap();
        let mut batches =
            collect_target_diff_batches(&repo, &targets, std::slice::from_ref(&destination))
                .unwrap();
        let diffs = batches
            .iter_mut()
            .map(|batch| batch.diffs.take(&destination))
            .collect::<Vec<_>>();

        assert_eq!(
            diffs
                .iter()
                .map(|diff| diff.changed_path().source_location.as_str())
                .collect::<Vec<_>>(),
            vec!["src/old_a.rs", "src/old_b.rs", "src/old_a.rs"]
        );
        assert_eq!(
            diffs
                .into_iter()
                .map(|diff| {
                    diff.into_hunks()
                        .into_iter()
                        .flat_map(|hunk| hunk.lines)
                        .find(|line| matches!(line.kind, vcs::DiffLineKind::Added))
                        .map(|line| line.text)
                })
                .collect::<Vec<_>>(),
            vec![
                Some("pub fn first_marker() {}\n".to_string()),
                Some("pub fn second_marker() {}\n".to_string()),
                Some("pub fn first_marker() {}\n".to_string()),
            ]
        );
    }
}
