mod completion;
mod tools;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use completion::XshellHelper;
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use xshell_adapters::{AgentAdapter, OllamaAdapter, OpenAiCompatibleAdapter};
use xshell_core::{
    AgentEvent, ChatMessage, ChatRequest, ControlCommand, DEFAULT_SYSTEM_PROMPT, InputRoute,
    ToolCall, classify_input,
};

/// Maximum number of agent tool-call steps per turn before the loop
/// aborts. Kept bounded so a misbehaving model cannot loop forever.
const MAX_AGENT_STEPS: usize = 64;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Provider {
    Ollama,
    Openai,
}

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
    #[arg(long, env = "XSHELL_PROVIDER", value_enum, default_value = "ollama")]
    provider: Provider,

    #[arg(long, env = "XSHELL_MODEL", default_value = "qwen3:8b")]
    model: String,

    #[arg(long, env = "XSHELL_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, env = "XSHELL_API_KEY_ENV", default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    #[arg(long, env = "XSHELL_SYSTEM_PROMPT", default_value = DEFAULT_SYSTEM_PROMPT)]
    system_prompt: String,

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
    let mut cwd = args
        .cwd
        .canonicalize()
        .with_context(|| format!("cannot use working directory {}", args.cwd.display()))?;
    let mut agent = build_adapter(&args);
    let mut history = vec![ChatMessage::system(args.system_prompt.clone())];
    let mut editor = Editor::<XshellHelper, DefaultHistory>::new()
        .context("could not initialize terminal input")?;
    editor.set_helper(Some(XshellHelper::new(cwd.clone())));

    println!("xshell local prototype — //help for commands; //quit or Ctrl-D to exit");
    print_status(agent.as_ref(), &cwd, args.approval);

    loop {
        let descriptor = agent.descriptor();
        let prompt = format!(
            "[local:default {} {}] › ",
            descriptor.id,
            compact_path(&cwd)
        );

        let line = match editor.readline(&prompt) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(error) => return Err(error).context("terminal input failed"),
        };

        if !line.trim().is_empty() {
            let _ = editor.add_history_entry(line.as_str());
        }

        match classify_input(&line) {
            InputRoute::Empty => {}
            InputRoute::Shell(command) => {
                if let Err(error) = run_shell(&command, &mut cwd) {
                    eprintln!("xshell: {error:#}");
                }
                if let Some(helper) = editor.helper_mut() {
                    helper.set_cwd(cwd.clone());
                }
            }
            InputRoute::Control(ControlCommand::Quit) => break,
            InputRoute::Control(command) => {
                handle_control(command, agent.as_ref(), &cwd, args.approval)
            }
            InputRoute::Agent(message) => {
                if let Err(error) =
                    run_agent_turn(agent.as_mut(), &mut history, message, &cwd, args.approval).await
                {
                    eprintln!("xshell agent error: {error:#}");
                }
            }
        }
    }

    Ok(())
}

async fn run_agent_turn(
    agent: &mut dyn AgentAdapter,
    history: &mut Vec<ChatMessage>,
    message: String,
    cwd: &Path,
    approval: ApprovalMode,
) -> Result<()> {
    let checkpoint = history.len();
    history.push(ChatMessage::user(message));
    let definitions = tools::definitions();

    for _ in 0..MAX_AGENT_STEPS {
        let mut streamed_text = false;
        let mut emit = |event| match event {
            AgentEvent::TextDelta(delta) => {
                streamed_text = true;
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
                history.truncate(checkpoint);
                return Err(error.into());
            }
        };
        if streamed_text {
            println!();
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

            if decision == ApprovalDecision::AbortTurn {
                for skipped in &response.tool_calls[index..] {
                    history.push(ChatMessage::tool_result(
                        skipped,
                        "tool execution aborted by user; agent turn stopped",
                    ));
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
            print_tool_result(&result);
            history.push(ChatMessage::tool_result(call, result));
        }
    }

    bail!("agent exceeded the {MAX_AGENT_STEPS}-step tool-call limit")
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

fn build_adapter(args: &Args) -> Box<dyn AgentAdapter> {
    match args.provider {
        Provider::Ollama => Box::new(OllamaAdapter::new(
            args.base_url.as_deref().unwrap_or("http://127.0.0.1:11434"),
            &args.model,
        )),
        Provider::Openai => Box::new(OpenAiCompatibleAdapter::new(
            args.base_url.as_deref().unwrap_or("https://api.openai.com"),
            &args.model,
            env::var(&args.api_key_env).ok(),
        )),
    }
}

fn handle_control(
    command: ControlCommand,
    agent: &dyn AgentAdapter,
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
  //agent            show active agent capabilities
  //tools            show tools exposed to the active agent
  //quit             exit xshell"
        ),
        ControlCommand::Status => print_status(agent, cwd, approval),
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
        ControlCommand::Quit => unreachable!("quit is handled by the REPL"),
    }
}

fn print_status(agent: &dyn AgentAdapter, cwd: &Path, approval: ApprovalMode) {
    let descriptor = agent.descriptor();
    println!("session: local:default");
    println!("cwd: {}", cwd.display());
    println!("agent: {} ({})", descriptor.display_name, descriptor.id);
    println!("model: {}", descriptor.model);
    println!("capabilities: {}", descriptor.capabilities.join(", "));
    println!("approval mode: auto-read within cwd; {}", approval);
}

fn print_agent(agent: &dyn AgentAdapter) {
    let descriptor = agent.descriptor();
    println!("{} / {}", descriptor.display_name, descriptor.model);
    println!("id: {}", descriptor.id);
    println!("capabilities: {}", descriptor.capabilities.join(", "));
}

fn run_shell(command: &str, cwd: &mut PathBuf) -> Result<()> {
    if command.trim().is_empty() {
        return Ok(());
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
        return Ok(());
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
    Ok(())
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
}
