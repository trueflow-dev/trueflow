use crate::analysis::Language;
use crate::block::BlockKind;
use crate::config::{AiConfig, AiProviderConfig};
use crate::hashing::TreeHash;
use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::path::Path;

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
                model: config.model.clone(),
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
            resolve_explicit_provider(AiProvider::Anthropic, config.model.clone(), env, detected)
        }
        AiProviderConfig::OpenAi => {
            resolve_explicit_provider(AiProvider::OpenAi, config.model.clone(), env, detected)
        }
        AiProviderConfig::ClaudeCli => {
            resolve_explicit_provider(AiProvider::ClaudeCli, config.model.clone(), env, detected)
        }
        AiProviderConfig::CodexCli => {
            resolve_explicit_provider(AiProvider::CodexCli, config.model.clone(), env, detected)
        }
    }
}

fn resolve_explicit_provider(
    provider: AiProvider,
    model: String,
    env: &AiEnvironment,
    detected: Vec<AiProvider>,
) -> AiAvailability {
    if env.has_provider(provider) {
        AiAvailability::Ready { provider, model }
    } else {
        AiAvailability::Unavailable {
            reason: format!("{} credentials or executable not found", provider.label()),
            detected,
        }
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
                model: "auto".to_string(),
            }
        );
        assert_eq!(availability.modeline_text(), "AI: ready (OpenAI)");
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
