mod client;
mod completion;
mod execution;
mod model;
mod protocol;
mod registry;

pub use client::SessionClient;
pub use completion::complete_shell;
pub use execution::ExecutionCoordinator;
pub use model::{
    AccessMode, ApprovalReply, AttachmentRole, EventBatch, ModelBinding, PersistenceMode,
    SESSION_PROTOCOL_VERSION, SessionActivity, SessionConfig, SessionCreation, SessionDescriptor,
    SessionEvent, SessionEventKind, SessionSnapshot, SessionStatus, ShellCompletionCandidate,
    ShellCompletionResult, TurnInput, Visibility,
};
pub use protocol::{ClientRequest, ServerResponse};
pub use registry::SessionRegistry;
