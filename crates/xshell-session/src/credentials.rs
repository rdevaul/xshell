//! Which provider credentials a daemon may send, and where.
//!
//! A session's [`ModelBinding`] comes from a client. A binding that names a
//! credential variable (`api_key_env`) makes the daemon read that variable
//! from its own environment and send it as a bearer token to the binding's
//! `base_url`, so the daemon accepts that pair only when its own
//! configuration declares it: a `[models.NAME]` profile with the same
//! provider, endpoint, and variable. A binding with no credential may name
//! any endpoint; it sends the conversation there, as the client asked, but
//! no secret of the daemon's.

use crate::ModelBinding;
use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// The endpoint a provider uses when a profile names none. The client
/// resolves profiles the same way, so a profile without `base_url` matches
/// the binding the client derives from it.
pub fn default_model_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "ollama" => Some("http://127.0.0.1:11434"),
        "openai" => Some("https://api.openai.com"),
        _ => None,
    }
}

/// The variable the client uses for `--provider openai` when no profile is
/// selected. It is allowed only with OpenAI's own endpoint.
const BUILTIN_OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";

/// The parts of a `[models.NAME]` profile the daemon reads. The client owns
/// the full profile format and validates it; the daemon ignores the rest.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfiguredModel {
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// The (provider, endpoint, credential variable) combinations a daemon may
/// use, taken from its own configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRoutes {
    routes: BTreeSet<(String, String, String)>,
}

impl Default for CredentialRoutes {
    /// Only the built-in route: `OPENAI_API_KEY` to OpenAI's endpoint.
    fn default() -> Self {
        Self::from_profiles(&BTreeMap::new())
    }
}

impl CredentialRoutes {
    pub fn from_profiles(profiles: &BTreeMap<String, ConfiguredModel>) -> Self {
        let mut routes = BTreeSet::new();
        routes.insert((
            "openai".to_owned(),
            normalize(default_model_base_url("openai").expect("openai has a default endpoint")),
            BUILTIN_OPENAI_KEY_ENV.to_owned(),
        ));
        for profile in profiles.values() {
            let Some(variable) = &profile.api_key_env else {
                continue;
            };
            let Some(base_url) = profile
                .base_url
                .as_deref()
                .or_else(|| default_model_base_url(&profile.provider))
            else {
                continue;
            };
            routes.insert((
                profile.provider.clone(),
                normalize(base_url),
                variable.clone(),
            ));
        }
        Self { routes }
    }

    /// The credential variables the configured profiles name.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.routes.iter().map(|(_, _, variable)| variable.as_str())
    }

    /// Refuse a binding that would send a credential this configuration
    /// does not route to that endpoint. Keyless bindings always pass.
    ///
    /// The error names neither the variable nor its value, so a client
    /// cannot use it to learn which variables the daemon has.
    pub fn check(&self, binding: &ModelBinding) -> Result<()> {
        let Some(variable) = &binding.api_key_env else {
            return Ok(());
        };
        let route = (
            binding.provider.clone(),
            normalize(&binding.base_url),
            variable.clone(),
        );
        if self.routes.contains(&route) {
            return Ok(());
        }
        bail!(
            "this session host has no model profile that sends that credential to {}; add a \
[models.NAME] profile with this provider, base_url, and api_key_env to the session host's \
configuration",
            binding.base_url
        )
    }
}

fn normalize(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(provider: &str, base_url: &str, api_key_env: Option<&str>) -> ModelBinding {
        ModelBinding {
            profile_name: None,
            provider: provider.into(),
            model: "m".into(),
            base_url: base_url.into(),
            api_key_env: api_key_env.map(str::to_owned),
            max_history_bytes: None,
            context_tokens: None,
            reserve_output_tokens: None,
            vision: false,
        }
    }

    fn routes() -> CredentialRoutes {
        let profiles = toml::from_str::<BTreeMap<String, ConfiguredModel>>(
            r#"
            [router]
            provider = "openai"
            model = "some/model"
            base_url = "https://openrouter.ai/api/v1"
            api_key_env = "OPENROUTER_API_KEY"
            vision = true

            [default-endpoint]
            provider = "openai"
            model = "gpt"
            api_key_env = "WORK_OPENAI_KEY"

            [local]
            provider = "ollama"
            model = "qwen3:8b"
            "#,
        )
        .unwrap();
        CredentialRoutes::from_profiles(&profiles)
    }

    #[test]
    fn f23_a_binding_naming_an_arbitrary_variable_is_refused() {
        let routes = routes();
        for refused in [
            // A variable no profile names.
            binding(
                "openai",
                "https://openrouter.ai/api/v1",
                Some("GITHUB_TOKEN"),
            ),
            // A configured variable, but to another endpoint.
            binding(
                "openai",
                "https://attacker.example/v1",
                Some("OPENROUTER_API_KEY"),
            ),
            // The built-in variable away from OpenAI's endpoint.
            binding("openai", "https://attacker.example", Some("OPENAI_API_KEY")),
            // A configured pair under another provider.
            binding(
                "ollama",
                "https://openrouter.ai/api/v1",
                Some("OPENROUTER_API_KEY"),
            ),
        ] {
            let error = routes.check(&refused).unwrap_err().to_string();
            let variable = refused.api_key_env.as_deref().unwrap();
            assert!(!error.contains(variable), "names the variable: {error}");
        }
    }

    #[test]
    fn f23_configured_and_keyless_bindings_are_accepted() {
        let routes = routes();
        for accepted in [
            binding(
                "openai",
                "https://openrouter.ai/api/v1/",
                Some("OPENROUTER_API_KEY"),
            ),
            binding("openai", "https://api.openai.com", Some("WORK_OPENAI_KEY")),
            binding("openai", "https://api.openai.com/", Some("OPENAI_API_KEY")),
            // No credential: any endpoint.
            binding("ollama", "http://gpu-box.lan:11434", None),
            binding("openai", "http://127.0.0.1:8080/v1", None),
        ] {
            routes.check(&accepted).unwrap();
        }
        assert_eq!(
            routes.variables().collect::<BTreeSet<_>>(),
            BTreeSet::from(["OPENAI_API_KEY", "OPENROUTER_API_KEY", "WORK_OPENAI_KEY"])
        );
    }
}
