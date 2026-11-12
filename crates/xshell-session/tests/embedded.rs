//! A daemon built by another program, with its own compiled-in tool provider,
//! serves agent turns exactly as `xshelld` does and gives the provider its
//! calls.
//!
//! The daemon runs in this process and installs its SIGTERM handler here, so
//! this file holds one test: it signals the process to stop the daemon.

mod common;

use common::{connect_when_ready, serve_sse, sse_text, sse_tool_call};
use std::path::Path;
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
    ModelBinding, PersistenceMode, SessionClient, SessionCreation, SessionEventKind, SessionTools,
    TurnInput, Visibility,
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

fn creation(name: &str, base_url: &str, cwd: &Path, tools: SessionTools) -> SessionCreation {
    SessionCreation {
        name: name.into(),
        model: ModelBinding {
            profile_name: Some("fake".into()),
            provider: "openai".into(),
            model: "fake-model".into(),
            base_url: base_url.into(),
            api_key_env: None,
            max_history_bytes: None,
            context_tokens: None,
            reserve_output_tokens: None,
            vision: false,
        },
        cwd: cwd.into(),
        persistence: PersistenceMode::Daemon,
        visibility: Visibility::Fabric,
        history: vec![ChatMessage::system("test")],
        tools,
    }
}

/// Run one agent turn to its end and return its tool results and whether
/// anything asked for approval.
fn run_turn(
    client: &mut SessionClient,
    session_id: &str,
    message: &str,
    approval: ApprovalPolicy,
) -> (Vec<String>, bool) {
    let after_start = client
        .events(session_id.to_owned(), 0, 0)
        .unwrap()
        .events
        .last()
        .map_or(0, |record| record.sequence);
    client
        .submit(
            session_id.to_owned(),
            TurnInput::Agent {
                message: message.into(),
                images: Vec::new(),
            },
            approval,
        )
        .unwrap();
    let mut after = after_start;
    let mut results = Vec::new();
    let mut prompted = false;
    for _ in 0..100 {
        let batch = client.events(session_id.to_owned(), after, 500).unwrap();
        for record in batch.events {
            after = record.sequence;
            match record.event {
                SessionEventKind::Execution { event } => match event {
                    ExecutionEvent::ToolResult { result, .. } => results.push(result),
                    ExecutionEvent::ApprovalRequested { .. } => prompted = true,
                    _ => {}
                },
                SessionEventKind::TurnCompleted => return (results, prompted),
                SessionEventKind::TurnFailed { message, .. } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    }
    panic!("the turn did not complete");
}

#[test]
fn an_embedded_daemon_runs_its_provided_tools_in_agent_turns() {
    let (base_url, model_server) = serve_sse(vec![
        // The unrestricted session's turn.
        sse_tool_call("call_1", "memory_get", serde_json::json!({})),
        sse_text("remembered"),
        // The restricted session's turn: a tool outside its selection, then
        // one inside it.
        sse_tool_call(
            "call_1",
            "run_shell",
            serde_json::json!({"command": "touch marker"}),
        ),
        sse_tool_call("call_2", "memory_get", serde_json::json!({})),
        sse_text("restricted done"),
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
        .create(creation(
            "memory",
            &base_url,
            temporary.path(),
            SessionTools::All,
        ))
        .unwrap();
    let session_id = session.descriptor.id.clone();
    // Ask with nobody to answer: the read-only, ungated tool must run
    // without a prompt.
    let (results, prompted) = run_turn(
        &mut client,
        &session_id,
        "what do you remember?",
        ApprovalPolicy::Ask,
    );
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

    // A session limited to one tool: names the daemon does not offer, or
    // repeated names, are refused when the session is created.
    for (tools, message) in [
        (vec!["memory_nope"], "offers no tool named \"memory_nope\""),
        (vec!["memory_get", "memory_get"], "listed twice"),
    ] {
        let tools = SessionTools::Only(tools.into_iter().map(String::from).collect());
        let error = client
            .create(creation("bad", &base_url, temporary.path(), tools))
            .unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }
    let only_memory = SessionTools::Only(vec!["memory_get".into()]);
    let restricted = client
        .create(creation(
            "restricted",
            &base_url,
            temporary.path(),
            only_memory.clone(),
        ))
        .unwrap();
    assert_eq!(restricted.descriptor.tools, only_memory);
    let listed = client.list().unwrap();
    let listed = listed
        .iter()
        .find(|session| session.name == "restricted")
        .unwrap();
    assert_eq!(listed.tools, only_memory);
    let restricted_id = restricted.descriptor.id.clone();
    let (results, prompted) = run_turn(
        &mut client,
        &restricted_id,
        "try everything",
        ApprovalPolicy::Ask,
    );
    // run_shell is refused as unavailable before any approval, so nothing
    // prompts and nothing runs.
    assert!(!prompted, "a tool outside the session's selection prompted");
    assert!(!temporary.path().join("marker").exists());
    assert_eq!(
        results,
        [
            "tool error: run_shell is not available to this agent".to_owned(),
            format!("memory_get for session {restricted_id}"),
        ]
    );
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
