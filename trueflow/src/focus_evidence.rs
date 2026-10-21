//! Bounded, source-pinned observations, not a risk ranking or a coverage analysis.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::block::BlockKind;
use crate::commands::review::CollectedReview;
use crate::coverage::CoverageIndex;
use crate::hashing::BytesHash;
use crate::onboarding::ReviewLocation;
use crate::repo_path::RepoPath;
use crate::store::{FileStore, ReviewCheck, ReviewDatabase, ReviewStore};
use crate::vcs::{self, DiffLineKind, FileDiff};

const MAX_CANDIDATES: usize = 20_000;
const MAX_INVENTORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_EXCERPT_BYTES: usize = 1600;
const MAX_HISTORY_COMMITS: usize = 8;
const MAX_HISTORY_BLOB_BYTES: u64 = 128 * 1024;
const MAX_PATCH_BYTES: usize = 2048;
const MAX_REVIEW_NOTES: usize = 8;
const MAX_TEST_BLOCKS: usize = 2048;
const MAX_LISTED_DIAGNOSTICS_PER_KIND: usize = 3;
const MAX_DIAGNOSTIC_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Complexity,
    EntryPoint,
    TrustBoundary,
    Write,
    ErrorHandling,
    Concurrency,
    SourceClassification,
    RelatedTest,
    HumanReview,
    GitPatch,
    HistoryStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FocusEvidence {
    pub id: String,
    pub kind: EvidenceKind,
    pub detail: String,
    /// Exact source/record digest or immutable Git commit and blob identities.
    pub provenance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FocusCandidate {
    pub target: ReviewLocation,
    pub estimated_seconds: usize,
    pub kind: BlockKind,
    pub evidence: Vec<FocusEvidence>,
    pub excerpt: String,
}

/// The unreviewed blocks an inventory covers. Evidence is gathered per path and block,
/// so narrowing the scope never changes the evidence IDs of a covered candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusScope {
    Repository,
    /// A user-selected file or directory; it must name scanned source.
    Under(RepoPath),
    /// Exactly the files a saved plan targets.
    Files(BTreeSet<RepoPath>),
}

impl FocusScope {
    fn contains(&self, path: &RepoPath) -> bool {
        match self {
            Self::Repository => true,
            Self::Under(scope) => path.is_under(scope),
            Self::Files(paths) => paths.contains(path),
        }
    }
}

/// Where an evidence limitation comes from. Limitations are repository-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    Scan,
    ReviewBinding,
    GitHistory,
    RelatedTests,
}

impl DiagnosticKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::ReviewBinding => "review binding",
            Self::GitHistory => "Git history",
            Self::RelatedTests => "related tests",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct FocusDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

impl FocusDiagnostic {
    pub fn new(kind: DiagnosticKind, message: &str) -> Self {
        Self {
            kind,
            message: bounded(message, MAX_DIAGNOSTIC_BYTES).to_owned(),
        }
    }
}

/// A bounded view of the repository-wide limitations: every diagnostic is counted
/// by kind, but only the first few of each kind are listed. Their number grows with
/// stale review records and scan problems, so they must not consume a prompt budget.
#[derive(Debug, Serialize)]
pub struct LimitationSummary<'a> {
    pub total: usize,
    pub counts: BTreeMap<DiagnosticKind, usize>,
    pub listed: Vec<&'a FocusDiagnostic>,
}

impl<'a> LimitationSummary<'a> {

    pub fn of(diagnostics: &'a [FocusDiagnostic]) -> Self {
        let mut counts = BTreeMap::new();
        let mut listed = Vec::new();
        for diagnostic in diagnostics {
            let count = counts.entry(diagnostic.kind).or_insert(0);
            if *count < MAX_LISTED_DIAGNOSTICS_PER_KIND {
                listed.push(diagnostic);
            }
            *count += 1;
        }
        Self {
            total: diagnostics.len(),
            counts,
            listed,
        }
    }
}

impl std::fmt::Display for LimitationSummary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} repository-wide", self.total)?;
        for (kind, count) in &self.counts {
            write!(f, "; {}: {count}", kind.label())?;
        }
        for diagnostic in &self.listed {
            write!(f, "\n- {}", diagnostic.message)?;
        }
        if self.listed.len() < self.total {
            write!(
                f,
                "\n({} more; see `trueflow focus --candidates`)",
                self.total - self.listed.len()
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct FocusInventory {
    pub candidates: Vec<FocusCandidate>,
    pub snapshot_hash: BytesHash,
    pub diagnostics: Vec<FocusDiagnostic>,
}

impl FocusInventory {
    pub fn capture(review: &CollectedReview, scope: &FocusScope) -> Result<Self> {
        let database = FileStore::new()?.load_database()?;
        let repo = vcs::repo_from_workdir();
        let mut inventory = capture_with(review, scope, &database, repo.as_ref().ok())?;
        if let Err(error) = repo {
            inventory.diagnostics.push(FocusDiagnostic::new(
                DiagnosticKind::GitHistory,
                &format!("Git discovery unavailable: {error}"),
            ));
        }
        Ok(inventory)
    }

    pub fn contains_evidence(&self, target: &ReviewLocation, id: &str) -> bool {
        self.candidates.iter().any(|candidate| {
            &candidate.target == target
                && candidate.evidence.iter().any(|evidence| evidence.id == id)
        })
    }
}

impl FocusEvidence {
    fn new(
        target: &ReviewLocation,
        kind: EvidenceKind,
        detail: String,
        provenance: String,
    ) -> Result<Self> {
        let bytes = serde_json::to_vec(&(1, target, kind, &detail, &provenance))?;
        Ok(Self {
            id: format!("ev-{}", BytesHash::from_bytes(&bytes)),
            kind,
            detail,
            provenance,
        })
    }
}

fn capture_with(
    review: &CollectedReview,
    scope: &FocusScope,
    database: &ReviewDatabase,
    repo: Option<&gix::Repository>,
) -> Result<FocusInventory> {
    if let FocusScope::Under(path) = scope {
        ensure!(
            review.tree.nodes().iter().any(|node| node.path.is_under(path)),
            "--path {path} matches no scanned source files; check the path and scan/review filters"
        );
    }
    let mut nodes: Vec<_> = review
        .unreviewed_block_nodes
        .iter()
        .copied()
        .filter(|id| {
            !review.tree.is_container_block(*id) && scope.contains(&review.tree.node(*id).path)
        })
        .collect();
    ensure!(
        nodes.len() <= MAX_CANDIDATES,
        "focus inventory exceeds {MAX_CANDIDATES} candidates; narrow the path scope"
    );
    nodes.sort_unstable_by_key(|id| {
        let node = review.tree.node(*id);
        (
            &node.path,
            node.block.as_ref().map(|b| (b.start_byte, b.end_byte)),
        )
    });
    let paths: BTreeSet<_> = nodes
        .iter()
        .map(|id| review.tree.node(*id).path.clone())
        .collect();
    let coverage = CoverageIndex::build(&review.tree, database)?;
    let mut diagnostics: Vec<_> = review
        .summary
        .diagnostics
        .iter()
        .map(|d| FocusDiagnostic::new(DiagnosticKind::Scan, &d.display_message()))
        .collect();
    for diagnostic in coverage.diagnostics() {
        diagnostics.push(FocusDiagnostic::new(
            DiagnosticKind::ReviewBinding,
            &format!(
                "Human review binding diagnostic: {}",
                serde_json::to_string(diagnostic)?
            ),
        ));
    }
    let history = history(repo, &paths, &mut diagnostics)?;
    let tests = TestIndex::build(review);
    if tests.truncated {
        diagnostics.push(FocusDiagnostic::new(DiagnosticKind::RelatedTests, "Related-test index truncated; only bounded sampled tokens/paths are considered; this is not test coverage."));
    }
    let mut candidates = Vec::with_capacity(nodes.len());
    let mut payload_bytes = 0usize;
    for id in nodes {
        let node = review.tree.node(id);
        let block = node
            .block
            .as_ref()
            .context("focus candidate is not a block")?;
        let target = ReviewLocation::for_block(&node.path, block);
        let sample = source_sample(&block.content);
        let mut evidence = Vec::new();
        let source_pin = target.content_hash.to_string();
        let mut add = |kind, detail| -> Result<()> {
            evidence.push(FocusEvidence::new(
                &target,
                kind,
                detail,
                source_pin.clone(),
            )?);
            Ok(())
        };
        add(
            EvidenceKind::Complexity,
            match block.complexity {
                Some(score) => format!(
                    "Existing block complexity metadata: {score}; {} source lines. This is not a final risk score.",
                    target.end_line.saturating_sub(target.start_line) + 1
                ),
                None => {
                    "Block complexity metadata unavailable; no complexity value inferred.".into()
                }
            },
        )?;
        if let Some(class) = source_class(node) {
            add(
                EvidenceKind::SourceClassification,
                format!(
                    "{}; discount volume/churn as a risk signal. Classification is heuristic, not a safety claim.",
                    class.label()
                ),
            )?;
        }
        for kind in [
            EvidenceKind::EntryPoint,
            EvidenceKind::TrustBoundary,
            EvidenceKind::Write,
            EvidenceKind::ErrorHandling,
            EvidenceKind::Concurrency,
        ] {
            if let Some(line) = sample.lines().find(|line| signal(kind, line)) {
                add(
                    kind,
                    format!(
                        "Lexical source heuristic, not semantic proof: {}",
                        bounded(line.trim(), 240)
                    ),
                )?;
            }
        }
        if block.content.len() > MAX_SOURCE_BYTES {
            add(
                EvidenceKind::SourceClassification,
                format!(
                    "Source observations truncated to {MAX_SOURCE_BYTES} sampled bytes from beginning/middle/end; exact target hash still covers the whole block."
                ),
            )?;
        }
        let block_coverage = coverage.block(&node.path, block);
        let records = block_coverage.effective_records();
        let latest = block_coverage.effective_latest_verdict_for(&ReviewCheck::review());
        let records_pin = BytesHash::from_bytes(&serde_json::to_vec(&records)?).to_string();
        let mut review_detail = match latest {
            Some(verdict) => format!("Effective stored human review verdict: {verdict}. Records are review-store claims, not verified reviewer identity or test coverage."),
            None => "No effective human review verdict for the review check. Test relationships are not human review coverage.".into(),
        };
        for record in records.iter().rev().take(MAX_REVIEW_NOTES) {
            review_detail.push_str(&format!(
                "\nRecord {} check={} verdict={} timestamp={} identity={}: {}",
                bounded(&record.id, 80),
                record.check,
                record.verdict,
                record.timestamp,
                bounded(&serde_json::to_string(&record.identity)?, 160),
                bounded(record.note.as_deref().unwrap_or("(no note)"), 512)
            ));
        }
        if records.len() > MAX_REVIEW_NOTES {
            review_detail
                .push_str("\nReview notes truncated; provenance hashes all effective records.");
        }
        evidence.push(FocusEvidence::new(
            &target,
            EvidenceKind::HumanReview,
            review_detail,
            records_pin,
        )?);
        let mut related = tests.related(&node.path, &sample);
        related.retain(|test| *test != id);
        related.sort_unstable_by_key(|test| {
            let node = review.tree.node(*test);
            (
                &node.path,
                node.block.as_ref().map(|block| block.start_byte),
            )
        });
        for test in related.into_iter().take(3) {
            let test_node = review.tree.node(test);
            let test_block = test_node
                .block
                .as_ref()
                .context("related test is not a block")?;
            let test_target = ReviewLocation::for_block(&test_node.path, test_block);
            evidence.push(FocusEvidence::new(&target, EvidenceKind::RelatedTest,
                format!("Related test heuristic by declared symbol or file stem: {}:{}-{}; not coverage, execution, or an assertion of correctness.\n{}",
                    test_target.path, test_target.start_line, test_target.end_line, bounded(&source_sample(&test_block.content), 320)),
                serde_json::to_string(&test_target)?)?);
        }
        if let Some(observations) = history.get(&node.path) {
            for observation in observations {
                evidence.push(FocusEvidence::new(
                    &target,
                    observation.kind,
                    observation.detail.clone(),
                    observation.provenance.clone(),
                )?);
            }
        }
        let excerpt = excerpt(&sample);
        payload_bytes = payload_bytes.saturating_add(excerpt.len()).saturating_add(
            evidence
                .iter()
                .map(|e| e.detail.len() + e.provenance.len() + e.id.len())
                .sum::<usize>(),
        );
        ensure!(
            payload_bytes <= MAX_INVENTORY_BYTES,
            "focus evidence exceeds {MAX_INVENTORY_BYTES} payload bytes; narrow the path scope"
        );
        candidates.push(FocusCandidate {
            estimated_seconds: target.estimated_seconds(),
            target,
            kind: block.kind,
            evidence,
            excerpt,
        });
    }
    // Neither diagnostics nor transient scan counters participate in source/evidence identity.
    let snapshot_hash = BytesHash::from_bytes(&serde_json::to_vec(&candidates)?);
    diagnostics.sort();
    diagnostics.dedup();
    Ok(FocusInventory {
        candidates,
        snapshot_hash,
        diagnostics,
    })
}

fn bounded(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn source_sample(source: &str) -> std::borrow::Cow<'_, str> {
    if source.len() <= MAX_SOURCE_BYTES {
        return std::borrow::Cow::Borrowed(source);
    }
    let width = (MAX_SOURCE_BYTES - 64) / 3;
    let mut sample = String::with_capacity(MAX_SOURCE_BYTES);
    for offset in [0, source.len() / 2 - width / 2, source.len() - width] {
        let mut start = offset;
        while !source.is_char_boundary(start) {
            start += 1;
        }
        if !sample.is_empty() {
            sample.push_str("\n[... source sampling gap ...]\n");
        }
        sample.push_str(bounded(&source[start..], width));
    }
    std::borrow::Cow::Owned(sample)
}

fn is_comment(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.starts_with("# ")
}

pub(crate) fn signal(kind: EvidenceKind, line: &str) -> bool {
    if is_comment(line) {
        return false;
    }
    let terms: &[&str] = match kind {
        EvidenceKind::EntryPoint => &[
            "pub fn ",
            "pub async fn ",
            "export ",
            "public ",
            "fn main(",
            "def main(",
            "route(",
            "handler",
        ],
        EvidenceKind::TrustBoundary => &[
            "request",
            "authorize",
            "authenticat",
            "permission",
            "deserialize",
            "parse(",
            "unsafe ",
            "Command::",
            "exec(",
            "token",
            "socket",
        ],
        EvidenceKind::Write => &[
            "write(",
            "write_all(",
            "save(",
            "insert(",
            "update(",
            "delete(",
            "remove_file(",
            "commit(",
            "UPDATE ",
            "INSERT ",
            "DELETE ",
        ],
        EvidenceKind::ErrorHandling => &[
            "?", "Err(", "unwrap(", "expect(", "panic!", "throw ", "catch", "except", "raise ",
        ],
        EvidenceKind::Concurrency => &[
            "async ", "await", "spawn(", "Mutex", "RwLock", "Atomic", "lock(", "thread",
            "channel(", "select!",
        ],
        _ => &[],
    };
    terms.iter().any(|term| line.contains(term))
}

fn excerpt(sample: &str) -> String {
    let lines: Vec<_> = sample
        .lines()
        .filter(|line| !line.trim().is_empty() && !is_comment(line))
        .collect();
    let mut selected = BTreeSet::new();
    if !lines.is_empty() {
        for index in 0..8 {
            selected.insert(index * (lines.len() - 1) / 7);
        }
        for kind in [
            EvidenceKind::TrustBoundary,
            EvidenceKind::Write,
            EvidenceKind::ErrorHandling,
            EvidenceKind::Concurrency,
        ] {
            if let Some(index) = lines.iter().position(|line| signal(kind, line)) {
                selected.insert(index);
            }
        }
    }
    let mut output = String::new();
    for index in selected {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(bounded(lines[index], 120));
    }
    bounded(&output, MAX_EXCERPT_BYTES).to_owned()
}

/// Heuristic provenance of a block's source. Absent means first-party, non-test source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceClass {
    Vendor,
    Generated,
    Test,
}

impl SourceClass {
    const fn label(self) -> &'static str {
        match self {
            Self::Vendor => "vendor/dependency source",
            Self::Generated => "generated/build source",
            Self::Test => "test-only source heuristic",
        }
    }
}

/// Dependency lock files are machine-generated even though they are committed.
const LOCK_FILE_NAMES: &[&str] = &[
    "Cargo.lock",
    "flake.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "Package.resolved",
    "pubspec.lock",
    "mix.lock",
];

pub(crate) fn source_class(node: &crate::tree::TreeNode) -> Option<SourceClass> {
    let block = node.block.as_ref()?;
    let path = node.path.as_str();
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if path.split('/').any(|part| {
        matches!(
            part,
            "vendor" | "node_modules" | "third_party" | "third-party"
        )
    }) {
        Some(SourceClass::Vendor)
    } else if LOCK_FILE_NAMES.contains(&file_name)
        || path
            .split('/')
            .any(|part| matches!(part, "generated" | "dist" | "build"))
        || block.tags.iter().any(|tag| tag == "generated")
        || bounded(&block.content, 1024)
            .to_ascii_lowercase()
            .contains("automatically generated")
        || bounded(&block.content, 1024).contains("@generated")
    {
        Some(SourceClass::Generated)
    } else if crate::review_order::is_test_block(&node.path, node) {
        Some(SourceClass::Test)
    } else {
        None
    }
}

fn stem(path: &RepoPath) -> &str {
    Path::new(path.as_str())
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim_start_matches("test_")
        .trim_end_matches("_test")
        .trim_end_matches(".test")
        .trim_end_matches(".spec")
}

fn declared_symbol(source: &str) -> Option<&str> {
    let mut words = source.split(|c: char| !c.is_alphanumeric() && c != '_');
    while let Some(word) = words.next() {
        if matches!(word, "fn" | "function" | "def" | "func") {
            return words.find(|word| !word.is_empty());
        }
    }
    None
}

#[derive(Default)]
struct TestIndex {
    tokens: BTreeMap<String, Vec<crate::tree::TreeNodeId>>,
    stems: BTreeMap<String, Vec<crate::tree::TreeNodeId>>,
    truncated: bool,
}

impl TestIndex {
    fn build(review: &CollectedReview) -> Self {
        let mut index = Self::default();
        let mut nodes: Vec<_> = review
            .tree
            .nodes()
            .iter()
            .filter(|node| {
                node.block.is_some()
                    && crate::review_order::is_test_block(&node.path, node)
                    && !review.tree.is_container_block(node.id)
            })
            .collect();
        nodes.sort_unstable_by_key(|node| (&node.path, node.block.as_ref().map(|b| b.start_byte)));
        index.truncated = nodes.len() > MAX_TEST_BLOCKS;
        for node in nodes.into_iter().take(MAX_TEST_BLOCKS) {
            let Some(block) = &node.block else {
                continue;
            };
            let ids = index.stems.entry(stem(&node.path).to_owned()).or_default();
            if ids.len() < 3 {
                ids.push(node.id);
            } else {
                index.truncated = true;
            }
            let sample = source_sample(&block.content);
            let mut tokens = sample
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|word| word.len() > 2);
            for token in tokens.by_ref().take(256) {
                let ids = index.tokens.entry(token.to_owned()).or_default();
                if ids.last() == Some(&node.id) {
                    continue;
                }
                if ids.len() < 3 {
                    ids.push(node.id);
                } else {
                    index.truncated = true;
                }
            }
            index.truncated |= tokens.next().is_some() || block.content.len() > MAX_SOURCE_BYTES;
        }
        index
    }

    fn related(&self, path: &RepoPath, sample: &str) -> Vec<crate::tree::TreeNodeId> {
        let mut ids: BTreeSet<&crate::tree::TreeNodeId> = BTreeSet::new();
        if let Some(nodes) = self.stems.get(stem(path)) {
            ids.extend(nodes);
        }
        if let Some(nodes) = declared_symbol(sample).and_then(|symbol| self.tokens.get(symbol)) {
            ids.extend(nodes);
        }
        ids.into_iter().copied().collect()
    }
}

struct Observation {
    kind: EvidenceKind,
    detail: String,
    provenance: String,
}

fn history(
    repo: Option<&gix::Repository>,
    paths: &BTreeSet<RepoPath>,
    diagnostics: &mut Vec<FocusDiagnostic>,
) -> Result<BTreeMap<RepoPath, Vec<Observation>>> {
    let mut result: BTreeMap<_, _> = paths
        .iter()
        .cloned()
        .map(|path| (path, Vec::new()))
        .collect();
    let mut status = "Git history unavailable: no repository.".to_owned();
    let mut pin = "unavailable".to_owned();
    if let Some(repo) = repo {
        match vcs::recent_commits_in_repo(repo, MAX_HISTORY_COMMITS + 1) {
            Ok(commits) if !commits.is_empty() => {
                let truncated = commits.len() > MAX_HISTORY_COMMITS;
                let commits = &commits[..commits.len().min(MAX_HISTORY_COMMITS)];
                pin = commits
                    .iter()
                    .map(|commit| commit.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                status = format!(
                    "Git history available: {} recent first-parent commits inspected{}; file-local patches, not current-block line attribution. No merge-side traversal, rename following, or full-history claim.",
                    commits.len(),
                    if truncated {
                        "; history truncated at commit limit"
                    } else {
                        ""
                    }
                );
                for commit in commits {
                    if let Err(error) = append_commit_history(repo, commit, paths, &mut result) {
                        let message = format!(
                            "Git history unavailable in {}: {}",
                            commit.id,
                            bounded(&error.to_string(), 512)
                        );
                        diagnostics.push(FocusDiagnostic::new(DiagnosticKind::GitHistory, &message));
                        for observations in result.values_mut() {
                            observations.push(Observation {
                                kind: EvidenceKind::HistoryStatus,
                                detail: message.clone(),
                                provenance: commit.id.to_string(),
                            });
                        }
                    }
                    let bytes: usize = result
                        .values()
                        .flatten()
                        .map(|o| o.detail.len() + o.provenance.len())
                        .sum();
                    ensure!(
                        bytes <= MAX_INVENTORY_BYTES,
                        "focus history exceeds {MAX_INVENTORY_BYTES} payload bytes; narrow the path scope"
                    );
                }
            }
            Ok(_) => {
                status =
                    "Git history unavailable: no readable HEAD commit (empty or unborn repository)."
                        .into();
            }
            Err(error) => {
                status = format!(
                    "Git history unavailable: first-parent traversal failed: {}",
                    bounded(&error.to_string(), 512)
                );
            }
        }
    }
    diagnostics.push(FocusDiagnostic::new(DiagnosticKind::GitHistory, &status));
    for observations in result.values_mut() {
        observations.push(Observation {
            kind: EvidenceKind::HistoryStatus,
            detail: status.clone(),
            provenance: pin.clone(),
        });
    }
    Ok(result)
}

fn bounded_blob(tree: &gix::Tree<'_>, path: &RepoPath) -> Result<String> {
    let Some(entry) = tree.lookup_entry_by_path(Path::new(path.as_str()))? else {
        return Ok("absent".into());
    };
    let header = entry.id().header()?;
    ensure!(header.kind() == gix::object::Kind::Blob, "non-blob source");
    ensure!(
        header.size() <= MAX_HISTORY_BLOB_BYTES,
        "history source truncated: blob exceeds {MAX_HISTORY_BLOB_BYTES} bytes"
    );
    Ok(entry.id().to_string())
}

fn append_commit_history(
    repo: &gix::Repository,
    info: &vcs::CommitInfo,
    paths: &BTreeSet<RepoPath>,
    output: &mut BTreeMap<RepoPath, Vec<Observation>>,
) -> Result<()> {
    let commit = repo
        .rev_parse_single(info.id.as_str())?
        .object()?
        .peel_to_commit()?;
    let head = commit.tree()?;
    let base = match commit.parent_ids().next() {
        Some(parent) => repo.find_commit(parent)?.tree()?,
        None => repo.empty_tree(),
    };
    let changes = vcs::files_changed_in_revision_in_repo(repo, info.id.as_str())?;
    let mut selected = Vec::new();
    let mut pins = BTreeMap::new();
    for change in changes {
        if !paths.contains(&change.location) {
            continue;
        }
        let source = &change.source_location;
        let pin = (|| -> Result<String> {
            Ok(format!(
                "commit={} base_blob={} head_blob={} source={} destination={}",
                info.id,
                bounded_blob(&base, source)?,
                bounded_blob(&head, &change.location)?,
                source,
                change.location
            ))
        })();
        match pin {
            Ok(pin) => {
                pins.insert(change.location.clone(), pin);
                selected.push(change.location);
            }
            Err(error) => {
                if let Some(observations) = output.get_mut(&change.location) {
                    observations.push(Observation {
                        kind: EvidenceKind::HistoryStatus,
                        detail: format!(
                            "Git patch unavailable in {}: {}",
                            info.id,
                            bounded(&error.to_string(), 512)
                        ),
                        provenance: info.id.to_string(),
                    });
                }
            }
        }
    }
    selected.sort();
    // Keep at most sixteen bounded file diffs live; noisy earlier paths cannot
    // consume a first-N history quota and hide later candidates.
    for batch in selected.chunks(16) {
        let mut diffs = vcs::file_diffs_for_revision(repo, info.id.as_str(), batch)?;
        for path in batch {
            let diff = diffs.take(path);
            let observations = output
                .get_mut(path)
                .context("selected history path missing")?;
            let provenance = pins
                .remove(path)
                .context("selected history provenance missing")?;
            match diff {
            FileDiff::Text { hunks, .. } => {
                let mut removed = String::new();
                let mut added = String::new();
                let mut patch = String::new();
                let mut patch_truncated = false;
                for hunk in &hunks {
                    for line in &hunk.lines {
                        let (prefix, normalized) = match line.kind {
                            DiffLineKind::Added => ('+', &mut added),
                            DiffLineKind::Removed => ('-', &mut removed),
                            DiffLineKind::Context => continue,
                        };
                        normalized.push_str(line.text.trim_end());
                        normalized.push('\n');
                        let remaining = MAX_PATCH_BYTES.saturating_sub(patch.len());
                        if remaining > 2 {
                            patch.push(prefix);
                            let text = bounded(&line.text, remaining - 2);
                            patch.push_str(text);
                            if !text.ends_with('\n') { patch.push('\n'); }
                            patch_truncated |= text.len() != line.text.len();
                        } else { patch_truncated = true; }
                    }
                }
                let label = if removed == added { "mechanical trailing-whitespace-only change; discounted churn" }
                    else { "substantive textual change; corrective intent is unverified, not inferred from the message" };
                observations.push(Observation { kind: EvidenceKind::GitPatch, provenance,
                    detail: format!("Commit {} ({}) — {label}. File-local evidence for {path}; generated/vendor/test classification still applies.{} \n{patch}",
                        info.id, bounded(&info.summary, 160), if patch_truncated { " Patch excerpt truncated." } else { "" }) });
            }
            FileDiff::Unavailable { reason, .. } => observations.push(Observation { kind: EvidenceKind::HistoryStatus,
                detail: format!("Git patch unavailable in {} for {path}: {reason:?}", info.id), provenance }),
            FileDiff::NoTextChanges { .. } => observations.push(Observation { kind: EvidenceKind::HistoryStatus,
                detail: format!("Commit {} changed file identity/metadata without a text patch for {path}; not substantive churn evidence.", info.id), provenance }),
        }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Language;
    use crate::block::{Block, ByteSpan, FileState};
    use crate::commands::review::ReviewSummary;
    use crate::store::{BlockState, Identity, Record, RepoRef, ReviewTargetRef, Verdict};
    use crate::targets::ReviewDiffSelection;
    use crate::test_git::{run_git, temp_git_repo};
    use crate::tree::build_tree_from_files;

    fn review(sources: &[(&str, &str)]) -> CollectedReview {
        let files: Vec<_> = sources
            .iter()
            .map(|(path, source)| {
                let mut block = Block::from_file_range(
                    source,
                    BlockKind::Function,
                    ByteSpan::new(0, source.len()),
                )
                .unwrap();
                block.complexity = Some(3);
                FileState::from_text(
                    RepoPath::new(path).unwrap(),
                    Language::Rust,
                    source.as_bytes(),
                    vec![block],
                )
            })
            .collect();
        let tree = build_tree_from_files(&files);
        let unreviewed_block_nodes = tree
            .nodes()
            .iter()
            .filter(|node| node.block.is_some())
            .map(|node| node.id)
            .collect();
        CollectedReview {
            summary: ReviewSummary {
                files: Vec::new(),
                total_blocks: files.len(),
                diagnostics: Vec::new(),
            },
            tree,
            unreviewed_block_nodes,
            diff_selection: ReviewDiffSelection::None,
            commented_block_nodes: Default::default(),
            diff_block_sides: Default::default(),
            file_change_kinds: Default::default(),
            block_change_kinds: Default::default(),
        }
    }

    fn capture(
        review: &CollectedReview,
        database: &ReviewDatabase,
        repo: Option<&gix::Repository>,
    ) -> FocusInventory {
        capture_with(review, &FocusScope::Repository, database, repo).unwrap()
    }

    #[test]
    fn limitation_summary_counts_every_kind_but_lists_only_a_few() {
        let mut diagnostics =
            vec![FocusDiagnostic::new(DiagnosticKind::ReviewBinding, "stale record"); 700];
        diagnostics.push(FocusDiagnostic::new(
            DiagnosticKind::Scan,
            "src/x.rs: invalid UTF-8",
        ));
        let text = LimitationSummary::of(&diagnostics).to_string();
        assert!(
            text.starts_with("701 repository-wide; scan: 1; review binding: 700\n"),
            "{text}"
        );
        assert!(text.contains("\n- src/x.rs: invalid UTF-8"));
        assert_eq!(text.lines().filter(|l| l.starts_with("- ")).count(), 4);
        assert!(text.ends_with("(697 more; see `trueflow focus --candidates`)"));
    }

    #[test]
    fn deterministic_ids_reject_stale_source_and_scope_excludes_siblings() {
        let db = ReviewDatabase::default();
        let initial = review(&[
            ("src/a.rs", "pub fn run() { save(); }"),
            ("src/ab.rs", "fn other() {}"),
        ]);
        let first = capture(&initial, &db, None);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(capture(&initial, &db, None)).unwrap()
        );
        let mut reordered = review(&[
            ("src/ab.rs", "fn other() {}"),
            ("src/a.rs", "pub fn run() { save(); }"),
        ]);
        reordered.summary.total_blocks = usize::MAX;
        assert_eq!(
            first.snapshot_hash,
            capture(&reordered, &db, None).snapshot_hash
        );
        let changed = capture(
            &review(&[("src/a.rs", "pub fn run() { save_checked(); }")]),
            &db,
            None,
        );
        assert_ne!(first.snapshot_hash, changed.snapshot_hash);
        for evidence in &first.candidates[0].evidence {
            assert!(!changed.contains_evidence(&first.candidates[0].target, &evidence.id));
        }
        let scope = RepoPath::new("src/a.rs").unwrap();
        let scoped = capture_with(&initial, &FocusScope::Under(scope.clone()), &db, None).unwrap();
        assert_eq!(
            scoped
                .candidates
                .iter()
                .map(|c| c.target.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs"]
        );
        let planned =
            capture_with(&initial, &FocusScope::Files([scope].into()), &db, None).unwrap();
        assert_eq!(
            serde_json::to_value(&planned.candidates).unwrap(),
            serde_json::to_value(&first.candidates[..1]).unwrap(),
            "a plan-scoped inventory must reproduce the same evidence identities"
        );
    }

    #[test]
    fn effective_human_notes_are_distinct_from_heuristic_test_links() {
        let review = review(&[
            ("src/account.rs", "pub fn transfer() { write(balance); }"),
            (
                "tests/account.rs",
                "#[test]\nfn checks_transfer() { transfer(); }",
            ),
        ]);
        let source = review
            .tree
            .nodes()
            .iter()
            .find(|n| n.path.as_str() == "src/account.rs" && n.block.is_none())
            .unwrap();
        let mut record = Record::new(
            ReviewTargetRef::File {
                hash: source.hash.clone(),
            },
            ReviewCheck::review(),
            Verdict::Rejected,
            Identity::Email {
                email: "reviewer@example.com".into(),
            },
            RepoRef::Unknown,
            BlockState::Unknown,
        );
        record.path_hint = Some(source.path.clone());
        record.note = Some("Check the authorization before writing".into());
        let db = ReviewDatabase::from_records(vec![record.clone()]);
        let inventory = capture(&review, &db, None);
        let candidate = inventory
            .candidates
            .iter()
            .find(|c| c.target.path.as_str() == "src/account.rs")
            .unwrap();
        assert!(
            candidate
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::RelatedTest
                    && e.detail.contains("heuristic")
                    && e.detail.contains("not coverage"))
        );
        assert!(
            candidate
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::HumanReview
                    && e.detail.contains("rejected")
                    && e.detail.contains("authorization"))
        );
        record.note = Some("A different review note".into());
        let changed = capture(&review, &ReviewDatabase::from_records(vec![record]), None);
        assert_ne!(inventory.snapshot_hash, changed.snapshot_hash);
        let no_reviews = capture(&review, &ReviewDatabase::default(), None);
        assert!(
            no_reviews
                .candidates
                .iter()
                .find(|c| c.target.path.as_str() == "src/account.rs")
                .unwrap()
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::HumanReview && e.detail.contains("No effective"))
        );
    }

    #[test]
    fn missing_history_is_not_a_clean_history_and_real_patches_distinguish_mechanical_churn() {
        let root = temp_git_repo("focus-evidence-history");
        let repo = gix::open(&root).unwrap();
        let db = ReviewDatabase::default();
        let initial = "pub fn authorize(ok: bool) { if ok { write(); } }\n";
        let fixture = review(&[("auth.rs", initial)]);
        let absent = capture(&fixture, &db, None);
        let unborn = capture(&fixture, &db, Some(&repo));
        assert!(absent.diagnostics.iter().any(|d| d.kind == DiagnosticKind::GitHistory
            && d.message.contains("unavailable")));
        assert!(unborn.diagnostics.iter().any(|d| d.kind == DiagnosticKind::GitHistory
            && d.message.contains("unavailable")));
        std::fs::write(root.join("auth.rs"), initial).unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-qm", "initial"]);
        std::fs::write(root.join("auth.rs"), initial.replace("if ok", "if !ok")).unwrap();
        run_git(&root, &["commit", "-qam", "fix authorization"]);
        let substantive = capture(&fixture, &db, Some(&gix::open(&root).unwrap()));
        assert!(
            substantive.candidates[0]
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::GitPatch
                    && e.detail.contains("substantive")
                    && e.detail.contains("if !ok"))
        );
        let corrected = std::fs::read_to_string(root.join("auth.rs")).unwrap();
        std::fs::write(root.join("auth.rs"), corrected.replace("}\n", "}  \n")).unwrap();
        run_git(&root, &["commit", "-qam", "fix formatting"]);
        let mechanical = capture(&fixture, &db, Some(&gix::open(&root).unwrap()));
        assert!(
            mechanical.candidates[0]
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::GitPatch && e.detail.contains("mechanical"))
        );
        assert_ne!(substantive.snapshot_hash, mechanical.snapshot_hash);
        run_git(
            &root,
            &["commit", "--allow-empty", "-qm", "fix another critical bug"],
        );
        let empty = capture(&fixture, &db, Some(&gix::open(&root).unwrap()));
        assert!(
            !empty.candidates[0]
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::GitPatch
                    && e.detail.contains("fix another critical bug"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_and_source_bounds_are_reported_instead_of_silently_clean() {
        let root = temp_git_repo("focus-evidence-bounds");
        let huge = "x".repeat(usize::try_from(MAX_HISTORY_BLOB_BYTES).unwrap() + 1);
        std::fs::write(root.join("large.rs"), &huge).unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-qm", "large input"]);
        let fixture = review(&[(
            "large.rs",
            "pub fn parse(request: Request) { write(request); }\n",
        )]);
        let db = ReviewDatabase::default();
        let inventory = capture(&fixture, &db, Some(&gix::open(&root).unwrap()));
        assert!(
            !inventory.candidates[0]
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::GitPatch)
        );
        assert!(
            inventory.candidates[0]
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::HistoryStatus && e.detail.contains("truncated"))
        );
        for _ in 0..MAX_HISTORY_COMMITS {
            run_git(&root, &["commit", "--allow-empty", "-qm", "empty"]);
        }
        let inventory = capture(&fixture, &db, Some(&gix::open(&root).unwrap()));
        assert!(
            inventory
                .diagnostics
                .iter()
                .any(|d| d.message.contains("history truncated"))
        );
        let source = format!(
            "pub fn handle() {{\n{}\nstd::fs::write(path, request.body())?;\n}}",
            "// padding\n".repeat(MAX_SOURCE_BYTES / 10 + 1)
        );
        let inventory = capture(&review(&[("large.rs", &source)]), &db, None);
        assert!(inventory.candidates[0].excerpt.len() <= MAX_EXCERPT_BYTES);
        assert!(inventory.candidates[0].excerpt.contains("std::fs::write"));
        assert!(inventory.candidates[0].evidence.iter().any(|e| e.kind
            == EvidenceKind::SourceClassification
            && e.detail.contains("truncated")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn risky_tail_survives_many_noisy_files_and_excerpt_reaches_meaningful_body() {
        let names: Vec<_> = (0..300).map(|n| format!("vendor/noise{n:03}.rs")).collect();
        let source = format!(
            "pub fn accept(request: Request) {{\n{}\nlet token = request.headers();\nauthorize(token)?;\nstd::fs::write(path, request.body())?;\n}}\n",
            "// explanatory comment\n".repeat(80)
        );
        let mut sources: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), "fn boilerplate() {}\n"))
            .collect();
        sources.push(("zzz/security.rs", source.as_str()));
        let inventory = capture(&review(&sources), &ReviewDatabase::default(), None);
        let risky = inventory
            .candidates
            .iter()
            .find(|c| c.target.path.as_str() == "zzz/security.rs")
            .unwrap();
        assert!(risky.excerpt.contains("std::fs::write"));
        assert!(
            risky
                .evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::TrustBoundary)
        );
        assert!(risky.evidence.iter().any(|e| e.kind == EvidenceKind::Write));
        assert!(
            inventory
                .candidates
                .iter()
                .filter(|c| c.target.path.as_str().starts_with("vendor/"))
                .all(|c| c
                    .evidence
                    .iter()
                    .any(|e| e.kind == EvidenceKind::SourceClassification
                        && e.detail.contains("vendor")))
        );
    }
}
