use crate::images::{Laid, lay_out};
use crate::{AgentAdapter, stream_lines};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use xshell_core::{
    AdapterError, AgentDescriptor, AgentEvent, AssistantResponse, ChatRequest, ToolCall,
    ToolDefinition,
};

pub struct OpenAiCompatibleAdapter {
    client: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    vision: bool,
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
            vision: false,
        }
    }

    /// Send images to the model as image parts. Without this, images are
    /// replaced by text placeholders.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
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
    content: OpenAiContent<'a>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenAiToolCallRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    /// Prior reasoning, in the `reasoning_content` field used by vLLM,
    /// SGLang, llama.cpp, TabbyAPI and DeepSeek. Templates such as Qwen3.x
    /// re-render it; without it every earlier step looks like an empty
    /// `<think>` block to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
}

/// A message's content: plain text, or text and image parts when it carries
/// images.
#[derive(Serialize)]
#[serde(untagged)]
enum OpenAiContent<'a> {
    Text(&'a str),
    Parts(Vec<OpenAiPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiPart {
    Text { text: String },
    ImageUrl { image_url: OpenAiImageUrl },
}

#[derive(Serialize)]
struct OpenAiImageUrl {
    url: String,
}

impl<'a> From<&'a Laid<'a>> for OpenAiMessage<'a> {
    fn from(laid: &'a Laid<'a>) -> Self {
        let content = if laid.images.is_empty() {
            OpenAiContent::Text(&laid.content)
        } else {
            let mut parts = vec![OpenAiPart::Text {
                text: laid.content.to_string(),
            }];
            parts.extend(laid.images.iter().map(|image| OpenAiPart::ImageUrl {
                image_url: OpenAiImageUrl {
                    url: image.data_url(),
                },
            }));
            OpenAiContent::Parts(parts)
        };
        let Some(message) = laid.source else {
            return Self {
                role: laid.role(),
                content,
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
            };
        };
        Self {
            role: laid.role(),
            content,
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
            reasoning_content: message.reasoning.as_deref(),
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
    /// An error the provider reports inside a successful stream.
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    /// xshell requests a single choice, so only index 0 is valid.
    #[serde(default)]
    index: usize,
    #[serde(default)]
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
    /// Reasoning streamed apart from the answer. `reasoning` is OpenRouter's
    /// spelling of the same thing.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
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
        let messages = laid.iter().map(OpenAiMessage::from).collect();
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
            crate::check_credential_endpoint(&self.base_url)?;
            builder = builder.bearer_auth(api_key);
        }
        let response = crate::send_with_retry(builder).await?;

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason = None;
        let mut saw_done = false;
        let mut partial_calls = BTreeMap::<usize, ToolCallAccumulator>::new();
        let mut tool_argument_bytes = 0_usize;
        stream_lines(response, |line| {
            let Some(data) = line.trim().strip_prefix("data:") else {
                return Ok(());
            };
            let data = data.trim();
            if data == "[DONE]" {
                saw_done = true;
                return Ok(());
            }
            if data.is_empty() {
                return Ok(());
            }
            let chunk: OpenAiChunk = serde_json::from_str(data)
                .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
            if let Some(error) = &chunk.error {
                return Err(crate::stream_error(error));
            }
            for choice in chunk.choices {
                if choice.index != 0 {
                    return Err(AdapterError::InvalidResponse(format!(
                        "the agent endpoint streamed choice {} although one was requested",
                        choice.index
                    )));
                }
                if choice.finish_reason.is_some() {
                    finish_reason = choice.finish_reason;
                }
                if let Some(delta) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
                    reasoning.push_str(&delta);
                }
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

        crate::require_dispatchable(
            saw_done || finish_reason.is_some(),
            finish_reason.as_deref(),
            partial_calls.len(),
        )?;
        let tool_calls = partial_calls
            .into_iter()
            .map(|(index, call)| {
                if call.name.is_empty() {
                    return Err(AdapterError::InvalidResponse(format!(
                        "tool call {index} has no function name"
                    )));
                }
                let arguments = if call.arguments.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&call.arguments).map_err(|error| {
                        AdapterError::InvalidToolArguments {
                            tool: call.name.clone(),
                            detail: error.to_string(),
                        }
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
            reasoning: Some(reasoning).filter(|text| !text.is_empty()),
            finish_reason,
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

    /// One message as the adapter would send it.
    fn encoded(message: &ChatMessage, images: &xshell_core::ImageBytes, vision: bool) -> Value {
        let messages = [message.clone()];
        let laid = lay_out(&messages, images, vision);
        serde_json::to_value(OpenAiMessage::from(&laid[0])).unwrap()
    }

    #[test]
    fn images_are_sent_as_data_url_parts() {
        let image = xshell_core::ImageRef::new(
            "ab".repeat(32),
            xshell_core::ImageMediaType::Png,
            8,
            8,
            3,
            "plot.png",
        )
        .unwrap();
        let bytes: xshell_core::ImageBytes = [(
            image.sha256().to_owned(),
            std::sync::Arc::new(b"png".to_vec()),
        )]
        .into();
        let message = ChatMessage::user("look").with_images([image]);
        let sent = encoded(&message, &bytes, true);
        assert_eq!(
            sent["content"],
            json!([
                {"type": "text", "text": "look\n[image: plot.png, 8x8]"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
            ])
        );
        // A model without vision gets text only.
        let blind = encoded(&message, &bytes, false);
        assert!(
            blind["content"]
                .as_str()
                .unwrap()
                .contains("not configured for images")
        );
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
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments["path"], "README.md");
        assert!(request.contains("\"stream\":true"));
    }

    const READ_CALL: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
        "\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]}}]}\n\n"
    );

    /// Stream `body` and return the adapter's result.
    async fn stream(body: String) -> Result<AssistantResponse, AdapterError> {
        let (base_url, server) = serve_once("text/event-stream", body);
        let mut adapter = OpenAiCompatibleAdapter::new(base_url, "test-model", None);
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
            Err(
                error @ (AdapterError::InvalidResponse(_)
                | AdapterError::StreamError(_)
                | AdapterError::InvalidToolArguments { .. }),
            ) => {
                let message = error.to_string();
                assert!(
                    message.contains(reason),
                    "expected {reason:?} in {message:?}"
                );
            }
            other => panic!("expected a refused response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn f9_tool_calls_need_a_completed_stream() {
        // EOF after tool deltas, with neither `[DONE]` nor a finish reason.
        assert_refused(
            stream(READ_CALL.to_owned()).await,
            "ended before the provider finished it",
        );
        // Either completion signal alone is enough.
        let done = stream(format!("{READ_CALL}data: [DONE]\n\n"))
            .await
            .unwrap();
        assert_eq!(done.tool_calls[0].name, "read_file");
        let finished = stream(format!(
            "{READ_CALL}data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n"
        ))
        .await
        .unwrap();
        assert_eq!(finished.tool_calls.len(), 1);
    }

    #[tokio::test]
    async fn f9_truncated_and_filtered_responses_dispatch_nothing() {
        for (reason, expected) in [
            ("length", "output token limit"),
            ("content_filter", "content filter"),
            ("unexpected", "reason \"unexpected\""),
        ] {
            assert_refused(
                stream(format!(
                    "{READ_CALL}data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}\n\ndata: [DONE]\n\n"
                ))
                .await,
                expected,
            );
        }
    }

    #[tokio::test]
    async fn f9_text_only_responses_are_not_held_to_the_tool_rule() {
        // No tool call, no effect: a stream without a completion marker still
        // yields its text.
        let response =
            stream("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".to_owned())
                .await
                .unwrap();
        assert_eq!(response.content, "partial");
        assert!(response.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn f9_mid_stream_errors_fail_the_response() {
        assert_refused(
            stream(format!(
                "{READ_CALL}data: {{\"error\":{{\"message\":\"upstream exploded\",\"code\":502}}}}\n\ndata: [DONE]\n\n"
            ))
            .await,
            "upstream exploded",
        );
        // A context overflow reported mid-stream keeps its own variant, so the
        // engine can still compact and retry.
        let overflow = stream(
            "data: {\"error\":{\"message\":\"This model's maximum context length is 32768 tokens. However, you requested 40000 tokens\",\"code\":\"context_length_exceeded\"}}\n\n".to_owned(),
        )
        .await;
        assert!(
            matches!(overflow, Err(AdapterError::ContextOverflow { .. })),
            "got {overflow:?}"
        );
    }

    #[tokio::test]
    async fn f9_extra_choices_and_unnamed_calls_are_rejected() {
        assert_refused(
            stream(
                "data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"other\"}}]}\n\ndata: [DONE]\n\n"
                    .to_owned(),
            )
            .await,
            "streamed choice 1",
        );
        assert_refused(
            stream(
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",",
                    "\"function\":{\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n"
                )
                .to_owned(),
            )
            .await,
            "has no function name",
        );
        // Partial arguments that do not parse were already refused.
        assert_refused(
            stream(concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",",
                "\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\ndata: [DONE]\n\n"
            ).to_owned())
            .await,
            "invalid arguments",
        );
    }

    #[tokio::test]
    async fn collects_reasoning_and_finish_reason_apart_from_content() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"more\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{}}\n\n",
            "data: {\"choices\":[],\"usage\":{\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, server) = serve_once("text/event-stream", body.into());
        let mut adapter = OpenAiCompatibleAdapter::new(base_url, "test-model", None);
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
        server.join().unwrap();

        assert_eq!(deltas, "answer");
        assert_eq!(response.content, "answer");
        assert_eq!(response.reasoning.as_deref(), Some("think more"));
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn replays_assistant_reasoning_as_reasoning_content() {
        let message =
            ChatMessage::assistant("done").with_reasoning(Some("checked the file".into()));
        let encoded = encoded(&message, &Default::default(), false);
        assert_eq!(encoded["reasoning_content"], "checked the file");

        let plain = super::tests::encoded(&ChatMessage::assistant("x"), &Default::default(), false);
        assert!(plain.get("reasoning_content").is_none());
    }

    #[test]
    fn serializes_openai_tool_results_by_call_id() {
        let call = ToolCall {
            id: "call_123".into(),
            name: "read_file".into(),
            arguments: json!({"path": "README.md"}),
        };
        let message = ChatMessage::tool_result(&call, "contents");
        let encoded = encoded(&message, &Default::default(), false);
        assert_eq!(encoded["role"], "tool");
        assert_eq!(encoded["tool_call_id"], "call_123");
        assert!(encoded.get("tool_name").is_none());
    }
}
