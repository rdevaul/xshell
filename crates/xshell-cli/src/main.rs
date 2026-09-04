mod audit;
mod completion;
mod config;
mod control;
mod model;
mod pty_ui;
mod session;
mod sessions_ui;
mod tools;
mod turn;
mod util;
mod view;

use anyhow::{Context, Result};
use audit::AuditRuntime;
use clap::Parser;
use completion::XshellHelper;
use config::{ModelOverrides, OutputMode, Provider, XshellConfig};
use control::*;
use model::*;
use pty_ui::*;
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use session::SessionRuntime;
use sessions_ui::*;
use std::env;
use std::path::PathBuf;
use turn::*;
use util::*;
use view::*;
use xshell_audit::AuditEvent;
use xshell_core::{ChatMessage, ControlCommand, DEFAULT_SYSTEM_PROMPT, InputRoute, classify_input};
use xshell_execution::{ApprovalPolicy, TurnPolicy};
use xshell_session::TurnInput;
use xshell_view::{RenderOptions, ViewerRegistry};

#[derive(Debug, Parser)]
#[command(version, about = "An agent-first, network-aware interactive shell")]
struct Args {
    #[arg(long, env = "XSHELL_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, env = "XSHELL_PROFILE")]
    profile: Option<String>,

    #[arg(long, env = "XSHELL_PROVIDER", value_enum)]
    provider: Option<Provider>,

    #[arg(long, env = "XSHELL_MODEL")]
    model: Option<String>,

    #[arg(long, env = "XSHELL_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, env = "XSHELL_API_KEY_ENV")]
    api_key_env: Option<String>,

    #[arg(long, env = "XSHELL_SYSTEM_PROMPT", default_value = DEFAULT_SYSTEM_PROMPT)]
    system_prompt: String,

    #[arg(long, env = "XSHELL_SESSION")]
    session: Option<String>,

    #[arg(long, default_value = ".")]
    cwd: PathBuf,

    #[arg(
        long,
        env = "XSHELL_APPROVAL",
        value_enum,
        default_value = "ask",
        help = "Approval policy: ask (prompt, default), auto (run all), off (deny shell)"
    )]
    approval: ApprovalPolicy,

    #[arg(
        long,
        env = "XSHELL_MARKDOWN",
        value_enum,
        help = "Agent Markdown rendering: auto, always, or never"
    )]
    markdown: Option<OutputMode>,

    #[arg(
        long,
        env = "XSHELL_COLOR",
        value_enum,
        help = "Rendered ANSI styling: auto, always, or never (NO_COLOR wins)"
    )]
    color: Option<OutputMode>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (model_config, config_path) = XshellConfig::load(args.config.as_deref())?;
    let render_options =
        RenderOptions::resolve(&model_config.rendering, args.markdown, args.color)?;
    let pty_escape = xshell_pty::parse_escape_prefix(&model_config.session_fabric.pty_escape)?;
    // Approval and sensitive paths are fixed for the process; the history
    // budget is resolved per turn from the active model so `//model` switches
    // are honoured (see `turn_policy_for`).
    let base_policy = TurnPolicy::new(args.approval)
        .with_sensitive_paths(model_config.session_fabric.sensitive_paths());
    let viewers = ViewerRegistry::with_builtins();
    let mut active_model = model_config.resolve_startup(
        args.profile.as_deref(),
        ModelOverrides {
            provider: args.provider,
            model: args.model.clone(),
            base_url: args.base_url.clone(),
            api_key_env: args.api_key_env.clone(),
        },
    )?;
    let mut cwd = args
        .cwd
        .canonicalize()
        .with_context(|| format!("cannot use working directory {}", args.cwd.display()))?;
    let mut history = vec![ChatMessage::system(args.system_prompt.clone())];
    let (mut sessions, restored) = SessionRuntime::start(
        &model_config.session_fabric,
        args.session.as_deref(),
        &active_model,
        &cwd,
        &history,
    )?;
    if let Some(snapshot) = restored {
        restore_session_state(
            snapshot,
            &mut active_model,
            &mut cwd,
            &mut history,
            &args.system_prompt,
        )?;
    }
    let mut agent = build_adapter(&active_model, !sessions.enabled())?;
    let mut audit = AuditRuntime::start(&model_config.audit)?;
    if sessions.enabled() {
        audit.delegate_execution_events();
    }
    audit.append(AuditEvent::SessionStarted {
        client_version: env!("CARGO_PKG_VERSION").into(),
        cwd: cwd.display().to_string(),
        model_profile: active_model_label(&active_model).into(),
        provider: format!("{:?}", active_model.provider),
        model: active_model.model.clone(),
        endpoint: active_model.base_url.clone(),
        system_prompt: args.system_prompt.clone(),
        approval: args.approval.to_string(),
    })?;
    audit_logical_session_attached(&mut audit, &sessions, "startup")?;
    let model_profiles: Vec<String> = model_config.models.keys().cloned().collect();
    let mut editor = Editor::<XshellHelper, DefaultHistory>::new()
        .context("could not initialize terminal input")?;
    editor.set_helper(Some(XshellHelper::new(cwd.clone(), model_profiles)));
    refresh_session_completions(&mut sessions, &mut editor);
    refresh_shell_completions(&sessions, &mut editor);

    println!("xshell local prototype — //help for commands; //quit or Ctrl-D to exit");
    if config_path.exists() {
        println!("config: {}", config_path.display());
    }
    print_status(
        agent.as_ref(),
        &active_model,
        &audit,
        &sessions,
        &cwd,
        args.approval,
    );

    if let Some(turn_id) = sessions.active_turn_id().map(str::to_owned) {
        println!("reattaching to active turn {turn_id}");
        let snapshot = follow_daemon_turn(&mut sessions, &mut audit, render_options)?;
        apply_runtime_snapshot(
            snapshot,
            &mut active_model,
            &mut agent,
            &mut cwd,
            &mut history,
            &args.system_prompt,
            &mut editor,
            true,
        )?;
    }

    let mut sticky_shell = false;
    let exit_reason = loop {
        let prompt = format!(
            "[{} {} {}] › ",
            session_label(&sessions),
            active_model_label(&active_model),
            compact_path(&cwd)
        );

        let line = match if sticky_shell {
            editor.readline_with_initial(&prompt, ("$", ""))
        } else {
            editor.readline(&prompt)
        } {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break "eof",
            Err(error) => return Err(error).context("terminal input failed"),
        };

        if !line.trim().is_empty() {
            let _ = editor.add_history_entry(line.as_str());
        }

        let route = apply_sticky_shell_mode(classify_input(&line), &mut sticky_shell);
        if !matches!(route, InputRoute::Empty) {
            // Control commands never reach the daemon; always record them.
            // Agent and shell input is recorded by whichever process runs it.
            if matches!(route, InputRoute::Control(_)) {
                audit.append(AuditEvent::Input {
                    route: input_route_name(&route).into(),
                    text: line.clone(),
                })?;
            } else {
                audit.append_execution(AuditEvent::Input {
                    route: input_route_name(&route).into(),
                    text: line.clone(),
                })?;
            }
        }

        match route {
            InputRoute::Empty => {}
            InputRoute::Shell(command) => {
                if sessions.enabled() {
                    if xshell_pty::controller_is_terminal() && !is_simple_cd(&command) {
                        let previous_session = sessions.active().map(|session| session.id.clone());
                        let result = match run_session_pty(&mut sessions, &command, pty_escape) {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                eprintln!("xshell: {error:#}");
                                TerminalFocusOutcome {
                                    description: format!("error: {error:#}"),
                                }
                            }
                        };
                        if result.description != "exit status: 0"
                            && !result.description.starts_with("error:")
                        {
                            eprintln!("xshell: {}", result.description);
                        }
                        if sessions.active().map(|session| &session.id) != previous_session.as_ref()
                        {
                            let snapshot = sessions.refresh_snapshot()?;
                            apply_runtime_snapshot(
                                snapshot,
                                &mut active_model,
                                &mut agent,
                                &mut cwd,
                                &mut history,
                                &args.system_prompt,
                                &mut editor,
                                true,
                            )?;
                            refresh_shell_completions(&sessions, &mut editor);
                            refresh_session_completions(&mut sessions, &mut editor);
                        }
                        continue;
                    }
                    match run_daemon_turn(
                        &mut sessions,
                        TurnInput::Shell {
                            command: command.clone(),
                        },
                        args.approval,
                        &mut audit,
                        render_options,
                    ) {
                        Ok(snapshot) => apply_runtime_snapshot(
                            snapshot,
                            &mut active_model,
                            &mut agent,
                            &mut cwd,
                            &mut history,
                            &args.system_prompt,
                            &mut editor,
                            true,
                        )?,
                        Err(error) => eprintln!("xshell: {error:#}"),
                    }
                    continue;
                }
                let previous_cwd = cwd.clone();
                let outcome = match run_shell(&command, &mut cwd) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        eprintln!("xshell: {error:#}");
                        format!("error: {error:#}")
                    }
                };
                if cwd != previous_cwd {
                    audit.append(AuditEvent::WorkingDirectoryChanged {
                        cwd: cwd.display().to_string(),
                    })?;
                }
                audit.append(AuditEvent::ShellFinished {
                    command,
                    outcome,
                    cwd: cwd.display().to_string(),
                })?;
                if let Some(helper) = editor.helper_mut() {
                    helper.set_cwd(cwd.clone());
                }
            }
            InputRoute::Control(ControlCommand::Quit) => {
                sessions.sync(&active_model, &cwd, &history)?;
                let detached = sessions.active().cloned();
                if sessions.detach().is_ok() {
                    audit_logical_session_detached(&mut audit, detached.as_ref(), "quit")?;
                }
                break "quit";
            }
            InputRoute::Control(ControlCommand::Connect(connect_args)) => {
                let options = match parse_connect_options(&connect_args) {
                    Ok(options) => options,
                    Err(error) => {
                        eprintln!("xshell: {error:#}");
                        continue;
                    }
                };
                if let Err(error) = sessions.sync(&active_model, &cwd, &history) {
                    eprintln!("xshell: cannot synchronize current session: {error:#}");
                    continue;
                }
                match sessions.connect_ssh(
                    &options.destination,
                    options.session.as_deref(),
                    &model_config.session_fabric.default_session,
                    &active_model,
                    &args.system_prompt,
                ) {
                    Ok(snapshot) => {
                        apply_runtime_snapshot(
                            snapshot,
                            &mut active_model,
                            &mut agent,
                            &mut cwd,
                            &mut history,
                            &args.system_prompt,
                            &mut editor,
                            true,
                        )?;
                        refresh_shell_completions(&sessions, &mut editor);
                        audit_logical_session_attached(&mut audit, &sessions, "connect_ssh")?;
                        refresh_session_completions(&mut sessions, &mut editor);
                        println!("connected to {}", session_label(&sessions));
                    }
                    Err(error) => eprintln!("xshell: {error:#}"),
                }
            }
            InputRoute::Control(ControlCommand::Sessions) => {
                if let Err(error) = print_sessions(&mut sessions) {
                    eprintln!("xshell: {error:#}");
                }
                refresh_session_completions(&mut sessions, &mut editor);
            }
            InputRoute::Control(ControlCommand::Terminal(terminal_args)) => {
                match terminal_args.as_slice() {
                    [command] if command == "list" => match sessions.terminal_jobs() {
                        Ok(jobs) if jobs.is_empty() => println!("no terminal jobs"),
                        Ok(jobs) => {
                            for (session, terminal) in jobs {
                                let state = terminal.exit_status.as_deref().unwrap_or(
                                    if terminal.attached {
                                        "attached"
                                    } else {
                                        "running"
                                    },
                                );
                                println!(
                                    "{}:{}  {}  {} — {}",
                                    session.host_alias,
                                    session.name,
                                    state,
                                    session.cwd.display(),
                                    terminal.command
                                );
                            }
                        }
                        Err(error) => eprintln!("xshell: {error:#}"),
                    },
                    [command] if command == "kill" => {
                        if let Err(error) = sessions.pty_close_current() {
                            eprintln!("xshell: {error:#}");
                        } else {
                            println!("terminal job terminated");
                        }
                    }
                    _ if terminal_args.is_empty() || terminal_args.as_slice() == ["attach"] => {
                        let previous_session = sessions.active().map(|session| session.id.clone());
                        let result = run_existing_session_pty(&mut sessions, pty_escape);
                        match result {
                            Ok(outcome) if outcome != "exit status: 0" => {
                                println!("xshell: {outcome}")
                            }
                            Ok(_) => {}
                            Err(error) => eprintln!("xshell: {error:#}"),
                        }
                        if sessions.active().map(|session| &session.id) != previous_session.as_ref()
                        {
                            let snapshot = sessions.refresh_snapshot()?;
                            apply_runtime_snapshot(
                                snapshot,
                                &mut active_model,
                                &mut agent,
                                &mut cwd,
                                &mut history,
                                &args.system_prompt,
                                &mut editor,
                                true,
                            )?;
                            refresh_shell_completions(&sessions, &mut editor);
                            refresh_session_completions(&mut sessions, &mut editor);
                        }
                    }
                    _ => eprintln!("xshell: usage: //terminal [attach|list|kill]"),
                }
            }
            InputRoute::Control(ControlCommand::Switch(session_args)) => {
                let selector = match session_args.as_slice() {
                    [selector] => selector,
                    _ => {
                        eprintln!("xshell: usage: //switch SESSION");
                        continue;
                    }
                };
                match switch_session(
                    selector,
                    &mut sessions,
                    &mut active_model,
                    &mut agent,
                    &mut cwd,
                    &mut history,
                    &args.system_prompt,
                    &mut editor,
                ) {
                    Ok(()) => {
                        refresh_shell_completions(&sessions, &mut editor);
                        audit_logical_session_attached(&mut audit, &sessions, "switch")?;
                        refresh_session_completions(&mut sessions, &mut editor);
                        if let Err(error) = resume_active_terminal_if_running(
                            &mut sessions,
                            pty_escape,
                            &mut active_model,
                            &mut agent,
                            &mut cwd,
                            &mut history,
                            &args.system_prompt,
                            &mut editor,
                        ) {
                            eprintln!("xshell: could not resume terminal job: {error:#}");
                        }
                    }
                    Err(error) => eprintln!("xshell: {error:#}"),
                }
            }
            InputRoute::Control(ControlCommand::New(session_args)) => {
                match create_session(
                    session_args,
                    &model_config,
                    &mut sessions,
                    &mut active_model,
                    &mut agent,
                    &mut cwd,
                    &mut history,
                    &args.system_prompt,
                    &mut editor,
                ) {
                    Ok(()) => {
                        refresh_shell_completions(&sessions, &mut editor);
                        audit_logical_session_attached(&mut audit, &sessions, "create")?;
                        refresh_session_completions(&mut sessions, &mut editor);
                    }
                    Err(error) => eprintln!("xshell: {error:#}"),
                }
            }
            InputRoute::Control(ControlCommand::Detach) => {
                sessions.sync(&active_model, &cwd, &history)?;
                let detached = sessions.active().cloned();
                if let Err(error) = sessions.detach() {
                    eprintln!("xshell: {error:#}");
                    continue;
                }
                audit_logical_session_detached(&mut audit, detached.as_ref(), "detach")?;
                break "detach";
            }
            InputRoute::Control(ControlCommand::Close(close_args)) => {
                if !close_args.is_empty() {
                    eprintln!("xshell: usage: //close");
                    continue;
                }
                sessions.sync(&active_model, &cwd, &history)?;
                let closed = sessions.active().cloned();
                let fallback = match sessions.close_current_and_fallback() {
                    Ok(result) => result,
                    Err(error) => {
                        eprintln!("xshell: {error:#}");
                        continue;
                    }
                };
                audit_logical_session_detached(&mut audit, closed.as_ref(), "close")?;
                println!(
                    "closed session {}",
                    closed
                        .as_ref()
                        .map(|session| session.name.as_str())
                        .unwrap_or("(unknown)")
                );
                let Some(snapshot) = fallback else {
                    break "close";
                };
                restore_session_state(
                    snapshot,
                    &mut active_model,
                    &mut cwd,
                    &mut history,
                    &args.system_prompt,
                )?;
                agent = build_adapter(&active_model, false)?;
                if let Some(helper) = editor.helper_mut() {
                    helper.set_cwd(cwd.clone());
                }
                refresh_shell_completions(&sessions, &mut editor);
                audit_logical_session_attached(&mut audit, &sessions, "close_fallback")?;
                refresh_session_completions(&mut sessions, &mut editor);
                println!("switched to {}", session_label(&sessions));
            }
            InputRoute::Control(ControlCommand::Model(model_args)) => {
                if let Err(error) = handle_model_command(
                    model_args,
                    &model_config,
                    &mut active_model,
                    &mut agent,
                    &mut history,
                    &args.system_prompt,
                    &mut audit,
                    sessions.enabled(),
                ) {
                    eprintln!("xshell: {error:#}");
                }
            }
            InputRoute::Control(ControlCommand::View(view_args)) => {
                if let Err(error) = handle_view(
                    &view_args,
                    &mut sessions,
                    &cwd,
                    &viewers,
                    render_options,
                    &mut audit,
                ) {
                    eprintln!("xshell: {error:#}");
                }
            }
            InputRoute::Control(command) => handle_control(
                command,
                agent.as_ref(),
                &active_model,
                &audit,
                &sessions,
                &cwd,
                args.approval,
            ),
            InputRoute::Agent(message) => {
                if sessions.enabled() {
                    match run_daemon_turn(
                        &mut sessions,
                        TurnInput::Agent {
                            message: message.clone(),
                        },
                        args.approval,
                        &mut audit,
                        render_options,
                    ) {
                        Ok(snapshot) => apply_runtime_snapshot(
                            snapshot,
                            &mut active_model,
                            &mut agent,
                            &mut cwd,
                            &mut history,
                            &args.system_prompt,
                            &mut editor,
                            true,
                        )?,
                        Err(error) => {
                            let _ = audit.append_execution(AuditEvent::AgentError {
                                message: format!("{error:#}"),
                            });
                            eprintln!("xshell agent error: {error:#}");
                        }
                    }
                    continue;
                }
                let turn_policy = turn_policy_for(
                    &base_policy,
                    &model_config.session_fabric.compaction,
                    &active_model,
                );
                if let Err(error) = run_agent_turn(
                    agent.as_mut(),
                    &mut history,
                    message,
                    &cwd,
                    &turn_policy,
                    &mut audit,
                    render_options,
                )
                .await
                {
                    let _ = audit.append(AuditEvent::AgentError {
                        message: format!("{error:#}"),
                    });
                    eprintln!("xshell agent error: {error:#}");
                }
            }
            InputRoute::StickyShell(_) => {
                unreachable!("sticky shell routes are normalized before dispatch")
            }
        }
        sessions.sync(&active_model, &cwd, &history)?;
    };

    sessions.sync(&active_model, &cwd, &history)?;
    audit.close(exit_reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn approval_modes_parse() {
        assert_eq!(
            ApprovalPolicy::from_str("ask", false).unwrap(),
            ApprovalPolicy::Ask
        );
        assert_eq!(
            ApprovalPolicy::from_str("auto", false).unwrap(),
            ApprovalPolicy::Auto
        );
        assert_eq!(
            ApprovalPolicy::from_str("off", false).unwrap(),
            ApprovalPolicy::Off
        );
        assert!(ApprovalPolicy::from_str("yolo", false).is_err());
    }
}
