use crate::github::PullRequestRef;
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::store::CommitId;
use crate::vcs::{self, ChangedPath};
use anyhow::{Result, anyhow};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RevisionExpr(String);

impl RevisionExpr {
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

impl fmt::Display for RevisionExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which commit a revision range's comparison starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RevisionRangeBase {
    /// `A..B`: compare `A`'s tree with `B`'s.
    Start,
    /// `A...B`: compare from the merge base of `A` and `B`, like
    /// `git diff A...B`, so only `B`'s side of the fork is reviewed.
    MergeBase,
}

impl RevisionRangeBase {
    fn separator(self) -> &'static str {
        match self {
            Self::Start => "..",
            Self::MergeBase => "...",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RevisionRangeExpr {
    pub start: RevisionExpr,
    pub end: RevisionExpr,
    pub base: RevisionRangeBase,
}

impl RevisionRangeExpr {
    pub fn new(start: impl Into<String>, end: impl Into<String>) -> Result<Self> {
        Self::with_base(start, end, RevisionRangeBase::Start)
    }

    fn with_base(
        start: impl Into<String>,
        end: impl Into<String>,
        base: RevisionRangeBase,
    ) -> Result<Self> {
        Ok(Self {
            start: RevisionExpr::new(start)?,
            end: RevisionExpr::new(end)?,
            base,
        })
    }

    /// Parses git's `A..B` and `A...B`; like git, an omitted side means `HEAD`.
    fn parse(raw: &str) -> Result<Option<Self>> {
        // `...` first: `A...B` also contains `..`.
        let Some((base, (start, end))) = [RevisionRangeBase::MergeBase, RevisionRangeBase::Start]
            .into_iter()
            .find_map(|base| raw.split_once(base.separator()).map(|sides| (base, sides)))
        else {
            return Ok(None);
        };
        let or_head = |side: &str| match side.trim() {
            "" => "HEAD".to_string(),
            side => side.to_string(),
        };
        Self::with_base(or_head(start), or_head(end), base).map(Some)
    }
}

impl fmt::Display for RevisionRangeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}{}", self.start, self.base.separator(), self.end)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    DirtyWorktree,
    MainDiff,
    File(RepoPath),
    /// Scope the review to every reviewable file under `path` (inclusive).
    /// Uses the workdir as the content source, same as `File`.
    Dir(RepoPath),
    Revision(RevisionExpr),
    RevisionRange(RevisionRangeExpr),
    PullRequest(PullRequestRef),
}

impl ReviewTarget {
    pub fn from_cli(raw: &str) -> Result<Self> {
        match raw {
            "dirty" => return Ok(Self::DirtyWorktree),
            "main" => return Ok(Self::MainDiff),
            _ => {}
        }
        if let Some(rest) = raw.strip_prefix("file:") {
            return Ok(Self::File(RepoPath::new(rest)?));
        }
        if let Some(rest) = raw.strip_prefix("dir:") {
            return Ok(Self::Dir(RepoPath::new(rest)?));
        }
        if let Some(rest) = raw.strip_prefix("rev:") {
            return Ok(match RevisionRangeExpr::parse(rest)? {
                Some(range) => Self::RevisionRange(range),
                None => Self::Revision(RevisionExpr::new(rest)?),
            });
        }
        if raw.starts_with("pr:") || raw.starts_with("http://") || raw.starts_with("https://") {
            return Ok(Self::PullRequest(PullRequestRef::from_cli(raw)?));
        }
        Err(anyhow!("Unknown review target: {raw}"))
    }

    pub fn pull_request(&self) -> Option<&PullRequestRef> {
        match self {
            Self::PullRequest(pull_request) => Some(pull_request),
            Self::DirtyWorktree
            | Self::MainDiff
            | Self::File(_)
            | Self::Dir(_)
            | Self::Revision(_)
            | Self::RevisionRange(_) => None,
        }
    }
}

impl FromStr for ReviewTarget {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_cli(raw)
    }
}

pub fn extract_pull_request_target(targets: &[ReviewTarget]) -> Result<Option<&PullRequestRef>> {
    let mut pull_request = None;

    for target in targets {
        let Some(candidate) = target.pull_request() else {
            continue;
        };

        if pull_request.is_some() || targets.len() != 1 {
            return Err(anyhow!(
                "Pull request targets cannot be combined with other review targets"
            ));
        }

        pull_request = Some(candidate);
    }

    Ok(pull_request)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewContentSource {
    Workdir,
    Revision(CommitId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPathSelection {
    All,
    Empty,
    /// Explicit files plus one or more directory prefixes. A changed pair is
    /// eligible when its destination is requested and either endpoint matches
    /// the explicit scope; callers still receive only the destination.
    Scoped {
        files: HashSet<RepoPath>,
        dirs: Vec<RepoPath>,
        changed: Option<HashSet<ChangedPath>>,
    },
}

impl ReviewPathSelection {
    pub fn includes(&self, file_path: &RepoPath) -> bool {
        match self {
            Self::All => true,
            Self::Empty => false,
            Self::Scoped {
                files,
                dirs,
                changed,
            } => match changed {
                Some(changed_paths) => changed_paths.iter().any(|changed_path| {
                    changed_path.location == *file_path
                        && (files.is_empty() && dirs.is_empty()
                            || path_matches_explicit_selection(
                                files,
                                dirs,
                                &changed_path.source_location,
                            )
                            || path_matches_explicit_selection(files, dirs, &changed_path.location))
                }),
                None => {
                    files.is_empty() && dirs.is_empty()
                        || path_matches_explicit_selection(files, dirs, file_path)
                }
            },
        }
    }
}

fn path_matches_explicit_selection(
    files: &HashSet<RepoPath>,
    dirs: &[RepoPath],
    file_path: &RepoPath,
) -> bool {
    path_matches_specific_selection(files, file_path) || path_matches_dir_selection(dirs, file_path)
}

fn path_matches_specific_selection(targets: &HashSet<RepoPath>, file_path: &RepoPath) -> bool {
    targets.contains(file_path)
}

fn path_matches_dir_selection(dirs: &[RepoPath], file_path: &RepoPath) -> bool {
    dirs.iter().any(|dir| file_path.is_under(dir))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitRange {
    pub start: CommitId,
    pub end: CommitId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiffTarget {
    MainDiff(CommitRange),
    Revision(CommitId),
    RevisionRange(CommitRange),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiffSelection {
    None,
    Targets(Vec<ReviewDiffTarget>),
}

impl ReviewDiffSelection {
    pub(crate) fn targets(&self) -> Option<&[ReviewDiffTarget]> {
        match self {
            Self::None => None,
            Self::Targets(targets) => Some(targets),
        }
    }

    pub(crate) fn requires_repo(&self) -> bool {
        matches!(self, Self::Targets(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTargets {
    pub content_source: ReviewContentSource,
    pub diff_selection: ReviewDiffSelection,
    files: HashSet<RepoPath>,
    dirs: Vec<RepoPath>,
    changed: HashSet<ChangedPath>,
    changed_target_requested: bool,
}

impl ResolvedTargets {
    pub(crate) fn new(
        content_source: ReviewContentSource,
        diff_selection: ReviewDiffSelection,
        files: HashSet<RepoPath>,
        dirs: Vec<RepoPath>,
        changed: HashSet<ChangedPath>,
    ) -> Self {
        Self {
            content_source,
            diff_selection,
            files,
            dirs,
            changed_target_requested: !changed.is_empty(),
            changed,
        }
    }

    pub fn explicit_selection(&self) -> Option<ReviewPathSelection> {
        if self.files.is_empty() && self.dirs.is_empty() {
            None
        } else {
            Some(ReviewPathSelection::Scoped {
                files: self.files.clone(),
                dirs: self.dirs.clone(),
                changed: None,
            })
        }
    }

    pub fn changed_selection(&self) -> Option<ReviewPathSelection> {
        if self.changed.is_empty() && !self.changed_target_requested {
            None
        } else {
            Some(ReviewPathSelection::Scoped {
                files: HashSet::new(),
                dirs: Vec::new(),
                changed: Some(self.changed.clone()),
            })
        }
    }

    pub fn path_selection(&self) -> ReviewPathSelection {
        if self.files.is_empty() && self.dirs.is_empty() && self.changed.is_empty() {
            if self.changed_target_requested {
                ReviewPathSelection::Empty
            } else {
                ReviewPathSelection::All
            }
        } else {
            ReviewPathSelection::Scoped {
                files: self.files.clone(),
                dirs: self.dirs.clone(),
                changed: self
                    .changed_target_requested
                    .then_some(self.changed.clone()),
            }
        }
    }
}

pub fn resolve_targets(targets: &[ReviewTarget]) -> Result<ResolvedTargets> {
    let repo_cache = RefCell::new(None);
    resolve_targets_with_changed(
        targets,
        |revision| {
            with_workdir_repo(&repo_cache, |repo| {
                vcs::resolve_commit_id_in_repo(repo, revision.as_str())
            })
        },
        |left, right| {
            with_workdir_repo(&repo_cache, |repo| {
                vcs::merge_base_in_repo(repo, left.as_str(), right.as_str())
            })
        },
        || with_workdir_repo(&repo_cache, vcs::dirty_changed_paths),
        || with_workdir_repo(&repo_cache, vcs::resolved_main_diff),
        |revision| {
            with_workdir_repo(&repo_cache, |repo| {
                vcs::files_changed_in_revision_in_repo(repo, revision)
            })
        },
        |start, end| {
            with_workdir_repo(&repo_cache, |repo| {
                vcs::files_changed_in_range_in_repo(repo, start, end)
            })
        },
    )
}

fn with_workdir_repo<T>(
    repo_cache: &RefCell<Option<gix::Repository>>,
    action: impl FnOnce(&gix::Repository) -> Result<T>,
) -> Result<T> {
    if repo_cache.borrow().is_none() {
        *repo_cache.borrow_mut() = Some(vcs::repo_from_workdir()?);
    }

    let repo = repo_cache.borrow();
    let Some(repo) = repo.as_ref() else {
        return Err(anyhow!("workdir repository cache was not initialized"));
    };
    action(repo)
}

pub(crate) fn resolve_targets_with<ResolveFn, MergeBaseFn, DirtyFn, MainFn, RevisionFn, RangeFn>(
    targets: &[ReviewTarget],
    resolve_revision: ResolveFn,
    merge_base: MergeBaseFn,
    dirty_files: DirtyFn,
    main_diff_files: MainFn,
    revision_files: RevisionFn,
    range_files: RangeFn,
) -> Result<ResolvedTargets>
where
    ResolveFn: Fn(&RevisionExpr) -> Result<CommitId>,
    MergeBaseFn: Fn(&CommitId, &CommitId) -> Result<CommitId>,
    DirtyFn: Fn() -> Result<HashSet<RepoPath>>,
    MainFn: Fn() -> Result<(CommitRange, HashSet<ChangedPath>)>,
    RevisionFn: Fn(&str) -> Result<HashSet<ChangedPath>>,
    RangeFn: Fn(&str, &str) -> Result<HashSet<ChangedPath>>,
{
    resolve_targets_with_changed(
        targets,
        resolve_revision,
        merge_base,
        || {
            dirty_files().map(|paths| {
                paths
                    .into_iter()
                    .map(ChangedPath::identity)
                    .collect::<HashSet<_>>()
            })
        },
        main_diff_files,
        revision_files,
        range_files,
    )
}

fn resolve_targets_with_changed<ResolveFn, MergeBaseFn, DirtyFn, MainFn, RevisionFn, RangeFn>(
    targets: &[ReviewTarget],
    resolve_revision: ResolveFn,
    merge_base: MergeBaseFn,
    dirty_files: DirtyFn,
    main_diff_files: MainFn,
    revision_files: RevisionFn,
    range_files: RangeFn,
) -> Result<ResolvedTargets>
where
    ResolveFn: Fn(&RevisionExpr) -> Result<CommitId>,
    MergeBaseFn: Fn(&CommitId, &CommitId) -> Result<CommitId>,
    DirtyFn: Fn() -> Result<HashSet<ChangedPath>>,
    MainFn: Fn() -> Result<(CommitRange, HashSet<ChangedPath>)>,
    RevisionFn: Fn(&str) -> Result<HashSet<ChangedPath>>,
    RangeFn: Fn(&str, &str) -> Result<HashSet<ChangedPath>>,
{
    reject_mixed_content_source_targets(targets)?;
    let main_diff = targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::MainDiff))
        .then(main_diff_files)
        .transpose()?;
    let resolved_targets = resolve_target_exprs(
        targets,
        &resolve_revision,
        &merge_base,
        main_diff.as_ref().map(|(range, _)| range),
    )?;
    let content_source = resolve_content_source(&resolved_targets)?;
    let diff_selection = resolve_diff_selection(&resolved_targets);
    let mut files = HashSet::new();
    let mut dirs = Vec::new();
    let mut changed = HashSet::new();
    let mut changed_target_requested = false;

    for target in resolved_targets {
        match target {
            ResolvedReviewTarget::DirtyWorktree => {
                changed_target_requested = true;
                changed.extend(dirty_files()?);
            }
            ResolvedReviewTarget::MainDiff(_) => {
                changed_target_requested = true;
                changed.extend(
                    main_diff
                        .iter()
                        .flat_map(|(_, paths)| paths.iter().cloned()),
                );
            }
            ResolvedReviewTarget::File(path) => {
                files.insert(path);
            }
            ResolvedReviewTarget::Dir(path) => {
                if !dirs.contains(&path) {
                    dirs.push(path);
                }
            }
            ResolvedReviewTarget::Revision(revision) => {
                changed_target_requested = true;
                changed.extend(revision_files(revision.as_str())?);
            }
            ResolvedReviewTarget::RevisionRange(range) => {
                changed_target_requested = true;
                changed.extend(range_files(range.start.as_str(), range.end.as_str())?);
            }
        }
    }

    Ok(ResolvedTargets {
        content_source,
        diff_selection,
        files,
        dirs,
        changed,
        changed_target_requested,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedReviewTarget {
    DirtyWorktree,
    MainDiff(CommitRange),
    File(RepoPath),
    Dir(RepoPath),
    Revision(CommitId),
    RevisionRange(CommitRange),
}

impl ResolvedReviewTarget {
    fn historical_content_revision(&self) -> Option<&CommitId> {
        match self {
            Self::Revision(revision) => Some(revision),
            Self::MainDiff(range) | Self::RevisionRange(range) => Some(&range.end),
            Self::DirtyWorktree | Self::File(_) | Self::Dir(_) => None,
        }
    }

    fn is_worktree_content_target(&self) -> bool {
        matches!(self, Self::DirtyWorktree)
    }

    fn diff_target(&self) -> Option<ReviewDiffTarget> {
        match self {
            Self::MainDiff(range) => Some(ReviewDiffTarget::MainDiff(range.clone())),
            Self::Revision(revision) => Some(ReviewDiffTarget::Revision(revision.clone())),
            Self::RevisionRange(range) => Some(ReviewDiffTarget::RevisionRange(range.clone())),
            Self::DirtyWorktree | Self::File(_) | Self::Dir(_) => None,
        }
    }
}

fn reject_mixed_content_source_targets(targets: &[ReviewTarget]) -> Result<()> {
    let saw_worktree = targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::DirtyWorktree));
    let saw_historical = targets.iter().any(|target| {
        matches!(
            target,
            ReviewTarget::MainDiff | ReviewTarget::Revision(_) | ReviewTarget::RevisionRange(_)
        )
    });

    if saw_worktree && saw_historical {
        return Err(anyhow!(
            "Historical targets cannot be mixed with worktree-based targets"
        ));
    }

    Ok(())
}

fn resolve_target_exprs<ResolveFn, MergeBaseFn>(
    targets: &[ReviewTarget],
    resolve_revision: &ResolveFn,
    merge_base: &MergeBaseFn,
    main_diff: Option<&CommitRange>,
) -> Result<Vec<ResolvedReviewTarget>>
where
    ResolveFn: Fn(&RevisionExpr) -> Result<CommitId>,
    MergeBaseFn: Fn(&CommitId, &CommitId) -> Result<CommitId>,
{
    let mut revision_cache = HashMap::<RevisionExpr, CommitId>::new();
    let mut resolved = Vec::with_capacity(targets.len());

    for target in targets {
        let target = match target {
            ReviewTarget::DirtyWorktree => ResolvedReviewTarget::DirtyWorktree,
            ReviewTarget::MainDiff => ResolvedReviewTarget::MainDiff(
                main_diff
                    .cloned()
                    .ok_or_else(|| anyhow!("main comparison was not resolved"))?,
            ),
            ReviewTarget::File(path) => ResolvedReviewTarget::File(path.clone()),
            ReviewTarget::Dir(path) => ResolvedReviewTarget::Dir(path.clone()),
            ReviewTarget::Revision(revision) => ResolvedReviewTarget::Revision(
                resolve_revision_expr(revision, resolve_revision, &mut revision_cache)?,
            ),
            ReviewTarget::RevisionRange(range) => {
                let start =
                    resolve_revision_expr(&range.start, resolve_revision, &mut revision_cache)?;
                let end = resolve_revision_expr(&range.end, resolve_revision, &mut revision_cache)?;
                let start = match range.base {
                    RevisionRangeBase::Start => start,
                    RevisionRangeBase::MergeBase => merge_base(&start, &end)?,
                };
                ResolvedReviewTarget::RevisionRange(CommitRange { start, end })
            }
            ReviewTarget::PullRequest(_) => {
                return Err(anyhow!(
                    "Pull request targets require command-specific handling"
                ));
            }
        };
        resolved.push(target);
    }

    Ok(resolved)
}

fn resolve_revision_expr<ResolveFn>(
    revision: &RevisionExpr,
    resolve_revision: &ResolveFn,
    revision_cache: &mut HashMap<RevisionExpr, CommitId>,
) -> Result<CommitId>
where
    ResolveFn: Fn(&RevisionExpr) -> Result<CommitId>,
{
    if let Some(commit) = revision_cache.get(revision) {
        return Ok(commit.clone());
    }

    let commit = resolve_revision(revision)?;
    revision_cache.insert(revision.clone(), commit.clone());
    Ok(commit)
}

fn resolve_content_source(targets: &[ResolvedReviewTarget]) -> Result<ReviewContentSource> {
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

fn resolve_diff_selection(targets: &[ResolvedReviewTarget]) -> ReviewDiffSelection {
    let diff_targets = targets
        .iter()
        .filter_map(ResolvedReviewTarget::diff_target)
        .collect::<Vec<_>>();

    if diff_targets.is_empty() {
        ReviewDiffSelection::None
    } else {
        ReviewDiffSelection::Targets(diff_targets)
    }
}

pub fn workdir_prefix_from_git_root() -> Option<String> {
    let repo_root = vcs::git_root_from_workdir().ok().flatten()?;
    path_utils::current_workdir_prefix_for_repo_root(&repo_root)
}

#[cfg(test)]
mod tests {
    use super::{
        ResolvedTargets, ReviewContentSource, ReviewDiffSelection, ReviewDiffTarget,
        ReviewPathSelection, ReviewTarget, RevisionExpr, RevisionRangeBase, RevisionRangeExpr,
        extract_pull_request_target, resolve_targets_with,
    };
    use crate::github::PullRequestRef;
    use crate::repo_path::RepoPath;
    use crate::store::CommitId;
    use crate::vcs::ChangedPath;
    use std::collections::HashSet;

    fn empty_main_diff() -> anyhow::Result<(super::CommitRange, HashSet<ChangedPath>)> {
        Ok((
            super::CommitRange {
                start: CommitId::new("aaaaaaa")?,
                end: CommitId::new("bbbbbbb")?,
            },
            HashSet::new(),
        ))
    }

    fn unexpected_merge_base(left: &CommitId, right: &CommitId) -> anyhow::Result<CommitId> {
        panic!("unexpected merge-base lookup for {left} and {right}")
    }

    #[test]
    fn revision_ranges_parse_git_dot_syntax() {
        let range = |start: &str, end: &str, base| {
            ReviewTarget::RevisionRange(RevisionRangeExpr::with_base(start, end, base).unwrap())
        };
        for (raw, expected) in [
            (
                "rev:main..HEAD",
                range("main", "HEAD", RevisionRangeBase::Start),
            ),
            (
                "rev:main...HEAD",
                range("main", "HEAD", RevisionRangeBase::MergeBase),
            ),
            (
                "rev:HEAD~3..",
                range("HEAD~3", "HEAD", RevisionRangeBase::Start),
            ),
            (
                "rev:..feature",
                range("HEAD", "feature", RevisionRangeBase::Start),
            ),
            (
                "rev:main...",
                range("main", "HEAD", RevisionRangeBase::MergeBase),
            ),
            (
                "rev:...",
                range("HEAD", "HEAD", RevisionRangeBase::MergeBase),
            ),
        ] {
            assert_eq!(ReviewTarget::from_cli(raw).unwrap(), expected, "{raw}");
        }
        assert_eq!(
            ReviewTarget::from_cli("rev:HEAD~2").unwrap(),
            ReviewTarget::Revision(RevisionExpr::new("HEAD~2").unwrap())
        );
    }

    #[test]
    fn revision_range_display_keeps_its_separator() {
        let two_dot = RevisionRangeExpr::new("main", "HEAD").unwrap();
        let three_dot =
            RevisionRangeExpr::with_base("main", "HEAD", RevisionRangeBase::MergeBase).unwrap();
        assert_eq!(two_dot.to_string(), "main..HEAD");
        assert_eq!(three_dot.to_string(), "main...HEAD");
    }

    #[test]
    fn merge_base_range_starts_at_the_merge_base() {
        let resolved = resolve_targets_with(
            &[ReviewTarget::from_cli("rev:aaaaaaa...bbbbbbb").unwrap()],
            |revision| CommitId::new(revision.as_str()),
            |left, right| {
                assert_eq!((left.as_str(), right.as_str()), ("aaaaaaa", "bbbbbbb"));
                CommitId::new("ccccccc")
            },
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |start, end| {
                assert_eq!((start, end), ("ccccccc", "bbbbbbb"));
                Ok(HashSet::new())
            },
        )
        .unwrap();

        assert_eq!(
            resolved.diff_selection,
            ReviewDiffSelection::Targets(vec![ReviewDiffTarget::RevisionRange(
                super::CommitRange {
                    start: CommitId::new("ccccccc").unwrap(),
                    end: CommitId::new("bbbbbbb").unwrap(),
                }
            )])
        );
    }

    #[test]
    fn dir_target_parses() {
        let target = ReviewTarget::from_cli("dir:website")
            .unwrap_or_else(|error| panic!("dir target should parse: {error}"));
        assert_eq!(
            target,
            ReviewTarget::Dir(
                RepoPath::new("website").unwrap_or_else(|error| panic!("valid repo path: {error}")),
            )
        );
    }

    #[test]
    fn dir_target_rejects_absolute_path() {
        assert!(ReviewTarget::from_cli("dir:/tmp/absolute").is_err());
    }

    #[test]
    fn pull_request_target_parses_short_form() {
        assert_eq!(
            ReviewTarget::from_cli("pr:11").unwrap(),
            ReviewTarget::PullRequest(PullRequestRef::Number { number: 11 })
        );
    }

    #[test]
    fn pull_request_target_parses_full_url() {
        assert_eq!(
            ReviewTarget::from_cli("https://github.com/jmqd/trueflow/pull/11").unwrap(),
            ReviewTarget::PullRequest(PullRequestRef::HostedRepository {
                host: "github.com".to_string(),
                owner: "jmqd".to_string(),
                repo: "trueflow".to_string(),
                number: 11,
            })
        );
    }

    #[test]
    fn extract_pull_request_target_rejects_mixed_targets() {
        let err = extract_pull_request_target(&[
            ReviewTarget::PullRequest(PullRequestRef::Number { number: 11 }),
            ReviewTarget::Dir(RepoPath::new("src").unwrap()),
        ])
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Pull request targets cannot be combined with other review targets")
        );
    }

    #[test]
    fn dir_target_is_workdir_content() {
        let target = ReviewTarget::Dir(RepoPath::new("website").unwrap());
        let resolved = resolve_targets_with(
            &[target],
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_or_else(|error| panic!("expected resolved dir target: {error}"));
        assert_eq!(resolved.content_source, ReviewContentSource::Workdir);
        assert_eq!(resolved.diff_selection, ReviewDiffSelection::None);
    }

    #[test]
    fn scoped_selection_includes_files_under_dir() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: None,
        };
        assert!(selection.includes(&RepoPath::new("website/index.html").unwrap()));
        assert!(selection.includes(&RepoPath::new("website/a/b/c.js").unwrap()));
        assert!(selection.includes(&RepoPath::new("website").unwrap()));
    }

    #[test]
    fn scoped_selection_excludes_files_outside_dir() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: None,
        };
        assert!(!selection.includes(&RepoPath::new("docs/intro.md").unwrap()));
        assert!(!selection.includes(&RepoPath::new("website-next/index.html").unwrap()));
    }

    #[test]
    fn scoped_selection_still_checks_explicit_files() {
        let explicit = RepoPath::new("README.md").unwrap();
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::from([explicit.clone()]),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: None,
        };
        assert!(selection.includes(&explicit));
    }

    #[test]
    fn scoped_selection_with_changed_paths_requires_dir_intersection() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: Some(HashSet::from([ChangedPath::identity(
                RepoPath::new("website/index.html").unwrap(),
            )])),
        };
        assert!(selection.includes(&RepoPath::new("website/index.html").unwrap()));
        assert!(!selection.includes(&RepoPath::new("website/other.html").unwrap()));
        assert!(!selection.includes(&RepoPath::new("docs/index.html").unwrap()));
    }

    #[test]
    fn scoped_selection_matches_repo_relative_paths_directly() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("src/nested").unwrap()],
            changed: Some(HashSet::from([ChangedPath::identity(
                RepoPath::new("src/nested/keep.rs").unwrap(),
            )])),
        };
        assert!(selection.includes(&RepoPath::new("src/nested/keep.rs").unwrap()));
        assert!(!selection.includes(&RepoPath::new("src/other.rs").unwrap()));
    }

    #[test]
    fn scoped_selection_matches_rename_when_either_endpoint_is_selected() {
        let outside_to_inside = ChangedPath {
            source_location: RepoPath::new("archive/old.rs").unwrap(),
            location: RepoPath::new("src/scoped/new.rs").unwrap(),
        };
        let inside_to_outside = ChangedPath {
            source_location: RepoPath::new("src/scoped/old.rs").unwrap(),
            location: RepoPath::new("archive/new.rs").unwrap(),
        };
        let unrelated = ChangedPath {
            source_location: RepoPath::new("docs/old.rs").unwrap(),
            location: RepoPath::new("archive/docs-new.rs").unwrap(),
        };
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("src/scoped").unwrap()],
            changed: Some(HashSet::from([
                outside_to_inside.clone(),
                inside_to_outside.clone(),
                unrelated.clone(),
            ])),
        };

        assert!(selection.includes(&outside_to_inside.location));
        assert!(selection.includes(&inside_to_outside.location));
        assert!(!selection.includes(&unrelated.location));

        let explicit_source_selection = ReviewPathSelection::Scoped {
            files: HashSet::from([inside_to_outside.source_location.clone()]),
            dirs: Vec::new(),
            changed: Some(HashSet::from([inside_to_outside.clone()])),
        };
        assert!(explicit_source_selection.includes(&inside_to_outside.location));
    }

    #[test]
    fn resolve_targets_rejects_mixed_historical_and_worktree_content_sources() {
        let targets = vec![
            ReviewTarget::DirtyWorktree,
            ReviewTarget::Revision(RevisionExpr::new("abc1234").unwrap()),
        ];

        let err = resolve_targets_with(
            &targets,
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("Historical targets cannot be mixed with worktree-based targets")
        );
    }

    #[test]
    fn resolve_targets_makes_historical_content_and_diff_explicit() {
        let targets = vec![ReviewTarget::RevisionRange(
            RevisionRangeExpr::new("abc1234", "def5678").unwrap(),
        )];

        let resolved = resolve_targets_with(
            &targets,
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_or_else(|error| panic!("expected resolved targets: {error}"));

        assert_eq!(
            resolved.content_source,
            ReviewContentSource::Revision(CommitId::new("def5678").unwrap())
        );
        assert!(matches!(
            resolved.diff_selection,
            ReviewDiffSelection::Targets(_)
        ));
    }

    #[test]
    fn resolve_targets_intersects_explicit_file_and_changed_paths() {
        let file = RepoPath::new("src/lib.rs").unwrap();
        let other = RepoPath::new("src/other.rs").unwrap();
        let targets = vec![
            ReviewTarget::File(file.clone()),
            ReviewTarget::RevisionRange(RevisionRangeExpr::new("abc1234", "def5678").unwrap()),
        ];

        let resolved = resolve_targets_with(
            &targets,
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::from([ChangedPath::identity(other.clone())])),
        )
        .unwrap_or_else(|error| panic!("expected resolved targets: {error}"));

        let selection = resolved.path_selection();
        assert!(!selection.includes(&file));
        assert!(!selection.includes(&other));
    }

    #[test]
    fn changed_target_with_no_changed_paths_selects_nothing() {
        let other = RepoPath::new("src/other.rs").unwrap();
        let resolved = resolve_targets_with(
            &[ReviewTarget::MainDiff],
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_or_else(|error| panic!("expected resolved main target: {error}"));

        let selection = resolved.path_selection();
        assert_eq!(selection, ReviewPathSelection::Empty);
        assert!(!selection.includes(&other));
    }

    #[test]
    fn dirty_target_propagates_dirty_file_errors() {
        let err = resolve_targets_with(
            &[ReviewTarget::DirtyWorktree],
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Err(anyhow::anyhow!("dirty file query failed")),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_err();

        assert!(err.to_string().contains("dirty file query failed"));
    }

    #[test]
    fn explicit_target_intersected_with_empty_changed_paths_selects_nothing() {
        let file = RepoPath::new("src/lib.rs").unwrap();
        let resolved = resolve_targets_with(
            &[
                ReviewTarget::File(file.clone()),
                ReviewTarget::RevisionRange(RevisionRangeExpr::new("abc1234", "def5678").unwrap()),
            ],
            |revision| CommitId::new(revision.as_str()),
            unexpected_merge_base,
            || Ok(HashSet::new()),
            empty_main_diff,
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_or_else(|error| panic!("expected resolved range target: {error}"));

        let selection = resolved.path_selection();
        assert!(!selection.includes(&file));
    }

    #[test]
    fn changed_only_selection_includes_only_changed_paths() {
        let changed = RepoPath::new("src/lib.rs").unwrap();
        let other = RepoPath::new("src/other.rs").unwrap();
        let resolved = ResolvedTargets::new(
            ReviewContentSource::Workdir,
            ReviewDiffSelection::None,
            HashSet::new(),
            Vec::new(),
            HashSet::from([ChangedPath::identity(changed.clone())]),
        );

        let selection = resolved.path_selection();
        assert!(selection.includes(&changed));
        assert!(!selection.includes(&other));
    }

    #[test]
    fn explicit_and_changed_selections_split_cleanly_for_feedback() {
        let file = RepoPath::new("src/lib.rs").unwrap();
        let dir = RepoPath::new("src/nested").unwrap();
        let changed_path = RepoPath::new("src/nested/mod.rs").unwrap();
        let resolved = ResolvedTargets::new(
            ReviewContentSource::Workdir,
            ReviewDiffSelection::None,
            HashSet::from([file.clone()]),
            vec![dir.clone()],
            HashSet::from([ChangedPath::identity(changed_path.clone())]),
        );

        match resolved
            .explicit_selection()
            .unwrap_or_else(|| panic!("expected explicit selection"))
        {
            ReviewPathSelection::Scoped {
                files,
                dirs,
                changed,
            } => {
                assert!(files.contains(&file));
                assert_eq!(dirs, vec![dir]);
                assert!(changed.is_none());
            }
            other => panic!("expected Scoped, got {other:?}"),
        }

        match resolved
            .changed_selection()
            .unwrap_or_else(|| panic!("expected changed selection"))
        {
            ReviewPathSelection::Scoped {
                files,
                dirs,
                changed,
            } => {
                assert!(files.is_empty());
                assert!(dirs.is_empty());
                assert_eq!(
                    changed,
                    Some(HashSet::from([ChangedPath::identity(changed_path)]))
                );
            }
            other => panic!("expected Scoped, got {other:?}"),
        }
    }
}
