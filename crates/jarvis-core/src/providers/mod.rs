//! LLM provider registry and factory.

pub mod agy_cli;
pub mod ollama;
pub mod openai_compat;
pub mod traits;

use crate::config::{AppConfig, ProviderKind, SecurityMode};
use crate::secrets::{keys, SecretStore};
use agy_cli::AgyCliBridge;
use ollama::OllamaProvider;
use openai_compat::OpenAiCompatProvider;
use std::sync::Arc;
use traits::{LlmProvider, ProviderError};

/// Resolves and constructs [`LlmProvider`] instances from [`AppConfig`] and [`SecretStore`].
#[derive(Clone)]
pub struct ProviderRegistry {
    secrets: Arc<dyn SecretStore>,
}

impl ProviderRegistry {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self { secrets }
    }

    /// Build a provider for the requested [`ProviderKind`].
    pub fn build_provider(
        &self,
        kind: ProviderKind,
        config: &AppConfig,
    ) -> Result<Arc<dyn LlmProvider>, ProviderError> {
        match kind {
            ProviderKind::OpenAiCompat => {
                let api_key = self.resolve_openai_compat_api_key(&config.openai_base_url)?;
                Ok(Arc::new(OpenAiCompatProvider::new(
                    config.openai_base_url.clone(),
                    api_key,
                )))
            }
            ProviderKind::Ollama => Ok(Arc::new(OllamaProvider::new(
                config.ollama_base_url.clone(),
            ))),
            ProviderKind::AgyCli => Ok(Arc::new(AgyCliBridge::new(
                config.workspace_root.clone(),
                config.agy_effort.clone(),
                config.security_mode == SecurityMode::FullAuto,
            ))),
        }
    }

    /// Resolve the API key for an OpenAI-compatible endpoint, prioritizing
    /// `GROQ_API_KEY` when targeting `groq.com` and `OPENAI_API_KEY` otherwise.
    fn resolve_openai_compat_api_key(
        &self,
        base_url: &str,
    ) -> Result<Option<String>, ProviderError> {
        let is_groq = base_url.to_ascii_lowercase().contains("groq.com");
        let (primary, fallback) = if is_groq {
            (keys::GROQ_API_KEY, Some(keys::OPENAI_API_KEY))
        } else {
            (keys::OPENAI_API_KEY, None)
        };

        if let Some(key) = self.get_non_empty_secret(primary)? {
            return Ok(Some(key));
        }
        if let Some(fallback_key) = fallback {
            if let Some(key) = self.get_non_empty_secret(fallback_key)? {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }

    fn get_non_empty_secret(&self, key_name: &str) -> Result<Option<String>, ProviderError> {
        Ok(self
            .secrets
            .get(key_name)
            .map_err(|e| ProviderError::Config(e.to_string()))?
            .filter(|v| !v.trim().is_empty()))
    }

    /// Build the currently active provider configured in [`AppConfig`].
    pub fn active_provider(&self, config: &AppConfig) -> Result<Arc<dyn LlmProvider>, ProviderError> {
        self.build_provider(config.active_provider, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::InMemorySecretStore;

    #[test]
    fn openai_compat_resolves_groq_key_for_groq_url_and_openai_key_otherwise() {
        let secrets = Arc::new(InMemorySecretStore::with_entries([
            (keys::GROQ_API_KEY, "dummy-groq-key"),
            (keys::OPENAI_API_KEY, "dummy-openai-key"),
        ]));
        let registry = ProviderRegistry::new(secrets);

        assert_eq!(
            registry
                .resolve_openai_compat_api_key("https://api.groq.com/openai/v1")
                .unwrap()
                .as_deref(),
            Some("dummy-groq-key")
        );
        assert_eq!(
            registry
                .resolve_openai_compat_api_key("https://API.GROQ.COM/openai/v1")
                .unwrap()
                .as_deref(),
            Some("dummy-groq-key")
        );
        assert_eq!(
            registry
                .resolve_openai_compat_api_key("https://api.openai.com/v1")
                .unwrap()
                .as_deref(),
            Some("dummy-openai-key")
        );

        let groq_fallback_secrets = Arc::new(InMemorySecretStore::with_entries([
            (keys::GROQ_API_KEY, "  "),
            (keys::OPENAI_API_KEY, "dummy-openai-fallback"),
        ]));
        let groq_fallback_registry = ProviderRegistry::new(groq_fallback_secrets);
        assert_eq!(
            groq_fallback_registry
                .resolve_openai_compat_api_key("https://api.groq.com/openai/v1")
                .unwrap()
                .as_deref(),
            Some("dummy-openai-fallback")
        );
    }
}
