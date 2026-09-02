use crate::{
    ApprovalReply, AttachmentRole, EventBatch, ModelBinding, PtyExchangeResult, PtySize,
    SESSION_PROTOCOL_VERSION, SessionCreation, SessionDescriptor, SessionSnapshot,
    ShellCompletionResult, TurnInput, ViewResource,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use xshell_core::ChatMessage;
use xshell_execution::ApprovalPolicy;

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
    Snapshot {
        session_id: String,
    },
    Submit {
        session_id: String,
        input: TurnInput,
        approval: ApprovalPolicy,
    },
    Events {
        session_id: String,
        after_sequence: u64,
        wait_ms: u64,
    },
    Approve {
        session_id: String,
        reply: ApprovalReply,
    },
    Cancel {
        session_id: String,
        turn_id: String,
    },
    CompleteShell {
        session_id: String,
        line: String,
        cursor: usize,
    },
    ViewSource {
        session_id: String,
        path: PathBuf,
    },
    PtyStart {
        session_id: String,
        command: String,
        size: PtySize,
        terminal_type: Option<String>,
    },
    PtyExchange {
        pty_id: String,
        input: Vec<u8>,
        size: PtySize,
        wait_ms: u64,
    },
    PtyClose {
        pty_id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    Snapshot {
        session: SessionSnapshot,
    },
    Accepted {
        turn_id: String,
    },
    Events {
        batch: EventBatch,
    },
    ApprovalAccepted,
    CancellationAccepted,
    ShellCompletions {
        result: ShellCompletionResult,
    },
    ViewSource {
        resource: ViewResource,
    },
    PtyStarted {
        pty_id: String,
    },
    PtyExchange {
        result: PtyExchangeResult,
    },
    PtyClosed,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_requests_round_trip_as_tagged_json() {
        let request = ClientRequest::Submit {
            session_id: "session-1".into(),
            input: TurnInput::Agent {
                message: "hello".into(),
            },
            approval: ApprovalPolicy::Ask,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"request\":\"submit\""));
        assert_eq!(
            serde_json::from_str::<ClientRequest>(&encoded).unwrap(),
            request
        );
    }

    #[test]
    fn pty_exchange_preserves_binary_input() {
        let request = ClientRequest::PtyExchange {
            pty_id: "pty-1".into(),
            input: vec![0, 3, 0xff],
            size: PtySize {
                rows: 24,
                columns: 80,
            },
            wait_ms: 40,
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<ClientRequest>(&encoded).unwrap(),
            request
        );
    }
}
