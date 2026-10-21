//! Focused review passes: a reading-budgeted, explainable subset of a scope's
//! pending blocks.
//!
//! Selection is a pure function of candidates given in canonical review order.
//! A pass never records a verdict and never widens to unselected blocks:
//! everything it leaves out stays pending for a full review of the same scope.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;

use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::commands::review::{BlockChangeKind, DiffBlockSides};
use crate::focus::ReviewDuration;
use crate::focus_evidence::{EvidenceKind, SourceClass, signal as lexical_signal, source_class};
use crate::review_order::ReviewOrder;
use crate::tree::{Tree, TreeNode, TreeNodeId};

/// Which part of a scope's pending queue a review walks or lists.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPass {
    /// Every pending block, in canonical review order.
    #[default]
    Full,
    /// A reading-budgeted subset chosen by the AI review plan or explicit heuristics.
    Focused,
}

/// Default reading budget for a focused pass.
pub const DEFAULT_PASS_BUDGET: ReviewDuration = ReviewDuration::from_minutes(5);

/// Fixed per-card overhead, shared with onboarding and focus plans.
pub const CARD_OVERHEAD_SECONDS: usize = 10;
/// Reading cost per displayed line, shared with onboarding and focus plans.
pub const SECONDS_PER_LINE: usize = 2;
/// Unchanged lines read on each side of a changed region.
const CONTEXT_LINES: usize = 3;
/// Longest quoted source line in a signal explanation.
const SIGNAL_QUOTE_CHARS: usize = 60;
/// Lines inspected for a public declaration (attributes and doc lines precede it).
const SIGNATURE_LINES: usize = 4;

/// Reading estimate for a card that shows `lines` lines.
pub const fn reading_seconds(lines: usize) -> usize {
    CARD_OVERHEAD_SECONDS.saturating_add(lines.saturating_mul(SECONDS_PER_LINE))
}

/// Compact estimate label: `45s`, `5m`, `4m 30s`.
pub fn format_reading_estimate(seconds: usize) -> String {
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, seconds) => format!("{minutes}m {seconds}s"),
    }
}

/// Coarse source class. A heuristic pass draws only from the most consequential
/// non-empty class, so code is never padded with tests, prose, or generated churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CandidateClass {
    Code,
    Test,
    Prose,
    Generated,
}

impl CandidateClass {
    pub fn of(node: &TreeNode) -> Self {
        match source_class(node) {
            Some(SourceClass::Vendor | SourceClass::Generated) => Self::Generated,
            Some(SourceClass::Test) => Self::Test,
            None => match node.language {
                Some(Language::Markdown | Language::Text) => Self::Prose,
                _ => Self::Code,
            },
        }
    }
}

/// What a reviewer must read for a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateChange<'a> {
    /// No comparison (all-files scope): read the whole block.
    Unchanged,
    /// New block: read all of it.
    Added,
    /// Removed block: read all of it.
    Deleted,
    /// Changed block: read the changed region plus surrounding context.
    Changed { base: &'a str },
}

/// One pending, independently approvable block.
#[derive(Debug, Clone, Copy)]
pub struct PassCandidate<'a, Id> {
    pub id: Id,
    pub kind: BlockKind,
    pub class: CandidateClass,
    /// Displayed content: the head side when present, else the base side.
    pub content: &'a str,
    pub change: CandidateChange<'a>,
    pub complexity: Option<u32>,
}

/// The pending blocks of a collected review scope.
#[derive(Clone, Copy)]
pub struct PassScope<'a> {
    pub tree: &'a Tree,
    pub pending: &'a HashSet<TreeNodeId>,
    pub diff_block_sides: &'a HashMap<TreeNodeId, DiffBlockSides>,
    pub block_change_kinds: &'a HashMap<TreeNodeId, BlockChangeKind>,
}

impl<'a> PassScope<'a> {
    /// Pending blocks in canonical order. Containers with pending descendants
    /// are excluded because their approvals would cover blocks outside a pass.
    pub fn candidates(self) -> Vec<PassCandidate<'a, TreeNodeId>> {
        let canonical = ReviewOrder::from_tree(self.tree, self.pending);
        canonical
            .iter_node_ids()
            .filter(|&node_id| !self.has_pending_descendant(node_id))
            .filter_map(|node_id| {
                let node = self.tree.node(node_id);
                let (block, change) = self.block_and_change(node_id)?;
                Some(PassCandidate {
                    id: node_id,
                    kind: block.kind,
                    class: CandidateClass::of(node),
                    content: &block.content,
                    change,
                    complexity: block.complexity,
                })
            })
            .collect()
    }

    /// The reading estimate a pass charges for `node_id`, if it is a block.
    pub fn estimated_seconds(self, node_id: TreeNodeId) -> Option<usize> {
        let (block, change) = self.block_and_change(node_id)?;
        Some(reading_seconds(reading_lines(&block.content, change).read))
    }

    fn block_and_change(self, node_id: TreeNodeId) -> Option<(&'a Block, CandidateChange<'a>)> {
        let block = self.tree.node(node_id).block.as_ref()?;
        let base = self
            .diff_block_sides
            .get(&node_id)
            .and_then(|sides| sides.base.as_ref());
        let change = match (self.block_change_kinds.get(&node_id), base) {
            (None, _) => CandidateChange::Unchanged,
            (Some(BlockChangeKind::Added), _) | (Some(BlockChangeKind::Changed), None) => {
                CandidateChange::Added
            }
            (Some(BlockChangeKind::Deleted), _) => CandidateChange::Deleted,
            (Some(BlockChangeKind::Changed), Some(base)) => CandidateChange::Changed {
                base: &base.content,
            },
        };
        Some((block, change))
    }

    /// Whether approving `node_id` would also cover another pending block.
    pub fn has_pending_descendant(self, node_id: TreeNodeId) -> bool {
        let mut stack = self.tree.node(node_id).children.clone();
        while let Some(child) = stack.pop() {
            if self.pending.contains(&child) {
                return true;
            }
            stack.extend_from_slice(&self.tree.node(child).children);
        }
        false
    }
}

/// Who chose a focused pass's cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PassSource {
    AiPlan,
    Heuristic,
}

impl PassSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::AiPlan => "AI review plan",
            Self::Heuristic => "heuristics",
        }
    }
}

/// An explicit heuristic observation. Lexical signals are not findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassSignal {
    DataShape,
    PublicInterface,
    TrustBoundary(String),
    Writes(String),
    Concurrency(String),
    Complexity(u32),
    ChangedLines(usize),
}

impl PassSignal {
    fn weight(&self) -> usize {
        match self {
            Self::DataShape | Self::TrustBoundary(_) => 3,
            Self::PublicInterface | Self::Writes(_) | Self::Concurrency(_) => 2,
            Self::Complexity(score) => {
                usize::try_from(*score / 5).map_or(3, |weight| weight.min(3))
            }
            Self::ChangedLines(lines) => (lines / 10).min(2),
        }
    }

    /// Compact label for one-line summaries; `Display` adds the quoted source line.
    pub fn label(&self) -> String {
        match self {
            Self::DataShape => "data shape".to_string(),
            Self::PublicInterface => "public interface".to_string(),
            Self::TrustBoundary(_) => "trust boundary".to_string(),
            Self::Writes(_) => "writes".to_string(),
            Self::Concurrency(_) => "concurrency".to_string(),
            Self::Complexity(score) => format!("complexity {score}"),
            Self::ChangedLines(lines) => format!("{lines} changed lines"),
        }
    }
}

impl fmt::Display for PassSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataShape => f.write_str("data shape"),
            Self::PublicInterface => f.write_str("public interface"),
            Self::TrustBoundary(line) => write!(f, "trust boundary `{line}`"),
            Self::Writes(line) => write!(f, "writes `{line}`"),
            Self::Concurrency(line) => write!(f, "concurrency `{line}`"),
            Self::Complexity(score) => write!(f, "complexity {score}"),
            Self::ChangedLines(lines) => write!(f, "{lines} changed lines"),
        }
    }
}

/// Why a card is in the pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassWhy {
    Signals(Vec<PassSignal>),
    AiPlan { reason: String, checks: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassCard<Id> {
    pub id: Id,
    pub estimated_seconds: usize,
    pub why: PassWhy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassPlan<Id> {
    pub source: PassSource,
    pub budget: ReviewDuration,
    pub cards: Vec<PassCard<Id>>,
    /// Pending candidates left for a full review of the same scope.
    pub remainder: usize,
}

impl<Id> PassPlan<Id> {
    pub fn estimated_seconds(&self) -> usize {
        self.cards.iter().map(|card| card.estimated_seconds).sum()
    }
}

/// An AI-proposed priority, already resolved to a candidate id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiPick<Id> {
    pub id: Id,
    pub reason: String,
    pub checks: Vec<String>,
}

/// Ranks the most consequential non-empty class by review value per minute:
/// explicit signal weight per estimated reading second, so short data shapes and
/// interfaces outrank a long body with a similar score. Blocks without signals
/// only fill the remaining budget, in canonical order.
pub fn heuristic_pass<Id: Copy>(
    candidates: &[PassCandidate<'_, Id>],
    budget: ReviewDuration,
) -> PassPlan<Id> {
    let Some(class) = candidates.iter().map(|candidate| candidate.class).min() else {
        return PassPlan {
            source: PassSource::Heuristic,
            budget,
            cards: Vec::new(),
            remainder: 0,
        };
    };
    let mut ranked = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.class == class)
        .map(|(index, candidate)| {
            let reading = reading_lines(candidate.content, candidate.change);
            let signals = signals(candidate, reading.changed);
            let score = signals.iter().map(PassSignal::weight).sum::<usize>();
            let estimated_seconds = reading_seconds(reading.read);
            let rank = HeuristicRank {
                has_signal: score > 0,
                milli_score_per_second: score.saturating_mul(1000) / estimated_seconds,
                score,
                canonical_index: index,
            };
            (
                rank,
                PassCard {
                    id: candidate.id,
                    estimated_seconds,
                    why: PassWhy::Signals(signals),
                },
            )
        })
        .collect::<Vec<_>>();
    ranked.sort_unstable_by_key(|(rank, _)| *rank);
    let cards = fit_budget(ranked.into_iter().map(|(_, card)| card), budget);
    PassPlan {
        source: PassSource::Heuristic,
        budget,
        remainder: candidates.len() - cards.len(),
        cards,
    }
}

/// Sort key for heuristic cards; ascending order is best first. The canonical
/// index makes the order total and deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeuristicRank {
    has_signal: bool,
    milli_score_per_second: usize,
    score: usize,
    canonical_index: usize,
}

impl Ord for HeuristicRank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .has_signal
            .cmp(&self.has_signal)
            .then(
                other
                    .milli_score_per_second
                    .cmp(&self.milli_score_per_second),
            )
            .then(other.score.cmp(&self.score))
            .then(self.canonical_index.cmp(&other.canonical_index))
    }
}

impl PartialOrd for HeuristicRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Keeps the AI's order, ignoring picks that are not pending candidates, then
/// fits the reading budget.
pub fn ai_pass<Id: Copy + Eq + Hash>(
    candidates: &[PassCandidate<'_, Id>],
    picks: Vec<AiPick<Id>>,
    budget: ReviewDuration,
) -> PassPlan<Id> {
    let by_id = candidates
        .iter()
        .map(|candidate| (candidate.id, candidate))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::with_capacity(picks.len());
    let ordered = picks.into_iter().filter_map(|pick| {
        let candidate = by_id.get(&pick.id)?;
        seen.insert(pick.id).then(|| PassCard {
            id: pick.id,
            estimated_seconds: reading_seconds(
                reading_lines(candidate.content, candidate.change).read,
            ),
            why: PassWhy::AiPlan {
                reason: pick.reason,
                checks: pick.checks,
            },
        })
    });
    let cards = fit_budget(ordered, budget);
    PassPlan {
        source: PassSource::AiPlan,
        budget,
        remainder: candidates.len() - cards.len(),
        cards,
    }
}

/// Greedy in rank order: keep every card that still fits. Rather than return an
/// empty pass when nothing fits, keep the top-ranked card and let its estimate
/// show the overrun.
fn fit_budget<Id>(
    ranked: impl Iterator<Item = PassCard<Id>>,
    budget: ReviewDuration,
) -> Vec<PassCard<Id>> {
    let mut cards = Vec::new();
    let mut top = None;
    let mut remaining = budget.seconds();
    for card in ranked {
        if card.estimated_seconds <= remaining {
            remaining -= card.estimated_seconds;
            cards.push(card);
            if remaining < reading_seconds(1) {
                break;
            }
        } else if top.is_none() && cards.is_empty() {
            top = Some(card);
        }
    }
    if cards.is_empty() {
        cards.extend(top);
    }
    cards
}

struct ReadingLines {
    /// Lines a reviewer reads, including context around changed regions.
    read: usize,
    /// Added plus removed lines, when the scope compares two sides.
    changed: Option<usize>,
}

fn reading_lines(content: &str, change: CandidateChange<'_>) -> ReadingLines {
    let whole = content.lines().count().max(1);
    match change {
        CandidateChange::Unchanged => ReadingLines {
            read: whole,
            changed: None,
        },
        CandidateChange::Added | CandidateChange::Deleted => ReadingLines {
            read: whole,
            changed: Some(whole),
        },
        CandidateChange::Changed { base } => {
            let base = base.lines().collect::<Vec<_>>();
            let head = content.lines().collect::<Vec<_>>();
            let prefix = base
                .iter()
                .zip(&head)
                .take_while(|(left, right)| left == right)
                .count();
            let suffix = base
                .iter()
                .rev()
                .zip(head.iter().rev())
                .take(base.len().min(head.len()) - prefix)
                .take_while(|(left, right)| left == right)
                .count();
            let changed = (base.len() - prefix - suffix) + (head.len() - prefix - suffix);
            let context = prefix.min(CONTEXT_LINES) + suffix.min(CONTEXT_LINES);
            ReadingLines {
                read: (changed + context).max(1),
                changed: Some(changed),
            }
        }
    }
}

/// Every returned signal has a positive weight.
fn signals(candidate: &PassCandidate<'_, impl Copy>, changed: Option<usize>) -> Vec<PassSignal> {
    let mut signals = Vec::new();
    if matches!(
        candidate.kind,
        BlockKind::Struct
            | BlockKind::Enum
            | BlockKind::Type
            | BlockKind::Interface
            | BlockKind::Class
    ) {
        signals.push(PassSignal::DataShape);
    }
    if candidate
        .content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(SIGNATURE_LINES)
        .any(|line| lexical_signal(EvidenceKind::EntryPoint, line))
    {
        signals.push(PassSignal::PublicInterface);
    }
    let quoted = |kind| {
        candidate
            .content
            .lines()
            .find(|line| lexical_signal(kind, line))
            .map(|line| quote(line.trim()))
    };
    if let Some(line) = quoted(EvidenceKind::TrustBoundary) {
        signals.push(PassSignal::TrustBoundary(line));
    }
    if let Some(line) = quoted(EvidenceKind::Write) {
        signals.push(PassSignal::Writes(line));
    }
    if let Some(line) = quoted(EvidenceKind::Concurrency) {
        signals.push(PassSignal::Concurrency(line));
    }
    signals.extend(candidate.complexity.map(PassSignal::Complexity));
    signals.extend(changed.map(PassSignal::ChangedLines));
    signals.retain(|signal| signal.weight() > 0);
    signals
}

fn quote(line: &str) -> String {
    match line.char_indices().nth(SIGNAL_QUOTE_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{ByteSpan, LineSpan};
    use crate::tree::TreeBuilder;

    fn node(path: &str, language: Language, content: &str) -> TreeNode {
        let block = crate::block::Block::new(
            content.to_string(),
            BlockKind::Content,
            LineSpan::new(0, content.lines().count()),
            ByteSpan::new(0, content.len()),
        );
        let mut builder = TreeBuilder::new();
        let root = builder.root();
        let file = builder.add_file(root, path.to_string(), path, block.hash.clone(), language);
        let id = builder.add_block(file, "block".to_string(), path, block, language);
        builder.finalize().node(id).clone()
    }

    #[test]
    fn candidate_classes_separate_code_from_tests_prose_and_generated_sources() {
        let class = |path, language| CandidateClass::of(&node(path, language, "x = 1\n"));
        assert_eq!(class("src/lib.rs", Language::Rust), CandidateClass::Code);
        assert_eq!(class("Cargo.toml", Language::Toml), CandidateClass::Code);
        assert_eq!(class("tests/e2e.rs", Language::Rust), CandidateClass::Test);
        assert_eq!(
            class("README.md", Language::Markdown),
            CandidateClass::Prose
        );
        assert_eq!(
            class("Cargo.lock", Language::Toml),
            CandidateClass::Generated
        );
        assert_eq!(
            class("web/package-lock.json", Language::Json),
            CandidateClass::Generated
        );
        assert_eq!(
            class("vendor/lib.rs", Language::Rust),
            CandidateClass::Generated
        );
    }

    fn candidate(id: usize, kind: BlockKind, content: &str) -> PassCandidate<'_, usize> {
        PassCandidate {
            id,
            kind,
            class: CandidateClass::Code,
            content,
            change: CandidateChange::Unchanged,
            complexity: None,
        }
    }

    fn lines(count: usize) -> String {
        (0..count)
            .map(|line| format!("    step_{line}();\n"))
            .collect()
    }

    fn ids<Id: Copy>(plan: &PassPlan<Id>) -> Vec<Id> {
        plan.cards.iter().map(|card| card.id).collect()
    }

    fn budget(seconds: usize) -> ReviewDuration {
        ReviewDuration::from_seconds(seconds).unwrap()
    }

    #[test]
    fn heuristic_pass_ranks_explicit_signals_and_keeps_canonical_ties() {
        let helper = "fn helper() {\n    value + 1\n}\n";
        let shell = "pub fn run(repo: &Path) -> Result<()> {\n    let out = Command::new(\"git\").output()?;\n    fs::write(path, out.stdout)?;\n}\n";
        let shape = "pub struct Config {\n    name: String,\n}\n";
        let other_helper = "fn other() {\n    value - 1\n}\n";
        let candidates = [
            candidate(0, BlockKind::Function, helper),
            candidate(1, BlockKind::Function, shell),
            candidate(2, BlockKind::Struct, shape),
            candidate(3, BlockKind::Function, other_helper),
        ];

        let plan = heuristic_pass(&candidates, budget(300));

        assert_eq!(plan.source, PassSource::Heuristic);
        assert_eq!(ids(&plan), vec![1, 2, 0, 3]);
        assert_eq!(plan.remainder, 0);
        assert_eq!(
            plan.cards[0].why,
            PassWhy::Signals(vec![
                PassSignal::PublicInterface,
                PassSignal::TrustBoundary("let out = Command::new(\"git\").output()?;".to_string()),
                PassSignal::Writes("fs::write(path, out.stdout)?;".to_string()),
            ])
        );
        assert_eq!(
            plan.cards[1].why,
            PassWhy::Signals(vec![PassSignal::DataShape])
        );
        assert_eq!(plan.cards[2].why, PassWhy::Signals(Vec::new()));
    }

    #[test]
    fn heuristic_pass_fits_the_reading_budget_and_reports_the_remainder() {
        let big = lines(100);
        let small = lines(5);
        let mut risky = candidate(0, BlockKind::Function, &big);
        risky.complexity = Some(20);
        let candidates = [
            risky,
            candidate(1, BlockKind::Function, &small),
            candidate(2, BlockKind::Function, &small),
            candidate(3, BlockKind::Function, &small),
        ];

        // 100 lines cost 210s; each 5-line block costs 20s.
        let plan = heuristic_pass(&candidates, budget(240));

        assert_eq!(ids(&plan), vec![0, 1]);
        assert_eq!(plan.estimated_seconds(), 230);
        assert_eq!(plan.remainder, 2);
        assert!(plan.estimated_seconds() <= plan.budget.seconds());
    }

    #[test]
    fn heuristic_pass_skips_cards_that_do_not_fit_but_keeps_scanning() {
        let big = lines(100);
        let small = lines(5);
        let mut risky = candidate(0, BlockKind::Function, &big);
        risky.complexity = Some(20);
        let candidates = [risky, candidate(1, BlockKind::Function, &small)];

        let plan = heuristic_pass(&candidates, budget(60));

        assert_eq!(ids(&plan), vec![1]);
        assert_eq!(plan.remainder, 1);
    }

    #[test]
    fn heuristic_pass_prefers_review_value_per_minute_over_raw_signal_volume() {
        // A long risky function scores higher than each short data shape, but
        // three shapes deliver more signal per minute than one long body.
        let long_body = format!(
            "pub fn sync(dir: &Path) {{\n    Command::new(\"git\").output();\n{}}}\n",
            lines(60)
        );
        let mut long = candidate(0, BlockKind::Function, &long_body);
        long.complexity = Some(20);
        let shape = "pub struct Config {\n    name: String,\n}\n";
        let candidates = [
            long,
            candidate(1, BlockKind::Struct, shape),
            candidate(2, BlockKind::Enum, shape),
            candidate(3, BlockKind::Struct, shape),
        ];

        let plan = heuristic_pass(&candidates, budget(150));

        assert_eq!(ids(&plan), vec![1, 2, 3]);
        assert_eq!(plan.remainder, 1);
    }

    #[test]
    fn heuristic_pass_keeps_one_oversized_top_card_instead_of_an_empty_pass() {
        let big = lines(100);
        let bigger = lines(200);
        let candidates = [
            candidate(0, BlockKind::Function, &big),
            candidate(1, BlockKind::Struct, &bigger),
        ];

        let plan = heuristic_pass(&candidates, budget(60));

        assert_eq!(ids(&plan), vec![1]);
        assert!(plan.estimated_seconds() > plan.budget.seconds());
        assert_eq!(plan.remainder, 1);
    }

    #[test]
    fn heuristic_pass_never_pads_code_with_tests_prose_or_generated_blocks() {
        let code = "fn change() {}\n";
        let mut test = candidate(1, BlockKind::Function, "fn test_change() {}\n");
        test.class = CandidateClass::Test;
        let mut prose = candidate(2, BlockKind::Paragraph, "Docs.\n");
        prose.class = CandidateClass::Prose;
        let mut lock = candidate(3, BlockKind::Content, "[[package]]\n");
        lock.class = CandidateClass::Generated;
        let candidates = [candidate(0, BlockKind::Function, code), test, prose, lock];

        let plan = heuristic_pass(&candidates, budget(300));
        assert_eq!(ids(&plan), vec![0]);
        assert_eq!(plan.remainder, 3);

        let without_code = [test, prose, lock];
        assert_eq!(ids(&heuristic_pass(&without_code, budget(300))), vec![1]);
        let docs_and_lock = [lock, prose];
        assert_eq!(ids(&heuristic_pass(&docs_and_lock, budget(300))), vec![2]);
    }

    #[test]
    fn heuristic_pass_estimates_changed_blocks_by_changed_region_and_context() {
        let base = lines(200);
        let head = base.replacen("    step_100();\n", "    step_100(changed);\n", 1);
        let mut changed = candidate(0, BlockKind::Function, &head);
        changed.change = CandidateChange::Changed { base: &base };
        let mut added = candidate(1, BlockKind::Function, &head);
        added.change = CandidateChange::Added;

        let plan = heuristic_pass(&[changed, added], budget(3600));

        // One removed + one added line, three context lines on each side.
        assert_eq!(plan.cards[0].id, 1);
        assert_eq!(plan.cards[1].estimated_seconds, reading_seconds(8));
        assert_eq!(plan.cards[0].estimated_seconds, reading_seconds(200));
        assert_eq!(
            plan.cards[0].why,
            PassWhy::Signals(vec![PassSignal::ChangedLines(200)])
        );
    }

    #[test]
    fn heuristic_pass_of_nothing_is_empty() {
        let plan = heuristic_pass::<usize>(&[], budget(300));
        assert!(plan.cards.is_empty());
        assert_eq!(plan.remainder, 0);
    }

    #[test]
    fn ai_pass_keeps_model_order_drops_unknown_and_duplicate_picks_and_fits_budget() {
        let small = lines(5);
        let big = lines(200);
        let candidates = [
            candidate(0, BlockKind::Function, &small),
            candidate(1, BlockKind::Function, &big),
            candidate(2, BlockKind::Function, &small),
            candidate(3, BlockKind::Function, &small),
        ];
        let pick = |id: usize| AiPick {
            id,
            reason: format!("reason {id}"),
            checks: vec![format!("check {id}")],
        };

        let plan = ai_pass(
            &candidates,
            vec![pick(2), pick(9), pick(1), pick(2), pick(0)],
            budget(60),
        );

        assert_eq!(plan.source, PassSource::AiPlan);
        assert_eq!(ids(&plan), vec![2, 0]);
        assert_eq!(plan.remainder, 2);
        assert_eq!(
            plan.cards[0].why,
            PassWhy::AiPlan {
                reason: "reason 2".to_string(),
                checks: vec!["check 2".to_string()],
            }
        );
    }

    #[test]
    fn ai_pass_without_usable_picks_is_empty() {
        let small = lines(5);
        let candidates = [candidate(0, BlockKind::Function, &small)];
        let plan = ai_pass(
            &candidates,
            vec![AiPick {
                id: 7,
                reason: "invented".to_string(),
                checks: Vec::new(),
            }],
            budget(300),
        );
        assert!(plan.cards.is_empty());
        assert_eq!(plan.remainder, 1);
    }

    #[test]
    fn reading_estimates_format_compactly() {
        assert_eq!(format_reading_estimate(45), "45s");
        assert_eq!(format_reading_estimate(300), "5m");
        assert_eq!(format_reading_estimate(270), "4m 30s");
    }
}
