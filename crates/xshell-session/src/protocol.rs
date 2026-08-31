use crate::{
    AttachmentRole, ModelBinding, SESSION_PROTOCOL_VERSION, SessionCreation, SessionDescriptor,
    SessionSnapshot,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use xshell_core::ChatMessage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum ClientRequest {
    Open {
        protocol_version: u32,
        client_version: String,
    },
    List,
    Create {
        session: SessionCreation,
    },
    Attach {
        selector: String,
        role: AttachmentRole,
    },
    Switch {
        selector: String,
        role: AttachmentRole,
    },
    Update {
        session_id: String,
        model: ModelBinding,
        cwd: PathBuf,
        history: Vec<ChatMessage>,
    },
    Detach,
    Close {
        selector: Option<String>,
    },
}

impl ClientRequest {
    pub fn open(client_version: impl Into<String>) -> Self {
        Self::Open {
            protocol_version: SESSION_PROTOCOL_VERSION,
            client_version: client_version.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum ServerResponse {
    Opened {
        protocol_version: u32,
        client_id: String,
        host_id: String,
        host_alias: String,
        user: String,
    },
    Catalog {
        sessions: Vec<SessionDescriptor>,
    },
    Created {
        session: SessionSnapshot,
    },
    Attached {
        session: SessionSnapshot,
        role: AttachmentRole,
    },
    Updated {
        session: SessionDescriptor,
    },
    Detached {
        session_id: Option<String>,
    },
    Closed {
        session_id: String,
    },
    Error {
        code: String,
        message: String,
    },
}
