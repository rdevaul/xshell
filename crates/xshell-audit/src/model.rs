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
pub const AUDIT_FORMAT_VERSION: u32 = 3;
pub const MIN_SUPPORTED_AUDIT_FORMAT_VERSION: u32 = 1;
// Version 3 added `AuditEvent::HistoryCompacted`; version 4 adds
// `AuditEvent::TerminalStream`. Tagged enum variants are not
// forward-compatible in serde, so mixed-version peers must fail at handshake.
pub const AUDIT_PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub enabled: bool,
    pub required: bool,
    pub socket: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub checkpoint_interval: u64,
    /// Record the byte-for-byte terminal-job stream (what the operator typed
    /// and saw) in addition to job start and completion. Off by default: the
    /// audit trail exists to hold agents accountable, and terminal jobs are
    /// human-driven. When enabled, capture is performed by `xshelld` from the
    /// same buffer that feeds terminal replay.
    pub terminal_stream: bool,
    /// Upper bound on captured stream bytes per terminal job (input and output
    /// combined). Beyond it, capture stops and the final stream record for the
    /// job reports how many bytes were not recorded. `0` means no bound.
    pub terminal_stream_max_bytes: u64,
}

impl AuditConfig {
    /// Default per-job capture budget: 16 MiB.
    pub const DEFAULT_TERMINAL_STREAM_MAX_BYTES: u64 = 16 * 1024 * 1024;
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
        }
    }
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
        /// terminal jobs (the CLI) or by a pre-format-3 writer.
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
    },
    AgentError {
        message: String,
    },
    ToolRequested {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolDecision {
        call_id: String,
        decision: String,
    },
    ToolResult {
        call_id: String,
        name: String,
        result: String,
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
    /// A slice of a terminal job's byte stream, recorded only when
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
