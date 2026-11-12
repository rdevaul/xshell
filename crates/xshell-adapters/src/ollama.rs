use crate::images::{Laid, lay_out};
use crate::{AgentAdapter, stream_lines};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xshell_core::{
    AdapterError, AgentDescriptor, AgentEvent, AssistantResponse, ChatRequest, ToolCall,
    ToolDefinition,
};

pub struct OllamaAdapter {
    client: Client,
    base_url: String,
    model: String,
    vision: bool,
}

impl OllamaAdapter {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::http_client(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            vision: false,
        }
    }

    /// Send images to the model in each message's `images` array. Without
    /// this, images are replaced by text placeholders.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaMessage<'a>>,
    tools: Vec<OllamaToolDefinition<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct OllamaMessage<'a> {
    role: String,
    content: &'a str,
    /// Base64 image data, without a `data:` prefix.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OllamaToolCallRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<&'a str>,
}

impl<'a> From<&'a Laid<'a>> for OllamaMessage<'a> {
    fn from(laid: &'a Laid<'a>) -> Self {
        let images = laid
            .images
            .iter()
            .map(|image| image.base64.as_str())
            .collect();
        let Some(message) = laid.source else {
            return Self {
                role: laid.role(),
                content: &laid.content,
                images,
                tool_calls: Vec::new(),
                tool_name: None,
            };
        };
        Self {
            role: laid.role(),
            content: &laid.content,
            images,
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| OllamaToolCallRef {
                    kind: "function",
                    function: OllamaFunctionCallRef {
                        name: &call.name,
                        arguments: &call.arguments,
                    },
                })
                .collect(),
            tool_name: message.tool_name.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct OllamaToolCallRef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OllamaFunctionCallRef<'a>,
}

#[derive(Serialize)]
struct OllamaFunctionCallRef<'a> {
    name: &'a str,
    arguments: &'a Value,
}

#[derive(Serialize)]
struct OllamaToolDefinition<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: &'a ToolDefinition,
}

#[derive(Deserialize)]
struct OllamaChunk {
    #[serde(default)]
    message: OllamaChunkMessage,
    /// An error Ollama reports in place of a message.
    #[serde(default)]
    error: Option<Value>,
    /// Set on the final chunk of a completed response.
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct OllamaChunkMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

#[derive(Deserialize)]
struct OllamaToolCall {
    function: OllamaFunctionCall,
}

#[derive(Deserialize)]
struct OllamaFunctionCall {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[async_trait]
impl AgentAdapter for OllamaAdapter {
    fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor {
            id: "ollama".into(),
            display_name: "Ollama".into(),
            model: self.model.clone(),
            capabilities: ["chat", "streaming", "tool_calls"]
                .into_iter()
                .chain(self.vision.then_some("vision"))
                .map(String::from)
                .collect(),
        }
    }

    fn accepts_images(&self) -> bool {
        self.vision
    }

    async fn chat_stream(
        &mut self,
        request: ChatRequest,
        events: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<AssistantResponse, AdapterError> {
        let laid = lay_out(&request.messages, &request.images, self.vision);
        let messages = laid.iter().map(OllamaMessage::from).collect();
        let tools = request
            .tools
            .iter()
            .map(|function| OllamaToolDefinition {
                kind: "function",
                function,
            })
            .collect();
        let request = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&OllamaRequest {
                model: &self.model,
                messages,
                tools,
                stream: true,
            });
        let response = crate::send_with_retry(request).await?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_argument_bytes = 0_usize;
        let mut done = false;
        let mut done_reason = None;
        stream_lines(response, |line| {
            if line.trim().is_empty() {
                return Ok(());
            }
            let chunk: OllamaChunk = serde_json::from_str(line)
                .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
            if let Some(error) = &chunk.error {
                return Err(crate::stream_error(error));
            }
            if chunk.done {
                done = true;
                done_reason = chunk.done_reason;
            }
            if !chunk.message.content.is_empty() {
                content.push_str(&chunk.message.content);
                events(AgentEvent::TextDelta(chunk.message.content));
            }
            for call in chunk.message.tool_calls {
                if tool_calls.len() >= crate::MAX_TOOL_CALLS {
                    return Err(AdapterError::InvalidResponse(format!(
                        "the agent endpoint returned more than {} tool calls",
                        crate::MAX_TOOL_CALLS
                    )));
                }
                let argument_bytes = call.function.arguments.to_string().len();
                tool_argument_bytes = tool_argument_bytes
                    .checked_add(argument_bytes)
                    .filter(|size| *size <= crate::MAX_TOOL_ARGUMENT_BYTES)
                    .ok_or_else(|| {
                        AdapterError::InvalidResponse(format!(
                            "streamed tool arguments exceed {} bytes",
                            crate::MAX_TOOL_ARGUMENT_BYTES
                        ))
                    })?;
                if call.function.name.is_empty() {
                    return Err(AdapterError::InvalidResponse(format!(
                        "tool call {} has no function name",
                        tool_calls.len()
                    )));
                }
                tool_calls.push(ToolCall {
                    id: format!("ollama-call-{}", tool_calls.len()),
                    name: call.function.name,
                    arguments: call.function.arguments,
                });
            }
            Ok(())
        })
        .await?;

        crate::require_dispatchable(done, done_reason.as_deref(), tool_calls.len())?;
        Ok(AssistantResponse {
            content,
            tool_calls,
            finish_reason: done_reason,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_once;
    use serde_json::json;
    use xshell_core::{ChatMessage, ChatRequest, ToolCall};

    #[test]
    fn images_are_sent_as_base64_and_tool_images_follow_the_tool_block() {
        let image = xshell_core::ImageRef::new(
            "cd".repeat(32),
            xshell_core::ImageMediaType::Png,
            4,
            4,
            3,
            "render.png",
        )
        .unwrap();
        let bytes: xshell_core::ImageBytes = [(
            image.sha256().to_owned(),
            std::sync::Arc::new(b"png".to_vec()),
        )]
        .into();
        let call = ToolCall {
            id: "call_1".into(),
            name: "run_shell".into(),
            arguments: json!({}),
        };
        let messages = vec![
            ChatMessage::assistant_with_tools("", vec![call.clone()]),
            ChatMessage::tool_result(&call, "rendered").with_images([image]),
        ];
        let laid = lay_out(&messages, &bytes, true);
        let sent: Vec<Value> = laid
            .iter()
            .map(|laid| serde_json::to_value(OllamaMessage::from(laid)).unwrap())
            .collect();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[1]["role"], "tool");
        assert!(sent[1].get("images").is_none());
        assert_eq!(sent[2]["role"], "user");
        assert_eq!(sent[2]["images"], json!(["cG5n"]));
        assert_eq!(sent[2]["content"], "Images from the run_shell call call_1:");
    }

    const OLLAMA_CALL: &str = concat!(
        "{\"message\":{\"role\":\"assistant\",\"content\":\"\",",
        "\"tool_calls\":[{\"function\":{\"name\":\"read_file\",",
        "\"arguments\":{\"path\":\"README.md\"}}}]},\"done\":false}\n"
    );

    async fn stream(body: String) -> Result<AssistantResponse, AdapterError> {
        let (base_url, server) = serve_once("application/x-ndjson", body);
        let mut adapter = OllamaAdapter::new(base_url, "test-model");
        let result = adapter
            .chat_stream(
                ChatRequest::new(vec![ChatMessage::user("hello")], Vec::new()),
                &mut |_| {},
            )
            .await;
        server.join().unwrap();
        result
    }

    fn assert_refused(result: Result<AssistantResponse, AdapterError>, reason: &str) {
        match result {
            Err(AdapterError::InvalidResponse(message)) => assert!(
                message.contains(reason),
                "expected {reason:?} in {message:?}"
            ),
            other => panic!("expected a refused response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn f9_tool_calls_need_the_final_done_chunk() {
        assert_refused(
            stream(OLLAMA_CALL.to_owned()).await,
            "ended before the provider finished it",
        );
        let response = stream(format!(
            "{OLLAMA_CALL}{{\"message\":{{\"role\":\"assistant\",\"content\":\"\"}},\"done\":true,\"done_reason\":\"stop\"}}\n"
        ))
        .await
        .unwrap();
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn f9_truncated_responses_and_errors_dispatch_nothing() {
        assert_refused(
            stream(format!(
                "{OLLAMA_CALL}{{\"message\":{{\"role\":\"assistant\",\"content\":\"\"}},\"done\":true,\"done_reason\":\"length\"}}\n"
            ))
            .await,
            "output token limit",
        );
        assert_refused(
            stream(format!(
                "{OLLAMA_CALL}{{\"error\":\"model runner crashed\"}}\n"
            ))
            .await,
            "model runner crashed",
        );
    }

    #[tokio::test]
    async fn streams_text_and_collects_tool_calls() {
        let body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"hello \"}}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"world\",",
            "\"tool_calls\":[{\"function\":{\"name\":\"read_file\",",
            "\"arguments\":{\"path\":\"README.md\"}}}]},\"done\":true}\n"
        );
        let (base_url, server) = serve_once("application/x-ndjson", body.into());
        let mut adapter = OllamaAdapter::new(base_url, "test-model");
        let mut deltas = String::new();
        let response = adapter
            .chat_stream(
                ChatRequest::new(vec![ChatMessage::user("hello")], Vec::new()),
                &mut |event| match event {
                    AgentEvent::TextDelta(delta) => deltas.push_str(&delta),
                },
            )
            .await
            .unwrap();
        let request = server.join().unwrap();

        assert_eq!(deltas, "hello world");
        assert_eq!(response.content, deltas);
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments["path"], "README.md");
        assert!(request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn rejects_unterminated_lines_beyond_the_buffer_cap() {
        // A single line larger than the cap, never terminated by '\n'.
        let body = "x".repeat(crate::MAX_PENDING_LINE_BYTES + 1);
        let (base_url, server) = serve_once("application/x-ndjson", body);
        let mut adapter = OllamaAdapter::new(base_url, "test-model");
        let error = adapter
            .chat_stream(
                ChatRequest::new(vec![ChatMessage::user("hello")], Vec::new()),
                &mut |_| {},
            )
            .await
            .unwrap_err();
        let _ = server.join();
        assert!(
            matches!(error, AdapterError::InvalidResponse(ref message) if message.contains("without a line terminator")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn serializes_ollama_tool_results_by_name() {
        let call = ToolCall {
            id: "local-id".into(),
            name: "read_file".into(),
            arguments: json!({"path": "README.md"}),
        };
        let messages = [ChatMessage::tool_result(&call, "contents")];
        let laid = lay_out(&messages, &Default::default(), false);
        let encoded = serde_json::to_value(OllamaMessage::from(&laid[0])).unwrap();
        assert_eq!(encoded["role"], "tool");
        assert_eq!(encoded["tool_name"], "read_file");
        assert!(encoded.get("tool_call_id").is_none());
    }
}
