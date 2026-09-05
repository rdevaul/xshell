use crate::SensitivePaths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;
use xshell_core::{ToolCall, ToolDefinition};

const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 500;
const SHELL_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a UTF-8 text file inside the current xshell working directory. \
Files that look like credentials or keys require user approval."
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

/// Why a tool call needs a human decision before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    /// The tool executes arbitrary commands.
    ShellExecution,
    /// The tool would read or list a path matching the sensitive-path policy.
    SensitivePath,
}

impl std::fmt::Display for GateReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ShellExecution => "shell execution",
            Self::SensitivePath => "sensitive path",
        })
    }
}

/// Decide whether `call` must be approved before running in `root`.
///
/// `run_shell` is always gated. Read-only tools are gated only when their
/// resolved target matches `sensitive`. Resolution failures (missing file,
/// escape from root) are not gated here: the tool itself will return an
/// error, and prompting for a call that cannot run would be noise.
pub fn requires_approval(
    call: &ToolCall,
    root: &Path,
    sensitive: &SensitivePaths,
) -> Option<GateReason> {
    if requires_approval_by_name(&call.name) {
        return Some(GateReason::ShellExecution);
    }
    if sensitive.patterns().is_empty() {
        return None;
    }
    let requested = match call.name.as_str() {
        "read_file" => call.arguments.get("path").and_then(Value::as_str)?,
        "list_directory" => call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("."),
        _ => return None,
    };
    let (root, resolved) = resolve_existing_with_root(root, requested).ok()?;
    let relative = resolved.strip_prefix(&root).unwrap_or(&resolved);
    sensitive
        .matches(relative)
        .then_some(GateReason::SensitivePath)
}

/// Whether a tool is gated regardless of its arguments.
pub fn requires_approval_by_name(name: &str) -> bool {
    name == "run_shell"
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
    // Agent-requested commands run as a plain non-login shell. Sourcing the
    // user's login profile on every tool call is slow, replays profile side
    // effects, and exposes profile-exported credentials to model-authored
    // commands. Direct `$` input, which the user types, keeps `-l`.
    //
    // The child is placed in its own process group so that a timeout kills
    // the whole pipeline rather than only the shell leader.
    process
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = process
        .spawn()
        .with_context(|| format!("could not launch shell {shell}"))?;
    let group = child.id().and_then(|pid| i32::try_from(pid).ok());
    let stdout = child.stdout.take().context("cannot capture shell stdout")?;
    let stderr = child.stderr.take().context("cannot capture shell stderr")?;
    match timeout(SHELL_TOOL_TIMEOUT, async move {
        let (stdout, stderr, status) = tokio::try_join!(
            read_bounded_output(stdout),
            read_bounded_output(stderr),
            child.wait()
        )?;
        Ok::<_, std::io::Error>((stdout, stderr, status))
    })
    .await
    {
        Ok(Ok((stdout, stderr, status))) => Ok(format!(
            "exit status: {}\nstdout:\n{}\nstderr:\n{}",
            status,
            stdout.render(),
            stderr.render()
        )),
        Ok(Err(error)) => {
            kill_process_group(group);
            Err(error).context("could not collect shell tool output")
        }
        Err(_) => {
            kill_process_group(group);
            bail!(
                "shell tool timed out after {} seconds; the process group was killed",
                SHELL_TOOL_TIMEOUT.as_secs()
            )
        }
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedOutput {
    fn render(&self) -> String {
        let mut output = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            output.push_str("\n[stream output truncated]");
        }
        output
    }
}

/// Drain a pipe to EOF while retaining only a bounded prefix. Continuing to
/// drain is important: stopping at the limit would block the child on a full
/// pipe and turn a memory bound into a deadlock.
async fn read_bounded_output(mut reader: impl AsyncRead + Unpin) -> std::io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(MAX_TOOL_OUTPUT_BYTES);
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(BoundedOutput { bytes, truncated });
        }
        let remaining = MAX_TOOL_OUTPUT_BYTES.saturating_sub(bytes.len());
        let retained = count.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
}

/// Kill every process in the group led by `pid`. `process_group(0)` makes the
/// child its own group leader, so its pgid equals its pid.
fn kill_process_group(pid: Option<i32>) {
    if let Some(pid) = pid.filter(|pid| *pid > 0) {
        // SAFETY: kill(2) with a negative pid targets a process group; it has
        // no memory-safety preconditions and errors are irrelevant here.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

fn required_string<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {key}"))
}

fn resolve_existing(root: &Path, requested: &str) -> Result<PathBuf> {
    resolve_existing_with_root(root, requested).map(|(_, path)| path)
}

/// Like [`resolve_existing`] but also returns the canonical root, so callers
/// can compute the path relative to it.
fn resolve_existing_with_root(root: &Path, requested: &str) -> Result<(PathBuf, PathBuf)> {
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
    Ok((root, candidate))
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

    #[tokio::test]
    async fn shell_stream_collection_drains_but_retains_only_the_limit() {
        let input = vec![b'x'; MAX_TOOL_OUTPUT_BYTES * 3];
        let output = read_bounded_output(input.as_slice()).await.unwrap();
        assert_eq!(output.bytes.len(), MAX_TOOL_OUTPUT_BYTES);
        assert!(output.truncated);
    }

    #[test]
    fn blocks_symlink_escape_from_root() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        assert!(resolve_existing(&root, "escape").is_err());
        assert!(resolve_existing(&root, "escape/hosts").is_err());
        assert!(resolve_existing(&root, "../").is_err());
    }

    #[tokio::test]
    async fn shell_tool_runs_without_login_profile_and_captures_output() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = run_shell(
            &json!({"command": "printf out; printf err >&2; exit 3"}),
            root,
        )
        .await
        .unwrap();
        // `ExitStatus` displays as "exit status: 3", so the tool result line
        // reads "exit status: exit status: 3".
        assert!(output.starts_with("exit status: exit status: 3\n"));
        assert!(output.contains("stdout:\nout"));
        assert!(output.contains("stderr:\nerr"));
    }

    #[tokio::test]
    async fn shell_tool_timeout_kills_the_whole_process_group() {
        // Use a short timeout by driving the same machinery directly rather
        // than waiting 60 seconds for the production constant.
        let temporary = tempfile::TempDir::new().unwrap();
        let marker = temporary.path().join("pid");
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut process = Command::new(&shell);
        process
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", marker.display()))
            .current_dir(temporary.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = process.spawn().unwrap();
        let group = child.id().and_then(|pid| i32::try_from(pid).ok());
        // Wait for the grandchild's pid to be recorded.
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(
            timeout(Duration::from_millis(200), child.wait())
                .await
                .is_err()
        );
        kill_process_group(group);
        let _ = child.wait().await;
        // The grandchild must be gone. Poll briefly for the kernel to reap it.
        let mut alive = true;
        for _ in 0..100 {
            // SAFETY: kill with signal 0 only checks for existence.
            alive = unsafe { libc::kill(grandchild, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !alive,
            "grandchild {grandchild} survived process-group kill"
        );
    }

    #[test]
    fn sensitive_paths_promote_read_only_tools_to_gated() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::write(root.join("README.md"), "hi").unwrap();
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        std::fs::write(root.join(".ssh/id_ed25519"), "key").unwrap();
        // A symlink with an innocent name pointing at a secret is still gated,
        // because matching runs on the canonical path.
        std::os::unix::fs::symlink(root.join(".env"), root.join("notes.txt")).unwrap();
        let sensitive = SensitivePaths::default();
        let call = |name: &str, path: &str| ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: json!({"path": path}),
        };

        assert_eq!(
            requires_approval(&call("read_file", ".env"), root, &sensitive),
            Some(GateReason::SensitivePath)
        );
        assert_eq!(
            requires_approval(&call("read_file", "notes.txt"), root, &sensitive),
            Some(GateReason::SensitivePath)
        );
        assert_eq!(
            requires_approval(&call("list_directory", ".ssh"), root, &sensitive),
            Some(GateReason::SensitivePath)
        );
        assert_eq!(
            requires_approval(&call("read_file", "README.md"), root, &sensitive),
            None
        );
        assert_eq!(
            requires_approval(&call("list_directory", "."), root, &sensitive),
            None
        );
        assert_eq!(
            requires_approval(&call("run_shell", "x"), root, &sensitive),
            Some(GateReason::ShellExecution)
        );
        // Missing files are not gated; the tool reports the error itself.
        assert_eq!(
            requires_approval(&call("read_file", "missing.pem"), root, &sensitive),
            None
        );
        // An empty policy gates nothing but shell.
        assert_eq!(
            requires_approval(&call("read_file", ".env"), root, &SensitivePaths::none()),
            None
        );
    }

    #[test]
    fn lists_current_crate_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = list_directory(&json!({}), root).unwrap();
        assert!(output.contains("Cargo.toml"));
    }
}
