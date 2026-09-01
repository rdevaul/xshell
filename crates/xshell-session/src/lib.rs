mod client;
mod execution;
mod model;
mod protocol;
mod registry;

pub use client::SessionClient;
pub use execution::ExecutionCoordinator;
pub use model::{
    AccessMode, ApprovalReply, AttachmentRole, EventBatch, ModelBinding, PersistenceMode,
    SESSION_PROTOCOL_VERSION, SessionActivity, SessionConfig, SessionCreation, SessionDescriptor,
    SessionEvent, SessionEventKind, SessionSnapshot, SessionStatus, TurnInput, Visibility,
};
pub use protocol::{ClientRequest, ServerResponse};
pub use registry::SessionRegistry;
