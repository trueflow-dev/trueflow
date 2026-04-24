use crate::config::{AiConfig, AiProviderConfig};
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
