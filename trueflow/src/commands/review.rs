use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::config::{BlockFilters, load as load_config};
use crate::context::TrueflowContext;
use crate::path_utils;
use crate::policy::{should_skip_impl_by_default, should_skip_imports_by_default};
use crate::repo_path::RepoPath;
use crate::scanner::{self, ScanDiagnostic, ScanOptions};
use crate::store::{FileStore, ReviewCheck, ReviewStore, ReviewTargetRef};
use crate::sub_splitter;
use crate::tree;
use crate::vcs;
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::HashSet;
use std::fmt;
use tracing::info;

#[derive(Serialize)]
pub struct UnreviewedFile {
    pub path: RepoPath,
    pub language: Language,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RevisionSpec(String);

impl RevisionSpec {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(anyhow!("revision cannot be empty"));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RevisionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RevisionRangeSpec {
    pub start: RevisionSpec,
    pub end: RevisionSpec,
}

impl RevisionRangeSpec {
    pub fn new(start: impl Into<String>, end: impl Into<String>) -> Result<Self> {
        Ok(Self {
            start: RevisionSpec::new(start)?,
            end: RevisionSpec::new(end)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    DirtyWorktree,
    MainDiff,
    File(RepoPath),
    Revision(RevisionSpec),
    RevisionRange(RevisionRangeSpec),
}

impl ReviewTarget {
    fn from_cli(raw: &str) -> Result<Self> {
        if let Some(rest) = raw.strip_prefix("file:") {
            return Ok(Self::File(RepoPath::new(rest)?));
        }
        if let Some(rest) = raw.strip_prefix("rev:") {
            if let Some((start, end)) = rest.split_once("..") {
                return Ok(Self::RevisionRange(RevisionRangeSpec::new(start, end)?));
            }
            return Ok(Self::Revision(RevisionSpec::new(rest)?));
        }
        Err(anyhow!("Unknown review target: {raw}"))
    }

    fn historical_content_revision(&self) -> Option<&RevisionSpec> {
        match self {
            Self::Revision(revision) => Some(revision),
            Self::RevisionRange(range) => Some(&range.end),
            Self::DirtyWorktree | Self::MainDiff | Self::File(_) => None,
        }
    }

    fn is_worktree_content_target(&self) -> bool {
        matches!(self, Self::DirtyWorktree | Self::MainDiff)
    }

    fn diff_target(&self) -> Option<ReviewDiffTarget> {
        match self {
            Self::MainDiff => Some(ReviewDiffTarget::MainDiff),
            Self::Revision(revision) => Some(ReviewDiffTarget::Revision(revision.clone())),
            Self::RevisionRange(range) => Some(ReviewDiffTarget::RevisionRange(range.clone())),
            Self::DirtyWorktree | Self::File(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRequest {
    AllFiles,
    Targets(Vec<ReviewTarget>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewContentSource {
    Workdir,
    Revision(RevisionSpec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPathSelection {
    All,
    Specific(HashSet<RepoPath>),
}

impl ReviewPathSelection {
    fn includes(&self, file_path: &RepoPath, workdir_prefix: Option<&str>) -> Result<bool> {
        match self {
            Self::All => Ok(true),
            Self::Specific(targets) => {
                if targets.contains(file_path) {
                    return Ok(true);
                }
                if let Some(prefix) = workdir_prefix {
                    let repo_path = RepoPath::new(format!("{prefix}/{file_path}"))?;
                    return Ok(targets.contains(&repo_path));
                }
                Ok(false)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiffTarget {
    MainDiff,
    Revision(RevisionSpec),
    RevisionRange(RevisionRangeSpec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiffSelection {
    None,
    Targets(Vec<ReviewDiffTarget>),
}

impl ReviewDiffSelection {
    fn targets(&self) -> Option<&[ReviewDiffTarget]> {
        match self {
            Self::None => None,
            Self::Targets(targets) => Some(targets),
        }
    }

    fn requires_repo(&self) -> bool {
        matches!(self, Self::Targets(_))
    }
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

pub struct CollectedReview {
    pub summary: ReviewSummary,
    pub tree: tree::Tree,
    pub unreviewed_block_nodes: HashSet<tree::TreeNodeId>,
}

pub fn parse_review_request(all: bool, values: &[String]) -> Result<ReviewRequest> {
    if all {
        if !values.is_empty() {
            return Err(anyhow!(
                "Explicit review targets cannot be combined with --all"
            ));
        }
        return Ok(ReviewRequest::AllFiles);
    }

    if values.is_empty() {
        return Ok(ReviewRequest::Targets(vec![ReviewTarget::DirtyWorktree]));
    }

    let targets = values
        .iter()
        .map(|raw| ReviewTarget::from_cli(raw))
        .collect::<Result<Vec<_>>>()?;
    Ok(ReviewRequest::Targets(targets))
}

pub fn resolve_review_request(
    request: ReviewRequest,
    filters: BlockFilters,
    scan_options: ScanOptions,
) -> Result<ResolvedReviewQuery> {
    let (content_source, path_selection, diff_selection) = match request {
        ReviewRequest::AllFiles => (
            ReviewContentSource::Workdir,
            ReviewPathSelection::All,
            ReviewDiffSelection::None,
        ),
        ReviewRequest::Targets(targets) if targets.is_empty() => {
            return Err(anyhow!(
                "review target list cannot be empty; use AllFiles or an explicit target"
            ));
        }
        ReviewRequest::Targets(targets) => (
            resolve_review_content_source(&targets)?,
            resolve_review_path_selection(&targets)?,
            resolve_review_diff_selection(&targets),
        ),
    };

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
    let review_index = database.latest_index(Some(&review_check));
    let approved_targets = review_index.approved_targets();

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
            let paths = match &query.path_selection {
                ReviewPathSelection::Specific(paths) => paths,
                ReviewPathSelection::All => {
                    return Err(anyhow!(
                        "historical review targets must resolve to explicit paths"
                    ));
                }
            };
            (
                vcs::file_states_for_paths_in_revision(
                    repo,
                    revision.as_str(),
                    paths,
                    workdir_prefix.as_deref(),
                )?,
                Vec::new(),
            )
        }
    };
    info!("scanned {} files", files.len());
    let tree = tree::build_tree_from_files(&files);

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
            if should_skip_impl_by_default(&block, &query.filters) {
                continue;
            }
            if let Some(hunks) = file_diff_hunks.as_deref()
                && vcs::block_has_changed_lines_in_diff(&block, hunks)
                    != vcs::BlockDiffChangeKind::ReviewableChanges
            {
                continue;
            }
            reviewable_blocks.push(block);
        }
        total_blocks += reviewable_blocks.len();

        if review_index.is_approved(&ReviewTargetRef::File {
            hash: file.tree_hash.clone(),
        }) {
            continue;
        }

        let mut unreviewed_blocks = Vec::new();
        for block in reviewable_blocks {
            let node_id = tree.find_block_node(&file.path, &block);
            if let Some(node_id) = node_id
                && tree.is_node_covered(node_id, &approved_targets, workdir_prefix.as_deref())
            {
                continue;
            }

            if review_index.is_block_approved(
                &block.hash,
                &file.path,
                block.start_line,
                workdir_prefix.as_deref(),
            ) {
                continue;
            }

            if review_index
                .block_verdict_for(
                    &block.hash,
                    &file.path,
                    block.start_line,
                    workdir_prefix.as_deref(),
                )
                .is_none()
                && let Ok(sub_blocks) = sub_splitter::split(&block, language)
                && !sub_blocks.is_empty()
            {
                let all_approved = sub_blocks.iter().all(|sb| {
                    if !query.filters.allows_subblock(sb.kind) {
                        return true;
                    }
                    review_index.is_block_approved(
                        &sb.hash,
                        &file.path,
                        sb.start_line,
                        workdir_prefix.as_deref(),
                    )
                });

                if all_approved {
                    continue;
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
    })
}

pub fn collect_review_summary(query: &ResolvedReviewQuery) -> Result<ReviewSummary> {
    let collected = collect_review(query)?;
    Ok(collected.summary)
}

fn resolve_review_content_source(targets: &[ReviewTarget]) -> Result<ReviewContentSource> {
    let mut revision = None;
    let mut saw_worktree_target = false;

    for target in targets {
        if target.is_worktree_content_target() {
            if revision.is_some() {
                return Err(anyhow!(
                    "Historical targets cannot be mixed with worktree-based targets"
                ));
            }
            saw_worktree_target = true;
            continue;
        }

        let Some(candidate) = target.historical_content_revision() else {
            continue;
        };

        if saw_worktree_target {
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

    Ok(revision
        .map(ReviewContentSource::Revision)
        .unwrap_or(ReviewContentSource::Workdir))
}

fn resolve_review_path_selection(targets: &[ReviewTarget]) -> Result<ReviewPathSelection> {
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
                paths.insert(path.clone());
            }
            ReviewTarget::Revision(revision) => {
                paths.extend(vcs::files_changed_in_revision(revision.as_str())?);
            }
            ReviewTarget::RevisionRange(range) => {
                paths.extend(vcs::files_changed_in_range(
                    range.start.as_str(),
                    range.end.as_str(),
                )?);
            }
        }
    }

    Ok(ReviewPathSelection::Specific(paths))
}

fn resolve_review_diff_selection(targets: &[ReviewTarget]) -> ReviewDiffSelection {
    let mut diff_targets = Vec::new();

    for target in targets {
        let Some(diff_target) = target.diff_target() else {
            return ReviewDiffSelection::None;
        };
        diff_targets.push(diff_target);
    }

    if diff_targets.is_empty() {
        ReviewDiffSelection::None
    } else {
        ReviewDiffSelection::Targets(diff_targets)
    }
}

fn diff_hunks_for_file_targets(
    repo: &gix::Repository,
    targets: &[ReviewDiffTarget],
    file_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<Vec<vcs::DiffHunk>> {
    let repo_relative_path = repo_relative_path_for_diff(file_path, workdir_prefix)?;
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

pub fn run(
    _context: &TrueflowContext,
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
    let scan_options = config.scan.resolve_options()?;
    let request = parse_review_request(all, target)?;
    let query = resolve_review_request(request, filters, scan_options)?;
    let summary = collect_review_summary(&query)?;
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
    fn parse_review_request_all_rejects_explicit_targets() {
        let err = parse_review_request(true, &["file:src/lib.rs".to_string()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("Explicit review targets cannot be combined with --all")
        );
    }

    #[test]
    fn parse_review_request_defaults_to_dirty_worktree() {
        let request = parse_review_request(false, &[])
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
                "file:src/lib.rs".to_string(),
                "rev:abc1234..def5678".to_string(),
            ],
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

    #[test]
    fn resolve_review_request_makes_historical_content_and_diff_explicit() {
        let targets = vec![ReviewTarget::RevisionRange(
            RevisionRangeSpec::new("abc1234", "def5678").unwrap(),
        )];

        let content_source = resolve_review_content_source(&targets)
            .unwrap_or_else(|error| panic!("expected historical content source: {error}"));
        let diff_selection = resolve_review_diff_selection(&targets);

        assert_eq!(
            content_source,
            ReviewContentSource::Revision(RevisionSpec::new("def5678").unwrap())
        );
        assert!(matches!(diff_selection, ReviewDiffSelection::Targets(_)));
    }
}
