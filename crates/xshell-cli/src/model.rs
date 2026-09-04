//! Model profiles: `//model`, adapter construction, and profile display.

use crate::audit::AuditRuntime;
use crate::config::{ActiveModel, Provider, XshellConfig};
use anyhow::{Result, bail};
use std::env;
use xshell_adapters::AgentAdapter;
use xshell_audit::AuditEvent;
use xshell_core::ChatMessage;
use xshell_execution::{AdapterConfig, build_adapter as build_execution_adapter};

pub(crate) fn build_adapter(
    active: &ActiveModel,
    include_credentials: bool,
) -> Result<Box<dyn AgentAdapter>> {
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
pub(crate) fn handle_model_command(
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
pub(crate) fn switch_model_profile(
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

pub(crate) fn print_model_profiles(config: &XshellConfig, active: &ActiveModel) {
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

pub(crate) fn print_model(active: &ActiveModel, daemon_owned: bool) {
    println!(
        "profile: {}",
        active.profile_name.as_deref().unwrap_or("(command-line)")
    );
    println!("provider: {:?}", active.provider);
    println!("model: {}", active.model);
    println!("endpoint: {}", active.base_url);
    match active.max_history_bytes {
        Some(0) => println!("history budget: unlimited (profile)"),
        Some(bytes) => println!("history budget: {bytes} bytes (profile)"),
        None => println!("history budget: session default"),
    }
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

pub(crate) fn active_model_label(active: &ActiveModel) -> &str {
    active.profile_name.as_deref().unwrap_or(&active.model)
}
