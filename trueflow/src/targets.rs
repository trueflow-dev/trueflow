use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::vcs;
use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

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
    /// Scope the review to every reviewable file under `path` (inclusive).
    /// Uses the workdir as the content source, same as `File`.
    Dir(RepoPath),
    Revision(RevisionSpec),
    RevisionRange(RevisionRangeSpec),
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
            if let Some((start, end)) = rest.split_once("..") {
                return Ok(Self::RevisionRange(RevisionRangeSpec::new(start, end)?));
            }
            return Ok(Self::Revision(RevisionSpec::new(rest)?));
        }
        Err(anyhow!("Unknown review target: {raw}"))
    }

    pub(crate) fn historical_content_revision(&self) -> Option<&RevisionSpec> {
        match self {
            Self::Revision(revision) => Some(revision),
            Self::RevisionRange(range) => Some(&range.end),
            Self::DirtyWorktree | Self::MainDiff | Self::File(_) | Self::Dir(_) => None,
        }
    }

    pub(crate) fn is_worktree_content_target(&self) -> bool {
        matches!(self, Self::DirtyWorktree | Self::MainDiff)
    }

    pub(crate) fn diff_target(&self) -> Option<ReviewDiffTarget> {
        match self {
            Self::MainDiff => Some(ReviewDiffTarget::MainDiff),
            Self::Revision(revision) => Some(ReviewDiffTarget::Revision(revision.clone())),
            Self::RevisionRange(range) => Some(ReviewDiffTarget::RevisionRange(range.clone())),
            Self::DirtyWorktree | Self::File(_) | Self::Dir(_) => None,
        }
    }
}

impl FromStr for ReviewTarget {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_cli(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewContentSource {
    Workdir,
    Revision(RevisionSpec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPathSelection {
    All,
    /// Explicit files plus one or more directory prefixes. A file matches when
    /// it satisfies the explicit file/dir selection and, when `changed` is
    /// present, also appears in that changed-path set.
    Scoped {
        files: HashSet<RepoPath>,
        dirs: Vec<RepoPath>,
        changed: Option<HashSet<RepoPath>>,
    },
}

impl ReviewPathSelection {
    pub fn includes(&self, file_path: &RepoPath, workdir_prefix: Option<&str>) -> Result<bool> {
        match self {
            Self::All => Ok(true),
            Self::Scoped {
                files,
                dirs,
                changed,
            } => {
                let explicit_match = if files.is_empty() && dirs.is_empty() {
                    true
                } else {
                    path_matches_specific_selection(files, file_path, workdir_prefix)?
                        || path_matches_dir_selection(dirs, file_path, workdir_prefix)?
                };

                if !explicit_match {
                    return Ok(false);
                }

                match changed {
                    Some(changed_paths) => {
                        path_matches_specific_selection(changed_paths, file_path, workdir_prefix)
                    }
                    None => Ok(true),
                }
            }
        }
    }
}

fn path_matches_specific_selection(
    targets: &HashSet<RepoPath>,
    file_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<bool> {
    if targets.contains(file_path) {
        return Ok(true);
    }
    if let Some(prefix) = workdir_prefix {
        let repo_path = RepoPath::new(format!("{prefix}/{file_path}"))?;
        return Ok(targets.contains(&repo_path));
    }
    Ok(false)
}

fn path_matches_dir_selection(
    dirs: &[RepoPath],
    file_path: &RepoPath,
    workdir_prefix: Option<&str>,
) -> Result<bool> {
    if dirs.iter().any(|dir| path_under_dir(file_path, dir)) {
        return Ok(true);
    }
    if let Some(prefix) = workdir_prefix {
        let repo_path = RepoPath::new(format!("{prefix}/{file_path}"))?;
        return Ok(dirs.iter().any(|dir| path_under_dir(&repo_path, dir)));
    }
    Ok(false)
}

/// True if `file` equals `dir` or lives under `dir` as a subtree.
/// Root directory matches every path.
fn path_under_dir(file: &RepoPath, dir: &RepoPath) -> bool {
    if dir.is_root() {
        return true;
    }

    file == dir
        || file
            .as_str()
            .strip_prefix(dir.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
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
    changed: HashSet<RepoPath>,
}

impl ResolvedTargets {
    pub(crate) fn new(
        content_source: ReviewContentSource,
        diff_selection: ReviewDiffSelection,
        files: HashSet<RepoPath>,
        dirs: Vec<RepoPath>,
        changed: HashSet<RepoPath>,
    ) -> Self {
        Self {
            content_source,
            diff_selection,
            files,
            dirs,
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
        if self.changed.is_empty() {
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
            ReviewPathSelection::All
        } else {
            ReviewPathSelection::Scoped {
                files: self.files.clone(),
                dirs: self.dirs.clone(),
                changed: (!self.changed.is_empty()).then_some(self.changed.clone()),
            }
        }
    }
}

pub fn resolve_targets(targets: &[ReviewTarget]) -> Result<ResolvedTargets> {
    resolve_targets_with(
        targets,
        vcs::dirty_files_from_workdir,
        vcs::files_changed_main_to_head,
        vcs::files_changed_in_revision,
        vcs::files_changed_in_range,
    )
}

pub(crate) fn resolve_targets_with<DirtyFn, MainFn, RevisionFn, RangeFn>(
    targets: &[ReviewTarget],
    dirty_files: DirtyFn,
    main_diff_files: MainFn,
    revision_files: RevisionFn,
    range_files: RangeFn,
) -> Result<ResolvedTargets>
where
    DirtyFn: Fn() -> Result<HashSet<RepoPath>>,
    MainFn: Fn() -> Result<HashSet<RepoPath>>,
    RevisionFn: Fn(&str) -> Result<HashSet<RepoPath>>,
    RangeFn: Fn(&str, &str) -> Result<HashSet<RepoPath>>,
{
    let content_source = resolve_content_source(targets)?;
    let diff_selection = resolve_diff_selection(targets);
    let mut files = HashSet::new();
    let mut dirs = Vec::new();
    let mut changed = HashSet::new();

    for target in targets {
        match target {
            ReviewTarget::DirtyWorktree => {
                if let Ok(dirty) = dirty_files() {
                    changed.extend(dirty);
                }
            }
            ReviewTarget::MainDiff => {
                changed.extend(main_diff_files()?);
            }
            ReviewTarget::File(path) => {
                files.insert(path.clone());
            }
            ReviewTarget::Dir(path) => {
                if !dirs.contains(path) {
                    dirs.push(path.clone());
                }
            }
            ReviewTarget::Revision(revision) => {
                changed.extend(revision_files(revision.as_str())?);
            }
            ReviewTarget::RevisionRange(range) => {
                changed.extend(range_files(range.start.as_str(), range.end.as_str())?);
            }
        }
    }

    Ok(ResolvedTargets::new(
        content_source,
        diff_selection,
        files,
        dirs,
        changed,
    ))
}

fn resolve_content_source(targets: &[ReviewTarget]) -> Result<ReviewContentSource> {
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

fn resolve_diff_selection(targets: &[ReviewTarget]) -> ReviewDiffSelection {
    let diff_targets = targets
        .iter()
        .filter_map(ReviewTarget::diff_target)
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
        ResolvedTargets, ReviewContentSource, ReviewDiffSelection, ReviewPathSelection,
        ReviewTarget, RevisionRangeSpec, RevisionSpec, path_under_dir, resolve_targets_with,
    };
    use crate::repo_path::RepoPath;
    use std::collections::HashSet;

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
    fn dir_target_parses_nested_path() {
        let target = ReviewTarget::from_cli("dir:trueflow/src/commands")
            .unwrap_or_else(|error| panic!("nested dir target: {error}"));
        assert_eq!(
            target,
            ReviewTarget::Dir(
                RepoPath::new("trueflow/src/commands")
                    .unwrap_or_else(|error| panic!("valid repo path: {error}")),
            )
        );
    }

    #[test]
    fn dir_target_rejects_absolute_path() {
        assert!(ReviewTarget::from_cli("dir:/tmp/absolute").is_err());
    }

    #[test]
    fn dir_target_is_workdir_content() {
        let target = ReviewTarget::Dir(RepoPath::new("website").unwrap());
        assert!(target.historical_content_revision().is_none());
        assert!(target.diff_target().is_none());
    }

    #[test]
    fn scoped_selection_includes_files_under_dir() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: None,
        };
        assert!(
            selection
                .includes(&RepoPath::new("website/index.html").unwrap(), None)
                .unwrap()
        );
        assert!(
            selection
                .includes(&RepoPath::new("website/a/b/c.js").unwrap(), None)
                .unwrap()
        );
        assert!(
            selection
                .includes(&RepoPath::new("website").unwrap(), None)
                .unwrap()
        );
    }

    #[test]
    fn scoped_selection_excludes_files_outside_dir() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: None,
        };
        assert!(
            !selection
                .includes(&RepoPath::new("docs/intro.md").unwrap(), None)
                .unwrap()
        );
        assert!(
            !selection
                .includes(&RepoPath::new("website-next/index.html").unwrap(), None)
                .unwrap()
        );
    }

    #[test]
    fn scoped_selection_still_checks_explicit_files() {
        let explicit = RepoPath::new("README.md").unwrap();
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::from([explicit.clone()]),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: None,
        };
        assert!(selection.includes(&explicit, None).unwrap());
    }

    #[test]
    fn scoped_selection_with_changed_paths_requires_dir_intersection() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("website").unwrap()],
            changed: Some(HashSet::from(
                [RepoPath::new("website/index.html").unwrap()],
            )),
        };
        assert!(
            selection
                .includes(&RepoPath::new("website/index.html").unwrap(), None)
                .unwrap()
        );
        assert!(
            !selection
                .includes(&RepoPath::new("website/other.html").unwrap(), None)
                .unwrap()
        );
        assert!(
            !selection
                .includes(&RepoPath::new("docs/index.html").unwrap(), None)
                .unwrap()
        );
    }

    #[test]
    fn scoped_selection_uses_workdir_prefix_for_repo_relative_dir_targets() {
        let selection = ReviewPathSelection::Scoped {
            files: HashSet::new(),
            dirs: vec![RepoPath::new("src/nested").unwrap()],
            changed: Some(HashSet::from(
                [RepoPath::new("src/nested/keep.rs").unwrap()],
            )),
        };
        assert!(
            selection
                .includes(&RepoPath::new("nested/keep.rs").unwrap(), Some("src"))
                .unwrap()
        );
        assert!(
            !selection
                .includes(&RepoPath::new("other.rs").unwrap(), Some("src"))
                .unwrap()
        );
    }

    #[test]
    fn path_under_dir_matches_exact_and_subtree() {
        let dir = RepoPath::new("website").unwrap();
        assert!(path_under_dir(&RepoPath::new("website").unwrap(), &dir));
        assert!(path_under_dir(
            &RepoPath::new("website/index.html").unwrap(),
            &dir
        ));
        assert!(!path_under_dir(
            &RepoPath::new("website-next/index.html").unwrap(),
            &dir
        ));
        assert!(!path_under_dir(&RepoPath::new("docs/x.md").unwrap(), &dir));
    }

    #[test]
    fn path_under_dir_root_matches_everything() {
        let root = RepoPath::root();
        assert!(path_under_dir(
            &RepoPath::new("anything.rs").unwrap(),
            &root
        ));
    }

    #[test]
    fn resolve_targets_rejects_mixed_historical_and_worktree_content_sources() {
        let targets = vec![
            ReviewTarget::MainDiff,
            ReviewTarget::Revision(RevisionSpec::new("abc1234").unwrap()),
        ];

        let err = resolve_targets_with(
            &targets,
            || Ok(HashSet::new()),
            || Ok(HashSet::new()),
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
            RevisionRangeSpec::new("abc1234", "def5678").unwrap(),
        )];

        let resolved = resolve_targets_with(
            &targets,
            || Ok(HashSet::new()),
            || Ok(HashSet::new()),
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::new()),
        )
        .unwrap_or_else(|error| panic!("expected resolved targets: {error}"));

        assert_eq!(
            resolved.content_source,
            ReviewContentSource::Revision(RevisionSpec::new("def5678").unwrap())
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
            ReviewTarget::RevisionRange(RevisionRangeSpec::new("abc1234", "def5678").unwrap()),
        ];

        let resolved = resolve_targets_with(
            &targets,
            || Ok(HashSet::new()),
            || Ok(HashSet::new()),
            |_revision| Ok(HashSet::new()),
            |_start, _end| Ok(HashSet::from([other.clone()])),
        )
        .unwrap_or_else(|error| panic!("expected resolved targets: {error}"));

        let selection = resolved.path_selection();
        assert!(!selection.includes(&file, None).unwrap());
        assert!(!selection.includes(&other, None).unwrap());
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
            HashSet::from([changed.clone()]),
        );

        let selection = resolved.path_selection();
        assert!(selection.includes(&changed, None).unwrap());
        assert!(!selection.includes(&other, None).unwrap());
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
            HashSet::from([changed_path.clone()]),
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
                assert_eq!(changed, Some(HashSet::from([changed_path])));
            }
            other => panic!("expected Scoped, got {other:?}"),
        }
    }
}
