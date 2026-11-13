mod audit;
mod client;
mod completion;
mod control_token;
mod credentials;
pub mod daemon;
mod execution;
mod model;
mod protocol;
mod pty;
mod pty_stream;
mod registry;
mod turns;
mod view;

pub use audit::{
    DaemonAudit, SessionAuditDescriptor, SessionAuditHandle, TerminalStreamPolicy, audit_approval,
    audit_decision_basis, audit_image, audit_policy, audit_provided_tools, audit_tool_decision,
    audit_turn_started,
};
pub use client::{
    RemoteBootstrapAction, RemoteDaemonProbe, RemoteRepair, RemoteTarget, SessionClient,
    SessionHandshake,
};
pub use completion::complete_shell;
pub use control_token::{control_token_path, read_token, tokens_match, write_new_token};
pub use credentials::{ConfiguredModel, CredentialRoutes, default_model_base_url};
pub use execution::{DaemonPolicy, ExecutionCoordinator};
pub use model::{
    AccessMode, AgentTurnPhase, ApprovalReply, AttachmentRole, CapacityExhausted,
    DAEMON_PROBE_SCHEMA_VERSION, DEFAULT_MAX_CONCURRENT_TURNS, DEFAULT_MAX_SESSIONS,
    DaemonProbeReport, DaemonProbeStatus, EventBatch, ModelBinding, PersistenceMode, PtySize,
    PtyTicket, SESSION_PROTOCOL_VERSION, SensitivityAction, SensitivityClass, SessionActivity,
    SessionConfig, SessionCreation, SessionDescriptor, SessionEvent, SessionEventKind,
    SessionSnapshot, SessionStatus, SessionTools, ShellCompletionCandidate, ShellCompletionResult,
    TurnAuditRef, TurnFailureReason, TurnInput, ViewResource, Visibility,
};
pub use protocol::{ClientRequest, MAX_UPLOAD_IMAGE_BYTES, ServerResponse};
pub use pty::{PtyAudit, PtyClaim, PtyCoordinator, PtyReadResult};
pub use pty_stream::{
    ClientPtyFrame, PtyStreamClient, ServerPtyFrame, read_client_frame, read_server_frame,
    write_client_frame, write_server_frame,
};
pub use registry::SessionRegistry;
pub use turns::{
    MAX_RECORDED_ANSWER_BYTES, MAX_TURN_RECORDS, TurnLocation, TurnOutcome, TurnRecord, TurnStore,
};
pub use view::load_view_resource;
