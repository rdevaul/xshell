mod audit;
mod client;
mod completion;
mod execution;
mod model;
mod protocol;
mod pty;
mod pty_stream;
mod registry;
mod view;

pub use audit::{DaemonAudit, SessionAuditDescriptor, SessionAuditHandle, TerminalStreamPolicy};
pub use client::SessionClient;
pub use completion::complete_shell;
pub use execution::ExecutionCoordinator;
pub use model::{
    AccessMode, ApprovalReply, AttachmentRole, EventBatch, ModelBinding, PersistenceMode,
    PtyDescriptor, PtySize, PtyTicket, SESSION_PROTOCOL_VERSION, SessionActivity, SessionConfig,
    SessionCreation, SessionDescriptor, SessionEvent, SessionEventKind, SessionSnapshot,
    SessionStatus, ShellCompletionCandidate, ShellCompletionResult, TurnInput, ViewResource,
    Visibility,
};
pub use protocol::{ClientRequest, ServerResponse};
pub use pty::{PtyAudit, PtyClaim, PtyCoordinator, PtyReadResult};
pub use pty_stream::{
    ClientPtyFrame, PtyStreamClient, ServerPtyFrame, read_client_frame, read_server_frame,
    write_client_frame, write_server_frame,
};
pub use registry::SessionRegistry;
pub use view::load_view_resource;
