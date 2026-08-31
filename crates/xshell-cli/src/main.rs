mod audit;
mod completion;
mod config;
mod session;
mod tools;

use anyhow::{Context, Result, bail};
use audit::AuditRuntime;
use clap::{Parser, ValueEnum};
use completion::XshellHelper;
use config::{ActiveModel, ModelOverrides, Provider, XshellConfig};
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use session::SessionRuntime;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use xshell_adapters::{AgentAdapter, OllamaAdapter, OpenAiCompatibleAdapter};
use xshell_audit::AuditEvent;
use xshell_core::{
    AgentEvent, ChatMessage, ChatRequest, ControlCommand, DEFAULT_SYSTEM_PROMPT, InputRoute,
    ToolCall, classify_input,
};
use xshell_session::{PersistenceMode, SessionSnapshot, Visibility};

/// Maximum number of agent tool-call steps per turn before the loop
/// aborts. Kept bounded so a misbehaving model cannot loop forever.
const MAX_AGENT_STEPS: usize = 64;

/// Approval policy for tools that require user confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ApprovalMode {
    /// Prompt before shell execution.
    Ask,
    /// Run all tools without prompting.
    Auto,
    /// Deny shell execution while allowing read-only tools.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalDecision {
    Approve,
    Deny,
    AbortTurn,
}

impl std::fmt::Display for ApprovalMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Ask => "ask before shell execution",
            Self::Auto => "auto-run all tools",
            Self::Off => "deny shell execution",
        })
    }
}

fn resolve_approval(mode: ApprovalMode, gated: bool) -> bool {
    match mode {
        ApprovalMode::Ask | ApprovalMode::Off => !gated,
        ApprovalMode::Auto => true,
    }
}

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
    approval: ApprovalMode,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (model_config, config_path) = XshellConfig::load(args.config.as_deref())?;
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
    let mut agent = build_adapter(&active_model)?;
    let mut audit = AuditRuntime::start(&model_config.audit)?;
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

    let exit_reason = loop {
        let prompt = format!(
            "[{} {} {}] › ",
            session_label(&sessions),
            active_model_label(&active_model),
            compact_path(&cwd)
        );

        let line = match editor.readline(&prompt) {
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

        let route = classify_input(&line);
        if !matches!(route, InputRoute::Empty) {
            audit.append(AuditEvent::Input {
                route: input_route_name(&route).into(),
                text: line.clone(),
            })?;
        }

        match route {
            InputRoute::Empty => {}
            InputRoute::Shell(command) => {
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
            InputRoute::Control(ControlCommand::Sessions) => {
                if let Err(error) = print_sessions(&mut sessions) {
                    eprintln!("xshell: {error:#}");
                }
                refresh_session_completions(&mut sessions, &mut editor);
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
                        audit_logical_session_attached(&mut audit, &sessions, "switch")?;
                        refresh_session_completions(&mut sessions, &mut editor);
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
                agent = build_adapter(&active_model)?;
                if let Some(helper) = editor.helper_mut() {
                    helper.set_cwd(cwd.clone());
                }
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
                if let Err(error) = run_agent_turn(
                    agent.as_mut(),
                    &mut history,
                    message,
                    &cwd,
                    args.approval,
                    &mut audit,
                )
                .await
                {
                    let _ = audit.append(AuditEvent::AgentError {
                        message: format!("{error:#}"),
                    });
                    eprintln!("xshell agent error: {error:#}");
                }
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
        InputRoute::Shell(_) => "shell",
        InputRoute::Control(_) => "control",
        InputRoute::Empty => "empty",
    }
}

async fn run_agent_turn(
    agent: &mut dyn AgentAdapter,
    history: &mut Vec<ChatMessage>,
    message: String,
    cwd: &Path,
    approval: ApprovalMode,
    audit: &mut AuditRuntime,
) -> Result<()> {
    let checkpoint = history.len();
    history.push(ChatMessage::user(message));
    let definitions = tools::definitions();

    for _ in 0..MAX_AGENT_STEPS {
        let mut streamed_text = String::new();
        let mut emit = |event| match event {
            AgentEvent::TextDelta(delta) => {
                streamed_text.push_str(&delta);
                print!("{delta}");
                let _ = io::stdout().flush();
            }
        };
        let response = match agent
            .chat_stream(
                ChatRequest {
                    messages: history.clone(),
                    tools: definitions.clone(),
                },
                &mut emit,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if !streamed_text.is_empty() {
                    audit.append(AuditEvent::AgentResponse {
                        content: streamed_text,
                        tool_call_count: 0,
                        partial: true,
                    })?;
                }
                history.truncate(checkpoint);
                return Err(error.into());
            }
        };
        if !streamed_text.is_empty() {
            println!();
        }

        audit.append(AuditEvent::AgentResponse {
            content: response.content.clone(),
            tool_call_count: response.tool_calls.len(),
            partial: false,
        })?;
        for call in &response.tool_calls {
            audit.append(AuditEvent::ToolRequested {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })?;
        }
        history.push(ChatMessage::assistant_with_tools(
            response.content,
            response.tool_calls.clone(),
        ));
        if response.tool_calls.is_empty() {
            println!();
            return Ok(());
        }

        for (index, call) in response.tool_calls.iter().enumerate() {
            println!("\nagent requests: {}", tools::summary(call));
            // Approval policy: `auto` runs every tool, `off` denies
            // gated (shell) tools, and `ask` (default) prompts for them.
            let gated = tools::requires_approval(call);
            let decision = if resolve_approval(approval, gated) {
                ApprovalDecision::Approve
            } else if approval == ApprovalMode::Ask && gated {
                confirm_tool(call)?
            } else {
                ApprovalDecision::Deny
            };
            audit.append(AuditEvent::ToolDecision {
                call_id: call.id.clone(),
                decision: approval_decision_name(decision).into(),
            })?;

            if decision == ApprovalDecision::AbortTurn {
                for skipped in &response.tool_calls[index..] {
                    history.push(ChatMessage::tool_result(
                        skipped,
                        "tool execution aborted by user; agent turn stopped",
                    ));
                }
                for skipped in &response.tool_calls[index + 1..] {
                    audit.append(AuditEvent::ToolDecision {
                        call_id: skipped.id.clone(),
                        decision: "skipped_after_abort".into(),
                    })?;
                }
                println!("agent turn aborted; no remaining tools were executed\n");
                return Ok(());
            }

            let result = if decision == ApprovalDecision::Approve {
                if !tools::requires_approval(call) {
                    println!("policy: allowed read-only tool within {}", cwd.display());
                }
                tools::execute(call, cwd).await
            } else {
                "tool denied by user".into()
            };
            audit.append(AuditEvent::ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                result: result.clone(),
            })?;
            print_tool_result(&result);
            history.push(ChatMessage::tool_result(call, result));
        }
    }

    bail!("agent exceeded the {MAX_AGENT_STEPS}-step tool-call limit")
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
        print!("Approve `{}`? [y/N/q] ", tools::summary(call));
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
    println!("tool result:\n{}", &result[..end]);
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

fn build_adapter(active: &ActiveModel) -> Result<Box<dyn AgentAdapter>> {
    let adapter: Box<dyn AgentAdapter> = match active.provider {
        Provider::Ollama => Box::new(OllamaAdapter::new(&active.base_url, &active.model)),
        Provider::Openai => Box::new(OpenAiCompatibleAdapter::new(
            &active.base_url,
            &active.model,
            resolve_api_key(active)?,
        )),
    };
    Ok(adapter)
}

fn resolve_api_key(active: &ActiveModel) -> Result<Option<String>> {
    let Some(variable) = &active.api_key_env else {
        return Ok(None);
    };
    let value = env::var(variable).context(
        "the configured credential environment variable is not set or is not valid Unicode",
    )?;
    if value.is_empty() {
        bail!("the configured credential environment variable is empty");
    }
    Ok(Some(value))
}

fn handle_model_command(
    args: Vec<String>,
    config: &XshellConfig,
    active: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    history: &mut Vec<ChatMessage>,
    system_prompt: &str,
    audit: &mut AuditRuntime,
) -> Result<()> {
    if args.is_empty() || args == ["show"] {
        print_model(active);
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
    switch_model_profile(name, config, active, agent, history, system_prompt, audit)
}

fn switch_model_profile(
    name: &str,
    config: &XshellConfig,
    active: &mut ActiveModel,
    agent: &mut Box<dyn AgentAdapter>,
    history: &mut Vec<ChatMessage>,
    system_prompt: &str,
    audit: &mut AuditRuntime,
) -> Result<()> {
    let next = config.resolve_profile(name)?;
    if next == *active {
        println!("model profile {name:?} is already active");
        return Ok(());
    }

    let next_agent = build_adapter(&next)?;
    audit.append(AuditEvent::ModelSwitched {
        profile: name.into(),
        model: next.model.clone(),
    })?;
    *agent = next_agent;
    *active = next;
    history.clear();
    history.push(ChatMessage::system(system_prompt));
    println!("switched to model profile {name:?}; conversation history was cleared");
    print_model(active);
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

fn print_model(active: &ActiveModel) {
    println!(
        "profile: {}",
        active.profile_name.as_deref().unwrap_or("(command-line)")
    );
    println!("provider: {:?}", active.provider);
    println!("model: {}", active.model);
    println!("endpoint: {}", active.base_url);
    if active.provider == Provider::Openai {
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
    let restored_cwd = snapshot.descriptor.cwd.canonicalize().with_context(|| {
        format!(
            "session {:?} working directory is unavailable: {}",
            snapshot.descriptor.name,
            snapshot.descriptor.cwd.display()
        )
    })?;
    *active_model = restored_model;
    *cwd = restored_cwd;
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
    *agent = build_adapter(active_model)?;
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
    *agent = build_adapter(active_model)?;
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
        println!("no sessions on this host");
        return Ok(());
    }
    for session in catalog {
        let marker = if active_id.as_deref() == Some(session.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {}/{}:{} — {} / {} — {:?}, {:?}, {:?}",
            session.host_alias,
            session.user,
            session.name,
            session.model.profile_name.as_deref().unwrap_or("custom"),
            session.model.model,
            session.status,
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

fn handle_control(
    command: ControlCommand,
    agent: &dyn AgentAdapter,
    active_model: &ActiveModel,
    audit: &AuditRuntime,
    sessions: &SessionRuntime,
    cwd: &Path,
    approval: ApprovalMode,
) {
    match command {
        ControlCommand::Help => println!(
            "\
xshell input routes:
  plain text        send a message to the active agent
  $COMMAND          run COMMAND using the configured shell

control commands:
  //help            show this help
  //status          show local session state
  //sessions        list named sessions on the current host
  //new NAME        create and switch to a daemon-lifetime session
  //switch NAME     switch to another local session
  //detach          detach, preserving a persistent session, and exit
  //close           delete the current session and return to the previous one
  //audit           show audit session state
  //model           show the active model profile
  //model list      list configured model profiles
  //model NAME      switch profiles and start a fresh conversation
  //agent            show active agent capabilities
  //tools            show tools exposed to the active agent
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
        ControlCommand::Sessions
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
    approval: ApprovalMode,
) {
    let descriptor = agent.descriptor();
    println!("session: {}", session_label(sessions));
    if let Some(socket) = sessions.socket() {
        println!("session service: {}", socket.display());
    } else {
        println!("session service: disabled");
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
    let status = Command::new(&shell)
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("could not launch shell {shell}"))?;
    if !status.success() {
        eprintln!("xshell: command exited with {status}");
    }
    Ok(format!("exit status: {status}"))
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

    #[test]
    fn compact_path_leaves_non_home_paths_alone() {
        let path = Path::new("/not-the-home-directory/project");
        assert_eq!(compact_path(path), path.display().to_string());
    }

    #[test]
    fn approval_modes_parse() {
        assert_eq!(
            ApprovalMode::from_str("ask", false).unwrap(),
            ApprovalMode::Ask
        );
        assert_eq!(
            ApprovalMode::from_str("auto", false).unwrap(),
            ApprovalMode::Auto
        );
        assert_eq!(
            ApprovalMode::from_str("off", false).unwrap(),
            ApprovalMode::Off
        );
        assert!(ApprovalMode::from_str("yolo", false).is_err());
    }

    #[test]
    fn approval_policy_handles_gated_and_read_only_tools() {
        assert!(!resolve_approval(ApprovalMode::Ask, true));
        assert!(resolve_approval(ApprovalMode::Ask, false));
        assert!(resolve_approval(ApprovalMode::Auto, true));
        assert!(resolve_approval(ApprovalMode::Auto, false));
        assert!(!resolve_approval(ApprovalMode::Off, true));
        assert!(resolve_approval(ApprovalMode::Off, false));
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
}
