use crate::{AgentAdapter, stream_lines};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xshell_core::{
    AdapterError, AgentDescriptor, AgentEvent, AssistantResponse, ChatMessage, ChatRequest,
    ToolCall, ToolDefinition,
};

pub struct OllamaAdapter {
    client: Client,
    base_url: String,
    model: String,
}

impl OllamaAdapter {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::http_client(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
        }
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OllamaToolCallRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<&'a str>,
}

impl<'a> From<&'a ChatMessage> for OllamaMessage<'a> {
    fn from(message: &'a ChatMessage) -> Self {
        Self {
            role: message.role.to_string(),
            content: &message.content,
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
    message: OllamaChunkMessage,
}

#[derive(Deserialize)]
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
            capabilities: vec!["chat".into(), "streaming".into(), "tool_calls".into()],
        }
    }

    async fn chat_stream(
        &mut self,
        request: ChatRequest,
        events: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<AssistantResponse, AdapterError> {
        let messages = request.messages.iter().map(OllamaMessage::from).collect();
        let tools = request
            .tools
            .iter()
            .map(|function| OllamaToolDefinition {
                kind: "function",
                function,
            })
            .collect();
        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&OllamaRequest {
                model: &self.model,
                messages,
                tools,
                stream: true,
            })
            .send()
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_argument_bytes = 0_usize;
        stream_lines(response, |line| {
            if line.trim().is_empty() {
                return Ok(());
            }
            let chunk: OllamaChunk = serde_json::from_str(line)
                .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
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
                tool_calls.push(ToolCall {
                    id: format!("ollama-call-{}", tool_calls.len()),
                    name: call.function.name,
                    arguments: call.function.arguments,
                });
            }
            Ok(())
        })
        .await?;

        Ok(AssistantResponse {
            content,
            tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_once;
    use serde_json::json;
    use xshell_core::{ChatMessage, ChatRequest, ToolCall};

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
                ChatRequest {
                    messages: vec![ChatMessage::user("hello")],
                    tools: Vec::new(),
                },
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
                ChatRequest {
                    messages: vec![ChatMessage::user("hello")],
                    tools: Vec::new(),
                },
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
        let message = ChatMessage::tool_result(&call, "contents");
        let encoded = serde_json::to_value(OllamaMessage::from(&message)).unwrap();
        assert_eq!(encoded["role"], "tool");
        assert_eq!(encoded["tool_name"], "read_file");
        assert!(encoded.get("tool_call_id").is_none());
    }
}
