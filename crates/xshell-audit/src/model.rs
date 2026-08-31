use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const AUDIT_FORMAT_VERSION: u32 = 1;
pub const AUDIT_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub enabled: bool,
    pub required: bool,
    pub socket: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub checkpoint_interval: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            required: true,
            socket: None,
            directory: None,
            checkpoint_interval: 16,
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
    ShellFinished {
        command: String,
        outcome: String,
        cwd: String,
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
