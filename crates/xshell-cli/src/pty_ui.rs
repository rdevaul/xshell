//! Interactive processes: starting, attaching, and the unified session switcher.

use crate::audit::AuditRuntime;
use crate::completion::XshellHelper;
use crate::config::ActiveModel;
use crate::session::SessionRuntime;
use crate::sessions_ui::*;
use crate::turn::*;
use anyhow::{Context, Result, bail};
use rustyline::history::DefaultHistory;
use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, EventHandler, KeyEvent, RepeatCount,
};
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use xshell_adapters::AgentAdapter;
use xshell_core::ChatMessage;
use xshell_view::RenderOptions;

pub(crate) struct TerminalFocusOutcome {
    pub(crate) description: String,
}

#[derive(Default)]
struct PromptControllerState {
    prefix_pending: bool,
    action: Option<xshell_pty::ControllerAction>,
}

#[derive(Clone, Default)]
pub(crate) struct ControllerActionMailbox(Arc<Mutex<PromptControllerState>>);

impl ControllerActionMailbox {
    pub(crate) fn take(&self) -> Option<xshell_pty::ControllerAction> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .action
            .take()
    }
}

struct PromptControllerBinding {
    mailbox: ControllerActionMailbox,
    kind: PromptBindingKind,
}

#[derive(Clone, Copy)]
enum PromptBindingKind {
    Prefix,
    Action(xshell_pty::ControllerAction),
    Fallback,
}

fn route_prompt_binding(state: &mut PromptControllerState, kind: PromptBindingKind) -> Option<Cmd> {
    match kind {
        PromptBindingKind::Fallback => {
            state.prefix_pending = false;
            None
        }
        PromptBindingKind::Prefix => {
            state.prefix_pending = true;
            Some(Cmd::Noop)
        }
        PromptBindingKind::Action(_) if !state.prefix_pending => None,
        PromptBindingKind::Action(action) => {
            state.prefix_pending = false;
            state.action = Some(action);
            Some(Cmd::Interrupt)
        }
    }
}

impl ConditionalEventHandler for PromptControllerBinding {
    fn handle(
        &self,
        _event: &Event,
        _repeat: RepeatCount,
        _positive: bool,
        _context: &EventContext,
    ) -> Option<Cmd> {
        let mut state = self
            .mailbox
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        route_prompt_binding(&mut state, self.kind)
    }
}

pub(crate) fn bind_controller_keys(
    editor: &mut Editor<XshellHelper, DefaultHistory>,
    escape_prefix: u8,
) -> ControllerActionMailbox {
    let mailbox = ControllerActionMailbox::default();
    let prefix = KeyEvent::from(char::from(escape_prefix));
    editor.bind_sequence(
        prefix,
        EventHandler::Conditional(Box::new(PromptControllerBinding {
            mailbox: mailbox.clone(),
            kind: PromptBindingKind::Prefix,
        })),
    );
    for (key, action) in [
        ('d', xshell_pty::ControllerAction::Detach),
        ('s', xshell_pty::ControllerAction::Switcher),
        ('l', xshell_pty::ControllerAction::Last),
        ('n', xshell_pty::ControllerAction::Next),
        ('p', xshell_pty::ControllerAction::Previous),
        ('q', xshell_pty::ControllerAction::Terminate),
        ('?', xshell_pty::ControllerAction::Help),
    ] {
        editor.bind_sequence(
            KeyEvent::from(key),
            EventHandler::Conditional(Box::new(PromptControllerBinding {
                mailbox: mailbox.clone(),
                kind: PromptBindingKind::Action(action),
            })),
        );
    }
    editor.bind_sequence(
        Event::Any,
        EventHandler::Conditional(Box::new(PromptControllerBinding {
            mailbox: mailbox.clone(),
            kind: PromptBindingKind::Fallback,
        })),
    );
    mailbox
}

pub(crate) fn run_session_pty(
    sessions: &mut SessionRuntime,
    command: &str,
    escape_prefix: u8,
) -> Result<TerminalFocusOutcome> {
    let initial = xshell_pty::controller_size().unwrap_or_default();
    let mut stream = sessions.pty_start_stream(
        command.to_owned(),
        xshell_session::PtySize {
            rows: initial.rows,
            columns: initial.columns,
        },
        env::var("TERM").ok(),
    )?;
    run_pty_focus_loop(sessions, &mut stream, escape_prefix)
}

pub(crate) fn run_existing_session_pty(
    sessions: &mut SessionRuntime,
    escape_prefix: u8,
) -> Result<String> {
    let mut stream = sessions.pty_attach_stream()?;
    Ok(run_pty_focus_loop(sessions, &mut stream, escape_prefix)?.description)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_active_session(
    sessions: &mut SessionRuntime,
    escape_prefix: u8,
    active_model: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    default_system_prompt: &str,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
) -> Result<()> {
    if !sessions.enabled() {
        return Ok(());
    }
    loop {
        if let Some(turn_id) = sessions.active_turn_id().map(str::to_owned) {
            println!("reattaching to active turn {turn_id}");
            match follow_daemon_turn(sessions, audit, render_options, escape_prefix)? {
                DaemonTurnOutcome::Completed(snapshot) => {
                    apply_runtime_snapshot(
                        *snapshot,
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
                    return Ok(());
                }
                DaemonTurnOutcome::Focus(xshell_pty::ControllerAction::Detach) => return Ok(()),
                DaemonTurnOutcome::Focus(action) => {
                    let Some(snapshot) = switch_from_controller_action(sessions, action)? else {
                        return Ok(());
                    };
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
                    audit_logical_session_attached(audit, sessions, "escape_switch")?;
                    continue;
                }
            }
        }
        let previous_session = sessions.active().map(|session| session.id.clone());
        resume_active_interactive_if_running(
            sessions,
            escape_prefix,
            active_model,
            agent,
            cwd,
            history,
            default_system_prompt,
            editor,
        )?;
        if sessions.active().map(|session| &session.id) != previous_session.as_ref() {
            audit_logical_session_attached(audit, sessions, "escape_switch")?;
            continue;
        }
        return Ok(());
    }
}

pub(crate) fn switch_from_controller_action(
    sessions: &mut SessionRuntime,
    action: xshell_pty::ControllerAction,
) -> Result<Option<xshell_session::SessionSnapshot>> {
    if action == xshell_pty::ControllerAction::Detach {
        return Ok(None);
    }
    let targets = sessions.session_targets()?;
    let current_session_id = sessions
        .active()
        .map(|session| session.id.clone())
        .context("there is no active session")?;
    let target = choose_session_target(
        &targets,
        &current_session_id,
        sessions.previous_session_id(),
        action,
    )?;
    let snapshot = match target {
        SessionTargetChoice::Existing(index) => sessions.switch(&targets[index].id)?,
        SessionTargetChoice::New => {
            let Some(name) = prompt_for_session_name()? else {
                return Ok(None);
            };
            sessions.create_sibling(name)?
        }
    };
    println!(
        "switched to {}:{}",
        snapshot.descriptor.host_alias, snapshot.descriptor.name
    );
    Ok(Some(snapshot))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_active_interactive_if_running(
    sessions: &mut SessionRuntime,
    escape_prefix: u8,
    active_model: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    default_system_prompt: &str,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
) -> Result<()> {
    if !xshell_pty::controller_is_terminal() || !sessions.active_interactive_running()? {
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
    stream: &mut xshell_session::PtyStreamClient,
    escape_prefix: u8,
) -> Result<TerminalFocusOutcome> {
    loop {
        let result = stream.relay(escape_prefix);
        sessions.remember_pty_cursor(stream.cursor());
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
                sessions.pty_close_current()?;
                return Ok(TerminalFocusOutcome {
                    description: "PTY terminated".into(),
                });
            }
            direction => {
                let direction = match direction {
                    xshell_pty::DuplexPtyOutcome::Last => xshell_pty::ControllerAction::Last,
                    xshell_pty::DuplexPtyOutcome::Next => xshell_pty::ControllerAction::Next,
                    xshell_pty::DuplexPtyOutcome::Previous => {
                        xshell_pty::ControllerAction::Previous
                    }
                    xshell_pty::DuplexPtyOutcome::Switcher => {
                        xshell_pty::ControllerAction::Switcher
                    }
                    _ => unreachable!("terminal outcome handled above"),
                };
                let targets = sessions.session_targets()?;
                let current_session_id = sessions
                    .active()
                    .map(|session| session.id.clone())
                    .context("there is no active terminal session")?;
                let target = choose_session_target(
                    &targets,
                    &current_session_id,
                    sessions.previous_session_id(),
                    direction,
                )?;
                let target = match target {
                    SessionTargetChoice::Existing(index) => targets[index].clone(),
                    SessionTargetChoice::New => {
                        let Some(name) = prompt_for_session_name()? else {
                            *stream = sessions.pty_attach_stream()?;
                            continue;
                        };
                        let created = sessions.create_sibling(name)?;
                        return Ok(TerminalFocusOutcome {
                            description: format!(
                                "created and switched to {}:{} prompt",
                                created.descriptor.host_alias, created.descriptor.name
                            ),
                        });
                    }
                };
                if target.id != current_session_id {
                    sessions.switch(&target.id)?;
                }
                if !target.activity.is_interactive_process() {
                    return Ok(TerminalFocusOutcome {
                        description: format!(
                            "switched to {}:{} prompt",
                            target.host_alias, target.name
                        ),
                    });
                }
                *stream = sessions.pty_attach_stream()?;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionTargetChoice {
    Existing(usize),
    New,
}

pub(crate) fn choose_session_target(
    targets: &[xshell_session::SessionDescriptor],
    current_session_id: &str,
    last_session_id: Option<&str>,
    direction: xshell_pty::ControllerAction,
) -> Result<SessionTargetChoice> {
    if targets.is_empty() {
        bail!("there are no sessions to switch to");
    }
    let current = targets
        .iter()
        .position(|session| session.id == current_session_id)
        .unwrap_or(0);
    let index = match direction {
        xshell_pty::ControllerAction::Next => (current + 1) % targets.len(),
        xshell_pty::ControllerAction::Previous => (current + targets.len() - 1) % targets.len(),
        xshell_pty::ControllerAction::Last => last_session_id
            .and_then(|id| targets.iter().position(|session| session.id == id))
            .unwrap_or((current + targets.len() - 1) % targets.len()),
        xshell_pty::ControllerAction::Switcher => {
            return choose_session_interactively(targets, current);
        }
        _ => bail!("invalid terminal-switch action"),
    };
    Ok(SessionTargetChoice::Existing(index))
}

pub(crate) fn choose_session_interactively(
    targets: &[xshell_session::SessionDescriptor],
    current: usize,
) -> Result<SessionTargetChoice> {
    println!("\r\nxshell session targets:");
    for (index, session) in targets.iter().enumerate() {
        let marker = if index == current { '*' } else { ' ' };
        let activity = session_activity_label(&session.activity);
        println!(
            " {marker} {}. {}:{} [{activity}] — {}",
            index + 1,
            session.host_alias,
            session.name,
            session.cwd.display()
        );
    }
    let host = &targets[current].host_alias;
    println!("   n. new session on {host}");
    print!(
        "select session [1-{} or n] (Enter keeps current): ",
        targets.len()
    );
    io::stdout().flush()?;
    let mut selection = String::new();
    io::stdin().read_line(&mut selection)?;
    match parse_picker_selection(&selection, targets.len(), current)? {
        PickerSelection::Existing(index) => Ok(SessionTargetChoice::Existing(index)),
        PickerSelection::New => Ok(SessionTargetChoice::New),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerSelection {
    Existing(usize),
    New,
}

fn parse_picker_selection(
    selection: &str,
    target_count: usize,
    current: usize,
) -> Result<PickerSelection> {
    let selection = selection.trim();
    if selection.is_empty() {
        return Ok(PickerSelection::Existing(current));
    }
    if selection.eq_ignore_ascii_case("n") {
        return Ok(PickerSelection::New);
    }
    let selected = selection
        .parse::<usize>()
        .context("session selection must be a number or n")?;
    if !(1..=target_count).contains(&selected) {
        bail!("session selection is out of range");
    }
    Ok(PickerSelection::Existing(selected - 1))
}

fn prompt_for_session_name() -> Result<Option<String>> {
    print!("new session name (blank cancels): ");
    io::stdout().flush()?;
    let mut name = String::new();
    io::stdin().read_line(&mut name)?;
    let name = name.trim();
    Ok((!name.is_empty()).then(|| name.to_owned()))
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
    fn terminal_switching_can_target_a_session_prompt_without_a_process() {
        let targets = vec![
            session_descriptor("default-id", "default"),
            session_descriptor("emacs-id", "emacs"),
        ];
        let target = choose_session_target(
            &targets,
            "emacs-id",
            Some("default-id"),
            xshell_pty::ControllerAction::Last,
        )
        .unwrap();
        let SessionTargetChoice::Existing(index) = target else {
            panic!("expected an existing session");
        };
        assert_eq!(targets[index].id, "default-id");
    }

    #[test]
    fn only_simple_cd_commands_use_session_cwd_updates() {
        assert!(is_simple_cd("cd"));
        assert!(is_simple_cd("cd 'design files'"));
        assert!(!is_simple_cd("cd /tmp && pwd"));
        assert!(!is_simple_cd("printf cd"));
    }

    #[test]
    fn session_picker_offers_a_new_session_action() {
        assert_eq!(
            parse_picker_selection("n", 2, 0).unwrap(),
            PickerSelection::New
        );
        assert_eq!(
            parse_picker_selection("", 2, 1).unwrap(),
            PickerSelection::Existing(1)
        );
    }

    #[test]
    fn prompt_router_requires_prefix_and_clears_it_after_unknown_input() {
        let mut state = PromptControllerState::default();
        assert_eq!(
            route_prompt_binding(
                &mut state,
                PromptBindingKind::Action(xshell_pty::ControllerAction::Switcher)
            ),
            None
        );
        assert_eq!(
            route_prompt_binding(&mut state, PromptBindingKind::Prefix),
            Some(Cmd::Noop)
        );
        assert_eq!(
            route_prompt_binding(&mut state, PromptBindingKind::Fallback),
            None
        );
        assert_eq!(
            route_prompt_binding(
                &mut state,
                PromptBindingKind::Action(xshell_pty::ControllerAction::Switcher)
            ),
            None
        );
        assert_eq!(
            route_prompt_binding(&mut state, PromptBindingKind::Prefix),
            Some(Cmd::Noop)
        );
        assert_eq!(
            route_prompt_binding(
                &mut state,
                PromptBindingKind::Action(xshell_pty::ControllerAction::Switcher)
            ),
            Some(Cmd::Interrupt)
        );
        assert_eq!(state.action, Some(xshell_pty::ControllerAction::Switcher));
    }
}
