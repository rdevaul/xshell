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
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy, ExecutionEvent};
use xshell_session::{
    AgentTurnPhase, PersistenceMode, SessionActivity, SessionClient, SessionCreation,
    SessionEventKind, TurnFailureReason, TurnInput, Visibility,
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

    /// Start the turn and exit at once. Follow it later with `xshell wait`.
    #[arg(long, conflicts_with = "ephemeral")]
    pub detach: bool,

    /// Read approval decisions from this file descriptor, one JSON object
    /// per line: {"call_id": ..., "decision": "approve" | "deny" | "abort"}.
    #[arg(long, value_name = "FD", conflicts_with = "detach")]
    pub approval_channel: Option<i32>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,
}

/// Options for `wait`, `status`, `result`, and `cancel`, which act on the
/// session named by `--session`.
#[derive(Debug, Args)]
pub struct JobArgs {
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFormat,

    /// For `wait`: read approval decisions from this file descriptor, as for
    /// `run --approval-channel`.
    #[arg(long, value_name = "FD")]
    pub approval_channel: Option<i32>,
}

/// What to do with a session's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCommand {
    /// Follow the active turn to its end, or report the last one.
    Wait,
    /// Report the session's activity.
    Status,
    /// Report the last finished turn, or that a turn is still running.
    Result,
    /// Cancel the active turn and report how it ended.
    Cancel,
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
    pub approval_policy: Option<PathBuf>,
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
    /// The turn is still running: `result` found it active, or `wait` was
    /// interrupted before it ended.
    Running,
    /// `run --detach` started the turn and left it running.
    Detached,
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
            Self::Running => 7,
            Self::Detached => 0,
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
            Self::Running => "running",
            Self::Detached => "detached",
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
    /// Use against each turn limit, as the daemon counted it.
    usage: Option<xshell_execution::TurnUsage>,
    /// Where the turn's records are in the audit log.
    audit: Option<xshell_session::TurnAuditRef>,
}

fn execute(
    shared: &SharedOptions,
    args: &RunArgs,
    interrupted: &AtomicBool,
    output: &mut Output,
) -> std::result::Result<Outcome, RunError> {
    let config = load_config(shared)?;
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
    let overlay = match &shared.approval_policy {
        Some(path) => {
            let (config, _, warnings) =
                crate::config::load_policy_file(path).or_outcome(Outcome::Usage)?;
            for warning in warnings {
                output.warning(&format!("approval policy: {warning}"));
            }
            Some(config)
        }
        None => None,
    };
    let mut responder = Responder::open(args.approval_channel).or_outcome(Outcome::Usage)?;

    let mut client = connect(&config)?;

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
    .map_err(|error| RunError {
        outcome: refusal_outcome(&error),
        error,
    })?;
    let session_id = snapshot.descriptor.id.clone();

    // Follow events from after anything already in the journal.
    let after = client
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
            overlay,
        )
        .map_err(|error| {
            // A session this run created for a turn that never started is
            // closed again, so refused runs do not use up the daemon's
            // session budget. An ephemeral one goes when the client does.
            if !existing && !args.ephemeral {
                let _ = client.close(Some(session_id.clone()));
            }
            RunError {
                outcome: refusal_outcome(&error),
                error,
            }
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

    if args.detach {
        if shared.approval == ApprovalPolicy::Ask {
            output.warning(
                "a detached turn's approval prompts wait until `xshell wait` or an interactive \
client answers them; use --approval auto or off to avoid that",
            );
        }
        output.event(json!({"event": "detached"}));
        summary.outcome = Some(Outcome::Detached);
        output.result(&summary);
        return Ok(Outcome::Detached);
    }
    let outcome = follow_turn(
        &mut client,
        &session_id,
        &turn_id,
        after,
        &mut summary,
        output,
        interrupted,
        Following::Owner,
        &mut responder,
    )?;
    summary.outcome = Some(outcome);
    output.result(&summary);
    Ok(outcome)
}

fn load_config(shared: &SharedOptions) -> std::result::Result<XshellConfig, RunError> {
    let (config, _) = XshellConfig::load(shared.config.as_deref()).or_outcome(Outcome::Usage)?;
    if !config.session_fabric.enabled {
        return Err(anyhow::anyhow!(
            "headless runs need the session fabric: set session_fabric.enabled = true and start xshelld"
        ))
        .or_outcome(Outcome::Usage);
    }
    Ok(config)
}

fn connect(config: &XshellConfig) -> std::result::Result<SessionClient, RunError> {
    let socket = config
        .session_fabric
        .resolved_socket()
        .context("HOME and XDG_STATE_HOME are not set; configure session_fabric.socket")
        .or_outcome(Outcome::Usage)?;
    SessionClient::connect(&socket, env!("CARGO_PKG_VERSION")).or_outcome(Outcome::Unavailable)
}

/// Run `wait`, `status`, `result`, or `cancel` on the session named by
/// `--session`, and return the exit code.
pub fn job(
    command: JobCommand,
    shared: SharedOptions,
    args: JobArgs,
    interrupted: Arc<AtomicBool>,
) -> i32 {
    let mut output = Output::new(args.output);
    let outcome = match execute_job(
        command,
        &shared,
        args.approval_channel,
        &interrupted,
        &mut output,
    ) {
        Ok(outcome) => outcome,
        Err(RunError { outcome, error }) => {
            output.result(&Summary {
                outcome: Some(outcome),
                message: Some(format!("{error:#}")),
                session: shared.session.clone(),
                ..Summary::default()
            });
            outcome
        }
    };
    let _ = std::io::stdout().flush();
    outcome.exit_code()
}

fn execute_job(
    command: JobCommand,
    shared: &SharedOptions,
    approval_channel: Option<i32>,
    interrupted: &AtomicBool,
    output: &mut Output,
) -> std::result::Result<Outcome, RunError> {
    let selector = shared
        .session
        .clone()
        .context("name the session with --session (`xshell run --detach` prints it)")
        .or_outcome(Outcome::Usage)?;
    let config = load_config(shared)?;
    // Take the channel's descriptor before opening any of our own, so it
    // cannot name a socket this process created.
    let mut responder = Responder::open(approval_channel).or_outcome(Outcome::Usage)?;
    let mut client = connect(&config)?;
    let descriptor = client
        .list()
        .or_outcome(Outcome::Unavailable)?
        .into_iter()
        .find(|session| session.name == selector || session.id == selector)
        .with_context(|| format!("no session named {selector:?}"))
        .or_outcome(Outcome::Usage)?;

    if command == JobCommand::Status {
        let (activity, turn_id) = describe_activity(&descriptor.activity);
        output.status(&descriptor.name, &descriptor.id, activity, turn_id);
        return Ok(Outcome::Completed);
    }

    // Following events, cancelling, and answering prompts need the session
    // to be this connection's current one.
    client
        .attach(descriptor.id.clone())
        .or_outcome(Outcome::Failed)?;
    let journal = client
        .events(descriptor.id.clone(), 0, 0)
        .or_outcome(Outcome::Unavailable)?;
    let active = journal.active_turn_id.clone();
    let turn_id = active
        .clone()
        .or_else(|| journal.events.last().map(|event| event.turn_id.clone()))
        .context("the session has no turn to report")
        .or_outcome(Outcome::Failed)?;
    // Replay the turn from its first event the journal still holds.
    let after = journal
        .events
        .iter()
        .find(|event| event.turn_id == turn_id)
        .map(|event| event.sequence.saturating_sub(1))
        .unwrap_or_else(|| journal.next_sequence.saturating_sub(1));
    let mut summary = Summary {
        session: Some(descriptor.name.clone()),
        session_id: Some(descriptor.id.clone()),
        turn_id: Some(turn_id.clone()),
        ..Summary::default()
    };

    let following = match command {
        JobCommand::Result if active.is_some() => {
            summary.outcome = Some(Outcome::Running);
            output.result(&summary);
            return Ok(Outcome::Running);
        }
        JobCommand::Cancel => {
            if active.is_none() {
                return Err(anyhow::anyhow!("the session has no active turn to cancel"))
                    .or_outcome(Outcome::Failed);
            }
            client
                .cancel(descriptor.id.clone(), turn_id.clone())
                .or_outcome(Outcome::Failed)?;
            output.event(json!({"event": "cancel_requested"}));
            Following::Observer
        }
        JobCommand::Wait if active.is_some() => Following::Waiter,
        _ => Following::Observer,
    };
    let outcome = follow_turn(
        &mut client,
        &descriptor.id,
        &turn_id,
        after,
        &mut summary,
        output,
        interrupted,
        following,
        &mut responder,
    )?;
    summary.outcome = Some(outcome);
    output.result(&summary);
    Ok(outcome)
}

/// A session's activity as a word, and the turn it concerns.
fn describe_activity(activity: &SessionActivity) -> (&'static str, Option<&str>) {
    match activity {
        SessionActivity::Idle => ("idle", None),
        SessionActivity::AgentTurn { turn_id, phase } => (
            match phase {
                AgentTurnPhase::Running => "agent_running",
                AgentTurnPhase::WaitingApproval => "agent_waiting_approval",
                AgentTurnPhase::Stopping => "agent_stopping",
            },
            Some(turn_id),
        ),
        SessionActivity::InteractiveProcess { .. } => ("interactive_process", None),
    }
}

/// How a client follows a turn: what it does on a signal, and whether it
/// answers approval prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Following {
    /// The run that started the turn: a signal cancels it, and prompts are
    /// answered.
    Owner,
    /// `wait`: a signal stops waiting and leaves the turn running; prompts
    /// are answered.
    Waiter,
    /// Replaying a finished turn, or following a cancellation: nothing is
    /// answered.
    Observer,
}

/// Follow `turn_id`'s events after `after` until it ends.
#[allow(clippy::too_many_arguments)]
fn follow_turn(
    client: &mut SessionClient,
    session_id: &str,
    turn_id: &str,
    mut after: u64,
    summary: &mut Summary,
    output: &mut Output,
    interrupted: &AtomicBool,
    following: Following,
    responder: &mut Responder,
) -> std::result::Result<Outcome, RunError> {
    let mut cancel_requested = false;
    let mut last_progress = Instant::now();
    loop {
        if interrupted.load(Ordering::SeqCst) && !cancel_requested {
            match following {
                Following::Waiter => {
                    output.warning("stopped waiting; the turn is still running");
                    return Ok(Outcome::Running);
                }
                Following::Owner => {
                    cancel_requested = true;
                    output.event(json!({"event": "cancel_requested"}));
                    if let Err(error) = client.cancel(session_id.to_owned(), turn_id.to_owned()) {
                        output.warning(&format!("could not request cancellation: {error:#}"));
                    }
                }
                Following::Observer => cancel_requested = true,
            }
        }
        let batch = client
            .events(session_id.to_owned(), after, POLL_WAIT_MS)
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
            let answering = (following != Following::Observer).then_some(&mut *responder);
            if let Some(outcome) = handle_event(
                record.event,
                client,
                session_id,
                turn_id,
                summary,
                output,
                answering.map(|responder| (responder, interrupted)),
            ) {
                terminal = Some(outcome);
            }
        }
        if let Some(outcome) = terminal {
            return Ok(outcome);
        }
        if batch.active_turn_id.as_deref() != Some(turn_id)
            && last_progress.elapsed() > Duration::from_secs(5)
        {
            return Err(anyhow::anyhow!(
                "the turn ended without reporting how; see the session's history"
            ))
            .or_outcome(Outcome::Failed);
        }
    }
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
    answering: Option<(&mut Responder, &AtomicBool)>,
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
            handle_execution(
                event, agent, client, session_id, turn_id, summary, output, answering,
            );
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
        SessionEventKind::TurnSummary { usage, audit } => {
            summary.usage = usage;
            summary.audit = audit;
            None
        }
        // Agent turns produce no shell events; ignore them if one appears.
        SessionEventKind::ShellOutput { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::ShellFinished { .. } => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_execution(
    event: ExecutionEvent,
    agent: Option<String>,
    client: &mut SessionClient,
    session_id: &str,
    turn_id: &str,
    summary: &mut Summary,
    output: &mut Output,
    answering: Option<(&mut Responder, &AtomicBool)>,
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
            // Without a channel nobody can answer, so a prompt is denied and
            // the model is told. A prompt replayed from the journal may
            // already be answered; the daemon then refuses the reply, and
            // the replay reports only what happened.
            let Some((responder, interrupted)) = answering else {
                return;
            };
            let (decision, source) = responder.decide(&call.id, interrupted, output);
            let approver = if source == "approval_channel" {
                xshell_execution::ApprovalSource::new(xshell_execution::Approver::ApprovalChannel)
            } else {
                // Nobody usable answered; the reason code says why.
                xshell_execution::ApprovalSource::new(xshell_execution::Approver::NoApprover)
                    .with_detail(source)
            };
            let sent = client.approve(
                session_id.to_owned(),
                turn_id.to_owned(),
                call.id.clone(),
                decision,
                approver,
            );
            if sent.is_err() {
                return;
            }
            if decision != ApprovalDecision::Approve {
                summary.approvals_denied += 1;
            }
            let decision_name = match decision {
                ApprovalDecision::Approve => "approve",
                ApprovalDecision::Deny => "deny",
                ApprovalDecision::AbortTurn => "abort",
            };
            if source == "no_approval_channel" {
                output.progress(&format!(
                    "denied {} (no approval channel; use --approval auto or --approval-channel)",
                    call.name
                ));
            } else {
                output.progress(&format!("{decision_name} {} ({source})", call.name));
            }
            output.event(tagged(json!({
                "event": "approval_decided",
                "call_id": call.id,
                "decision": decision_name,
                "source": source,
            })));
        }
        ExecutionEvent::ToolDecision {
            call_id,
            decision,
            basis,
            ..
        } => {
            output.event(tagged(json!({
                "event": "tool_decision",
                "call_id": call_id,
                "decision": decision,
                "basis": basis,
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
            images,
        } => {
            output.event(tagged(json!({
                "event": "tool_result",
                "call_id": call_id,
                "name": name,
                "result": result,
                "images": (!images.is_empty()).then_some(images),
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

/// Answers approval prompts: from an approval channel when there is one,
/// otherwise by denying them.
pub struct Responder {
    channel: Option<BufReader<std::fs::File>>,
    closed: bool,
}

impl Responder {
    /// Take ownership of the descriptor `fd`, if given.
    fn open(fd: Option<i32>) -> Result<Self> {
        let channel = match fd {
            None => None,
            Some(fd) => {
                if fd < 3 {
                    bail!("--approval-channel needs a descriptor of 3 or above, not {fd}");
                }
                // SAFETY: fcntl(F_GETFD) only queries the descriptor.
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                    bail!("--approval-channel {fd} is not an open file descriptor");
                }
                // SAFETY: the caller opened `fd` for this process to read
                // decisions from, and nothing else in xshell uses it, so
                // taking ownership cannot close a descriptor in use.
                Some(BufReader::new(unsafe { std::fs::File::from_raw_fd(fd) }))
            }
        };
        Ok(Self {
            channel,
            closed: false,
        })
    }

    /// The decision for the prompt on `call_id`, and where it came from.
    fn decide(
        &mut self,
        call_id: &str,
        interrupted: &AtomicBool,
        output: &mut Output,
    ) -> (ApprovalDecision, &'static str) {
        let Some(channel) = &mut self.channel else {
            return (ApprovalDecision::Deny, "no_approval_channel");
        };
        if self.closed {
            return (ApprovalDecision::Deny, "approval_channel_closed");
        }
        // Wait for a line without blocking signals out: poll the descriptor
        // in short steps unless a line is already buffered.
        while !channel.buffer().contains(&b'\n') {
            if interrupted.load(Ordering::SeqCst) {
                return (ApprovalDecision::Deny, "interrupted");
            }
            let mut poll = libc::pollfd {
                fd: channel.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `poll` is one initialized `pollfd`, matching the count of 1.
            let ready = unsafe { libc::poll(&mut poll, 1, POLL_WAIT_MS as i32) };
            if ready > 0 {
                break;
            }
        }
        let mut line = String::new();
        match channel.read_line(&mut line) {
            Ok(0) | Err(_) => {
                self.closed = true;
                output.warning("the approval channel closed; later prompts are denied");
                return (ApprovalDecision::Deny, "approval_channel_closed");
            }
            Ok(_) => {}
        }
        let reply: Value = match serde_json::from_str(line.trim()) {
            Ok(reply) => reply,
            Err(error) => {
                output.warning(&format!("invalid approval decision {line:?}: {error}"));
                return (ApprovalDecision::Deny, "approval_channel_invalid");
            }
        };
        if reply["call_id"].as_str() != Some(call_id) {
            output.warning(&format!(
                "the approval channel answered {:?}, not the waiting call {call_id:?}; denied",
                reply["call_id"]
            ));
            return (ApprovalDecision::Deny, "approval_channel_mismatch");
        }
        match reply["decision"].as_str() {
            Some("approve") => (ApprovalDecision::Approve, "approval_channel"),
            Some("deny") => (ApprovalDecision::Deny, "approval_channel"),
            Some("abort") => (ApprovalDecision::AbortTurn, "approval_channel"),
            other => {
                output.warning(&format!("unknown approval decision {other:?}; denied"));
                (ApprovalDecision::Deny, "approval_channel_invalid")
            }
        }
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

    fn status(&mut self, session: &str, session_id: &str, activity: &str, turn_id: Option<&str>) {
        match self.format {
            OutputFormat::Jsonl => self.event(json!({
                "event": "status",
                "schema": SCHEMA_VERSION,
                "session": session,
                "session_id": session_id,
                "activity": activity,
                "turn_id": turn_id,
            })),
            OutputFormat::Text => {
                let turn = turn_id
                    .map(|id| format!(" (turn {id})"))
                    .unwrap_or_default();
                println!("{session}: {}{turn}", activity.replace('_', " "));
            }
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
                "usage": summary.usage,
                "audit": summary.audit,
            })),
            OutputFormat::Text if outcome == Outcome::Detached => {
                // The session name is what `xshell wait --session` needs.
                if let Some(session) = &summary.session {
                    println!("{session}");
                }
            }
            OutputFormat::Text => {
                if !summary.answer.is_empty() {
                    let mut stdout = std::io::stdout().lock();
                    let _ = writeln!(stdout, "{}", summary.answer.trim_end());
                }
                match (outcome, summary.message.as_deref()) {
                    (Outcome::Completed, _) => {}
                    (Outcome::Running, _) => eprintln!(
                        "xshell: the turn is still running; `xshell wait --session {}` follows it",
                        summary.session.as_deref().unwrap_or("NAME")
                    ),
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

/// The outcome of a request the daemon refused before a turn started. A
/// spent daemon-wide budget (error code `capacity`) and a required audit that
/// cannot record the input are both "try again later"; anything else failed.
fn refusal_outcome(error: &anyhow::Error) -> Outcome {
    let text = format!("{error:#}");
    if text.contains("(capacity)") || text.contains("required audit") {
        Outcome::Unavailable
    } else {
        Outcome::Failed
    }
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
