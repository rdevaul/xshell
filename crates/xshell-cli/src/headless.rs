//! `xshell run`: one agent turn without a terminal, for scripts.
//!
//! The turn always runs in `xshelld`, which audits it, enforces its approval
//! and limit ceilings, and keeps it running if this client goes away. The
//! client submits the prompt, follows the turn's events, decides approval
//! prompts (nobody is at a terminal to answer them), and reports the outcome
//! as text or as a versioned JSON Lines stream, with a documented exit code.
//! See `docs/headless.md`.

use crate::config::{ModelOverrides, Provider, XshellConfig};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy, ExecutionEvent};
use xshell_session::{
    PersistenceMode, SessionClient, SessionCreation, SessionEventKind, TurnFailureReason,
    TurnInput, Visibility,
};

/// The version of the JSON Lines schema. Bumped on any incompatible change;
/// adding an event type or an optional field is not one.
pub const SCHEMA_VERSION: u32 = 1;

/// How long one event poll waits. Short enough that a signal is noticed
/// promptly; the daemon returns as soon as an event arrives.
const POLL_WAIT_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// The final answer on stdout; progress on stderr.
    Text,
    /// One JSON object per line on stdout (schema in docs/headless.md).
    Jsonl,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The prompt. Read from --prompt-file, or from stdin, when omitted.
    pub prompt: Option<String>,

    /// Read the prompt from this file.
    #[arg(long, conflicts_with = "prompt")]
    pub prompt_file: Option<PathBuf>,

    /// Use a session that will not outlive this run.
    #[arg(long, conflicts_with = "session")]
    pub ephemeral: bool,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,
}

/// The settings `run` shares with interactive xshell.
pub struct SharedOptions {
    pub config: Option<PathBuf>,
    pub profile: Option<String>,
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub system_prompt: String,
    pub session: Option<String>,
    pub cwd: PathBuf,
    pub approval: ApprovalPolicy,
    pub limits: Option<String>,
}

/// How a run ended. Each outcome has one exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    Completed,
    #[default]
    Failed,
    Usage,
    BudgetExhausted,
    Cancelled,
    ApprovalDenied,
    Unavailable,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::Usage => 2,
            Self::BudgetExhausted => 3,
            Self::Cancelled => 4,
            Self::ApprovalDenied => 5,
            Self::Unavailable => 6,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Usage => "usage",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Cancelled => "cancelled",
            Self::ApprovalDenied => "approval_denied",
            Self::Unavailable => "unavailable",
        }
    }
}

/// An error that ends the run before or outside the turn, with its outcome.
struct RunError {
    outcome: Outcome,
    error: anyhow::Error,
}

trait OrOutcome<T> {
    fn or_outcome(self, outcome: Outcome) -> std::result::Result<T, RunError>;
}

impl<T> OrOutcome<T> for Result<T> {
    fn or_outcome(self, outcome: Outcome) -> std::result::Result<T, RunError> {
        self.map_err(|error| RunError { outcome, error })
    }
}

/// Run one turn and return the exit code. Never panics on run failures; they
/// are reported in the chosen format.
pub fn run(shared: SharedOptions, args: RunArgs, interrupted: Arc<AtomicBool>) -> i32 {
    let mut output = Output::new(args.output);
    let outcome = match execute(&shared, &args, &interrupted, &mut output) {
        Ok(outcome) => outcome,
        Err(RunError { outcome, error }) => {
            output.result(&Summary {
                outcome: Some(outcome),
                message: Some(format!("{error:#}")),
                ..Summary::default()
            });
            outcome
        }
    };
    let _ = std::io::stdout().flush();
    outcome.exit_code()
}

#[derive(Default)]
struct Summary {
    outcome: Option<Outcome>,
    message: Option<String>,
    answer: String,
    session: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_calls: usize,
    approvals_denied: usize,
}

fn execute(
    shared: &SharedOptions,
    args: &RunArgs,
    interrupted: &AtomicBool,
    output: &mut Output,
) -> std::result::Result<Outcome, RunError> {
    let (config, _) = XshellConfig::load(shared.config.as_deref()).or_outcome(Outcome::Usage)?;
    if !config.session_fabric.enabled {
        return Err(anyhow::anyhow!(
            "xshell run needs the session fabric: set session_fabric.enabled = true and start xshelld"
        ))
        .or_outcome(Outcome::Usage);
    }
    let active_model = config
        .resolve_startup(
            shared.profile.as_deref(),
            ModelOverrides {
                provider: shared.provider,
                model: shared.model.clone(),
                base_url: shared.base_url.clone(),
                api_key_env: shared.api_key_env.clone(),
            },
        )
        .or_outcome(Outcome::Usage)?;
    let limits_name = config.limits_name_for(shared.limits.as_deref(), &active_model);
    let limits = config
        .limit_profile(&limits_name)
        .or_outcome(Outcome::Usage)?;
    let cwd = xshell_execution::validate_working_directory(&shared.cwd)
        .with_context(|| format!("cannot use working directory {}", shared.cwd.display()))
        .or_outcome(Outcome::Usage)?;
    let prompt = read_prompt(args).or_outcome(Outcome::Usage)?;

    let socket = config
        .session_fabric
        .resolved_socket()
        .context("HOME and XDG_STATE_HOME are not set; configure session_fabric.socket")
        .or_outcome(Outcome::Usage)?;
    let mut client = SessionClient::connect(&socket, env!("CARGO_PKG_VERSION"))
        .or_outcome(Outcome::Unavailable)?;

    let session_name = shared
        .session
        .clone()
        .unwrap_or_else(generated_session_name);
    let existing = client
        .list()
        .or_outcome(Outcome::Unavailable)?
        .into_iter()
        .any(|session| session.name == session_name || session.id == session_name);
    let snapshot = if existing {
        // An existing session keeps its own model and working directory.
        client.attach(session_name.clone())
    } else {
        client.create(SessionCreation {
            name: session_name.clone(),
            model: active_model.to_session_binding(),
            cwd,
            persistence: if args.ephemeral {
                PersistenceMode::Ephemeral
            } else {
                PersistenceMode::Daemon
            },
            visibility: Visibility::Fabric,
            history: vec![ChatMessage::system(shared.system_prompt.clone())],
        })
    }
    .or_outcome(Outcome::Failed)?;
    let session_id = snapshot.descriptor.id.clone();

    // Follow events from after anything already in the journal.
    let mut after = client
        .events(session_id.clone(), 0, 0)
        .or_outcome(Outcome::Unavailable)?
        .next_sequence
        .saturating_sub(1);
    let turn_id = client
        .submit_with_limits(
            session_id.clone(),
            TurnInput::Agent { message: prompt },
            shared.approval,
            Some(limits),
        )
        .map_err(|error| {
            // The daemon refuses the turn outright when required auditing
            // cannot record its input.
            let outcome = if format!("{error:#}").contains("required audit") {
                Outcome::Unavailable
            } else {
                Outcome::Failed
            };
            RunError { outcome, error }
        })?;

    let mut summary = Summary {
        session: Some(snapshot.descriptor.name.clone()),
        session_id: Some(session_id.clone()),
        turn_id: Some(turn_id.clone()),
        ..Summary::default()
    };
    output.event(json!({
        "event": "run_started",
        "schema": SCHEMA_VERSION,
        "session": snapshot.descriptor.name,
        "session_id": session_id,
        "turn_id": turn_id,
        "limits_profile": limits_name,
    }));

    let mut cancel_requested = false;
    let mut last_progress = Instant::now();
    let outcome = loop {
        if interrupted.load(Ordering::SeqCst) && !cancel_requested {
            cancel_requested = true;
            output.event(json!({"event": "cancel_requested"}));
            if let Err(error) = client.cancel(session_id.clone(), turn_id.clone()) {
                output.warning(&format!("could not request cancellation: {error:#}"));
            }
        }
        let batch = client
            .events(session_id.clone(), after, POLL_WAIT_MS)
            .or_outcome(Outcome::Unavailable)?;
        if let Some(sequence) = batch.truncated_before {
            output.warning(&format!(
                "the session's event journal dropped events before sequence {sequence}"
            ));
        }
        let mut terminal = None;
        for record in batch.events {
            after = record.sequence;
            if record.turn_id != turn_id {
                continue;
            }
            last_progress = Instant::now();
            if let Some(outcome) = handle_event(
                record.event,
                &mut client,
                &session_id,
                &turn_id,
                &mut summary,
                output,
            ) {
                terminal = Some(outcome);
            }
        }
        if let Some(outcome) = terminal {
            break outcome;
        }
        if batch.active_turn_id.as_deref() != Some(turn_id.as_str())
            && last_progress.elapsed() > Duration::from_secs(5)
        {
            break Err(anyhow::anyhow!(
                "the turn ended without reporting how; see the session's history"
            ))
            .or_outcome(Outcome::Failed)?;
        }
    };
    summary.outcome = Some(outcome);
    output.result(&summary);
    Ok(outcome)
}

/// Report one event, answer an approval prompt, and return the outcome when
/// the event ends the turn.
fn handle_event(
    event: SessionEventKind,
    client: &mut SessionClient,
    session_id: &str,
    turn_id: &str,
    summary: &mut Summary,
    output: &mut Output,
) -> Option<Outcome> {
    match event {
        SessionEventKind::TurnStarted {
            approval,
            requested_approval,
            limits,
            requested_limits,
            ..
        } => {
            output.event(json!({
                "event": "turn_started",
                "approval": approval.to_string(),
                "requested_approval": requested_approval.map(|value| value.to_string()),
                "limits": limits,
                "requested_limits": requested_limits,
            }));
            if requested_approval.is_some() || requested_limits.is_some() {
                output.warning("the session host lowered this run's approval mode or limits");
            }
            None
        }
        SessionEventKind::Execution { event } => {
            let (agent, event) = event.into_innermost();
            handle_execution(event, agent, client, session_id, turn_id, summary, output);
            None
        }
        SessionEventKind::TurnCompleted => {
            output.event(json!({"event": "turn_completed"}));
            Some(Outcome::Completed)
        }
        SessionEventKind::TurnCancelled => {
            output.event(json!({"event": "turn_cancelled"}));
            Some(Outcome::Cancelled)
        }
        SessionEventKind::TurnFailed { message, reason } => {
            output.event(json!({
                "event": "turn_failed",
                "message": message,
                "reason": reason,
            }));
            summary.message = Some(message);
            Some(match reason {
                Some(TurnFailureReason::BudgetExhausted) => Outcome::BudgetExhausted,
                Some(TurnFailureReason::AuditUnavailable) => Outcome::Unavailable,
                None if summary.approvals_denied > 0 => Outcome::ApprovalDenied,
                None => Outcome::Failed,
            })
        }
        // Agent turns produce no shell events; ignore them if one appears.
        SessionEventKind::ShellOutput { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::ShellFinished { .. } => None,
    }
}

fn handle_execution(
    event: ExecutionEvent,
    agent: Option<String>,
    client: &mut SessionClient,
    session_id: &str,
    turn_id: &str,
    summary: &mut Summary,
    output: &mut Output,
) {
    let tagged = |mut value: Value| {
        if let Some(agent) = &agent {
            value["agent"] = json!(agent);
        }
        value
    };
    match event {
        ExecutionEvent::TextDelta { text } => {
            output.event(tagged(json!({"event": "text", "text": text})));
        }
        ExecutionEvent::AgentResponse {
            content,
            tool_call_count,
            finish_reason,
            ..
        } => {
            if agent.is_none() && !content.trim().is_empty() {
                summary.answer = content.clone();
            }
            output.event(tagged(json!({
                "event": "assistant_message",
                "content": content,
                "tool_calls": tool_call_count,
                "finish_reason": finish_reason,
            })));
        }
        ExecutionEvent::AutoContinued { reason, .. } => {
            output.event(tagged(json!({"event": "auto_continued", "reason": reason})));
        }
        ExecutionEvent::ToolRequested { call } => {
            summary.tool_calls += 1;
            output.progress(&format!("tool: {}", call.name));
            output.event(tagged(json!({
                "event": "tool_requested",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            })));
        }
        ExecutionEvent::ApprovalRequested { call, reason } => {
            output.event(tagged(json!({
                "event": "approval_needed",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
                "reason": reason,
            })));
            // Nobody is at a terminal to answer, so a prompt is denied. The
            // model is told and the turn continues.
            summary.approvals_denied += 1;
            output.progress(&format!(
                "denied {} (no approval channel; use --approval auto to run without prompts)",
                call.name
            ));
            let sent = client.approve(
                session_id.to_owned(),
                turn_id.to_owned(),
                call.id.clone(),
                ApprovalDecision::Deny,
            );
            output.event(tagged(json!({
                "event": "approval_decided",
                "call_id": call.id,
                "decision": "deny",
                "source": "no_approval_channel",
            })));
            if let Err(error) = sent {
                output.warning(&format!("could not send the approval decision: {error:#}"));
            }
        }
        ExecutionEvent::ToolDecision { call_id, decision } => {
            output.event(tagged(json!({
                "event": "tool_decision",
                "call_id": call_id,
                "decision": decision,
            })));
        }
        ExecutionEvent::ToolSkipped { call_id, name } => {
            output.event(tagged(json!({
                "event": "tool_skipped",
                "call_id": call_id,
                "name": name,
            })));
        }
        ExecutionEvent::ToolResult {
            call_id,
            name,
            result,
        } => {
            output.event(tagged(json!({
                "event": "tool_result",
                "call_id": call_id,
                "name": name,
                "result": result,
            })));
        }
        ExecutionEvent::TurnAborted => {
            output.event(tagged(json!({"event": "turn_aborted"})));
        }
        ExecutionEvent::HistoryCompacted { report } => {
            output.event(tagged(
                json!({"event": "history_compacted", "report": report}),
            ));
        }
        ExecutionEvent::SubagentStarted {
            agent: child,
            task,
            tools,
        } => {
            output.progress(&format!("subagent {child} started"));
            output.event(tagged(json!({
                "event": "subagent_started",
                "subagent": child,
                "task": task,
                "tools": tools,
            })));
        }
        ExecutionEvent::SubagentFinished {
            agent: child,
            outcome,
            message,
        } => {
            output.event(tagged(json!({
                "event": "subagent_finished",
                "subagent": child,
                "outcome": outcome,
                "message": message,
            })));
        }
        // `into_innermost` removes these wrappers.
        ExecutionEvent::Subagent { .. } => {}
    }
}

/// Writes events in the chosen format: every event as a JSON line, or the
/// final answer on stdout with progress on stderr.
struct Output {
    format: OutputFormat,
}

impl Output {
    fn new(format: OutputFormat) -> Self {
        Self { format }
    }

    fn event(&mut self, value: Value) {
        if self.format == OutputFormat::Jsonl {
            let mut stdout = std::io::stdout().lock();
            let _ = serde_json::to_writer(&mut stdout, &without_nulls(value));
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        }
    }

    fn progress(&mut self, message: &str) {
        if self.format == OutputFormat::Text {
            eprintln!("xshell: {message}");
        }
    }

    fn warning(&mut self, message: &str) {
        match self.format {
            OutputFormat::Text => eprintln!("xshell: warning: {message}"),
            OutputFormat::Jsonl => self.event(json!({"event": "warning", "message": message})),
        }
    }

    fn result(&mut self, summary: &Summary) {
        let outcome = summary.outcome.unwrap_or_default();
        match self.format {
            OutputFormat::Jsonl => self.event(json!({
                "event": "result",
                "schema": SCHEMA_VERSION,
                "outcome": outcome.name(),
                "exit_code": outcome.exit_code(),
                "answer": summary.answer,
                "message": summary.message,
                "session": summary.session,
                "session_id": summary.session_id,
                "turn_id": summary.turn_id,
                "tool_calls": summary.tool_calls,
                "approvals_denied": summary.approvals_denied,
            })),
            OutputFormat::Text => {
                if !summary.answer.is_empty() {
                    let mut stdout = std::io::stdout().lock();
                    let _ = writeln!(stdout, "{}", summary.answer.trim_end());
                }
                match (outcome, summary.message.as_deref()) {
                    (Outcome::Completed, _) => {}
                    (Outcome::Usage, Some(message)) => eprintln!("xshell: {message}"),
                    (_, message) => eprintln!(
                        "xshell: run {}{}",
                        outcome.name().replace('_', " "),
                        message.map(|text| format!(": {text}")).unwrap_or_default()
                    ),
                }
            }
        }
    }
}

/// Drop `null` fields so optional values are simply absent.
fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, without_nulls(value)))
                .collect(),
        ),
        other => other,
    }
}

fn read_prompt(args: &RunArgs) -> Result<String> {
    let prompt = if let Some(prompt) = &args.prompt {
        prompt.clone()
    } else if let Some(path) = &args.prompt_file {
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read prompt file {}", path.display()))?
    } else if !std::io::stdin().is_terminal() {
        let mut prompt = String::new();
        std::io::stdin()
            .read_to_string(&mut prompt)
            .context("cannot read the prompt from stdin")?;
        prompt
    } else {
        bail!("give a prompt as an argument, with --prompt-file, or on stdin");
    };
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    Ok(prompt)
}

/// A unique, sortable name for a run's own session.
fn generated_session_name() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "run-{}-{:04x}",
        now.as_secs(),
        (now.subsec_nanos() ^ std::process::id()) & 0xffff
    )
}

/// Set `interrupted` on SIGINT or SIGTERM, so the run cancels its turn and
/// still reports how it ended.
pub fn watch_signals(interrupted: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        interrupted.store(true, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        let codes: Vec<_> = [
            Outcome::Completed,
            Outcome::Failed,
            Outcome::Usage,
            Outcome::BudgetExhausted,
            Outcome::Cancelled,
            Outcome::ApprovalDenied,
            Outcome::Unavailable,
        ]
        .into_iter()
        .map(Outcome::exit_code)
        .collect();
        assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn null_fields_are_dropped_recursively() {
        let value = without_nulls(json!({"a": null, "b": 1, "c": {"d": null, "e": 2}}));
        assert_eq!(value, json!({"b": 1, "c": {"e": 2}}));
    }

    #[test]
    fn generated_session_names_are_prefixed() {
        assert!(generated_session_name().starts_with("run-"));
    }
}
