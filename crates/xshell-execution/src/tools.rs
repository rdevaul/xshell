use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use xshell_core::{ToolCall, ToolDefinition};

const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 500;

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a UTF-8 text file inside the current xshell working directory."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the working directory"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_TOOL_OUTPUT_BYTES}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "list_directory".into(),
            description: "List files and directories inside the current xshell working directory."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the working directory; defaults to ."}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "run_shell".into(),
            description: "Run a shell command in the current xshell working directory. This always requires user approval."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command passed to the user's configured shell"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn requires_approval(call: &ToolCall) -> bool {
    call.name == "run_shell"
}

pub fn tool_summary(call: &ToolCall) -> String {
    match call.name.as_str() {
        "run_shell" => call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .map_or_else(
                || call.arguments.to_string(),
                |command| format!("$ {command}"),
            ),
        _ => format!("{} {}", call.name, call.arguments),
    }
}

pub async fn execute_tool(call: &ToolCall, root: &Path) -> String {
    match execute_inner(call, root).await {
        Ok(output) => truncate(output),
        Err(error) => format!("tool error: {error:#}"),
    }
}

async fn execute_inner(call: &ToolCall, root: &Path) -> Result<String> {
    match call.name.as_str() {
        "read_file" => read_file(&call.arguments, root),
        "list_directory" => list_directory(&call.arguments, root),
        "run_shell" => run_shell(&call.arguments, root).await,
        _ => bail!("unknown tool {}", call.name),
    }
}

fn read_file(arguments: &Value, root: &Path) -> Result<String> {
    let path = required_string(arguments, "path")?;
    let path = resolve_existing(root, path)?;
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    let max_bytes = arguments
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_TOOL_OUTPUT_BYTES as u64)
        .min(MAX_TOOL_OUTPUT_BYTES as u64) as usize;
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let truncated = bytes.len() > max_bytes;
    let content = String::from_utf8_lossy(&bytes[..bytes.len().min(max_bytes)]);
    Ok(if truncated {
        format!("{content}\n[output truncated at {max_bytes} bytes]")
    } else {
        content.into_owned()
    })
}

fn list_directory(arguments: &Value, root: &Path) -> Result<String> {
    let relative = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = resolve_existing(root, relative)?;
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let mut entries = std::fs::read_dir(&path)
        .with_context(|| format!("cannot list {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let truncated = entries.len() > MAX_DIRECTORY_ENTRIES;
    let mut output = entries
        .into_iter()
        .take(MAX_DIRECTORY_ENTRIES)
        .map(|entry| {
            let suffix = entry
                .file_type()
                .map(|kind| if kind.is_dir() { "/" } else { "" })
                .unwrap_or("");
            format!("{}{suffix}", entry.file_name().to_string_lossy())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if truncated {
        output.push_str("\n[listing truncated]");
    }
    Ok(output)
}

async fn run_shell(arguments: &Value, root: &Path) -> Result<String> {
    let command = required_string(arguments, "command")?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut process = Command::new(&shell);
    process
        .arg("-lc")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(60), process.output())
        .await
        .context("shell tool timed out after 60 seconds")?
        .with_context(|| format!("could not launch shell {shell}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format!(
        "exit status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status, stdout, stderr
    ))
}

fn required_string<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {key}"))
}

fn resolve_existing(root: &Path, requested: &str) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve tool root {}", root.display()))?;
    let requested = Path::new(requested);
    let candidate = if requested.is_absolute() {
        requested.to_owned()
    } else {
        root.join(requested)
    };
    let candidate = candidate
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", candidate.display()))?;
    if !candidate.starts_with(&root) {
        bail!("path is outside the current xshell working directory");
    }
    Ok(candidate)
}

fn truncate(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output;
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output.push_str("\n[tool output truncated]");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_paths_outside_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(resolve_existing(root, "/etc/hosts").is_err());
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let output = truncate("é".repeat(MAX_TOOL_OUTPUT_BYTES));
        assert!(output.ends_with("[tool output truncated]"));
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[test]
    fn lists_current_crate_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = list_directory(&json!({}), root).unwrap();
        assert!(output.contains("Cargo.toml"));
    }
}
