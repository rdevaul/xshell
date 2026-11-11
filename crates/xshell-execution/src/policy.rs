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
//! substitutions, redirections, brace expansion) or one whose command word is
//! not plain (`FOO=1 make`, `=make`); only a bare `run_shell` entry does.
//! Deny patterns are also checked against each part of a compound command.
//! When a policy has shell deny patterns and a command cannot be split into
//! parts that are read as written (a substitution, a separator inside quotes,
//! an expanding command word), the command is never run unattended: it is
//! prompted in `ask` mode and denied otherwise. Chaining and quoting therefore
//! cannot get around a denial. Patterns still match literal words, so wrappers
//! such as `xargs rm` or `env rm` are not recognised as `rm`.

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

/// Which of the two policies a decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyLayer {
    /// The session host's own policy.
    Base,
    /// The client's overlay, which can only restrict.
    Overlay,
}

impl PolicyLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Overlay => "overlay",
        }
    }
}

/// A policy's three lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyList {
    Denied,
    AlwaysAllowed,
    Allowed,
}

impl PolicyList {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::AlwaysAllowed => "always_allowed",
            Self::Allowed => "allowed",
        }
    }
}

/// What settled a call before any person or program was asked: the part of
/// the approval rules whose decision was the effective one. Recorded with
/// every decision so the audit trail says what decided, not only what was
/// decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionBasis {
    /// An entry in one of a policy's lists matched, as written in the
    /// policy. The approval mode still applies: an allowed call is denied in
    /// `off` mode and prompted in `ask` mode.
    Entry {
        layer: PolicyLayer,
        list: PolicyList,
        entry: String,
    },
    /// The policy has an `allowed` list and the call matched none of its
    /// entries.
    NotListed { layer: PolicyLayer },
    /// The policy denies some shell commands by pattern, and this command
    /// could not be read reliably enough to check it, so it is never run
    /// unattended.
    UncheckableCommand { layer: PolicyLayer },
    /// No policy entry matched: the approval mode's behaviour without a
    /// policy.
    ModeDefault,
    /// The tool is not one this agent may use, or does not exist. Refused
    /// before the policy is consulted.
    NotInToolset,
    /// The turn's limits refused the call before the policy was consulted:
    /// too many calls in one response, or the tool-call budget is spent.
    TurnLimit,
    /// The call would read a path in the session host's `block` class.
    /// Refused before the policy is consulted, in every approval mode.
    BlockedPath,
}

impl DecisionBasis {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Entry { .. } => "entry",
            Self::NotListed { .. } => "not_listed",
            Self::UncheckableCommand { .. } => "uncheckable_command",
            Self::ModeDefault => "mode_default",
            Self::NotInToolset => "not_in_toolset",
            Self::TurnLimit => "turn_limit",
            Self::BlockedPath => "blocked_path",
        }
    }

    pub fn layer(&self) -> Option<PolicyLayer> {
        match self {
            Self::Entry { layer, .. }
            | Self::NotListed { layer }
            | Self::UncheckableCommand { layer } => Some(*layer),
            Self::ModeDefault | Self::NotInToolset | Self::TurnLimit | Self::BlockedPath => None,
        }
    }
}

/// A decision and what made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuledDecision {
    pub decision: PolicyDecision,
    pub basis: DecisionBasis,
}

/// One parsed policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    always_allowed: Vec<Rule>,
    allowed: Option<Vec<Rule>>,
    denied: Vec<Rule>,
    /// The configuration it was parsed from, so decisions can name entries
    /// as written and the audit trail can record the policy itself.
    config: PolicyConfig,
    /// SHA-256 of the canonical encoding of `config`; `None` for an empty
    /// policy, which decides nothing.
    identity: Option<String>,
}

/// The identity of a policy configuration: SHA-256 over its JSON encoding.
/// The encoding is canonical because `PolicyConfig` serializes its fields in
/// declaration order and omits empty lists; entry order is part of the
/// identity, as it is part of what was written.
pub fn policy_identity(config: &PolicyConfig) -> Option<String> {
    use sha2::{Digest, Sha256};
    if config.is_empty() {
        return None;
    }
    let encoded = serde_json::to_vec(config).expect("a policy configuration always serializes");
    Some(hex::encode(Sha256::digest(encoded)))
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
            config: config.clone(),
            identity: policy_identity(config),
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

    /// SHA-256 of the policy's configuration, or `None` for an empty
    /// policy.
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// The configuration the policy was parsed from.
    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    /// Decide `call` under `mode`. `base` is true for the session host's
    /// policy, which alone may let a named path skip the sensitive-path
    /// prompt, and whose silence means the default behaviour.
    fn decide(&self, call: &CallFacts, mode: ApprovalPolicy, base: bool) -> RuledDecision {
        let layer = if base {
            PolicyLayer::Base
        } else {
            PolicyLayer::Overlay
        };
        let ruled = self.decide_by_rules(call, mode, layer);
        if self.denial_is_uncheckable(call) {
            // A shell denial exists but this command cannot be read reliably
            // enough to know it does not match. Fail closed: a person may
            // still approve it, but it never runs unattended.
            let ceiling = if mode == ApprovalPolicy::Ask {
                PolicyDecision::Prompt
            } else {
                PolicyDecision::Deny
            };
            if ceiling < ruled.decision {
                return RuledDecision {
                    decision: ceiling,
                    basis: DecisionBasis::UncheckableCommand { layer },
                };
            }
        }
        ruled
    }

    /// The `index`th entry of `list`, as written.
    fn entry(&self, layer: PolicyLayer, list: PolicyList, index: usize) -> DecisionBasis {
        let written = match list {
            PolicyList::Denied => &self.config.denied,
            PolicyList::AlwaysAllowed => &self.config.always_allowed,
            PolicyList::Allowed => self
                .config
                .allowed
                .as_ref()
                .expect("an allowed rule exists"),
        };
        DecisionBasis::Entry {
            layer,
            list,
            entry: written[index].trim().to_owned(),
        }
    }

    /// Whether this policy denies some shell commands by pattern and `call`
    /// is a shell command those patterns cannot be checked against reliably.
    fn denial_is_uncheckable(&self, call: &CallFacts) -> bool {
        call.tool == "run_shell"
            && self
                .denied
                .iter()
                .any(|rule| matches!(rule, Rule::Shell(_)))
            && call
                .command
                .is_none_or(|command| checked_parts(command).is_none())
    }

    fn decide_by_rules(
        &self,
        call: &CallFacts,
        mode: ApprovalPolicy,
        layer: PolicyLayer,
    ) -> RuledDecision {
        let base = layer == PolicyLayer::Base;
        let ruled = |decision, basis| RuledDecision { decision, basis };
        if let Some(index) = self.denied.iter().position(|rule| rule.denies(call)) {
            return ruled(
                PolicyDecision::Deny,
                self.entry(layer, PolicyList::Denied, index),
            );
        }
        let always = self
            .always_allowed
            .iter()
            .position(|rule| rule.allows(call));
        let allowed = self
            .allowed
            .as_ref()
            .and_then(|rules| rules.iter().position(|rule| rule.allows(call)));
        if self.allowed.is_some() && always.is_none() && allowed.is_none() {
            return ruled(PolicyDecision::Deny, DecisionBasis::NotListed { layer });
        }
        let matched = always
            .map(|index| (PolicyList::AlwaysAllowed, index))
            .or(allowed.map(|index| (PolicyList::Allowed, index)));
        if let Some((list, index)) = matched {
            let basis = self.entry(layer, list, index);
            if mode == ApprovalPolicy::Off {
                return ruled(PolicyDecision::Deny, basis);
            }
            let names_path =
                always.is_some_and(|index| matches!(self.always_allowed[index], Rule::Path { .. }));
            let needs_prompt = match always {
                // Always allowed: runs, unless the path is sensitive and
                // only the host's policy naming it may skip that prompt.
                Some(_) => call.sensitive && !(base && names_path),
                None => true,
            };
            let decision = if needs_prompt && mode == ApprovalPolicy::Ask {
                PolicyDecision::Prompt
            } else {
                PolicyDecision::Run
            };
            return ruled(decision, basis);
        }
        if !base {
            // An overlay is silent about calls it does not list.
            return ruled(PolicyDecision::Run, DecisionBasis::ModeDefault);
        }
        ruled(default_decision(call, mode), DecisionBasis::ModeDefault)
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
        self.decide_with_basis(call, mode).decision
    }

    /// The decision for `call` and what made it. When the overlay is
    /// stricter, its basis is reported; otherwise the base's, which also
    /// covers an overlay that says nothing about the call.
    pub fn decide_with_basis(&self, call: &CallFacts, mode: ApprovalPolicy) -> RuledDecision {
        let base = self.base.decide(call, mode, true);
        match &self.overlay {
            Some(overlay) => {
                let overlay = overlay.decide(call, mode, false);
                if overlay.decision < base.decision {
                    overlay
                } else {
                    base
                }
            }
            None => base,
        }
    }
}

/// Shell metacharacters that separate commands or redirect them. Checked on
/// the raw text, quoted or not, so the test errs towards "compound".
const SEPARATORS: &[char] = &['|', ';', '&', '<', '>', '\n', '\r'];

/// Shell metacharacters that substitute or expand text, so the words the
/// shell runs cannot be read from the command as written: command and
/// parameter substitution, subshells, and brace expansion.
const EXPANSIONS: &[char] = &['`', '$', '(', ')', '{', '}'];

/// Whether `word` is a plain command name or path, which the shell runs as
/// written. Rejects variable assignments (`FOO=1 rm`), globs (`r?`), tilde
/// and zsh `=` expansion (`=rm`), and anything else that could make the
/// shell run a command other than the one the word spells.
fn plain_command_word(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | ':' | '@' | ',')
        })
}

/// The words of `command` if it is one simple command whose command word is
/// plain.
fn simple_command(command: &str) -> Option<Vec<String>> {
    if command.contains(SEPARATORS) || command.contains(EXPANSIONS) {
        return None;
    }
    shell_words::split(command)
        .ok()
        .filter(|words| words.first().is_some_and(|word| plain_command_word(word)))
}

/// The words of each part of `command`, split at the metacharacters that
/// separate commands. Parts that do not parse are skipped, so this is a
/// best-effort reading: [`checked_parts`] says whether it can be trusted.
fn command_parts(command: &str) -> Vec<Vec<String>> {
    command
        .split(|c| SEPARATORS.contains(&c) || EXPANSIONS.contains(&c))
        .filter_map(|part| shell_words::split(part).ok())
        .filter(|words| !words.is_empty())
        .collect()
}

/// The words of each part of `command` if every part is a simple command
/// that can be read as written: no substitution or expansion anywhere, every
/// part parses (a separator inside quotes leaves an unbalanced part), and
/// every command word is plain. `None` means a denial cannot be checked
/// reliably against this command.
fn checked_parts(command: &str) -> Option<Vec<Vec<String>>> {
    if command.contains(EXPANSIONS) {
        return None;
    }
    let mut parts = Vec::new();
    for part in command.split(SEPARATORS) {
        let words = shell_words::split(part).ok()?;
        match words.first() {
            None => {}
            Some(word) if plain_command_word(word) => parts.push(words),
            Some(_) => return None,
        }
    }
    Some(parts)
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
        [Off, Ask, Auto].map(|mode| policy.decide(call, mode, true).decision)
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
        assert_eq!(overlay.decision, Prompt);
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
        // A substitution cannot be checked against the denial: never unattended.
        assert_eq!(base(&rules, &shell("ls $(whoami)")), [Deny, Prompt, Deny]);
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
    fn commands_a_denial_cannot_be_checked_against_never_run_unattended() {
        // Everything but `rm` runs: the configuration a denial must protect.
        let rules = policy(&["run_shell"], None, &["run_shell: rm *"]);
        assert_eq!(base(&rules, &shell("make all")), [Deny, Run, Run]);
        assert_eq!(base(&rules, &shell("ls | wc -l")), [Deny, Run, Run]);
        assert_eq!(base(&rules, &shell("rm -rf build")), [Deny, Deny, Deny]);
        for command in [
            // A separator inside quotes leaves parts that do not parse.
            "rm -rf \"build;\"",
            "rm -rf 'a|b' build",
            // Substitutions hide the command word.
            "$(echo rm) -rf build",
            "`echo rm` -rf build",
            "${RM:-rm} -rf build",
            // Expansions and assignments change what the shell runs.
            "{rm,-rf,build}",
            "r{m,} -rf build",
            "FOO=1 rm -rf build",
            "=rm -rf build",
            "r? -rf build",
            "~/bin/rm -rf build",
        ] {
            assert_eq!(
                base(&rules, &shell(command)),
                [Deny, Prompt, Deny],
                "{command}"
            );
        }
        // The same commands without a shell denial follow the other rules.
        let open = policy(&["run_shell"], None, &[]);
        assert_eq!(
            base(&open, &shell("$(echo rm) -rf build")),
            [Deny, Run, Run]
        );
        // An overlay's denial fails closed in the same way.
        let rules = ApprovalRules {
            base: policy(&["run_shell"], None, &[]),
            overlay: Some(policy(&[], None, &["run_shell: rm *"])),
        };
        assert_eq!(rules.decide(&shell("`echo rm` -rf build"), Auto), Deny);
        assert_eq!(rules.decide(&shell("`echo rm` -rf build"), Ask), Prompt);
    }

    #[test]
    fn allow_patterns_need_a_plain_command_word() {
        let rules = policy(&["run_shell: make *"], None, &[]);
        assert_eq!(base(&rules, &shell("make all")), [Deny, Run, Run]);
        assert_eq!(
            base(&rules, &shell("CC=evil make all")),
            [Deny, Prompt, Run]
        );
        assert_eq!(base(&rules, &shell("make{,} all")), [Deny, Prompt, Run]);
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

    fn rules(base: Policy, overlay: Option<Policy>) -> ApprovalRules {
        ApprovalRules { base, overlay }
    }

    fn entry(layer: PolicyLayer, list: PolicyList, entry: &str) -> DecisionBasis {
        DecisionBasis::Entry {
            layer,
            list,
            entry: entry.into(),
        }
    }

    #[test]
    fn decisions_name_the_entry_that_made_them() {
        use PolicyLayer::Base;
        let policy = policy(
            &["run_shell: git status"],
            Some(&["run_shell: cargo *"]),
            &["  run_shell: rm *  "],
        );
        let rules = rules(policy, None);
        let decide = |command, mode| rules.decide_with_basis(&shell(command), mode);

        let denied = decide("rm -rf target", Auto);
        assert_eq!(denied.decision, Deny);
        // As written, without the surrounding whitespace.
        assert_eq!(
            denied.basis,
            entry(Base, PolicyList::Denied, "run_shell: rm *")
        );
        // A part of a compound command is named the same way.
        assert_eq!(
            decide("cargo build && rm x", Auto).basis,
            entry(Base, PolicyList::Denied, "run_shell: rm *")
        );

        let always = decide("git status", Ask);
        assert_eq!(always.decision, Run);
        assert_eq!(
            always.basis,
            entry(Base, PolicyList::AlwaysAllowed, "run_shell: git status")
        );

        let allowed = decide("cargo test", Ask);
        assert_eq!(allowed.decision, Prompt);
        assert_eq!(
            allowed.basis,
            entry(Base, PolicyList::Allowed, "run_shell: cargo *")
        );
        // In `off` mode the entry still matched; the mode denied it.
        let off = decide("cargo test", Off);
        assert_eq!(off.decision, Deny);
        assert_eq!(
            off.basis,
            entry(Base, PolicyList::Allowed, "run_shell: cargo *")
        );

        let unlisted = decide("make", Auto);
        assert_eq!(unlisted.decision, Deny);
        assert_eq!(unlisted.basis, DecisionBasis::NotListed { layer: Base });
    }

    #[test]
    fn unlisted_calls_and_uncheckable_commands_say_so() {
        let rules = rules(policy(&[], None, &["run_shell: rm *"]), None);
        let plain = rules.decide_with_basis(&shell("ls"), Ask);
        assert_eq!(plain.decision, Prompt);
        assert_eq!(plain.basis, DecisionBasis::ModeDefault);

        // Allowed by the mode, but a denial cannot be checked against it.
        let uncheckable = rules.decide_with_basis(&shell("echo $(whoami)"), Auto);
        assert_eq!(uncheckable.decision, Deny);
        assert_eq!(
            uncheckable.basis,
            DecisionBasis::UncheckableCommand {
                layer: PolicyLayer::Base
            }
        );
        // When the rules already decide as strictly, they are the basis.
        let no_policy = ApprovalRules::default();
        assert_eq!(
            no_policy
                .decide_with_basis(&shell("echo $(whoami)"), Auto)
                .basis,
            DecisionBasis::ModeDefault
        );
    }

    #[test]
    fn the_stricter_layer_is_the_basis() {
        use PolicyLayer::{Base, Overlay};
        let base = policy(&["run_shell"], None, &[]);
        let overlay = policy(&[], None, &["run_shell: git push *"]);
        let rules = rules(base, Some(overlay));

        let push = rules.decide_with_basis(&shell("git push origin main"), Ask);
        assert_eq!(push.decision, Deny);
        assert_eq!(
            push.basis,
            entry(Overlay, PolicyList::Denied, "run_shell: git push *")
        );

        // The overlay is silent and so no stricter: the base decided.
        let status = rules.decide_with_basis(&shell("git status"), Ask);
        assert_eq!(status.decision, Run);
        assert_eq!(
            status.basis,
            entry(Base, PolicyList::AlwaysAllowed, "run_shell")
        );

        // On a tie the base is reported.
        let strict_base = policy(&[], None, &["run_shell: git push *"]);
        let rules = super::ApprovalRules {
            base: strict_base,
            overlay: Some(policy(&[], None, &["run_shell: git *"])),
        };
        assert_eq!(
            rules
                .decide_with_basis(&shell("git push x"), Ask)
                .basis
                .layer(),
            Some(Base)
        );
    }

    #[test]
    fn policy_identity_is_the_hash_of_what_was_written() {
        let config = |denied: &[&str]| PolicyConfig {
            denied: denied.iter().map(|s| (*s).to_owned()).collect(),
            ..PolicyConfig::default()
        };
        assert_eq!(
            Policy::parse(&PolicyConfig::default())
                .unwrap()
                .0
                .identity(),
            None
        );
        let (policy, _) = Policy::parse(&config(&["run_shell: rm *"])).unwrap();
        let identity = policy.identity().unwrap();
        assert_eq!(identity.len(), 64);
        assert_eq!(Some(identity.to_owned()), policy_identity(policy.config()));
        // Stable for the same text, different for any other.
        assert_eq!(
            Policy::parse(&config(&["run_shell: rm *"]))
                .unwrap()
                .0
                .identity(),
            Some(identity)
        );
        assert_ne!(
            Policy::parse(&config(&["run_shell: rm -r *"]))
                .unwrap()
                .0
                .identity(),
            Some(identity)
        );
        // An empty `allowed` list denies everything, so it is a policy.
        let deny_all = PolicyConfig {
            allowed: Some(Vec::new()),
            ..PolicyConfig::default()
        };
        assert!(Policy::parse(&deny_all).unwrap().0.identity().is_some());
    }
}
