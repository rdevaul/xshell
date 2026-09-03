mod engine;
mod sensitive;
mod tools;

pub use engine::{
    AdapterConfig, ApprovalDecision, ApprovalPolicy, CancellationFlag, DirectShellResult,
    ExecutionEvent, TurnObserver, TurnPolicy, build_adapter, run_agent_turn, run_direct_shell,
    run_direct_shell_streaming,
};
pub use sensitive::{DEFAULT_SENSITIVE_PATTERNS, SensitivePaths};
pub use tools::{
    GateReason, definitions, execute_tool, requires_approval, requires_approval_by_name,
    tool_summary,
};
