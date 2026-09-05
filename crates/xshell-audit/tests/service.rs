use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use xshell_audit::{AUDIT_PROTOCOL_VERSION, AuditClient, AuditEvent, ServerResponse, verify_log};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn daemon_rejects_the_previous_protocol_during_handshake() {
    let temp = TempDir::new().unwrap();
    let directory = temp.path().join("logs");
    let socket = temp.path().join("audit.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshell-auditd"))
        .args(["--directory"])
        .arg(&directory)
        .args(["--socket"])
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);

    let mut stream = (0..100)
        .find_map(|_| match UnixStream::connect(&socket) {
            Ok(stream) => Some(stream),
            Err(_) => {
                thread::sleep(Duration::from_millis(10));
                None
            }
        })
        .unwrap_or_else(|| panic!("audit daemon did not become ready at {}", socket.display()));
    writeln!(
        stream,
        "{{\"request\":\"open\",\"protocol_version\":{},\"client_version\":\"old\"}}",
        AUDIT_PROTOCOL_VERSION - 1
    )
    .unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(matches!(
        serde_json::from_str::<ServerResponse>(&response).unwrap(),
        ServerResponse::Error { .. }
    ));
}

#[test]
fn daemon_accepts_a_session_and_produces_a_verifiable_log() {
    let temp = TempDir::new().unwrap();
    let directory = temp.path().join("logs");
    let socket = temp.path().join("audit.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_xshell-auditd"))
        .args(["--directory"])
        .arg(&directory)
        .args(["--socket"])
        .arg(&socket)
        .args(["--checkpoint-interval", "2"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);

    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(socket.exists(), "audit daemon did not create its socket");

    let mut client = AuditClient::connect(&socket, "test-client").unwrap();
    let session_id = client.session_id().to_owned();
    client
        .append(AuditEvent::Input {
            route: "shell".into(),
            text: "$true".into(),
        })
        .unwrap();
    let checkpoint = client.close().unwrap();
    assert!(checkpoint.body.final_checkpoint);

    let report = verify_log(
        &directory
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
        &directory.join("signing-key.pub"),
    )
    .unwrap();
    assert_eq!(report.records, 1);
    assert!(report.final_checkpoint);
}
