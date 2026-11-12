use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful engineering assistant. \
Be explicit about uncertainty. Do not take irreversible, destructive, or security-sensitive \
actions without explaining the intended action and obtaining confirmation.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRoute {
    Agent(String),
    Shell(String),
    StickyShell(String),
    Control(ControlCommand),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    Help,
    History,
    Status,
    Audit(Vec<String>),
    Tools,
    Model(Vec<String>),
    Limits(Vec<String>),
    Agent(Vec<String>),
    View(String),
    /// Attach an image file to the next agent message: the path, or empty to
    /// list or clear pending attachments.
    Attach(String),
    Connect(Vec<String>),
    Sessions,
    Resume(Vec<String>),
    Stop(Vec<String>),
    New(Vec<String>),
    Switch(Vec<String>),
    Detach,
    Close(Vec<String>),
    Quit,
    Unknown {
        name: String,
        args: Vec<String>,
    },
}

pub fn classify_input(input: &str) -> InputRoute {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return InputRoute::Empty;
    }

    if let Some(control) = trimmed.strip_prefix("//") {
        return InputRoute::Control(parse_control(control));
    }

    if let Some(shell) = trimmed.strip_prefix("$$") {
        return InputRoute::StickyShell(shell.trim_start().to_owned());
    }

    if let Some(shell) = trimmed.strip_prefix('$') {
        return InputRoute::Shell(shell.trim_start().to_owned());
    }

    InputRoute::Agent(trimmed.to_owned())
}

fn parse_control(input: &str) -> ControlCommand {
    let mut words = input.splitn(2, char::is_whitespace);
    let Some(name) = words.next() else {
        return ControlCommand::Help;
    };
    let rest = words.next().unwrap_or("").trim();
    if name == "view" {
        return ControlCommand::View(rest.to_owned());
    }
    if name == "attach" {
        return ControlCommand::Attach(rest.to_owned());
    }
    let args: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();

    match name {
        "help" => ControlCommand::Help,
        "history" => ControlCommand::History,
        "status" => ControlCommand::Status,
        "audit" => ControlCommand::Audit(args),
        "tools" => ControlCommand::Tools,
        "model" => ControlCommand::Model(args),
        "limits" => ControlCommand::Limits(args),
        "agent" => ControlCommand::Agent(args),
        "connect" => ControlCommand::Connect(args),
        "sessions" => ControlCommand::Sessions,
        "resume" => ControlCommand::Resume(args),
        "stop" => ControlCommand::Stop(args),
        "new" => ControlCommand::New(args),
        "switch" => ControlCommand::Switch(args),
        "detach" => ControlCommand::Detach,
        "close" => ControlCommand::Close(args),
        "quit" | "exit" => ControlCommand::Quit,
        _ => ControlCommand::Unknown {
            name: name.to_owned(),
            args,
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl fmt::Display for MessageRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::User => f.write_str("user"),
            Self::Assistant => f.write_str("assistant"),
            Self::Tool => f.write_str("tool"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Reasoning ("thinking") the model produced alongside an assistant
    /// message. Replayed to providers whose chat templates expect prior
    /// reasoning in context; absent for other roles and in sessions written
    /// before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Images that belong to this message, in order. Each is a reference into
    /// the session's content-addressed image store; the bytes never travel
    /// with the message. Absent in sessions written before images existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            reasoning: None,
            images: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            reasoning: None,
            images: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            reasoning: None,
            images: Vec::new(),
        }
    }

    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            tool_name: None,
            reasoning: None,
            images: Vec::new(),
        }
    }

    /// Attach the model's reasoning to an assistant message; empty reasoning
    /// is stored as `None`.
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning.filter(|text| !text.trim().is_empty());
        self
    }

    /// Attach images to the message, after any it already has.
    pub fn with_images(mut self, images: impl IntoIterator<Item = ImageRef>) -> Self {
        self.images.extend(images);
        self
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
            reasoning: None,
            images: Vec::new(),
        }
    }
}

/// The encodings an image may have once ingested. Ingestion re-encodes every
/// image, so nothing else ever reaches a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageMediaType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
}

impl ImageMediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

impl fmt::Display for ImageMediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an [`ImageRef`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImageRefError {
    #[error("image hash must be 64 lowercase hexadecimal characters")]
    InvalidHash,
    #[error("image dimensions must be non-zero")]
    EmptyDimensions,
    #[error("image byte length must be non-zero")]
    EmptyBytes,
    #[error("image label must not contain control characters")]
    InvalidLabel,
}

/// A reference to an ingested image: enough to find its bytes in the
/// session's image store, charge it against the context, and describe it in
/// audit records and placeholders. It never holds the bytes.
///
/// Fields are private so every value is validated, whether constructed or
/// deserialized from a session file, a transcript, or the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "ImageRefFields", into = "ImageRefFields")]
pub struct ImageRef {
    sha256: String,
    media_type: ImageMediaType,
    width: u32,
    height: u32,
    byte_len: u64,
    label: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageRefFields {
    sha256: String,
    media_type: ImageMediaType,
    width: u32,
    height: u32,
    byte_len: u64,
    label: String,
}

impl ImageRef {
    /// Longest label kept, in characters; longer labels are truncated.
    pub const MAX_LABEL_CHARS: usize = 256;

    /// Validate and build a reference. `sha256` is the hash of the stored
    /// (re-encoded) bytes, `byte_len` their length, and `label` a short
    /// description such as the source path or the tool that produced it.
    pub fn new(
        sha256: impl Into<String>,
        media_type: ImageMediaType,
        width: u32,
        height: u32,
        byte_len: u64,
        label: impl Into<String>,
    ) -> Result<Self, ImageRefError> {
        let sha256 = sha256.into();
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ImageRefError::InvalidHash);
        }
        if width == 0 || height == 0 {
            return Err(ImageRefError::EmptyDimensions);
        }
        if byte_len == 0 {
            return Err(ImageRefError::EmptyBytes);
        }
        let label: String = label.into();
        if label.chars().any(char::is_control) {
            return Err(ImageRefError::InvalidLabel);
        }
        let label = label.chars().take(Self::MAX_LABEL_CHARS).collect();
        Ok(Self {
            sha256,
            media_type,
            width,
            height,
            byte_len,
            label,
        })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn media_type(&self) -> ImageMediaType {
        self.media_type
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

impl TryFrom<ImageRefFields> for ImageRef {
    type Error = ImageRefError;

    fn try_from(fields: ImageRefFields) -> Result<Self, Self::Error> {
        Self::new(
            fields.sha256,
            fields.media_type,
            fields.width,
            fields.height,
            fields.byte_len,
            fields.label,
        )
    }
}

impl From<ImageRef> for ImageRefFields {
    fn from(image: ImageRef) -> Self {
        Self {
            sha256: image.sha256,
            media_type: image.media_type,
            width: image.width,
            height: image.height,
            byte_len: image.byte_len,
            label: image.label,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    /// The bytes of images the messages refer to, by SHA-256. An image
    /// with no entry here is sent as a text placeholder, never dropped
    /// silently.
    pub images: ImageBytes,
}

impl ChatRequest {
    /// A request with no image bytes.
    pub fn new(messages: Vec<ChatMessage>, tools: Vec<ToolDefinition>) -> Self {
        Self {
            messages,
            tools,
            images: ImageBytes::new(),
        }
    }
}

/// Image bytes by the SHA-256 of their stored encoding.
pub type ImageBytes = std::collections::BTreeMap<String, std::sync::Arc<Vec<u8>>>;

/// Where an engine reads the bytes of the images a conversation refers to,
/// such as a session's image store.
pub trait ImageSource: Send + Sync {
    /// The verified bytes of `image`, or why they cannot be read.
    fn image_bytes(&self, image: &ImageRef) -> Result<Vec<u8>, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// Reasoning streamed separately from `content`, when the provider
    /// reports it.
    pub reasoning: Option<String>,
    /// The provider's stated reason for ending generation ("stop",
    /// "tool_calls", "length", ...), when reported.
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    TextDelta(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentDescriptor {
    pub id: String,
    pub display_name: String,
    pub model: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Error)]
pub enum AdapterError {
    /// The request needs something this adapter cannot send, such as images
    /// to a provider without image support. Raised before any request is
    /// made, so content is refused rather than silently dropped.
    #[error("the agent endpoint cannot accept this request: {0}")]
    Unsupported(String),
    #[error("the connection to the agent endpoint failed: {0}")]
    Transport(String),
    #[error("the agent endpoint rejected the request ({status}): {body}")]
    Http { status: u16, body: String },
    #[error("the agent returned an invalid response: {0}")]
    InvalidResponse(String),
    /// The provider said the prompt is larger than the model's context window.
    /// Carried as its own variant (rather than a plain `Http` error) so the
    /// engine can compact and retry instead of failing the turn, and so a
    /// rejected turn cannot leave the session permanently oversized.
    /// The token counts are `Option`s because several providers report the
    /// rejection without usable numbers (a truncated error body, or Ollama's
    /// bare-string error). Recognising the overflow matters far more than the
    /// counts, which are only used to describe the failure.
    #[error("the prompt exceeds the model's context window")]
    ContextOverflow {
        prompt_tokens: Option<usize>,
        available_tokens: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ImageRef {
        ImageRef::new(
            "0f".repeat(32),
            ImageMediaType::Png,
            640,
            480,
            1024,
            "plot.png",
        )
        .unwrap()
    }

    #[test]
    fn image_refs_reject_malformed_fields() {
        let new = |hash: &str, width, height, bytes, label: &str| {
            ImageRef::new(hash, ImageMediaType::Jpeg, width, height, bytes, label)
        };
        let hash = "a".repeat(64);
        assert!(new(&hash, 1, 1, 1, "ok").is_ok());
        assert_eq!(
            new(&"a".repeat(63), 1, 1, 1, ""),
            Err(ImageRefError::InvalidHash)
        );
        assert_eq!(
            new(&"A".repeat(64), 1, 1, 1, ""),
            Err(ImageRefError::InvalidHash)
        );
        assert_eq!(
            new(&"g".repeat(64), 1, 1, 1, ""),
            Err(ImageRefError::InvalidHash)
        );
        assert_eq!(new(&hash, 0, 1, 1, ""), Err(ImageRefError::EmptyDimensions));
        assert_eq!(new(&hash, 1, 0, 1, ""), Err(ImageRefError::EmptyDimensions));
        assert_eq!(new(&hash, 1, 1, 0, ""), Err(ImageRefError::EmptyBytes));
        assert_eq!(
            new(&hash, 1, 1, 1, "a\nb"),
            Err(ImageRefError::InvalidLabel)
        );
        assert_eq!(
            new(&hash, 1, 1, 1, "\u{1b}[31m"),
            Err(ImageRefError::InvalidLabel)
        );
    }

    #[test]
    fn image_ref_labels_are_bounded() {
        let long = "é".repeat(ImageRef::MAX_LABEL_CHARS + 10);
        let image = ImageRef::new("a".repeat(64), ImageMediaType::Png, 1, 1, 1, long).unwrap();
        assert_eq!(image.label().chars().count(), ImageRef::MAX_LABEL_CHARS);
    }

    #[test]
    fn image_refs_round_trip_and_validate_on_deserialize() {
        let encoded = serde_json::to_value(image()).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "sha256": "0f".repeat(32),
                "media_type": "image/png",
                "width": 640,
                "height": 480,
                "byte_len": 1024,
                "label": "plot.png",
            })
        );
        assert_eq!(
            serde_json::from_value::<ImageRef>(encoded.clone()).unwrap(),
            image()
        );

        let mut bad_hash = encoded.clone();
        bad_hash["sha256"] = "not-a-hash".into();
        assert!(serde_json::from_value::<ImageRef>(bad_hash).is_err());

        let mut bad_type = encoded.clone();
        bad_type["media_type"] = "image/svg+xml".into();
        assert!(serde_json::from_value::<ImageRef>(bad_type).is_err());

        let mut extra = encoded;
        extra["bytes"] = "iVBORw0KGgo=".into();
        assert!(serde_json::from_value::<ImageRef>(extra).is_err());
    }

    #[test]
    fn messages_without_images_serialize_as_before() {
        let message = ChatMessage::user("hello");
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","content":"hello"}"#
        );
        let loaded: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert_eq!(loaded, message);
        assert!(loaded.images.is_empty());
    }

    #[test]
    fn messages_carry_images_in_order() {
        let second =
            ImageRef::new("1e".repeat(32), ImageMediaType::Jpeg, 8, 8, 64, "photo.jpg").unwrap();
        let message = ChatMessage::user("compare these")
            .with_images([image()])
            .with_images([second.clone()]);
        assert_eq!(message.images, vec![image(), second]);

        let encoded = serde_json::to_string(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<ChatMessage>(&encoded).unwrap(),
            message
        );
    }

    #[test]
    fn classifies_agent_input() {
        assert_eq!(
            classify_input("  inspect this project  "),
            InputRoute::Agent("inspect this project".into())
        );
    }

    #[test]
    fn classifies_shell_input() {
        assert_eq!(
            classify_input("$  git status --short"),
            InputRoute::Shell("git status --short".into())
        );
        assert_eq!(
            classify_input("$$  git status --short"),
            InputRoute::StickyShell("git status --short".into())
        );
    }

    #[test]
    fn classifies_control_input() {
        assert_eq!(
            classify_input("//agent set local"),
            InputRoute::Control(ControlCommand::Agent(vec!["set".into(), "local".into()]))
        );
    }

    #[test]
    fn double_slash_takes_precedence_over_shell_and_agent() {
        assert!(matches!(
            classify_input("//status"),
            InputRoute::Control(ControlCommand::Status)
        ));
    }

    #[test]
    fn parses_tools_control_command() {
        assert_eq!(
            classify_input("//tools"),
            InputRoute::Control(ControlCommand::Tools)
        );
    }

    #[test]
    fn parses_history_control_command() {
        assert_eq!(
            classify_input("//history"),
            InputRoute::Control(ControlCommand::History)
        );
    }

    #[test]
    fn parses_model_control_command() {
        assert_eq!(
            classify_input("//model openrouter-free"),
            InputRoute::Control(ControlCommand::Model(vec!["openrouter-free".into()]))
        );
    }

    #[test]
    fn parses_limits_control_command() {
        assert_eq!(
            classify_input("//limits"),
            InputRoute::Control(ControlCommand::Limits(Vec::new()))
        );
        assert_eq!(
            classify_input("//limits relaxed"),
            InputRoute::Control(ControlCommand::Limits(vec!["relaxed".into()]))
        );
    }

    #[test]
    fn parses_audit_control_command() {
        assert_eq!(
            classify_input("//audit status"),
            InputRoute::Control(ControlCommand::Audit(vec!["status".into()]))
        );
    }

    #[test]
    fn parses_session_control_commands() {
        assert_eq!(
            classify_input("//connect rich@mini.local --session cad"),
            InputRoute::Control(ControlCommand::Connect(vec![
                "rich@mini.local".into(),
                "--session".into(),
                "cad".into()
            ]))
        );
        assert_eq!(
            classify_input("//new bees --durable"),
            InputRoute::Control(ControlCommand::New(vec!["bees".into(), "--durable".into()]))
        );
        assert_eq!(
            classify_input("//switch robot"),
            InputRoute::Control(ControlCommand::Switch(vec!["robot".into()]))
        );
        assert_eq!(
            classify_input("//sessions"),
            InputRoute::Control(ControlCommand::Sessions)
        );
        assert_eq!(
            classify_input("//stop"),
            InputRoute::Control(ControlCommand::Stop(Vec::new()))
        );
        assert_eq!(
            classify_input("//resume"),
            InputRoute::Control(ControlCommand::Resume(Vec::new()))
        );
        assert_eq!(
            classify_input("//detach"),
            InputRoute::Control(ControlCommand::Detach)
        );
    }

    #[test]
    fn preserves_view_arguments_for_quoted_paths() {
        assert_eq!(
            classify_input("//view --as rst \"docs/design notes.rst\""),
            InputRoute::Control(ControlCommand::View(
                "--as rst \"docs/design notes.rst\"".into()
            ))
        );
    }
}
