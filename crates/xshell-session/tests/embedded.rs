//! A daemon built by another program, with its own compiled-in tool provider,
//! serves agent turns exactly as `xshelld` does and gives the provider its
//! calls.
//!
//! The daemon runs in this process and installs its SIGTERM handler here, so
//! this file holds one test: it signals the process to stop the daemon.

mod common;

use common::{connect_when_ready, serve_sse, sse_text, sse_tool_call};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use xshell_core::{ChatMessage, ToolDefinition};
use xshell_execution::{
    ApprovalPolicy, ExecutionEvent, ProvidedTool, ToolAccess, ToolApproval, ToolContext,
    ToolProvider, ToolRegistry,
};
use xshell_session::daemon::Daemon;
use xshell_session::{
    ModelBinding, PersistenceMode, SessionCreation, SessionEventKind, TurnInput, Visibility,
};

type Calls = Arc<Mutex<Vec<ToolContext>>>;

struct Memory {
    calls: Calls,
}

#[async_trait::async_trait]
impl ToolProvider for Memory {
    fn name(&self) -> &str {
        "memory"
    }

    fn tools(&self) -> Vec<ProvidedTool> {
        vec![ProvidedTool {
            definition: ToolDefinition {
                name: "memory_get".into(),
                description: "Read this user's memory.".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            access: ToolAccess::ReadOnly,
            approval: ToolApproval::NotRequired,
        }]
    }

    async fn call(&self, tool: &str, context: ToolContext) -> anyhow::Result<String> {
        let answer = format!(
            "{tool} for session {}",
            context.session_id.as_deref().unwrap_or("none")
        );
        self.calls.lock().unwrap().push(context);
        Ok(answer)
    }
}

#[test]
fn an_embedded_daemon_runs_its_provided_tools_in_agent_turns() {
    let (base_url, model_server) = serve_sse(vec![
        sse_tool_call("call_1", "memory_get", serde_json::json!({})),
        sse_text("remembered"),
    ]);
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join("state");
    let socket = state.join("xshelld.sock");
    let calls = Calls::default();
    let tools = ToolRegistry::builder()
        .provider(Arc::new(Memory {
            calls: Arc::clone(&calls),
        }))
        .build()
        .unwrap();
    let daemon = {
        let args = [
            "embedded-xshelld".to_owned(),
            "--no-user-config".to_owned(),
            "--state-directory".to_owned(),
            state.display().to_string(),
            "--socket".to_owned(),
            socket.display().to_string(),
        ];
        thread::spawn(move || Daemon::new().with_tools(tools).run_with_args(args))
    };

    let mut client = connect_when_ready(&socket);
    let session = client
        .create(SessionCreation {
            name: "memory".into(),
            model: ModelBinding {
                profile_name: Some("fake".into()),
                provider: "openai".into(),
                model: "fake-model".into(),
                base_url,
                api_key_env: None,
                max_history_bytes: None,
                context_tokens: None,
                reserve_output_tokens: None,
                vision: false,
            },
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            history: vec![ChatMessage::system("test")],
        })
        .unwrap();
    let session_id = session.descriptor.id.clone();
    client
        .submit(
            session_id.clone(),
            TurnInput::Agent {
                message: "what do you remember?".into(),
            },
            // Ask with nobody to answer: the read-only, ungated tool must run
            // without a prompt.
            ApprovalPolicy::Ask,
        )
        .unwrap();

    let mut after = 0;
    let mut results = Vec::new();
    let mut prompted = false;
    let mut completed = false;
    for _ in 0..100 {
        let batch = client.events(session_id.clone(), after, 500).unwrap();
        for record in batch.events {
            after = record.sequence;
            match record.event {
                SessionEventKind::Execution { event } => match event {
                    ExecutionEvent::ToolResult { result, .. } => results.push(result),
                    ExecutionEvent::ApprovalRequested { .. } => prompted = true,
                    _ => {}
                },
                SessionEventKind::TurnCompleted => completed = true,
                SessionEventKind::TurnFailed { message, .. } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
        if completed {
            break;
        }
    }
    assert!(completed, "the turn did not complete");
    assert!(!prompted, "an ungated provided tool prompted");
    assert_eq!(results, [format!("memory_get for session {session_id}")]);
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "call_1");
        // The daemon canonicalizes the cwd (macOS temp dirs sit behind a symlink).
        assert_eq!(calls[0].cwd, temporary.path().canonicalize().unwrap());
        assert_eq!(calls[0].agent, None);
    }
    let history = client.snapshot(session_id).unwrap().history;
    assert_eq!(history.last().unwrap().content, "remembered");
    model_server.join().unwrap();

    // Stop the daemon the way a service manager would.
    drop(client);
    // SAFETY: kill(2) has no memory-safety preconditions; the daemon in this
    // process handles SIGTERM as a shutdown request.
    let signalled = unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
    assert_eq!(signalled, 0);
    for _ in 0..200 {
        if daemon.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(daemon.is_finished(), "the daemon did not shut down");
    daemon.join().unwrap().unwrap();
}
