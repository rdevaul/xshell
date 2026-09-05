use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use xshell_audit::AuditConfig;
use xshell_session::{ModelBinding, SessionConfig};
pub use xshell_view::{OutputMode, RenderingConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Ollama,
    Openai,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: Provider,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// History budget for this model, overriding `session_fabric.compaction`.
    /// Size it to the model's context window; 0 disables compaction.
    #[serde(default)]
    pub max_history_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XshellConfig {
    pub default_model: Option<String>,
    #[serde(default)]
    pub rendering: RenderingConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    #[serde(default)]
    pub session_fabric: SessionConfig,
    #[serde(default)]
    pub models: BTreeMap<String, ModelProfile>,
}

impl ActiveModel {
    pub fn to_session_binding(&self) -> ModelBinding {
        ModelBinding {
            profile_name: self.profile_name.clone(),
            provider: match self.provider {
                Provider::Ollama => "ollama",
                Provider::Openai => "openai",
            }
            .into(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            max_history_bytes: self.max_history_bytes,
        }
    }

    pub fn from_session_binding(binding: ModelBinding) -> Result<Self> {
        let provider = match binding.provider.as_str() {
            "ollama" => Provider::Ollama,
            "openai" => Provider::Openai,
            value => bail!("session uses unsupported provider {value:?}"),
        };
        let api_key_env = binding
            .api_key_env
            .map(|value| validate_api_key_env(value, binding.profile_name.as_deref()))
            .transpose()?;
        Ok(Self {
            profile_name: binding.profile_name,
            provider,
            model: binding.model,
            base_url: binding.base_url,
            api_key_env,
            max_history_bytes: binding.max_history_bytes,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModelOverrides {
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveModel {
    pub profile_name: Option<String>,
    pub provider: Provider,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub max_history_bytes: Option<usize>,
}

impl XshellConfig {
    pub fn load(explicit_path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = match explicit_path {
            Some(path) => path.to_owned(),
            None => default_config_path()?,
        };
        if !path.exists() {
            if explicit_path.is_some() {
                bail!("configuration file {} does not exist", path.display());
            }
            return Ok((Self::default(), path));
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read configuration file {}", path.display()))?;
        let config = toml::from_str(&source)
            .with_context(|| format!("invalid configuration file {}", path.display()))?;
        Ok((config, path))
    }

    pub fn resolve_startup(
        &self,
        requested_profile: Option<&str>,
        overrides: ModelOverrides,
    ) -> Result<ActiveModel> {
        let selected_name = requested_profile
            .map(str::to_owned)
            .or_else(|| self.default_model.clone());
        let selected = selected_name
            .as_deref()
            .map(|name| self.profile(name))
            .transpose()?;

        let provider = overrides
            .provider
            .or_else(|| selected.map(|profile| profile.provider))
            .unwrap_or(Provider::Ollama);
        let model = overrides
            .model
            .or_else(|| selected.map(|profile| profile.model.clone()))
            .unwrap_or_else(|| "qwen3:8b".into());
        let base_url = overrides
            .base_url
            .or_else(|| selected.and_then(|profile| profile.base_url.clone()))
            .unwrap_or_else(|| default_base_url(provider).into());
        let mut api_key_env = overrides
            .api_key_env
            .or_else(|| selected.and_then(|profile| profile.api_key_env.clone()))
            .map(|name| validate_api_key_env(name, selected_name.as_deref()))
            .transpose()?;
        if api_key_env.is_none() && selected.is_none() && provider == Provider::Openai {
            api_key_env = Some("OPENAI_API_KEY".into());
        }

        Ok(ActiveModel {
            profile_name: selected_name,
            provider,
            model,
            base_url,
            api_key_env,
            max_history_bytes: selected.and_then(|profile| profile.max_history_bytes),
        })
    }

    pub fn resolve_profile(&self, name: &str) -> Result<ActiveModel> {
        let profile = self.profile(name)?;
        Ok(ActiveModel {
            profile_name: Some(name.into()),
            provider: profile.provider,
            model: profile.model.clone(),
            base_url: profile
                .base_url
                .clone()
                .unwrap_or_else(|| default_base_url(profile.provider).into()),
            api_key_env: profile
                .api_key_env
                .clone()
                .map(|value| validate_api_key_env(value, Some(name)))
                .transpose()?,
            max_history_bytes: profile.max_history_bytes,
        })
    }

    fn profile(&self, name: &str) -> Result<&ModelProfile> {
        self.models
            .get(name)
            .with_context(|| format!("unknown model profile {name:?}"))
    }
}

fn validate_api_key_env(value: String, profile_name: Option<&str>) -> Result<String> {
    let mut characters = value.chars();
    let valid_first = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    let valid_rest =
        characters.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if valid_first && valid_rest {
        return Ok(value);
    }

    let location = profile_name
        .map(|name| format!(" in model profile {name:?}"))
        .unwrap_or_default();
    bail!(
        "invalid api_key_env{location}; expected an environment variable name such as \
OPENROUTER_API_KEY, not an API key"
    )
}

pub fn default_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XSHELL_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/xshell/config.toml"))
}

fn default_base_url(provider: Provider) -> &'static str {
    match provider {
        Provider::Ollama => "http://127.0.0.1:11434",
        Provider::Openai => "https://api.openai.com",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> XshellConfig {
        toml::from_str(
            r#"
default_model = "local"

[rendering]
markdown = "always"
color = "never"
width = 100

[models.local]
provider = "ollama"
model = "qwen3:8b"

[models.router]
provider = "openai"
model = "openrouter/free"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_default_profile() {
        let active = sample()
            .resolve_startup(
                None,
                ModelOverrides {
                    provider: None,
                    model: None,
                    base_url: None,
                    api_key_env: None,
                },
            )
            .unwrap();
        assert_eq!(active.profile_name.as_deref(), Some("local"));
        assert_eq!(active.provider, Provider::Ollama);
        assert_eq!(active.base_url, "http://127.0.0.1:11434");
        assert_eq!(sample().rendering.markdown, OutputMode::Always);
        assert_eq!(sample().rendering.color, OutputMode::Never);
        assert_eq!(sample().rendering.width, Some(100));
    }

    #[test]
    fn per_model_history_budget_travels_with_the_binding() {
        let config: XshellConfig = toml::from_str(
            r#"
            [session_fabric.compaction]
            max_history_bytes = 1000000
            [models.small]
            provider = "ollama"
            model = "qwen3:4b"
            max_history_bytes = 8192
            [models.big]
            provider = "ollama"
            model = "qwen3:235b"
            "#,
        )
        .unwrap();
        let small = config.resolve_profile("small").unwrap();
        let big = config.resolve_profile("big").unwrap();
        assert_eq!(small.max_history_bytes, Some(8192));
        assert_eq!(big.max_history_bytes, None);

        // Round-trip through the session protocol type.
        let binding = small.to_session_binding();
        assert_eq!(binding.max_history_bytes, Some(8192));
        let restored = ActiveModel::from_session_binding(binding).unwrap();
        assert_eq!(restored.max_history_bytes, Some(8192));

        // Resolution: profile overrides the session default; absent falls back.
        let session = &config.session_fabric.compaction;
        assert_eq!(
            session.for_model(small.max_history_bytes).max_history_bytes,
            Some(8192)
        );
        assert_eq!(
            session.for_model(big.max_history_bytes).max_history_bytes,
            Some(1_000_000)
        );
    }

    #[test]
    fn rendering_defaults_preserve_existing_configurations() {
        let config: XshellConfig = toml::from_str("").unwrap();
        assert_eq!(config.rendering, RenderingConfig::default());
    }

    #[test]
    fn resolves_openrouter_profile() {
        let active = sample().resolve_profile("router").unwrap();
        assert_eq!(active.provider, Provider::Openai);
        assert_eq!(active.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(active.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    }

    #[test]
    fn command_line_values_override_profile() {
        let active = sample()
            .resolve_startup(
                Some("local"),
                ModelOverrides {
                    provider: Some(Provider::Openai),
                    model: Some("custom/model".into()),
                    base_url: Some("http://localhost:9000/v1".into()),
                    api_key_env: Some("CUSTOM_KEY".into()),
                },
            )
            .unwrap();
        assert_eq!(active.provider, Provider::Openai);
        assert_eq!(active.model, "custom/model");
        assert_eq!(active.base_url, "http://localhost:9000/v1");
    }

    #[test]
    fn rejects_a_secret_in_api_key_env_without_echoing_it() {
        let secret = "sk-or-v1-private-value";
        let error = validate_api_key_env(secret.into(), Some("router"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("environment variable name"));
        assert!(!error.contains(secret));
    }
}
