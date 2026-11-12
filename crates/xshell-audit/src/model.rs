use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Format written by new audit logs. The verifier continues to accept older
/// versions down to `MIN_SUPPORTED_AUDIT_FORMAT_VERSION` so existing signed
/// logs remain readable after upgrading.
///
/// - 2: adds `AuditEvent::HistoryCompacted`.
/// - 3: adds `AuditEvent::TerminalStream` and the `terminal_stream` field on
///   `AuditEvent::LogicalSessionAttached`.
/// - 4: adds `AuditEvent::AutoContinued` and the `reasoning` and
///   `finish_reason` fields on `AuditEvent::AgentResponse`.
/// - 5: adds `AuditEvent::SubagentStarted` and `AuditEvent::SubagentFinished`.
/// - 6: adds the `images` field on `AuditEvent::Input` and
///   `AuditEvent::ToolResult`.
/// - 7: adds `AuditEvent::TurnStarted` and the `basis` and `approval` fields
///   on `AuditEvent::ToolDecision`.
/// - 8: adds `AuditEvent::TurnFinished` and the `provided_tools` field on
///   `AuditEvent::TurnStarted`.
/// - 9: adds the `offered_tools` field on `AuditEvent::TurnStarted`.
pub const AUDIT_FORMAT_VERSION: u32 = 9;
pub const MIN_SUPPORTED_AUDIT_FORMAT_VERSION: u32 = 1;
// Version 3 added `AuditEvent::HistoryCompacted`; version 4 adds
// `AuditEvent::TerminalStream`; version 5 adds `AuditEvent::AutoContinued`;
// version 6 adds `AuditEvent::SubagentStarted` and `SubagentFinished`;
// version 7 adds `images` on `Input` and `ToolResult`; version 8 adds
// `TurnStarted` and `basis` and `approval` on `ToolDecision`; version 9 adds
// `TurnFinished` and `provided_tools` on `TurnStarted`; version 10 adds
// `offered_tools` on `TurnStarted`.
// Tagged enum variants are not forward-compatible in serde, so mixed-version
// peers must fail at handshake. New fields are not either: an older service
// would drop them and sign a record without them.
pub const AUDIT_PROTOCOL_VERSION: u32 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub enabled: bool,
    pub required: bool,
    pub socket: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub checkpoint_interval: u64,
    /// Record the byte-for-byte interactive-process stream (what the operator
    /// typed and saw) in addition to job start and completion. Off by default:
    /// the audit trail exists to hold agents accountable, and interactive
    /// processes are human-driven. When enabled, capture is performed by
    /// `xshelld` from the same buffer that feeds terminal replay.
    pub terminal_stream: bool,
    /// Upper bound on captured stream bytes per interactive process (input and
    /// output combined). Beyond it, capture stops and the final stream record
    /// for the job reports how many bytes were not recorded. `0` means no
    /// bound.
    pub terminal_stream_max_bytes: u64,
    /// How long one exchange with the audit service may take, in
    /// milliseconds: sending a record and receiving its acknowledgement, or
    /// opening and closing an audit session. An exchange that misses it fails
    /// the record, and with `required = true` the action it covers.
    pub ack_timeout_ms: u64,
}

impl AuditConfig {
    /// Default per-job capture budget: 16 MiB.
    pub const DEFAULT_TERMINAL_STREAM_MAX_BYTES: u64 = 16 * 1024 * 1024;
    /// Default acknowledgement deadline: 5 seconds.
    pub const DEFAULT_ACK_TIMEOUT_MS: u64 = 5_000;

    /// The acknowledgement deadline, never zero.
    pub fn ack_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.ack_timeout_ms.max(1))
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            required: true,
            socket: None,
            directory: None,
            checkpoint_interval: 16,
            terminal_stream: false,
            terminal_stream_max_bytes: Self::DEFAULT_TERMINAL_STREAM_MAX_BYTES,
            ack_timeout_ms: Self::DEFAULT_ACK_TIMEOUT_MS,
        }
    }
}

/// A policy as the audit trail records it: its identity and its content, so
/// the identity can be checked against what was applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuditPolicy {
    /// SHA-256 of the policy's canonical JSON encoding, lowercase hex.
    pub sha256: String,
    /// The policy as configured: `always_allowed`, `allowed`, `denied`.
    pub policy: Value,
}

/// The tools providers added to a turn's agent, beyond the built-in tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuditProvidedTools {
    /// The tool registry's identity: SHA-256 of the canonical encoding of
    /// every provided tool's provider, name, description, schema, access,
    /// and approval, lowercase hex. Descriptions and schemas are not
    /// repeated here; they come from the binary that registered them.
    pub sha256: String,
    /// Each provided tool, in the order the model was offered them.
    pub tools: Vec<AuditProvidedTool>,
}

/// One provided tool and the facts authorization used.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditProvidedTool {
    pub provider: String,
    pub name: String,
    /// `read_only` or `read_write`.
    pub access: String,
    /// `required` or `not_required`.
    pub approval: String,
}

/// What settled a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditDecisionBasis {
    /// `entry`, `not_listed`, `uncheckable_command`, `mode_default`,
    /// `not_in_toolset`, `turn_limit`, or `blocked_path`.
    pub kind: String,
    /// `base` or `overlay`, for a decision a policy made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// `denied`, `always_allowed`, or `allowed`, for a matched entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    /// The matched entry as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

/// Who answered a prompted call. For a remote client this is the client's
/// own report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditApproval {
    /// `interactive`, `approval_channel`, `no_approver`, `cancelled`, or
    /// `unreported`.
    pub approver: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An image as the audit trail records it: what it was, never its bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditImage {
    /// SHA-256 of the stored (re-encoded) bytes, lowercase hex.
    pub sha256: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    /// The source path or the tool that produced it.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    SessionStarted {
        client_version: String,
        cwd: String,
        model_profile: String,
        provider: String,
        model: String,
        endpoint: String,
        system_prompt: String,
        approval: String,
    },
    LogicalSessionAttached {
        action: String,
        session_id: String,
        name: String,
        host_id: String,
        host_alias: String,
        user: String,
        /// Whether byte-for-byte terminal-stream capture was enabled for this
        /// audit session. Recorded by `xshelld` in the session's first record
        /// so a reader can distinguish "no terminal output" from "not
        /// captured". `None` when written by a component that does not run
        /// interactive processes (the CLI) or by a pre-format-3 writer.
        #[serde(default)]
        terminal_stream: Option<bool>,
    },
    LogicalSessionDetached {
        action: String,
        session_id: String,
        name: String,
    },
    Input {
        route: String,
        text: String,
        /// Images attached to the input. Absent when there are none, so
        /// records from earlier formats hash unchanged.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<AuditImage>,
    },
    WorkingDirectoryChanged {
        cwd: String,
    },
    ModelSwitched {
        profile: String,
        model: String,
    },
    AgentResponse {
        content: String,
        tool_call_count: usize,
        partial: bool,
        /// The model's reasoning for this response, when the provider streams
        /// it separately. Skipped when absent so records written before
        /// format 4 re-serialize, and therefore hash, unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        /// The provider's reason for ending generation, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
    },
    /// The model ended a response without a tool call while apparently
    /// mid-task, so xshell sent `prompt` as a user message and kept the turn
    /// going. `reason` names the heuristic that fired.
    AutoContinued {
        reason: String,
        prompt: String,
    },
    AgentError {
        message: String,
    },
    ToolRequested {
        call_id: String,
        name: String,
        arguments: Value,
    },
    /// The authority an agent turn runs under, recorded before its first
    /// tool call: the effective approval mode and limits, and each policy
    /// with its identity. Decisions that follow, up to the next
    /// `TurnStarted`, were made under it.
    TurnStarted {
        turn_id: String,
        /// The effective mode: `off`, `ask`, or `auto`.
        approval: String,
        /// The mode the client asked for, when the session host lowered it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_approval: Option<String>,
        /// The effective turn limits.
        limits: Value,
        /// The limits the client asked for, when the session host lowered
        /// any of them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_limits: Option<Box<Value>>,
        /// The session host's policy; absent when it has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_policy: Option<Box<AuditPolicy>>,
        /// The client's overlay; absent when the client sent none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlay_policy: Option<Box<AuditPolicy>>,
        /// Tools providers registered with the session host; absent when it
        /// has none, so records without them hash as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provided_tools: Option<Box<AuditProvidedTools>>,
        /// The tools the agent was offered, when not every built-in and
        /// provided tool: the session limits its agent to these. Absent when
        /// it offers them all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offered_tools: Option<Vec<String>>,
    },
    /// How an agent turn ended and what it used: the turn's last record.
    /// A supervisor finds it from the identifiers a headless run reports,
    /// and checks the run's own account against it.
    TurnFinished {
        turn_id: String,
        /// `completed`, `failed`, or `cancelled`.
        outcome: String,
        /// Why it failed, when known: `budget_exhausted` or
        /// `audit_unavailable`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Use against each turn limit, with its assurance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Box<Value>>,
    },
    ToolDecision {
        call_id: String,
        decision: String,
        /// What settled the call, or led to its prompt. Absent in records
        /// from earlier formats.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        basis: Option<AuditDecisionBasis>,
        /// Who answered, when the call was prompted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval: Option<AuditApproval>,
    },
    ToolResult {
        call_id: String,
        name: String,
        result: String,
        /// Images the tool returned to the model. Absent when there are
        /// none, so records from earlier formats hash unchanged.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<AuditImage>,
    },
    ViewOperation {
        path: String,
        sha256: Option<String>,
        viewer: Option<String>,
        media_type: Option<String>,
        byte_len: Option<u64>,
        outcome: String,
    },
    ShellFinished {
        command: String,
        outcome: String,
        cwd: String,
    },
    /// A slice of an interactive process's byte stream, recorded only when
    /// `audit.terminal_stream` is enabled. `direction` is `"input"` (operator
    /// keystrokes delivered to the job) or `"output"` (bytes the job wrote).
    /// `offset` is the position of the first byte of `data` within that
    /// direction's stream for this job; `data` is standard base64.
    /// `dropped_bytes` is non-zero only on the job's final stream record and
    /// counts bytes that were not recorded because the capture budget was
    /// exhausted.
    TerminalStream {
        command: String,
        direction: String,
        offset: u64,
        data: String,
        dropped_bytes: u64,
    },
    /// Older conversation turns were dropped (or summarized) to stay within
    /// the configured history budget. Recorded so a reader of the trail knows
    /// the model did not see the full transcript from this point on.
    HistoryCompacted {
        compactor: String,
        messages_before: usize,
        messages_after: usize,
        bytes_before: usize,
        bytes_after: usize,
        turns_removed: usize,
    },
    /// A `delegate_task` call started a subagent. Turns run sequentially, so
    /// every record up to the matching `SubagentFinished` belongs to `agent`;
    /// its tool-call ids are also prefixed with `agent/`.
    SubagentStarted {
        agent: String,
        task: String,
        tools: Vec<String>,
    },
    /// `outcome` is `completed`, `aborted`, `cancelled`, `budget_exhausted`,
    /// or `failed`.
    SubagentFinished {
        agent: String,
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    SessionEnded {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecordBody {
    pub format_version: u32,
    pub session_id: String,
    pub sequence: u64,
    pub daemon_timestamp_unix_ms: u64,
    pub client_uid: u32,
    pub previous_hash: String,
    pub event: AuditEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecord {
    pub body: AuditRecordBody,
    pub record_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointBody {
    pub format_version: u32,
    pub session_id: String,
    pub checkpoint_sequence: u64,
    pub previous_checkpoint_hash: String,
    pub sequence: u64,
    pub daemon_timestamp_unix_ms: u64,
    pub chain_head: String,
    pub signing_key_id: String,
    pub final_checkpoint: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WitnessCommitment {
    pub scheme: String,
    pub commitment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditCheckpoint {
    pub body: CheckpointBody,
    pub blinding_nonce: String,
    pub witness: WitnessCommitment,
    pub signature: String,
    pub checkpoint_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum AuditLogEntry {
    Record(AuditRecord),
    Checkpoint(AuditCheckpoint),
}
