//! Redrawing a session's recent output when focus moves to it.
//!
//! The daemon keeps each session's event journal, so output produced while
//! another session had focus is not lost; it was simply never drawn. On a
//! switch the screen is cleared and the tail of the journal is rendered,
//! ending with whatever the user has not yet seen.

use crate::session::SessionRuntime;
use crate::sessions_ui::session_label;
use crate::turn::{
    TRUNCATED_RESPONSE_NOTICE, format_auto_continue, format_compaction, format_tool_result,
};
use anyhow::Result;
use std::collections::HashSet;
use std::io::{self, Write};
use xshell_execution::{ApprovalDecision, ExecutionEvent, GateReason, tool_summary};
use xshell_session::{SessionEvent, SessionEventKind, TurnInput};
use xshell_view::{AgentRenderer, RenderOptions, escape_for_prompt, sanitize_terminal_text};

/// Rows kept free below the redraw: the "reattaching" notice and the prompt.
const RESERVED_ROWS: usize = 2;

const DROPPED_NOTICE: &str = "[xshell: earlier output from this session is no longer kept]\n";

/// Redraw the active session if focus moved to it since the last redraw.
pub(crate) fn redraw_if_pending(
    sessions: &mut SessionRuntime,
    render_options: RenderOptions,
) -> Result<()> {
    if !sessions.take_redraw() || !xshell_pty::controller_is_terminal() {
        return Ok(());
    }
    // An interactive process repaints itself from its own PTY replay.
    if sessions
        .active()
        .is_some_and(|session| session.activity.is_interactive_process())
    {
        return Ok(());
    }
    let batch = sessions.journal()?;
    let cursor = sessions.event_cursor().unwrap_or(0);
    let shown_through = backlog_end(
        &batch.events,
        batch.active_turn_id.as_deref(),
        cursor,
        batch.next_sequence,
    );
    // Events up to `shown_through` are drawn here; a live turn's later events
    // are left for `follow_daemon_turn`, which still has to answer approvals.
    sessions.acknowledge_event(shown_through)?;
    let shown = batch
        .events
        .iter()
        .take_while(|record| record.sequence <= shown_through);
    let rendered = render_events(shown, render_options)?;

    let size = xshell_pty::controller_size().unwrap_or_default();
    let rows = usize::from(size.rows)
        .saturating_sub(RESERVED_ROWS + 1)
        .max(1);
    let columns = usize::from(size.columns).max(1);
    let tail = tail_rows(&rendered, rows, columns);

    let mut stdout = io::stdout();
    writeln!(stdout, "\x1b[H\x1b[2J── {} ──", session_label(sessions))?;
    if tail.len() == rendered.len() && batch.truncated_before.is_some() {
        stdout.write_all(DROPPED_NOTICE.as_bytes())?;
    }
    stdout.write_all(tail)?;
    stdout.flush()?;
    Ok(())
}

/// The last journal sequence the redraw renders. With no live turn that is
/// everything. With one, it is everything before that turn plus whatever of
/// it was already seen; the rest is followed live so a pending approval is
/// prompted rather than merely displayed.
fn backlog_end(
    events: &[SessionEvent],
    active_turn_id: Option<&str>,
    cursor: u64,
    next_sequence: u64,
) -> u64 {
    let Some(turn_id) = active_turn_id else {
        return next_sequence.saturating_sub(1);
    };
    events
        .iter()
        .find(|record| record.turn_id == turn_id)
        .map_or(cursor, |first| cursor.max(first.sequence.saturating_sub(1)))
}

/// Render journal events the way `follow_daemon_turn` draws them live, but
/// into a buffer and without prompting, auditing, or acknowledging anything.
fn render_events<'a>(
    events: impl IntoIterator<Item = &'a SessionEvent>,
    render_options: RenderOptions,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut renderer = AgentRenderer::new(render_options);
    let mut prompted_calls = HashSet::new();
    for record in events {
        match &record.event {
            SessionEventKind::TurnStarted { input, .. } => {
                renderer.finish(&mut output)?;
                renderer = AgentRenderer::new(render_options);
                let text = match input {
                    TurnInput::Agent { message } => sanitize_terminal_text(message),
                    TurnInput::Shell { command } => {
                        format!("$ {}", sanitize_terminal_text(command))
                    }
                };
                writeln!(output, "› {}", text.trim_end())?;
            }
            SessionEventKind::Execution { event } => match event {
                ExecutionEvent::TextDelta { text } => renderer.push(text, &mut output)?,
                ExecutionEvent::AgentResponse {
                    content,
                    finish_reason,
                    ..
                } => {
                    if !renderer.received_delta() && !content.is_empty() {
                        renderer.push(content, &mut output)?;
                    }
                    renderer.finish(&mut output)?;
                    renderer = AgentRenderer::new(render_options);
                    if finish_reason.as_deref() == Some("length") {
                        writeln!(output, "{TRUNCATED_RESPONSE_NOTICE}")?;
                    }
                }
                ExecutionEvent::AutoContinued { reason, .. } => {
                    writeln!(output, "{}", format_auto_continue(reason))?;
                }
                ExecutionEvent::ToolRequested { call } => {
                    writeln!(
                        output,
                        "agent requests: {}",
                        escape_for_prompt(&tool_summary(call))
                    )?;
                }
                ExecutionEvent::ApprovalRequested { call, reason } => {
                    let why = match reason {
                        Some(GateReason::SensitivePath) => " (matches sensitive-path policy)",
                        _ => "",
                    };
                    write!(
                        output,
                        "Approve `{}`{why}? ",
                        escape_for_prompt(&tool_summary(call))
                    )?;
                    prompted_calls.insert(call.id.as_str());
                }
                ExecutionEvent::ToolDecision { call_id, decision } => {
                    if prompted_calls.remove(call_id.as_str()) {
                        writeln!(
                            output,
                            "{}",
                            match decision {
                                ApprovalDecision::Approve => "y",
                                ApprovalDecision::Deny => "n",
                                ApprovalDecision::AbortTurn => "q",
                            }
                        )?;
                    }
                }
                ExecutionEvent::ToolSkipped { .. } => {}
                ExecutionEvent::ToolResult { result, .. } => {
                    output.extend_from_slice(format_tool_result(result).as_bytes());
                }
                ExecutionEvent::TurnAborted => {
                    writeln!(
                        output,
                        "agent turn aborted; no remaining tools were executed"
                    )?;
                }
                ExecutionEvent::HistoryCompacted { report } => {
                    writeln!(output, "{}", format_compaction(report))?;
                }
            },
            SessionEventKind::ShellOutput { text, .. } => output.extend_from_slice(text.as_bytes()),
            SessionEventKind::WorkingDirectoryChanged { .. } => {}
            SessionEventKind::ShellFinished { status, .. } => {
                if status != "exit status: 0" && status != "working directory changed" {
                    writeln!(output, "xshell: command finished with {status}")?;
                }
            }
            SessionEventKind::TurnCompleted => renderer.finish(&mut output)?,
            SessionEventKind::TurnFailed { message } => {
                renderer.finish(&mut output)?;
                writeln!(
                    output,
                    "xshell agent error: {}",
                    sanitize_terminal_text(message)
                )?;
            }
            SessionEventKind::TurnCancelled => {
                renderer.finish(&mut output)?;
                writeln!(output, "xshell: session turn was cancelled")?;
            }
        }
    }
    renderer.finish(&mut output)?;
    Ok(output)
}

/// The shortest suffix of `rendered`, starting at a line boundary, that fills
/// at most `rows` terminal rows when wrapped at `columns`.
fn tail_rows(rendered: &[u8], rows: usize, columns: usize) -> &[u8] {
    let body = rendered.strip_suffix(b"\n").unwrap_or(rendered);
    let mut used = 0;
    let mut start = body.len();
    for line in body.rsplit(|byte| *byte == b'\n') {
        let height = display_width(line).div_ceil(columns).max(1);
        if used + height > rows {
            break;
        }
        used += height;
        start -= line.len();
        if start == 0 {
            return rendered;
        }
        start -= 1;
    }
    &rendered[start + 1..]
}

/// The visible width of one line, ignoring ANSI escape sequences. Characters
/// are counted as one column each, which is close enough to pick a tail.
fn display_width(line: &[u8]) -> usize {
    let text = String::from_utf8_lossy(line);
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
        } else if !ch.is_control() {
            width += 1;
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_execution::ApprovalPolicy;

    fn record(sequence: u64, turn_id: &str, event: SessionEventKind) -> SessionEvent {
        SessionEvent {
            sequence,
            turn_id: turn_id.to_owned(),
            timestamp_unix_ms: 0,
            event,
        }
    }

    fn started(sequence: u64, turn_id: &str, message: &str) -> SessionEvent {
        record(
            sequence,
            turn_id,
            SessionEventKind::TurnStarted {
                input: TurnInput::Agent {
                    message: message.to_owned(),
                },
                approval: ApprovalPolicy::default(),
                requested_approval: None,
            },
        )
    }

    fn delta(sequence: u64, turn_id: &str, text: &str) -> SessionEvent {
        record(
            sequence,
            turn_id,
            SessionEventKind::Execution {
                event: ExecutionEvent::TextDelta {
                    text: text.to_owned(),
                },
            },
        )
    }

    fn plain() -> RenderOptions {
        RenderOptions::resolve(
            &Default::default(),
            Some(xshell_view::OutputMode::Never),
            Some(xshell_view::OutputMode::Never),
        )
        .unwrap()
    }

    #[test]
    fn a_finished_turn_is_drawn_in_full() {
        let events = [
            started(1, "t1", "hello"),
            delta(2, "t1", "first answer\n"),
            record(3, "t1", SessionEventKind::TurnCompleted),
            started(4, "t2", "again"),
            delta(5, "t2", "second answer"),
            record(6, "t2", SessionEventKind::TurnCompleted),
        ];
        assert_eq!(backlog_end(&events, None, 2, 7), 6);
        let rendered = render_events(&events, plain()).unwrap();
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            "› hello\nfirst answer\n› again\nsecond answer\n"
        );
    }

    #[test]
    fn a_live_turn_leaves_its_unseen_events_to_the_follower() {
        let events = [
            started(1, "t1", "old"),
            record(2, "t1", SessionEventKind::TurnCompleted),
            started(3, "t2", "live"),
            delta(4, "t2", "partial"),
        ];
        // Never seen: draw the finished turn, follow all of the live one.
        assert_eq!(backlog_end(&events, Some("t2"), 0, 5), 2);
        // Already read into the live turn: draw up to where the reader was.
        assert_eq!(backlog_end(&events, Some("t2"), 3, 5), 3);
    }

    #[test]
    fn approvals_are_drawn_with_their_answers() {
        let call = xshell_core::ToolCall {
            id: "c1".into(),
            name: "run_shell".into(),
            arguments: serde_json::json!({ "command": "ls" }),
        };
        let events = [
            record(
                1,
                "t1",
                SessionEventKind::Execution {
                    event: ExecutionEvent::ApprovalRequested {
                        call: call.clone(),
                        reason: None,
                    },
                },
            ),
            record(
                2,
                "t1",
                SessionEventKind::Execution {
                    event: ExecutionEvent::ToolDecision {
                        call_id: "c1".into(),
                        decision: ApprovalDecision::Deny,
                    },
                },
            ),
        ];
        let rendered = String::from_utf8(render_events(&events, plain()).unwrap()).unwrap();
        assert!(rendered.starts_with("Approve `"), "{rendered}");
        assert!(rendered.ends_with("? n\n"), "{rendered}");
    }

    #[test]
    fn the_tail_fits_the_screen_and_counts_wrapped_rows() {
        let rendered = b"one\ntwo\nthree\nfour\n";
        assert_eq!(tail_rows(rendered, 2, 80), b"three\nfour\n");
        assert_eq!(tail_rows(rendered, 10, 80), rendered);
        // "four" wraps onto two rows at width 2, leaving room for nothing else.
        assert_eq!(tail_rows(rendered, 2, 2), b"four\n");
        assert_eq!(tail_rows(b"", 5, 80), b"");
    }

    #[test]
    fn escape_sequences_take_no_columns() {
        assert_eq!(display_width(b"\x1b[1mbold\x1b[0m"), 4);
    }
}
