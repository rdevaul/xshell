use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

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
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XshellConfig {
    pub default_model: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelProfile>,
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
    pub api_key_env: String,
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
        let api_key_env = overrides
            .api_key_env
            .or_else(|| selected.and_then(|profile| profile.api_key_env.clone()))
            .unwrap_or_else(|| "OPENAI_API_KEY".into());

        Ok(ActiveModel {
            profile_name: selected_name,
            provider,
            model,
            base_url,
            api_key_env,
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
                .unwrap_or_else(|| "OPENAI_API_KEY".into()),
        })
    }

    fn profile(&self, name: &str) -> Result<&ModelProfile> {
        self.models
            .get(name)
            .with_context(|| format!("unknown model profile {name:?}"))
    }
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
    }

    #[test]
    fn resolves_openrouter_profile() {
        let active = sample().resolve_profile("router").unwrap();
        assert_eq!(active.provider, Provider::Openai);
        assert_eq!(active.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(active.api_key_env, "OPENROUTER_API_KEY");
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
}
