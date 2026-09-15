use crate::process::{ProcessGroupGuard, configure_process_group};
use crate::tools::execute_tool_cancellable;
use crate::{
    CompactionConfig, CompactionReport, Compactor, GateReason, SensitivePaths, definitions,
    requires_approval,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Notify, mpsc};
use xshell_adapters::{AgentAdapter, OllamaAdapter, OpenAiCompatibleAdapter};
use xshell_core::{AgentEvent, ChatMessage, ChatRequest, ToolCall};

const MAX_AGENT_STEPS: usize = 64;
const DIRECT_SHELL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const DIRECT_SHELL_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Ask,
    Auto,
    Off,
}

impl ApprovalPolicy {
    /// How much a policy lets an agent do without a human decision.
    /// `Off` (deny shell) < `Ask` (prompt) < `Auto` (run everything).
    fn permissiveness(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Ask => 1,
            Self::Auto => 2,
        }
    }

    /// Return the less permissive of `self` and `ceiling`. Used by a session
    /// daemon to bound whatever a client requests.
    pub fn clamp_to(self, ceiling: Self) -> Self {
        if self.permissiveness() <= ceiling.permissiveness() {
            self
        } else {
            ceiling
        }
    }
}

impl std::fmt::Display for ApprovalPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Ask => "ask before shell execution",
            Self::Auto => "auto-run all tools",
            Self::Off => "deny shell execution",
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
    AbortTurn,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEvent {
    TextDelta {
        text: String,
    },
    AgentResponse {
        content: String,
        tool_call_count: usize,
        partial: bool,
    },
    ToolRequested {
        call: ToolCall,
    },
    ApprovalRequested {
        call: ToolCall,
        /// Why this call is gated. Absent in journals written before the
        /// field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<GateReason>,
    },
    ToolDecision {
        call_id: String,
        decision: ApprovalDecision,
    },
    /// A tool call that was never evaluated because the user aborted the
    /// turn at an earlier call in the same response.
    ToolSkipped {
        call_id: String,
        name: String,
    },
    ToolResult {
        call_id: String,
        name: String,
        result: String,
    },
    TurnAborted,
    /// Older turns were removed from the conversation after this turn
    /// completed, to keep the history within the configured budget.
    HistoryCompacted {
        report: CompactionReport,
    },
}

#[derive(Debug, Clone)]
pub struct AdapterConfig {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

/// A one-way "stop" signal shared between a running turn and whoever may
/// cancel it (a client request, an audit failure, daemon shutdown).
///
/// `cancel` may be called from any thread, including plain std threads that
/// have no tokio context; `wait` is a future that resolves promptly without
/// polling.
#[derive(Debug, Clone, Default)]
pub struct CancellationFlag(Arc<CancellationInner>);

#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationFlag {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        // Wake every current waiter. Futures that start waiting later see the
        // flag first (see `wait`) so no wake-up can be missed.
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn wait(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            // Register interest before re-checking so a `cancel` that lands
            // between the check above and the await below is not lost:
            // `notified()` captures the wake-up as soon as it is created.
            let notified = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[async_trait]
pub trait TurnObserver: Send {
    fn emit(&mut self, event: ExecutionEvent);
    fn cancellation(&self) -> CancellationFlag;
    async fn approve(&mut self, call: &ToolCall, reason: GateReason) -> ApprovalDecision;
}

pub fn build_adapter(config: &AdapterConfig) -> Result<Box<dyn AgentAdapter>> {
    let adapter: Box<dyn AgentAdapter> = match config.provider.as_str() {
        "ollama" => Box::new(OllamaAdapter::new(&config.base_url, &config.model)),
        "openai" => Box::new(OpenAiCompatibleAdapter::new(
            &config.base_url,
            &config.model,
            resolve_api_key(config)?,
        )),
        provider => bail!("unsupported agent provider {provider:?}"),
    };
    Ok(adapter)
}

fn resolve_api_key(config: &AdapterConfig) -> Result<Option<String>> {
    let Some(variable) = &config.api_key_env else {
        return Ok(None);
    };
    let value = env::var(variable).context(
        "the configured credential environment variable is not set or is not valid Unicode",
    )?;
    if value.is_empty() {
        bail!("the configured credential environment variable is empty");
    }
    Ok(Some(value))
}

/// Everything that governs what an agent may do during one turn.
pub struct TurnPolicy {
    pub approval: ApprovalPolicy,
    pub sensitive_paths: SensitivePaths,
    /// Applied before the first provider request and after a turn completes.
    /// Failed and cancelled turns restore the pre-turn history.
    pub compactor: Arc<dyn Compactor>,
}

impl std::fmt::Debug for TurnPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnPolicy")
            .field("approval", &self.approval)
            .field("sensitive_paths", &self.sensitive_paths)
            .field("compactor", &self.compactor.name())
            .finish()
    }
}

impl Clone for TurnPolicy {
    fn clone(&self) -> Self {
        Self {
            approval: self.approval,
            sensitive_paths: self.sensitive_paths.clone(),
            compactor: Arc::clone(&self.compactor),
        }
    }
}

impl TurnPolicy {
    pub fn new(approval: ApprovalPolicy) -> Self {
        Self {
            approval,
            sensitive_paths: SensitivePaths::default(),
            compactor: Arc::from(CompactionConfig::default().build()),
        }
    }

    pub fn with_sensitive_paths(mut self, sensitive_paths: SensitivePaths) -> Self {
        self.sensitive_paths = sensitive_paths;
        self
    }

    pub fn with_compaction(mut self, config: &CompactionConfig) -> Self {
        self.compactor = Arc::from(config.build());
        self
    }
}

/// Run the policy's compactor and report what it did. Used after a successful
/// turn; the pre-request pass is staged separately in [`run_agent_turn`] so it
/// can be rolled back if the provider rejects or the turn is cancelled.
pub fn compact_history(
    policy: &TurnPolicy,
    history: &mut Vec<ChatMessage>,
    observer: &mut dyn TurnObserver,
) {
    if let Some(report) = policy.compactor.compact(history) {
        observer.emit(ExecutionEvent::HistoryCompacted { report });
    }
}

pub async fn run_agent_turn(
    agent: &mut dyn AgentAdapter,
    history: &mut Vec<ChatMessage>,
    message: String,
    cwd: &Path,
    policy: &TurnPolicy,
    observer: &mut dyn TurnObserver,
) -> Result<()> {
    let approval = policy.approval;
    // Compact after staging the new user message so the compactor can discard
    // every older turn when necessary while preserving the prompt about to be
    // sent. Keep the exact prior history for failed/cancelled-turn rollback.
    let rollback_history = history.clone();
    history.push(ChatMessage::user(message));
    let mut request_compaction = policy.compactor.compact(history);
    let tools = definitions();
    let mut observed_tool_activity = false;

    for _ in 0..MAX_AGENT_STEPS {
        let mut streamed_text = String::new();
        let cancellation = observer.cancellation();
        let response = {
            let observer = &mut *observer;
            let mut emit = |event| match event {
                AgentEvent::TextDelta(text) => {
                    streamed_text.push_str(&text);
                    observer.emit(ExecutionEvent::TextDelta { text });
                }
            };
            tokio::select! {
                response = agent.chat_stream(
                    ChatRequest {
                        messages: history.clone(),
                        tools: tools.clone(),
                    },
                    &mut emit,
                ) => Some(response),
                () = cancellation.wait() => None,
            }
        };
        let Some(response) = response else {
            preserve_or_rollback_failed_turn(
                history,
                &rollback_history,
                observed_tool_activity,
                "agent turn cancelled",
            );
            bail!("agent turn cancelled");
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if !streamed_text.is_empty() {
                    observer.emit(ExecutionEvent::AgentResponse {
                        content: streamed_text,
                        tool_call_count: 0,
                        partial: true,
                    });
                }
                preserve_or_rollback_failed_turn(
                    history,
                    &rollback_history,
                    observed_tool_activity,
                    &format!("agent provider failed: {error}"),
                );
                return Err(error.into());
            }
        };

        // Do not claim that durable history changed until the provider has
        // accepted the compacted request. If this was a restored oversized
        // session, this event records exactly what the provider first saw.
        if let Some(report) = request_compaction.take() {
            observer.emit(ExecutionEvent::HistoryCompacted { report });
        }

        observer.emit(ExecutionEvent::AgentResponse {
            content: response.content.clone(),
            tool_call_count: response.tool_calls.len(),
            partial: false,
        });
        for call in &response.tool_calls {
            observer.emit(ExecutionEvent::ToolRequested { call: call.clone() });
        }
        history.push(ChatMessage::assistant_with_tools(
            response.content,
            response.tool_calls.clone(),
        ));
        if response.tool_calls.is_empty() {
            compact_history(policy, history, observer);
            return Ok(());
        }

        for (index, call) in response.tool_calls.iter().enumerate() {
            if observer.cancellation().is_cancelled() {
                if observed_tool_activity {
                    stub_unfinished_tool_calls(
                        history,
                        &response.tool_calls[index..],
                        "tool not run because the agent turn was cancelled",
                    );
                }
                preserve_or_rollback_failed_turn(
                    history,
                    &rollback_history,
                    observed_tool_activity,
                    "agent turn cancelled",
                );
                bail!("agent turn cancelled");
            }
            let gate = requires_approval(call, cwd, &policy.sensitive_paths);
            let decision = match gate {
                None => ApprovalDecision::Approve,
                Some(reason) => match approval {
                    ApprovalPolicy::Auto => ApprovalDecision::Approve,
                    ApprovalPolicy::Off => ApprovalDecision::Deny,
                    ApprovalPolicy::Ask => {
                        observer.emit(ExecutionEvent::ApprovalRequested {
                            call: call.clone(),
                            reason: Some(reason),
                        });
                        observer.approve(call, reason).await
                    }
                },
            };
            observer.emit(ExecutionEvent::ToolDecision {
                call_id: call.id.clone(),
                decision,
            });
            // A decision is the last audit boundary before the action. An
            // observer can cancel when recording it fails; honor that before
            // executing the exact tool whose audit record is unavailable.
            if observer.cancellation().is_cancelled() {
                if observed_tool_activity {
                    stub_unfinished_tool_calls(
                        history,
                        &response.tool_calls[index..],
                        "tool not run because the agent turn was cancelled before execution",
                    );
                }
                preserve_or_rollback_failed_turn(
                    history,
                    &rollback_history,
                    observed_tool_activity,
                    "agent turn cancelled before tool execution",
                );
                bail!("agent turn cancelled before tool execution");
            }
            if decision == ApprovalDecision::AbortTurn {
                for skipped in &response.tool_calls[index..] {
                    history.push(ChatMessage::tool_result(
                        skipped,
                        "tool execution aborted by user; agent turn stopped",
                    ));
                }
                for skipped in &response.tool_calls[index + 1..] {
                    observer.emit(ExecutionEvent::ToolSkipped {
                        call_id: skipped.id.clone(),
                        name: skipped.name.clone(),
                    });
                }
                observer.emit(ExecutionEvent::TurnAborted);
                return Ok(());
            }

            let result = if decision == ApprovalDecision::Approve {
                observed_tool_activity = true;
                execute_tool_cancellable(call, cwd, observer.cancellation()).await
            } else {
                "tool denied by user".into()
            };
            observer.emit(ExecutionEvent::ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                result: result.clone(),
            });
            history.push(ChatMessage::tool_result(call, result));
            if observer.cancellation().is_cancelled() {
                for skipped in &response.tool_calls[index + 1..] {
                    observer.emit(ExecutionEvent::ToolSkipped {
                        call_id: skipped.id.clone(),
                        name: skipped.name.clone(),
                    });
                }
                stub_unfinished_tool_calls(
                    history,
                    &response.tool_calls[index + 1..],
                    "tool not run because the agent turn was cancelled",
                );
                preserve_or_rollback_failed_turn(
                    history,
                    &rollback_history,
                    observed_tool_activity,
                    "agent turn cancelled during tool execution",
                );
                bail!("agent turn cancelled during tool execution");
            }
        }
    }
    preserve_or_rollback_failed_turn(
        history,
        &rollback_history,
        observed_tool_activity,
        &format!("agent exceeded the {MAX_AGENT_STEPS}-step tool-call limit"),
    );
    bail!("agent exceeded the {MAX_AGENT_STEPS}-step tool-call limit")
}

fn stub_unfinished_tool_calls(history: &mut Vec<ChatMessage>, calls: &[ToolCall], message: &str) {
    for call in calls {
        history.push(ChatMessage::tool_result(call, message));
    }
}

fn preserve_or_rollback_failed_turn(
    history: &mut Vec<ChatMessage>,
    rollback_history: &[ChatMessage],
    observed_tool_activity: bool,
    message: &str,
) {
    if observed_tool_activity {
        history.push(ChatMessage::system(format!(
            "xshell execution record: the previous turn ended after observed tool activity: {message}"
        )));
    } else {
        *history = rollback_history.to_vec();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectShellResult {
    pub cwd: PathBuf,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_direct_shell(command: &str, cwd: &Path) -> Result<DirectShellResult> {
    run_direct_shell_streaming(command, cwd, |_, _| {}).await
}

pub async fn run_direct_shell_streaming(
    command: &str,
    cwd: &Path,
    emit: impl FnMut(&str, String) + Send,
) -> Result<DirectShellResult> {
    run_direct_shell_streaming_cancellable(command, cwd, CancellationFlag::default(), emit).await
}

pub async fn run_direct_shell_streaming_cancellable(
    command: &str,
    cwd: &Path,
    cancellation: CancellationFlag,
    mut emit: impl FnMut(&str, String) + Send,
) -> Result<DirectShellResult> {
    if command.trim().is_empty() {
        return Ok(DirectShellResult {
            cwd: cwd.to_owned(),
            status: "empty command".into(),
            stdout: String::new(),
            stderr: String::new(),
        });
    }
    if let Some(next) = captured_cd_destination(command, cwd)? {
        return Ok(DirectShellResult {
            cwd: validate_working_directory(&next)?,
            status: "working directory changed".into(),
            stdout: String::new(),
            stderr: String::new(),
        });
    }

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut process = Command::new(&shell);
    process
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .with_context(|| format!("could not launch shell {shell}"))?;
    let mut group = ProcessGroupGuard::for_child(&child);
    let stdout = child.stdout.take().context("cannot capture shell stdout")?;
    let stderr = child.stderr.take().context("cannot capture shell stderr")?;
    let (sender, mut receiver) = mpsc::channel::<(&'static str, Vec<u8>)>(32);
    let stdout_task = tokio::spawn(read_output("stdout", stdout, sender.clone()));
    let stderr_task = tokio::spawn(read_output("stderr", stderr, sender.clone()));
    drop(sender);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut wait_task = tokio::spawn(async move { child.wait().await });
    let deadline = tokio::time::sleep(DIRECT_SHELL_TIMEOUT);
    tokio::pin!(deadline);
    let terminal = loop {
        tokio::select! {
            item = receiver.recv() => {
                if let Some((stream, bytes)) = item {
                    retain_shell_chunk(
                        stream,
                        &bytes,
                        &mut stdout_bytes,
                        &mut stderr_bytes,
                        &mut emit,
                    );
                }
            }
            result = &mut wait_task => {
                break Some(result.context("shell wait task failed")??);
            }
            () = cancellation.wait() => break None,
            () = &mut deadline => break None,
        }
    };
    let status = match terminal {
        Some(status) => status,
        None => {
            let cancelled = cancellation.is_cancelled();
            group.kill();
            let _ = wait_task.await;
            group.disarm();
            while let Some((stream, bytes)) = receiver.recv().await {
                retain_shell_chunk(
                    stream,
                    &bytes,
                    &mut stdout_bytes,
                    &mut stderr_bytes,
                    &mut emit,
                );
            }
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            if cancelled {
                bail!("shell command cancelled; the process group was killed and reaped");
            }
            bail!("shell command timed out; the process group was killed and reaped");
        }
    };
    group.disarm();
    while let Some((stream, bytes)) = receiver.recv().await {
        retain_shell_chunk(
            stream,
            &bytes,
            &mut stdout_bytes,
            &mut stderr_bytes,
            &mut emit,
        );
    }
    stdout_task.await.context("stdout reader failed")??;
    stderr_task.await.context("stderr reader failed")??;
    Ok(DirectShellResult {
        cwd: cwd.to_owned(),
        status: status.to_string(),
        stdout: bounded_utf8(&stdout_bytes),
        stderr: bounded_utf8(&stderr_bytes),
    })
}

fn retain_shell_chunk(
    stream: &str,
    bytes: &[u8],
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    emit: &mut impl FnMut(&str, String),
) {
    let target = if stream == "stderr" { stderr } else { stdout };
    if target.len() < DIRECT_SHELL_OUTPUT_LIMIT {
        let remaining = DIRECT_SHELL_OUTPUT_LIMIT - target.len();
        target.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }
    emit(stream, String::from_utf8_lossy(bytes).into_owned());
}

async fn read_output<R>(
    stream: &'static str,
    mut reader: R,
    sender: mpsc::Sender<(&'static str, Vec<u8>)>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        if sender
            .send((stream, buffer[..count].to_vec()))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}

fn bounded_utf8(bytes: &[u8]) -> String {
    let truncated = bytes.len() > DIRECT_SHELL_OUTPUT_LIMIT;
    let bytes = &bytes[..bytes.len().min(DIRECT_SHELL_OUTPUT_LIMIT)];
    let mut value = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        value.push_str("\n[output truncated]");
    }
    value
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn validate_working_directory(path: &Path) -> Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .with_context(|| format!("cannot resolve working directory {}", path.display()))?;
    if !resolved.is_dir() {
        bail!(
            "working directory {} is not a directory",
            resolved.display()
        );
    }
    std::fs::read_dir(&resolved)
        .with_context(|| format!("working directory {} is not accessible", resolved.display()))?;
    Ok(resolved)
}

pub fn captured_cd_destination(command: &str, cwd: &Path) -> Result<Option<PathBuf>> {
    let trimmed = command.trim();
    if trimmed != "cd" && (!trimmed.starts_with("cd ") || has_unquoted_shell_operator(trimmed)) {
        return Ok(None);
    }
    let words = shell_words::split(trimmed).context("could not parse shell command")?;
    if words.first().map(String::as_str) != Some("cd") {
        return Ok(None);
    }
    if words.len() > 2 {
        bail!("cd expects zero or one path");
    }
    let destination = match words.get(1) {
        Some(path) => expand_tilde(path)?,
        None => home_dir()?,
    };
    Ok(Some(if destination.is_absolute() {
        destination
    } else {
        cwd.join(destination)
    }))
}

fn has_unquoted_shell_operator(command: &str) -> bool {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if !single_quoted => escaped = true,
            '\'' if !double_quoted => single_quoted = !single_quoted,
            '"' if !single_quoted => double_quoted = !double_quoted,
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '\n' if !single_quoted && !double_quoted => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use tempfile::TempDir;
    use xshell_core::{AdapterError, AgentDescriptor, AssistantResponse};

    /// Replays a fixed script of assistant responses.
    struct ScriptedAdapter {
        responses: VecDeque<AssistantResponse>,
        requests: Vec<ChatRequest>,
    }

    #[async_trait]
    impl AgentAdapter for ScriptedAdapter {
        fn descriptor(&self) -> AgentDescriptor {
            AgentDescriptor {
                id: "scripted".into(),
                display_name: "scripted".into(),
                model: "test".into(),
                capabilities: Vec::new(),
            }
        }

        async fn chat_stream(
            &mut self,
            request: ChatRequest,
            events: &mut (dyn FnMut(AgentEvent) + Send),
        ) -> Result<AssistantResponse, AdapterError> {
            self.requests.push(request);
            let response = self
                .responses
                .pop_front()
                .ok_or_else(|| AdapterError::Transport("script exhausted".into()))?;
            if !response.content.is_empty() {
                events(AgentEvent::TextDelta(response.content.clone()));
            }
            Ok(response)
        }
    }

    /// Records every event and answers approvals from a script.
    struct RecordingObserver {
        events: Vec<ExecutionEvent>,
        decisions: VecDeque<ApprovalDecision>,
        cancellation: CancellationFlag,
        cancel_on_decision: bool,
    }

    #[async_trait]
    impl TurnObserver for RecordingObserver {
        fn emit(&mut self, event: ExecutionEvent) {
            if self.cancel_on_decision && matches!(event, ExecutionEvent::ToolDecision { .. }) {
                self.cancellation.cancel();
            }
            self.events.push(event);
        }

        fn cancellation(&self) -> CancellationFlag {
            self.cancellation.clone()
        }

        async fn approve(&mut self, _call: &ToolCall, _reason: GateReason) -> ApprovalDecision {
            self.decisions.pop_front().expect("unscripted approval")
        }
    }

    fn shell_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "run_shell".into(),
            arguments: json!({"command": command}),
        }
    }

    fn list_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "list_directory".into(),
            arguments: json!({}),
        }
    }

    fn kinds(events: &[ExecutionEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                ExecutionEvent::TextDelta { .. } => "delta",
                ExecutionEvent::AgentResponse { .. } => "response",
                ExecutionEvent::ToolRequested { .. } => "requested",
                ExecutionEvent::ApprovalRequested { .. } => "approval",
                ExecutionEvent::ToolDecision { .. } => "decision",
                ExecutionEvent::ToolSkipped { .. } => "skipped",
                ExecutionEvent::ToolResult { .. } => "result",
                ExecutionEvent::TurnAborted => "aborted",
                ExecutionEvent::HistoryCompacted { .. } => "compacted",
            })
            .collect()
    }

    #[test]
    fn approval_clamp_never_exceeds_the_ceiling() {
        use ApprovalPolicy::{Ask, Auto, Off};
        assert_eq!(Auto.clamp_to(Off), Off);
        assert_eq!(Auto.clamp_to(Ask), Ask);
        assert_eq!(Auto.clamp_to(Auto), Auto);
        assert_eq!(Ask.clamp_to(Off), Off);
        assert_eq!(Ask.clamp_to(Auto), Ask);
        assert_eq!(Off.clamp_to(Auto), Off);
    }

    #[tokio::test]
    async fn read_only_tools_run_without_approval_and_shell_tools_prompt() {
        let temporary = TempDir::new().unwrap();
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([
                AssistantResponse {
                    content: "looking".into(),
                    tool_calls: vec![list_call("a"), shell_call("b", "printf hi")],
                },
                AssistantResponse {
                    content: "done".into(),
                    tool_calls: Vec::new(),
                },
            ]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::from([ApprovalDecision::Deny]),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let mut history = Vec::new();
        run_agent_turn(
            &mut adapter,
            &mut history,
            "hello".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Ask),
            &mut observer,
        )
        .await
        .unwrap();

        assert_eq!(
            kinds(&observer.events),
            [
                "delta",
                "response",
                "requested",
                "requested",
                "decision",
                "result",
                "approval",
                "decision",
                "result",
                "delta",
                "response",
            ]
        );
        let results: Vec<_> = observer
            .events
            .iter()
            .filter_map(|event| match event {
                ExecutionEvent::ToolResult {
                    call_id, result, ..
                } => Some((call_id.as_str(), result.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(results[0].0, "a");
        assert!(!results[0].1.starts_with("tool error"));
        assert_eq!(results[1], ("b", "tool denied by user"));
        // user, assistant(2 tools), tool, tool, assistant
        assert_eq!(history.len(), 5);
        assert_eq!(adapter.requests.len(), 2);
    }

    #[tokio::test]
    async fn abort_skips_remaining_tools_and_stubs_history() {
        let temporary = TempDir::new().unwrap();
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([AssistantResponse {
                content: String::new(),
                tool_calls: vec![
                    shell_call("a", "true"),
                    shell_call("b", "true"),
                    list_call("c"),
                ],
            }]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::from([ApprovalDecision::AbortTurn]),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let mut history = Vec::new();
        run_agent_turn(
            &mut adapter,
            &mut history,
            "hello".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Ask),
            &mut observer,
        )
        .await
        .unwrap();

        assert_eq!(
            kinds(&observer.events),
            [
                "response",
                "requested",
                "requested",
                "requested",
                "approval",
                "decision",
                "skipped",
                "skipped",
                "aborted",
            ]
        );
        let skipped: Vec<_> = observer
            .events
            .iter()
            .filter_map(|event| match event {
                ExecutionEvent::ToolSkipped { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(skipped, ["b", "c"]);
        // Every tool call has a result stub so the transcript stays valid.
        assert_eq!(history.len(), 5);
        assert!(history[2..].iter().all(|message| {
            message.content == "tool execution aborted by user; agent turn stopped"
        }));
    }

    #[tokio::test]
    async fn policy_off_denies_shell_without_prompting() {
        let temporary = TempDir::new().unwrap();
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([
                AssistantResponse {
                    content: String::new(),
                    tool_calls: vec![shell_call("a", "true")],
                },
                AssistantResponse {
                    content: "ok".into(),
                    tool_calls: Vec::new(),
                },
            ]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let mut history = Vec::new();
        run_agent_turn(
            &mut adapter,
            &mut history,
            "hello".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Off),
            &mut observer,
        )
        .await
        .unwrap();
        assert!(!kinds(&observer.events).contains(&"approval"));
        assert!(observer.events.iter().any(|event| matches!(
            event,
            ExecutionEvent::ToolDecision {
                decision: ApprovalDecision::Deny,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn history_is_compacted_after_a_successful_turn_only() {
        let temporary = TempDir::new().unwrap();
        let big = "z".repeat(400);
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([
                AssistantResponse {
                    content: big.clone(),
                    tool_calls: Vec::new(),
                },
                AssistantResponse {
                    content: big.clone(),
                    tool_calls: Vec::new(),
                },
                AssistantResponse {
                    content: big.clone(),
                    tool_calls: Vec::new(),
                },
            ]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let policy = TurnPolicy::new(ApprovalPolicy::Ask).with_compaction(&CompactionConfig {
            max_history_bytes: Some(1000),
        });
        let mut history = vec![ChatMessage::system("sys")];
        for n in 0..3 {
            run_agent_turn(
                &mut adapter,
                &mut history,
                format!("q{n}"),
                temporary.path(),
                &policy,
                &mut observer,
            )
            .await
            .unwrap();
        }
        // Three 400-byte answers exceed 1000; the oldest turn(s) must go, the
        // system prompt and the latest turn must stay.
        assert!(crate::history_bytes(&history) <= 1000);
        assert_eq!(history[0].role, xshell_core::MessageRole::System);
        assert!(history.iter().any(|m| m.content == "q2"));
        assert!(!history.iter().any(|m| m.content == "q0"));
        let compactions = observer
            .events
            .iter()
            .filter(|e| matches!(e, ExecutionEvent::HistoryCompacted { .. }))
            .count();
        assert!(compactions >= 1);

        // A failing turn (script exhausted) rolls back and does not compact.
        let events_before = observer.events.len();
        let history_before = history.clone();
        assert!(
            run_agent_turn(
                &mut adapter,
                &mut history,
                "q3".into(),
                temporary.path(),
                &policy,
                &mut observer,
            )
            .await
            .is_err()
        );
        assert_eq!(history, history_before);
        assert!(
            !observer.events[events_before..]
                .iter()
                .any(|e| matches!(e, ExecutionEvent::HistoryCompacted { .. }))
        );
    }

    #[tokio::test]
    async fn oversized_restored_history_is_compacted_before_the_first_request() {
        let temporary = TempDir::new().unwrap();
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([AssistantResponse {
                content: "recovered".into(),
                tool_calls: Vec::new(),
            }]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let policy = TurnPolicy::new(ApprovalPolicy::Ask).with_compaction(&CompactionConfig {
            max_history_bytes: Some(100),
        });
        let mut history = vec![ChatMessage::system("sys")];
        for n in 0..3 {
            history.push(ChatMessage::user(format!("old question {n}")));
            history.push(ChatMessage::assistant_with_tools(
                format!("old answer {n} {}", "x".repeat(200)),
                Vec::new(),
            ));
        }

        run_agent_turn(
            &mut adapter,
            &mut history,
            "new question".into(),
            temporary.path(),
            &policy,
            &mut observer,
        )
        .await
        .unwrap();

        let sent = &adapter.requests[0].messages;
        assert!(crate::history_bytes(sent) <= 100);
        assert!(sent.iter().any(|message| message.content == "new question"));
        assert!(
            !sent
                .iter()
                .any(|message| message.content.starts_with("old question"))
        );
        assert!(
            observer
                .events
                .iter()
                .any(|event| matches!(event, ExecutionEvent::HistoryCompacted { .. }))
        );
    }

    #[tokio::test]
    async fn failed_first_request_restores_history_after_request_compaction() {
        let temporary = TempDir::new().unwrap();
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::new(),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let policy = TurnPolicy::new(ApprovalPolicy::Ask).with_compaction(&CompactionConfig {
            max_history_bytes: Some(10),
        });
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old question"),
            ChatMessage::assistant_with_tools("x".repeat(200), Vec::new()),
        ];
        let before = history.clone();

        assert!(
            run_agent_turn(
                &mut adapter,
                &mut history,
                "new question".into(),
                temporary.path(),
                &policy,
                &mut observer,
            )
            .await
            .is_err()
        );

        assert_eq!(history, before);
        assert!(
            !observer
                .events
                .iter()
                .any(|event| matches!(event, ExecutionEvent::HistoryCompacted { .. }))
        );
    }

    #[tokio::test]
    async fn f1_provider_failure_preserves_completed_tool_activity() {
        let temporary = TempDir::new().unwrap();
        let marker = temporary.path().join("marker");
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([AssistantResponse {
                content: String::new(),
                tool_calls: vec![shell_call(
                    "effect",
                    &format!("printf done > {}", marker.display()),
                )],
            }]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: false,
        };
        let mut history = vec![ChatMessage::system("test")];

        let error = run_agent_turn(
            &mut adapter,
            &mut history,
            "mutate".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Auto),
            &mut observer,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("script exhausted"));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "done");
        assert!(history.iter().any(|message| {
            message.tool_call_id.as_deref() == Some("effect")
                && message.content.contains("exit status")
        }));
        assert!(history.last().unwrap().content.contains("provider failed"));
        assert_eq!(adapter.requests.len(), 2);
        assert!(
            adapter.requests[1]
                .messages
                .iter()
                .any(|message| { message.tool_call_id.as_deref() == Some("effect") })
        );

        let mut resumed = ScriptedAdapter {
            responses: VecDeque::from([AssistantResponse {
                content: "reconciled".into(),
                tool_calls: Vec::new(),
            }]),
            requests: Vec::new(),
        };
        run_agent_turn(
            &mut resumed,
            &mut history,
            "continue safely".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Auto),
            &mut observer,
        )
        .await
        .unwrap();
        assert!(
            resumed.requests[0]
                .messages
                .iter()
                .any(|message| { message.tool_call_id.as_deref() == Some("effect") })
        );
        assert!(
            resumed.requests[0]
                .messages
                .iter()
                .any(|message| message.content.contains("provider failed"))
        );
    }

    #[tokio::test]
    async fn cancellation_wait_wakes_promptly_from_another_thread() {
        let flag = CancellationFlag::default();
        let remote = flag.clone();
        // A plain std thread with no tokio context, as the daemon's request
        // handler threads are.
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            remote.cancel();
        });
        tokio::time::timeout(Duration::from_secs(2), flag.wait())
            .await
            .expect("wait must resolve after cancel");
        assert!(flag.is_cancelled());
        // Well under any polling interval would have allowed; proves we are
        // event-driven rather than sleeping in a loop.
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn cancellation_before_wait_resolves_immediately() {
        let flag = CancellationFlag::default();
        flag.cancel();
        tokio::time::timeout(Duration::from_millis(10), flag.wait())
            .await
            .expect("already-cancelled flag must not block");
    }

    #[tokio::test]
    async fn cancellation_while_recording_a_decision_prevents_tool_execution() {
        let temporary = TempDir::new().unwrap();
        let marker = temporary.path().join("must-not-exist");
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([AssistantResponse {
                content: String::new(),
                tool_calls: vec![shell_call(
                    "a",
                    &format!("touch {}", marker.to_string_lossy()),
                )],
            }]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation: CancellationFlag::default(),
            cancel_on_decision: true,
        };
        let mut history = Vec::new();

        let error = run_agent_turn(
            &mut adapter,
            &mut history,
            "hello".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Auto),
            &mut observer,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("before tool execution"));
        assert!(!marker.exists());
        assert!(history.is_empty(), "cancelled turn must roll back history");
    }

    #[tokio::test]
    async fn f2_cancellation_kills_agent_shell_pipeline_before_delayed_effect() {
        let temporary = TempDir::new().unwrap();
        let started = temporary.path().join("started");
        let delayed = temporary.path().join("delayed");
        let command = format!(
            "printf ready > {}; (sleep 0.4; printf late > {}) & wait",
            started.display(),
            delayed.display()
        );
        let cancellation = CancellationFlag::default();
        let cancel = cancellation.clone();
        let started_for_task = started.clone();
        let signal = tokio::spawn(async move {
            while !started_for_task.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.cancel();
        });
        let mut adapter = ScriptedAdapter {
            responses: VecDeque::from([AssistantResponse {
                content: String::new(),
                tool_calls: vec![shell_call("cancelled", &command)],
            }]),
            requests: Vec::new(),
        };
        let mut observer = RecordingObserver {
            events: Vec::new(),
            decisions: VecDeque::new(),
            cancellation,
            cancel_on_decision: false,
        };
        let mut history = Vec::new();

        let error = run_agent_turn(
            &mut adapter,
            &mut history,
            "start work".into(),
            temporary.path(),
            &TurnPolicy::new(ApprovalPolicy::Auto),
            &mut observer,
        )
        .await
        .unwrap_err();
        signal.await.unwrap();
        assert!(
            error
                .to_string()
                .contains("cancelled during tool execution")
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !delayed.exists(),
            "F2 delayed agent-tool effect survived cancellation"
        );
        assert!(history.iter().any(|message| {
            message.tool_call_id.as_deref() == Some("cancelled")
                && message.content.contains("killed and reaped")
        }));
    }

    #[tokio::test]
    async fn f2_cancellation_kills_captured_shell_pipeline_before_delayed_effect() {
        let temporary = TempDir::new().unwrap();
        let started = temporary.path().join("started");
        let delayed = temporary.path().join("delayed");
        let command = format!(
            "printf ready > {}; (sleep 0.4; printf late > {}) & wait",
            started.display(),
            delayed.display()
        );
        let cancellation = CancellationFlag::default();
        let cancel = cancellation.clone();
        let started_for_task = started.clone();
        let signal = tokio::spawn(async move {
            while !started_for_task.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.cancel();
        });

        let error = run_direct_shell_streaming_cancellable(
            &command,
            temporary.path(),
            cancellation,
            |_, _| {},
        )
        .await
        .unwrap_err();
        signal.await.unwrap();
        assert!(error.to_string().contains("killed and reaped"));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !delayed.exists(),
            "F2 delayed captured-shell effect survived cancellation"
        );
    }

    #[tokio::test]
    async fn direct_shell_streams_output_and_tracks_cd() {
        let temporary = TempDir::new().unwrap();
        let mut chunks = Vec::new();
        let result = run_direct_shell_streaming(
            "printf first; sleep 0.05; printf second",
            temporary.path(),
            |stream, text| chunks.push((stream.to_owned(), text)),
        )
        .await
        .unwrap();
        assert_eq!(result.stdout, "firstsecond");
        assert_eq!(chunks.len(), 2);

        let child = temporary.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let changed = run_direct_shell("cd child", temporary.path())
            .await
            .unwrap();
        assert_eq!(changed.cwd, child.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn f11_cd_rejects_files_and_compound_commands_stay_in_the_shell() {
        let temporary = TempDir::new().unwrap();
        let file = temporary.path().join("file");
        std::fs::write(&file, "text").unwrap();
        let error = run_direct_shell("cd file", temporary.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is not a directory"));

        let child = temporary.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let result = run_direct_shell("cd child && pwd", temporary.path())
            .await
            .unwrap();
        assert_eq!(result.cwd, temporary.path());
        assert_eq!(
            Path::new(result.stdout.trim()),
            child.canonicalize().unwrap()
        );
    }
}
