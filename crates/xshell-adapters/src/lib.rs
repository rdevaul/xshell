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
/// Absolute ceiling for one streamed model response, even if the endpoint
/// keeps sending bytes often enough to avoid the idle timeout.
const MAX_STREAM_DURATION: Duration = Duration::from_secs(60 * 60);
/// Maximum bytes buffered while waiting for a line terminator from the
/// provider. Streamed JSON chunks and SSE events are far smaller than this.
const MAX_PENDING_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum wire bytes accepted for one successful streamed response. This
/// bounds accumulated model text and tool-call data even when every event is
/// individually well formed and newline terminated.
const MAX_STREAM_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Error pages are diagnostic only and should never consume arbitrary memory.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TOOL_CALLS: usize = 256;
pub(crate) const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;

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
        let body = read_bounded_error(response).await?;
        return Err(AdapterError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut total_bytes = 0_usize;
    let deadline = tokio::time::Instant::now() + MAX_STREAM_DURATION;
    loop {
        // `read_timeout` on the client covers socket-level stalls; this guard
        // additionally bounds the wait for the next frame at the stream layer
        // so HTTP/2 or proxy keepalives cannot hold the turn open indefinitely.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(stream_duration_error());
        }
        let chunk =
            match tokio::time::timeout(remaining.min(STREAM_IDLE_TIMEOUT), stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(stream_duration_error());
                    }
                    return Err(AdapterError::Transport(format!(
                        "no data received from the agent endpoint for {} seconds",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    )));
                }
            };
        let chunk = chunk.map_err(|error| AdapterError::Transport(error.to_string()))?;
        account_response_bytes(&mut total_bytes, chunk.len())?;
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

fn stream_duration_error() -> AdapterError {
    AdapterError::Transport(format!(
        "the agent endpoint response exceeded {} seconds",
        MAX_STREAM_DURATION.as_secs()
    ))
}

fn account_response_bytes(total: &mut usize, chunk: usize) -> Result<(), AdapterError> {
    *total = total.saturating_add(chunk);
    if *total > MAX_STREAM_RESPONSE_BYTES {
        return Err(AdapterError::InvalidResponse(format!(
            "the agent endpoint response exceeds {MAX_STREAM_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(())
}

async fn read_bounded_error(response: reqwest::Response) -> Result<String, AdapterError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::with_capacity(MAX_ERROR_BODY_BYTES.min(8 * 1024));
    let mut truncated = false;
    while let Some(chunk) = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next())
        .await
        .map_err(|_| {
            AdapterError::Transport(format!(
                "no error-response data received for {} seconds",
                STREAM_IDLE_TIMEOUT.as_secs()
            ))
        })?
    {
        let chunk = chunk.map_err(|error| AdapterError::Transport(error.to_string()))?;
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        let retained = remaining.min(chunk.len());
        body.extend_from_slice(&chunk[..retained]);
        if retained < chunk.len() {
            truncated = true;
            break;
        }
    }
    let mut body = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        body.push_str("\n[error body truncated]");
    }
    Ok(body)
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

#[cfg(test)]
mod bounds_tests {
    use super::*;

    #[test]
    fn total_response_budget_applies_across_newline_terminated_chunks() {
        let mut total = 0;
        account_response_bytes(&mut total, MAX_STREAM_RESPONSE_BYTES).unwrap();
        let error = account_response_bytes(&mut total, 1).unwrap_err();
        assert!(matches!(error, AdapterError::InvalidResponse(_)));
    }
}
