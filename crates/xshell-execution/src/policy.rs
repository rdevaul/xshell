//! Approval policies: which tool calls run, which need a human decision,
//! and which are refused, combined with the approval mode.
//!
//! A policy has three optional lists. Each entry names a tool (`read_file`),
//! `run_shell` with a pattern matched against the command's parsed argument
//! vector (`run_shell: yapcad export *`), or a read-only tool with a path
//! pattern matched against the canonical path of the object it opens
//! (`read_file: config/.env`). A call is decided by the first rule of this
//! table that matches:
//!
//! | The call | `off` | `ask` | `auto` |
//! |---|---|---|---|
//! | matches `denied` | deny | deny | deny |
//! | matches no list while an `allowed` list exists | deny | deny | deny |
//! | matches `always_allowed` | deny | run | run |
//! | matches `allowed` | deny | prompt | run |
//! | is unlisted, with no `allowed` list | default | default | default |
//!
//! The default is the behaviour without a policy: read-only calls on
//! non-sensitive paths run, and gated calls are denied, prompted, or run by
//! mode. A path in a `prompt` sensitivity class stays gated even when an
//! entry always allows its tool, unless the session host's own policy names
//! the path.
//!
//! The session host's policy is the base. A client may add an overlay; each
//! policy decides the call and the stricter decision wins, so an overlay can
//! only restrict. An overlay is silent about calls it does not list.
//!
//! Allow patterns never match a compound shell command (pipelines, `;`, `&&`,
//! substitutions, redirections); only a bare `run_shell` entry does. Deny
//! patterns are also checked against each part of a compound command, so
//! chaining commands cannot get around a denial.

use crate::{ApprovalPolicy, SensitivePaths};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A policy as written in configuration or sent by a client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub always_allowed: Vec<String>,
    /// When present, calls outside `always_allowed` and this list are
    /// denied, even if the list is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
}

impl PolicyConfig {
    pub fn is_empty(&self) -> bool {
        self.always_allowed.is_empty() && self.allowed.is_none() && self.denied.is_empty()
    }
}

/// What a policy decides for one call. Ordered from strictest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicyDecision {
    Deny,
    Prompt,
    Run,
}

/// The facts about a call that its decision depends on.
#[derive(Debug, Clone, Copy)]
pub struct CallFacts<'a> {
    pub tool: &'a str,
    /// The `run_shell` command, when the call is one.
    pub command: Option<&'a str>,
    /// The canonical path a read-only tool opened, when preparation
    /// succeeded.
    pub path: Option<&'a Path>,
    /// Whether the call needs approval without a policy: a shell command, or
    /// a read of a path in a `prompt` sensitivity class.
    pub gated: bool,
    /// Whether the path is in a `prompt` sensitivity class.
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// Any call to the tool.
    Tool(String),
    /// A simple `run_shell` command whose words match.
    Shell(Vec<String>),
    /// A read-only tool on a path matching the pattern.
    Path { tool: String, pattern: String },
}

/// The tools a path entry may name.
const PATH_TOOLS: &[&str] = &["read_file", "list_directory"];

impl Rule {
    fn parse(entry: &str) -> Result<Self> {
        let entry = entry.trim();
        let (tool, argument) = match entry.split_once(':') {
            Some((tool, argument)) => (tool.trim(), Some(argument.trim())),
            None => (entry, None),
        };
        if tool.is_empty() || !tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("{entry:?} does not start with a tool name");
        }
        match argument {
            None => Ok(Self::Tool(tool.to_owned())),
            Some("") => bail!("{entry:?} has nothing after the colon"),
            Some(pattern) if tool == "run_shell" => {
                let words = shell_words::split(pattern)
                    .map_err(|error| anyhow::anyhow!("{entry:?}: {error}"))?;
                if words.is_empty() {
                    bail!("{entry:?} has no command pattern");
                }
                Ok(Self::Shell(words))
            }
            Some(pattern) if PATH_TOOLS.contains(&tool) => Ok(Self::Path {
                tool: tool.to_owned(),
                pattern: pattern.to_owned(),
            }),
            Some(_) => bail!(
                "{entry:?}: only run_shell takes a command pattern, and only {} take a path",
                PATH_TOOLS.join(" and ")
            ),
        }
    }

    fn tool(&self) -> &str {
        match self {
            Self::Tool(tool) | Self::Path { tool, .. } => tool,
            Self::Shell(_) => "run_shell",
        }
    }

    /// Whether the rule allows (or always allows) this call.
    fn allows(&self, call: &CallFacts) -> bool {
        if self.tool() != call.tool {
            return false;
        }
        match self {
            Self::Tool(_) => true,
            Self::Shell(pattern) => call
                .command
                .and_then(simple_command)
                .is_some_and(|words| words_match(pattern, &words)),
            Self::Path { pattern, .. } => call.path.is_some_and(|path| path_matches(pattern, path)),
        }
    }

    /// Whether the rule denies this call. Shell patterns are also checked
    /// against each part of a compound command.
    fn denies(&self, call: &CallFacts) -> bool {
        if self.tool() != call.tool {
            return false;
        }
        match self {
            Self::Shell(pattern) => call.command.is_some_and(|command| {
                command_parts(command)
                    .iter()
                    .any(|words| words_match(pattern, words))
            }),
            _ => self.allows(call),
        }
    }

    /// Whether a call this rule matches is necessarily also matched by
    /// `other`, for override warnings.
    fn covered_by(&self, other: &Rule) -> bool {
        self == other || matches!(other, Rule::Tool(tool) if tool == self.tool())
    }
}

/// One parsed policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    always_allowed: Vec<Rule>,
    allowed: Option<Vec<Rule>>,
    denied: Vec<Rule>,
}

impl Policy {
    /// Parse a policy, returning warnings for allow entries a denial
    /// overrides.
    pub fn parse(config: &PolicyConfig) -> Result<(Self, Vec<String>)> {
        let parse = |entries: &[String]| -> Result<Vec<Rule>> {
            entries.iter().map(|entry| Rule::parse(entry)).collect()
        };
        let policy = Self {
            always_allowed: parse(&config.always_allowed)?,
            allowed: config.allowed.as_deref().map(parse).transpose()?,
            denied: parse(&config.denied)?,
        };
        let mut warnings = Vec::new();
        let listed = config
            .always_allowed
            .iter()
            .zip(&policy.always_allowed)
            .map(|(entry, rule)| ("always_allowed", entry, rule))
            .chain(
                config
                    .allowed
                    .iter()
                    .flatten()
                    .zip(policy.allowed.iter().flatten())
                    .map(|(entry, rule)| ("allowed", entry, rule)),
            );
        for (list, entry, rule) in listed {
            if let Some((denied, _)) = config
                .denied
                .iter()
                .zip(&policy.denied)
                .find(|(_, denial)| rule.covered_by(denial))
            {
                warnings.push(format!(
                    "{list} entry {entry:?} has no effect: denied entry {denied:?} overrides it"
                ));
            }
        }
        Ok((policy, warnings))
    }

    /// Decide `call` under `mode`. `base` is true for the session host's
    /// policy, which alone may let a named path skip the sensitive-path
    /// prompt, and whose silence means the default behaviour.
    fn decide(&self, call: &CallFacts, mode: ApprovalPolicy, base: bool) -> PolicyDecision {
        if self.denied.iter().any(|rule| rule.denies(call)) {
            return PolicyDecision::Deny;
        }
        let always = self.always_allowed.iter().find(|rule| rule.allows(call));
        let allowed = self
            .allowed
            .as_ref()
            .and_then(|rules| rules.iter().find(|rule| rule.allows(call)));
        if self.allowed.is_some() && always.is_none() && allowed.is_none() {
            return PolicyDecision::Deny;
        }
        if always.is_some() || allowed.is_some() {
            if mode == ApprovalPolicy::Off {
                return PolicyDecision::Deny;
            }
            let names_path = matches!(always, Some(Rule::Path { .. }));
            let needs_prompt = match always {
                // Always allowed: runs, unless the path is sensitive and
                // only the host's policy naming it may skip that prompt.
                Some(_) => call.sensitive && !(base && names_path),
                None => true,
            };
            return if needs_prompt && mode == ApprovalPolicy::Ask {
                PolicyDecision::Prompt
            } else {
                PolicyDecision::Run
            };
        }
        if !base {
            // An overlay is silent about calls it does not list.
            return PolicyDecision::Run;
        }
        default_decision(call, mode)
    }
}

/// The behaviour without a policy.
pub fn default_decision(call: &CallFacts, mode: ApprovalPolicy) -> PolicyDecision {
    if !call.gated {
        return PolicyDecision::Run;
    }
    match mode {
        ApprovalPolicy::Off => PolicyDecision::Deny,
        ApprovalPolicy::Ask => PolicyDecision::Prompt,
        ApprovalPolicy::Auto => PolicyDecision::Run,
    }
}

/// The session host's policy and an optional client overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalRules {
    pub base: Policy,
    pub overlay: Option<Policy>,
}

impl ApprovalRules {
    /// The decision for `call`: the stricter of the base's and the
    /// overlay's.
    pub fn decide(&self, call: &CallFacts, mode: ApprovalPolicy) -> PolicyDecision {
        let base = self.base.decide(call, mode, true);
        match &self.overlay {
            Some(overlay) => base.min(overlay.decide(call, mode, false)),
            None => base,
        }
    }
}

/// Shell metacharacters that make a command compound: more than one command,
/// a redirection, or a substitution. Checked on the raw text, quoted or not,
/// so the test errs towards "compound".
const COMPOUND: &[char] = &['|', ';', '&', '<', '>', '`', '$', '(', ')', '\n', '\r'];

/// The words of `command` if it is one simple command.
fn simple_command(command: &str) -> Option<Vec<String>> {
    if command.contains(COMPOUND) {
        return None;
    }
    shell_words::split(command)
        .ok()
        .filter(|words| !words.is_empty())
}

/// The words of each part of `command`, split at the metacharacters that
/// separate commands. Parts that do not parse are skipped.
fn command_parts(command: &str) -> Vec<Vec<String>> {
    command
        .split(COMPOUND)
        .filter_map(|part| shell_words::split(part).ok())
        .filter(|words| !words.is_empty())
        .collect()
}

/// Whether `words` match `pattern` word for word. A pattern word may contain
/// `*` wildcards; a final pattern word of exactly `*` matches any number of
/// remaining words, including none.
fn words_match(pattern: &[String], words: &[String]) -> bool {
    match pattern.split_last() {
        Some((last, init)) if last == "*" => {
            words.len() >= init.len()
                && init
                    .iter()
                    .zip(words)
                    .all(|(pattern, word)| wildcard(pattern, word))
        }
        _ => {
            pattern.len() == words.len()
                && pattern
                    .iter()
                    .zip(words)
                    .all(|(pattern, word)| wildcard(pattern, word))
        }
    }
}

/// `*` matches any run of characters, including none.
fn wildcard(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, rest) = parts.split_first().expect("split yields at least one part");
    let Some(mut remaining) = text.strip_prefix(first) else {
        return false;
    };
    let (last, middle) = rest.split_last().expect("a `*` yields two parts");
    for part in middle {
        match remaining.find(part) {
            Some(index) => remaining = &remaining[index + part.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
}

/// Path patterns follow sensitive-path matching: a pattern with `/` matches at
/// any depth, a leading `/` anchors it, and a bare name matches a file name.
fn path_matches(pattern: &str, path: &Path) -> bool {
    SensitivePaths::new([pattern.to_owned()]).matches(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ApprovalPolicy::{Ask, Auto, Off};
    use PolicyDecision::{Deny, Prompt, Run};
    use std::path::PathBuf;

    fn policy(always: &[&str], allowed: Option<&[&str]>, denied: &[&str]) -> Policy {
        let config = PolicyConfig {
            always_allowed: always.iter().map(|s| (*s).to_owned()).collect(),
            allowed: allowed.map(|list| list.iter().map(|s| (*s).to_owned()).collect()),
            denied: denied.iter().map(|s| (*s).to_owned()).collect(),
        };
        Policy::parse(&config).unwrap().0
    }

    fn shell(command: &str) -> CallFacts<'_> {
        CallFacts {
            tool: "run_shell",
            command: Some(command),
            path: None,
            gated: true,
            sensitive: false,
        }
    }

    fn read<'a>(path: &'a Path, sensitive: bool) -> CallFacts<'a> {
        CallFacts {
            tool: "read_file",
            command: None,
            path: Some(path),
            gated: sensitive,
            sensitive,
        }
    }

    fn base(policy: &Policy, call: &CallFacts) -> [PolicyDecision; 3] {
        [Off, Ask, Auto].map(|mode| policy.decide(call, mode, true))
    }

    #[test]
    fn the_rule_table_holds_row_by_row() {
        let rules = policy(
            &["run_shell: git status"],
            Some(&["run_shell: yapcad *"]),
            &["run_shell: yapcad clean *"],
        );
        // 1. denied, in every mode, even though `allowed` also matches.
        assert_eq!(base(&rules, &shell("yapcad clean all")), [Deny, Deny, Deny]);
        // 2. an allowed list exists and the call is on no list.
        assert_eq!(base(&rules, &shell("ls")), [Deny, Deny, Deny]);
        // 3. always allowed.
        assert_eq!(base(&rules, &shell("git status")), [Deny, Run, Run]);
        // 4. allowed.
        assert_eq!(
            base(&rules, &shell("yapcad export a.step")),
            [Deny, Prompt, Run]
        );
        // 5. unlisted with no allowed list: the default behaviour.
        let open = policy(&[], None, &[]);
        assert_eq!(base(&open, &shell("ls")), [Deny, Prompt, Run]);
        let file = PathBuf::from("/work/notes.txt");
        assert_eq!(base(&open, &read(&file, false)), [Run, Run, Run]);
    }

    #[test]
    fn listed_read_only_tools_are_denied_in_off_mode() {
        let file = PathBuf::from("/work/notes.txt");
        let rules = policy(&["read_file"], None, &[]);
        assert_eq!(base(&rules, &read(&file, false)), [Deny, Run, Run]);
    }

    #[test]
    fn sensitive_paths_keep_their_prompt_unless_the_host_names_them() {
        let secret = PathBuf::from("/work/config/.env");
        let by_tool = policy(&["read_file"], None, &[]);
        assert_eq!(base(&by_tool, &read(&secret, true)), [Deny, Prompt, Run]);
        let by_path = policy(&["read_file: config/.env"], None, &[]);
        assert_eq!(base(&by_path, &read(&secret, true)), [Deny, Run, Run]);
        // An overlay naming the path cannot skip the prompt.
        let overlay = by_path.decide(&read(&secret, true), Ask, false);
        assert_eq!(overlay, Prompt);
    }

    #[test]
    fn an_overlay_can_only_restrict() {
        let host = policy(&["run_shell: make *"], None, &[]);
        let quiet = ApprovalRules {
            base: host.clone(),
            overlay: Some(policy(&[], None, &[])),
        };
        // A silent overlay leaves the host's decision alone.
        assert_eq!(quiet.decide(&shell("make all"), Ask), Run);
        assert_eq!(quiet.decide(&shell("ls"), Ask), Prompt);
        // An overlay that always allows more cannot loosen the host.
        let looser = ApprovalRules {
            base: host.clone(),
            overlay: Some(policy(&["run_shell"], None, &[])),
        };
        assert_eq!(looser.decide(&shell("rm -rf build"), Ask), Prompt);
        // An overlay can deny, demote to a prompt, or close the list.
        let stricter = ApprovalRules {
            base: host.clone(),
            overlay: Some(policy(
                &[],
                Some(&["run_shell: make test"]),
                &["run_shell: make clean"],
            )),
        };
        assert_eq!(stricter.decide(&shell("make clean"), Auto), Deny);
        assert_eq!(stricter.decide(&shell("make test"), Ask), Prompt);
        assert_eq!(stricter.decide(&shell("make all"), Auto), Deny);
        // With no overlay the host's policy decides alone.
        let alone = ApprovalRules {
            base: host,
            overlay: None,
        };
        assert_eq!(alone.decide(&shell("make all"), Ask), Run);
    }

    #[test]
    fn compound_commands_never_match_allow_patterns_but_do_match_denials() {
        let rules = policy(&["run_shell: ls *"], None, &["run_shell: rm *"]);
        assert_eq!(base(&rules, &shell("ls -la")), [Deny, Run, Run]);
        // Chained onto an allowed command: falls back to the default.
        assert_eq!(base(&rules, &shell("ls; touch x")), [Deny, Prompt, Run]);
        assert_eq!(base(&rules, &shell("ls $(whoami)")), [Deny, Prompt, Run]);
        assert_eq!(base(&rules, &shell("ls > out")), [Deny, Prompt, Run]);
        // A denied command anywhere in the chain is denied.
        assert_eq!(base(&rules, &shell("ls && rm -rf x")), [Deny, Deny, Deny]);
        assert_eq!(
            base(&rules, &shell("echo ok | rm -f y")),
            [Deny, Deny, Deny]
        );
        // A bare run_shell entry matches compound commands too.
        let bare = policy(&["run_shell"], None, &[]);
        assert_eq!(base(&bare, &shell("ls | wc -l")), [Deny, Run, Run]);
    }

    #[test]
    fn quoting_cannot_smuggle_a_second_command_past_a_pattern() {
        let rules = policy(&["run_shell: echo *"], Some(&[]), &[]);
        assert_eq!(base(&rules, &shell("echo hello world")), [Deny, Run, Run]);
        assert_eq!(base(&rules, &shell("echo 'a;b'")), [Deny, Deny, Deny]);
        assert_eq!(base(&rules, &shell("echo \"$(id)\"")), [Deny, Deny, Deny]);
    }

    #[test]
    fn word_patterns_match_exactly_or_with_wildcards() {
        let words = |text: &str| shell_words::split(text).unwrap();
        assert!(words_match(&words("git status"), &words("git status")));
        assert!(!words_match(&words("git status"), &words("git status -s")));
        assert!(words_match(&words("git *"), &words("git")));
        assert!(words_match(&words("git *"), &words("git log --oneline")));
        assert!(words_match(
            &words("yapcad export *.step"),
            &words("yapcad export part.step")
        ));
        assert!(!words_match(
            &words("yapcad export *.step"),
            &words("yapcad export part.stl")
        ));
        assert!(wildcard("a*b*c", "aXXbYYc"));
        assert!(!wildcard("a*b*c", "aXXc"));
        assert!(wildcard("*", ""));
    }

    #[test]
    fn invalid_entries_are_rejected() {
        for entry in [
            "",
            ":x",
            "run_shell:",
            "read_file: ",
            "delegate_task: x",
            "bad name",
        ] {
            let config = PolicyConfig {
                denied: vec![entry.to_owned()],
                ..PolicyConfig::default()
            };
            assert!(Policy::parse(&config).is_err(), "accepted {entry:?}");
        }
    }

    #[test]
    fn overridden_allow_entries_are_reported() {
        let config = PolicyConfig {
            always_allowed: vec!["run_shell: make *".into(), "read_file".into()],
            allowed: Some(vec!["run_shell: git push *".into()]),
            denied: vec!["run_shell: git push *".into(), "read_file".into()],
        };
        let (_, warnings) = Policy::parse(&config).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("\"read_file\""))
        );
        assert!(warnings.iter().any(|warning| warning.contains("git push")));
    }
}
