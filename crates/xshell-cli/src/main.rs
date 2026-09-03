mod audit;
mod completion;
mod config;
mod session;
mod tools;

use anyhow::{Context, Result, bail};
use audit::AuditRuntime;
use clap::Parser;
use completion::XshellHelper;
use config::{ActiveModel, ModelOverrides, OutputMode, Provider, XshellConfig};
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use session::SessionRuntime;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use xshell_adapters::AgentAdapter;
use xshell_audit::AuditEvent;
use xshell_core::{
    ChatMessage, ControlCommand, DEFAULT_SYSTEM_PROMPT, InputRoute, ToolCall, classify_input,
};
use xshell_execution::{
    AdapterConfig, ApprovalDecision, ApprovalPolicy, CancellationFlag, ExecutionEvent,
    TurnObserver, build_adapter as build_execution_adapter, tool_summary,
};
use xshell_session::{
    PersistenceMode, SessionEventKind, SessionSnapshot, TurnInput, Visibility, load_view_resource,
};
use xshell_view::{
    AgentRenderer, RenderOptions, ViewInput, ViewerRegistry, escape_for_prompt,
    sanitize_terminal_text,
};

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
                if let Err(error) = run_agent_turn(
                    agent.as_mut(),
                    &mut history,
                    message,
                    &cwd,
                    args.approval,
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

fn input_route_name(route: &InputRoute) -> &'static str {
    match route {
        InputRoute::Agent(_) => "agent",
        InputRoute::Shell(_) | InputRoute::StickyShell(_) => "shell",
        InputRoute::Control(_) => "control",
        InputRoute::Empty => "empty",
    }
}

fn apply_sticky_shell_mode(route: InputRoute, sticky: &mut bool) -> InputRoute {
    match route {
        InputRoute::StickyShell(command) => {
            *sticky = true;
            InputRoute::Shell(command)
        }
        InputRoute::Shell(command) => InputRoute::Shell(command),
        route => {
            *sticky = false;
            route
        }
    }
}

fn run_daemon_turn(
    sessions: &mut SessionRuntime,
    input: TurnInput,
    approval: ApprovalPolicy,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
) -> Result<SessionSnapshot> {
    sessions.submit(input, approval)?;
    follow_daemon_turn(sessions, audit, render_options)
}

fn follow_daemon_turn(
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
                    ExecutionEvent::ApprovalRequested { call } => {
                        let decision = confirm_tool(&call)?;
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
fn apply_runtime_snapshot(
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
struct LocalObserver<'a> {
    audit: &'a mut AuditRuntime,
    render_options: RenderOptions,
    renderer: AgentRenderer,
    cwd: &'a Path,
    cancellation: CancellationFlag,
    /// Whether the most recent `ToolDecision` approved execution, so the
    /// read-only policy note is printed only for tools that actually ran.
    last_approved: bool,
    /// First error raised while rendering or auditing. The engine's observer
    /// interface is infallible, so failures are captured here and surfaced
    /// after the turn returns.
    failure: Option<anyhow::Error>,
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
            ExecutionEvent::ApprovalRequested { .. } => Ok(()),
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
                if self.last_approved && !tools::requires_approval_by_name(&name) {
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
        };
        self.record(result);
    }

    fn cancellation(&self) -> CancellationFlag {
        self.cancellation.clone()
    }

    async fn approve(&mut self, call: &ToolCall) -> ApprovalDecision {
        match confirm_tool(call) {
            Ok(decision) => decision,
            Err(error) => {
                self.record(Err(error));
                ApprovalDecision::AbortTurn
            }
        }
    }
}

async fn run_agent_turn(
    agent: &mut dyn AgentAdapter,
    history: &mut Vec<ChatMessage>,
    message: String,
    cwd: &Path,
    approval: ApprovalPolicy,
    audit: &mut AuditRuntime,
    render_options: RenderOptions,
) -> Result<()> {
    let mut observer = LocalObserver::new(audit, render_options, cwd);
    let outcome =
        xshell_execution::run_agent_turn(agent, history, message, cwd, approval, &mut observer)
            .await;
    observer.finish(outcome)
}

fn approval_decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approve => "approve",
        ApprovalDecision::Deny => "deny",
        ApprovalDecision::AbortTurn => "abort_turn",
    }
}

fn confirm_tool(call: &ToolCall) -> Result<ApprovalDecision> {
    loop {
        // Tool arguments are model-controlled. Escape them so the command the
        // user approves is exactly the command that will run: no control
        // sequences can redraw the line, and embedded newlines are visible.
        print!(
            "Approve `{}`? [y/N/q] ",
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

fn parse_approval_response(answer: &str) -> Option<ApprovalDecision> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalDecision::Approve),
        "" | "n" | "no" => Some(ApprovalDecision::Deny),
        "q" | "quit" | "abort" => Some(ApprovalDecision::AbortTurn),
        _ => None,
    }
}

fn print_tool_result(result: &str) {
    const DISPLAY_LIMIT: usize = 4 * 1024;
    let end = floor_char_boundary(result, result.len().min(DISPLAY_LIMIT));
    // Tool output (file contents, command stdout) is untrusted terminal text.
    println!("tool result:\n{}", sanitize_terminal_text(&result[..end]));
    if end < result.len() {
        println!("[terminal display truncated; full result returned to agent]");
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn build_adapter(active: &ActiveModel, include_credentials: bool) -> Result<Box<dyn AgentAdapter>> {
    build_execution_adapter(&AdapterConfig {
        provider: match active.provider {
            Provider::Ollama => "ollama",
            Provider::Openai => "openai",
        }
        .into(),
        model: active.model.clone(),
        base_url: active.base_url.clone(),
        api_key_env: include_credentials
            .then(|| active.api_key_env.clone())
            .flatten(),
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_model_command(
    args: Vec<String>,
    config: &XshellConfig,
    active: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    history: &mut Vec<ChatMessage>,
    system_prompt: &str,
    audit: &mut AuditRuntime,
    daemon_owned: bool,
) -> Result<()> {
    if args.is_empty() || args == ["show"] {
        print_model(active, daemon_owned);
        return Ok(());
    }
    if args == ["list"] {
        print_model_profiles(config, active);
        return Ok(());
    }

    let name = match args.as_slice() {
        [name] => name,
        [command, name] if command == "use" => name,
        _ => bail!("usage: //model [show|list|PROFILE] or //model use PROFILE"),
    };
    switch_model_profile(
        name,
        config,
        active,
        agent,
        history,
        system_prompt,
        audit,
        daemon_owned,
    )
}

#[allow(clippy::too_many_arguments)]
fn switch_model_profile(
    name: &str,
    config: &XshellConfig,
    active: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    history: &mut Vec<ChatMessage>,
    system_prompt: &str,
    audit: &mut AuditRuntime,
    daemon_owned: bool,
) -> Result<()> {
    let next = config.resolve_profile(name)?;
    if next == *active {
        println!("model profile {name:?} is already active");
        return Ok(());
    }

    let next_agent = build_adapter(&next, !daemon_owned)?;
    audit.append(AuditEvent::ModelSwitched {
        profile: name.into(),
        model: next.model.clone(),
    })?;
    *agent = next_agent;
    *active = next;
    history.clear();
    history.push(ChatMessage::system(system_prompt));
    println!("switched to model profile {name:?}; conversation history was cleared");
    print_model(active, daemon_owned);
    Ok(())
}

fn print_model_profiles(config: &XshellConfig, active: &ActiveModel) {
    if config.models.is_empty() {
        println!("no named model profiles are configured");
        return;
    }
    for (name, profile) in &config.models {
        let marker = if active.profile_name.as_deref() == Some(name) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {name} — {:?} / {}",
            profile.provider, profile.model
        );
    }
}

fn print_model(active: &ActiveModel, daemon_owned: bool) {
    println!(
        "profile: {}",
        active.profile_name.as_deref().unwrap_or("(command-line)")
    );
    println!("provider: {:?}", active.provider);
    println!("model: {}", active.model);
    println!("endpoint: {}", active.base_url);
    if active.provider == Provider::Openai {
        if daemon_owned {
            println!("credentials: resolved by xshelld");
            return;
        }
        let credential_status = match active.api_key_env.as_deref() {
            Some(variable) if env::var_os(variable).is_some() => "set",
            Some(_) => "missing",
            None => "not configured",
        };
        println!("credentials: {credential_status}");
    }
}

fn active_model_label(active: &ActiveModel) -> &str {
    active.profile_name.as_deref().unwrap_or(&active.model)
}

fn session_label(sessions: &SessionRuntime) -> String {
    sessions.active().map_or_else(
        || "local:standalone".into(),
        |session| format!("{}:{}", session.host_alias, session.name),
    )
}

fn audit_logical_session_attached(
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
    })
}

fn audit_logical_session_detached(
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

fn restore_session_state(
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
fn switch_session(
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

struct NewSessionOptions {
    name: String,
    profile: Option<String>,
    persistence: PersistenceMode,
    visibility: Visibility,
}

struct ConnectOptions {
    destination: String,
    session: Option<String>,
}

fn parse_connect_options(args: &[String]) -> Result<ConnectOptions> {
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

fn parse_new_session_options(args: &[String]) -> Result<NewSessionOptions> {
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
fn create_session(
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

fn print_sessions(sessions: &mut SessionRuntime) -> Result<()> {
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

fn refresh_session_completions(
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

fn refresh_shell_completions(
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

struct ViewOptions {
    path: PathBuf,
    viewer: Option<String>,
}

fn parse_view_options(arguments: &str) -> Result<ViewOptions> {
    let words = shell_words::split(arguments).context("invalid //view quoting")?;
    let mut path = None;
    let mut viewer = None;
    let mut parse_options = true;
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        if parse_options && word == "--" {
            parse_options = false;
        } else if parse_options && word == "--as" {
            index += 1;
            let value = words
                .get(index)
                .context("//view --as requires a viewer name")?;
            if viewer.replace(value.clone()).is_some() {
                bail!("//view accepts --as only once");
            }
        } else if parse_options && word.starts_with("--as=") {
            let value = word.trim_start_matches("--as=");
            if value.is_empty() {
                bail!("//view --as requires a viewer name");
            }
            if viewer.replace(value.into()).is_some() {
                bail!("//view accepts --as only once");
            }
        } else if parse_options && word.starts_with('-') {
            bail!("unknown //view option {word:?}");
        } else if path.replace(PathBuf::from(word)).is_some() {
            bail!("//view accepts exactly one path");
        }
        index += 1;
    }
    Ok(ViewOptions {
        path: path.context("usage: //view [--as VIEWER] PATH")?,
        viewer,
    })
}

fn handle_view(
    arguments: &str,
    sessions: &mut SessionRuntime,
    cwd: &Path,
    viewers: &ViewerRegistry,
    render_options: RenderOptions,
    audit: &mut AuditRuntime,
) -> Result<()> {
    let options = parse_view_options(arguments)?;
    let requested_path = options.path.display().to_string();
    let resource = match if sessions.enabled() {
        sessions.view_source(options.path.clone())
    } else {
        load_view_resource(&options.path, cwd)
    } {
        Ok(resource) => resource,
        Err(error) => {
            audit.append(AuditEvent::ViewOperation {
                path: requested_path,
                sha256: None,
                viewer: options.viewer,
                media_type: None,
                byte_len: None,
                outcome: format!("acquisition failed: {error:#}"),
            })?;
            return Err(error);
        }
    };

    let rendered = match viewers.render(
        &ViewInput {
            name: &resource.path.to_string_lossy(),
            media_type: &resource.media_type,
            text: &resource.content,
        },
        options.viewer.as_deref(),
        render_options,
    ) {
        Ok(rendered) => rendered,
        Err(error) => {
            audit.append(AuditEvent::ViewOperation {
                path: resource.path.display().to_string(),
                sha256: Some(resource.sha256),
                viewer: options.viewer,
                media_type: Some(resource.media_type),
                byte_len: Some(resource.byte_len),
                outcome: format!("render failed: {error:#}"),
            })?;
            return Err(error);
        }
    };

    let mut stdout = io::stdout();
    if let Err(error) = stdout
        .write_all(&rendered.bytes)
        .and_then(|()| stdout.flush())
    {
        audit.append(AuditEvent::ViewOperation {
            path: resource.path.display().to_string(),
            sha256: Some(resource.sha256),
            viewer: Some(rendered.viewer_id),
            media_type: Some(resource.media_type),
            byte_len: Some(resource.byte_len),
            outcome: format!("display failed: {error}"),
        })?;
        return Err(error).context("cannot display view resource");
    }
    audit.append(AuditEvent::ViewOperation {
        path: resource.path.display().to_string(),
        sha256: Some(resource.sha256),
        viewer: Some(rendered.viewer_id),
        media_type: Some(resource.media_type),
        byte_len: Some(resource.byte_len),
        outcome: "rendered".into(),
    })?;
    Ok(())
}

fn handle_control(
    command: ControlCommand,
    agent: &dyn AgentAdapter,
    active_model: &ActiveModel,
    audit: &AuditRuntime,
    sessions: &SessionRuntime,
    cwd: &Path,
    approval: ApprovalPolicy,
) {
    match command {
        ControlCommand::Help => println!(
            "\
xshell input routes:
  plain text        send a message to the active agent
  $COMMAND          run COMMAND using the configured shell
  $$COMMAND         run COMMAND and keep `$` inserted for following inputs

control commands:
  //help            show this help
  //status          show local session state
  //connect DEST    connect to xshelld on an SSH host
  //sessions        list named sessions on connected hosts
  //terminal        attach the current session's terminal job
  //terminal list   list terminal jobs on connected hosts
  //terminal kill   terminate the current session's terminal job
  //new NAME        create and switch to a daemon-lifetime session
  //switch SESSION  switch locally or across connected hosts
  //detach          detach, preserving a persistent session, and exit
  //close           delete the current session and return to the previous one
  //audit           show audit session state
  //model           show the active model profile
  //model list      list configured model profiles
  //model NAME      switch profiles and start a fresh conversation
  //agent            show active agent capabilities
  //tools            show tools exposed to the active agent
  //view PATH        render a Markdown or reStructuredText file
  //quit             detach from the current session and exit xshell"
        ),
        ControlCommand::Status => print_status(agent, active_model, audit, sessions, cwd, approval),
        ControlCommand::Audit(args) if args.is_empty() || args == ["status"] => {
            print_audit_status(audit)
        }
        ControlCommand::Audit(args) => {
            eprintln!("xshell: unsupported //audit arguments {args:?}; try //audit status")
        }
        ControlCommand::Tools => {
            for tool in tools::definitions() {
                println!("{} — {}", tool.name, tool.description);
            }
        }
        ControlCommand::Agent(args) if args.is_empty() || args == ["show"] => print_agent(agent),
        ControlCommand::Agent(args) => eprintln!(
            "xshell: //agent {:?} is not implemented; configure the adapter at startup",
            args
        ),
        ControlCommand::Unknown { name, .. } => {
            eprintln!("xshell: unknown control command //{name}; try //help")
        }
        ControlCommand::Model(_) => unreachable!("model is handled by the REPL"),
        ControlCommand::View(_) => unreachable!("view is handled by the REPL"),
        ControlCommand::Connect(_)
        | ControlCommand::Sessions
        | ControlCommand::Terminal(_)
        | ControlCommand::New(_)
        | ControlCommand::Switch(_)
        | ControlCommand::Detach
        | ControlCommand::Close(_) => unreachable!("session command is handled by the REPL"),
        ControlCommand::Quit => unreachable!("quit is handled by the REPL"),
    }
}

fn print_status(
    agent: &dyn AgentAdapter,
    active_model: &ActiveModel,
    audit: &AuditRuntime,
    sessions: &SessionRuntime,
    cwd: &Path,
    approval: ApprovalPolicy,
) {
    let descriptor = agent.descriptor();
    println!("session: {}", session_label(sessions));
    if let Some(service) = sessions.service_label() {
        println!("session service: {service}");
        println!("execution owner: xshelld");
    } else {
        println!("session service: disabled");
        println!("execution owner: xshell CLI");
    }
    println!("cwd: {}", cwd.display());
    println!("agent: {} ({})", descriptor.display_name, descriptor.id);
    println!("profile: {}", active_model_label(active_model));
    println!("model: {}", descriptor.model);
    println!("capabilities: {}", descriptor.capabilities.join(", "));
    println!("approval mode: auto-read within cwd; {}", approval);
    print_audit_status(audit);
}

fn print_audit_status(audit: &AuditRuntime) {
    match audit.session_id() {
        Some(session_id) => {
            println!("audit session: {session_id}");
            println!(
                "audit signing key: {}",
                audit.signing_key_id().unwrap_or("unknown")
            );
        }
        None => println!("audit: disabled"),
    }
}

fn print_agent(agent: &dyn AgentAdapter) {
    let descriptor = agent.descriptor();
    println!("{} / {}", descriptor.display_name, descriptor.model);
    println!("id: {}", descriptor.id);
    println!("capabilities: {}", descriptor.capabilities.join(", "));
}

fn run_shell(command: &str, cwd: &mut PathBuf) -> Result<String> {
    if command.trim().is_empty() {
        return Ok("empty command".into());
    }

    let words = shell_words::split(command).context("could not parse shell command")?;
    if words.first().map(String::as_str) == Some("cd") {
        if words.len() > 2 {
            bail!("cd expects zero or one path");
        }
        let destination = match words.get(1) {
            Some(path) => expand_tilde(path)?,
            None => home_dir()?,
        };
        let next = if destination.is_absolute() {
            destination
        } else {
            cwd.join(destination)
        };
        *cwd = next
            .canonicalize()
            .with_context(|| format!("cannot cd to {}", next.display()))?;
        return Ok("working directory changed".into());
    }

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let status = if xshell_pty::controller_is_terminal() {
        xshell_pty::run(command, cwd)?
    } else {
        Command::new(&shell)
            .arg("-lc")
            .arg(command)
            .current_dir(cwd)
            .status()
            .with_context(|| format!("could not launch shell {shell}"))?
    };
    if !status.success() {
        eprintln!("xshell: command exited with {status}");
    }
    Ok(format!("exit status: {status}"))
}

struct TerminalFocusOutcome {
    description: String,
}

fn run_session_pty(
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

fn run_existing_session_pty(sessions: &mut SessionRuntime, escape_prefix: u8) -> Result<String> {
    let (mut pty_id, mut stream) = sessions.pty_attach_stream()?;
    Ok(run_pty_focus_loop(sessions, &mut pty_id, &mut stream, escape_prefix)?.description)
}

#[allow(clippy::too_many_arguments)]
fn resume_active_terminal_if_running(
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

fn run_pty_focus_loop(
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

fn choose_terminal_target(
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

fn choose_terminal_interactively(
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

fn is_simple_cd(command: &str) -> bool {
    shell_words::split(command)
        .is_ok_and(|words| words.len() <= 2 && words.first().map(String::as_str) == Some("cd"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
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

fn compact_path(path: &Path) -> String {
    if let Some(home) = env::var_os("HOME").map(PathBuf::from)
        && let Ok(relative) = path.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

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
    fn compact_path_leaves_non_home_paths_alone() {
        let path = Path::new("/not-the-home-directory/project");
        assert_eq!(compact_path(path), path.display().to_string());
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

    #[test]
    fn double_dollar_enters_sticky_shell_until_prefix_is_removed() {
        let mut sticky = false;
        assert_eq!(
            apply_sticky_shell_mode(classify_input("$$pwd"), &mut sticky),
            InputRoute::Shell("pwd".into())
        );
        assert!(sticky);
        assert_eq!(
            apply_sticky_shell_mode(classify_input("$ls"), &mut sticky),
            InputRoute::Shell("ls".into())
        );
        assert!(sticky);
        assert_eq!(
            apply_sticky_shell_mode(classify_input("explain this"), &mut sticky),
            InputRoute::Agent("explain this".into())
        );
        assert!(!sticky);
    }

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

    #[test]
    fn parses_view_path_and_explicit_viewer() {
        let options = parse_view_options("--as rst \"docs/design notes.rst\"").unwrap();
        assert_eq!(options.path, Path::new("docs/design notes.rst"));
        assert_eq!(options.viewer.as_deref(), Some("rst"));

        let options = parse_view_options("--as=markdown -- -draft.md").unwrap();
        assert_eq!(options.path, Path::new("-draft.md"));
        assert_eq!(options.viewer.as_deref(), Some("markdown"));
        assert!(parse_view_options("").is_err());
        assert!(parse_view_options("one.md two.md").is_err());
    }

    #[test]
    fn only_simple_cd_commands_use_session_cwd_updates() {
        assert!(is_simple_cd("cd"));
        assert!(is_simple_cd("cd 'design files'"));
        assert!(!is_simple_cd("cd /tmp && pwd"));
        assert!(!is_simple_cd("printf cd"));
    }
}
