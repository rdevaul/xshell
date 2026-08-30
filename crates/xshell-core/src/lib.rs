use clap::ValueEnum;
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
    Control(ControlCommand),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    Help,
    Status,
    Tools,
    Agent(Vec<String>),
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

    if let Some(shell) = trimmed.strip_prefix('$') {
        return InputRoute::Shell(shell.trim_start().to_owned());
    }

    InputRoute::Agent(trimmed.to_owned())
}

fn parse_control(input: &str) -> ControlCommand {
    let mut words = input.split_whitespace();
    let Some(name) = words.next() else {
        return ControlCommand::Help;
    };
    let args: Vec<String> = words.map(str::to_owned).collect();

    match name {
        "help" => ControlCommand::Help,
        "status" => ControlCommand::Status,
        "tools" => ControlCommand::Tools,
        "agent" => ControlCommand::Agent(args),
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
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
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

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
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
}

/// Approval policy for tools that require user confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApprovalMode {
    /// Prompt the user before any tool that requires approval (e.g. shell execution).
    Ask,
    /// Auto-run every tool without prompting, including shell execution.
    Auto,
    /// Refuse every tool that requires approval; read-only tools still run.
    Off,
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ApprovalMode::Ask => "ask before shell execution",
            ApprovalMode::Auto => "auto-run all tools",
            ApprovalMode::Off => "deny shell execution",
        };
        f.write_str(text)
    }
}

/// Decide whether a tool call runs now without prompting.
///
/// `gated` is true for tools that require approval (e.g. `run_shell`).
/// `Ask` returns false for gated calls so the caller can prompt; `Auto`
/// runs everything; `Off` refuses gated calls. Read-only tools are
/// gated by `requires_approval` at the call site and run in every mode.
pub fn resolve_approval(mode: ApprovalMode, gated: bool) -> bool {
    match mode {
        ApprovalMode::Ask => !gated,
        ApprovalMode::Auto => true,
        ApprovalMode::Off => !gated,
    }
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
    fn approval_mode_parses_each_variant() {
        use clap::ValueEnum;
        // clap 4's ValueEnum::from_str is (name, ignore_case).
        assert_eq!(
            ApprovalMode::from_str("ask", false).unwrap(),
            ApprovalMode::Ask
        );
        assert_eq!(
            ApprovalMode::from_str("auto", false).unwrap(),
            ApprovalMode::Auto
        );
        assert_eq!(
            ApprovalMode::from_str("off", false).unwrap(),
            ApprovalMode::Off
        );
        assert!(ApprovalMode::from_str("yolo", false).is_err());
    }

    #[test]
    fn ask_prompts_only_gated_calls() {
        assert!(!resolve_approval(ApprovalMode::Ask, true));
        assert!(resolve_approval(ApprovalMode::Ask, false));
    }

    #[test]
    fn auto_runs_everything() {
        assert!(resolve_approval(ApprovalMode::Auto, true));
        assert!(resolve_approval(ApprovalMode::Auto, false));
    }

    #[test]
    fn off_denies_gated_but_allows_read_only() {
        assert!(!resolve_approval(ApprovalMode::Off, true));
        assert!(resolve_approval(ApprovalMode::Off, false));
    }

    #[test]
    fn approval_mode_display_names() {
        assert_eq!(ApprovalMode::Ask.to_string(), "ask before shell execution");
        assert_eq!(ApprovalMode::Auto.to_string(), "auto-run all tools");
        assert_eq!(ApprovalMode::Off.to_string(), "deny shell execution");
    }
}
