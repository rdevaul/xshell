//! Named-session commands: `//new`, `//switch`, `//connect`, `//sessions`, and audit of logical attach/detach.

use crate::audit::AuditRuntime;
use crate::completion::XshellHelper;
use crate::config::{ActiveModel, XshellConfig};
use crate::model::*;
use crate::session::SessionRuntime;
use anyhow::{Context, Result, bail};
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use std::path::PathBuf;
use xshell_adapters::AgentAdapter;
use xshell_audit::AuditEvent;
use xshell_core::ChatMessage;
use xshell_session::{PersistenceMode, SessionSnapshot, Visibility};

pub(crate) fn session_label(sessions: &SessionRuntime) -> String {
    sessions.active().map_or_else(
        || "local:standalone".into(),
        |session| format!("{}:{}", session.host_alias, session.name),
    )
}

pub(crate) fn audit_logical_session_attached(
    audit: &mut AuditRuntime,
    sessions: &SessionRuntime,
    action: &str,
) -> Result<()> {
    let Some(session) = sessions.active() else {
        return Ok(());
    };
    audit.append(AuditEvent::LogicalSessionAttached {
        action: action.into(),
        session_id: session.id.clone(),
        name: session.name.clone(),
        host_id: session.host_id.clone(),
        host_alias: session.host_alias.clone(),
        user: session.user.clone(),
        // The CLI does not run terminal jobs; xshelld records the policy.
        terminal_stream: None,
    })
}

pub(crate) fn audit_logical_session_detached(
    audit: &mut AuditRuntime,
    session: Option<&xshell_session::SessionDescriptor>,
    action: &str,
) -> Result<()> {
    let Some(session) = session else {
        return Ok(());
    };
    audit.append(AuditEvent::LogicalSessionDetached {
        action: action.into(),
        session_id: session.id.clone(),
        name: session.name.clone(),
    })
}

pub(crate) fn restore_session_state(
    snapshot: SessionSnapshot,
    active_model: &mut ActiveModel,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    default_system_prompt: &str,
) -> Result<()> {
    let restored_model = ActiveModel::from_session_binding(snapshot.descriptor.model)?;
    *active_model = restored_model;
    // The active working directory may be on another host. Its daemon validates
    // the path; a controller must not try to resolve it against its local filesystem.
    *cwd = snapshot.descriptor.cwd;
    *history = snapshot.history;
    if history.is_empty() {
        history.push(ChatMessage::system(default_system_prompt));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn switch_session(
    selector: &str,
    sessions: &mut SessionRuntime,
    active_model: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    default_system_prompt: &str,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
) -> Result<()> {
    sessions.sync(active_model, cwd, history)?;
    let snapshot = sessions.switch(selector)?;
    restore_session_state(snapshot, active_model, cwd, history, default_system_prompt)?;
    *agent = build_adapter(active_model, false)?;
    if let Some(helper) = editor.helper_mut() {
        helper.set_cwd(cwd.clone());
    }
    println!("switched to {}", session_label(sessions));
    Ok(())
}

pub(crate) struct NewSessionOptions {
    pub(crate) name: String,
    pub(crate) profile: Option<String>,
    pub(crate) persistence: PersistenceMode,
    pub(crate) visibility: Visibility,
}

pub(crate) struct ConnectOptions {
    pub(crate) destination: String,
    pub(crate) session: Option<String>,
}

pub(crate) fn parse_connect_options(args: &[String]) -> Result<ConnectOptions> {
    let Some(destination) = args.first() else {
        bail!("usage: //connect SSH_DESTINATION [--session NAME]");
    };
    let mut session = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--session" => {
                index += 1;
                session = Some(
                    args.get(index)
                        .context("--session requires a session name")?
                        .clone(),
                );
            }
            argument => bail!("unknown //connect option {argument:?}"),
        }
        index += 1;
    }
    Ok(ConnectOptions {
        destination: destination.clone(),
        session,
    })
}

pub(crate) fn parse_new_session_options(args: &[String]) -> Result<NewSessionOptions> {
    let Some(name) = args.first() else {
        bail!(
            "usage: //new NAME [--model PROFILE] [--ephemeral|--daemon|--durable] \
[--host-only|--fabric]"
        );
    };
    let mut options = NewSessionOptions {
        name: name.clone(),
        profile: None,
        persistence: PersistenceMode::Daemon,
        visibility: Visibility::Fabric,
    };
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--model" => {
                index += 1;
                options.profile = Some(
                    args.get(index)
                        .context("--model requires a profile name")?
                        .clone(),
                );
            }
            "--ephemeral" => options.persistence = PersistenceMode::Ephemeral,
            "--daemon" => options.persistence = PersistenceMode::Daemon,
            "--durable" => options.persistence = PersistenceMode::Durable,
            "--host-only" => options.visibility = Visibility::HostOnly,
            "--fabric" => options.visibility = Visibility::Fabric,
            argument => bail!("unknown //new option {argument:?}"),
        }
        index += 1;
    }
    Ok(options)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_session(
    args: Vec<String>,
    config: &XshellConfig,
    sessions: &mut SessionRuntime,
    active_model: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    cwd: &mut PathBuf,
    history: &mut Vec<ChatMessage>,
    system_prompt: &str,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
) -> Result<()> {
    let options = parse_new_session_options(&args)?;
    sessions.sync(active_model, cwd, history)?;
    let new_model = match options.profile.as_deref() {
        Some(profile) => config.resolve_profile(profile)?,
        None => active_model.clone(),
    };
    let new_history = vec![ChatMessage::system(system_prompt)];
    let snapshot = sessions.create(
        options.name,
        &new_model,
        cwd,
        new_history,
        options.persistence,
        options.visibility,
    )?;
    restore_session_state(snapshot, active_model, cwd, history, system_prompt)?;
    *agent = build_adapter(active_model, false)?;
    if let Some(helper) = editor.helper_mut() {
        helper.set_cwd(cwd.clone());
    }
    println!("created and attached {}", session_label(sessions));
    Ok(())
}

pub(crate) fn print_sessions(sessions: &mut SessionRuntime) -> Result<()> {
    let active_id = sessions.active().map(|session| session.id.clone());
    let catalog = sessions.list()?;
    if catalog.is_empty() {
        println!("no sessions on connected hosts");
        return Ok(());
    }
    for session in catalog {
        let marker = if active_id.as_deref() == Some(session.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {}/{}:{} — {} / {} — {:?}, {:?}, {:?}, {:?}",
            session.host_alias,
            session.user,
            session.name,
            session.model.profile_name.as_deref().unwrap_or("custom"),
            session.model.model,
            session.status,
            session.activity,
            session.persistence,
            session.visibility
        );
    }
    Ok(())
}

pub(crate) fn refresh_session_completions(
    sessions: &mut SessionRuntime,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
) {
    let Ok(names) = sessions.session_names() else {
        return;
    };
    if let Some(helper) = editor.helper_mut() {
        helper.set_session_names(names);
    }
}

pub(crate) fn refresh_shell_completions(
    sessions: &SessionRuntime,
    editor: &mut Editor<XshellHelper, DefaultHistory>,
) {
    let remote = match sessions.remote_completion_client() {
        Ok(remote) => remote,
        Err(error) => {
            eprintln!("xshell: remote shell completion unavailable: {error:#}");
            if let Some(helper) = editor.helper_mut() {
                helper.set_remote_shell_completion(None);
                helper.set_shell_completion_enabled(false);
            }
            return;
        }
    };
    if let Some(helper) = editor.helper_mut() {
        helper.set_remote_shell_completion(remote);
        helper.set_shell_completion_enabled(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_new_session_lifecycle_visibility_and_model() {
        let args = vec![
            "robot".into(),
            "--durable".into(),
            "--host-only".into(),
            "--model".into(),
            "local-qwen".into(),
        ];
        let options = parse_new_session_options(&args).unwrap();
        assert_eq!(options.name, "robot");
        assert_eq!(options.profile.as_deref(), Some("local-qwen"));
        assert_eq!(options.persistence, PersistenceMode::Durable);
        assert_eq!(options.visibility, Visibility::HostOnly);
    }

    #[test]
    fn parses_ssh_connection_destination_and_session() {
        let args = vec!["rich@mini.local".into(), "--session".into(), "cad".into()];
        let options = parse_connect_options(&args).unwrap();
        assert_eq!(options.destination, "rich@mini.local");
        assert_eq!(options.session.as_deref(), Some("cad"));
        assert!(parse_connect_options(&[]).is_err());
    }
}
