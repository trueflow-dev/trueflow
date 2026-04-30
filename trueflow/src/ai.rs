use crate::analysis::Language;
use crate::block::BlockKind;
use crate::config::{AiConfig, AiProviderConfig};
use crate::hashing::TreeHash;
use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const OPENAI_API_KEY: &str = "OPENAI_API_KEY";

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
                "AI: off ({} detected; set [ai].enabled = true)",
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
    pub path: String,
    pub block_hash: TreeHash,
    pub start_line: usize,
    pub end_line: usize,
    pub max_context_lines: usize,
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
    pub context: AiReviewContext,
    pub prompt: String,
}

impl AiSuggestionRequest {
    pub fn new(
        provider: AiProvider,
        model: String,
        context: AiReviewContext,
        max_context_lines: usize,
    ) -> Self {
        let key = AiSuggestionKey {
            provider,
            model,
            path: context.path.clone(),
            block_hash: context.block_hash.clone(),
            start_line: context.start_line,
            end_line: context.end_line,
            max_context_lines,
        };
        let prompt = build_review_hint_prompt(&context, max_context_lines);
        Self {
            key,
            context,
            prompt,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSuggestion {
    pub sentence: String,
}

impl AiSuggestion {
    pub fn from_provider_text(raw: &str) -> Result<Self> {
        let collapsed = collapse_whitespace(raw);
        if collapsed.is_empty() {
            return Err(anyhow!("AI provider returned an empty suggestion"));
        }
        let sentence = truncate_for_modeline(first_sentence(&collapsed), 180);
        Ok(Self { sentence })
    }
}

pub trait AiSuggestionProvider: Send + Sync {
    fn suggest(&self, request: &AiSuggestionRequest) -> Result<AiSuggestion>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCliInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: String,
}

#[derive(Debug, Clone)]
pub struct CommandAiSuggestionProvider {
    provider: AiProvider,
    model: String,
}

impl CommandAiSuggestionProvider {
    pub fn new(provider: AiProvider, model: String) -> Result<Self> {
        if !matches!(provider, AiProvider::ClaudeCli | AiProvider::CodexCli) {
            return Err(anyhow!(
                "{} does not have a CLI suggestion provider",
                provider.label()
            ));
        }
        Ok(Self { provider, model })
    }
}

impl AiSuggestionProvider for CommandAiSuggestionProvider {
    fn suggest(&self, request: &AiSuggestionRequest) -> Result<AiSuggestion> {
        let invocation = cli_invocation_for_request(self.provider, &self.model, request)?;
        let output = run_cli_invocation(&invocation)?;
        AiSuggestion::from_provider_text(&output)
    }
}

pub fn cli_invocation_for_request(
    provider: AiProvider,
    model: &str,
    request: &AiSuggestionRequest,
) -> Result<AiCliInvocation> {
    let prompt = cli_prompt_for_request(request);
    match provider {
        AiProvider::CodexCli => Ok(codex_invocation(model, prompt)),
        AiProvider::ClaudeCli => Ok(claude_invocation(model, prompt)),
        AiProvider::Anthropic | AiProvider::OpenAi => Err(anyhow!(
            "{} direct API suggestions are not implemented yet",
            provider.label()
        )),
    }
}

fn cli_prompt_for_request(request: &AiSuggestionRequest) -> String {
    format!(
        "{}\n\nImportant: return only the one-sentence review hint. Do not run shell commands, inspect additional files, modify files, or produce markdown fences.",
        request.prompt
    )
}

fn codex_invocation(model: &str, prompt: String) -> AiCliInvocation {
    let mut args = vec![
        "exec".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--ephemeral".to_string(),
        "--skip-git-repo-check".to_string(),
        "--color".to_string(),
        "never".to_string(),
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
    }
}

fn claude_invocation(model: &str, prompt: String) -> AiCliInvocation {
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
    ];
    if model != "auto" {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    AiCliInvocation {
        program: "claude".to_string(),
        args,
        stdin: prompt,
    }
}

fn run_cli_invocation(invocation: &AiCliInvocation) -> Result<String> {
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
    if !output.status.success() {
        return Err(anyhow!(
            "{} exited with {}: {}",
            invocation.program,
            output.status,
            collapse_whitespace(&String::from_utf8_lossy(&output.stderr))
        ));
    }

    String::from_utf8(output.stdout)
        .with_context(|| format!("{} returned non-UTF-8 output", invocation.program))
}

pub fn build_review_hint_prompt(context: &AiReviewContext, max_context_lines: usize) -> String {
    let line_start = context.start_line.saturating_add(1);
    let line_end = context.end_line.max(context.start_line.saturating_add(1));
    let block_content = clipped_content(&context.content, max_context_lines);
    format!(
        "Return exactly one concise sentence that helps a code reviewer decide whether to approve or comment. If no issue is obvious, say that it looks good and why. Do not propose code edits.\n\nPath: {}\nLanguage: {:?}\nBlock kind: {}\nLines: {line_start}-{line_end}\n\n```\n{block_content}\n```",
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
    if !config.enabled {
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
        AiProvider::OpenAi | AiProvider::CodexCli => "gpt-5-mini",
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

    fn config(enabled: bool, provider: AiProviderConfig) -> AiConfig {
        AiConfig {
            enabled,
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

    #[test]
    fn review_hint_prompt_contains_metadata_and_clipped_block_content() {
        let prompt = build_review_hint_prompt(&review_context("one\ntwo\nthree"), 2);

        assert!(prompt.contains("Return exactly one concise sentence"));
        assert!(prompt.contains("Path: src/lib.rs"));
        assert!(prompt.contains("Language: Rust"));
        assert!(prompt.contains("Block kind: function"));
        assert!(prompt.contains("Lines: 5-8"));
        assert!(prompt.contains("one\ntwo\n..."));
        assert!(!prompt.contains("three"));
    }

    #[test]
    fn suggestion_normalization_keeps_only_first_sentence_for_modeline() {
        let suggestion = AiSuggestion::from_provider_text(
            "  consider asking why unwrap is safe. second sentence should not render. ",
        )
        .unwrap_or_else(|error| panic!("expected suggestion: {error}"));

        assert_eq!(suggestion.sentence, "consider asking why unwrap is safe.");
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
    fn suggestion_request_key_includes_provider_model_and_context_limit() {
        let request = AiSuggestionRequest::new(
            AiProvider::Anthropic,
            "claude-3-5-haiku".to_string(),
            review_context("fn checked() {}"),
            40,
        );

        assert_eq!(request.key.provider, AiProvider::Anthropic);
        assert_eq!(request.key.model, "claude-3-5-haiku");
        assert_eq!(request.key.path, "src/lib.rs");
        assert_eq!(request.key.start_line, 4);
        assert_eq!(request.key.end_line, 8);
        assert_eq!(request.key.max_context_lines, 40);
    }

    #[test]
    fn codex_cli_invocation_uses_read_only_ephemeral_exec_with_stdin_prompt() {
        let request = AiSuggestionRequest::new(
            AiProvider::CodexCli,
            "gpt-5-mini".to_string(),
            review_context("fn checked() {}"),
            80,
        );
        let invocation =
            match cli_invocation_for_request(AiProvider::CodexCli, "gpt-5-mini", &request) {
                Ok(invocation) => invocation,
                Err(error) => panic!("expected codex invocation: {error}"),
            };

        assert_eq!(invocation.program, "codex");
        assert_eq!(
            invocation.args,
            vec![
                "exec",
                "--sandbox",
                "read-only",
                "--ephemeral",
                "--skip-git-repo-check",
                "--color",
                "never",
                "--model",
                "gpt-5-mini",
                "-",
            ]
        );
        assert!(
            invocation
                .stdin
                .contains("return only the one-sentence review hint")
        );
        assert!(invocation.stdin.contains("fn checked() {}"));
    }

    #[test]
    fn claude_cli_invocation_uses_print_mode_and_omits_auto_model_arg() {
        let request = AiSuggestionRequest::new(
            AiProvider::ClaudeCli,
            "auto".to_string(),
            review_context("fn checked() {}"),
            80,
        );
        let invocation = match cli_invocation_for_request(AiProvider::ClaudeCli, "auto", &request) {
            Ok(invocation) => invocation,
            Err(error) => panic!("expected claude invocation: {error}"),
        };

        assert_eq!(invocation.program, "claude");
        assert_eq!(invocation.args, vec!["--print", "--output-format", "text"]);
        assert!(!invocation.args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn direct_api_provider_invocation_is_rejected_until_http_provider_exists() {
        let request = AiSuggestionRequest::new(
            AiProvider::Anthropic,
            "auto".to_string(),
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

    #[test]
    fn effective_model_uses_fast_provider_default_for_auto_model() {
        assert_eq!(
            effective_model_for_provider(AiProvider::CodexCli, "auto"),
            "gpt-5-mini"
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
        let availability = resolve_ai_availability(&config(false, AiProviderConfig::Auto), &env);

        assert_eq!(
            availability.modeline_text(),
            "AI: off (Anthropic, Claude CLI detected; set [ai].enabled = true)"
        );
    }

    #[test]
    fn auto_provider_selects_first_detected_provider_when_enabled() {
        let env = AiEnvironment::for_tests(false, true, ["claude"]);
        let availability = resolve_ai_availability(&config(true, AiProviderConfig::Auto), &env);

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
        let availability =
            resolve_ai_availability(&config(true, AiProviderConfig::Anthropic), &env);

        assert_eq!(
            availability,
            AiAvailability::Unavailable {
                reason: "Anthropic credentials or executable not found".to_string(),
                detected: Vec::new(),
            }
        );
    }
}
