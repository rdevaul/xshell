use crate::{AUDIT_PROTOCOL_VERSION, AuditCheckpoint, AuditEvent};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum ClientRequest {
    Open {
        protocol_version: u32,
        client_version: String,
    },
    Append {
        event: AuditEvent,
    },
    Close,
}

impl ClientRequest {
    pub fn open(client_version: impl Into<String>) -> Self {
        Self::Open {
            protocol_version: AUDIT_PROTOCOL_VERSION,
            client_version: client_version.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum ServerResponse {
    Opened {
        protocol_version: u32,
        session_id: String,
        signing_key_id: String,
    },
    Ack {
        sequence: u64,
        record_hash: String,
    },
    Closed {
        checkpoint: AuditCheckpoint,
    },
    Error {
        message: String,
    },
}
