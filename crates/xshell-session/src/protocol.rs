use crate::{
    ApprovalReply, AttachmentRole, EventBatch, ModelBinding, PtySize, PtyTicket,
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
        /// The daemon's control token (F27). Without it the connection may
        /// only learn the daemon's identity, as a probe does; every other
        /// request is refused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
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
        /// The limits the client asks for. The daemon clamps them to its
        /// `max_limits`; absent means the built-in default profile.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<xshell_execution::TurnLimits>,
        /// An approval policy that can only restrict the daemon's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<xshell_execution::PolicyConfig>,
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
    /// Where a turn is: the session running it, and its durable record once
    /// it has finished. Served to local clients only.
    LocateTurn {
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
    PtyAttach {
        session_id: String,
        after_offset: Option<u64>,
    },
    PtyClose {
        session_id: String,
    },
    PtyClaim {
        ticket: String,
    },
    Detach,
    Close {
        selector: Option<String>,
    },
    /// Add an image to the session's store, for the client to attach to its
    /// next agent message. The daemon decodes and re-encodes it itself, so
    /// the dimensions it reports are its own, never the client's.
    UploadImage {
        session_id: String,
        /// The image file, base64-encoded. At most
        /// [`MAX_UPLOAD_IMAGE_BYTES`] once decoded.
        data: String,
        /// A short description, such as the file's name.
        label: String,
    },
}

/// Largest image file a client may upload: the store's per-image limit.
pub const MAX_UPLOAD_IMAGE_BYTES: u64 = xshell_execution::MAX_SOURCE_BYTES;

impl ClientRequest {
    /// An open request without a token: enough to probe the daemon.
    pub fn open(client_version: impl Into<String>) -> Self {
        Self::Open {
            protocol_version: SESSION_PROTOCOL_VERSION,
            client_version: client_version.into(),
            token: None,
        }
    }

    /// An open request carrying the daemon's control token.
    pub fn open_with_token(client_version: impl Into<String>, token: String) -> Self {
        Self::Open {
            protocol_version: SESSION_PROTOCOL_VERSION,
            client_version: client_version.into(),
            token: Some(token),
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
    TurnLocated {
        location: crate::TurnLocation,
    },
    ShellCompletions {
        result: ShellCompletionResult,
    },
    ViewSource {
        resource: ViewResource,
    },
    PtyStarted {
        ticket: PtyTicket,
    },
    PtyAttached {
        ticket: PtyTicket,
    },
    PtyClosed,
    PtyClaimed,
    Detached {
        session_id: Option<String>,
    },
    Closed {
        session_id: String,
    },
    ImageStored {
        image: xshell_core::ImageRef,
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
                images: Vec::new(),
            },
            approval: ApprovalPolicy::Ask,
            limits: None,
            policy: None,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"request\":\"submit\""));
        assert_eq!(
            serde_json::from_str::<ClientRequest>(&encoded).unwrap(),
            request
        );
    }

    #[test]
    fn pty_attach_round_trips() {
        let request = ClientRequest::PtyAttach {
            session_id: "session-1".into(),
            after_offset: Some(42),
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<ClientRequest>(&encoded).unwrap(),
            request
        );
    }
}
