//! End-to-end tests of `xshell run` against a real `xshelld` and a scripted
//! OpenAI-compatible provider.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// A response body that is never sent: the connection is held open, as a
/// provider that is still thinking would.
const HOLD: &str = "HOLD";

fn xshelld_binary() -> PathBuf {
    let candidate = PathBuf::from(env!("CARGO_BIN_EXE_xshell")).with_file_name("xshelld");
    if !candidate.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "xshell-session", "--bin", "xshelld"])
            .status()
            .expect("cargo build for xshelld");
        assert!(status.success(), "could not build xshelld");
    }
    candidate
}

fn sse_text(content: &str) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"content": content}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

fn sse_tool_call(id: &str, name: &str, arguments: Value) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": id,
        "function": {"name": name, "arguments": arguments.to_string()},
    }]}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// Serve one scripted response per provider request.
fn serve(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let mut held = Vec::new();
        for body in responses {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            let header_end = loop {
                let count = socket.read(&mut buffer).unwrap_or(0);
                if count == 0 {
                    break None;
                }
                request.extend_from_slice(&buffer[..count]);
                if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break Some(index + 4);
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() - header_end < length {
                let count = socket.read(&mut buffer).unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            if body == HOLD {
                held.push(socket);
                continue;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
        }
        // Keep held connections open until the test process exits.
        thread::sleep(Duration::from_secs(60));
        drop(held);
    });
    format!("http://{address}")
}

struct Fabric {
    directory: TempDir,
    config: PathBuf,
    daemon: Option<Child>,
}

impl Fabric {
    /// A config whose default model is the scripted provider, with the
    /// session fabric enabled. `start_daemon` controls whether xshelld runs.
    fn new(provider_url: &str, start_daemon: bool, enabled: bool) -> Self {
        let directory = TempDir::new().unwrap();
        let state = directory.path().join("state");
        let socket = state.join("xshelld.sock");
        let config = directory.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "default_model = \"fake\"\n\n[models.fake]\nprovider = \"openai\"\n\
model = \"fake-model\"\nbase_url = \"{provider_url}\"\n\n[session_fabric]\n\
enabled = {enabled}\nsocket = {socket:?}\nstate_directory = {state:?}\n\n\
[agent.limit_profiles.tiny]\nprovider_requests = 1\n",
                socket = socket.to_str().unwrap(),
                state = state.to_str().unwrap(),
            ),
        )
        .unwrap();
        let daemon = start_daemon.then(|| {
            let child = Command::new(xshelld_binary())
                .args(["--config", config.to_str().unwrap()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            wait_for_socket(&socket);
            child
        });
        Self {
            directory,
            config,
            daemon,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xshell"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.directory.path())
            .args(["--config", self.config.to_str().unwrap()])
            .args(["--cwd", self.directory.path().to_str().unwrap()])
            .arg("run")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let child = self.command(args).spawn().unwrap();
        wait_with_timeout(child, Duration::from_secs(30))
    }
}

impl Drop for Fabric {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

fn wait_for_socket(socket: &Path) {
    let started = Instant::now();
    while UnixStream::connect(socket).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "xshelld did not start at {}",
            socket.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Output {
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "xshell run did not finish\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn events(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect()
}

fn names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect()
}

#[test]
fn a_completed_run_streams_jsonl_and_exits_zero() {
    let fabric = Fabric::new(&serve(vec![sse_text("the answer is 42")]), true, true);
    let output = fabric.run(&["--output", "jsonl", "--ephemeral", "what is the answer?"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = events(&output);
    let names = names(&events);
    assert_eq!(names.first(), Some(&"run_started"));
    assert!(names.contains(&"turn_started"), "{names:?}");
    assert!(names.contains(&"assistant_message"), "{names:?}");
    assert_eq!(&names[names.len() - 2..], ["turn_completed", "result"]);
    assert_eq!(events[0]["schema"], 1);
    let result = events.last().unwrap();
    assert_eq!(result["outcome"], "completed");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["answer"], "the answer is 42");
    assert_eq!(result["schema"], 1);
    // The turn ran under the default profile's limits.
    let started = events
        .iter()
        .find(|event| event["event"] == "turn_started")
        .unwrap();
    assert_eq!(started["limits"]["provider_requests"], 128);
}

#[test]
fn text_output_prints_only_the_answer_on_stdout() {
    let fabric = Fabric::new(&serve(vec![sse_text("plain answer")]), true, true);
    let output = fabric.run(&["--ephemeral", "say something"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "plain answer\n");
}

#[test]
fn the_prompt_can_come_from_stdin() {
    let fabric = Fabric::new(&serve(vec![sse_text("from stdin")]), true, true);
    let mut child = fabric
        .command(&["--ephemeral"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a prompt on stdin\n")
        .unwrap();
    let output = wait_with_timeout(child, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from stdin\n");
}

#[test]
fn approval_prompts_are_denied_without_a_channel_and_the_command_never_runs() {
    let directory = TempDir::new().unwrap();
    let marker = directory.path().join("must-not-exist");
    let provider = serve(vec![
        sse_tool_call(
            "call_1",
            "run_shell",
            serde_json::json!({"command": format!("touch {}", marker.display())}),
        ),
        sse_text("I could not run it."),
    ]);
    let fabric = Fabric::new(&provider, true, true);
    let output = fabric.run(&["--output", "jsonl", "--ephemeral", "make a file"]);
    // The model is told about the denial and finishes the turn.
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = events(&output);
    let names = names(&events);
    assert!(names.contains(&"approval_needed"), "{names:?}");
    let decided = events
        .iter()
        .find(|event| event["event"] == "approval_decided")
        .unwrap();
    assert_eq!(decided["decision"], "deny");
    assert_eq!(decided["source"], "no_approval_channel");
    assert_eq!(events.last().unwrap()["approvals_denied"], 1);
    assert!(!marker.exists());
}

#[test]
fn an_exhausted_budget_exits_three() {
    // The tiny profile allows one provider request; the tool call needs a
    // second one to finish.
    let provider = serve(vec![sse_tool_call(
        "call_1",
        "list_directory",
        serde_json::json!({"path": "."}),
    )]);
    let fabric = Fabric::new(&provider, true, true);
    let mut command = fabric.command(&["--output", "jsonl", "--ephemeral", "look around"]);
    command.args(["--limits", "tiny"]);
    let output = wait_with_timeout(command.spawn().unwrap(), Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let events = events(&output);
    let failed = events
        .iter()
        .find(|event| event["event"] == "turn_failed")
        .unwrap();
    assert_eq!(failed["reason"], "budget_exhausted");
    let result = events.last().unwrap();
    assert_eq!(result["outcome"], "budget_exhausted");
    assert_eq!(result["exit_code"], 3);
}

#[test]
fn a_missing_daemon_exits_six() {
    let fabric = Fabric::new(&serve(Vec::new()), false, true);
    let output = fabric.run(&["--output", "jsonl", "hello"]);
    assert_eq!(output.status.code(), Some(6), "{output:?}");
    let result = events(&output).pop().unwrap();
    assert_eq!(result["outcome"], "unavailable");
}

#[test]
fn a_disabled_session_fabric_is_a_usage_error() {
    let fabric = Fabric::new(&serve(Vec::new()), false, false);
    let output = fabric.run(&["hello"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("session_fabric.enabled"));
}

#[test]
fn sigterm_cancels_the_turn_and_exits_four() {
    use std::io::BufRead;
    use std::sync::mpsc;

    let fabric = Fabric::new(&serve(vec![HOLD.into()]), true, true);
    let mut child = fabric
        .command(&["--output", "jsonl", "--ephemeral", "think for a long time"])
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    // Read events as they arrive; signal once the turn has started.
    let stdout = child.stdout.take().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.contains("\"event\":\"turn_started\"") {
                let _ = started_tx.send(());
            }
            lines.push(line);
        }
        lines
    });
    started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the run never started its turn");
    // SAFETY: kill(2) has no memory-safety preconditions; the child has not
    // been reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let output = wait_with_timeout(child, Duration::from_secs(30));
    let lines = reader.join().unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?} {lines:?}");
    let events: Vec<Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let names = names(&events);
    assert!(names.contains(&"cancel_requested"), "{names:?}");
    assert_eq!(&names[names.len() - 2..], ["turn_cancelled", "result"]);
    assert_eq!(events.last().unwrap()["outcome"], "cancelled");
}
