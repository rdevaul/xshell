use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy, ExecutionEvent};
use xshell_session::{
    AttachmentRole, ClientRequest, ModelBinding, PersistenceMode, SESSION_PROTOCOL_VERSION,
    ServerResponse, SessionActivity, SessionClient, SessionCreation, SessionEventKind, TurnInput,
    Visibility,
};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn model(name: &str) -> ModelBinding {
    ModelBinding {
        profile_name: Some(name.into()),
        provider: "ollama".into(),
        model: "qwen3:8b".into(),
        base_url: "http://127.0.0.1:11434".into(),
        api_key_env: None,
    }
}

fn connect_when_ready(socket: &Path) -> SessionClient {
    for _ in 0..100 {
        if let Ok(client) = SessionClient::connect(socket, "test") {
            return client;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("xshelld did not become ready at {}", socket.display());
}

fn send_request(writer: &mut impl Write, request: &ClientRequest) {
    serde_json::to_writer(&mut *writer, request).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
}

fn receive_response(reader: &mut impl BufRead) -> ServerResponse {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn serve_sse(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for body in responses {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let count = socket.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() - header_end < content_length {
                let count = socket.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{address}"), handle)
}

#[test]
fn daemon_preserves_and_switches_named_sessions() {
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join("state");
    let socket = state.join("xshelld.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["--state-directory", state.to_str().unwrap()])
        .args(["--socket", socket.to_str().unwrap()])
        .args(["--host-alias", "test-host", "--user", "tester"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);

    let mut client = connect_when_ready(&socket);
    let bees = client
        .create(SessionCreation {
            name: "bees".into(),
            model: model("local"),
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            history: vec![ChatMessage::system("be helpful")],
        })
        .unwrap();
    client
        .update(
            bees.descriptor.id.clone(),
            model("local"),
            temporary.path().into(),
            vec![ChatMessage::user("research bees")],
        )
        .unwrap();

    let robot = client
        .create(SessionCreation {
            name: "ornithopter".into(),
            model: model("router"),
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::HostOnly,
            history: vec![ChatMessage::user("design a robot")],
        })
        .unwrap();
    assert_eq!(client.list().unwrap().len(), 2);
    assert_eq!(robot.descriptor.name, "ornithopter");

    let restored = client.switch("bees".into()).unwrap();
    assert_eq!(restored.history, vec![ChatMessage::user("research bees")]);
    client.detach().unwrap();
    drop(client);

    let mut reconnected = connect_when_ready(&socket);
    let restored = reconnected.attach("ornithopter".into()).unwrap();
    assert_eq!(restored.history, vec![ChatMessage::user("design a robot")]);
    reconnected.close(None).unwrap();
    assert_eq!(reconnected.list().unwrap().len(), 1);
}

#[test]
fn stdio_transport_proxies_protocol_to_running_daemon() {
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join("state");
    let socket = state.join("xshelld.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["--state-directory", state.to_str().unwrap()])
        .args(["--socket", socket.to_str().unwrap()])
        .args(["--host-alias", "remote-test", "--user", "tester"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);
    let mut readiness = connect_when_ready(&socket);
    std::fs::write(temporary.path().join("remote-file.txt"), "test").unwrap();
    let shared = readiness
        .create(SessionCreation {
            name: "shared".into(),
            model: model("local"),
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            history: Vec::new(),
        })
        .unwrap();
    let private = readiness
        .create(SessionCreation {
            name: "private".into(),
            model: model("local"),
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::HostOnly,
            history: Vec::new(),
        })
        .unwrap();
    drop(readiness);

    let mut proxy = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["serve-stdio", "--socket", socket.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut writer = proxy.stdin.take().unwrap();
    let mut reader = BufReader::new(proxy.stdout.take().unwrap());
    send_request(&mut writer, &ClientRequest::open("stdio-test"));
    match receive_response(&mut reader) {
        ServerResponse::Opened {
            protocol_version,
            host_alias,
            ..
        } => {
            assert_eq!(protocol_version, SESSION_PROTOCOL_VERSION);
            assert_eq!(host_alias, "remote-test");
        }
        response => panic!("unexpected open response: {response:?}"),
    }
    send_request(&mut writer, &ClientRequest::List);
    assert!(matches!(
        receive_response(&mut reader),
        ServerResponse::Catalog { sessions }
            if sessions.len() == 1 && sessions[0].name == "shared"
    ));
    send_request(
        &mut writer,
        &ClientRequest::Attach {
            selector: "private".into(),
            role: AttachmentRole::Owner,
        },
    );
    assert!(matches!(
        receive_response(&mut reader),
        ServerResponse::Error { code, .. } if code == "remote_session_not_visible"
    ));
    send_request(
        &mut writer,
        &ClientRequest::CompleteShell {
            session_id: shared.descriptor.id,
            line: "$cat remote".into(),
            cursor: 11,
        },
    );
    assert!(matches!(
        receive_response(&mut reader),
        ServerResponse::ShellCompletions { result }
            if result.candidates.iter().any(|candidate| candidate.replacement == "remote-file.txt")
    ));
    send_request(
        &mut writer,
        &ClientRequest::CompleteShell {
            session_id: private.descriptor.id,
            line: "$cat remote".into(),
            cursor: 11,
        },
    );
    assert!(matches!(
        receive_response(&mut reader),
        ServerResponse::Error { code, .. } if code == "remote_session_not_visible"
    ));
    send_request(
        &mut writer,
        &ClientRequest::Create {
            session: SessionCreation {
                name: "remote-home".into(),
                model: model("local"),
                cwd: "~".into(),
                persistence: PersistenceMode::Daemon,
                visibility: Visibility::Fabric,
                history: Vec::new(),
            },
        },
    );
    match receive_response(&mut reader) {
        ServerResponse::Created { session } => assert_eq!(
            session.descriptor.cwd,
            Path::new(&std::env::var_os("HOME").unwrap())
                .canonicalize()
                .unwrap()
        ),
        response => panic!("unexpected create response: {response:?}"),
    }
    drop(writer);
    assert!(proxy.wait().unwrap().success());
}

#[test]
fn shell_turn_continues_after_disconnect_and_replays_on_attach() {
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join("state");
    let socket = state.join("xshelld.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["--state-directory", state.to_str().unwrap()])
        .args(["--socket", socket.to_str().unwrap()])
        .args(["--host-alias", "test-host", "--user", "tester"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);

    let mut client = connect_when_ready(&socket);
    let session = client
        .create(SessionCreation {
            name: "background".into(),
            model: model("local"),
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            history: vec![ChatMessage::system("test")],
        })
        .unwrap();
    let turn_id = client
        .submit(
            session.descriptor.id.clone(),
            TurnInput::Shell {
                command: "sleep 0.1; printf daemon-owned".into(),
            },
            ApprovalPolicy::Ask,
        )
        .unwrap();
    drop(client);

    let mut reconnected = connect_when_ready(&socket);
    for _ in 0..100 {
        match reconnected.attach("background".into()) {
            Ok(_) => break,
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    let mut after = 0;
    let mut output = String::new();
    let mut completed = false;
    for _ in 0..100 {
        let batch = reconnected
            .events(session.descriptor.id.clone(), after, 100)
            .unwrap();
        for event in batch.events {
            after = event.sequence;
            assert_eq!(event.turn_id, turn_id);
            match event.event {
                SessionEventKind::ShellOutput { text, .. } => output.push_str(&text),
                SessionEventKind::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed {
            break;
        }
    }
    assert!(completed);
    assert_eq!(output, "daemon-owned");
}

#[test]
fn running_shell_turn_can_be_cancelled() {
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join("state");
    let socket = state.join("xshelld.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["--state-directory", state.to_str().unwrap()])
        .args(["--socket", socket.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);
    let mut client = connect_when_ready(&socket);
    let session = client
        .create(SessionCreation {
            name: "cancel".into(),
            model: model("local"),
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            history: Vec::new(),
        })
        .unwrap();
    let turn_id = client
        .submit(
            session.descriptor.id.clone(),
            TurnInput::Shell {
                command: "sleep 5".into(),
            },
            ApprovalPolicy::Ask,
        )
        .unwrap();
    client
        .cancel(session.descriptor.id.clone(), turn_id)
        .unwrap();

    let mut after = 0;
    let mut cancelled = false;
    for _ in 0..50 {
        let batch = client
            .events(session.descriptor.id.clone(), after, 100)
            .unwrap();
        for event in batch.events {
            after = event.sequence;
            if matches!(event.event, SessionEventKind::TurnCancelled) {
                cancelled = true;
            }
        }
        if cancelled {
            break;
        }
    }
    assert!(cancelled);
}

#[test]
fn agent_turn_waits_for_remote_approval_then_continues() {
    let tool_response = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
        "\"id\":\"call_1\",\"function\":{\"name\":\"run_shell\",",
        "\"arguments\":\"{\\\"command\\\":\\\"printf approved\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_owned();
    let final_response = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"finished\"}}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_owned();
    let (base_url, model_server) = serve_sse(vec![tool_response, final_response]);
    let temporary = TempDir::new().unwrap();
    let state = temporary.path().join("state");
    let socket = state.join("xshelld.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["--state-directory", state.to_str().unwrap()])
        .args(["--socket", socket.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);
    let mut client = connect_when_ready(&socket);
    let session = client
        .create(SessionCreation {
            name: "approval".into(),
            model: ModelBinding {
                profile_name: Some("fake".into()),
                provider: "openai".into(),
                model: "fake-model".into(),
                base_url,
                api_key_env: None,
            },
            cwd: temporary.path().into(),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            history: vec![ChatMessage::system("test")],
        })
        .unwrap();
    let turn_id = client
        .submit(
            session.descriptor.id.clone(),
            TurnInput::Agent {
                message: "use the tool".into(),
            },
            ApprovalPolicy::Ask,
        )
        .unwrap();

    let mut after = 0;
    let call_id = loop {
        let batch = client
            .events(session.descriptor.id.clone(), after, 500)
            .unwrap();
        let mut requested = None;
        for event in batch.events {
            after = event.sequence;
            match event.event {
                SessionEventKind::Execution {
                    event: ExecutionEvent::ApprovalRequested { call },
                } => requested = Some(call.id),
                SessionEventKind::TurnFailed { message } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
        if let Some(call_id) = requested {
            break call_id;
        }
    };
    assert_eq!(
        client
            .list()
            .unwrap()
            .into_iter()
            .find(|entry| entry.id == session.descriptor.id)
            .unwrap()
            .activity,
        SessionActivity::WaitingApproval
    );
    client
        .approve(
            session.descriptor.id.clone(),
            turn_id,
            call_id,
            ApprovalDecision::Approve,
        )
        .unwrap();

    let mut completed = false;
    let mut tool_result = String::new();
    for _ in 0..100 {
        let batch = client
            .events(session.descriptor.id.clone(), after, 500)
            .unwrap();
        for event in batch.events {
            after = event.sequence;
            match event.event {
                SessionEventKind::Execution {
                    event: ExecutionEvent::ToolResult { result, .. },
                } => tool_result = result,
                SessionEventKind::TurnCompleted => completed = true,
                SessionEventKind::TurnFailed { message } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
        if completed {
            break;
        }
    }
    assert!(completed);
    assert!(tool_result.contains("approved"));
    assert!(
        client
            .snapshot(session.descriptor.id)
            .unwrap()
            .history
            .iter()
            .any(|message| message.content == "finished")
    );
    model_server.join().unwrap();
}
