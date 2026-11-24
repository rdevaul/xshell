//! The Anthropic Messages API (`POST /v1/messages`), streamed.
//!
//! See `docs/anthropic-provider.md`. This is stage P1: request mapping,
//! streaming, `stop_reason` mapping, images, both credential headers, the
//! API version, effort and thinking settings, and retry with backoff.
//! Replaying signed thinking blocks (P2), usage (P3), credential commands
//! (P4), cache breakpoints (P5) and append-only history (P6) come later.
//!
//! Until P2, a profile that turns thinking on cannot run a tool loop: the API
//! needs the thinking blocks of the assistant message that called a tool
//! when its result comes back, and this adapter does not yet keep them.

use crate::images::{Laid, lay_out};
use crate::{AgentAdapter, stream_lines};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use xshell_core::{
    AdapterError, AgentDescriptor, AgentEvent, AnthropicSettings, AssistantResponse, ChatMessage,
    ChatRequest, MessageRole, ProviderBlocks, ToolCall,
};

/// The provider name stored with replayable blocks.
const PROVIDER: &str = "anthropic";
/// `anthropic-version` sent when the profile names none.
pub const DEFAULT_API_VERSION: &str = "2023-06-01";
/// `max_tokens` sent when the profile names none. Streaming makes large
/// values safe.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_000;
/// Text sent in place of a tool result with no output; the API refuses an
/// empty text block.
const EMPTY_TOOL_RESULT: &str = "(no output)";
/// Text of the result supplied for a `tool_use` whose result is missing from
/// the history, so the request still pairs every call with a result.
const INTERRUPTED_TOOL_RESULT: &str = "(this tool call was interrupted and produced no result)";
/// Longest tool id the API accepts.
const MAX_TOOL_ID_LEN: usize = 64;

/// Retries of a request the API answered with an overload, rate-limit or
/// server error status.
const MAX_STATUS_RETRIES: u32 = 6;
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(30);
const RATE_LIMIT_RETRY_CAP: Duration = Duration::from_secs(60);
/// Total time spent waiting between retries of one request.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(120);

/// Which header carries the credential.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AnthropicAuth {
    /// `x-api-key`, as the API itself expects.
    #[default]
    ApiKey,
    /// `Authorization: Bearer`, as most gateways issuing short-lived tokens
    /// expect.
    Bearer,
}

/// What the profile sets beyond the model and endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicOptions {
    pub auth: AnthropicAuth,
    pub api_version: String,
    pub max_output_tokens: u32,
    /// `output_config.effort`. Sent whenever set: the default differs by
    /// model, and comparisons depend on it.
    pub effort: Option<String>,
    /// `thinking: {type: "adaptive", display}` when set, with the display
    /// ("omitted", "summarized"); thinking is not requested when `None`.
    pub thinking_display: Option<String>,
    /// Extra `anthropic-beta` values.
    pub betas: Vec<String>,
    /// Whether to send `anthropic-beta` at all. Some gateways reject it.
    pub send_betas: bool,
}

/// Effort levels the API accepts.
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const THINKING_DISPLAYS: &[&str] = &["omitted", "summarized"];

impl AnthropicOptions {
    /// Options from a profile's settings, or why they are not valid.
    pub fn from_settings(settings: &AnthropicSettings) -> Result<Self, String> {
        let defaults = Self::default();
        let auth = match settings.auth.as_deref() {
            None | Some("x-api-key") => AnthropicAuth::ApiKey,
            Some("bearer") => AnthropicAuth::Bearer,
            Some(other) => {
                return Err(format!("auth {other:?} is not \"x-api-key\" or \"bearer\""));
            }
        };
        if let Some(effort) = settings.effort.as_deref()
            && !EFFORTS.contains(&effort)
        {
            return Err(format!(
                "effort {effort:?} is not one of {}",
                EFFORTS.join(", ")
            ));
        }
        let thinking_display = match settings.thinking.as_deref() {
            None | Some("omit") => {
                if settings.thinking_display.is_some() {
                    return Err("thinking_display needs thinking = \"adaptive\"".into());
                }
                None
            }
            Some("adaptive") => {
                let display = settings.thinking_display.as_deref().unwrap_or("omitted");
                if !THINKING_DISPLAYS.contains(&display) {
                    return Err(format!(
                        "thinking_display {display:?} is not one of {}",
                        THINKING_DISPLAYS.join(", ")
                    ));
                }
                Some(display.to_owned())
            }
            Some(other) => {
                return Err(format!(
                    "thinking {other:?} is not \"adaptive\" or \"omit\"; the current models \
reject \"disabled\" and budget_tokens"
                ));
            }
        };
        if settings.max_output_tokens == Some(0) {
            return Err("max_output_tokens must be positive".into());
        }
        Ok(Self {
            auth,
            api_version: settings.api_version.clone().unwrap_or(defaults.api_version),
            max_output_tokens: settings
                .max_output_tokens
                .unwrap_or(defaults.max_output_tokens),
            effort: settings.effort.clone(),
            thinking_display,
            betas: settings.betas.clone(),
            send_betas: settings.send_betas.unwrap_or(defaults.send_betas),
        })
    }
}

impl Default for AnthropicOptions {
    fn default() -> Self {
        Self {
            auth: AnthropicAuth::default(),
            api_version: DEFAULT_API_VERSION.to_owned(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            effort: None,
            thinking_display: None,
            betas: Vec::new(),
            send_betas: true,
        }
    }
}

pub struct AnthropicAdapter {
    client: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    vision: bool,
    options: AnthropicOptions,
}

impl AnthropicAdapter {
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
            options: AnthropicOptions::default(),
        }
    }

    /// Send images to the model as image blocks. Without this, images are
    /// replaced by text placeholders.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    pub fn with_options(mut self, options: AnthropicOptions) -> Self {
        self.options = options;
        self
    }

    fn endpoint(&self) -> String {
        match self.base_url.strip_suffix("/v1") {
            Some(root) => format!("{root}/v1/messages"),
            None => format!("{}/v1/messages", self.base_url),
        }
    }

    /// The request body for `request`.
    fn body(&self, request: &ChatRequest) -> Value {
        let laid = lay_out(&request.messages, &request.images, self.vision);
        let (system, messages) = wire_messages(&laid);
        let mut tools: Vec<_> = request.tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.options.max_output_tokens,
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = Value::Array(system);
        }
        if !tools.is_empty() {
            body["tools"] = tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                        "eager_input_streaming": true,
                    })
                })
                .collect();
        }
        if let Some(display) = &self.options.thinking_display {
            body["thinking"] = json!({"type": "adaptive", "display": display});
        }
        if let Some(effort) = &self.options.effort {
            body["output_config"] = json!({"effort": effort});
        }
        body
    }

    fn request(&self, body: &Value) -> Result<reqwest::RequestBuilder, AdapterError> {
        let mut builder = self
            .client
            .post(self.endpoint())
            .header("accept", "text/event-stream")
            .header("anthropic-version", &self.options.api_version)
            .json(body);
        if self.options.send_betas && !self.options.betas.is_empty() {
            builder = builder.header("anthropic-beta", self.options.betas.join(","));
        }
        if let Some(api_key) = &self.api_key {
            crate::check_credential_endpoint(&self.base_url)?;
            builder = match self.options.auth {
                AnthropicAuth::ApiKey => builder.header("x-api-key", api_key),
                AnthropicAuth::Bearer => builder.bearer_auth(api_key),
            };
        }
        Ok(builder)
    }
}

/// Split laid-out messages into the `system` blocks and the `messages`
/// array.
///
/// - The leading system messages become `system` text blocks.
/// - A system message later in the history (an xshell execution record)
///   becomes user text, since the API has no system role inside `messages`.
/// - Consecutive messages of one role are merged into one message. That
///   puts every tool result that answers an assistant message, any images a
///   tool produced, and text that follows the results into the single user
///   message the API requires after a `tool_use`.
/// - Every `tool_use` gets a result: one missing from the history is
///   supplied as an error saying the call was interrupted. A result with no
///   matching call is sent as text.
fn wire_messages(laid: &[Laid<'_>]) -> (Vec<Value>, Vec<Value>) {
    let mut system = Vec::new();
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut ids = ToolIds::default();
    // Calls of the latest assistant message still waiting for a result.
    let mut open_calls: Vec<String> = Vec::new();
    let mut leading = true;
    for message in laid {
        let role = message
            .source
            .map_or(MessageRole::User, |source| source.role.clone());
        if role == MessageRole::System && leading {
            push_text(&mut system, &message.content);
            continue;
        }
        leading = false;
        let (wire_role, blocks) = match (&role, message.source) {
            (MessageRole::Assistant, Some(source)) => {
                close_calls(&mut messages, &mut open_calls);
                if let Some(stored) = replayable(source) {
                    let blocks = stored
                        .iter()
                        .map(|block| {
                            let mut block = block.clone();
                            if block["type"] == "tool_use" {
                                let id = ids.wire(block["id"].as_str().unwrap_or_default());
                                open_calls.push(id.clone());
                                block["id"] = json!(id);
                            }
                            block
                        })
                        .collect();
                    messages.push(("assistant", blocks));
                    continue;
                }
                let mut blocks = Vec::new();
                push_text(&mut blocks, &message.content);
                for call in &source.tool_calls {
                    let id = ids.wire(&call.id);
                    open_calls.push(id.clone());
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": call.name,
                        "input": tool_input(call),
                    }));
                }
                ("assistant", blocks)
            }
            (MessageRole::Tool, Some(source)) => {
                let answered = source
                    .tool_call_id
                    .as_deref()
                    .map(|id| ids.wire(id))
                    .and_then(|id| {
                        let index = open_calls.iter().position(|open| *open == id)?;
                        Some(open_calls.remove(index))
                    });
                let text = if message.content.trim().is_empty() {
                    EMPTY_TOOL_RESULT
                } else {
                    &message.content
                };
                let block = match answered {
                    Some(id) => {
                        let content = if message.images.is_empty() {
                            json!(text)
                        } else {
                            let mut parts = vec![json!({"type": "text", "text": text})];
                            parts.extend(image_blocks(message));
                            Value::Array(parts)
                        };
                        json!({"type": "tool_result", "tool_use_id": id, "content": content})
                    }
                    None => json!({"type": "text", "text": format!(
                        "[result of tool {}]\n{text}",
                        source.tool_name.as_deref().unwrap_or("call")
                    )}),
                };
                ("user", vec![block])
            }
            _ => {
                let mut blocks = Vec::new();
                if role == MessageRole::System {
                    push_text(&mut blocks, &format!("[xshell] {}", message.content));
                } else {
                    push_text(&mut blocks, &message.content);
                }
                blocks.extend(image_blocks(message));
                ("user", blocks)
            }
        };
        // Anything but a result ends the results for the open calls. (An
        // assistant message closed them before its own calls opened.)
        if wire_role == "user" && !is_tool_result(&blocks) {
            close_calls(&mut messages, &mut open_calls);
        }
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((last, existing)) if *last == wire_role => {
                // Tool results must come first in a user message.
                if is_tool_result(&blocks) {
                    let at = existing
                        .iter()
                        .position(|block| block["type"] != "tool_result")
                        .unwrap_or(existing.len());
                    existing.splice(at..at, blocks);
                } else {
                    existing.extend(blocks);
                }
            }
            _ => messages.push((wire_role, blocks)),
        }
    }
    close_calls(&mut messages, &mut open_calls);
    let messages = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    (system, messages)
}

/// The stored blocks to send for an assistant message, when they came from
/// this provider. Blocks from another provider are never sent; the message's
/// text and tool calls are used instead. Blocks from another Anthropic model
/// are sent: the API drops thinking a model cannot read, unbilled.
fn replayable(message: &ChatMessage) -> Option<&Vec<Value>> {
    let stored = message.provider_blocks.as_ref()?;
    (stored.provider == PROVIDER).then_some(())?;
    stored.blocks.as_array().filter(|blocks| !blocks.is_empty())
}

/// Supply an interrupted-call result for every call still open, at the head
/// of the user message that follows the assistant message, adding that user
/// message when there is none.
fn close_calls(messages: &mut Vec<(&'static str, Vec<Value>)>, open_calls: &mut Vec<String>) {
    if open_calls.is_empty() {
        return;
    }
    let results: Vec<Value> = open_calls
        .drain(..)
        .map(|id| {
            json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": INTERRUPTED_TOOL_RESULT,
                "is_error": true,
            })
        })
        .collect();
    match messages.last_mut() {
        Some((role, existing)) if *role == "user" => {
            let at = existing
                .iter()
                .position(|block| block["type"] != "tool_result")
                .unwrap_or(existing.len());
            existing.splice(at..at, results);
        }
        _ => messages.push(("user", results)),
    }
}

fn is_tool_result(blocks: &[Value]) -> bool {
    blocks
        .first()
        .is_some_and(|block| block["type"] == "tool_result")
}

fn push_text(blocks: &mut Vec<Value>, text: &str) {
    if !text.trim().is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
}

fn image_blocks<'a>(message: &'a Laid<'a>) -> impl Iterator<Item = Value> + 'a {
    message.images.iter().map(|image| {
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": image.media_type, "data": image.base64},
        })
    })
}

/// A tool call's arguments as `tool_use.input`, which must be an object.
fn tool_input(call: &ToolCall) -> Value {
    match &call.arguments {
        Value::Object(_) => call.arguments.clone(),
        Value::Null => Value::Object(Map::new()),
        other => json!({ "value": other }),
    }
}

/// Tool ids as the API accepts them: `[A-Za-z0-9_-]{1,64}`. Ids from another
/// provider (a session that switched model) are rewritten, consistently for
/// a call and its result, and kept distinct from one another.
#[derive(Default)]
struct ToolIds {
    rewritten: BTreeMap<String, String>,
    used: BTreeSet<String>,
}

impl ToolIds {
    fn wire(&mut self, id: &str) -> String {
        if let Some(wire) = self.rewritten.get(id) {
            return wire.clone();
        }
        let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        let wire = if !id.is_empty()
            && id.len() <= MAX_TOOL_ID_LEN
            && id.chars().all(valid)
            && !self.used.contains(id)
        {
            id.to_owned()
        } else {
            let mut base: String = id
                .chars()
                .map(|c| if valid(c) { c } else { '_' })
                .take(MAX_TOOL_ID_LEN - 8)
                .collect();
            if base.is_empty() {
                base = "call".into();
            }
            (1..)
                .map(|n| format!("{base}_x{n}"))
                .find(|candidate| !self.used.contains(candidate))
                .expect("an unused id exists")
        };
        self.used.insert(wire.clone());
        self.rewritten.insert(id.to_owned(), wire.clone());
        wire
    }
}

/// One server-sent event's `data`.
#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    content_block: Option<Value>,
    #[serde(default)]
    delta: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    message: Option<Value>,
}

/// A content block being streamed.
enum Block {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        /// The signature, attached only once the block is complete.
        pending_signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// A block type this adapter does not know (a server tool's, say),
    /// kept as it started.
    Other(Value),
}

/// What a stream has delivered so far.
#[derive(Default)]
struct Collected {
    /// The model that answered, from `message_start`.
    model: Option<String>,
    content: String,
    reasoning: String,
    open: BTreeMap<usize, Block>,
    tool_calls: Vec<ToolCall>,
    tool_argument_bytes: usize,
    stop_reason: Option<String>,
    stopped: bool,
    /// Completed blocks by index, in the form they are replayed.
    blocks: BTreeMap<usize, Value>,
    /// Whether any block is thinking, which must be replayed exactly.
    thinking: bool,
}

impl Collected {
    fn handle(
        &mut self,
        event: StreamEvent,
        events: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<(), AdapterError> {
        match event.kind.as_str() {
            "content_block_start" => {
                let index = event_index(&event)?;
                let block = event.content_block.unwrap_or_default();
                let opened = match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or_default();
                        self.text(text, events);
                        Block::Text {
                            text: text.to_owned(),
                        }
                    }
                    Some("thinking") => {
                        let thinking = block["thinking"].as_str().unwrap_or_default();
                        self.reasoning.push_str(thinking);
                        Block::Thinking {
                            thinking: thinking.to_owned(),
                            pending_signature: String::new(),
                        }
                    }
                    Some("redacted_thinking") => Block::RedactedThinking {
                        data: block["data"].as_str().unwrap_or_default().to_owned(),
                    },
                    Some("tool_use") => {
                        if self.tool_calls.len() + self.open_tool_calls() >= crate::MAX_TOOL_CALLS {
                            return Err(AdapterError::InvalidResponse(format!(
                                "the agent endpoint returned more than {} tool calls",
                                crate::MAX_TOOL_CALLS
                            )));
                        }
                        let name = block["name"].as_str().unwrap_or_default().to_owned();
                        if name.is_empty() {
                            return Err(AdapterError::InvalidResponse(format!(
                                "tool call {index} has no function name"
                            )));
                        }
                        Block::ToolUse {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            name,
                            json: String::new(),
                        }
                    }
                    _ => Block::Other(block),
                };
                if self.open.insert(index, opened).is_some() {
                    return Err(AdapterError::InvalidResponse(format!(
                        "the agent endpoint opened content block {index} twice"
                    )));
                }
            }
            "content_block_delta" => {
                let index = event_index(&event)?;
                let delta = event.delta.unwrap_or_default();
                let Some(block) = self.open.get_mut(&index) else {
                    return Err(AdapterError::InvalidResponse(format!(
                        "the agent endpoint sent a delta for content block {index}, which is not open"
                    )));
                };
                match (delta["type"].as_str(), block) {
                    (Some("text_delta"), Block::Text { text }) => {
                        let part = delta["text"].as_str().unwrap_or_default();
                        text.push_str(part);
                        let part = part.to_owned();
                        self.text(&part, events);
                    }
                    (Some("thinking_delta"), Block::Thinking { thinking, .. }) => {
                        let part = delta["thinking"].as_str().unwrap_or_default();
                        thinking.push_str(part);
                        self.reasoning.push_str(part);
                    }
                    (
                        Some("signature_delta"),
                        Block::Thinking {
                            pending_signature, ..
                        },
                    ) => {
                        pending_signature.push_str(delta["signature"].as_str().unwrap_or_default());
                    }
                    (Some("input_json_delta"), Block::ToolUse { json, .. }) => {
                        let part = delta["partial_json"].as_str().unwrap_or_default();
                        self.tool_argument_bytes = self
                            .tool_argument_bytes
                            .checked_add(part.len())
                            .filter(|size| *size <= crate::MAX_TOOL_ARGUMENT_BYTES)
                            .ok_or_else(|| {
                                AdapterError::InvalidResponse(format!(
                                    "streamed tool arguments exceed {} bytes",
                                    crate::MAX_TOOL_ARGUMENT_BYTES
                                ))
                            })?;
                        json.push_str(part);
                    }
                    // Deltas for blocks this adapter does not interpret, and
                    // delta types added later, are ignored.
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event_index(&event)?;
                let Some(block) = self.open.remove(&index) else {
                    return Err(AdapterError::InvalidResponse(format!(
                        "the agent endpoint closed content block {index}, which is not open"
                    )));
                };
                let replayed = match block {
                    Block::ToolUse { id, name, json } => {
                        let arguments = if json.trim().is_empty() {
                            Value::Object(Map::new())
                        } else {
                            serde_json::from_str(&json).map_err(|error| {
                                AdapterError::InvalidToolArguments {
                                    tool: name.clone(),
                                    detail: error.to_string(),
                                }
                            })?
                        };
                        if !arguments.is_object() {
                            return Err(AdapterError::InvalidToolArguments {
                                tool: name,
                                detail: "the arguments are not a JSON object".into(),
                            });
                        }
                        let id = if id.is_empty() {
                            format!("anthropic-call-{index}")
                        } else {
                            id
                        };
                        let replayed = json!({
                            "type": "tool_use", "id": id, "name": name, "input": arguments,
                        });
                        self.tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments,
                        });
                        replayed
                    }
                    Block::Thinking {
                        thinking,
                        pending_signature,
                    } => {
                        // Replay needs the signature; without one the block
                        // cannot be sent back, and the turn cannot continue.
                        if pending_signature.is_empty() {
                            return Err(AdapterError::InvalidResponse(format!(
                                "thinking block {index} ended without a signature"
                            )));
                        }
                        self.thinking = true;
                        json!({"type": "thinking", "thinking": thinking, "signature": pending_signature})
                    }
                    Block::RedactedThinking { data } => {
                        self.thinking = true;
                        json!({"type": "redacted_thinking", "data": data})
                    }
                    Block::Text { text } => json!({"type": "text", "text": text}),
                    Block::Other(block) => block,
                };
                self.blocks.insert(index, replayed);
            }
            "message_delta" => {
                let delta = event.delta.unwrap_or_default();
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
            }
            "message_start" => {
                self.model = event
                    .message
                    .as_ref()
                    .and_then(|message| message["model"].as_str())
                    .map(str::to_owned);
            }
            "message_stop" => self.stopped = true,
            "error" => {
                return Err(crate::stream_error(
                    &event.error.unwrap_or_else(|| json!("unknown error")),
                ));
            }
            // `ping`, and event types added later.
            _ => {}
        }
        Ok(())
    }

    fn text(&mut self, text: &str, events: &mut (dyn FnMut(AgentEvent) + Send)) {
        if !text.is_empty() {
            self.content.push_str(text);
            events(AgentEvent::TextDelta(text.to_owned()));
        }
    }

    fn open_tool_calls(&self) -> usize {
        self.open
            .values()
            .filter(|block| matches!(block, Block::ToolUse { .. }))
            .count()
    }

    /// The response, once the stream has ended.
    fn finish(self, requested_model: &str) -> Result<AssistantResponse, AdapterError> {
        let calls = self.tool_calls.len() + self.open_tool_calls();
        if self.stop_reason.as_deref() == Some("max_tokens") && self.open_tool_calls() > 0 {
            return Err(AdapterError::InvalidResponse(format!(
                "the response was cut off by the output token limit in the middle of a tool \
call; raise max_output_tokens. Its {calls} tool call(s) were not run"
            )));
        }
        if !self.stopped {
            return Err(AdapterError::InvalidResponse(format!(
                "the response stream ended before message_stop{}",
                if calls > 0 {
                    format!("; its {calls} tool call(s) were not run")
                } else {
                    String::new()
                }
            )));
        }
        let finish_reason = match self.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence" | "compaction") | None => "stop",
            Some("tool_use") => "tool_calls",
            Some("max_tokens" | "model_context_window_exceeded") => "length",
            Some("pause_turn") => "pause",
            Some("refusal") => {
                return Err(AdapterError::InvalidResponse(
                    "the model declined to continue this request (stop reason \"refusal\")".into(),
                ));
            }
            Some(other) => {
                return Err(AdapterError::InvalidResponse(format!(
                    "the response finished with unknown stop reason {other:?}"
                )));
            }
        };
        crate::require_dispatchable(true, Some(finish_reason), self.tool_calls.len())?;
        // Kept only when thinking needs exact replay; otherwise the text and
        // tool calls rebuild the same message.
        let provider_blocks = self.thinking.then(|| ProviderBlocks {
            provider: PROVIDER.to_owned(),
            model: self.model.unwrap_or_else(|| requested_model.to_owned()),
            blocks: self
                .blocks
                .into_values()
                .filter(|block| block["type"] != "text" || block["text"] != "")
                .collect(),
        });
        Ok(AssistantResponse {
            content: self.content,
            tool_calls: self.tool_calls,
            reasoning: Some(self.reasoning).filter(|text| !text.is_empty()),
            finish_reason: Some(finish_reason.to_owned()),
            provider_blocks,
        })
    }
}

fn event_index(event: &StreamEvent) -> Result<usize, AdapterError> {
    event.index.ok_or_else(|| {
        AdapterError::InvalidResponse(format!("{} event without an index", event.kind))
    })
}

/// Whether a status is worth retrying: rate limits, overload and server
/// errors.
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

/// The wait before retry `attempt` (0-based): exponential from one second
/// with jitter, capped, and raised to what `retry-after` asks for, up to the
/// cap.
fn retry_wait(attempt: u32, status: u16, headers: &reqwest::header::HeaderMap) -> Duration {
    let cap = if status == 429 {
        RATE_LIMIT_RETRY_CAP
    } else {
        RETRY_CAP
    };
    let backoff = RETRY_BASE.saturating_mul(2_u32.saturating_pow(attempt));
    let wait = backoff.mul_f64(jitter()).min(cap);
    let asked = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok()?.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()?.trim().parse::<f64>().ok())
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .map(Duration::from_secs_f64)
        });
    asked.map_or(wait, |asked| wait.max(asked.min(cap)))
}

/// A factor in 0.5–1.5, so that clients rejected together do not retry
/// together.
fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    0.5 + f64::from(nanos % 1_000_000) / 1_000_000.0
}

#[async_trait]
impl AgentAdapter for AnthropicAdapter {
    fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor {
            id: "anthropic".into(),
            display_name: "Anthropic Messages API".into(),
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
        let body = self.body(&request);
        let mut attempt = 0;
        let mut waited = Duration::ZERO;
        let response = loop {
            let response = crate::send_with_retry(self.request(&body)?).await?;
            let status = response.status().as_u16();
            if !retryable_status(status) || attempt >= MAX_STATUS_RETRIES {
                break response;
            }
            let wait = retry_wait(attempt, status, response.headers());
            if waited + wait > MAX_RETRY_WAIT {
                break response;
            }
            drop(response);
            tokio::time::sleep(wait).await;
            waited += wait;
            attempt += 1;
        };

        let mut collected = Collected::default();
        let streamed = stream_lines(response, |line| {
            let Some(data) = line.trim().strip_prefix("data:") else {
                // `event:` lines repeat the type the data carries.
                return Ok(());
            };
            let data = data.trim();
            if data.is_empty() {
                return Ok(());
            }
            let event: StreamEvent = serde_json::from_str(data)
                .map_err(|error| AdapterError::InvalidResponse(error.to_string()))?;
            collected.handle(event, events)
        })
        .await;
        match streamed {
            // The API's answer to a request body over its size limit.
            Err(AdapterError::Http { status: 413, .. }) => Err(AdapterError::ContextOverflow {
                prompt_tokens: None,
                available_tokens: None,
            }),
            Err(error) => Err(error),
            Ok(()) => collected.finish(&self.model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_once;
    use xshell_core::ToolDefinition;

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn start() -> Value {
        json!({"type": "message_start", "message": {"id": "msg_1", "model": "m", "usage": {"input_tokens": 10, "output_tokens": 1}}})
    }

    fn stop(reason: &str) -> [Value; 2] {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ]
    }

    fn text_block(index: usize, text: &str) -> [Value; 3] {
        [
            json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": index}),
        ]
    }

    fn tool_block(index: usize, id: &str, name: &str, parts: &[&str]) -> Vec<Value> {
        let mut events = vec![json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}})];
        events.extend(parts.iter().map(|part| {
            json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": part}})
        }));
        events.push(json!({"type": "content_block_stop", "index": index}));
        events
    }

    async fn stream_with(
        body: String,
        adapter: impl FnOnce(String) -> AnthropicAdapter,
    ) -> (Result<AssistantResponse, AdapterError>, String) {
        let (base_url, server) = serve_once("text/event-stream", body);
        let mut adapter = adapter(base_url);
        let result = adapter
            .chat_stream(
                ChatRequest::new(vec![ChatMessage::user("hello")], Vec::new()),
                &mut |_| {},
            )
            .await;
        (result, server.join().unwrap())
    }

    async fn stream(body: String) -> Result<AssistantResponse, AdapterError> {
        stream_with(body, |url| AnthropicAdapter::new(url, "m", None))
            .await
            .0
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

    #[test]
    fn settings_are_validated() {
        let ok = AnthropicOptions::from_settings(&AnthropicSettings {
            auth: Some("bearer".into()),
            effort: Some("high".into()),
            thinking: Some("adaptive".into()),
            ..AnthropicSettings::default()
        })
        .unwrap();
        assert_eq!(ok.auth, AnthropicAuth::Bearer);
        assert_eq!(ok.thinking_display.as_deref(), Some("omitted"));
        assert_eq!(ok.api_version, DEFAULT_API_VERSION);
        for (settings, reason) in [
            (
                AnthropicSettings {
                    thinking: Some("disabled".into()),
                    ..Default::default()
                },
                "budget_tokens",
            ),
            (
                AnthropicSettings {
                    effort: Some("extreme".into()),
                    ..Default::default()
                },
                "effort",
            ),
            (
                AnthropicSettings {
                    auth: Some("basic".into()),
                    ..Default::default()
                },
                "auth",
            ),
            (
                AnthropicSettings {
                    thinking_display: Some("summarized".into()),
                    ..Default::default()
                },
                "needs thinking",
            ),
            (
                AnthropicSettings {
                    max_output_tokens: Some(0),
                    ..Default::default()
                },
                "positive",
            ),
        ] {
            let error = AnthropicOptions::from_settings(&settings).unwrap_err();
            assert!(error.contains(reason), "{error}");
        }
    }

    #[test]
    fn endpoint_accepts_an_optional_v1_suffix() {
        let without = AnthropicAdapter::new("https://api.anthropic.com", "m", None);
        let with = AnthropicAdapter::new("https://gw.example/anthropic/v1/", "m", None);
        assert_eq!(without.endpoint(), "https://api.anthropic.com/v1/messages");
        assert_eq!(with.endpoint(), "https://gw.example/anthropic/v1/messages");
    }

    #[tokio::test]
    async fn streams_text_and_maps_end_turn_to_stop() {
        let mut events = vec![start()];
        events.extend(text_block(0, "hello "));
        events.push(json!({"type": "ping"}));
        events.extend(text_block(1, "world"));
        events.extend(stop("end_turn"));
        let (base_url, server) = serve_once("text/event-stream", sse(&events));
        let mut adapter = AnthropicAdapter::new(base_url, "m", None);
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
        assert_eq!(deltas, "hello world");
        assert_eq!(response.content, "hello world");
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn reassembles_parallel_tool_calls() {
        let mut events = vec![start()];
        events.extend(text_block(0, "Reading both."));
        events.extend(tool_block(
            1,
            "toolu_a",
            "read_file",
            &["{\"pa", "th\":\"a\"}"],
        ));
        events.extend(tool_block(2, "toolu_b", "read_file", &["{\"path\":\"b\"}"]));
        events.extend(stop("tool_use"));
        let response = stream(sse(&events)).await.unwrap();
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "toolu_a");
        assert_eq!(response.tool_calls[0].arguments, json!({"path": "a"}));
        assert_eq!(response.tool_calls[1].arguments, json!({"path": "b"}));
    }

    #[tokio::test]
    async fn a_call_with_no_input_has_empty_arguments() {
        let mut events = vec![start()];
        events.extend(tool_block(0, "toolu_a", "list", &[]));
        events.extend(stop("tool_use"));
        let response = stream(sse(&events)).await.unwrap();
        assert_eq!(response.tool_calls[0].arguments, json!({}));
    }

    #[tokio::test]
    async fn collects_thinking_text_as_reasoning() {
        let events = [
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "consider "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "it"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "c2ln"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "b3BhcXVl"}}),
            json!({"type": "content_block_stop", "index": 1}),
        ];
        let mut events = events.to_vec();
        events.extend(text_block(2, "answer"));
        events.extend(stop("end_turn"));
        let response = stream(sse(&events)).await.unwrap();
        assert_eq!(response.reasoning.as_deref(), Some("consider it"));
        assert_eq!(response.content, "answer");
    }

    fn thinking_block(index: usize, thinking: &str, signature: &str) -> Vec<Value> {
        let mut events = vec![json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "thinking", "thinking": "", "signature": ""}})];
        if !thinking.is_empty() {
            events.push(json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "thinking_delta", "thinking": thinking}}));
        }
        if !signature.is_empty() {
            events.push(json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "signature_delta", "signature": signature}}));
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
        events
    }

    #[tokio::test]
    async fn keeps_signed_thinking_and_tool_calls_for_replay() {
        let mut events = vec![json!({"type": "message_start",
            "message": {"id": "msg_1", "model": "claude-answered", "usage": {}}})];
        // Opus 5.5 omits the thinking text by default; the signature remains.
        events.extend(thinking_block(0, "", "c2lnLTE="));
        events.extend(tool_block(1, "toolu_a", "read_file", &["{\"path\":\"a\"}"]));
        events.extend(stop("tool_use"));
        let response = stream(sse(&events)).await.unwrap();
        let stored = response.provider_blocks.expect("thinking is kept");
        assert_eq!(stored.provider, "anthropic");
        assert_eq!(stored.model, "claude-answered", "the model that answered");
        assert_eq!(
            stored.blocks,
            json!([
                {"type": "thinking", "thinking": "", "signature": "c2lnLTE="},
                {"type": "tool_use", "id": "toolu_a", "name": "read_file", "input": {"path": "a"}},
            ])
        );
        assert!(
            response.reasoning.is_none(),
            "no readable reasoning was shown"
        );
    }

    #[tokio::test]
    async fn responses_without_thinking_store_no_blocks() {
        let mut events = vec![start()];
        events.extend(text_block(0, "plain"));
        events.extend(stop("end_turn"));
        assert!(
            stream(sse(&events))
                .await
                .unwrap()
                .provider_blocks
                .is_none()
        );
    }

    #[tokio::test]
    async fn redacted_thinking_is_kept_verbatim() {
        let mut events = vec![
            start(),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "redacted_thinking", "data": "b3BhcXVl"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(text_block(1, "done"));
        events.extend(stop("end_turn"));
        let stored = stream(sse(&events)).await.unwrap().provider_blocks.unwrap();
        assert_eq!(
            stored.blocks,
            json!([
                {"type": "redacted_thinking", "data": "b3BhcXVl"},
                {"type": "text", "text": "done"},
            ])
        );
    }

    #[tokio::test]
    async fn a_thinking_block_without_a_signature_is_refused() {
        let mut events = vec![start()];
        events.extend(thinking_block(0, "partial thought", ""));
        events.extend(text_block(1, "x"));
        events.extend(stop("end_turn"));
        assert_refused(stream(sse(&events)).await, "without a signature");
    }

    #[test]
    fn stored_blocks_are_replayed_exactly() {
        let a = call("toolu_a", "a");
        let blocks = json!([
            {"type": "thinking", "thinking": "", "signature": "c2lnLTE="},
            {"type": "text", "text": "Reading."},
            {"type": "tool_use", "id": "toolu_a", "name": "read_file", "input": {"path": "a"}},
        ]);
        let assistant = ChatMessage::assistant_with_tools("Reading.", vec![a.clone()])
            .with_reasoning(Some("a summary that is never sent".into()))
            .with_provider_blocks(Some(ProviderBlocks {
                provider: "anthropic".into(),
                model: "m".into(),
                blocks: blocks.clone(),
            }));
        let body = body_for(
            vec![
                ChatMessage::user("go"),
                assistant.clone(),
                ChatMessage::tool_result(&a, "A"),
            ],
            Vec::new(),
        );
        assert_eq!(body["messages"][1]["content"], blocks);
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_a");
        assert!(!body.to_string().contains("never sent"));

        // Blocks another provider wrote are not sent; the message is rebuilt.
        let foreign = assistant.with_provider_blocks(Some(ProviderBlocks {
            provider: "openai".into(),
            model: "m".into(),
            blocks,
        }));
        let body = body_for(vec![ChatMessage::user("go"), foreign], Vec::new());
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({"type": "text", "text": "Reading."})
        );
    }

    #[tokio::test]
    async fn a_stream_without_message_stop_is_refused() {
        let mut events = vec![start()];
        events.extend(tool_block(0, "toolu_a", "read_file", &["{\"path\":\"a\"}"]));
        events.push(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}));
        assert_refused(
            stream(sse(&events)).await,
            "ended before message_stop; its 1 tool call(s) were not run",
        );
        // Text alone is refused too: the text may be cut short.
        let mut events = vec![start()];
        events.extend(text_block(0, "partial"));
        assert_refused(stream(sse(&events)).await, "ended before message_stop");
    }

    #[tokio::test]
    async fn max_tokens_with_an_open_tool_call_names_the_limit() {
        let mut events = vec![start()];
        events.extend(tool_block(0, "toolu_a", "write", &["{\"text\":\"abc"]));
        events.pop(); // the block never closes
        events.extend(stop("max_tokens"));
        assert_refused(stream(sse(&events)).await, "raise max_output_tokens");
    }

    #[tokio::test]
    async fn truncated_tool_calls_are_not_dispatched_after_max_tokens() {
        let mut events = vec![start()];
        events.extend(tool_block(0, "toolu_a", "read_file", &["{\"path\":\"a\"}"]));
        events.extend(stop("max_tokens"));
        assert_refused(stream(sse(&events)).await, "output token limit");
    }

    #[tokio::test]
    async fn invalid_eager_tool_json_is_refused() {
        let mut events = vec![start()];
        events.extend(tool_block(0, "toolu_a", "read_file", &["{\"path\":"]));
        events.extend(stop("tool_use"));
        assert_refused(
            stream(sse(&events)).await,
            "invalid arguments for tool read_file",
        );
        let mut events = vec![start()];
        events.extend(tool_block(0, "toolu_a", "read_file", &["[1]"]));
        events.extend(stop("tool_use"));
        assert_refused(stream(sse(&events)).await, "not a JSON object");
    }

    #[tokio::test]
    async fn refusal_and_unknown_stop_reasons_fail_the_response() {
        let mut events = vec![start()];
        events.extend(stop("refusal"));
        assert_refused(stream(sse(&events)).await, "declined");
        let mut events = vec![start()];
        events.extend(stop("something_new"));
        assert_refused(stream(sse(&events)).await, "unknown stop reason");
    }

    #[tokio::test]
    async fn stop_reasons_map_to_finish_reasons() {
        for (reason, expected) in [
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("pause_turn", "pause"),
        ] {
            let mut events = vec![start()];
            events.extend(text_block(0, "x"));
            events.extend(stop(reason));
            let response = stream(sse(&events)).await.unwrap();
            assert_eq!(
                response.finish_reason.as_deref(),
                Some(expected),
                "{reason}"
            );
        }
    }

    #[tokio::test]
    async fn mid_stream_errors_fail_the_response() {
        let mut events = vec![start()];
        events.extend(text_block(0, "x"));
        events.push(json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}));
        assert_refused(stream(sse(&events)).await, "Overloaded");
    }

    #[tokio::test]
    async fn sends_headers_and_settings() {
        let mut events = vec![start()];
        events.extend(text_block(0, "x"));
        events.extend(stop("end_turn"));
        let (_, request) = stream_with(sse(&events), |url| {
            AnthropicAdapter::new(url, "claude-test", Some("secret".into())).with_options(
                AnthropicOptions {
                    auth: AnthropicAuth::Bearer,
                    effort: Some("high".into()),
                    thinking_display: Some("summarized".into()),
                    max_output_tokens: 64_000,
                    betas: vec!["beta-a".into(), "beta-b".into()],
                    ..AnthropicOptions::default()
                },
            )
        })
        .await;
        let body: Value = serde_json::from_str(&request).unwrap();
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["stream"], true);
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        // No sampling parameter is ever sent: the 5.x models reject them.
        for field in ["temperature", "top_p", "top_k", "tool_choice"] {
            assert!(body.get(field).is_none(), "{field} was sent");
        }
    }

    /// The JSON body sent for `messages` and `tools`.
    fn body_for(messages: Vec<ChatMessage>, tools: Vec<ToolDefinition>) -> Value {
        AnthropicAdapter::new("https://api.anthropic.com", "m", None)
            .body(&ChatRequest::new(messages, tools))
    }

    fn call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": path}),
        }
    }

    #[test]
    fn maps_system_tools_and_grouped_results() {
        let (a, b) = (call("toolu_a", "a"), call("toolu_b", "b"));
        let body = body_for(
            vec![
                ChatMessage::system("You are xshell."),
                ChatMessage::user("read a and b"),
                ChatMessage::assistant_with_tools("Reading.", vec![a.clone(), b.clone()]),
                ChatMessage::tool_result(&a, "A"),
                ChatMessage::tool_result(&b, ""),
                ChatMessage::user("now summarise"),
            ],
            vec![
                ToolDefinition {
                    name: "write".into(),
                    description: "w".into(),
                    parameters: json!({"type": "object"}),
                },
                ToolDefinition {
                    name: "read_file".into(),
                    description: "r".into(),
                    parameters: json!({"type": "object"}),
                },
            ],
        );
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "You are xshell."}])
        );
        assert_eq!(body["tools"][0]["name"], "read_file", "tools are sorted");
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert_eq!(body["tools"][1]["input_schema"], json!({"type": "object"}));
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "read a and b"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading."},
                    {"type": "tool_use", "id": "toolu_a", "name": "read_file", "input": {"path": "a"}},
                    {"type": "tool_use", "id": "toolu_b", "name": "read_file", "input": {"path": "b"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_a", "content": "A"},
                    {"type": "tool_result", "tool_use_id": "toolu_b", "content": "(no output)"},
                    {"type": "text", "text": "now summarise"},
                ]},
            ])
        );
    }

    #[test]
    fn repairs_unanswered_calls_and_orphan_results() {
        let (a, b) = (call("toolu_a", "a"), call("toolu_b", "b"));
        let orphan = call("toolu_gone", "c");
        let body = body_for(
            vec![
                ChatMessage::user("go"),
                ChatMessage::assistant_with_tools("", vec![a.clone(), b]),
                ChatMessage::tool_result(&a, "A"),
                ChatMessage::system("xshell execution record: the turn was interrupted"),
                ChatMessage::tool_result(&orphan, "late"),
            ],
            Vec::new(),
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1]["content"].as_array().unwrap().len(),
            2,
            "no empty text block"
        );
        let user = messages[2]["content"].as_array().unwrap();
        assert_eq!(user[0]["tool_use_id"], "toolu_a");
        assert_eq!(user[1]["tool_use_id"], "toolu_b");
        assert_eq!(user[1]["is_error"], true);
        assert_eq!(
            user[2],
            json!({"type": "text", "text": "[xshell] xshell execution record: the turn was interrupted"})
        );
        assert_eq!(user[3]["text"], "[result of tool read_file]\nlate");
    }

    #[test]
    fn a_trailing_unanswered_call_gets_a_result() {
        let a = call("toolu_a", "a");
        let body = body_for(
            vec![
                ChatMessage::user("go"),
                ChatMessage::assistant_with_tools("", vec![a]),
            ],
            Vec::new(),
        );
        assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
    }

    #[test]
    fn foreign_tool_ids_are_rewritten_consistently() {
        let a = call("call:1/x", "a");
        let b = call("call_1_x", "b");
        let body = body_for(
            vec![
                ChatMessage::user("go"),
                ChatMessage::assistant_with_tools("", vec![a.clone(), b.clone()]),
                ChatMessage::tool_result(&a, "A"),
                ChatMessage::tool_result(&b, "B"),
            ],
            Vec::new(),
        );
        let calls = &body["messages"][1]["content"];
        let results = &body["messages"][2]["content"];
        let (id_a, id_b) = (
            calls[0]["id"].as_str().unwrap(),
            calls[1]["id"].as_str().unwrap(),
        );
        assert_ne!(id_a, id_b);
        assert_eq!(id_b, "call_1_x", "a valid id is kept");
        assert!(
            id_a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert_eq!(results[0]["tool_use_id"], id_a);
        assert_eq!(results[1]["tool_use_id"], id_b);
        let long = ToolIds::default().wire(&"x".repeat(100));
        assert!(long.len() <= MAX_TOOL_ID_LEN);
    }

    #[test]
    fn images_are_sent_as_base64_blocks() {
        let (image, bytes) = crate::images::tests::image(7, "plot.png");
        let images: xshell_core::ImageBytes =
            [(image.sha256().to_owned(), std::sync::Arc::new(bytes))].into();
        let mut request = ChatRequest::new(
            vec![ChatMessage::user("look").with_images([image])],
            Vec::new(),
        );
        request.images = images;
        let body = AnthropicAdapter::new("https://api.anthropic.com", "m", None)
            .with_vision(true)
            .body(&request);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert!(
            content[1]["source"]["data"]
                .as_str()
                .is_some_and(|d| !d.is_empty())
        );
    }

    #[test]
    fn identical_histories_give_identical_bodies() {
        let history = || {
            vec![
                ChatMessage::system("s"),
                ChatMessage::user("u"),
                ChatMessage::assistant("a"),
            ]
        };
        assert_eq!(
            serde_json::to_string(&body_for(history(), Vec::new())).unwrap(),
            serde_json::to_string(&body_for(history(), Vec::new())).unwrap()
        );
    }

    #[test]
    fn retry_waits_grow_honour_retry_after_and_stay_capped() {
        let none = reqwest::header::HeaderMap::new();
        let first = retry_wait(0, 529, &none);
        assert!(first >= Duration::from_millis(500) && first <= Duration::from_millis(1500));
        assert!(retry_wait(10, 529, &none) <= RETRY_CAP);
        assert!(retry_wait(10, 429, &none) <= RATE_LIMIT_RETRY_CAP);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "20".parse().unwrap());
        assert!(retry_wait(0, 429, &headers) >= Duration::from_secs(20));
        headers.insert("retry-after", "600".parse().unwrap());
        assert_eq!(retry_wait(0, 429, &headers), RATE_LIMIT_RETRY_CAP);
        assert!(retryable_status(529) && retryable_status(429) && !retryable_status(400));
    }
}
