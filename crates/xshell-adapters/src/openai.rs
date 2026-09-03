use crate::{AgentAdapter, stream_lines};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use xshell_core::{
    AdapterError, AgentDescriptor, AgentEvent, AssistantResponse, ChatMessage, ChatRequest,
    ToolCall, ToolDefinition,
};

pub struct OpenAiCompatibleAdapter {
    client: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl OpenAiCompatibleAdapter {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client: crate::http_client(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            api_key,
        }
    }

    fn endpoint(&self) -> String {
        let root = self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url);
        format!("{root}/v1/chat/completions")
    }
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    tools: Vec<OpenAiToolDefinition<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct OpenAiMessage<'a> {
    role: String,
    content: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenAiToolCallRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

impl<'a> From<&'a ChatMessage> for OpenAiMessage<'a> {
    fn from(message: &'a ChatMessage) -> Self {
        Self {
            role: message.role.to_string(),
            content: &message.content,
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| OpenAiToolCallRef {
                    id: &call.id,
                    kind: "function",
                    function: OpenAiFunctionCallRef {
                        name: &call.name,
                        arguments: serde_json::to_string(&call.arguments)
                            .expect("JSON values always serialize"),
                    },
                })
                .collect(),
            tool_call_id: message.tool_call_id.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct OpenAiToolCallRef<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionCallRef<'a>,
}

#[derive(Serialize)]
struct OpenAiFunctionCallRef<'a> {
    name: &'a str,
    arguments: String,
}

#[derive(Serialize)]
struct OpenAiToolDefinition<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: &'a ToolDefinition,
}

#[derive(Deserialize)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCallDelta>,
}

#[derive(Deserialize)]
struct OpenAiToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Deserialize)]
struct OpenAiFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

#[async_trait]
impl AgentAdapter for OpenAiCompatibleAdapter {
    fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor {
            id: "openai-compatible".into(),
            display_name: "OpenAI-compatible endpoint".into(),
            model: self.model.clone(),
            capabilities: vec!["chat".into(), "streaming".into(), "tool_calls".into()],
        }
    }

    async fn chat_stream(
        &mut self,
        request: ChatRequest,
        events: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<AssistantResponse, AdapterError> {
        let messages = request.messages.iter().map(OpenAiMessage::from).collect();
        let tools = request
            .tools
            .iter()
            .map(|function| OpenAiToolDefinition {
                kind: "function",
                function,
            })
            .collect();
        let mut builder = self.client.post(self.endpoint()).json(&OpenAiRequest {
            model: &self.model,
            messages,
            tools,
            stream: true,
        });
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;

        let mut content = String::new();
        let mut partial_calls = BTreeMap::<usize, ToolCallAccumulator>::new();
        let mut tool_argument_bytes = 0_usize;
        stream_lines(response, |line| {
            let Some(data) = line.trim().strip_prefix("data:") else {
                return Ok(());
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                return Ok(());
            }
            let chunk: OpenAiChunk = serde_json::from_str(data)
                .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
            for choice in chunk.choices {
                if let Some(delta) = choice.delta.content {
                    content.push_str(&delta);
                    events(AgentEvent::TextDelta(delta));
                }
                for delta in choice.delta.tool_calls {
                    if !partial_calls.contains_key(&delta.index)
                        && partial_calls.len() >= crate::MAX_TOOL_CALLS
                    {
                        return Err(AdapterError::InvalidResponse(format!(
                            "the agent endpoint returned more than {} tool calls",
                            crate::MAX_TOOL_CALLS
                        )));
                    }
                    let call = partial_calls.entry(delta.index).or_default();
                    if let Some(id) = delta.id {
                        call.id.push_str(&id);
                    }
                    if let Some(function) = delta.function {
                        if let Some(name) = function.name {
                            call.name.push_str(&name);
                        }
                        if let Some(arguments) = function.arguments {
                            tool_argument_bytes = tool_argument_bytes
                                .checked_add(arguments.len())
                                .filter(|size| *size <= crate::MAX_TOOL_ARGUMENT_BYTES)
                                .ok_or_else(|| {
                                    AdapterError::InvalidResponse(format!(
                                        "streamed tool arguments exceed {} bytes",
                                        crate::MAX_TOOL_ARGUMENT_BYTES
                                    ))
                                })?;
                            call.arguments.push_str(&arguments);
                        }
                    }
                }
            }
            Ok(())
        })
        .await?;

        let tool_calls = partial_calls
            .into_iter()
            .map(|(index, call)| {
                let arguments = if call.arguments.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&call.arguments).map_err(|error| {
                        AdapterError::InvalidResponse(format!(
                            "invalid arguments for tool {}: {error}",
                            call.name
                        ))
                    })?
                };
                Ok(ToolCall {
                    id: if call.id.is_empty() {
                        format!("openai-call-{index}")
                    } else {
                        call.id
                    },
                    name: call.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, AdapterError>>()?;

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

    #[test]
    fn openai_base_url_accepts_optional_v1_suffix() {
        let without = OpenAiCompatibleAdapter::new("http://localhost:1234", "model", None);
        let with = OpenAiCompatibleAdapter::new("http://localhost:1234/v1", "model", None);
        assert_eq!(
            without.endpoint(),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(with.endpoint(), without.endpoint());
    }

    #[tokio::test]
    async fn streams_sse_and_reassembles_tool_arguments() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello \",\"tool_calls\":[]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"world\",\"tool_calls\":[",
            "{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":",
            "{\"name\":\"file\",\"arguments\":\"th\\\":\\\"README.md\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, server) = serve_once("text/event-stream", body.into());
        let mut adapter = OpenAiCompatibleAdapter::new(base_url, "test-model", None);
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
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments["path"], "README.md");
        assert!(request.contains("\"stream\":true"));
    }

    #[test]
    fn serializes_openai_tool_results_by_call_id() {
        let call = ToolCall {
            id: "call_123".into(),
            name: "read_file".into(),
            arguments: json!({"path": "README.md"}),
        };
        let message = ChatMessage::tool_result(&call, "contents");
        let encoded = serde_json::to_value(OpenAiMessage::from(&message)).unwrap();
        assert_eq!(encoded["role"], "tool");
        assert_eq!(encoded["tool_call_id"], "call_123");
        assert!(encoded.get("tool_name").is_none());
    }
}
