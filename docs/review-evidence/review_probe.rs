use async_trait::async_trait;
use serde_json::json;
use std::{path::Path, time::Duration};
use xshell_adapters::AgentAdapter;
use xshell_core::{
    AdapterError, AgentDescriptor, AgentEvent, AssistantResponse, ChatMessage, ChatRequest,
    ToolCall,
};
use xshell_execution::*;

struct Script {
    called: bool,
    command: String,
}
#[async_trait]
impl AgentAdapter for Script {
    fn descriptor(&self) -> AgentDescriptor {
        unreachable!()
    }
    async fn chat_stream(
        &mut self,
        _: ChatRequest,
        _: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<AssistantResponse, AdapterError> {
        if self.called {
            return Err(AdapterError::Transport("simulated disconnect".into()));
        }
        self.called = true;
        Ok(AssistantResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "one".into(),
                name: "run_shell".into(),
                arguments: json!({"command": self.command}),
            }],
        })
    }
}
struct Observer(CancellationFlag);
#[async_trait]
impl TurnObserver for Observer {
    fn emit(&mut self, _: ExecutionEvent) {}
    fn cancellation(&self) -> CancellationFlag {
        self.0.clone()
    }
    async fn approve(&mut self, _: &ToolCall, _: GateReason) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

#[test]
fn sensitive_gate_depends_on_cwd_and_misses_nested_git() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("sub/.git")).unwrap();
    std::fs::write(temp.path().join("sub/.git/config"), "FAKE_TOKEN").unwrap();
    let call = |path| ToolCall {
        id: "one".into(),
        name: "read_file".into(),
        arguments: json!({"path": path}),
    };
    let policy = SensitivePaths::default();
    assert_eq!(
        requires_approval(&call("sub/.git/config"), temp.path(), &policy),
        None
    );
    assert_eq!(
        requires_approval(&call(".git/config"), &temp.path().join("sub"), &policy),
        Some(GateReason::SensitivePath)
    );
    assert_eq!(
        requires_approval(&call("config"), &temp.path().join("sub/.git"), &policy),
        None
    );
}

#[tokio::test]
async fn path_can_change_between_gate_and_execution() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("notes.txt"), "public").unwrap();
    std::fs::write(temp.path().join(".env"), "FAKE_SECRET").unwrap();
    let call = ToolCall {
        id: "one".into(),
        name: "read_file".into(),
        arguments: json!({"path": "notes.txt"}),
    };
    assert_eq!(
        requires_approval(&call, temp.path(), &SensitivePaths::default()),
        None
    );
    std::fs::remove_file(temp.path().join("notes.txt")).unwrap();
    std::os::unix::fs::symlink(".env", temp.path().join("notes.txt")).unwrap();
    assert_eq!(execute_tool(&call, temp.path()).await, "FAKE_SECRET");
}

#[tokio::test]
async fn provider_failure_loses_history_of_completed_effect() {
    let temp = tempfile::tempdir().unwrap();
    let mut agent = Script {
        called: false,
        command: "printf done > marker".into(),
    };
    let mut history = vec![ChatMessage::system("test")];
    let before = history.clone();
    let result = run_agent_turn(
        &mut agent,
        &mut history,
        "work".into(),
        temp.path(),
        &TurnPolicy::new(ApprovalPolicy::Auto),
        &mut Observer(CancellationFlag::default()),
    )
    .await;
    assert!(result.is_err());
    assert!(temp.path().join("marker").exists());
    assert_eq!(history, before);
}

#[tokio::test]
async fn cancellation_does_not_interrupt_active_shell_tool() {
    let temp = tempfile::tempdir().unwrap();
    let mut agent = Script {
        called: false,
        command: "printf ready > started; sleep 1; printf done > marker".into(),
    };
    let flag = CancellationFlag::default();
    let cancel = flag.clone();
    let root = temp.path().to_owned();
    let signal = tokio::spawn(async move {
        while !root.join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    });
    let _ = run_agent_turn(
        &mut agent,
        &mut vec![],
        "work".into(),
        temp.path(),
        &TurnPolicy::new(ApprovalPolicy::Auto),
        &mut Observer(flag),
    )
    .await;
    signal.await.unwrap();
    assert!(
        temp.path().join("marker").exists(),
        "effect executes after cancellation"
    );
}

#[tokio::test]
async fn direct_cd_accepts_regular_file() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file"), "text").unwrap();
    let result = run_direct_shell("cd file", temp.path()).await.unwrap();
    assert!(!Path::new(&result.cwd).is_dir());
}

#[tokio::test]
async fn openai_adapter_accepts_tool_call_without_completion_marker() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 16384];
        let _ = socket.read(&mut request).unwrap();
        let data = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call1", "function": {"name": "run_shell", "arguments": "{\"command\":\"true\"}"}}]}}]});
        let body = format!("data: {data}\n\n");
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let mut adapter =
        xshell_adapters::OpenAiCompatibleAdapter::new(format!("http://{address}"), "fake", None);
    let result = adapter
        .chat_stream(
            ChatRequest {
                messages: vec![],
                tools: vec![],
            },
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(result.tool_calls.len(), 1);
    server.join().unwrap();
}
