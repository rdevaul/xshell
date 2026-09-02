mod client;
mod completion;
mod execution;
mod model;
mod protocol;
mod pty;
mod registry;
mod view;

pub use client::SessionClient;
pub use completion::complete_shell;
pub use execution::ExecutionCoordinator;
pub use model::{
    AccessMode, ApprovalReply, AttachmentRole, EventBatch, ModelBinding, PersistenceMode,
    PtyExchangeResult, PtySize, SESSION_PROTOCOL_VERSION, SessionActivity, SessionConfig,
    SessionCreation, SessionDescriptor, SessionEvent, SessionEventKind, SessionSnapshot,
    SessionStatus, ShellCompletionCandidate, ShellCompletionResult, TurnInput, ViewResource,
    Visibility,
};
pub use protocol::{ClientRequest, ServerResponse};
pub use pty::PtyCoordinator;
pub use registry::SessionRegistry;
pub use view::load_view_resource;
