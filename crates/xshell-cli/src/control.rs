//! Input routing and the `//` control-command dispatcher.

use crate::audit::AuditRuntime;
use crate::config::ActiveModel;
use crate::model::*;
use crate::session::SessionRuntime;
use crate::sessions_ui::*;
use crate::tools;
use crate::util::*;
use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use xshell_adapters::AgentAdapter;
use xshell_core::{ControlCommand, InputRoute};
use xshell_execution::ApprovalPolicy;

pub(crate) fn input_route_name(route: &InputRoute) -> &'static str {
    match route {
        InputRoute::Agent(_) => "agent",
        InputRoute::Shell(_) | InputRoute::StickyShell(_) => "shell",
        InputRoute::Control(_) => "control",
        InputRoute::Empty => "empty",
    }
}

pub(crate) fn apply_sticky_shell_mode(route: InputRoute, sticky: &mut bool) -> InputRoute {
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

pub(crate) fn handle_control(
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

pub(crate) fn print_status(
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

pub(crate) fn print_audit_status(audit: &AuditRuntime) {
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

pub(crate) fn print_agent(agent: &dyn AgentAdapter) {
    let descriptor = agent.descriptor();
    println!("{} / {}", descriptor.display_name, descriptor.model);
    println!("id: {}", descriptor.id);
    println!("capabilities: {}", descriptor.capabilities.join(", "));
}

pub(crate) fn run_shell(command: &str, cwd: &mut PathBuf) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_core::classify_input;

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
}
