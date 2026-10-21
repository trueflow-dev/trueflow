use crate::analysis::Language;
use crate::block::BlockKind;
use crate::config::{AiConfig, AiProviderConfig};
use crate::hashing::TreeHash;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
pub const DEFAULT_AI_RESPONSE_CHAR_LIMIT: usize = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiProvider {
    Anthropic,
    OpenAi,
    ClaudeCli,
    CodexCli,
}

impl AiProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::ClaudeCli => "Claude CLI",
            Self::CodexCli => "Codex CLI",
        }
    }

    /// Direct API providers are detected from their keys but have no client
    /// yet, so only the CLI providers can serve plans and hints.
    const fn is_implemented(self) -> bool {
        match self {
            Self::ClaudeCli | Self::CodexCli => true,
            Self::Anthropic | Self::OpenAi => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiAvailability {
    Disabled {
        detected: Vec<AiProvider>,
    },
    Ready {
        provider: AiProvider,
        model: String,
    },
    Unavailable {
        reason: String,
        detected: Vec<AiProvider>,
    },
}

impl AiAvailability {
    pub fn modeline_text(&self) -> String {
        match self {
            Self::Disabled { detected } if detected.is_empty() => "AI: off".to_string(),
            Self::Disabled { detected } => format!(
                "AI: off ({} detected; set [ai].mode = \"review_plan\")",
                provider_list(detected)
            ),
            Self::Ready { provider, model } if model == "auto" => {
                format!("AI: ready ({})", provider.label())
            }
            Self::Ready { provider, model } => {
                format!("AI: ready ({} / {model})", provider.label())
            }
            Self::Unavailable { reason, .. } => format!("AI: unavailable ({reason})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AiSuggestionKey {
    pub provider: AiProvider,
    pub model: String,
    pub review_set_hash: TreeHash,
    pub path: String,
    pub block_hash: TreeHash,
    pub start_line: usize,
    pub end_line: usize,
    pub max_context_lines: usize,
    pub max_response_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewSetContext {
    pub review_set_hash: TreeHash,
    pub overview: String,
}

impl AiReviewSetContext {
    pub fn new(review_set_hash: TreeHash, overview: String) -> Self {
        Self {
            review_set_hash,
            overview,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewContext {
    pub path: String,
    pub language: Language,
    pub block_kind: BlockKind,
    pub block_hash: TreeHash,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSuggestionRequest {
    pub key: AiSuggestionKey,
    pub review_set: AiReviewSetContext,
    pub context: AiReviewContext,
    pub prompt: String,
}

impl AiSuggestionRequest {
    pub fn new(
        provider: AiProvider,
        model: String,
        review_set: AiReviewSetContext,
        context: AiReviewContext,
        max_context_lines: usize,
    ) -> Self {
        Self::with_response_char_limit(
            provider,
            model,
            review_set,
            context,
            max_context_lines,
            DEFAULT_AI_RESPONSE_CHAR_LIMIT,
        )
    }

    pub fn with_response_char_limit(
        provider: AiProvider,
        model: String,
        review_set: AiReviewSetContext,
        context: AiReviewContext,
        max_context_lines: usize,
        max_response_chars: usize,
    ) -> Self {
        let key = AiSuggestionKey {
            provider,
            model,
            review_set_hash: review_set.review_set_hash.clone(),
            path: context.path.clone(),
            block_hash: context.block_hash.clone(),
            start_line: context.start_line,
            end_line: context.end_line,
            max_context_lines,
            max_response_chars,
        };
        let prompt = build_review_block_prompt(
            &context,
            Some(&review_set.review_set_hash),
            max_context_lines,
            max_response_chars,
        );
        Self {
            key,
            review_set,
            context,
            prompt,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewPlanBlock {
    pub block_id: String,
    pub is_test: bool,
    pub path: String,
    pub language: Language,
    pub block_kind: BlockKind,
    pub block_hash: TreeHash,
    pub start_line: usize,
    pub end_line: usize,
    /// Reading estimate a focused pass would charge for this block.
    pub estimated_seconds: usize,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewPlanCoverage {
    pub total_blocks: usize,
    pub presented_blocks: usize,
    pub excerpted_blocks: usize,
    pub excerpt_lines: usize,
    pub fully_excerpted_blocks: usize,
}

impl AiReviewPlanCoverage {
    pub const fn is_partial(&self) -> bool {
        self.presented_blocks < self.total_blocks
            || self.fully_excerpted_blocks < self.presented_blocks
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewPlanRequest {
    pub review_set_hash: TreeHash,
    pub coverage: AiReviewPlanCoverage,
    pub prompt: String,
}

impl AiReviewPlanRequest {
    pub fn new(
        review_set_hash: TreeHash,
        scope_label: &str,
        blocks: &[AiReviewPlanBlock],
        max_context_lines: usize,
        focused_pass_budget_seconds: usize,
    ) -> Self {
        let selected = review_plan_manifest_indices(blocks, 120);
        let sources = selected
            .iter()
            .map(|&index| PlanSource::new(&blocks[index].content))
            .collect::<Vec<_>>();
        let mut allowances = vec![0; selected.len()];
        let mut remaining = max_context_lines;
        // Water-fill: every presented block gets a line before any gets a second.
        for depth in 0..12 {
            let eligible = sources
                .iter()
                .enumerate()
                .filter_map(|(index, source)| (source.line_count() > depth).then_some(index))
                .collect::<Vec<_>>();
            let count = remaining.min(eligible.len());
            for slot in 0..count {
                let offset = if count <= 1 {
                    eligible.len() - 1
                } else {
                    slot * (eligible.len() - 1) / (count - 1)
                };
                allowances[eligible[offset]] += 1;
            }
            remaining -= count;
            if remaining == 0 {
                break;
            }
        }
        let mut coverage = AiReviewPlanCoverage {
            total_blocks: blocks.len(),
            presented_blocks: selected.len(),
            excerpted_blocks: 0,
            excerpt_lines: max_context_lines - remaining,
            fully_excerpted_blocks: 0,
        };
        let presented = selected
            .iter()
            .zip(sources.iter())
            .zip(allowances)
            .map(|((&index, source), allowance)| {
                coverage.excerpted_blocks += usize::from(allowance > 0);
                coverage.fully_excerpted_blocks += usize::from(allowance == source.line_count());
                (
                    &blocks[index],
                    format!("B{:04}", index + 1),
                    source.excerpt(allowance),
                )
            })
            .collect::<Vec<_>>();
        let prompt = build_review_plan_prompt(
            &review_set_hash,
            scope_label,
            &coverage,
            &presented,
            focused_pass_budget_seconds,
        );

        Self {
            review_set_hash,
            coverage,
            prompt,
        }
    }
}

pub(crate) fn review_plan_manifest_indices(
    blocks: &[AiReviewPlanBlock],
    limit: usize,
) -> Vec<usize> {
    let mut groups: [Vec<Vec<usize>>; 2] = [Vec::new(), Vec::new()];
    let mut files = HashMap::new();
    let mut totals = [0usize; 2];
    for (index, block) in blocks.iter().enumerate() {
        let group = usize::from(block.is_test);
        let file = *files
            .entry((group, block.path.as_str()))
            .or_insert_with(|| {
                groups[group].push(Vec::new());
                groups[group].len() - 1
            });
        groups[group][file].push(index);
        totals[group] += 1;
    }
    let mut quotas = [0usize; 2];
    for _ in 0..limit.min(blocks.len()) {
        let group = if quotas[0] < totals[0] && (quotas[0] <= quotas[1] || quotas[1] == totals[1]) {
            0
        } else {
            1
        };
        quotas[group] += 1;
    }
    let mut selections: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    for group in 0..2 {
        let mut depth = 0;
        while selections[group].len() < quotas[group] {
            let round = groups[group]
                .iter()
                .filter_map(|file| file.get(depth).copied())
                .collect::<Vec<_>>();
            let count = round.len().min(quotas[group] - selections[group].len());
            for slot in 0..count {
                // Sample the whole file range, not a prefix, when the manifest is full.
                let offset = if count <= 1 {
                    round.len() - 1
                } else {
                    slot * (round.len() - 1) / (count - 1)
                };
                selections[group].push(round[offset]);
            }
            depth += 1;
        }
    }
    let mut selected = Vec::with_capacity(limit.min(blocks.len()));
    for index in 0..selections[0].len().max(selections[1].len()) {
        for group in &selections {
            if let Some(&block) = group.get(index) {
                selected.push(block);
            }
        }
    }
    selected
}

/// Block source split into base and head sides so a line budget keeps both.
pub(crate) struct PlanSource<'a> {
    base: Option<Vec<&'a str>>,
    head: Vec<&'a str>,
    single_side_label: Option<&'static str>,
}

impl<'a> PlanSource<'a> {
    pub(crate) fn new(content: &'a str) -> Self {
        if let Some((base, head)) = content
            .strip_prefix("[base]\n")
            .and_then(|text| text.split_once("[head]\n"))
        {
            Self {
                base: Some(base.lines().collect()),
                head: head.lines().collect(),
                single_side_label: None,
            }
        } else if let Some(head) = content.strip_prefix("[head]\n") {
            Self {
                base: None,
                head: head.lines().collect(),
                single_side_label: Some("head"),
            }
        } else if let Some(base) = content.strip_prefix("[base]\n") {
            Self {
                base: None,
                head: base.lines().collect(),
                single_side_label: Some("base"),
            }
        } else {
            Self {
                base: None,
                head: content.lines().collect(),
                single_side_label: None,
            }
        }
    }

    pub(crate) fn line_count(&self) -> usize {
        self.head.len() + self.base.as_ref().map_or(0, Vec::len)
    }

    pub(crate) fn excerpt(&self, allowance: usize) -> Vec<String> {
        if allowance == 0 {
            return Vec::new();
        }
        let Some(base) = &self.base else {
            let mut lines = self
                .head
                .iter()
                .take(allowance)
                .map(|line| (*line).to_string())
                .collect::<Vec<_>>();
            if let Some(label) = self.single_side_label {
                lines.insert(0, format!("[{label}]"));
            }
            if allowance < self.head.len() {
                lines.push("[source partial; remaining lines omitted]".to_string());
            }
            return lines;
        };
        let head_count = self.head.len().min(allowance.div_ceil(2));
        let base_count = base.len().min(allowance - head_count);
        let head_count = self.head.len().min(allowance - base_count);
        let change = base
            .iter()
            .zip(&self.head)
            .take_while(|(old, new)| old == new)
            .count();
        let mut lines = Vec::new();
        for (label, source, count) in [("base", base, base_count), ("head", &self.head, head_count)]
        {
            lines.push(format!("[{label}]"));
            if source.is_empty() {
                lines.push(format!("[{label} empty]"));
            } else if count == 0 {
                lines.push(format!("[{label} omitted by context budget]"));
            } else {
                let start = change.saturating_sub(count / 3).min(source.len() - count);
                if start > 0 {
                    lines.push(format!(
                        "[{label} partial; {start} preceding lines omitted]"
                    ));
                }
                lines.extend(
                    source[start..start + count]
                        .iter()
                        .map(|line| (*line).to_string()),
                );
                if start + count < source.len() {
                    lines.push(format!("[{label} partial; remaining lines omitted]"));
                }
            }
        }
        lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewPlan {
    pub summary: String,
    pub global_checks: Vec<String>,
    pub priority_blocks: Vec<AiReviewPlanPriority>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewPlanPriority {
    pub block_id: String,
    pub reason: String,
    pub checks: Vec<String>,
}

impl AiReviewPlan {
    pub fn from_provider_text(raw: &str) -> Result<Self> {
        let wire: AiReviewPlanWire = serde_json::from_str(raw.trim())
            .map_err(|error| anyhow!("AI review plan must be exactly one JSON object: {error}"))?;
        let summary = normalize_plan_value(&wire.summary, 240);
        if summary.is_empty() {
            return Err(anyhow!("AI review plan returned a blank summary"));
        }

        let mut global_checks = Vec::new();
        for check in wire.global_checks {
            let check = normalize_plan_value(&check, 160);
            if !check.is_empty() && !global_checks.contains(&check) {
                global_checks.push(check);
            }
            if global_checks.len() == 6 {
                break;
            }
        }

        let mut priority_blocks = Vec::new();
        for priority in wire.priority_blocks {
            let block_id = normalize_plan_value(&priority.block_id, 160);
            let reason = normalize_plan_value(&priority.reason, 160);
            if block_id.is_empty() || reason.is_empty() {
                continue;
            }
            let mut checks = Vec::new();
            for check in priority.checks {
                let check = normalize_plan_value(&check, 160);
                if !check.is_empty() && !checks.contains(&check) {
                    checks.push(check);
                }
                if checks.len() == 3 {
                    break;
                }
            }
            priority_blocks.push(AiReviewPlanPriority {
                block_id,
                reason,
                checks,
            });
            if priority_blocks.len() == 8 {
                break;
            }
        }

        Ok(Self {
            summary,
            global_checks,
            priority_blocks,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AiReviewPlanWire {
    summary: String,
    global_checks: Vec<String>,
    priority_blocks: Vec<AiReviewPlanPriorityWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AiReviewPlanPriorityWire {
    block_id: String,
    reason: String,
    checks: Vec<String>,
}

fn normalize_plan_value(raw: &str, max_chars: usize) -> String {
    truncate_for_modeline(&collapse_whitespace(raw), max_chars)
}

fn build_review_plan_prompt(
    review_set_hash: &TreeHash,
    scope_label: &str,
    coverage: &AiReviewPlanCoverage,
    presented: &[(&AiReviewPlanBlock, String, Vec<String>)],
    focused_pass_budget_seconds: usize,
) -> String {
    use crate::review_pass::format_reading_estimate;

    let suffix = if coverage.is_partial() {
        " (partial)"
    } else {
        " (all)"
    };
    let mut manifest = String::new();
    for (block, block_id, excerpt) in presented {
        let line_start = block.start_line.saturating_add(1);
        let line_end = block.end_line.max(block.start_line.saturating_add(1));
        manifest.push_str(&format!(
            "\n- {}\n  Path: {}\n  Language: {:?}\n  Block kind: {}\n  Block hash: {}\n  Lines: {line_start}-{line_end}\n  Reading estimate: ~{}\n",
            block_id,
            block.path,
            block.language,
            block.block_kind.as_str(),
            block.block_hash,
            format_reading_estimate(block.estimated_seconds),
        ));
        if excerpt.is_empty() {
            manifest.push_str("  Excerpt: (excerpt omitted by context budget)\n");
        } else {
            manifest.push_str("  Excerpt:\n```\n");
            for line in excerpt {
                manifest.push_str(line);
                manifest.push('\n');
            }
            manifest.push_str("```\n");
        }
    }

    format!(
        "Prepare a targeted review briefing using only the supplied context.\n\
Review-set hash: {review_set_hash}\n\
Scope: {scope_label}\n\
Context: {presented}/{total} blocks listed{suffix}; {excerpt_lines} excerpt lines across {excerpted} blocks\n\
Manifest:{manifest}\n\
Return exactly one JSON object with no Markdown and exactly these keys:\n\
{{\n\
  \"summary\": \"one concise review briefing\",\n\
  \"global_checks\": [\"cross-cutting invariant to verify\"],\n\
  \"priority_blocks\": [\n\
    {{\n\
      \"block_id\": \"B0001\",\n\
      \"reason\": \"why this block should be reviewed early\",\n\
      \"checks\": [\"specific question or invariant for this block\"]\n\
    }}\n\
  ]\n\
}}\n\
Use only the supplied context. Do not run tools or inspect other files. Copy block IDs verbatim.\n\
Return at most eight priority blocks. Avoid style-only or speculative advice.\n\
A focused pass reads only the priority blocks, in your order, within about {budget}; prefer a coherent set whose reading estimates fit, most consequential first.\n\
Do not include Markdown fences or any text outside the JSON object.",
        budget = format_reading_estimate(focused_pass_budget_seconds),
        presented = coverage.presented_blocks,
        total = coverage.total_blocks,
        excerpt_lines = coverage.excerpt_lines,
        excerpted = coverage.excerpted_blocks,
    )
}

pub trait AiReviewPlanProvider: Send + Sync {
    fn plan(
        &self,
        request: &AiReviewPlanRequest,
        options: &AiExecutionOptions,
    ) -> Result<AiReviewPlan>;
}

#[derive(Debug, Clone)]
pub struct CommandAiReviewPlanProvider {
    provider: AiProvider,
    model: String,
}

impl CommandAiReviewPlanProvider {
    pub fn new(provider: AiProvider, model: String) -> Result<Self> {
        if !matches!(provider, AiProvider::ClaudeCli | AiProvider::CodexCli) {
            return Err(anyhow!(
                "{} does not have a CLI review-plan provider",
                provider.label()
            ));
        }
        Ok(Self { provider, model })
    }

    pub fn generate_with_options(
        &self,
        prompt: &str,
        options: &AiExecutionOptions,
    ) -> Result<String> {
        let invocation =
            cli_invocation_for_generation(self.provider, &self.model, prompt.to_string())?;
        Ok(run_cli_invocation_with_options(&invocation, options)?.text)
    }

    pub fn generate_from_evidence(&self, prompt: &str, timeout: Duration) -> Result<String> {
        self.generate_from_evidence_with_options(
            prompt,
            &AiExecutionOptions {
                timeout,
                ..Default::default()
            },
        )
    }

    pub fn generate_from_evidence_with_options(
        &self,
        prompt: &str,
        options: &AiExecutionOptions,
    ) -> Result<String> {
        let mut invocation =
            cli_invocation_for_generation(self.provider, &self.model, prompt.to_string())?;
        restrict_evidence_generation_tools(self.provider, &mut invocation)?;
        Ok(run_cli_invocation_with_options(&invocation, options)?.text)
    }
}

fn restrict_evidence_generation_tools(
    provider: AiProvider,
    invocation: &mut AiCliInvocation,
) -> Result<()> {
    match provider {
        AiProvider::ClaudeCli => {
            // Official print-mode controls: no built-ins, configured MCP servers,
            // customizations, slash commands, or browser integration.
            invocation.args.extend(
                [
                    "--tools",
                    "",
                    "--strict-mcp-config",
                    "--safe-mode",
                    "--disable-slash-commands",
                    "--no-chrome",
                ]
                .map(str::to_string),
            );
            Ok(())
        }
        AiProvider::CodexCli => {
            // Explicitly weaker than Claude's tools-off mode: retain the existing
            // read-only sandbox and remove supported execution/integration features.
            // Ignore user configuration so MCP servers, hooks and rules cannot widen it.
            invocation.args.extend(
                [
                    "--ignore-user-config",
                    "--ignore-rules",
                    "--ephemeral",
                    "--strict-config",
                    "-c",
                    "web_search=\"disabled\"",
                ]
                .map(str::to_string),
            );
            for feature in [
                "shell_tool",
                "unified_exec",
                "apps",
                "plugins",
                "hooks",
                "multi_agent",
                "multi_agent_v2",
                "browser_use",
                "browser_use_external",
                "computer_use",
                "code_mode",
                "code_mode_host",
                "image_generation",
                "skill_mcp_dependency_install",
                "skill_search",
                "workspace_dependencies",
            ] {
                invocation
                    .args
                    .extend(["--disable".to_string(), feature.to_string()]);
            }
            Ok(())
        }
        AiProvider::Anthropic | AiProvider::OpenAi => Err(anyhow!(
            "{} has no tool-free CLI provider",
            provider.label()
        )),
    }
}

impl AiReviewPlanProvider for CommandAiReviewPlanProvider {
    fn plan(
        &self,
        request: &AiReviewPlanRequest,
        options: &AiExecutionOptions,
    ) -> Result<AiReviewPlan> {
        AiReviewPlan::from_provider_text(&self.generate_with_options(&request.prompt, options)?)
    }
}

const DEFAULT_LGTM_EXPLANATION: &str = "No change suggested; LGTM.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSuggestion {
    pub explanation: Option<String>,
    pub proposed_change: Option<String>,
}

impl AiSuggestion {
    pub fn from_provider_text(raw: &str) -> Result<Self> {
        let collapsed = collapse_whitespace(raw);
        if collapsed.is_empty() {
            return Err(anyhow!("AI provider returned an empty suggestion"));
        }

        let parsed = parse_structured_provider_text(raw);
        let explanation = parsed
            .explanation
            .as_deref()
            .and_then(normalize_optional_sentence);
        let mut proposed_change = parsed.change.as_deref().and_then(normalize_proposed_change);

        if parsed.explanation.is_none() && parsed.change.is_none() {
            proposed_change = normalize_unstructured_proposed_change(&collapsed);
        }

        Ok(Self {
            explanation,
            proposed_change,
        })
    }

    pub fn visible_sentence(&self) -> Option<&str> {
        self.proposed_change
            .as_deref()
            .or(self.explanation.as_deref())
            .or(Some(DEFAULT_LGTM_EXPLANATION))
    }

    pub fn proposed_change_sentence(&self) -> Option<&str> {
        self.proposed_change.as_deref()
    }
}

pub trait AiSuggestionProvider: Send + Sync {
    fn suggest(&self, request: &AiSuggestionRequest) -> Result<AiSuggestion>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiCliOutputFormat {
    Text,
    CodexJson,
    ClaudeJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCliInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: String,
    pub final_message_path: Option<PathBuf>,
    pub output_format: AiCliOutputFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AiConversationKey {
    provider: AiProvider,
    model: String,
    review_set_hash: TreeHash,
}

impl AiConversationKey {
    fn from_request(provider: AiProvider, model: &str, request: &AiSuggestionRequest) -> Self {
        Self {
            provider,
            model: model.to_string(),
            review_set_hash: request.key.review_set_hash.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AiConversationState {
    session_id: String,
}

#[derive(Debug, Clone)]
pub struct CommandAiSuggestionProvider {
    provider: AiProvider,
    model: String,
    conversations: Arc<Mutex<HashMap<AiConversationKey, AiConversationState>>>,
    cancellation: AiCancellationToken,
}

impl CommandAiSuggestionProvider {
    pub fn new(provider: AiProvider, model: String) -> Result<Self> {
        if !matches!(provider, AiProvider::ClaudeCli | AiProvider::CodexCli) {
            return Err(anyhow!(
                "{} does not have a CLI suggestion provider",
                provider.label()
            ));
        }
        Ok(Self {
            provider,
            model,
            conversations: Arc::new(Mutex::new(HashMap::new())),
            cancellation: AiCancellationToken::default(),
        })
    }

    pub fn with_cancellation(mut self, cancellation: AiCancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    fn cached_session_id(&self, key: &AiConversationKey) -> Option<String> {
        self.conversations
            .lock()
            .ok()
            .and_then(|cache| cache.get(key).map(|state| state.session_id.clone()))
    }

    fn record_session_id(&self, key: AiConversationKey, session_id: String) {
        if session_id.trim().is_empty() {
            return;
        }
        if let Ok(mut cache) = self.conversations.lock() {
            cache.insert(key, AiConversationState { session_id });
        }
    }

    fn forget_session_id(&self, key: &AiConversationKey) {
        if let Ok(mut cache) = self.conversations.lock() {
            cache.remove(key);
        }
    }
}

impl AiSuggestionProvider for CommandAiSuggestionProvider {
    fn suggest(&self, request: &AiSuggestionRequest) -> Result<AiSuggestion> {
        let started = Instant::now();
        let mut options = AiExecutionOptions {
            cancellation: self.cancellation.clone(),
            ..Default::default()
        };
        let conversation_key = AiConversationKey::from_request(self.provider, &self.model, request);
        let cached_session_id = self.cached_session_id(&conversation_key);
        let invocation = cli_invocation_for_request_with_session(
            self.provider,
            &self.model,
            request,
            cached_session_id.as_deref(),
        )?;
        let output = match run_cli_invocation_with_options(&invocation, &options) {
            Ok(output) => output,
            Err(error)
                if cached_session_id.is_some()
                    && error.downcast_ref::<AiExecutionStopped>().is_none() =>
            {
                self.forget_session_id(&conversation_key);
                let fresh_invocation = cli_invocation_for_request_with_session(
                    self.provider,
                    &self.model,
                    request,
                    None,
                )?;
                options.timeout = options.timeout.saturating_sub(started.elapsed());
                run_cli_invocation_with_options(&fresh_invocation, &options)
                    .with_context(|| format!("failed to resume cached AI conversation ({error})"))?
            }
            Err(error) => return Err(error),
        };

        if let Some(session_id) = output.session_id.clone() {
            self.record_session_id(conversation_key, session_id);
        }

        AiSuggestion::from_provider_text(&output.text)
    }
}

pub fn cli_invocation_for_request(
    provider: AiProvider,
    model: &str,
    request: &AiSuggestionRequest,
) -> Result<AiCliInvocation> {
    cli_invocation_for_request_with_session(provider, model, request, None)
}

pub fn cli_invocation_for_review_plan(
    provider: AiProvider,
    model: &str,
    request: &AiReviewPlanRequest,
) -> Result<AiCliInvocation> {
    cli_invocation_for_generation(provider, model, request.prompt.clone())
}

fn cli_invocation_for_generation(
    provider: AiProvider,
    model: &str,
    prompt: String,
) -> Result<AiCliInvocation> {
    match provider {
        AiProvider::CodexCli => Ok(codex_review_plan_invocation(model, prompt)),
        AiProvider::ClaudeCli => Ok(claude_invocation(model, prompt, None)),
        AiProvider::Anthropic | AiProvider::OpenAi => Err(anyhow!(
            "{} direct API review plans are not implemented yet",
            provider.label()
        )),
    }
}

fn cli_invocation_for_request_with_session(
    provider: AiProvider,
    model: &str,
    request: &AiSuggestionRequest,
    session_id: Option<&str>,
) -> Result<AiCliInvocation> {
    let prompt = cli_prompt_for_request(request, session_id.is_none());
    match provider {
        AiProvider::CodexCli => Ok(codex_invocation(model, prompt, session_id)),
        AiProvider::ClaudeCli => Ok(claude_invocation(model, prompt, session_id)),
        AiProvider::Anthropic | AiProvider::OpenAi => Err(anyhow!(
            "{} direct API suggestions are not implemented yet",
            provider.label()
        )),
    }
}

fn cli_prompt_for_request(
    request: &AiSuggestionRequest,
    include_review_set_context: bool,
) -> String {
    let review_set_context = if include_review_set_context {
        format!(
            "Review-set context for this conversation. Keep this context for all later block prompts in the same review set.\nReview set hash: {}\n\n{}\n\n",
            request.review_set.review_set_hash, request.review_set.overview,
        )
    } else {
        format!(
            "Use the review-set context already provided in this conversation. Review set hash: {}.\n\n",
            request.review_set.review_set_hash,
        )
    };

    format!(
        "{review_set_context}{}\n\nImportant: return exactly two plain-text lines, `EXPLANATION: ...` and `CHANGE: ...`. Keep each value after the label within {} visible characters so the TUI hint fits. Use `CHANGE: NONE` unless there is a concrete, reasonable change to request. If `CHANGE: NONE`, make `EXPLANATION` a one-line what-this-does + LGTM-style note. Do not run shell commands, inspect additional files, modify files, or produce markdown fences.",
        request.prompt, request.key.max_response_chars,
    )
}

fn codex_invocation(model: &str, prompt: String, session_id: Option<&str>) -> AiCliInvocation {
    let final_message_path = temporary_codex_final_message_path();
    let mut args = if session_id.is_some() {
        vec![
            "exec".to_string(),
            "resume".to_string(),
            "--skip-git-repo-check".to_string(),
            "--json".to_string(),
            "-c".to_string(),
            "model_reasoning_effort=\"high\"".to_string(),
            "--output-last-message".to_string(),
            final_message_path.to_string_lossy().to_string(),
        ]
    } else {
        vec![
            "exec".to_string(),
            "--sandbox".to_string(),
            "read-only".to_string(),
            "--skip-git-repo-check".to_string(),
            "--color".to_string(),
            "never".to_string(),
            "--json".to_string(),
            "-c".to_string(),
            "model_reasoning_effort=\"high\"".to_string(),
            "--output-last-message".to_string(),
            final_message_path.to_string_lossy().to_string(),
        ]
    };
    args.push("--model".to_string());
    args.push(effective_model_for_provider(AiProvider::CodexCli, model));
    if let Some(session_id) = session_id {
        args.push(session_id.to_string());
    }
    args.push("-".to_string());
    AiCliInvocation {
        program: "codex".to_string(),
        args,
        stdin: prompt,
        final_message_path: Some(final_message_path),
        output_format: AiCliOutputFormat::CodexJson,
    }
}

fn codex_review_plan_invocation(model: &str, prompt: String) -> AiCliInvocation {
    let final_message_path = temporary_codex_final_message_path_with_prefix("review-plan");
    let mut args = vec![
        "exec".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--skip-git-repo-check".to_string(),
        "--color".to_string(),
        "never".to_string(),
        "--json".to_string(),
        "-c".to_string(),
        "model_reasoning_effort=\"high\"".to_string(),
        "--output-last-message".to_string(),
        final_message_path.to_string_lossy().to_string(),
    ];
    args.push("--model".to_string());
    args.push(effective_model_for_provider(AiProvider::CodexCli, model));
    args.push("-".to_string());
    AiCliInvocation {
        program: "codex".to_string(),
        args,
        stdin: prompt,
        final_message_path: Some(final_message_path),
        output_format: AiCliOutputFormat::CodexJson,
    }
}

fn temporary_codex_final_message_path() -> PathBuf {
    temporary_codex_final_message_path_with_prefix("hint")
}

fn temporary_codex_final_message_path_with_prefix(prefix: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "trueflow-codex-{prefix}-{}-{now}.txt",
        std::process::id()
    ))
}

fn claude_invocation(model: &str, prompt: String, session_id: Option<&str>) -> AiCliInvocation {
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ];
    if let Some(session_id) = session_id {
        args.push("--resume".to_string());
        args.push(session_id.to_string());
    }
    if model != "auto" {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    AiCliInvocation {
        program: "claude".to_string(),
        args,
        stdin: prompt,
        final_message_path: None,
        output_format: AiCliOutputFormat::ClaudeJson,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AiCliRunOutput {
    text: String,
    session_id: Option<String>,
}

pub const DEFAULT_AI_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_AI_OUTPUT_BYTE_LIMIT: usize = 1024 * 1024;

/// One-shot cancellation shared between the UI and its provider worker.
#[derive(Debug, Clone, Default)]
pub struct AiCancellationToken(Arc<AtomicBool>);

impl AiCancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Keep this guard in the owning UI scope so errors and early returns cancel
    /// the worker too. Dropping a receiver alone does not stop a provider.
    pub fn cancel_on_drop(&self) -> AiCancellationGuard {
        AiCancellationGuard(self.clone())
    }
}

#[derive(Debug)]
pub struct AiCancellationGuard(AiCancellationToken);

impl Drop for AiCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug, Clone)]
pub struct AiExecutionOptions {
    pub timeout: Duration,
    pub cancellation: AiCancellationToken,
    /// Independent cap for stdout, stderr, and the final-message file.
    pub max_output_bytes: usize,
}

impl Default for AiExecutionOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_AI_TIMEOUT,
            cancellation: AiCancellationToken::default(),
            max_output_bytes: DEFAULT_AI_OUTPUT_BYTE_LIMIT,
        }
    }
}

#[derive(Debug)]
enum AiExecutionStopped {
    Timeout,
    Cancelled,
    OutputLimit,
    Interrupted,
}

impl std::fmt::Display for AiExecutionStopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Timeout => "AI provider timed out",
            Self::Cancelled => "AI provider cancelled",
            Self::OutputLimit => "AI provider exceeded output limit",
            Self::Interrupted => "AI provider interrupted by a signal",
        })
    }
}

impl std::error::Error for AiExecutionStopped {}

/// Defers SIGINT, SIGTERM, and SIGHUP while a provider runs. The provider has
/// its own process group, so these signals reach only trueflow; deferring them
/// lets trueflow kill that group and remove temporary files before the signal's
/// default action ends the process. A second signal acts immediately.
#[cfg(unix)]
struct DeferredSignals {
    received: Vec<(std::ffi::c_int, Arc<AtomicBool>)>,
    handlers: Vec<signal_hook::SigId>,
}

#[cfg(unix)]
impl DeferredSignals {
    fn register() -> Result<Self> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

        let mut deferred = Self {
            received: Vec::new(),
            handlers: Vec::new(),
        };
        for signal in [SIGINT, SIGTERM, SIGHUP] {
            let flag = Arc::new(AtomicBool::new(false));
            // Registered first, so the first signal only arms it.
            deferred
                .handlers
                .push(signal_hook::flag::register_conditional_default(
                    signal,
                    Arc::clone(&flag),
                )?);
            deferred
                .handlers
                .push(signal_hook::flag::register(signal, Arc::clone(&flag))?);
            deferred.received.push((signal, flag));
        }
        Ok(deferred)
    }

    fn received(&self) -> Option<std::ffi::c_int> {
        self.received
            .iter()
            .find(|(_, flag)| flag.load(Ordering::Acquire))
            .map(|(signal, _)| *signal)
    }

    /// Restores prior signal handling, then performs any deferred default action.
    fn finish(mut self) -> Result<()> {
        let received = self.received();
        self.unregister();
        if let Some(signal) = received {
            signal_hook::low_level::emulate_default_handler(signal)?;
            // Only reached when the signal's default action does not terminate.
            return Err(AiExecutionStopped::Interrupted.into());
        }
        Ok(())
    }

    fn unregister(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}

#[cfg(unix)]
impl Drop for DeferredSignals {
    fn drop(&mut self) {
        self.unregister();
    }
}

struct FinalMessageCleanup<'a>(Option<&'a PathBuf>);

impl Drop for FinalMessageCleanup<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct ProviderProcess(std::process::Child, bool);

impl ProviderProcess {
    fn terminate(&mut self) {
        if !self.1 {
            return;
        }
        self.1 = false;
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            if let Ok(pid) = i32::try_from(self.0.id()) {
                // The child starts its own process group. Negative PID targets
                // that group, including grandchildren retaining our pipes.
                unsafe {
                    kill(-pid, 9);
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.0.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn read_bounded_output(
    mut pipe: impl Read,
    limit: usize,
    overflow: &AtomicBool,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            return Ok(bytes);
        }
        if count > limit.saturating_sub(bytes.len()) {
            overflow.store(true, Ordering::Release);
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn run_cli_invocation_with_options(
    invocation: &AiCliInvocation,
    options: &AiExecutionOptions,
) -> Result<AiCliRunOutput> {
    #[cfg(unix)]
    {
        let signals = DeferredSignals::register()?;
        // The provider group is dead and temporary files are gone before any
        // deferred signal's default action runs.
        let output = run_cli_invocation_until(invocation, options, || signals.received().is_some());
        signals.finish()?;
        output
    }
    #[cfg(not(unix))]
    {
        run_cli_invocation_until(invocation, options, || false)
    }
}

fn run_cli_invocation_until(
    invocation: &AiCliInvocation,
    options: &AiExecutionOptions,
    interrupted: impl Fn() -> bool,
) -> Result<AiCliRunOutput> {
    let _cleanup = FinalMessageCleanup(invocation.final_message_path.as_ref());
    if options.cancellation.is_cancelled() {
        return Err(AiExecutionStopped::Cancelled.into());
    }
    if options.timeout.is_zero() {
        return Err(AiExecutionStopped::Timeout.into());
    }
    let started = Instant::now();
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut process = ProviderProcess(
        command
            .spawn()
            .with_context(|| format!("failed to start {}", invocation.program))?,
        true,
    );
    let mut stdin = process.0.stdin.take().context("provider stdin missing")?;
    let stdout = process.0.stdout.take().context("provider stdout missing")?;
    let stderr = process.0.stderr.take().context("provider stderr missing")?;
    let overflow = AtomicBool::new(false);
    let (status, stdout, stderr) = std::thread::scope(|scope| -> Result<_> {
        // Drop inside the scope, before it joins any workers, even if starting
        // another worker panics. Otherwise a live provider can keep pipes open.
        let mut process = process;
        // Concurrent drains avoid both large-prompt and full-output pipe deadlocks.
        // Moving stdin into the writer ensures EOF as soon as the prompt is sent.
        let writer = scope.spawn(move || stdin.write_all(invocation.stdin.as_bytes()));
        let stdout_reader =
            scope.spawn(|| read_bounded_output(stdout, options.max_output_bytes, &overflow));
        let stderr_reader =
            scope.spawn(|| read_bounded_output(stderr, options.max_output_bytes, &overflow));
        let status: Result<std::process::ExitStatus> = loop {
            let file_overflow = invocation
                .final_message_path
                .as_ref()
                .and_then(|path| fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.len() > options.max_output_bytes as u64);
            if options.cancellation.is_cancelled() {
                break Err(AiExecutionStopped::Cancelled.into());
            }
            if interrupted() {
                break Err(AiExecutionStopped::Interrupted.into());
            }
            if overflow.load(Ordering::Acquire) || file_overflow {
                break Err(AiExecutionStopped::OutputLimit.into());
            }
            if started.elapsed() >= options.timeout {
                break Err(AiExecutionStopped::Timeout.into());
            }
            match process.0.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => break Err(error.into()),
            }
        };
        // Also stop descendants after a successful leader exits; otherwise they
        // could retain output/stdin pipes forever. Always reap before joining I/O.
        process.terminate();
        let write_result = writer
            .join()
            .map_err(|_panic| anyhow!("provider stdin worker panicked"))?;
        let stdout_result = stdout_reader
            .join()
            .map_err(|_panic| anyhow!("provider stdout worker panicked"))?;
        let stderr_result = stderr_reader
            .join()
            .map_err(|_panic| anyhow!("provider stderr worker panicked"))?;
        let status = status?;
        if overflow.load(Ordering::Acquire) {
            return Err(AiExecutionStopped::OutputLimit.into());
        }
        if status.success() {
            write_result.context("failed to send AI prompt")?;
        }
        Ok((status, stdout_result?, stderr_result?))
    })?;
    let stdout = String::from_utf8(stdout)
        .with_context(|| format!("{} returned non-UTF-8 stdout", invocation.program))?;
    if !status.success() {
        let diagnostic = match invocation.output_format {
            AiCliOutputFormat::ClaudeJson => serde_json::from_str(&stdout)
                .ok()
                .as_ref()
                .and_then(claude_error_message),
            _ => None,
        };
        return Err(anyhow!(
            "{} exited with {}: {}",
            invocation.program,
            status,
            diagnostic.unwrap_or_else(|| collapse_whitespace(&String::from_utf8_lossy(&stderr)))
        ));
    }
    let session_id = match invocation.output_format {
        AiCliOutputFormat::Text => None,
        AiCliOutputFormat::CodexJson | AiCliOutputFormat::ClaudeJson => {
            extract_session_id_from_json_text(&stdout)
        }
    };
    let text = if let Some(path) = invocation.final_message_path.as_ref() {
        let file = fs::File::open(path).with_context(|| {
            format!(
                "failed to read {} final message from {}",
                invocation.program,
                path.display()
            )
        })?;
        let bytes = read_bounded_output(file, options.max_output_bytes, &overflow)?;
        if overflow.load(Ordering::Acquire) {
            return Err(AiExecutionStopped::OutputLimit.into());
        }
        String::from_utf8(bytes).context("provider final message is not UTF-8")?
    } else {
        match invocation.output_format {
            AiCliOutputFormat::Text | AiCliOutputFormat::CodexJson => stdout,
            AiCliOutputFormat::ClaudeJson => parse_claude_json_result(&stdout)?,
        }
    };
    Ok(AiCliRunOutput { text, session_id })
}

pub fn build_review_hint_prompt(context: &AiReviewContext, max_context_lines: usize) -> String {
    build_review_block_prompt(
        context,
        None,
        max_context_lines,
        DEFAULT_AI_RESPONSE_CHAR_LIMIT,
    )
}

fn build_review_block_prompt(
    context: &AiReviewContext,
    review_set_hash: Option<&TreeHash>,
    max_context_lines: usize,
    max_response_chars: usize,
) -> String {
    let line_start = context.start_line.saturating_add(1);
    let line_end = context.end_line.max(context.start_line.saturating_add(1));
    let block_content = PlanSource::new(&context.content)
        .excerpt(max_context_lines.max(1))
        .join("\n");
    let review_set_line = review_set_hash
        .map(|hash| format!("Review set hash: {hash}\n"))
        .unwrap_or_default();
    format!(
        "Review this block in the context of the full review set.\n{review_set_line}Return exactly two concise lines; keep each value after the label within {max_response_chars} visible characters:\nEXPLANATION: one sentence explaining what this block is/does in context; if no change is needed, include a brief LGTM-style reassurance in the same sentence.\nCHANGE: one sentence with a concrete requested change, or NONE if there is no reasonable change to propose.\nBe conservative: do not propose style-only or speculative changes.\n\nPath: {}\nLanguage: {:?}\nBlock kind: {}\nLines: {line_start}-{line_end}\n\n```\n{block_content}\n```",
        context.path,
        context.language,
        context.block_kind.as_str(),
    )
}

fn collapse_whitespace(raw: &str) -> String {
    let mut out = String::new();
    for part in raw.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

/// Return `text` through its first sentence terminator. A `.`, `!` or `?`
/// ends a sentence only after a non-space character and before whitespace or
/// the end of text, so `self.cache`, `1.2`, `config.rs` and a standalone `?`
/// operator stay inside the sentence.
fn first_sentence(text: &str) -> &str {
    let mut previous = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let ends_sentence = matches!(ch, '.' | '!' | '?')
            && previous.is_some_and(|previous: char| !previous.is_whitespace())
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        if ends_sentence {
            return &text[..index + ch.len_utf8()];
        }
        previous = Some(ch);
    }
    text
}

fn truncate_for_modeline(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let take_chars = max_chars.saturating_sub(1);
    let mut truncated = text.chars().take(take_chars).collect::<String>();
    truncated.push('…');
    truncated
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ParsedProviderText {
    explanation: Option<String>,
    change: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderField {
    Explanation,
    Change,
}

fn parse_structured_provider_text(raw: &str) -> ParsedProviderText {
    let mut parsed = ParsedProviderText::default();
    let mut current_field = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = strip_provider_label(trimmed, "EXPLANATION") {
            parsed.explanation = Some(rest.to_string());
            current_field = Some(ProviderField::Explanation);
            continue;
        }
        if let Some(rest) = strip_provider_label(trimmed, "CHANGE") {
            parsed.change = Some(rest.to_string());
            current_field = Some(ProviderField::Change);
            continue;
        }

        match current_field {
            Some(ProviderField::Explanation) => {
                append_provider_field(&mut parsed.explanation, trimmed);
            }
            Some(ProviderField::Change) => append_provider_field(&mut parsed.change, trimmed),
            None => {}
        }
    }

    parsed
}

fn strip_provider_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let trimmed = line
        .trim_start_matches(|ch: char| ch == '-' || ch == '*' || ch.is_whitespace())
        .trim_start();
    let prefix = trimmed.get(..label.len())?;
    if !prefix.eq_ignore_ascii_case(label) {
        return None;
    }
    let rest = trimmed.get(label.len()..)?.trim_start();
    if rest.is_empty() {
        return Some(rest);
    }
    let rest = rest.strip_prefix(':')?.trim_start();
    Some(rest)
}

fn append_provider_field(field: &mut Option<String>, line: &str) {
    let Some(existing) = field else {
        *field = Some(line.to_string());
        return;
    };
    if !existing.is_empty() {
        existing.push(' ');
    }
    existing.push_str(line);
}

fn normalize_optional_sentence(raw: &str) -> Option<String> {
    let collapsed = collapse_whitespace(raw);
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_for_modeline(first_sentence(&collapsed), 180))
}

fn normalize_proposed_change(raw: &str) -> Option<String> {
    let collapsed = collapse_whitespace(raw);
    if collapsed.is_empty() || is_no_change_text(&collapsed) {
        return None;
    }
    Some(truncate_for_modeline(first_sentence(&collapsed), 180))
}

fn normalize_unstructured_proposed_change(collapsed: &str) -> Option<String> {
    if is_no_change_text(collapsed) {
        return None;
    }
    Some(truncate_for_modeline(first_sentence(collapsed), 180))
}

fn is_no_change_text(text: &str) -> bool {
    let lower = text
        .trim()
        .trim_matches(|ch: char| ch == '.' || ch == '!' || ch == '?' || ch == '`')
        .to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "none" | "no" | "no change" | "no changes" | "no proposed change" | "no proposed changes"
    ) || lower.contains("no reasonable change")
        || lower.contains("no concrete change")
        || lower.contains("nothing to change")
        || lower.contains("looks good")
        || lower.contains("looks fine")
}

fn claude_error_message(value: &serde_json::Value) -> Option<String> {
    if value.get("is_error").and_then(serde_json::Value::as_bool) != Some(true)
        && !value
            .get("subtype")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| s.starts_with("error"))
    {
        return None;
    }
    let result = value
        .get("result")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned);
    let errors = || {
        value
            .get("errors")
            .and_then(serde_json::Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .take(8)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|text| !text.is_empty())
    };
    let message = result
        .or_else(errors)
        .unwrap_or_else(|| "Claude reported an error without details".to_string());
    Some(collapse_whitespace(&message).chars().take(2000).collect())
}

fn parse_claude_json_result(stdout: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| "failed to parse claude JSON output")?;
    if let Some(message) = claude_error_message(&value) {
        return Err(anyhow!("{message}"));
    }
    value
        .get("result")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("text"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("claude JSON output did not include a text result"))
}

fn extract_session_id_from_json_text(stdout: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim())
        && let Some(session_id) = extract_session_id_from_json_value(&value)
    {
        return Some(session_id);
    }

    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .find_map(|value| extract_session_id_from_json_value(&value))
}

fn extract_session_id_from_json_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["session_id", "thread_id", "conversation_id"] {
                if let Some(session_id) = map.get(key).and_then(serde_json::Value::as_str)
                    && !session_id.trim().is_empty()
                {
                    return Some(session_id.to_string());
                }
            }
            if let Some(thread_id) = map
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(serde_json::Value::as_str)
                && !thread_id.trim().is_empty()
            {
                return Some(thread_id.to_string());
            }
            map.values().find_map(extract_session_id_from_json_value)
        }
        serde_json::Value::Array(items) => {
            items.iter().find_map(extract_session_id_from_json_value)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiEnvironment {
    anthropic_api_key: bool,
    openai_api_key: bool,
    executables: HashSet<String>,
}

impl AiEnvironment {
    pub fn detect_current() -> Self {
        let anthropic_api_key = std::env::var_os(ANTHROPIC_API_KEY)
            .is_some_and(|value| !value.to_string_lossy().trim().is_empty());
        let openai_api_key = std::env::var_os(OPENAI_API_KEY)
            .is_some_and(|value| !value.to_string_lossy().trim().is_empty());
        let executables = ["claude", "codex"]
            .into_iter()
            .filter(|name| executable_exists(name))
            .map(str::to_string)
            .collect();
        Self {
            anthropic_api_key,
            openai_api_key,
            executables,
        }
    }

    #[cfg(test)]
    fn for_tests(
        anthropic_api_key: bool,
        openai_api_key: bool,
        executables: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            anthropic_api_key,
            openai_api_key,
            executables: executables.into_iter().map(str::to_string).collect(),
        }
    }

    fn has_provider(&self, provider: AiProvider) -> bool {
        match provider {
            AiProvider::Anthropic => self.anthropic_api_key,
            AiProvider::OpenAi => self.openai_api_key,
            AiProvider::ClaudeCli => self.executables.contains("claude"),
            AiProvider::CodexCli => self.executables.contains("codex"),
        }
    }

    pub fn detected_providers(&self) -> Vec<AiProvider> {
        let mut providers = Vec::new();
        for provider in [
            AiProvider::Anthropic,
            AiProvider::OpenAi,
            AiProvider::ClaudeCli,
            AiProvider::CodexCli,
        ] {
            if self.has_provider(provider) {
                providers.push(provider);
            }
        }
        providers
    }
}

pub fn resolve_ai_availability(config: &AiConfig, env: &AiEnvironment) -> AiAvailability {
    let detected = env.detected_providers();
    if !config.mode.review_plan_enabled() && !config.mode.block_hints_enabled() {
        return AiAvailability::Disabled { detected };
    }

    match config.provider {
        AiProviderConfig::Auto => detected
            .iter()
            .copied()
            .find(|provider| provider.is_implemented())
            .map(|provider| AiAvailability::Ready {
                provider,
                model: effective_model_for_provider(provider, &config.model),
            })
            .unwrap_or_else(|| AiAvailability::Unavailable {
                reason: "install the claude or codex CLI; direct API providers are not implemented"
                    .to_string(),
                detected,
            }),
        AiProviderConfig::None => AiAvailability::Unavailable {
            reason: "provider is none".to_string(),
            detected,
        },
        AiProviderConfig::Anthropic => {
            resolve_explicit_provider(AiProvider::Anthropic, &config.model, env, detected)
        }
        AiProviderConfig::OpenAi => {
            resolve_explicit_provider(AiProvider::OpenAi, &config.model, env, detected)
        }
        AiProviderConfig::ClaudeCli => {
            resolve_explicit_provider(AiProvider::ClaudeCli, &config.model, env, detected)
        }
        AiProviderConfig::CodexCli => {
            resolve_explicit_provider(AiProvider::CodexCli, &config.model, env, detected)
        }
    }
}

fn resolve_explicit_provider(
    provider: AiProvider,
    configured_model: &str,
    env: &AiEnvironment,
    detected: Vec<AiProvider>,
) -> AiAvailability {
    if !provider.is_implemented() {
        AiAvailability::Unavailable {
            reason: format!(
                "{} direct API provider is not implemented; set provider = \"claude_cli\" or \"codex_cli\"",
                provider.label()
            ),
            detected,
        }
    } else if env.has_provider(provider) {
        AiAvailability::Ready {
            provider,
            model: effective_model_for_provider(provider, configured_model),
        }
    } else {
        AiAvailability::Unavailable {
            reason: format!("{} credentials or executable not found", provider.label()),
            detected,
        }
    }
}

pub fn effective_model_for_provider(provider: AiProvider, configured_model: &str) -> String {
    if configured_model != "auto" {
        return configured_model.to_string();
    }
    default_model_for_provider(provider).to_string()
}

pub fn default_model_for_provider(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::Anthropic => "claude-3-5-haiku-latest",
        AiProvider::ClaudeCli => "auto",
        AiProvider::OpenAi => "gpt-5-mini",
        AiProvider::CodexCli => "gpt-6",
    }
}

fn provider_list(providers: &[AiProvider]) -> String {
    providers
        .iter()
        .map(|provider| provider.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn executable_exists(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|directory| executable_candidate_exists(&directory, name))
}

#[cfg(unix)]
fn executable_candidate_exists(directory: &Path, name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(name);
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn executable_candidate_exists(directory: &Path, name: &str) -> bool {
    let path = directory.join(name);
    if path.is_file() {
        return true;
    }
    std::env::var_os("PATHEXT")
        .map(|pathext| {
            pathext
                .to_string_lossy()
                .split(';')
                .any(|extension| directory.join(format!("{name}{extension}")).is_file())
        })
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn executable_candidate_exists(directory: &Path, name: &str) -> bool {
    directory.join(name).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiMode;

    fn config(mode: AiMode, provider: AiProviderConfig) -> AiConfig {
        AiConfig {
            mode,
            provider,
            model: "auto".to_string(),
            max_context_lines: 80,
            cache: true,
        }
    }

    fn review_context(content: &str) -> AiReviewContext {
        AiReviewContext {
            path: "src/lib.rs".to_string(),
            language: Language::Rust,
            block_kind: BlockKind::Function,
            block_hash: TreeHash::from_content(content),
            start_line: 4,
            end_line: 8,
            content: content.to_string(),
        }
    }

    fn review_set() -> AiReviewSetContext {
        AiReviewSetContext::new(
            TreeHash::from_content("review-set"),
            "Scope: diff vs main\nReview blocks: 1".to_string(),
        )
    }

    #[test]
    fn review_hint_prompt_clips_block_content() {
        let prompt = build_review_hint_prompt(&review_context("one\ntwo\nthree"), 2);

        assert!(prompt.contains("one\ntwo\n[source partial; remaining lines omitted]"));
        assert!(!prompt.contains("three"));
    }

    #[test]
    fn block_hint_prompt_keeps_head_side_for_long_base() {
        let content = format!("[base]\n{}\n[head]\nNEW_MARKER\n", "old();\n".repeat(100));
        let request = AiSuggestionRequest::new(
            AiProvider::ClaudeCli,
            "auto".to_string(),
            review_set(),
            review_context(&content),
            80,
        );

        assert!(request.prompt.contains("NEW_MARKER"), "{}", request.prompt);
        assert!(request.prompt.contains("[base partial;"), "{}", request.prompt);
        assert!(request.prompt.matches("old();").count() < 80);
    }

    #[test]
    fn suggestion_sentences_end_only_at_terminators_before_whitespace() {
        for (raw, expected_explanation, expected_change) in [
            (
                "EXPLANATION: Reads config.toml once! Then caches it.\nCHANGE: Cache it in self.cache instead. Extra sentence.",
                "Reads config.toml once!",
                "Cache it in self.cache instead.",
            ),
            (
                "EXPLANATION: Parses v1.2 headers.\nCHANGE: Propagate the error with ? instead of unwrap. Extra.",
                "Parses v1.2 headers.",
                "Propagate the error with ? instead of unwrap.",
            ),
            (
                "EXPLANATION: Is this safe?\nCHANGE: Validate the path in config.rs before opening it",
                "Is this safe?",
                "Validate the path in config.rs before opening it",
            ),
        ] {
            let suggestion = AiSuggestion::from_provider_text(raw)
                .unwrap_or_else(|error| panic!("expected suggestion: {error}"));

            assert_eq!(suggestion.explanation.as_deref(), Some(expected_explanation));
            assert_eq!(suggestion.proposed_change.as_deref(), Some(expected_change));
        }
    }

    #[test]
    fn suggestion_normalization_keeps_only_first_sentence_for_visible_change() {
        let suggestion = AiSuggestion::from_provider_text(
            "  consider asking why unwrap is safe. second sentence should not render. ",
        )
        .unwrap_or_else(|error| panic!("expected suggestion: {error}"));

        assert_eq!(
            suggestion.proposed_change.as_deref(),
            Some("consider asking why unwrap is safe.")
        );
    }

    #[test]
    fn structured_suggestion_uses_explanation_as_visible_lgtm_text_without_change() {
        let suggestion = AiSuggestion::from_provider_text(
            "EXPLANATION: This validates user input before saving it.\nCHANGE: NONE",
        )
        .unwrap_or_else(|error| panic!("expected suggestion: {error}"));

        assert_eq!(
            suggestion.explanation.as_deref(),
            Some("This validates user input before saving it.")
        );
        assert_eq!(suggestion.proposed_change, None);
        assert_eq!(
            suggestion.visible_sentence(),
            Some("This validates user input before saving it.")
        );
    }

    #[test]
    fn structured_suggestion_surfaces_only_concrete_change() {
        let suggestion = AiSuggestion::from_provider_text(
            "EXPLANATION: This loads cached review state.\nCHANGE: Handle corrupt cache files instead of silently dropping all review status. Extra sentence.",
        )
        .unwrap_or_else(|error| panic!("expected suggestion: {error}"));

        assert_eq!(
            suggestion.visible_sentence(),
            Some("Handle corrupt cache files instead of silently dropping all review status.")
        );
    }

    #[test]
    fn suggestion_normalization_rejects_empty_provider_output() {
        let error = match AiSuggestion::from_provider_text(" \n\t ") {
            Ok(suggestion) => panic!("expected empty suggestion error, got {suggestion:?}"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("empty suggestion"));
    }

    #[test]
    fn suggestion_request_key_tracks_every_cache_identity_dimension() {
        let base_review_set = review_set();
        let base_context = review_context("fn checked() {}");
        let key_for = |provider,
                       model: &str,
                       review_set: &AiReviewSetContext,
                       context: &AiReviewContext,
                       max_context_lines,
                       max_response_chars| {
            AiSuggestionRequest::with_response_char_limit(
                provider,
                model.to_string(),
                review_set.clone(),
                context.clone(),
                max_context_lines,
                max_response_chars,
            )
            .key
        };
        let base = key_for(
            AiProvider::Anthropic,
            "claude-3-5-haiku",
            &base_review_set,
            &base_context,
            40,
            DEFAULT_AI_RESPONSE_CHAR_LIMIT,
        );

        assert_eq!(
            base,
            key_for(
                AiProvider::Anthropic,
                "claude-3-5-haiku",
                &base_review_set,
                &base_context,
                40,
                DEFAULT_AI_RESPONSE_CHAR_LIMIT,
            )
        );
        assert_ne!(
            base,
            key_for(
                AiProvider::OpenAi,
                "claude-3-5-haiku",
                &base_review_set,
                &base_context,
                40,
                DEFAULT_AI_RESPONSE_CHAR_LIMIT,
            )
        );
        assert_ne!(
            base,
            key_for(
                AiProvider::Anthropic,
                "different-model",
                &base_review_set,
                &base_context,
                40,
                DEFAULT_AI_RESPONSE_CHAR_LIMIT,
            )
        );

        let different_review_set = AiReviewSetContext::new(
            TreeHash::from_content("different-review-set"),
            base_review_set.overview.clone(),
        );
        assert_ne!(
            base,
            key_for(
                AiProvider::Anthropic,
                "claude-3-5-haiku",
                &different_review_set,
                &base_context,
                40,
                DEFAULT_AI_RESPONSE_CHAR_LIMIT,
            )
        );

        let mut different_path = base_context.clone();
        different_path.path = "src/other.rs".to_string();
        let different_content = review_context("fn changed() {}");
        let mut different_start = base_context.clone();
        different_start.start_line += 1;
        let mut different_end = base_context.clone();
        different_end.end_line += 1;
        for context in [
            &different_path,
            &different_content,
            &different_start,
            &different_end,
        ] {
            assert_ne!(
                base,
                key_for(
                    AiProvider::Anthropic,
                    "claude-3-5-haiku",
                    &base_review_set,
                    context,
                    40,
                    DEFAULT_AI_RESPONSE_CHAR_LIMIT,
                )
            );
        }
        assert_ne!(
            base,
            key_for(
                AiProvider::Anthropic,
                "claude-3-5-haiku",
                &base_review_set,
                &base_context,
                41,
                DEFAULT_AI_RESPONSE_CHAR_LIMIT,
            )
        );
        assert_ne!(
            base,
            key_for(
                AiProvider::Anthropic,
                "claude-3-5-haiku",
                &base_review_set,
                &base_context,
                40,
                DEFAULT_AI_RESPONSE_CHAR_LIMIT + 1,
            )
        );
    }

    #[test]
    fn json_session_extraction_reads_codex_and_claude_shapes() {
        assert_eq!(
            extract_session_id_from_json_text(
                r#"{"type":"thread.started","thread_id":"codex-thread"}"#
            )
            .as_deref(),
            Some("codex-thread")
        );
        assert_eq!(
            extract_session_id_from_json_text(
                r#"{"type":"result","session_id":"claude-session","result":"ok"}"#
            )
            .as_deref(),
            Some("claude-session")
        );
    }

    #[test]
    fn direct_api_provider_invocation_is_rejected_until_http_provider_exists() {
        let request = AiSuggestionRequest::new(
            AiProvider::Anthropic,
            "auto".to_string(),
            review_set(),
            review_context("fn checked() {}"),
            80,
        );
        let error = match cli_invocation_for_request(AiProvider::Anthropic, "auto", &request) {
            Ok(invocation) => panic!("expected direct API rejection, got {invocation:?}"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("direct API suggestions are not implemented")
        );
    }

    fn review_plan_block(index: usize, content: String) -> AiReviewPlanBlock {
        AiReviewPlanBlock {
            block_id: String::new(),
            is_test: false,
            path: format!("src/file_{index}.rs"),
            language: Language::Rust,
            block_kind: BlockKind::Function,
            block_hash: TreeHash::from_content(&content),
            start_line: index + 1,
            end_line: index + 3,
            estimated_seconds: 16,
            content,
        }
    }

    fn review_plan_request(
        blocks: &[AiReviewPlanBlock],
        max_context_lines: usize,
    ) -> AiReviewPlanRequest {
        AiReviewPlanRequest::new(
            TreeHash::from_content("review-set"),
            "changed files",
            blocks,
            max_context_lines,
            300,
        )
    }

    #[test]
    fn review_plan_prompt_states_reading_estimates_and_the_focused_pass_budget() {
        let mut long = review_plan_block(1, "fn long() {}".to_string());
        long.estimated_seconds = 410;
        let blocks = vec![review_plan_block(0, "fn short() {}".to_string()), long];
        let prompt = review_plan_request(&blocks, 20).prompt;
        assert!(
            prompt.contains("- B0001\n  Path: src/file_0.rs")
                && prompt.contains("  Reading estimate: ~16s\n"),
            "{prompt}"
        );
        assert!(prompt.contains("  Reading estimate: ~6m 50s\n"), "{prompt}");
        assert!(
            prompt.contains(
                "A focused pass reads only the priority blocks, in your order, within about 5m"
            ),
            "{prompt}"
        );
    }

    #[test]
    fn review_plan_spreads_manifest_across_files_and_preserves_canonical_ids() {
        let mut blocks = (0..140)
            .map(|index| {
                let mut block = review_plan_block(index, "source".to_string());
                block.path = "a.rs".to_string();
                block.is_test = true;
                block
            })
            .collect::<Vec<_>>();
        blocks.push(review_plan_block(140, "late source".to_string()));
        let request = review_plan_request(&blocks, 120);
        assert_eq!(request.coverage.presented_blocks, 120);
        assert!(request.prompt.contains("- B0141\n  Path: src/file_140.rs"));
        assert!(request.prompt.contains("- B0001\n  Path: a.rs"));
        assert_eq!(request.coverage.excerpted_blocks, 120);
        assert!(request.coverage.is_partial());
        assert_eq!(request, review_plan_request(&blocks, 120));
    }

    #[test]
    fn review_plan_samples_late_files_even_when_files_exceed_manifest_limit() {
        let blocks = (0..240)
            .map(|index| review_plan_block(index, format!("source_{index}")))
            .collect::<Vec<_>>();
        let request = review_plan_request(&blocks, 240);
        assert!(request.prompt.contains("- B0240\n  Path: src/file_239.rs"));
        assert_eq!(request.coverage.presented_blocks, 120);
    }

    #[test]
    fn review_plan_shares_excerpt_lines_and_reports_clipped_source() {
        let blocks = (0..3)
            .map(|index| {
                review_plan_block(
                    index,
                    format!("first_{index}\nsecond_{index}\nthird_{index}"),
                )
            })
            .collect::<Vec<_>>();
        let request = review_plan_request(&blocks, 6);
        for index in 0..3 {
            assert!(
                request
                    .prompt
                    .contains(&format!("first_{index}\nsecond_{index}"))
            );
            assert!(!request.prompt.contains(&format!("third_{index}")));
        }
        assert_eq!(request.coverage.excerpt_lines, 6);
        assert_eq!(request.coverage.excerpted_blocks, 3);
        assert!(request.coverage.is_partial());
        assert!(!review_plan_request(&blocks, 9).coverage.is_partial());
        assert_eq!(review_plan_request(&blocks, 0).coverage.excerpt_lines, 0);
    }

    #[test]
    fn review_plan_keeps_changed_head_visible_after_long_base() {
        let content = format!(
            "[base]\n{}[head]\nnew_behavior();\n",
            "old_behavior();\n".repeat(100)
        );
        let blocks = [review_plan_block(0, content)];
        let request = review_plan_request(&blocks, 2);
        assert!(request.prompt.contains("[head]\nnew_behavior();"));
        assert!(request.prompt.contains("[base]\nold_behavior();"));
        assert!(request.prompt.contains("base partial"));
        assert_eq!(request.coverage.excerpt_lines, 2);
        assert!(request.coverage.is_partial());
        let narrow = review_plan_request(&blocks, 1);
        assert!(narrow.prompt.contains("[head]\nnew_behavior();"));
        assert!(narrow.prompt.contains("base omitted"));
    }

    #[test]
    fn review_plan_head_excerpt_targets_change_after_common_prefix() {
        let prefix = "unchanged();\n".repeat(50);
        let blocks = [review_plan_block(
            0,
            format!("[base]\n{prefix}old();\n[head]\n{prefix}new();"),
        )];
        let request = review_plan_request(&blocks, 2);
        assert!(request.prompt.contains("new();"));
        assert!(request.prompt.contains("old();"));
        assert!(request.coverage.is_partial());
    }

    #[cfg(unix)]
    fn fake_invocation(script: &str) -> AiCliInvocation {
        AiCliInvocation {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            stdin: String::new(),
            final_message_path: None,
            output_format: AiCliOutputFormat::Text,
        }
    }

    #[test]
    #[cfg(unix)]
    fn subprocess_drains_output_while_writing_prompt_and_closes_stdin() {
        let mut invocation = fake_invocation(
            "dd if=/dev/zero bs=1024 count=128 2>/dev/null; cat >/dev/null; printf done",
        );
        invocation.stdin = "x".repeat(256 * 1024);
        let options = AiExecutionOptions {
            timeout: Duration::from_secs(3),
            ..Default::default()
        };
        let output = run_cli_invocation_with_options(&invocation, &options).unwrap();
        assert!(output.text.ends_with("done"));
        assert_eq!(output.text.len(), 128 * 1024 + 4);
    }

    #[test]
    #[cfg(unix)]
    fn subprocess_timeout_and_cancellation_terminate_descendants() {
        for cancel in [false, true] {
            let marker = temporary_codex_final_message_path_with_prefix("child-pid");
            let invocation = fake_invocation(&format!(
                "sleep 30 & echo $! > '{}'; wait",
                marker.display()
            ));
            let options = AiExecutionOptions {
                timeout: if cancel {
                    Duration::from_secs(3)
                } else {
                    Duration::from_millis(100)
                },
                ..Default::default()
            };
            let token = options.cancellation.clone();
            let start = Instant::now();
            let cancel_worker = std::thread::spawn(move || {
                if cancel {
                    std::thread::sleep(Duration::from_millis(100));
                    token.cancel();
                }
            });
            let error = run_cli_invocation_with_options(&invocation, &options).unwrap_err();
            cancel_worker.join().unwrap();
            assert!(
                error
                    .to_string()
                    .contains(if cancel { "cancelled" } else { "timed out" })
            );
            assert!(start.elapsed() < Duration::from_secs(2));
            let pid = fs::read_to_string(&marker).unwrap();
            let _ = fs::remove_file(marker);
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let output = Command::new("ps")
                    .args(["-o", "stat=", "-p", pid.trim()])
                    .output()
                    .unwrap();
                let state = String::from_utf8_lossy(&output.stdout);
                if state.trim().is_empty() || state.trim().starts_with('Z') {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "descendant remains alive: {state}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn subprocess_bounds_both_output_pipes_and_final_message() {
        for script in ["yes x", "yes x >&2"] {
            let options = AiExecutionOptions {
                max_output_bytes: 1024,
                timeout: Duration::from_secs(2),
                ..Default::default()
            };
            let start = Instant::now();
            let error =
                run_cli_invocation_with_options(&fake_invocation(script), &options).unwrap_err();
            assert!(error.to_string().contains("output limit"));
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        let path = temporary_codex_final_message_path_with_prefix("overflow");
        let mut invocation = fake_invocation(&format!(
            "dd if=/dev/zero of='{}' bs=2048 count=1 2>/dev/null; sleep 30",
            path.display()
        ));
        invocation.final_message_path = Some(path.clone());
        let options = AiExecutionOptions {
            max_output_bytes: 1024,
            timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let error = run_cli_invocation_with_options(&invocation, &options).unwrap_err();
        assert!(error.to_string().contains("output limit"));
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn subprocess_reads_final_message_and_session_and_cleans_up_on_failure() {
        let path = temporary_codex_final_message_path_with_prefix("result");
        let mut invocation = fake_invocation(&format!(
            "cat >/dev/null; printf answer > '{}'; printf '%s' '{{\"thread_id\":\"conversation\"}}'",
            path.display()
        ));
        invocation.final_message_path = Some(path.clone());
        invocation.output_format = AiCliOutputFormat::CodexJson;
        let output =
            run_cli_invocation_with_options(&invocation, &AiExecutionOptions::default()).unwrap();
        assert_eq!(output.text, "answer");
        assert_eq!(output.session_id.as_deref(), Some("conversation"));
        assert!(!path.exists());
        invocation.args = vec![
            "-c".to_string(),
            format!("printf partial > '{}'; exit 1", path.display()),
        ];
        assert!(
            run_cli_invocation_with_options(&invocation, &AiExecutionOptions::default()).is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn review_plan_limits_each_excerpt_and_keeps_single_side_source() {
        let blocks = [review_plan_block(0, "source\n".repeat(30))];
        let request = review_plan_request(&blocks, 100);
        assert_eq!(request.coverage.excerpt_lines, 12);
        assert!(request.coverage.is_partial());
        for side in ["base", "head"] {
            let blocks = [review_plan_block(0, format!("[{side}]\nchanged();"))];
            let request = review_plan_request(&blocks, 1);
            assert!(request.prompt.contains(&format!("[{side}]\nchanged();")));
            assert!(!request.coverage.is_partial());
        }
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_guard_prevents_spawn_after_owner_exits() {
        let marker = temporary_codex_final_message_path_with_prefix("not-started");
        let invocation = fake_invocation(&format!("touch '{}'", marker.display()));
        let options = AiExecutionOptions::default();
        {
            let _guard = options.cancellation.cancel_on_drop();
        }
        let error = run_cli_invocation_with_options(&invocation, &options).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!marker.exists());
    }

    #[test]
    fn review_plan_response_is_strictly_parsed_and_normalized() {
        let plan = AiReviewPlan::from_provider_text(
            r#"{
                "summary": "  concise briefing  ",
                "global_checks": [" check one ", "check one", " ", "check two"],
                "priority_blocks": [{
                    "block_id": " B0001 ",
                    "reason": " reason ",
                    "checks": [" check ", "check", ""]
                }]
            }"#,
        )
        .unwrap_or_else(|error| panic!("parse review plan: {error}"));

        assert_eq!(plan.summary, "concise briefing");
        assert_eq!(plan.global_checks, vec!["check one", "check two"]);
        assert_eq!(plan.priority_blocks[0].block_id, "B0001");
        assert_eq!(plan.priority_blocks[0].checks, vec!["check"]);

        let unknown = AiReviewPlan::from_provider_text(
            r#"{"summary":"ok","global_checks":[],"priority_blocks":[],"extra":true}"#,
        )
        .unwrap_err();
        assert!(unknown.to_string().contains("unknown field"));
    }

    #[test]
    #[cfg(unix)]
    fn claude_structured_errors_remain_actionable_on_any_exit_status() {
        for status in [0, 1] {
            let mut invocation = fake_invocation(&format!(
                "printf '%s' '{{\"is_error\":true,\"result\":\"Authentication expired: log in again\"}}'; exit {status}"
            ));
            invocation.output_format = AiCliOutputFormat::ClaudeJson;
            let error =
                run_cli_invocation_with_options(&invocation, &AiExecutionOptions::default())
                    .unwrap_err();
            assert!(
                error.to_string().contains("Authentication expired"),
                "{error}"
            );
        }
    }

    #[test]
    fn review_plan_response_rejects_malformed_or_blank_summary() {
        for raw in [
            "",
            "not json",
            r#"{"summary":" ","global_checks":[],"priority_blocks":[]}"#,
        ] {
            assert!(
                AiReviewPlan::from_provider_text(raw).is_err(),
                "expected rejection for {raw:?}"
            );
        }
    }

    #[test]
    fn auto_provider_selects_first_implemented_provider_when_enabled() {
        let env = AiEnvironment::for_tests(true, true, ["claude", "codex"]);
        let availability =
            resolve_ai_availability(&config(AiMode::ReviewPlan, AiProviderConfig::Auto), &env);

        assert_eq!(
            availability,
            AiAvailability::Ready {
                provider: AiProvider::ClaudeCli,
                model: "auto".to_string(),
            }
        );

        let api_keys_only = AiEnvironment::for_tests(true, true, []);
        let AiAvailability::Unavailable { reason, detected } = resolve_ai_availability(
            &config(AiMode::ReviewPlan, AiProviderConfig::Auto),
            &api_keys_only,
        ) else {
            panic!("auto must not select an unimplemented direct API provider");
        };
        assert!(reason.contains("claude"), "{reason}");
        assert_eq!(detected, vec![AiProvider::Anthropic, AiProvider::OpenAi]);
    }

    #[test]
    fn explicit_direct_api_provider_is_unavailable_even_with_credentials() {
        let env = AiEnvironment::for_tests(true, true, ["claude", "codex"]);
        for (provider_config, label) in [
            (AiProviderConfig::Anthropic, "Anthropic"),
            (AiProviderConfig::OpenAi, "OpenAI"),
        ] {
            let AiAvailability::Unavailable { reason, .. } =
                resolve_ai_availability(&config(AiMode::ReviewPlan, provider_config), &env)
            else {
                panic!("{label} direct API provider must not resolve as ready");
            };
            assert!(reason.contains(label), "{reason}");
            assert!(reason.contains("not implemented"), "{reason}");
        }
    }

    #[test]
    fn explicit_provider_reports_unavailable_when_missing() {
        let env = AiEnvironment::for_tests(false, false, []);
        let availability = resolve_ai_availability(
            &config(AiMode::ReviewPlan, AiProviderConfig::ClaudeCli),
            &env,
        );

        assert_eq!(
            availability,
            AiAvailability::Unavailable {
                reason: "Claude CLI credentials or executable not found".to_string(),
                detected: Vec::new(),
            }
        );
    }

    #[test]
    fn default_configuration_does_not_enable_ai_with_providers_available() {
        let env = AiEnvironment::for_tests(true, true, ["claude", "codex"]);
        assert!(matches!(
            resolve_ai_availability(&AiConfig::default(), &env),
            AiAvailability::Disabled { .. }
        ));
    }

    #[test]
    fn enabled_default_selects_codex_without_falling_back_to_other_providers() {
        let config = AiConfig {
            mode: AiMode::ReviewPlan,
            ..AiConfig::default()
        };
        let available = AiEnvironment::for_tests(true, true, ["claude", "codex"]);
        assert_eq!(
            resolve_ai_availability(&config, &available),
            AiAvailability::Ready {
                provider: AiProvider::CodexCli,
                model: "gpt-6".to_string(),
            }
        );

        let missing = AiEnvironment::for_tests(true, true, ["claude"]);
        let AiAvailability::Unavailable { reason, .. } = resolve_ai_availability(&config, &missing)
        else {
            panic!("missing Codex must not silently select another available provider");
        };
        assert!(reason.contains("Codex"));
    }

    #[test]
    fn explicit_provider_and_model_override_available_codex() {
        let env = AiEnvironment::for_tests(false, false, ["claude", "codex"]);
        let config = AiConfig {
            mode: AiMode::ReviewPlan,
            provider: AiProviderConfig::ClaudeCli,
            model: "claude-custom".to_string(),
            ..AiConfig::default()
        };
        let AiAvailability::Ready { provider, model } = resolve_ai_availability(&config, &env)
        else {
            panic!("explicit available Claude provider must remain available");
        };
        assert_eq!(provider, AiProvider::ClaudeCli);
        assert_eq!(model, "claude-custom");
    }
}
