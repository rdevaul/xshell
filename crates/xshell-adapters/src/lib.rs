mod ollama;
mod openai;

use async_trait::async_trait;
use futures_util::StreamExt;
use std::time::Duration;
use xshell_core::{AdapterError, AgentDescriptor, AgentEvent, AssistantResponse, ChatRequest};

pub use ollama::OllamaAdapter;
pub use openai::OpenAiCompatibleAdapter;

/// Time allowed to establish a TCP/TLS connection to the provider.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed between consecutive bytes of a streamed response. Applies to
/// waiting for response headers as well. Generous enough for slow local models
/// to produce the first token after prompt processing, but bounded so that a
/// stalled provider cannot wedge a detached daemon turn forever.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Maximum bytes buffered while waiting for a line terminator from the
/// provider. Streamed JSON chunks and SSE events are far smaller than this.
const MAX_PENDING_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Build the HTTP client shared by all adapters. Every provider request goes
/// through this so the timeout policy is defined in exactly one place.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .build()
        .expect("static reqwest client configuration is valid")
}

#[async_trait]
pub trait AgentAdapter: Send {
    fn descriptor(&self) -> AgentDescriptor;

    async fn chat_stream(
        &mut self,
        request: ChatRequest,
        events: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<AssistantResponse, AdapterError>;
}

async fn stream_lines(
    response: reqwest::Response,
    mut consume: impl FnMut(&str) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        return Err(AdapterError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    loop {
        // `read_timeout` on the client covers socket-level stalls; this guard
        // additionally bounds the wait for the next frame at the stream layer
        // so HTTP/2 or proxy keepalives cannot hold the turn open indefinitely.
        let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                return Err(AdapterError::Transport(format!(
                    "no data received from the agent endpoint for {} seconds",
                    STREAM_IDLE_TIMEOUT.as_secs()
                )));
            }
        };
        let chunk = chunk.map_err(|error| AdapterError::Transport(error.to_string()))?;
        if pending.len().saturating_add(chunk.len()) > MAX_PENDING_LINE_BYTES {
            return Err(AdapterError::InvalidResponse(format!(
                "the agent endpoint sent more than {MAX_PENDING_LINE_BYTES} bytes without a line terminator"
            )));
        }
        pending.extend_from_slice(&chunk);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line)
                .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
            consume(line)?;
        }
    }

    if !pending.is_empty() {
        let line = std::str::from_utf8(&pending)
            .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
        consume(line)?;
    }
    Ok(())
}

#[cfg(test)]
mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    pub fn serve_once(
        content_type: &'static str,
        body: String,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
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
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request[header_end..header_end + content_length].to_vec()).unwrap()
        });
        (format!("http://{address}"), handle)
    }
}
