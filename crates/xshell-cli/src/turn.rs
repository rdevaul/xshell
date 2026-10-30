//! Agent turns: the local `TurnObserver`, daemon event following, approval prompts, and tool-result display.

use crate::audit::AuditRuntime;
use crate::completion::XshellHelper;
use crate::config::ActiveModel;
use crate::model::*;
use crate::session::SessionRuntime;
use crate::sessions_ui::*;
use crate::tools;
use anyhow::{Context, Result, bail};
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use xshell_adapters::AgentAdapter;
use xshell_audit::AuditEvent;
use xshell_core::{ChatMessage, ToolCall};
use xshell_execution::{
    ApprovalDecision, CancellationFlag, ExecutionEvent, GateReason, Tool, TurnObserver, TurnPolicy,
    tool_summary,
};
use xshell_session::{SessionEventKind, SessionSnapshot};
use xshell_view::{AgentRenderer, RenderOptions, escape_for_prompt, sanitize_terminal_text};

pub(crate) enum DaemonTurnOutcome {
    Completed(Box<SessionSnapshot>),
    Focus(xshell_pty::ControllerAction),
}

pub(crate) fn follow_daemon_turn(
    sessions: &mut SessionRuntime,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
    escape_prefix: u8,
) -> Result<DaemonTurnOutcome> {
    let mut renderer = AgentRenderer::new(render_options);
    let mut stdout = io::stdout();
    let mut shell_finished: Option<(String, String)> = None;
    let mut controller = xshell_pty::ControllerInput::acquire(escape_prefix)?;
    loop {
        if let Some(action) = poll_controller_action(&mut controller)? {
            match action {
                xshell_pty::ControllerAction::Help => {
                    print_controller_help()?;
                }
                xshell_pty::ControllerAction::Terminate => {
                    sessions.stop_current_activity()?;
                }
                action => {
                    renderer.finish(&mut stdout)?;
                    return Ok(DaemonTurnOutcome::Focus(action));
                }
            }
        }
        let batch = sessions.events(if controller.is_some() { 50 } else { 1_000 })?;
        if let Some(sequence) = batch.truncated_before {
            eprintln!("xshell: session event replay was truncated before sequence {sequence}");
        }
        if batch.events.is_empty() && batch.active_turn_id.is_none() {
            bail!("session turn ended without a terminal event");
        }
        for record in batch.events {
            let sequence = record.sequence;
            match record.event {
                SessionEventKind::TurnStarted {
                    approval,
                    requested_approval,
                    ..
                } => {
                    if let Some(requested) = requested_approval {
                        eprintln!(
                            "xshell: session host limits approval to \"{approval}\"; \
requested \"{requested}\" was not applied"
                        );
                    }
                }
                SessionEventKind::Execution { event } => {
                    // A subagent's events are shown tagged with its name. Its
                    // text is not streamed: its final answer reaches the
                    // terminal as the delegate_task tool result.
                    let (agent, event) = event.into_innermost();
                    let tag = agent_tag(agent.as_deref());
                    match event {
                        ExecutionEvent::TextDelta { text } => {
                            if agent.is_none() {
                                renderer.push(&text, &mut stdout)?;
                            }
                        }
                        ExecutionEvent::AgentResponse {
                            content,
                            tool_call_count,
                            partial,
                            reasoning,
                            finish_reason,
                        } => {
                            if agent.is_none() {
                                if !renderer.received_delta() && !content.is_empty() {
                                    renderer.push(&content, &mut stdout)?;
                                }
                                renderer.finish(&mut stdout)?;
                                renderer = AgentRenderer::new(render_options);
                            }
                            warn_if_truncated(finish_reason.as_deref());
                            audit.append_execution(AuditEvent::AgentResponse {
                                content,
                                tool_call_count,
                                partial,
                                reasoning,
                                finish_reason,
                            })?;
                        }
                        ExecutionEvent::AutoContinued { reason, prompt } => {
                            print_auto_continue(&reason);
                            audit.append_execution(AuditEvent::AutoContinued { reason, prompt })?;
                        }
                        ExecutionEvent::ToolRequested { call } => {
                            println!(
                                "{tag}agent requests: {}",
                                escape_for_prompt(&tool_summary(&call))
                            );
                            audit.append_execution(AuditEvent::ToolRequested {
                                call_id: call.id,
                                name: call.name,
                                arguments: call.arguments,
                            })?;
                        }
                        ExecutionEvent::ApprovalRequested { call, reason } => {
                            print!("{tag}");
                            match confirm_daemon_tool(
                                &call,
                                reason.unwrap_or(GateReason::ShellExecution),
                                &mut controller,
                            )? {
                                ApprovalPromptOutcome::Decision(decision) => {
                                    sessions.approve(
                                        record.turn_id.clone(),
                                        call.id.clone(),
                                        decision,
                                    )?;
                                }
                                ApprovalPromptOutcome::Focus(action) => {
                                    renderer.finish(&mut stdout)?;
                                    return Ok(DaemonTurnOutcome::Focus(action));
                                }
                            }
                        }
                        ExecutionEvent::ToolDecision { call_id, decision } => {
                            audit.append_execution(AuditEvent::ToolDecision {
                                call_id,
                                decision: approval_decision_name(decision).into(),
                            })?;
                        }
                        ExecutionEvent::ToolSkipped { call_id, .. } => {
                            audit.append_execution(AuditEvent::ToolDecision {
                                call_id,
                                decision: "skipped_after_abort".into(),
                            })?;
                        }
                        ExecutionEvent::ToolResult {
                            call_id,
                            name,
                            result,
                        } => {
                            audit.append_execution(AuditEvent::ToolResult {
                                call_id,
                                name,
                                result: result.clone(),
                            })?;
                            print!("{tag}");
                            print_tool_result(&result);
                        }
                        ExecutionEvent::TurnAborted => {
                            println!("{tag}agent turn aborted; no remaining tools were executed");
                        }
                        ExecutionEvent::HistoryCompacted { report } => {
                            print_compaction(&report);
                            audit.append_execution(compaction_audit_event(&report))?;
                        }
                        ExecutionEvent::SubagentStarted { agent, task, tools } => {
                            print_subagent_started(&agent, &task, &tools);
                            audit.append_execution(AuditEvent::SubagentStarted {
                                agent,
                                task,
                                tools,
                            })?;
                        }
                        ExecutionEvent::SubagentFinished {
                            agent,
                            outcome,
                            message,
                        } => {
                            print_subagent_finished(&agent, &outcome, message.as_deref());
                            audit.append_execution(AuditEvent::SubagentFinished {
                                agent,
                                outcome,
                                message,
                            })?;
                        }
                        // Removed by `into_innermost` above.
                        ExecutionEvent::Subagent { .. } => {}
                    }
                }
                SessionEventKind::ShellOutput { stream, text } => {
                    if stream == "stderr" {
                        eprint!("{text}");
                        io::stderr().flush()?;
                    } else {
                        write!(stdout, "{text}")?;
                        stdout.flush()?;
                    }
                }
                SessionEventKind::WorkingDirectoryChanged { cwd } => {
                    audit.append_execution(AuditEvent::WorkingDirectoryChanged {
                        cwd: cwd.display().to_string(),
                    })?;
                }
                SessionEventKind::ShellFinished { command, status } => {
                    if status != "exit status: 0" && status != "working directory changed" {
                        eprintln!("xshell: command finished with {status}");
                    }
                    shell_finished = Some((command, status));
                }
                SessionEventKind::TurnCompleted => {
                    sessions.mark_turn_finished();
                    renderer.finish(&mut stdout)?;
                    sessions.acknowledge_event(sequence)?;
                    let snapshot = sessions.refresh_snapshot()?;
                    if let Some((command, status)) = shell_finished.take() {
                        audit.append_execution(AuditEvent::ShellFinished {
                            command,
                            outcome: status,
                            cwd: snapshot.descriptor.cwd.display().to_string(),
                        })?;
                    }
                    return Ok(DaemonTurnOutcome::Completed(Box::new(snapshot)));
                }
                SessionEventKind::TurnFailed { message } => {
                    sessions.mark_turn_finished();
                    renderer.finish(&mut stdout)?;
                    sessions.acknowledge_event(sequence)?;
                    bail!("{message}");
                }
                SessionEventKind::TurnCancelled => {
                    sessions.mark_turn_finished();
                    renderer.finish(&mut stdout)?;
                    sessions.acknowledge_event(sequence)?;
                    bail!("session turn was cancelled");
                }
            }
            sessions.acknowledge_event(sequence)?;
        }
    }
}

fn poll_controller_action(
    controller: &mut Option<xshell_pty::ControllerInput>,
) -> Result<Option<xshell_pty::ControllerAction>> {
    let Some(controller) = controller else {
        return Ok(None);
    };
    let (ignored, action) = controller.read(Duration::ZERO)?;
    if !ignored.is_empty() {
        // Structured agent output has no byte stream to receive ordinary
        // keystrokes. Ring the terminal bell instead of silently treating
        // them as agent or shell input.
        print!("\x07");
        io::stdout().flush()?;
    }
    Ok(action)
}

pub(crate) fn print_controller_help() -> Result<()> {
    print!(
        "\r\n[xshell: d detach | s switch | l last | n/p next/previous | q stop activity | ? help]\r\n"
    );
    io::stdout().flush()?;
    Ok(())
}

enum ApprovalPromptOutcome {
    Decision(ApprovalDecision),
    Focus(xshell_pty::ControllerAction),
}

fn confirm_daemon_tool(
    call: &ToolCall,
    reason: GateReason,
    controller: &mut Option<xshell_pty::ControllerInput>,
) -> Result<ApprovalPromptOutcome> {
    let Some(controller) = controller else {
        return confirm_tool(call, reason).map(ApprovalPromptOutcome::Decision);
    };
    let why = match reason {
        GateReason::ShellExecution => String::new(),
        GateReason::SensitivePath => " (matches sensitive-path policy)".to_owned(),
    };
    print!(
        "Approve `{}`{why}? [y/N/q] ",
        escape_for_prompt(&tools::summary(call))
    );
    io::stdout().flush()?;
    loop {
        let (bytes, action) = controller.read(Duration::from_millis(100))?;
        if let Some(action) = action {
            match action {
                xshell_pty::ControllerAction::Help => print_controller_help()?,
                xshell_pty::ControllerAction::Terminate => {
                    println!("q");
                    return Ok(ApprovalPromptOutcome::Decision(ApprovalDecision::AbortTurn));
                }
                action => {
                    println!();
                    return Ok(ApprovalPromptOutcome::Focus(action));
                }
            }
        }
        for byte in bytes {
            let decision = match byte.to_ascii_lowercase() {
                b'y' => Some(ApprovalDecision::Approve),
                b'n' | b'\r' | b'\n' => Some(ApprovalDecision::Deny),
                b'q' => Some(ApprovalDecision::AbortTurn),
                _ => None,
            };
            if let Some(decision) = decision {
                println!(
                    "{}",
                    match decision {
                        ApprovalDecision::Approve => "y",
                        ApprovalDecision::Deny => "n",
                        ApprovalDecision::AbortTurn => "q",
                    }
                );
                return Ok(ApprovalPromptOutcome::Decision(decision));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_runtime_snapshot(
    snapshot: SessionSnapshot,
    active_model: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    default_system_prompt: &str,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
    daemon_owned: bool,
) -> Result<()> {
    restore_session_state(snapshot, active_model, cwd, history, default_system_prompt)?;
    *agent = build_adapter(active_model, !daemon_owned)?;
    if let Some(helper) = editor.helper_mut() {
        helper.set_cwd(cwd.clone());
    }
    Ok(())
}

/// Observer for turns executed in-process (no session daemon). It renders
/// streamed output, prompts for approvals, and appends audit events exactly
/// as `follow_daemon_turn` does for daemon-executed turns, so both paths
/// share `xshell_execution::run_agent_turn` and cannot drift apart.
pub(crate) struct LocalObserver<'a> {
    pub(crate) audit: &'a mut AuditRuntime,
    pub(crate) render_options: RenderOptions,
    pub(crate) renderer: AgentRenderer,
    pub(crate) cwd: &'a Path,
    pub(crate) cancellation: CancellationFlag,
    /// Whether the most recent `ToolDecision` approved execution, so the
    /// read-only policy note is printed only for tools that actually ran.
    pub(crate) last_approved: bool,
    /// Whether the most recent tool call went through an approval prompt;
    /// the automatic-policy note is only printed when it did not.
    pub(crate) last_prompted: bool,
    /// First error raised while rendering or auditing. The engine's observer
    /// interface is infallible, so failures are captured here and surfaced
    /// after the turn returns.
    pub(crate) failure: Option<anyhow::Error>,
    /// The subagent whose approval `approve` is about to ask for, so the
    /// prompt can name it.
    pub(crate) approval_agent: Option<String>,
}

impl<'a> LocalObserver<'a> {
    fn new(audit: &'a mut AuditRuntime, render_options: RenderOptions, cwd: &'a Path) -> Self {
        Self {
            audit,
            render_options,
            renderer: AgentRenderer::new(render_options),
            cwd,
            cancellation: CancellationFlag::default(),
            last_approved: false,
            last_prompted: false,
            failure: None,
            approval_agent: None,
        }
    }

    fn record(&mut self, result: Result<()>) {
        if let Err(error) = result
            && self.failure.is_none()
        {
            self.failure = Some(error);
            self.cancellation.cancel();
        }
    }

    fn finish(mut self, outcome: Result<()>) -> Result<()> {
        let flush = self
            .renderer
            .finish(&mut io::stdout())
            .context("could not render agent response");
        self.record(flush);
        match (outcome, self.failure) {
            (_, Some(error)) => Err(error),
            (Err(error), None) => Err(error),
            (Ok(()), None) => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl TurnObserver for LocalObserver<'_> {
    fn emit(&mut self, event: ExecutionEvent) {
        let mut stdout = io::stdout();
        // Shown as in `follow_daemon_turn`: tagged, with subagent text left
        // to arrive as the delegate_task tool result.
        let (agent, event) = event.into_innermost();
        let tag = agent_tag(agent.as_deref());
        let result: Result<()> = match event {
            ExecutionEvent::TextDelta { .. } if agent.is_some() => Ok(()),
            ExecutionEvent::TextDelta { text } => self
                .renderer
                .push(&text, &mut stdout)
                .context("could not render agent response"),
            ExecutionEvent::AgentResponse {
                content,
                tool_call_count,
                partial,
                reasoning,
                finish_reason,
            } => {
                let mut result = Ok(());
                if agent.is_none() {
                    if !self.renderer.received_delta() && !content.is_empty() {
                        result = self
                            .renderer
                            .push(&content, &mut stdout)
                            .context("could not render agent response");
                    }
                    result = result.and(
                        self.renderer
                            .finish(&mut stdout)
                            .context("could not render agent response"),
                    );
                    self.renderer = AgentRenderer::new(self.render_options);
                    if !partial && tool_call_count == 0 {
                        println!();
                    }
                }
                warn_if_truncated(finish_reason.as_deref());
                result.and(self.audit.append(AuditEvent::AgentResponse {
                    content,
                    tool_call_count,
                    partial,
                    reasoning,
                    finish_reason,
                }))
            }
            ExecutionEvent::AutoContinued { reason, prompt } => {
                print_auto_continue(&reason);
                self.audit
                    .append(AuditEvent::AutoContinued { reason, prompt })
            }
            ExecutionEvent::ToolRequested { call } => {
                self.last_prompted = false;
                println!(
                    "\n{tag}agent requests: {}",
                    escape_for_prompt(&tool_summary(&call))
                );
                self.audit.append(AuditEvent::ToolRequested {
                    call_id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                })
            }
            // The prompt itself is issued from `approve`; nothing to echo.
            ExecutionEvent::ApprovalRequested { .. } => {
                self.last_prompted = true;
                self.approval_agent = agent;
                Ok(())
            }
            ExecutionEvent::ToolDecision { call_id, decision } => {
                self.last_approved = decision == ApprovalDecision::Approve;
                self.audit.append(AuditEvent::ToolDecision {
                    call_id,
                    decision: approval_decision_name(decision).into(),
                })
            }
            ExecutionEvent::ToolSkipped { call_id, .. } => {
                self.audit.append(AuditEvent::ToolDecision {
                    call_id,
                    decision: "skipped_after_abort".into(),
                })
            }
            ExecutionEvent::ToolResult {
                call_id,
                name,
                result,
            } => {
                // Only for tools that are read-only: not run_shell, and not
                // delegate_task, whose subagent made its own decisions.
                if self.last_approved
                    && !self.last_prompted
                    && Tool::from_name(&name).is_some_and(Tool::is_read_only)
                {
                    println!(
                        "policy: allowed read-only tool within {}",
                        self.cwd.display()
                    );
                }
                let appended = self.audit.append(AuditEvent::ToolResult {
                    call_id,
                    name,
                    result: result.clone(),
                });
                print!("{tag}");
                print_tool_result(&result);
                appended
            }
            ExecutionEvent::TurnAborted => {
                println!("{tag}agent turn aborted; no remaining tools were executed\n");
                Ok(())
            }
            ExecutionEvent::HistoryCompacted { report } => {
                print_compaction(&report);
                self.audit.append(compaction_audit_event(&report))
            }
            ExecutionEvent::SubagentStarted { agent, task, tools } => {
                print_subagent_started(&agent, &task, &tools);
                self.audit
                    .append(AuditEvent::SubagentStarted { agent, task, tools })
            }
            ExecutionEvent::SubagentFinished {
                agent,
                outcome,
                message,
            } => {
                print_subagent_finished(&agent, &outcome, message.as_deref());
                self.audit.append(AuditEvent::SubagentFinished {
                    agent,
                    outcome,
                    message,
                })
            }
            // Removed by `into_innermost` above.
            ExecutionEvent::Subagent { .. } => Ok(()),
        };
        self.record(result);
    }

    fn cancellation(&self) -> CancellationFlag {
        self.cancellation.clone()
    }

    async fn approve(&mut self, call: &ToolCall, reason: GateReason) -> ApprovalDecision {
        if let Some(agent) = self.approval_agent.take() {
            print!("{}", agent_tag(Some(&agent)));
        }
        match confirm_tool(call, reason) {
            Ok(decision) => decision,
            Err(error) => {
                self.record(Err(error));
                ApprovalDecision::AbortTurn
            }
        }
    }
}

pub(crate) async fn run_agent_turn(
    agent: &mut dyn AgentAdapter,
    history: &mut Vec<ChatMessage>,
    message: String,
    cwd: &Path,
    policy: &TurnPolicy,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
) -> Result<()> {
    let mut observer = LocalObserver::new(audit, render_options, cwd);
    let outcome =
        xshell_execution::run_agent_turn(agent, history, message, cwd, policy, &mut observer).await;
    observer.finish(outcome)
}

/// The session-wide compaction default, overridden by the active model's own
/// budget when its profile sets one.
pub(crate) fn turn_policy_for(
    base: &TurnPolicy,
    session_default: &xshell_execution::CompactionConfig,
    active: &ActiveModel,
) -> TurnPolicy {
    base.clone()
        .with_compaction(&session_default.for_model(xshell_execution::ModelBudget {
            max_history_bytes: active.max_history_bytes,
            context_tokens: active.context_tokens,
            reserve_output_tokens: active.reserve_output_tokens,
        }))
}

/// The model's output hit the provider's token limit; the turn ends there.
pub(crate) const TRUNCATED_RESPONSE_NOTICE: &str =
    "xshell: the model's response was cut off by the provider's output token limit";

pub(crate) fn warn_if_truncated(finish_reason: Option<&str>) {
    if finish_reason == Some("length") {
        eprintln!("{TRUNCATED_RESPONSE_NOTICE}");
    }
}

/// A prefix naming the subagent an event came from; empty for the root agent.
fn agent_tag(agent: Option<&str>) -> String {
    agent.map_or_else(String::new, |agent| {
        format!("[{}] ", sanitize_terminal_text(agent))
    })
}

pub(crate) fn print_subagent_started(agent: &str, task: &str, tools: &[String]) {
    let tools = if tools.is_empty() {
        "no tools".to_owned()
    } else {
        tools.join(", ")
    };
    // The task is model-written; escape it like a tool summary.
    println!(
        "\n{}subagent started ({tools}): {}",
        agent_tag(Some(agent)),
        escape_for_prompt(task.lines().next().unwrap_or_default())
    );
}

pub(crate) fn print_subagent_finished(agent: &str, outcome: &str, message: Option<&str>) {
    match message {
        Some(message) => println!(
            "{}subagent {outcome}: {}",
            agent_tag(Some(agent)),
            sanitize_terminal_text(message)
        ),
        None => println!("{}subagent {outcome}", agent_tag(Some(agent))),
    }
}

pub(crate) fn print_auto_continue(reason: &str) {
    eprintln!("{}", format_auto_continue(reason));
}

pub(crate) fn format_auto_continue(reason: &str) -> String {
    format!("xshell: model stopped mid-task ({reason}); asking it to continue")
}

pub(crate) fn print_compaction(report: &xshell_execution::CompactionReport) {
    eprintln!("{}", format_compaction(report));
}

pub(crate) fn format_compaction(report: &xshell_execution::CompactionReport) -> String {
    // `turns_removed == 0` with a size change means oversized tool results
    // were elided from the current turn instead of whole turns being dropped.
    let action = if report.turns_removed > 0 {
        format!("dropped {} older turn(s)", report.turns_removed)
    } else {
        "elided oversized tool results".to_owned()
    };
    format!(
        "xshell: history compacted ({}): {action}, {} -> {} messages, {} -> {} bytes",
        report.compactor,
        report.messages_before,
        report.messages_after,
        report.bytes_before,
        report.bytes_after
    )
}

pub(crate) fn compaction_audit_event(report: &xshell_execution::CompactionReport) -> AuditEvent {
    AuditEvent::HistoryCompacted {
        compactor: report.compactor.clone(),
        messages_before: report.messages_before,
        messages_after: report.messages_after,
        bytes_before: report.bytes_before,
        bytes_after: report.bytes_after,
        turns_removed: report.turns_removed,
    }
}

pub(crate) fn approval_decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approve => "approve",
        ApprovalDecision::Deny => "deny",
        ApprovalDecision::AbortTurn => "abort_turn",
    }
}

pub(crate) fn confirm_tool(call: &ToolCall, reason: GateReason) -> Result<ApprovalDecision> {
    loop {
        // Tool arguments are model-controlled. Escape them so the command the
        // user approves is exactly the command that will run: no control
        // sequences can redraw the line, and embedded newlines are visible.
        let why = match reason {
            GateReason::ShellExecution => String::new(),
            GateReason::SensitivePath => " (matches sensitive-path policy)".to_owned(),
        };
        print!(
            "Approve `{}`{why}? [y/N/q] ",
            escape_for_prompt(&tools::summary(call))
        );
        io::stdout()
            .flush()
            .context("could not flush approval prompt")?;
        let mut answer = String::new();
        let bytes_read = io::stdin()
            .read_line(&mut answer)
            .context("could not read approval")?;
        if bytes_read == 0 {
            return Ok(ApprovalDecision::AbortTurn);
        }
        if let Some(decision) = parse_approval_response(&answer) {
            return Ok(decision);
        }
        eprintln!("Please answer y (approve), n (deny), or q (abort turn).");
    }
}

pub(crate) fn parse_approval_response(answer: &str) -> Option<ApprovalDecision> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalDecision::Approve),
        "" | "n" | "no" => Some(ApprovalDecision::Deny),
        "q" | "quit" | "abort" => Some(ApprovalDecision::AbortTurn),
        _ => None,
    }
}

pub(crate) fn print_tool_result(result: &str) {
    print!("{}", format_tool_result(result));
}

pub(crate) fn format_tool_result(result: &str) -> String {
    const DISPLAY_LIMIT: usize = 4 * 1024;
    let end = floor_char_boundary(result, result.len().min(DISPLAY_LIMIT));
    // Tool output (file contents, command stdout) is untrusted terminal text.
    let mut text = format!("tool result:\n{}\n", sanitize_terminal_text(&result[..end]));
    if end < result.len() {
        text.push_str("[terminal display truncated; full result returned to agent]\n");
    }
    text
}

pub(crate) fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_prompt_distinguishes_deny_from_abort() {
        assert_eq!(
            parse_approval_response("y"),
            Some(ApprovalDecision::Approve)
        );
        assert_eq!(parse_approval_response(""), Some(ApprovalDecision::Deny));
        assert_eq!(parse_approval_response("no"), Some(ApprovalDecision::Deny));
        assert_eq!(
            parse_approval_response("q"),
            Some(ApprovalDecision::AbortTurn)
        );
        assert_eq!(
            parse_approval_response("abort"),
            Some(ApprovalDecision::AbortTurn)
        );
        assert_eq!(parse_approval_response("maybe"), None);
    }
}
