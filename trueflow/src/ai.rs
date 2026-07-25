use crate::analysis::Language;
use crate::block::BlockKind;
use crate::config::{AiConfig, AiProviderConfig};
use crate::hashing::TreeHash;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub path: String,
    pub language: Language,
    pub block_kind: BlockKind,
    pub block_hash: TreeHash,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewPlanCoverage {
    pub total_blocks: usize,
    pub presented_blocks: usize,
    pub excerpted_blocks: usize,
    pub excerpt_lines: usize,
}

impl AiReviewPlanCoverage {
    pub const fn is_partial(&self) -> bool {
        self.presented_blocks < self.total_blocks || self.excerpted_blocks < self.presented_blocks
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
    ) -> Self {
        const MAX_BLOCKS: usize = 120;
        const MAX_LINES_PER_BLOCK: usize = 12;

        let total_blocks = blocks.len();
        let presented_blocks = total_blocks.min(MAX_BLOCKS);
        let mut remaining_lines = max_context_lines.max(1);
        let mut excerpted_blocks = 0;
        let mut excerpt_lines = 0;
        let presented = blocks
            .iter()
            .take(presented_blocks)
            .enumerate()
            .map(|(index, source)| {
                let mut block = source.clone();
                block.block_id = format!("B{:04}", index + 1);
                let lines = block
                    .content
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let included_lines = lines.len().min(MAX_LINES_PER_BLOCK).min(remaining_lines);
                if included_lines > 0 {
                    excerpted_blocks += 1;
                    excerpt_lines += included_lines;
                    remaining_lines -= included_lines;
                }
                (
                    block,
                    lines.into_iter().take(included_lines).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let coverage = AiReviewPlanCoverage {
            total_blocks,
            presented_blocks,
            excerpted_blocks,
            excerpt_lines,
        };
        let prompt = build_review_plan_prompt(&review_set_hash, scope_label, &coverage, &presented);

        Self {
            review_set_hash,
            coverage,
            prompt,
        }
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
    presented: &[(AiReviewPlanBlock, Vec<String>)],
) -> String {
    let suffix = if coverage.is_partial() {
        " (partial)"
    } else {
        " (all)"
    };
    let mut manifest = String::new();
    for (block, excerpt) in presented {
        let line_start = block.start_line.saturating_add(1);
        let line_end = block.end_line.max(block.start_line.saturating_add(1));
        manifest.push_str(&format!(
            "\n- {}\n  Path: {}\n  Language: {:?}\n  Block kind: {}\n  Block hash: {}\n  Lines: {line_start}-{line_end}\n",
            block.block_id,
            block.path,
            block.language,
            block.block_kind.as_str(),
            block.block_hash,
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
Do not include Markdown fences or any text outside the JSON object.",
        presented = coverage.presented_blocks,
        total = coverage.total_blocks,
        excerpt_lines = coverage.excerpt_lines,
        excerpted = coverage.excerpted_blocks,
    )
}

pub trait AiReviewPlanProvider: Send + Sync {
    fn plan(&self, request: &AiReviewPlanRequest) -> Result<AiReviewPlan>;
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
}

impl AiReviewPlanProvider for CommandAiReviewPlanProvider {
    fn plan(&self, request: &AiReviewPlanRequest) -> Result<AiReviewPlan> {
        let invocation = cli_invocation_for_review_plan(self.provider, &self.model, request)?;
        let output = run_cli_invocation(&invocation)?;
        AiReviewPlan::from_provider_text(&output.text)
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
        })
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
        let conversation_key = AiConversationKey::from_request(self.provider, &self.model, request);
        let cached_session_id = self.cached_session_id(&conversation_key);
        let invocation = cli_invocation_for_request_with_session(
            self.provider,
            &self.model,
            request,
            cached_session_id.as_deref(),
        )?;
        let output = match run_cli_invocation(&invocation) {
            Ok(output) => output,
            Err(error) if cached_session_id.is_some() => {
                self.forget_session_id(&conversation_key);
                let fresh_invocation = cli_invocation_for_request_with_session(
                    self.provider,
                    &self.model,
                    request,
                    None,
                )?;
                run_cli_invocation(&fresh_invocation)
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
    let prompt = request.prompt.clone();
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
            "model_reasoning_effort=\"low\"".to_string(),
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
            "model_reasoning_effort=\"low\"".to_string(),
            "--output-last-message".to_string(),
            final_message_path.to_string_lossy().to_string(),
        ]
    };
    if model != "auto" {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
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
        "model_reasoning_effort=\"medium\"".to_string(),
        "--output-last-message".to_string(),
        final_message_path.to_string_lossy().to_string(),
    ];
    if model != "auto" {
        args.push("--model".to_string());
        args.push(model.to_string());
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

fn run_cli_invocation(invocation: &AiCliInvocation) -> Result<AiCliRunOutput> {
    let mut child = Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {}", invocation.program))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(invocation.stdin.as_bytes())
            .with_context(|| format!("failed to write prompt to {}", invocation.program))?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {}", invocation.program))?;
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{} returned non-UTF-8 stdout", invocation.program))?;
    if !output.status.success() {
        return Err(anyhow!(
            "{} exited with {}: {}",
            invocation.program,
            output.status,
            collapse_whitespace(&String::from_utf8_lossy(&output.stderr))
        ));
    }

    let session_id = match invocation.output_format {
        AiCliOutputFormat::Text => None,
        AiCliOutputFormat::CodexJson | AiCliOutputFormat::ClaudeJson => {
            extract_session_id_from_json_text(&stdout)
        }
    };

    let text = if let Some(path) = invocation.final_message_path.as_ref() {
        let final_message = fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read {} final message from {}",
                invocation.program,
                path.display()
            )
        })?;
        let _cleanup = fs::remove_file(path);
        final_message
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
    let block_content = clipped_content(&context.content, max_context_lines);
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

fn clipped_content(content: &str, max_context_lines: usize) -> String {
    let max_context_lines = max_context_lines.max(1);
    let mut out = String::new();
    for (index, line) in content.lines().enumerate() {
        if index >= max_context_lines {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("...");
            break;
        }
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
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

fn first_sentence(text: &str) -> &str {
    for (index, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            return &text[..=index];
        }
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

fn parse_claude_json_result(stdout: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| "failed to parse claude JSON output")?;
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
            .first()
            .copied()
            .map(|provider| AiAvailability::Ready {
                provider,
                model: effective_model_for_provider(provider, &config.model),
            })
            .unwrap_or_else(|| AiAvailability::Unavailable {
                reason: format!(
                    "set {ANTHROPIC_API_KEY}, {OPENAI_API_KEY}, or install claude/codex CLI"
                ),
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
    if env.has_provider(provider) {
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
    fast_default_model_for_provider(provider).to_string()
}

pub fn fast_default_model_for_provider(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::Anthropic | AiProvider::ClaudeCli => "claude-3-5-haiku-latest",
        AiProvider::OpenAi => "gpt-5-mini",
        AiProvider::CodexCli => "auto",
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
    fn review_hint_prompt_contains_metadata_and_clipped_block_content() {
        let prompt = build_review_hint_prompt(&review_context("one\ntwo\nthree"), 2);

        assert!(prompt.contains("Return exactly two concise lines"));
        assert!(prompt.contains("within 90 visible characters"));
        assert!(prompt.contains("EXPLANATION:"));
        assert!(prompt.contains("CHANGE:"));
        assert!(prompt.contains("LGTM-style reassurance"));
        assert!(prompt.contains("Path: src/lib.rs"));
        assert!(prompt.contains("Language: Rust"));
        assert!(prompt.contains("Block kind: function"));
        assert!(prompt.contains("Lines: 5-8"));
        assert!(prompt.contains("one\ntwo\n..."));
        assert!(!prompt.contains("three"));
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
    fn no_change_suggestion_has_stable_lgtm_fallback_text() {
        let suggestion = AiSuggestion::from_provider_text("CHANGE: NONE")
            .unwrap_or_else(|error| panic!("expected suggestion: {error}"));

        assert_eq!(suggestion.proposed_change, None);
        assert_eq!(suggestion.explanation, None);
        assert_eq!(
            suggestion.visible_sentence(),
            Some("No change suggested; LGTM.")
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
    fn suggestion_request_prompt_includes_custom_response_character_limit() {
        let request = AiSuggestionRequest::with_response_char_limit(
            AiProvider::Anthropic,
            "claude-3-5-haiku".to_string(),
            review_set(),
            review_context("fn checked() {}"),
            40,
            72,
        );

        assert_eq!(request.key.max_response_chars, 72);
        assert!(request.prompt.contains("within 72 visible characters"));
    }

    #[test]
    fn codex_cli_invocation_uses_read_only_low_effort_exec_with_final_message_capture() {
        let request = AiSuggestionRequest::new(
            AiProvider::CodexCli,
            "auto".to_string(),
            review_set(),
            review_context("fn checked() {}"),
            80,
        );
        let invocation = match cli_invocation_for_request(AiProvider::CodexCli, "auto", &request) {
            Ok(invocation) => invocation,
            Err(error) => panic!("expected codex invocation: {error}"),
        };

        assert_eq!(invocation.program, "codex");
        assert_eq!(
            invocation.args[..9],
            [
                "exec",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--color",
                "never",
                "--json",
                "-c",
                "model_reasoning_effort=\"low\"",
            ]
        );
        assert!(
            invocation
                .args
                .iter()
                .any(|arg| arg == "--output-last-message")
        );
        assert!(invocation.final_message_path.is_some());
        assert_eq!(invocation.output_format, AiCliOutputFormat::CodexJson);
        assert!(!invocation.args.iter().any(|arg| arg == "--model"));
        assert!(invocation.stdin.contains("Review-set context"));
        assert!(invocation.stdin.contains("CHANGE: NONE"));
        assert!(invocation.stdin.contains("fn checked() {}"));
    }

    #[test]
    fn codex_resume_invocation_reuses_cached_session_without_resending_review_set_context() {
        let request = AiSuggestionRequest::new(
            AiProvider::CodexCli,
            "auto".to_string(),
            review_set(),
            review_context("fn checked() {}"),
            80,
        );
        let invocation = cli_invocation_for_request_with_session(
            AiProvider::CodexCli,
            "auto",
            &request,
            Some("session-123"),
        )
        .unwrap_or_else(|error| panic!("expected codex resume invocation: {error}"));

        assert_eq!(invocation.program, "codex");
        assert_eq!(invocation.args[0], "exec");
        assert_eq!(invocation.args[1], "resume");
        assert!(invocation.args.iter().any(|arg| arg == "session-123"));
        assert!(
            invocation
                .stdin
                .contains("already provided in this conversation")
        );
        assert!(!invocation.stdin.contains("Scope: diff vs main"));
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
    fn claude_cli_invocation_uses_print_mode_and_omits_auto_model_arg() {
        let request = AiSuggestionRequest::new(
            AiProvider::ClaudeCli,
            "auto".to_string(),
            review_set(),
            review_context("fn checked() {}"),
            80,
        );
        let invocation = match cli_invocation_for_request(AiProvider::ClaudeCli, "auto", &request) {
            Ok(invocation) => invocation,
            Err(error) => panic!("expected claude invocation: {error}"),
        };

        assert_eq!(invocation.program, "claude");
        assert_eq!(invocation.args, vec!["--print", "--output-format", "json"]);
        assert_eq!(invocation.output_format, AiCliOutputFormat::ClaudeJson);
        assert!(!invocation.args.iter().any(|arg| arg == "--model"));
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
            path: format!("src/file_{index}.rs"),
            language: Language::Rust,
            block_kind: BlockKind::Function,
            block_hash: TreeHash::from_content(&content),
            start_line: index + 1,
            end_line: index + 3,
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
        )
    }

    #[test]
    fn review_plan_prompt_contains_scope_hash_coverage_manifest_and_json_contract() {
        let blocks = vec![review_plan_block(0, "fn first() {}\n".to_string())];
        let request = review_plan_request(&blocks, 80);

        assert!(request.prompt.contains("Review-set hash:"));
        assert!(request.prompt.contains("Scope: changed files"));
        assert!(request.prompt.contains("Context: 1/1 blocks listed (all)"));
        assert!(request.prompt.contains("B0001"));
        assert!(request.prompt.contains("src/file_0.rs"));
        assert!(request.prompt.contains("\"summary\""));
        assert!(request.prompt.contains("\"global_checks\""));
        assert!(request.prompt.contains("\"priority_blocks\""));
        assert!(request.prompt.contains("exactly one JSON object"));
        assert!(request.prompt.contains("Do not run tools"));
    }

    #[test]
    fn review_plan_request_enforces_block_and_excerpt_bounds() {
        let blocks = (0..125)
            .map(|index| review_plan_block(index, "a\nb\nc\nd\ne\nf\n".to_string()))
            .collect::<Vec<_>>();
        let request = review_plan_request(&blocks, 5);

        assert_eq!(request.coverage.total_blocks, 125);
        assert_eq!(request.coverage.presented_blocks, 120);
        assert_eq!(request.coverage.excerpted_blocks, 1);
        assert_eq!(request.coverage.excerpt_lines, 5);
        assert!(request.coverage.is_partial());
        assert!(request.prompt.contains("B0001"));
        assert!(request.prompt.contains("B0120"));
        assert!(!request.prompt.contains("B0121"));
        assert!(
            request
                .prompt
                .contains("(excerpt omitted by context budget)")
        );
        assert!(!request.prompt.contains("B0001\nPath: src/file_0.rs"));
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
    fn review_plan_cli_invocation_is_fresh_read_only_medium_effort() {
        let blocks = vec![review_plan_block(0, "fn first() {}".to_string())];
        let request = review_plan_request(&blocks, 80);
        let invocation =
            cli_invocation_for_review_plan(AiProvider::CodexCli, "gpt-5.6-sol", &request)
                .unwrap_or_else(|error| panic!("build review-plan invocation: {error}"));

        assert_eq!(invocation.program, "codex");
        assert!(
            invocation
                .args
                .windows(2)
                .any(|args| args == ["--sandbox", "read-only"])
        );
        assert!(
            invocation
                .args
                .iter()
                .any(|arg| arg == r#"model_reasoning_effort="medium""#)
        );
        assert!(!invocation.args.iter().any(|arg| arg == "resume"));
        assert!(invocation.stdin.contains("B0001"));
    }

    #[test]
    fn effective_model_uses_fast_provider_default_for_auto_model() {
        assert_eq!(
            effective_model_for_provider(AiProvider::CodexCli, "auto"),
            "auto"
        );
        assert_eq!(
            effective_model_for_provider(AiProvider::ClaudeCli, "auto"),
            "claude-3-5-haiku-latest"
        );
        assert_eq!(
            effective_model_for_provider(AiProvider::CodexCli, "gpt-4.1-mini"),
            "gpt-4.1-mini"
        );
    }

    #[test]
    fn detection_prefers_api_keys_before_cli_tools() {
        let env = AiEnvironment::for_tests(true, true, ["claude", "codex"]);

        assert_eq!(
            env.detected_providers(),
            vec![
                AiProvider::Anthropic,
                AiProvider::OpenAi,
                AiProvider::ClaudeCli,
                AiProvider::CodexCli,
            ]
        );
    }

    #[test]
    fn disabled_modeline_mentions_detected_providers_without_enabling_calls() {
        let env = AiEnvironment::for_tests(true, false, ["claude"]);
        let availability =
            resolve_ai_availability(&config(AiMode::Off, AiProviderConfig::Auto), &env);

        assert_eq!(
            availability.modeline_text(),
            "AI: off (Anthropic, Claude CLI detected; set [ai].mode = \"review_plan\")"
        );
    }

    #[test]
    fn auto_provider_selects_first_detected_provider_when_enabled() {
        let env = AiEnvironment::for_tests(false, true, ["claude"]);
        let availability =
            resolve_ai_availability(&config(AiMode::ReviewPlan, AiProviderConfig::Auto), &env);

        assert_eq!(
            availability,
            AiAvailability::Ready {
                provider: AiProvider::OpenAi,
                model: "gpt-5-mini".to_string(),
            }
        );
        assert_eq!(
            availability.modeline_text(),
            "AI: ready (OpenAI / gpt-5-mini)"
        );
    }

    #[test]
    fn explicit_provider_reports_unavailable_when_missing() {
        let env = AiEnvironment::for_tests(false, false, []);
        let availability = resolve_ai_availability(
            &config(AiMode::ReviewPlan, AiProviderConfig::Anthropic),
            &env,
        );

        assert_eq!(
            availability,
            AiAvailability::Unavailable {
                reason: "Anthropic credentials or executable not found".to_string(),
                detected: Vec::new(),
            }
        );
    }
}
