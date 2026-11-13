use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy, ExecutionEvent};

// Version 12 adds an explicit stopping phase so controllers do not report a
// cancellation request as complete while owned process cleanup is still in
// progress. Version 13 adds `ExecutionEvent::AutoContinued` and the
// `reasoning` and `finish_reason` fields on `ExecutionEvent::AgentResponse`.
// Version 14 adds the `delegate_task` subagent events (`SubagentStarted`,
// `Subagent`, `SubagentFinished`). Version 17 adds `source` on
// `ApprovalReply` and `basis` and `approval` on `ExecutionEvent::ToolDecision`.
// Version 18 adds the `blocked_path` decision basis. Version 19 adds
// `tool_output_bytes` to the turn limits. Version 20 adds the
// `tool_declared` gate reason on `ExecutionEvent::ApprovalRequested`, sent for
// provided tools that require approval. Version 21 adds
// `SessionEventKind::TurnSummary`. Version 22 adds `tools` on
// `SessionCreation` and `SessionDescriptor`. Version 23 adds `vision` to
// `ModelBinding`. Version 24 adds `ClientRequest::LocateTurn` and durable
// turn records. Version 25 adds `UploadImage`, `images` on
// `TurnInput::Agent`, the `view_image` tool and `images` on
// `ExecutionEvent::ToolResult`. Version 26 adds the control `token` on
// `ClientRequest::Open` and refuses unauthenticated requests (F27).
pub const SESSION_PROTOCOL_VERSION: u32 = 26;
pub const DAEMON_PROBE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonProbeReport {
    pub schema_version: u32,
    pub binary_version: String,
    pub supported_protocol_version: u32,
    #[serde(flatten)]
    pub daemon: DaemonProbeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "daemon_status", rename_all = "snake_case")]
pub enum DaemonProbeStatus {
    Ready {
        protocol_version: u32,
        host_id: String,
        host_alias: String,
        user: String,
    },
    Incompatible {
        code: String,
        message: String,
    },
    Rejected {
        code: String,
        message: String,
    },
    Unavailable {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub enabled: bool,
    pub required: bool,
    /// Human-facing name advertised by this daemon. When omitted, xshelld
    /// uses the operating-system hostname.
    pub host_alias: Option<String>,
    pub socket: Option<PathBuf>,
    pub state_directory: Option<PathBuf>,
    pub default_session: String,
    pub pty_escape: String,
    /// Most permissive approval policy this daemon will apply to any turn,
    /// regardless of what a client requests. Defaults to `ask`, so a client
    /// on another host cannot turn on unattended shell execution here without
    /// the daemon's operator opting in.
    pub max_approval: ApprovalPolicy,
    /// Glob patterns (relative to the session cwd, or bare file names) for
    /// paths whose reads and listings need approval even though the tool is
    /// otherwise read-only. `None` uses the built-in defaults; an empty list
    /// disables the check.
    pub sensitive_paths: Option<Vec<String>>,
    /// Session-wide default for conversation-history compaction, applied
    /// before provider requests and after completed turns. A model profile's
    /// own `max_history_bytes` takes precedence. Omit to keep full history.
    pub compaction: xshell_execution::CompactionConfig,
    /// The most generous turn limits this daemon applies, whatever a client
    /// requests. Unset fields take the built-in `relaxed` profile's values,
    /// so clients can choose `relaxed` unless the operator lowers this.
    pub max_limits: xshell_execution::LimitSettings,
    /// The approval policy every turn on this daemon is decided by. A
    /// client's policy can only restrict it.
    pub approval_policy: xshell_execution::PolicyConfig,
    /// Sensitivity classes: paths agent read tools need approval for
    /// (`prompt`, added to `sensitive_paths`) or may never read (`block`).
    pub sensitivity: Vec<SensitivityClass>,
    /// Most sessions this daemon keeps at once, attached or not. Creating
    /// another is refused until one is closed, so detached runs cannot
    /// accumulate without bound.
    pub max_sessions: usize,
    /// Most turns (agent or `$`) running at once across all sessions. A
    /// submission beyond it is refused, so one long job cannot be joined by
    /// an unbounded number of others.
    pub max_concurrent_turns: usize,
}

/// Built-in `max_sessions`.
pub const DEFAULT_MAX_SESSIONS: usize = 256;
/// Built-in `max_concurrent_turns`.
pub const DEFAULT_MAX_CONCURRENT_TURNS: usize = 8;

/// A request refused because a daemon-wide budget is spent: too many
/// sessions or too many running turns. Reported to clients with the error
/// code `capacity`, so a supervisor can retry later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityExhausted {
    pub message: String,
}

impl std::fmt::Display for CapacityExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CapacityExhausted {}

/// A set of path patterns and what agent read tools must do with them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SensitivityClass {
    pub action: SensitivityAction,
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityAction {
    /// Reads need approval, as for `sensitive_paths`.
    Prompt,
    /// Reads are refused in every mode, whatever the policy says.
    Block,
}

impl SessionConfig {
    pub fn sensitive_paths(&self) -> xshell_execution::SensitivePaths {
        let base: Vec<String> = match &self.sensitive_paths {
            None => xshell_execution::DEFAULT_SENSITIVE_PATTERNS
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            Some(patterns) => patterns.clone(),
        };
        xshell_execution::SensitivePaths::new(
            base.into_iter()
                .chain(self.class_patterns(SensitivityAction::Prompt)),
        )
    }

    /// Paths agent read tools may never read: the configured `block`
    /// classes. Daemons and clients use [`Self::protected_paths`], which
    /// adds xshell's own files.
    pub fn blocked_paths(&self) -> xshell_execution::SensitivePaths {
        xshell_execution::SensitivePaths::new(self.class_patterns(SensitivityAction::Block))
    }

    /// The `block` classes plus xshell's own state, which agent read tools
    /// may never read whatever the configuration says (finding F24): the
    /// session state directory (every durable session's history, the image
    /// store, turn records), the audit log directory, and the configuration
    /// file. Without this, a session whose working directory is the home
    /// directory could read back, without approval, a secret it was once
    /// approved to see, or another session's conversation.
    ///
    /// Each is anchored to its resolved absolute path. The configuration
    /// file's whole directory is blocked only when it is xshell's own
    /// (named `xshell`), so `--config ./xshell.toml` in a project does not
    /// block the project.
    /// `state_directory` is the one in use, when a command line chose it;
    /// otherwise the configured or default one.
    pub fn protected_paths(
        &self,
        state_directory: Option<&std::path::Path>,
        config_file: Option<&std::path::Path>,
        audit_directory: Option<&std::path::Path>,
    ) -> xshell_execution::SensitivePaths {
        let mut patterns: Vec<String> = self.class_patterns(SensitivityAction::Block).collect();
        let mut directories: Vec<PathBuf> = Vec::new();
        directories.extend(
            state_directory
                .map(std::path::Path::to_path_buf)
                .or_else(|| self.resolved_state_directory()),
        );
        directories.extend(audit_directory.map(std::path::Path::to_path_buf));
        if let Some(parent) = config_file
            .and_then(std::path::Path::parent)
            .filter(|parent| parent.file_name().is_some_and(|name| name == "xshell"))
        {
            directories.push(parent.to_path_buf());
        }
        for directory in directories {
            patterns.push(format!("{}/**", anchored(&directory)));
        }
        if let Some(file) = config_file {
            patterns.push(anchored(file));
        }
        xshell_execution::SensitivePaths::new(patterns)
    }

    fn class_patterns(&self, action: SensitivityAction) -> impl Iterator<Item = String> + '_ {
        self.sensitivity
            .iter()
            .filter(move |class| class.action == action)
            .flat_map(|class| class.patterns.iter().cloned())
    }

    /// The parsed approval policy and warnings about entries it overrides.
    pub fn approval_policy(&self) -> anyhow::Result<(xshell_execution::Policy, Vec<String>)> {
        xshell_execution::Policy::parse(&self.approval_policy)
            .map_err(|error| error.context("in session_fabric.approval_policy"))
    }

    /// The ceiling `max_limits` describes: the built-in `relaxed` profile
    /// with the configured fields replaced.
    pub fn limit_ceiling(&self) -> anyhow::Result<xshell_execution::TurnLimits> {
        self.max_limits
            .apply_to(xshell_execution::TurnLimits::relaxed())
            .map_err(|error| error.context("in session_fabric.max_limits"))
    }

    /// Check the daemon-wide budgets: each must allow at least one.
    pub fn validate_budgets(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("max_sessions", self.max_sessions),
            ("max_concurrent_turns", self.max_concurrent_turns),
        ] {
            if value == 0 {
                anyhow::bail!("session_fabric.{name} must be at least 1");
            }
        }
        Ok(())
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            required: true,
            host_alias: None,
            socket: None,
            state_directory: None,
            default_session: "default".into(),
            pty_escape: "ctrl-]".into(),
            max_approval: ApprovalPolicy::Ask,
            sensitive_paths: None,
            compaction: xshell_execution::CompactionConfig::default(),
            max_limits: xshell_execution::LimitSettings::default(),
            approval_policy: xshell_execution::PolicyConfig::default(),
            sensitivity: Vec::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_concurrent_turns: DEFAULT_MAX_CONCURRENT_TURNS,
        }
    }
}

impl SessionConfig {
    pub fn resolved_state_directory(&self) -> Option<PathBuf> {
        self.state_directory
            .clone()
            .or_else(default_state_directory)
    }

    pub fn resolved_socket(&self) -> Option<PathBuf> {
        self.socket.clone().or_else(|| {
            self.resolved_state_directory()
                .map(|directory| directory.join("xshelld.sock"))
        })
    }
}

/// `path` as an anchored pattern: canonical where it exists (matching is on
/// canonical paths), absolute otherwise, without a trailing `/`.
fn anchored(path: &std::path::Path) -> String {
    let resolved = path
        .canonicalize()
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().replace('\\', "/");
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn default_state_directory() -> Option<PathBuf> {
    if let Some(directory) = env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(directory).join("xshell/sessions"));
    }
    let home = PathBuf::from(env::var_os("HOME")?);
    #[cfg(target_os = "macos")]
    return Some(home.join("Library/Application Support/xshell/sessions"));
    #[cfg(not(target_os = "macos"))]
    Some(home.join(".local/state/xshell/sessions"))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceMode {
    Ephemeral,
    Daemon,
    Durable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    HostOnly,
    Fabric,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    SingleUser,
    MultiUserReserved,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentRole {
    Owner,
    Operator,
    Viewer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Attached,
    Detached,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnPhase {
    Running,
    WaitingApproval,
    Stopping,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionActivity {
    #[default]
    Idle,
    AgentTurn {
        turn_id: String,
        phase: AgentTurnPhase,
    },
    InteractiveProcess {
        command: String,
        attached: bool,
    },
}

impl SessionActivity {
    pub fn is_interactive_process(&self) -> bool {
        matches!(self, Self::InteractiveProcess { .. })
    }
}

impl<'de> Deserialize<'de> for SessionActivity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyActivity {
            Idle,
            Running,
            WaitingApproval,
        }

        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum CurrentActivity {
            Idle,
            AgentTurn {
                turn_id: String,
                phase: AgentTurnPhase,
            },
            InteractiveProcess {
                command: String,
                attached: bool,
            },
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ActivityRepresentation {
            Legacy(LegacyActivity),
            Current(CurrentActivity),
        }

        Ok(match ActivityRepresentation::deserialize(deserializer)? {
            ActivityRepresentation::Legacy(LegacyActivity::Idle)
            | ActivityRepresentation::Current(CurrentActivity::Idle) => Self::Idle,
            ActivityRepresentation::Legacy(LegacyActivity::Running) => Self::AgentTurn {
                turn_id: String::new(),
                phase: AgentTurnPhase::Running,
            },
            ActivityRepresentation::Legacy(LegacyActivity::WaitingApproval) => Self::AgentTurn {
                turn_id: String::new(),
                phase: AgentTurnPhase::WaitingApproval,
            },
            ActivityRepresentation::Current(CurrentActivity::AgentTurn { turn_id, phase }) => {
                Self::AgentTurn { turn_id, phase }
            }
            ActivityRepresentation::Current(CurrentActivity::InteractiveProcess {
                command,
                attached,
            }) => Self::InteractiveProcess { command, attached },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelBinding {
    pub profile_name: Option<String>,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    /// Per-model history budget, overriding `session_fabric.compaction`.
    /// Travels with the binding so a session switched to a small-context
    /// model is compacted for that model, wherever the daemon runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_history_bytes: Option<usize>,
    /// The model's context window in tokens. Travels with the binding for the
    /// same reason `max_history_bytes` does: the enforced budget must follow
    /// the model, not the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<usize>,
    /// Tokens held back for the model's reply when `context_tokens` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_output_tokens: Option<usize>,
    /// Whether the model takes images; without it they are sent as text
    /// placeholders. Absent in sessions saved before images.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vision: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDescriptor {
    pub id: String,
    pub name: String,
    pub host_id: String,
    pub host_alias: String,
    pub user: String,
    pub model: ModelBinding,
    pub cwd: PathBuf,
    pub persistence: PersistenceMode,
    pub visibility: Visibility,
    pub access_mode: AccessMode,
    pub status: SessionStatus,
    #[serde(default)]
    pub activity: SessionActivity,
    pub attached_clients: u32,
    pub created_at_unix_ms: u64,
    pub last_active_at_unix_ms: u64,
    /// The tools this session's agent may use. Fixed when the session is
    /// created; sessions saved before it existed offer every tool.
    #[serde(default, skip_serializing_if = "SessionTools::is_all")]
    pub tools: SessionTools,
}

/// Which tools a session's agent may use, out of those its daemon offers:
/// the built-in tools and any tools compiled-in providers add.
///
/// A selection only narrows. It is fixed when the session is created, so a
/// session's authority never grows after the fact, and it bounds the
/// session's subagents too. A name the daemon does not offer is refused when
/// the session is created; if a later daemon build drops a tool, a restored
/// session simply no longer gets it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionTools {
    /// Every tool the daemon offers.
    #[default]
    All,
    /// Only these tools, by name. An empty list leaves the agent no tools.
    Only(Vec<String>),
}

impl SessionTools {
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub descriptor: SessionDescriptor,
    pub history: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCreation {
    pub name: String,
    pub model: ModelBinding,
    pub cwd: PathBuf,
    pub persistence: PersistenceMode,
    pub visibility: Visibility,
    pub history: Vec<ChatMessage>,
    /// The tools the session's agent may use; every tool when omitted.
    #[serde(default, skip_serializing_if = "SessionTools::is_all")]
    pub tools: SessionTools,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnInput {
    Agent {
        message: String,
        /// Images attached to the message, already stored in the session
        /// (see `UploadImage`). Absent when there are none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<xshell_core::ImageRef>,
    },
    Shell {
        command: String,
    },
}

impl TurnInput {
    /// An agent message without images.
    pub fn agent(message: impl Into<String>) -> Self {
        Self::Agent {
            message: message.into(),
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    TurnStarted {
        input: TurnInput,
        /// The policy actually applied after clamping to the daemon's
        /// `max_approval` ceiling.
        approval: ApprovalPolicy,
        /// The policy the client asked for, when it differed from `approval`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_approval: Option<ApprovalPolicy>,
        /// The limits an agent turn runs under, after clamping to the
        /// daemon's `max_limits`. Absent for `$` commands.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<xshell_execution::TurnLimits>,
        /// The limits the client asked for, when clamping lowered them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_limits: Option<xshell_execution::TurnLimits>,
    },
    Execution {
        event: ExecutionEvent,
    },
    ShellOutput {
        stream: String,
        text: String,
    },
    WorkingDirectoryChanged {
        cwd: PathBuf,
    },
    ShellFinished {
        command: String,
        status: String,
    },
    TurnCompleted,
    TurnFailed {
        message: String,
        /// Why the turn failed, when that is one of the causes clients act
        /// on. Absent for other failures and in journals written before the
        /// field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<TurnFailureReason>,
    },
    TurnCancelled,
    /// What an agent turn used and where its audit records are. Sent just
    /// before the turn's terminal event; absent from journals written
    /// before it existed and for `$` commands.
    TurnSummary {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<xshell_execution::TurnUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audit: Option<TurnAuditRef>,
    },
}

/// Where an agent turn's records are in the audit log, so a supervisor can
/// verify the turn there instead of trusting the event stream (H1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnAuditRef {
    /// The audit-service session holding the turn's `turn_finished` record.
    pub audit_session_id: String,
    /// The sequence of the turn's first record (its `input`). Absent when
    /// that record is in another audit session, because the connection was
    /// replaced during the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_sequence: Option<u64>,
    /// The sequence of the turn's `turn_finished` record.
    pub last_sequence: u64,
    /// The hash of that record, as the audit service acknowledged it.
    pub record_hash: String,
}

/// A failure cause a client can act on without parsing the message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureReason {
    /// A turn limit ran out.
    BudgetExhausted,
    /// Required auditing failed, so the turn was stopped.
    AuditUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEvent {
    pub sequence: u64,
    pub turn_id: String,
    pub timestamp_unix_ms: u64,
    pub event: SessionEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventBatch {
    pub events: Vec<SessionEvent>,
    pub truncated_before: Option<u64>,
    pub next_sequence: u64,
    pub active_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalReply {
    pub turn_id: String,
    pub call_id: String,
    pub decision: ApprovalDecision,
    /// Who answered, as the client reports it: a person at a terminal, or
    /// a headless run's approval channel. Recorded in the audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<xshell_execution::ApprovalSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellCompletionCandidate {
    pub display: String,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellCompletionResult {
    pub start: usize,
    pub candidates: Vec<ShellCompletionCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewResource {
    pub path: PathBuf,
    pub media_type: String,
    pub content: String,
    pub byte_len: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub columns: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyTicket {
    pub pty_id: String,
    pub ticket: String,
    pub replay_from: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f24_xshells_own_state_config_and_audit_are_always_blocked() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let audit = root.join("audit");
        let config_dir = root.join("home/.config/xshell");
        for directory in [&state, &audit, &config_dir] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let config_file = config_dir.join("config.toml");
        std::fs::write(&config_file, "").unwrap();
        let config = SessionConfig::default();
        let blocked = config.protected_paths(Some(&state), Some(&config_file), Some(&audit));
        for path in [
            state.join("sessions.json"),
            state.join("images/abc/def"),
            state.clone(),
            audit.join("sessions/one.jsonl"),
            config_file.clone(),
            config_dir.join("other.toml"),
        ] {
            assert!(blocked.matches(&path), "{} was not blocked", path.display());
        }
        // Case variants are blocked too (F13 folding).
        let upper = state.join("SESSIONS.JSON");
        assert!(blocked.matches(&upper));
        // A project beside them is not.
        assert!(!blocked.matches(&root.join("project/src/main.rs")));

        // A config file outside an `xshell` directory blocks only itself.
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let local_config = project.join("xshell.toml");
        std::fs::write(&local_config, "").unwrap();
        let blocked = config.protected_paths(Some(&state), Some(&local_config), None);
        assert!(blocked.matches(&local_config));
        assert!(!blocked.matches(&project.join("README.md")));
    }

    #[test]
    fn daemon_budgets_default_and_must_allow_one() {
        let config = SessionConfig::default();
        assert_eq!(config.max_sessions, DEFAULT_MAX_SESSIONS);
        assert_eq!(config.max_concurrent_turns, DEFAULT_MAX_CONCURRENT_TURNS);
        config.validate_budgets().unwrap();
        let zero = SessionConfig {
            max_concurrent_turns: 0,
            ..SessionConfig::default()
        };
        assert!(
            zero.validate_budgets()
                .unwrap_err()
                .to_string()
                .contains("max_concurrent_turns")
        );
    }

    #[test]
    fn session_activity_is_typed_and_reads_legacy_durable_values() {
        let activity = SessionActivity::InteractiveProcess {
            command: "emacs -nw notes.md".into(),
            attached: false,
        };
        let encoded = serde_json::to_string(&activity).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionActivity>(&encoded).unwrap(),
            activity
        );
        assert_eq!(
            serde_json::from_str::<SessionActivity>("\"idle\"").unwrap(),
            SessionActivity::Idle
        );
        assert_eq!(
            serde_json::from_str::<SessionActivity>("\"waiting_approval\"").unwrap(),
            SessionActivity::AgentTurn {
                turn_id: String::new(),
                phase: AgentTurnPhase::WaitingApproval,
            }
        );
    }
}
