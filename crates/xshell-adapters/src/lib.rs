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
/// Pause before the single retry of a request whose send failed, long enough
/// for a transient route change or reset to settle.
const SEND_RETRY_DELAY: Duration = Duration::from_millis(250);
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

/// Send a provider request, retrying once if it failed before any response
/// arrived.
///
/// A pooled connection can die while it sits idle between requests, for
/// example when a network route changes or a middlebox drops its state. The
/// next request on it then fails at once, although a fresh connection would
/// succeed. Such a request never produced a response, and a chat completion
/// has no side effects beyond its cost, so one retry is safe. Timeouts are
/// not retried: a second wait of the same length rarely helps and doubles the
/// delay before the user sees the error.
pub(crate) async fn send_with_retry(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, AdapterError> {
    let retry = request.try_clone();
    let first = match request.send().await {
        Ok(response) => return Ok(response),
        Err(error) => error,
    };
    let Some(retry) = retry.filter(|_| is_retryable_send_error(&first)) else {
        return Err(transport_error(&first));
    };
    tokio::time::sleep(SEND_RETRY_DELAY).await;
    retry.send().await.map_err(|second| {
        let (first, second) = (
            describe_transport_error(&first),
            describe_transport_error(&second),
        );
        AdapterError::Transport(if first == second {
            format!("{second} (retried once with the same result)")
        } else {
            format!("{second} (retried once; the first attempt: {first})")
        })
    })
}

fn is_retryable_send_error(error: &reqwest::Error) -> bool {
    (error.is_connect() || error.is_request()) && !error.is_timeout()
}

pub(crate) fn transport_error(error: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(describe_transport_error(error))
}

/// Describe a transport failure: the phase it happened in, the URL, and the
/// underlying cause.
///
/// `reqwest::Error`'s own `Display` stops at "error sending request for url
/// (...)" and leaves the cause, such as "Connection reset by peer" or "No
/// route to host", in its `source()` chain, which is the part that says what
/// actually went wrong.
fn describe_transport_error(error: &reqwest::Error) -> String {
    let url = error
        .url()
        .map_or_else(|| "the agent endpoint".to_owned(), ToString::to_string);
    let mut description = if error.is_timeout() {
        format!("timed out waiting for {url}")
    } else if error.is_connect() {
        format!("could not connect to {url}")
    } else if error.is_request() {
        format!("the request to {url} could not be sent")
    } else if error.is_body() || error.is_decode() {
        format!("the response from {url} was cut off")
    } else {
        url
    };
    // The intermediate layers ("client error (Connect)", "tcp connect error")
    // only restate the phase; the innermost cause is the one that explains it.
    let mut root = std::error::Error::source(error);
    while let Some(cause) = root.and_then(std::error::Error::source) {
        root = Some(cause);
    }
    if let Some(cause) = root {
        description.push_str(&format!(": {cause}"));
    }
    description
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
        if let Some((prompt_tokens, available_tokens)) = context_overflow(&body) {
            return Err(AdapterError::ContextOverflow {
                prompt_tokens,
                available_tokens,
            });
        }
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
        let chunk = chunk.map_err(|error| transport_error(&error))?;
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

/// Recognize a provider's "prompt too long" rejection and report the token
/// counts it gave us. OpenAI-compatible endpoints (OpenAI, vLLM, llama.cpp,
/// SGLang, Ollama's OpenAI shim) all answer this case with
/// `code: "context_length_exceeded"` and the numbers in the message text.
/// Unknown shapes stay plain `Http` errors: guessing at an overflow we cannot
/// confirm would retry a request that was rejected for another reason.
/// Smallest number we will accept as a token count. Below this, digits in an
/// error message belong to a model name ("qwen3:8b"), not to a window.
const MIN_PLAUSIBLE_CONTEXT_TOKENS: usize = 1024;

/// Recognize a provider's "prompt too long" rejection.
///
/// Returns `Some` as soon as the overflow is recognized, even when the token
/// counts cannot be read: recovering from the overflow matters more than
/// describing it. Unknown counts are reported as `None`.
///
/// The counts are *not* taken positionally. llama.cpp writes "Prompt length
/// 262159 exceeds the available context size of 262144", but OpenAI and vLLM
/// write "maximum context length is 32768 tokens. However, you requested
/// 33000 tokens" — limit first. Each count is read from the number after the
/// phrase that names it. Only when a phrase is missing do we fall back to the
/// two largest plausible numbers: an overflow means the prompt is larger than
/// the window, but a message may also carry a breakdown ("9000 in the
/// messages") that is larger than the window, which is why the anchored read
/// comes first.
fn context_overflow(body: &str) -> Option<(Option<usize>, Option<usize>)> {
    // Native Ollama errors are `{"error": "..."}`, a bare string rather than
    // an object; non-JSON bodies (see `read_bounded_error`, which truncates
    // large errors) and plain-text gateways are scanned as raw text.
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let error = parsed.as_ref().map(|value| &value["error"]);
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let message = error
        .and_then(|error| {
            error
                .get("message")
                .or(Some(error))
                .and_then(|v| v.as_str())
        })
        .unwrap_or(body);
    let recognized = code == "context_length_exceeded"
        || message.contains("context_length_exceeded")
        || message.contains("exceeds the available context size")
        || message.contains("exceeds the model's maximum context")
        || message.contains("maximum context length")
        // Ollama's own wording, e.g. "prompt is too long: 42000 tokens > 32768 maximum".
        || message.contains("prompt is too long");
    if !recognized {
        return None;
    }
    if let (Some(prompt), Some(available)) = (
        number_after(message, PROMPT_PHRASES),
        number_after(message, WINDOW_PHRASES),
    ) {
        return Some((Some(prompt), Some(available)));
    }
    let mut counts: Vec<usize> = message
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<usize>().ok())
        .filter(|tokens| *tokens >= MIN_PLAUSIBLE_CONTEXT_TOKENS)
        .collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    // Providers repeat the limit (OpenAI restates it when splitting prompt and
    // completion tokens), and a repeated value is not a second measurement.
    counts.dedup();
    if counts.len() < 2 {
        return Some((None, None));
    }
    Some((Some(counts[0]), Some(counts[1])))
}

/// Phrases that precede the prompt's token count, lowercase.
const PROMPT_PHRASES: &[&str] = &["you requested", "prompt length", "resulted in"];
/// Phrases that precede the context window's token count, lowercase.
const WINDOW_PHRASES: &[&str] = &[
    "maximum context length is",
    "available context size of",
    "context window of",
];

/// The first number following any of `phrases` (matched case-insensitively).
fn number_after(message: &str, phrases: &[&str]) -> Option<usize> {
    let lower = message.to_ascii_lowercase();
    phrases.iter().find_map(|phrase| {
        let rest = &lower[lower.find(phrase)? + phrase.len()..];
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
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
        let chunk = chunk.map_err(|error| transport_error(&error))?;
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
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    pub fn serve_once(
        content_type: &'static str,
        body: String,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            respond(socket, content_type, &body)
        });
        (format!("http://{address}"), handle)
    }

    /// Read the first request in full and close the connection without
    /// answering, as a connection that died while pooled does, then serve
    /// `body` on the next connection. Returns how many connections it saw.
    pub fn serve_after_dropping_first(
        content_type: &'static str,
        body: String,
    ) -> (String, thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            read_request(&mut first);
            drop(first);
            let (second, _) = listener.accept().unwrap();
            respond(second, content_type, &body);
            2
        });
        (format!("http://{address}"), handle)
    }

    fn respond(mut socket: TcpStream, content_type: &str, body: &str) -> String {
        let request = read_request(&mut socket);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).unwrap();
        request
    }

    /// Read one HTTP request and return its body.
    fn read_request(socket: &mut TcpStream) -> String {
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
        String::from_utf8(request[header_end..header_end + content_length].to_vec()).unwrap()
    }
}

#[cfg(test)]
mod transport_tests {
    use super::test_support::serve_after_dropping_first;
    use super::*;
    use xshell_core::ChatMessage;

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::user("hello")],
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn retries_once_when_the_connection_dies_before_a_response() {
        // A pooled connection that died while idle fails the next request at
        // once. A fresh connection succeeds, so the turn must not fail.
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: [DONE]\n\n";
        let (base_url, server) = serve_after_dropping_first("text/event-stream", body.into());
        let mut adapter = OpenAiCompatibleAdapter::new(base_url, "test-model", None);

        let response = adapter.chat_stream(request(), &mut |_| {}).await.unwrap();

        assert_eq!(response.content, "recovered");
        assert_eq!(server.join().unwrap(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn names_the_cause_of_a_connection_failure() {
        // Bind and release a port so nothing listens on it.
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut adapter = OpenAiCompatibleAdapter::new(format!("http://{address}"), "m", None);

        let error = adapter
            .chat_stream(request(), &mut |_| {})
            .await
            .unwrap_err()
            .to_string();

        // reqwest's own text stops at "error sending request for url"; the
        // reason it failed lives in the source chain and must reach the user.
        assert!(
            error.contains(&format!("could not connect to http://{address}/")),
            "missing phase and url: {error}"
        );
        assert!(
            error.to_ascii_lowercase().contains("refused"),
            "missing the underlying cause: {error}"
        );
        assert!(error.contains("retried once"), "missing the retry: {error}");
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    #[test]
    fn recognizes_a_provider_context_overflow() {
        // The exact llama.cpp/vLLM-shaped rejection from the field.
        let body = r#"{"error":{"message":"Prompt length 262159 exceeds the available context size of 262144 tokens","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#;
        assert_eq!(
            context_overflow(body),
            Some((Some(262_159), Some(262_144))),
            "the provider's own token counts must survive the round trip"
        );
    }

    #[test]
    fn reads_openai_style_counts_limit_first() {
        // OpenAI and vLLM put the limit *first*. Taking the numbers in
        // reading order would report a prompt smaller than the window.
        let body = r#"{"error":{"message":"This model's maximum context length is 32768 tokens. However, you requested 33000 tokens in your messages.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        assert_eq!(
            context_overflow(body),
            Some((Some(33_000), Some(32_768))),
            "the larger number is the prompt, whichever way round the sentence puts it"
        );
    }

    #[test]
    fn reads_openai_counts_when_the_limit_is_repeated() {
        // OpenAI restates the limit when splitting prompt and completion
        // tokens. The repeated value is not a second measurement.
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 8300 tokens (8000 in the messages, 300 in the completion).","code":"context_length_exceeded"}}"#;
        assert_eq!(
            context_overflow(body),
            Some((Some(8_300), Some(8_192))),
            "a repeated limit must not be read as the prompt size"
        );
    }

    #[test]
    fn reads_openai_counts_when_the_messages_alone_exceed_the_limit() {
        // The message breakdown (9000) is larger than the window (8192), so
        // "the two largest numbers" would report a 9000-token window.
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 10000 tokens (9000 in the messages, 1000 in the completion).","code":"context_length_exceeded"}}"#;
        assert_eq!(
            context_overflow(body),
            Some((Some(10_000), Some(8_192))),
            "each count is read from the phrase that names it"
        );
    }

    #[test]
    fn ignores_digits_in_model_names() {
        // "qwen3:8b" contributes 3 and 8; neither is a token count.
        let body = r#"{"error":{"message":"Prompt length 262159 for qwen3:8b exceeds the available context size","code":"context_length_exceeded"}}"#;
        assert_eq!(
            context_overflow(body),
            Some((None, None)),
            "model-name digits must not be read as token counts"
        );
    }

    #[test]
    fn recognizes_native_ollama_string_errors() {
        // Ollama answers with `{"error": "..."}`, a bare string, not an object.
        let body = r#"{"error":"prompt is too long: 42000 tokens > 32768 maximum"}"#;
        assert_eq!(
            context_overflow(body),
            Some((Some(42_000), Some(32_768))),
            "the bare-string error shape must still be recognized"
        );
    }

    #[test]
    fn recognizes_overflow_in_a_truncated_body() {
        // read_bounded_error appends a truncation notice, which makes the
        // body invalid JSON. The overflow must survive that.
        let body = r#"{"error":{"message":"Prompt length 262190 exceeds the available context size of 262144 tokens"#;
        assert_eq!(
            context_overflow(body),
            Some((Some(262_190), Some(262_144))),
            "a truncated error body must not downgrade the overflow to a plain Http error"
        );
    }

    #[test]
    fn recognizes_overflow_when_the_counts_are_unreadable() {
        // The counts only describe the failure; recovering from it matters
        // more, so an unrecognized number layout still yields the variant.
        let body = r#"{"error":{"code":"context_length_exceeded","message":"Input is too long"}}"#;
        assert_eq!(
            context_overflow(body),
            Some((None, None)),
            "a recognized overflow is never dropped for want of numbers"
        );
    }

    #[test]
    fn ignores_unrelated_http_errors() {
        // Retrying these would re-send a request that was rejected for a
        // different reason entirely.
        for body in [
            r#"{"error":{"message":"invalid api key","code":"invalid_api_key"}}"#,
            r#"{"error":{"message":"model not loaded"}}"#,
            "upstream gateway timeout",
            "",
        ] {
            assert_eq!(
                context_overflow(body),
                None,
                "must not treat {body:?} as a context overflow"
            );
        }
    }

    #[test]
    fn total_response_budget_applies_across_newline_terminated_chunks() {
        let mut total = 0;
        account_response_bytes(&mut total, MAX_STREAM_RESPONSE_BYTES).unwrap();
        let error = account_response_bytes(&mut total, 1).unwrap_err();
        assert!(matches!(error, AdapterError::InvalidResponse(_)));
    }
}
