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
use xshell_adapters::AgentAdapter;
use xshell_audit::AuditEvent;
use xshell_core::{ChatMessage, ToolCall};
use xshell_execution::{
    ApprovalDecision, ApprovalPolicy, CancellationFlag, ExecutionEvent, GateReason, TurnObserver,
    TurnPolicy, tool_summary,
};
use xshell_session::{SessionEventKind, SessionSnapshot, TurnInput};
use xshell_view::{AgentRenderer, RenderOptions, escape_for_prompt, sanitize_terminal_text};

pub(crate) fn run_daemon_turn(
    sessions: &mut SessionRuntime,
    input: TurnInput,
    approval: ApprovalPolicy,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
) -> Result<SessionSnapshot> {
    sessions.submit(input, approval)?;
    follow_daemon_turn(sessions, audit, render_options)
}

pub(crate) fn follow_daemon_turn(
    sessions: &mut SessionRuntime,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
) -> Result<SessionSnapshot> {
    let mut renderer = AgentRenderer::new(render_options);
    let mut stdout = io::stdout();
    let mut shell_finished: Option<(String, String)> = None;
    loop {
        let batch = sessions.events(1_000)?;
        if let Some(sequence) = batch.truncated_before {
            eprintln!("xshell: session event replay was truncated before sequence {sequence}");
        }
        if batch.events.is_empty() && batch.active_turn_id.is_none() {
            bail!("session turn ended without a terminal event");
        }
        for record in batch.events {
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
                SessionEventKind::Execution { event } => match event {
                    ExecutionEvent::TextDelta { text } => {
                        renderer.push(&text, &mut stdout)?;
                    }
                    ExecutionEvent::AgentResponse {
                        content,
                        tool_call_count,
                        partial,
                    } => {
                        if !renderer.received_delta() && !content.is_empty() {
                            renderer.push(&content, &mut stdout)?;
                        }
                        renderer.finish(&mut stdout)?;
                        renderer = AgentRenderer::new(render_options);
                        audit.append_execution(AuditEvent::AgentResponse {
                            content,
                            tool_call_count,
                            partial,
                        })?;
                    }
                    ExecutionEvent::ToolRequested { call } => {
                        println!(
                            "agent requests: {}",
                            escape_for_prompt(&tool_summary(&call))
                        );
                        audit.append_execution(AuditEvent::ToolRequested {
                            call_id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                        })?;
                    }
                    ExecutionEvent::ApprovalRequested { call, reason } => {
                        let decision =
                            confirm_tool(&call, reason.unwrap_or(GateReason::ShellExecution))?;
                        sessions.approve(record.turn_id.clone(), call.id.clone(), decision)?;
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
                        print_tool_result(&result);
                    }
                    ExecutionEvent::TurnAborted => {
                        println!("agent turn aborted; no remaining tools were executed");
                    }
                    ExecutionEvent::HistoryCompacted { report } => {
                        print_compaction(&report);
                        audit.append_execution(compaction_audit_event(&report))?;
                    }
                },
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
                    let snapshot = sessions.refresh_snapshot()?;
                    if let Some((command, status)) = shell_finished.take() {
                        audit.append_execution(AuditEvent::ShellFinished {
                            command,
                            outcome: status,
                            cwd: snapshot.descriptor.cwd.display().to_string(),
                        })?;
                    }
                    return Ok(snapshot);
                }
                SessionEventKind::TurnFailed { message } => {
                    sessions.mark_turn_finished();
                    renderer.finish(&mut stdout)?;
                    bail!("{message}");
                }
                SessionEventKind::TurnCancelled => {
                    sessions.mark_turn_finished();
                    renderer.finish(&mut stdout)?;
                    bail!("session turn was cancelled");
                }
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
        let result: Result<()> = match event {
            ExecutionEvent::TextDelta { text } => self
                .renderer
                .push(&text, &mut stdout)
                .context("could not render agent response"),
            ExecutionEvent::AgentResponse {
                content,
                tool_call_count,
                partial,
            } => {
                let mut result = Ok(());
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
                result.and(self.audit.append(AuditEvent::AgentResponse {
                    content,
                    tool_call_count,
                    partial,
                }))
            }
            ExecutionEvent::ToolRequested { call } => {
                self.last_prompted = false;
                println!(
                    "\nagent requests: {}",
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
                if self.last_approved
                    && !self.last_prompted
                    && !tools::requires_approval_by_name(&name)
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
                print_tool_result(&result);
                appended
            }
            ExecutionEvent::TurnAborted => {
                println!("agent turn aborted; no remaining tools were executed\n");
                Ok(())
            }
            ExecutionEvent::HistoryCompacted { report } => {
                print_compaction(&report);
                self.audit.append(compaction_audit_event(&report))
            }
        };
        self.record(result);
    }

    fn cancellation(&self) -> CancellationFlag {
        self.cancellation.clone()
    }

    async fn approve(&mut self, call: &ToolCall, reason: GateReason) -> ApprovalDecision {
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
        .with_compaction(&session_default.for_model(active.max_history_bytes))
}

pub(crate) fn print_compaction(report: &xshell_execution::CompactionReport) {
    eprintln!(
        "xshell: history compacted ({}): dropped {} older turn(s), {} -> {} messages, {} -> {} bytes",
        report.compactor,
        report.turns_removed,
        report.messages_before,
        report.messages_after,
        report.bytes_before,
        report.bytes_after
    );
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
    const DISPLAY_LIMIT: usize = 4 * 1024;
    let end = floor_char_boundary(result, result.len().min(DISPLAY_LIMIT));
    // Tool output (file contents, command stdout) is untrusted terminal text.
    println!("tool result:\n{}", sanitize_terminal_text(&result[..end]));
    if end < result.len() {
        println!("[terminal display truncated; full result returned to agent]");
    }
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
