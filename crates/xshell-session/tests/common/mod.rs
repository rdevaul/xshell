//! Helpers shared by the daemon integration tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::Duration;
use xshell_session::SessionClient;

pub fn connect_when_ready(socket: &Path) -> SessionClient {
    for _ in 0..100 {
        if let Ok(client) = SessionClient::connect(socket, "test") {
            return client;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("xshelld did not become ready at {}", socket.display());
}

pub fn serve_sse(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
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
