//! Helpers shared by the daemon integration tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use xshell_session::SessionClient;

/// How long a test daemon may take to start. A loaded machine (a full
/// workspace test run on a laptop) can take seconds.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub fn connect_when_ready(socket: &Path) -> SessionClient {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match SessionClient::connect(socket, "test") {
            Ok(client) => return client,
            Err(error) if Instant::now() >= deadline => panic!(
                "xshelld did not become ready at {} within {READY_TIMEOUT:?}: {error:#}",
                socket.display()
            ),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

pub fn serve_sse(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
    let (url, handle, _bodies) = serve_sse_recording(responses);
    (url, handle)
}

/// As [`serve_sse`], also keeping each request body the server received.
pub fn serve_sse_recording(
    responses: Vec<String>,
) -> (
    String,
    thread::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = std::sync::Arc::clone(&bodies);
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
            recorded
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request[header_end..]).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{address}"), handle, bodies)
}

pub fn sse_tool_call(id: &str, name: &str, arguments: serde_json::Value) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": id,
        "function": {"name": name, "arguments": arguments.to_string()},
    }]}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

pub fn sse_text(content: &str) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"content": content}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}
