use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy, ExecutionEvent};

pub const SESSION_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub enabled: bool,
    pub required: bool,
    pub socket: Option<PathBuf>,
    pub state_directory: Option<PathBuf>,
    pub default_session: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            required: true,
            socket: None,
            state_directory: None,
            default_session: "default".into(),
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivity {
    #[default]
    Idle,
    Running,
    WaitingApproval,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelBinding {
    pub profile_name: Option<String>,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnInput {
    Agent { message: String },
    Shell { command: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    TurnStarted {
        input: TurnInput,
        approval: ApprovalPolicy,
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
    },
    TurnCancelled,
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
}
