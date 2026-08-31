use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use xshell_core::ChatMessage;
use xshell_session::{ModelBinding, PersistenceMode, SessionClient, SessionCreation, Visibility};

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
