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
    Agent(Vec<String>),
    View(String),
    Connect(Vec<String>),
    Sessions,
    Resume(Vec<String>),
    Stop(Vec<String>),
    New(Vec<String>),
    Switch(Vec<String>),
    Detach,
    Close(Vec<String>),
    Quit,
    Unknown { name: String, args: Vec<String> },
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
    let args: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();

    match name {
        "help" => ControlCommand::Help,
        "history" => ControlCommand::History,
        "status" => ControlCommand::Status,
        "audit" => ControlCommand::Audit(args),
        "tools" => ControlCommand::Tools,
        "model" => ControlCommand::Model(args),
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
        }
    }

    /// Attach the model's reasoning to an assistant message; empty reasoning
    /// is stored as `None`.
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning.filter(|text| !text.trim().is_empty());
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
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
    #[error("the agent endpoint could not be reached: {0}")]
    Transport(String),
    #[error("the agent endpoint rejected the request ({status}): {body}")]
    Http { status: u16, body: String },
    #[error("the agent returned an invalid response: {0}")]
    InvalidResponse(String),
    /// The provider said the prompt is larger than the model's context window.
    /// Carried as its own variant (rather than a plain `Http` error) so the
    /// engine can compact and retry instead of failing the turn, and so a
    /// rejected turn cannot leave the session permanently oversized.
    #[error(
        "the prompt ({prompt_tokens} tokens) exceeds the model's context window ({available_tokens} tokens)"
    )]
    ContextOverflow {
        prompt_tokens: usize,
        available_tokens: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

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
