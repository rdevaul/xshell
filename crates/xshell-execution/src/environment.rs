//! The environment given to shells that xshell starts.
//!
//! Model-authored `run_shell` commands do not inherit the daemon's
//! environment. They receive only the variables named in
//! [`AGENT_ENVIRONMENT_ALLOWLIST`] (plus any `LC_*` locale variable), and
//! never a variable registered as a credential with
//! [`protect_credential_variable`], even if its name is on the allowlist.
//! Provider API keys, cloud and GitHub tokens, and anything else the daemon
//! happens to have are therefore absent unless a shell startup file the
//! command's shell reads (for zsh, `~/.zshenv`) sets them again.
//!
//! Direct `$` commands, which the user types, keep the daemon's environment
//! except for the registered credential variables: the provider key is
//! xshell's, not the command's, and is removed from both.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, PoisonError};
use tokio::process::Command;

/// Variables passed through to model-authored shell commands. They locate
/// tools and the user's files, and set the locale, terminal type, temporary
/// directory, and time zone. None carries a credential.
///
/// This list is a constant; making it configurable in daemon configuration is
/// a follow-up.
pub const AGENT_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TERM",
    "COLORTERM",
    "TMPDIR",
    "TZ",
];

/// Variable-name prefixes passed through as well (locale categories).
const AGENT_ENVIRONMENT_PREFIXES: &[&str] = &["LC_"];

static CREDENTIAL_VARIABLES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Record `name` as holding a credential, such as a configured provider's
/// `api_key_env`. Registered variables are removed from every shell xshell
/// starts, agent or direct. Registration lasts for the life of the process.
pub fn protect_credential_variable(name: &str) {
    if name.is_empty() {
        return;
    }
    CREDENTIAL_VARIABLES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name.to_owned());
}

fn credential_variables() -> BTreeSet<String> {
    CREDENTIAL_VARIABLES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn is_allowlisted(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    AGENT_ENVIRONMENT_ALLOWLIST.contains(&name)
        || AGENT_ENVIRONMENT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// The environment a model-authored command receives, drawn from `source`.
fn agent_environment(
    source: impl IntoIterator<Item = (OsString, OsString)>,
    credentials: &BTreeSet<String>,
) -> Vec<(OsString, OsString)> {
    source
        .into_iter()
        .filter(|(name, _)| is_allowlisted(name))
        .filter(|(name, _)| name.to_str().is_none_or(|name| !credentials.contains(name)))
        .collect()
}

/// Replace `command`'s environment with the agent allowlist.
pub(crate) fn apply_agent_environment(command: &mut Command) {
    command.env_clear().envs(agent_environment(
        std::env::vars_os(),
        &credential_variables(),
    ));
}

/// Remove the registered credential variables from `command`'s otherwise
/// inherited environment.
pub(crate) fn remove_credentials(command: &mut Command) {
    for name in credential_variables() {
        command.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        items
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn f18_agent_environment_keeps_only_allowlisted_variables() {
        let credentials = BTreeSet::from(["TERM".to_owned()]);
        let kept = agent_environment(
            pairs(&[
                ("PATH", "/bin"),
                ("HOME", "/home/u"),
                ("LC_ALL", "C"),
                ("OPENAI_API_KEY", "secret"),
                ("GITHUB_TOKEN", "secret"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
                // Allowlisted by name but registered as a credential.
                ("TERM", "secret"),
            ]),
            &credentials,
        );
        assert_eq!(
            kept,
            pairs(&[("PATH", "/bin"), ("HOME", "/home/u"), ("LC_ALL", "C")])
        );
    }
}
