//! Terminal jobs: starting, attaching, the focus loop, and the interactive session switcher.

use crate::completion::XshellHelper;
use crate::config::ActiveModel;
use crate::session::SessionRuntime;
use crate::sessions_ui::*;
use crate::turn::*;
use anyhow::{Context, Result, bail};
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use xshell_adapters::AgentAdapter;
use xshell_core::ChatMessage;

pub(crate) struct TerminalFocusOutcome {
    pub(crate) description: String,
}

pub(crate) fn run_session_pty(
    sessions: &mut SessionRuntime,
    command: &str,
    escape_prefix: u8,
) -> Result<TerminalFocusOutcome> {
    let initial = xshell_pty::controller_size().unwrap_or_default();
    let (mut pty_id, mut stream) = sessions.pty_start_stream(
        command.to_owned(),
        xshell_session::PtySize {
            rows: initial.rows,
            columns: initial.columns,
        },
        env::var("TERM").ok(),
    )?;
    run_pty_focus_loop(sessions, &mut pty_id, &mut stream, escape_prefix)
}

pub(crate) fn run_existing_session_pty(
    sessions: &mut SessionRuntime,
    escape_prefix: u8,
) -> Result<String> {
    let (mut pty_id, mut stream) = sessions.pty_attach_stream()?;
    Ok(run_pty_focus_loop(sessions, &mut pty_id, &mut stream, escape_prefix)?.description)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_active_terminal_if_running(
    sessions: &mut SessionRuntime,
    escape_prefix: u8,
    active_model: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    default_system_prompt: &str,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
) -> Result<()> {
    if !xshell_pty::controller_is_terminal() || !sessions.active_terminal_running()? {
        return Ok(());
    }
    let outcome = run_existing_session_pty(sessions, escape_prefix)?;
    if outcome != "exit status: 0" {
        println!("xshell: {outcome}");
    }
    let snapshot = sessions.refresh_snapshot()?;
    apply_runtime_snapshot(
        snapshot,
        active_model,
        agent,
        cwd,
        history,
        default_system_prompt,
        editor,
        true,
    )?;
    refresh_shell_completions(sessions, editor);
    refresh_session_completions(sessions, editor);
    Ok(())
}

pub(crate) fn run_pty_focus_loop(
    sessions: &mut SessionRuntime,
    pty_id: &mut String,
    stream: &mut xshell_session::PtyStreamClient,
    escape_prefix: u8,
) -> Result<TerminalFocusOutcome> {
    let mut last_session_id = sessions.previous_session_id().map(str::to_owned);
    loop {
        let result = stream.relay(escape_prefix);
        sessions.remember_pty_cursor(pty_id, stream.cursor());
        let action = result?;
        if !matches!(action, xshell_pty::DuplexPtyOutcome::Exited(_)) {
            stream.detach()?;
        }
        match action {
            xshell_pty::DuplexPtyOutcome::Exited(status) => {
                return Ok(TerminalFocusOutcome {
                    description: status,
                });
            }
            xshell_pty::DuplexPtyOutcome::Detached => {
                return Ok(TerminalFocusOutcome {
                    description: "PTY detached".into(),
                });
            }
            xshell_pty::DuplexPtyOutcome::Terminate => {
                sessions.pty_close(pty_id)?;
                return Ok(TerminalFocusOutcome {
                    description: "PTY terminated".into(),
                });
            }
            direction => {
                let targets = sessions.terminal_targets()?;
                let current_session_id = sessions
                    .active()
                    .map(|session| session.id.clone())
                    .context("there is no active terminal session")?;
                let target = choose_terminal_target(
                    &targets,
                    &current_session_id,
                    last_session_id.as_deref(),
                    direction,
                )?;
                if target.0.id != current_session_id {
                    last_session_id = Some(current_session_id);
                    sessions.switch(&target.0.id)?;
                }
                if !target.1 {
                    return Ok(TerminalFocusOutcome {
                        description: format!(
                            "switched to {}:{} REPL",
                            target.0.host_alias, target.0.name
                        ),
                    });
                }
                (*pty_id, *stream) = sessions.pty_attach_stream()?;
            }
        }
    }
}

pub(crate) fn choose_terminal_target(
    targets: &[(xshell_session::SessionDescriptor, bool)],
    current_session_id: &str,
    last_session_id: Option<&str>,
    direction: xshell_pty::DuplexPtyOutcome,
) -> Result<(xshell_session::SessionDescriptor, bool)> {
    if targets.is_empty() {
        bail!("there are no sessions to switch to");
    }
    let current = targets
        .iter()
        .position(|(session, _)| session.id == current_session_id)
        .unwrap_or(0);
    let index = match direction {
        xshell_pty::DuplexPtyOutcome::Next => (current + 1) % targets.len(),
        xshell_pty::DuplexPtyOutcome::Previous => (current + targets.len() - 1) % targets.len(),
        xshell_pty::DuplexPtyOutcome::Last => last_session_id
            .and_then(|id| targets.iter().position(|(session, _)| session.id == id))
            .unwrap_or((current + targets.len() - 1) % targets.len()),
        xshell_pty::DuplexPtyOutcome::Switcher => choose_terminal_interactively(targets, current)?,
        _ => bail!("invalid terminal-switch action"),
    };
    Ok(targets[index].clone())
}

pub(crate) fn choose_terminal_interactively(
    targets: &[(xshell_session::SessionDescriptor, bool)],
    current: usize,
) -> Result<usize> {
    println!("\r\nxshell session targets:");
    for (index, (session, has_terminal)) in targets.iter().enumerate() {
        let marker = if index == current { '*' } else { ' ' };
        let target_type = if *has_terminal { "terminal" } else { "REPL" };
        println!(
            " {marker} {}. {}:{} [{target_type}] — {}",
            index + 1,
            session.host_alias,
            session.name,
            session.cwd.display()
        );
    }
    print!(
        "select session [1-{}] (Enter keeps current): ",
        targets.len()
    );
    io::stdout().flush()?;
    let mut selection = String::new();
    io::stdin().read_line(&mut selection)?;
    let selection = selection.trim();
    if selection.is_empty() {
        return Ok(current);
    }
    let selected = selection
        .parse::<usize>()
        .context("terminal selection must be a number")?;
    if !(1..=targets.len()).contains(&selected) {
        bail!("terminal selection is out of range");
    }
    Ok(selected - 1)
}

pub(crate) fn is_simple_cd(command: &str) -> bool {
    shell_words::split(command)
        .is_ok_and(|words| words.len() <= 2 && words.first().map(String::as_str) == Some("cd"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_session::{PersistenceMode, Visibility};
    fn session_descriptor(id: &str, name: &str) -> xshell_session::SessionDescriptor {
        xshell_session::SessionDescriptor {
            id: id.into(),
            name: name.into(),
            host_id: "local-host".into(),
            host_alias: "local".into(),
            user: "tester".into(),
            model: xshell_session::ModelBinding {
                profile_name: None,
                provider: "ollama".into(),
                model: "test".into(),
                base_url: "http://localhost".into(),
                api_key_env: None,
                max_history_bytes: None,
            },
            cwd: PathBuf::from("/tmp"),
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
            access_mode: xshell_session::AccessMode::SingleUser,
            status: xshell_session::SessionStatus::Detached,
            activity: xshell_session::SessionActivity::Idle,
            attached_clients: 0,
            created_at_unix_ms: 0,
            last_active_at_unix_ms: 0,
        }
    }

    #[test]
    fn terminal_switching_can_target_a_session_repl_without_a_job() {
        let targets = vec![
            (session_descriptor("default-id", "default"), false),
            (session_descriptor("emacs-id", "emacs"), true),
        ];
        let target = choose_terminal_target(
            &targets,
            "emacs-id",
            Some("default-id"),
            xshell_pty::DuplexPtyOutcome::Last,
        )
        .unwrap();
        assert_eq!(target.0.id, "default-id");
        assert!(!target.1);
    }

    #[test]
    fn only_simple_cd_commands_use_session_cwd_updates() {
        assert!(is_simple_cd("cd"));
        assert!(is_simple_cd("cd 'design files'"));
        assert!(!is_simple_cd("cd /tmp && pwd"));
        assert!(!is_simple_cd("printf cd"));
    }
}
