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

/// Prefix for a response sent only after a delay, so the turn is observably
/// running for a while.
const SLOW: &str = "SLOW:";

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
            let body = match body.strip_prefix(SLOW) {
                Some(rest) => {
                    thread::sleep(Duration::from_secs(5));
                    rest.to_owned()
                }
                None => body,
            };
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
        Self::with_config(provider_url, start_daemon, enabled, "", "")
    }

    /// As `new`, with extra keys for `[session_fabric]` and extra tables.
    fn with_config(
        provider_url: &str,
        start_daemon: bool,
        enabled: bool,
        fabric_keys: &str,
        tables: &str,
    ) -> Self {
        let directory = TempDir::new().unwrap();
        let state = directory.path().join("state");
        let socket = state.join("xshelld.sock");
        let config = directory.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "default_model = \"fake\"\n\n[models.fake]\nprovider = \"openai\"\n\
model = \"fake-model\"\nbase_url = \"{provider_url}\"\n\n[session_fabric]\n\
enabled = {enabled}\nsocket = {socket:?}\nstate_directory = {state:?}\n{fabric_keys}\n\n\
[agent.limit_profiles.tiny]\nprovider_requests = 1\n\n{tables}\n",
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

    /// `xshell SUBCOMMAND ARGS` with this fabric's configuration.
    fn job(&self, subcommand: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xshell"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.directory.path())
            .args(["--config", self.config.to_str().unwrap()])
            .arg(subcommand)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        wait_with_timeout(command.spawn().unwrap(), Duration::from_secs(30))
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
    // The result reports use against each limit (H4). Auditing is off here,
    // so there is no audit reference (H1).
    let usage = &result["usage"];
    assert_eq!(
        usage["model_rounds"],
        serde_json::json!({"used": 1, "limit": 128, "assurance": "enforced"})
    );
    assert_eq!(usage["tool_dispatches"]["used"], 0);
    assert_eq!(usage["tool_output_bytes"]["limit"], 8 * 1024 * 1024);
    assert_eq!(usage["wall_time_ms"]["limit"], 3_600_000);
    assert_eq!(
        usage["tokens"],
        serde_json::json!({"assurance": "unavailable"})
    );
    assert!(result.get("audit").is_none(), "{result}");
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

#[test]
fn a_detached_turn_can_be_inspected_and_waited_for() {
    let fabric = Fabric::new(
        &serve(vec![format!("{SLOW}{}", sse_text("finished later"))]),
        true,
        true,
    );
    let started = fabric.run(&["--detach", "--approval", "auto", "--session", "job", "work"]);
    assert_eq!(started.status.code(), Some(0), "{started:?}");
    // Text output is the session name, for `wait --session`.
    assert_eq!(String::from_utf8_lossy(&started.stdout), "job\n");

    let status = fabric.job("status", &["--session", "job", "--output", "jsonl"]);
    assert_eq!(status.status.code(), Some(0), "{status:?}");
    let status = events(&status).pop().unwrap();
    assert_eq!(status["event"], "status");
    assert_eq!(status["activity"], "agent_running");

    let early = fabric.job("result", &["--session", "job", "--output", "jsonl"]);
    assert_eq!(early.status.code(), Some(7), "{early:?}");
    assert_eq!(events(&early).pop().unwrap()["outcome"], "running");

    let waited = fabric.job("wait", &["--session", "job", "--output", "jsonl"]);
    assert_eq!(waited.status.code(), Some(0), "{waited:?}");
    let waited = events(&waited);
    // The wait replays the turn from its start.
    assert_eq!(waited.first().unwrap()["event"], "turn_started");
    let result = waited.last().unwrap();
    assert_eq!(result["outcome"], "completed");
    assert_eq!(result["answer"], "finished later");
    // A replayed turn reports its usage too.
    assert_eq!(result["usage"]["model_rounds"]["used"], 1);

    // Once finished, `result` reports the same outcome without waiting.
    let late = fabric.job("result", &["--session", "job"]);
    assert_eq!(late.status.code(), Some(0), "{late:?}");
    assert_eq!(String::from_utf8_lossy(&late.stdout), "finished later\n");
    let idle = fabric.job("status", &["--session", "job"]);
    assert_eq!(String::from_utf8_lossy(&idle.stdout), "job: idle\n");
}

#[test]
fn cancel_stops_a_detached_turn_and_reports_it() {
    let fabric = Fabric::new(&serve(vec![HOLD.into()]), true, true);
    let started = fabric.run(&[
        "--detach",
        "--approval",
        "auto",
        "--session",
        "stuck",
        "think",
    ]);
    assert_eq!(started.status.code(), Some(0), "{started:?}");
    let cancelled = fabric.job("cancel", &["--session", "stuck", "--output", "jsonl"]);
    assert_eq!(cancelled.status.code(), Some(4), "{cancelled:?}");
    let events = events(&cancelled);
    assert_eq!(events.first().unwrap()["event"], "cancel_requested");
    assert_eq!(events.last().unwrap()["outcome"], "cancelled");
    // Nothing is left to cancel.
    let again = fabric.job("cancel", &["--session", "stuck"]);
    assert_eq!(again.status.code(), Some(1), "{again:?}");
}

#[test]
fn daemon_budgets_refuse_extra_turns_and_sessions_with_exit_six() {
    let fabric = Fabric::with_config(
        &serve(vec![HOLD.into(), sse_text("done")]),
        true,
        true,
        "max_sessions = 2\nmax_concurrent_turns = 1",
        "",
    );
    let started = fabric.run(&[
        "--detach",
        "--approval",
        "auto",
        "--session",
        "stuck",
        "think",
    ]);
    assert_eq!(started.status.code(), Some(0), "{started:?}");

    // A second turn anywhere on the host is refused while the first runs.
    let refused = fabric.run(&["--output", "jsonl", "hi"]);
    assert_eq!(refused.status.code(), Some(6), "{refused:?}");
    let result = events(&refused).pop().unwrap();
    assert_eq!(result["outcome"], "unavailable");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("max_concurrent_turns"),
        "{result}"
    );

    // Once it ends, the next run starts. It fits in the session budget only
    // because the refused run closed the session it had created.
    let cancelled = fabric.job("cancel", &["--session", "stuck"]);
    assert_eq!(cancelled.status.code(), Some(4), "{cancelled:?}");
    let second = fabric.run(&["--session", "second", "hi"]);
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    assert_eq!(String::from_utf8_lossy(&second.stdout), "done\n");

    // Two sessions are kept; a third is refused.
    let full = fabric.run(&["--output", "jsonl", "--session", "third", "hi"]);
    assert_eq!(full.status.code(), Some(6), "{full:?}");
    let result = events(&full).pop().unwrap();
    assert!(
        result["message"].as_str().unwrap().contains("max_sessions"),
        "{result}"
    );
}

/// A 1x1 RGBA PNG.
const TINY_PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0xcf, 0x50,
    0x0f, 0x00, 0x03, 0x86, 0x01, 0x80, 0x5a, 0x34, 0x7d, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn attach_needs_a_vision_profile_and_sends_the_image() {
    let fabric = Fabric::with_config(&serve(vec![sse_text("a pixel")]), true, true, "", "");
    let image = fabric.directory.path().join("pixel.png");
    std::fs::write(&image, TINY_PNG).unwrap();
    // The default profile has no vision: a usage error, before connecting.
    let blind = fabric.run(&["--ephemeral", "--attach", image.to_str().unwrap(), "what?"]);
    assert_eq!(blind.status.code(), Some(2), "{blind:?}");
    assert!(String::from_utf8_lossy(&blind.stdout).is_empty());

    let config = std::fs::read_to_string(&fabric.config).unwrap();
    let with_vision = config.replace(
        "model = \"fake-model\"\n",
        "model = \"fake-model\"\nvision = true\n",
    );
    assert_ne!(config, with_vision);
    std::fs::write(&fabric.config, with_vision).unwrap();
    let missing = fabric.run(&["--ephemeral", "--attach", "/nonexistent.png", "what?"]);
    assert_eq!(missing.status.code(), Some(2), "{missing:?}");
    let seen = fabric.run(&[
        "--ephemeral",
        "--output",
        "jsonl",
        "--attach",
        image.to_str().unwrap(),
        "what is it?",
    ]);
    assert_eq!(seen.status.code(), Some(0), "{seen:?}");
    assert_eq!(events(&seen).last().unwrap()["answer"], "a pixel");
}

#[test]
fn job_commands_need_an_existing_session() {
    let fabric = Fabric::new(&serve(Vec::new()), true, true);
    let unnamed = fabric.job("wait", &[]);
    assert_eq!(unnamed.status.code(), Some(2), "{unnamed:?}");
    assert!(String::from_utf8_lossy(&unnamed.stderr).contains("--session"));
    let unknown = fabric.job("status", &["--session", "nope"]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
}

#[test]
fn detaching_in_ask_mode_warns_that_prompts_will_wait() {
    let fabric = Fabric::new(&serve(vec![sse_text("ok")]), true, true);
    let started = fabric.run(&["--detach", "--output", "jsonl", "--session", "asky", "hi"]);
    assert_eq!(started.status.code(), Some(0), "{started:?}");
    let events = events(&started);
    assert!(events.iter().any(|event| {
        event["event"] == "warning"
            && event["message"]
                .as_str()
                .unwrap()
                .contains("approval prompts wait")
    }));
    assert_eq!(
        &names(&events)[names(&events).len() - 2..],
        ["detached", "result"]
    );
    assert_eq!(events.last().unwrap()["outcome"], "detached");
}

#[test]
fn interrupting_wait_leaves_the_turn_running() {
    use std::io::BufRead;
    use std::sync::mpsc;

    let fabric = Fabric::new(&serve(vec![HOLD.into()]), true, true);
    let started = fabric.run(&[
        "--detach",
        "--approval",
        "auto",
        "--session",
        "long",
        "work",
    ]);
    assert_eq!(started.status.code(), Some(0), "{started:?}");
    let mut waiter = Command::new(env!("CARGO_BIN_EXE_xshell"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", fabric.directory.path())
        .args(["--config", fabric.config.to_str().unwrap()])
        .args(["wait", "--session", "long", "--output", "jsonl"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = waiter.id() as libc::pid_t;
    let stdout = waiter.stdout.take().unwrap();
    let (seen_tx, seen_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.contains("\"event\":\"turn_started\"") {
                let _ = seen_tx.send(());
            }
            lines.push(line);
        }
        lines
    });
    seen_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("wait never replayed the turn's start");
    // SAFETY: kill(2) has no memory-safety preconditions; the child has not
    // been reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let output = wait_with_timeout(waiter, Duration::from_secs(30));
    let lines = reader.join().unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?} {lines:?}");
    assert!(lines.last().unwrap().contains("\"outcome\":\"running\""));
    // The turn was not cancelled.
    let status = fabric.job("status", &["--session", "long"]);
    assert_eq!(
        String::from_utf8_lossy(&status.stdout)
            .trim_end()
            .split(" (")
            .next(),
        Some("long: agent running")
    );
}

fn touch(marker: &Path) -> String {
    sse_tool_call(
        "call_1",
        "run_shell",
        serde_json::json!({"command": format!("touch {}", marker.display())}),
    )
}

fn tool_result(events: &[Value]) -> String {
    events
        .iter()
        .find(|event| event["event"] == "tool_result")
        .and_then(|event| event["result"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn the_daemon_policy_always_allows_a_listed_command_without_a_prompt() {
    let fabric_dir = TempDir::new().unwrap();
    let marker = fabric_dir.path().join("made-by-policy");
    let fabric = Fabric::with_config(
        &serve(vec![touch(&marker), sse_text("made it")]),
        true,
        true,
        "",
        "[session_fabric.approval_policy]\nalways_allowed = [\"run_shell: touch *\"]\n",
    );
    let output = fabric.run(&["--output", "jsonl", "--ephemeral", "make a file"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = events(&output);
    assert!(!names(&events).contains(&"approval_needed"), "{events:?}");
    assert!(marker.exists());
}

#[test]
fn the_daemon_policy_denies_a_listed_command_even_in_auto_mode() {
    let fabric_dir = TempDir::new().unwrap();
    let marker = fabric_dir.path().join("must-not-exist");
    let fabric = Fabric::with_config(
        &serve(vec![touch(&marker), sse_text("refused")]),
        true,
        true,
        "max_approval = \"auto\"",
        "[session_fabric.approval_policy]\ndenied = [\"run_shell: touch *\"]\n",
    );
    let output = fabric.run(&[
        "--output",
        "jsonl",
        "--ephemeral",
        "--approval",
        "auto",
        "go",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        tool_result(&events(&output)),
        "tool denied by the approval policy"
    );
    assert!(!marker.exists());
}

#[test]
fn a_client_policy_can_restrict_but_never_loosen_the_daemon_policy() {
    let fabric_dir = TempDir::new().unwrap();
    // The daemon always allows `touch`; the client denies it.
    let marker = fabric_dir.path().join("restricted");
    let fabric = Fabric::with_config(
        &serve(vec![touch(&marker), sse_text("refused")]),
        true,
        true,
        "",
        "[session_fabric.approval_policy]\nalways_allowed = [\"run_shell: touch *\"]\n",
    );
    let policy = fabric_dir.path().join("client.toml");
    std::fs::write(&policy, "denied = [\"run_shell: touch *\"]\n").unwrap();
    let mut command = fabric.command(&["--output", "jsonl", "--ephemeral", "go"]);
    command.args(["--approval-policy", policy.to_str().unwrap()]);
    let output = wait_with_timeout(command.spawn().unwrap(), Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        tool_result(&events(&output)),
        "tool denied by the approval policy"
    );
    assert!(!marker.exists());

    // The daemon has no policy; the client always allows every command. In
    // ask mode the prompt remains, and with no channel it is denied.
    let marker = fabric_dir.path().join("not-loosened");
    let fabric = Fabric::new(
        &serve(vec![touch(&marker), sse_text("refused")]),
        true,
        true,
    );
    let policy = fabric_dir.path().join("loose.toml");
    std::fs::write(&policy, "always_allowed = [\"run_shell\"]\n").unwrap();
    let mut command = fabric.command(&["--output", "jsonl", "--ephemeral", "go"]);
    command.args(["--approval-policy", policy.to_str().unwrap()]);
    let output = wait_with_timeout(command.spawn().unwrap(), Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(names(&events(&output)).contains(&"approval_needed"));
    assert!(!marker.exists());
}

#[test]
fn an_invalid_client_policy_is_a_usage_error() {
    let fabric = Fabric::new(&serve(Vec::new()), true, true);
    let policy = fabric.directory.path().join("bad.toml");
    std::fs::write(&policy, "denied = [\"delegate_task: x\"]\n").unwrap();
    let mut command = fabric.command(&["--ephemeral", "go"]);
    command.args(["--approval-policy", policy.to_str().unwrap()]);
    let output = wait_with_timeout(command.spawn().unwrap(), Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn a_blocked_path_is_never_read_whatever_the_mode() {
    let fabric = Fabric::with_config(
        &serve(vec![
            sse_tool_call(
                "call_1",
                "read_file",
                serde_json::json!({"path": "server.pem"}),
            ),
            sse_text("could not read it"),
        ]),
        true,
        true,
        "max_approval = \"auto\"",
        "[[session_fabric.sensitivity]]\naction = \"block\"\npatterns = [\"*.pem\"]\n",
    );
    std::fs::write(fabric.directory.path().join("server.pem"), "PRIVATE KEY").unwrap();
    let output = fabric.run(&[
        "--output",
        "jsonl",
        "--ephemeral",
        "--approval",
        "auto",
        "read it",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = events(&output);
    let result = tool_result(&events);
    assert!(
        result.contains("blocked by the session host's policy"),
        "{result}"
    );
    assert!(!result.contains("PRIVATE KEY"));
    // Recorded as a refusal of its own, not as a call the mode approved.
    let decision = events
        .iter()
        .find(|event| event["event"] == "tool_decision")
        .expect("a tool_decision event");
    assert_eq!(decision["decision"], "deny", "{decision}");
    assert_eq!(decision["basis"]["kind"], "blocked_path", "{decision}");
}

/// Spawn `xshell run` with a pipe on descriptor 3 for approval decisions.
fn spawn_with_channel(fabric: &Fabric, args: &[&str]) -> (Child, std::fs::File) {
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    let mut fds = [0; 2];
    // SAFETY: `fds` is a writable array of two descriptors, as pipe(2) needs.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_end, write_end) = (fds[0], fds[1]);
    let mut command = fabric.command(args);
    command.args(["--approval-channel", "3"]);
    // SAFETY: the hook runs in the forked child before exec and calls only
    // dup2(2) and close(2), which are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(read_end, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(write_end);
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    // SAFETY: the parent no longer needs the read end, and owns the write end.
    unsafe { libc::close(read_end) };
    // SAFETY: `write_end` is an open descriptor this process owns.
    (child, unsafe { std::fs::File::from_raw_fd(write_end) })
}

#[test]
fn the_approval_channel_answers_prompts() {
    use std::io::BufRead;
    let fabric_dir = TempDir::new().unwrap();
    let marker = fabric_dir.path().join("approved");
    let fabric = Fabric::new(&serve(vec![touch(&marker), sse_text("done")]), true, true);
    let (mut child, mut channel) =
        spawn_with_channel(&fabric, &["--output", "jsonl", "--ephemeral", "go"]);
    let stdout = child.stdout.take().unwrap();
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "approval_needed" {
            let reply = serde_json::json!({"call_id": event["call_id"], "decision": "approve"});
            writeln!(channel, "{reply}").unwrap();
        }
        lines.push(event);
    }
    let output = wait_with_timeout(child, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(0), "{output:?} {lines:?}");
    let decided = lines
        .iter()
        .find(|event| event["event"] == "approval_decided")
        .unwrap();
    assert_eq!(decided["decision"], "approve");
    assert_eq!(decided["source"], "approval_channel");
    assert!(marker.exists());
}

#[test]
fn a_closed_approval_channel_denies() {
    let fabric_dir = TempDir::new().unwrap();
    let marker = fabric_dir.path().join("not-approved");
    let fabric = Fabric::new(
        &serve(vec![touch(&marker), sse_text("refused")]),
        true,
        true,
    );
    let (child, channel) = spawn_with_channel(&fabric, &["--output", "jsonl", "--ephemeral", "go"]);
    drop(channel);
    let output = wait_with_timeout(child, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = events(&output);
    let decided = events
        .iter()
        .find(|event| event["event"] == "approval_decided")
        .unwrap();
    assert_eq!(decided["decision"], "deny");
    assert_eq!(decided["source"], "approval_channel_closed");
    assert!(!marker.exists());
}
