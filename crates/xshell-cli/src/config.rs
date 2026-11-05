use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use xshell_audit::AuditConfig;
use xshell_execution::{LimitSettings, TurnLimits};
use xshell_release::ReleaseSource;
use xshell_session::{ModelBinding, SessionConfig};
pub use xshell_view::{OutputMode, RenderingConfig};

use crate::view::ViewConfig;

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
    /// The model's context window in tokens. When set, the enforced history
    /// budget is additionally capped by what this window can hold, so a
    /// `max_history_bytes` that is too large for the model cannot overflow it.
    #[serde(default)]
    pub context_tokens: Option<usize>,
    /// Tokens held back for the model's own reply when `context_tokens` is
    /// set. Defaults to `DEFAULT_RESERVE_OUTPUT_TOKENS`.
    #[serde(default)]
    pub reserve_output_tokens: Option<usize>,
    /// Limit profile for turns with this model, unless the command line or
    /// `//limits` chose one.
    #[serde(default)]
    pub limits: Option<String>,
}

/// Turn limits: which profile applies when nothing else chooses one, and
/// the profiles themselves. `default` and `relaxed` are built in; a table
/// with either name overrides individual limits of the built-in, and a
/// table with another name defines a profile that starts from `default`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub limits: Option<String>,
    #[serde(default)]
    pub limit_profiles: BTreeMap<String, LimitSettings>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XshellConfig {
    pub default_model: Option<String>,
    #[serde(default)]
    pub rendering: RenderingConfig,
    #[serde(default)]
    pub(crate) view: ViewConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    pub remote_bootstrap: Option<ReleaseSource>,
    #[serde(default)]
    pub session_fabric: SessionConfig,
    #[serde(default)]
    pub agent: AgentConfig,
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
            context_tokens: self.context_tokens,
            reserve_output_tokens: self.reserve_output_tokens,
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
            context_tokens: binding.context_tokens,
            reserve_output_tokens: binding.reserve_output_tokens,
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
    pub context_tokens: Option<usize>,
    pub reserve_output_tokens: Option<usize>,
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
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("invalid configuration file {}", path.display()))?;
        config
            .view
            .validate()
            .with_context(|| format!("invalid configuration file {}", path.display()))?;
        if let Some(source) = &config.remote_bootstrap {
            source
                .validate()
                .with_context(|| format!("invalid configuration file {}", path.display()))?;
        }
        config
            .validate_limits()
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
            context_tokens: selected.and_then(|profile| profile.context_tokens),
            reserve_output_tokens: selected.and_then(|profile| profile.reserve_output_tokens),
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
            context_tokens: profile.context_tokens,
            reserve_output_tokens: profile.reserve_output_tokens,
        })
    }

    /// The limits of the named profile.
    pub fn limit_profile(&self, name: &str) -> Result<TurnLimits> {
        let profiles = &self.agent.limit_profiles;
        let default = match profiles.get("default") {
            Some(settings) => settings
                .apply_to(TurnLimits::default())
                .context("in limit profile \"default\"")?,
            None => TurnLimits::default(),
        };
        if name == "default" {
            return Ok(default);
        }
        let builtin = TurnLimits::builtin(name);
        match (profiles.get(name), builtin) {
            (Some(settings), base) => settings
                .apply_to(base.unwrap_or(default))
                .with_context(|| format!("in limit profile {name:?}")),
            (None, Some(limits)) => Ok(limits),
            (None, None) => bail!(
                "unknown limit profile {name:?}; known profiles: {}",
                self.limit_profile_names().join(", ")
            ),
        }
    }

    /// Every profile name: the built-in ones, then the configured ones.
    pub fn limit_profile_names(&self) -> Vec<String> {
        let mut names = vec!["default".to_owned(), "relaxed".to_owned()];
        for name in self.agent.limit_profiles.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }

    /// The limit profile for a turn: an explicit choice (the command line or
    /// `//limits`), else the active model profile's, else `agent.limits`,
    /// else `default`.
    pub fn limits_name_for(&self, explicit: Option<&str>, active: &ActiveModel) -> String {
        explicit
            .map(str::to_owned)
            .or_else(|| {
                active
                    .profile_name
                    .as_deref()
                    .and_then(|name| self.models.get(name))
                    .and_then(|profile| profile.limits.clone())
            })
            .or_else(|| self.agent.limits.clone())
            .unwrap_or_else(|| "default".into())
    }

    fn validate_limits(&self) -> Result<()> {
        for name in self.limit_profile_names() {
            self.limit_profile(&name)?;
        }
        if let Some(name) = &self.agent.limits {
            self.limit_profile(name).context("in agent.limits")?;
        }
        for (model, profile) in &self.models {
            if let Some(name) = &profile.limits {
                self.limit_profile(name)
                    .with_context(|| format!("in model profile {model:?}"))?;
            }
        }
        Ok(())
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
    use crate::view::PaginationMode;

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
            session
                .for_model(xshell_execution::ModelBudget {
                    max_history_bytes: small.max_history_bytes,
                    ..Default::default()
                })
                .max_history_bytes,
            Some(8192)
        );
        assert_eq!(
            session
                .for_model(xshell_execution::ModelBudget {
                    max_history_bytes: big.max_history_bytes,
                    ..Default::default()
                })
                .max_history_bytes,
            Some(1_000_000)
        );
    }

    #[test]
    fn a_models_context_window_caps_its_history_budget() {
        // The misconfiguration this guardrail exists for: a byte budget that
        // reads like a token count, paired with a model that cannot hold it.
        let config: XshellConfig = toml::from_str(
            r#"
            [session_fabric.compaction]
            max_history_bytes = 262144
            [models.small]
            provider = "openai"
            model = "qwen3:8b"
            base_url = "http://127.0.0.1:8080/v1"
            context_tokens = 8192
            "#,
        )
        .unwrap();
        let small = config.resolve_profile("small").unwrap();
        assert_eq!(small.context_tokens, Some(8192));

        let enforced = config
            .session_fabric
            .compaction
            .for_model(xshell_execution::ModelBudget {
                max_history_bytes: small.max_history_bytes,
                context_tokens: small.context_tokens,
                reserve_output_tokens: small.reserve_output_tokens,
            })
            .effective_max_bytes()
            .expect("a declared window always enforces a budget");
        let derived = (8192 - 2 * 1024) * 9 / 10 * xshell_execution::BYTES_PER_TOKEN;
        assert_eq!(enforced, derived, "the declared window must cap the budget");
        assert!(enforced < 262144);

        // The window travels with the session binding too.
        let binding = small.to_session_binding();
        assert_eq!(binding.context_tokens, Some(8192));
        let restored = ActiveModel::from_session_binding(binding).unwrap();
        assert_eq!(restored.context_tokens, Some(8192));
    }

    #[test]
    fn rendering_defaults_preserve_existing_configurations() {
        let config: XshellConfig = toml::from_str("").unwrap();
        assert_eq!(config.rendering, RenderingConfig::default());
        assert_eq!(config.view, ViewConfig::default());
        assert!(config.remote_bootstrap.is_none());
    }

    #[test]
    fn parses_typed_view_policy() {
        let config: XshellConfig = toml::from_str(
            r#"
            [view]
            pagination = "always"
            pager = ["less", "-R", "-X"]

            [view.classes.text]
            pagination = "auto"

            [view.viewers.markdown]
            pagination = "never"
            "#,
        )
        .unwrap();
        assert_eq!(config.view.pagination, PaginationMode::Always);
        assert_eq!(config.view.pager[0], "less");
        assert_eq!(
            config.view.classes["text"].pagination,
            Some(PaginationMode::Auto)
        );
        assert_eq!(
            config.view.viewers["markdown"].pagination,
            Some(PaginationMode::Never)
        );
        config.view.validate().unwrap();

        let invalid: XshellConfig = toml::from_str("[view]\npager = []").unwrap();
        assert!(invalid.view.validate().is_err());
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

    fn limits_config(source: &str) -> XshellConfig {
        toml::from_str(source).unwrap()
    }

    #[test]
    fn builtin_limit_profiles_need_no_configuration() {
        let config = XshellConfig::default();
        assert_eq!(
            config.limit_profile("default").unwrap(),
            TurnLimits::default()
        );
        assert_eq!(
            config.limit_profile("relaxed").unwrap(),
            TurnLimits::relaxed()
        );
        let error = config.limit_profile("huge").unwrap_err().to_string();
        assert!(
            error.contains("known profiles: default, relaxed"),
            "{error}"
        );
        config.validate_limits().unwrap();
    }

    #[test]
    fn configured_profiles_override_builtins_and_start_new_ones_from_default() {
        let config = limits_config(
            r#"
[agent.limit_profiles.default]
tool_calls = 300

[agent.limit_profiles.relaxed]
wall_time = "12h"

[agent.limit_profiles.render]
shell_timeout = "45m"
"#,
        );
        config.validate_limits().unwrap();
        assert_eq!(config.limit_profile("default").unwrap().tool_calls, 300);
        let relaxed = config.limit_profile("relaxed").unwrap();
        assert_eq!(relaxed.wall_time, std::time::Duration::from_secs(12 * 3600));
        assert_eq!(relaxed.tool_calls, TurnLimits::relaxed().tool_calls);
        // A new profile starts from the configured default.
        let render = config.limit_profile("render").unwrap();
        assert_eq!(render.tool_calls, 300);
        assert_eq!(
            render.shell_timeout,
            std::time::Duration::from_secs(45 * 60)
        );
        assert_eq!(
            config.limit_profile_names(),
            ["default", "relaxed", "render"]
        );
    }

    #[test]
    fn limit_profile_selection_prefers_explicit_then_model_then_agent_default() {
        let config = limits_config(
            r#"
[agent]
limits = "relaxed"

[agent.limit_profiles.render]
shell_timeout = "45m"

[models.cad]
provider = "ollama"
model = "m"
limits = "render"

[models.chat]
provider = "ollama"
model = "m"
"#,
        );
        config.validate_limits().unwrap();
        let cad = config.resolve_profile("cad").unwrap();
        let chat = config.resolve_profile("chat").unwrap();
        assert_eq!(config.limits_name_for(Some("default"), &cad), "default");
        assert_eq!(config.limits_name_for(None, &cad), "render");
        assert_eq!(config.limits_name_for(None, &chat), "relaxed");
        assert_eq!(
            XshellConfig::default().limits_name_for(None, &chat),
            "default"
        );
    }

    #[test]
    fn invalid_limit_configuration_is_rejected_at_load() {
        for source in [
            "[agent]\nlimits = \"missing\"\n",
            "[agent.limit_profiles.default]\ntool_calls = 0\n",
            "[agent.limit_profiles.relaxed]\nwall_time = \"forever\"\n",
            "[models.m]\nprovider = \"ollama\"\nmodel = \"m\"\nlimits = \"missing\"\n",
        ] {
            let rejected = toml::from_str::<XshellConfig>(source)
                .map_err(anyhow::Error::from)
                .and_then(|config| config.validate_limits());
            assert!(rejected.is_err(), "accepted: {source}");
        }
    }

    #[test]
    fn the_example_configuration_loads_with_its_limit_examples_enabled() {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml"),
        )
        .unwrap();
        let shipped: XshellConfig = toml::from_str(&source).unwrap();
        shipped.validate_limits().unwrap();

        // Uncomment the limit examples, as a user following them would.
        let enabled: String = source
            .lines()
            .map(|line| {
                let uncommented = line.strip_prefix("# ").unwrap_or(line);
                let is_limits_example = uncommented.starts_with("[agent")
                    || uncommented.starts_with("[session_fabric.max_limits]")
                    || uncommented.starts_with("limits = ")
                    || uncommented.starts_with("wall_time = ")
                    || uncommented.starts_with("shell_timeout = ")
                    || uncommented.starts_with("tool_calls = ");
                if is_limits_example { uncommented } else { line }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config: XshellConfig = toml::from_str(&enabled).unwrap();
        config.validate_limits().unwrap();
        assert_eq!(config.agent.limits.as_deref(), Some("default"));
        assert_eq!(
            config.limit_profile("cad-render").unwrap().shell_timeout,
            std::time::Duration::from_secs(45 * 60)
        );
        let ceiling = config.session_fabric.limit_ceiling().unwrap();
        assert_eq!(ceiling.wall_time, std::time::Duration::from_secs(4 * 3600));
        assert_eq!(ceiling.tool_calls, 2000);
    }
}
