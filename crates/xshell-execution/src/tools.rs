use crate::SensitivePaths;
use crate::engine::CancellationFlag;
use crate::prepared::{self, ObjectKind, PreparedObject};
use crate::process::{ProcessGroupGuard, configure_process_group};
use crate::provider::{RegisteredTool, ToolAccess, ToolRegistry};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use xshell_core::{ToolCall, ToolDefinition};

const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 500;
/// Longest wait for one preparation or read of a read-only tool.
const FILESYSTEM_DEADLINE: Duration = Duration::from_secs(30);

/// Every tool the engine can dispatch. Closed on purpose: adding a tool is a
/// new variant, and exhaustive matches find every place that must handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    ReadFile,
    ListDirectory,
    RunShell,
    /// Starts a subagent. Dispatched by the engine rather than
    /// [`execute_tool`], because it runs a nested agent turn.
    DelegateTask,
    /// Shows the model an image file. Dispatched by the engine, which holds
    /// the session's image store; offered only to models that take images.
    ViewImage,
}

impl Tool {
    pub const ALL: [Self; 5] = [
        Self::ReadFile,
        Self::ListDirectory,
        Self::RunShell,
        Self::DelegateTask,
        Self::ViewImage,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListDirectory => "list_directory",
            Self::RunShell => "run_shell",
            Self::DelegateTask => "delegate_task",
            Self::ViewImage => "view_image",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// Whether the tool can change state outside xshell. Read-only tools may
    /// still be gated by the sensitive-path policy.
    pub fn is_read_only(self) -> bool {
        match self {
            Self::ReadFile | Self::ListDirectory | Self::ViewImage => true,
            Self::RunShell | Self::DelegateTask => false,
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The tools one agent may use: both what is advertised to the model and what
/// the engine will dispatch. A call outside the set is refused before
/// approval, so narrowing a set narrows authority, not just the prompt.
///
/// A set holds built-in tools and, optionally, tools from one
/// [`ToolRegistry`]. Sets built without a registry hold built-in tools only.
#[derive(Clone)]
pub struct ToolSet {
    builtins: u8,
    registry: Arc<ToolRegistry>,
    /// Bit `i` is the registry's tool `i`; the registry holds at most
    /// [`MAX_PROVIDED_TOOLS`](crate::MAX_PROVIDED_TOOLS) tools.
    provided: u64,
}

/// A tool in a [`ToolSet`].
#[derive(Debug, Clone, Copy)]
pub enum ToolRef<'a> {
    Builtin(Tool),
    Provided(&'a RegisteredTool),
}

impl ToolRef<'_> {
    pub fn name(&self) -> &str {
        match self {
            Self::Builtin(tool) => tool.name(),
            Self::Provided(tool) => tool.name(),
        }
    }

    pub fn is_read_only(&self) -> bool {
        match self {
            Self::Builtin(tool) => tool.is_read_only(),
            Self::Provided(tool) => tool.access() == ToolAccess::ReadOnly,
        }
    }

    /// Whether this is the built-in `tool`.
    pub fn is(&self, tool: Tool) -> bool {
        matches!(self, Self::Builtin(builtin) if *builtin == tool)
    }
}

impl ToolSet {
    /// Every built-in tool.
    pub fn all() -> Self {
        Self::from_tools(Tool::ALL)
    }

    pub fn none() -> Self {
        Self::from_tools([])
    }

    /// The read-only built-in tools.
    pub fn read_only() -> Self {
        Self::from_tools(Tool::ALL.into_iter().filter(|tool| tool.is_read_only()))
    }

    pub fn from_tools(tools: impl IntoIterator<Item = Tool>) -> Self {
        Self {
            builtins: tools.into_iter().fold(0, |bits, tool| bits | tool.bit()),
            registry: ToolRegistry::empty(),
            provided: 0,
        }
    }

    /// This set's built-in tools plus every tool in `registry`, replacing
    /// any provided tools the set held before.
    pub fn with_provided(self, registry: Arc<ToolRegistry>) -> Self {
        let provided = match registry.len() {
            0 => 0,
            count => u64::MAX >> (64 - count),
        };
        Self {
            builtins: self.builtins,
            registry,
            provided,
        }
    }

    pub fn contains(&self, tool: Tool) -> bool {
        self.builtins & tool.bit() != 0
    }

    /// The registry this set draws provided tools from.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// The tool named `name`, if it exists and is in this set.
    pub fn get(&self, name: &str) -> Option<ToolRef<'_>> {
        if let Some(tool) = Tool::from_name(name) {
            return self.contains(tool).then_some(ToolRef::Builtin(tool));
        }
        let index = self.registry.position(name)?;
        (self.provided & (1 << index) != 0)
            .then(|| self.registry.get(index).map(ToolRef::Provided))
            .flatten()
    }

    /// Whether `name` is a tool this set's engine knows, in this set or not:
    /// the difference between "not available to this agent" and "unknown".
    pub fn knows(&self, name: &str) -> bool {
        Tool::from_name(name).is_some() || self.registry.position(name).is_some()
    }

    pub fn without(mut self, tool: Tool) -> Self {
        self.builtins &= !tool.bit();
        self
    }

    /// The tools in both sets: how a child's request is bounded by its
    /// parent's authority. Provided tools are kept only when both sets draw
    /// on the same registry.
    pub fn intersect(&self, other: &Self) -> Self {
        let same_registry = Arc::ptr_eq(&self.registry, &other.registry);
        Self {
            builtins: self.builtins & other.builtins,
            registry: Arc::clone(&self.registry),
            provided: if same_registry {
                self.provided & other.provided
            } else {
                0
            },
        }
    }

    /// The tools of this set whose names are in `names`. Names this set
    /// does not hold are ignored, so the result never exceeds this set.
    pub fn only<S: AsRef<str>>(&self, names: &[S]) -> Self {
        let named = |name: &str| names.iter().any(|wanted| wanted.as_ref() == name);
        let builtins = Tool::ALL
            .into_iter()
            .filter(|tool| named(tool.name()))
            .fold(0, |bits, tool| bits | tool.bit());
        let provided = self
            .registry
            .tools()
            .iter()
            .enumerate()
            .filter(|(_, tool)| named(tool.name()))
            .fold(0, |bits, (index, _)| bits | (1 << index));
        Self {
            builtins: self.builtins & builtins,
            registry: Arc::clone(&self.registry),
            provided: self.provided & provided,
        }
    }

    /// Whether this set holds every tool its engine knows: every built-in
    /// tool and every tool in its registry.
    pub fn is_complete(&self) -> bool {
        *self == Self::all().with_provided(Arc::clone(&self.registry))
    }

    /// The read-only tools of this set, built-in and provided.
    pub fn read_only_subset(&self) -> Self {
        let provided = self
            .registry
            .tools()
            .iter()
            .enumerate()
            .filter(|(_, tool)| tool.access() == ToolAccess::ReadOnly)
            .fold(0, |bits, (index, _)| bits | (1 << index));
        Self {
            builtins: self.builtins & Self::read_only().builtins,
            registry: Arc::clone(&self.registry),
            provided: self.provided & provided,
        }
    }

    /// The built-in tools in this set, in `Tool::ALL` order.
    pub fn tools(&self) -> impl Iterator<Item = Tool> + '_ {
        Tool::ALL
            .into_iter()
            .filter(move |tool| self.contains(*tool))
    }

    /// Every tool in this set: built-in tools in `Tool::ALL` order, then
    /// provided tools in registry order.
    pub fn iter(&self) -> impl Iterator<Item = ToolRef<'_>> + '_ {
        let provided = self
            .registry
            .tools()
            .iter()
            .enumerate()
            .filter(|(index, _)| self.provided & (1 << index) != 0)
            .map(|(_, tool)| ToolRef::Provided(tool));
        self.tools().map(ToolRef::Builtin).chain(provided)
    }

    /// The names of every tool in this set, in [`iter`](Self::iter) order.
    pub fn names(&self) -> Vec<String> {
        self.iter().map(|tool| tool.name().to_owned()).collect()
    }

    /// Definitions for the tools in this set, in [`iter`](Self::iter) order.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let builtins = definitions()
            .into_iter()
            .filter(|definition| self.get(&definition.name).is_some());
        let provided = self.iter().filter_map(|tool| match tool {
            ToolRef::Provided(tool) => Some(tool.definition().clone()),
            ToolRef::Builtin(_) => None,
        });
        builtins.chain(provided).collect()
    }
}

impl PartialEq for ToolSet {
    fn eq(&self, other: &Self) -> bool {
        self.builtins == other.builtins
            && self.provided == other.provided
            && (self.provided == 0 || Arc::ptr_eq(&self.registry, &other.registry))
    }
}

impl Eq for ToolSet {}

impl Default for ToolSet {
    fn default() -> Self {
        Self::all()
    }
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.names()).finish()
    }
}

/// Definitions for every tool, in `Tool::ALL` order.
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
        ToolDefinition {
            name: "delegate_task".into(),
            description: "Start a subagent to complete one self-contained task and return its \
final answer. The subagent starts with no conversation context, so `task` must contain \
everything it needs. It works in the same directory with the same or fewer permissions and \
cannot start subagents of its own. Use it for bounded investigation whose details you do not \
need to keep in your own context."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete instructions for the subagent"},
                    "tools": {
                        "type": "string",
                        "enum": ["read_only", "inherit"],
                        "description": "read_only (default): your read-only tools only (read_file, list_directory, and any others you have). inherit: your tools, including run_shell under the same approval policy"
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "view_image".into(),
            description: "Look at an image file (PNG, JPEG, GIF or WebP) inside the current xshell \
working directory, such as a render or plot a command produced. The image is shown to you \
with the result, scaled to at most 1568 pixels on its longest edge. Files that look like \
credentials or keys require user approval."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the working directory"}
                },
                "required": ["path"],
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
    /// The approval policy lists the call as needing approval.
    Policy,
    /// A provided tool declares that its calls need approval.
    ToolDeclared,
}

impl std::fmt::Display for GateReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ShellExecution => "shell execution",
            Self::SensitivePath => "sensitive path",
            Self::Policy => "approval policy",
            Self::ToolDeclared => "tool requires approval",
        })
    }
}

/// Decide whether `call` must be approved before running in `root`.
///
/// `run_shell` is always gated. Read-only tools are gated only when the
/// object they would read is sensitive; see [`prepare_call`]. Preparation
/// failures (missing file, escape from root) are not gated: the tool reports
/// the error itself, and prompting for a call that cannot run would be noise.
///
/// This prepares the call and discards the result. The engine keeps the
/// preparation and reads from it, so the object it authorizes is the object
/// it reads.
pub fn requires_approval(
    call: &ToolCall,
    root: &Path,
    sensitive: &SensitivePaths,
) -> Option<GateReason> {
    let preparation = match read_request(call) {
        Some(Ok((path, kind))) => match prepared::prepare(root, &path, kind, sensitive) {
            Ok(object) => Preparation::Read(object),
            Err(error) => Preparation::Failed(format!("tool error: {error:#}")),
        },
        Some(Err(error)) => Preparation::Failed(format!("tool error: {error:#}")),
        None => Preparation::Other,
    };
    gate(call, &preparation)
}

/// A tool call made ready to run: for the read-only tools, the object they
/// will read, opened and classified; for every other tool, nothing.
#[derive(Debug)]
pub(crate) enum Preparation {
    Read(PreparedObject),
    /// The read could not be prepared; this is the tool's result.
    Failed(String),
    /// The read would open a path the session host blocks. Refused before
    /// the approval rules are consulted; this is the tool's result.
    Blocked(String),
    Other,
}

/// Prepare `call` for authorization and execution. Read-only tools are
/// resolved beneath `root` and opened here, on a blocking worker with a
/// deadline, so that [`gate`] and [`execute_prepared`] act on one object.
pub(crate) async fn prepare_call(
    call: &ToolCall,
    root: &Path,
    sensitive: &SensitivePaths,
    blocked: &SensitivePaths,
) -> Preparation {
    let (path, kind) = match read_request(call) {
        Some(Ok(request)) => request,
        Some(Err(error)) => return Preparation::Failed(format!("tool error: {error:#}")),
        None => return Preparation::Other,
    };
    let root = root.to_owned();
    let sensitive = sensitive.clone();
    match run_blocking(move || prepared::prepare(&root, &path, kind, &sensitive)).await {
        // A blocked path is never read, whatever the approval policy says.
        Ok(object) if blocked.matches(object.path()) => Preparation::Blocked(format!(
            "tool error: {} is blocked by the session host's policy",
            object.path().display()
        )),
        Ok(object) => Preparation::Read(object),
        Err(error) => Preparation::Failed(format!("tool error: {error:#}")),
    }
}

impl Preparation {
    /// The canonical path a read-only tool opened.
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Read(object) => Some(object.path()),
            _ => None,
        }
    }
}

/// Why `call` needs a human decision, judged from its preparation.
pub(crate) fn gate(call: &ToolCall, preparation: &Preparation) -> Option<GateReason> {
    if requires_approval_by_name(&call.name) {
        return Some(GateReason::ShellExecution);
    }
    match preparation {
        Preparation::Read(object) if object.sensitive() => Some(GateReason::SensitivePath),
        _ => None,
    }
}

/// The path and object kind a read-only tool asks for, or `None` for any
/// other tool.
fn read_request(call: &ToolCall) -> Option<Result<(String, ObjectKind)>> {
    match Tool::from_name(&call.name)? {
        Tool::ReadFile | Tool::ViewImage => Some(
            required_string(&call.arguments, "path")
                .map(|path| (path.to_owned(), ObjectKind::File)),
        ),
        Tool::ListDirectory => Some(Ok((
            call.arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_owned(),
            ObjectKind::Directory,
        ))),
        Tool::RunShell | Tool::DelegateTask => None,
    }
}

/// Whether a tool is gated regardless of its arguments.
pub fn requires_approval_by_name(name: &str) -> bool {
    Tool::from_name(name) == Some(Tool::RunShell)
}

pub fn tool_summary(call: &ToolCall) -> String {
    match call.name.as_str() {
        "delegate_task" => {
            let task = call
                .arguments
                .get("task")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let first_line = task.lines().next().unwrap_or_default();
            let mut summary: String = first_line.chars().take(120).collect();
            if summary.len() < task.len() {
                summary.push('…');
            }
            format!("delegate: {summary}")
        }
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
    execute_tool_cancellable(call, root, CancellationFlag::default()).await
}

/// Prepare and run `call` with no sensitive-path policy: callers that gate on
/// sensitivity prepare first and use [`execute_prepared`].
pub(crate) async fn execute_tool_cancellable(
    call: &ToolCall,
    root: &Path,
    cancellation: CancellationFlag,
) -> String {
    let preparation =
        prepare_call(call, root, &SensitivePaths::none(), &SensitivePaths::none()).await;
    execute_prepared(
        call,
        preparation,
        root,
        cancellation,
        crate::TurnLimits::default().shell_timeout,
    )
    .await
}

/// Run a prepared call. A read-only tool reads from the object prepared for
/// it, never from a fresh resolution of its path.
pub(crate) async fn execute_prepared(
    call: &ToolCall,
    preparation: Preparation,
    root: &Path,
    cancellation: CancellationFlag,
    shell_timeout: Duration,
) -> String {
    let output = match preparation {
        Preparation::Failed(message) | Preparation::Blocked(message) => return message,
        Preparation::Read(object) => match Tool::from_name(&call.name) {
            Some(Tool::ReadFile) => {
                let max_bytes = call
                    .arguments
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(MAX_TOOL_OUTPUT_BYTES as u64)
                    .min(MAX_TOOL_OUTPUT_BYTES as u64) as usize;
                run_blocking(move || read_file(object, max_bytes)).await
            }
            _ => run_blocking(move || list_directory(object)).await,
        },
        Preparation::Other => match Tool::from_name(&call.name) {
            Some(Tool::RunShell) => {
                run_shell(&call.arguments, root, cancellation, shell_timeout).await
            }
            Some(Tool::DelegateTask) => Err(anyhow::anyhow!(
                "delegate_task is dispatched by the agent engine"
            )),
            Some(Tool::ReadFile | Tool::ListDirectory | Tool::ViewImage) => {
                Err(anyhow::anyhow!("{} was not prepared", call.name))
            }
            None => Err(anyhow::anyhow!("unknown tool {}", call.name)),
        },
    };
    match output {
        Ok(output) => truncate(output),
        Err(error) => format!("tool error: {error:#}"),
    }
}

/// Run a prepared `view_image` call: read the file it opened (at most the
/// per-image limit), ingest it into the session's store, and return the
/// result text with the image to attach to it.
pub(crate) async fn execute_view_image(
    call: &ToolCall,
    preparation: Preparation,
    images: Arc<dyn crate::SessionImages>,
) -> (String, Vec<xshell_core::ImageRef>) {
    let object = match preparation {
        Preparation::Read(object) => object,
        Preparation::Failed(message) | Preparation::Blocked(message) => {
            return (message, Vec::new());
        }
        Preparation::Other => {
            return ("tool error: view_image was not prepared".into(), Vec::new());
        }
    };
    let label = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("image")
        .to_owned();
    let result = run_blocking(move || {
        let limit = crate::MAX_SOURCE_BYTES as usize;
        let (bytes, truncated) = object.read(limit)?;
        if truncated {
            bail!("the file is larger than the {limit}-byte image limit");
        }
        images.ingest(&bytes, &label).map_err(anyhow::Error::msg)
    })
    .await;
    match result {
        Ok(image) => (
            format!(
                "Viewed {}: {}x{} PNG, shown with this result.",
                image.label(),
                image.width(),
                image.height()
            ),
            vec![image],
        ),
        Err(error) => (format!("tool error: {error:#}"), Vec::new()),
    }
}

/// Run blocking filesystem work off the async runtime, bounded by
/// [`FILESYSTEM_DEADLINE`]. A timed-out worker cannot be interrupted, but the
/// turn and the runtime are no longer held by it.
async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    run_blocking_within(FILESYSTEM_DEADLINE, work).await
}

async fn run_blocking_within<T: Send + 'static>(
    deadline: Duration,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    match tokio::time::timeout(deadline, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow::anyhow!("filesystem worker failed: {error}")),
        Err(_) => bail!(
            "the filesystem did not respond within {} ms",
            deadline.as_millis()
        ),
    }
}

fn read_file(object: PreparedObject, max_bytes: usize) -> Result<String> {
    let (bytes, truncated) = object.read(max_bytes)?;
    let content = String::from_utf8_lossy(&bytes);
    Ok(if truncated {
        format!("{content}\n[output truncated at {max_bytes} bytes]")
    } else {
        content.into_owned()
    })
}

fn list_directory(object: PreparedObject) -> Result<String> {
    let (mut entries, truncated) = object.list(MAX_DIRECTORY_ENTRIES)?;
    entries.sort();
    let mut output = entries
        .into_iter()
        .map(|(name, is_directory)| {
            let suffix = if is_directory { "/" } else { "" };
            format!("{}{suffix}", name.to_string_lossy())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if truncated {
        output.push_str(&format!(
            "\n[listing truncated after {MAX_DIRECTORY_ENTRIES} entries; the rest were not read]"
        ));
    }
    Ok(output)
}

async fn run_shell(
    arguments: &Value,
    root: &Path,
    cancellation: CancellationFlag,
    timeout: Duration,
) -> Result<String> {
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
        .stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .with_context(|| format!("could not launch shell {shell}"))?;
    let mut group = ProcessGroupGuard::for_child(&child);
    let stdout = child.stdout.take().context("cannot capture shell stdout")?;
    let stderr = child.stderr.take().context("cannot capture shell stderr")?;
    let stdout_task = tokio::spawn(read_bounded_output(stdout));
    let stderr_task = tokio::spawn(read_bounded_output(stderr));
    let wait = tokio::select! {
        status = child.wait() => Some(status.context("could not wait for shell tool")),
        () = cancellation.wait() => None,
        () = tokio::time::sleep(timeout) => None,
    };
    let status = match wait {
        Some(status) => status?,
        None => {
            let cancelled = cancellation.is_cancelled();
            group.kill();
            group.sweep().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            group.disarm();
            if cancelled {
                bail!("shell tool cancelled; the process group was killed and reaped");
            }
            bail!(
                "shell tool timed out after {} seconds; the process group was killed and reaped",
                timeout.as_secs()
            );
        }
    };
    group.disarm();
    let stdout = stdout_task
        .await
        .context("shell stdout reader task failed")??;
    let stderr = stderr_task
        .await
        .context("shell stderr reader task failed")??;
    Ok(format!(
        "exit status: {}\nstdout:\n{}\nstderr:\n{}",
        status,
        stdout.render(),
        stderr.render()
    ))
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

fn required_string<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {key}"))
}

pub(crate) fn truncate(mut output: String) -> String {
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
    use std::path::PathBuf;
    use tokio::time::timeout;

    /// The package directory, read when the test runs. `cargo test` sets
    /// `CARGO_MANIFEST_DIR` for each test process. The compile-time `env!`
    /// form is baked into the binary, and Cargo does not rebuild when only the
    /// checkout path changes, so a test binary reused from another checkout
    /// (for example one built into a shared target directory) would look in
    /// that old, possibly deleted, location.
    fn manifest_dir() -> PathBuf {
        PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR")
                .expect("CARGO_MANIFEST_DIR is unset; run the tests through cargo"),
        )
    }

    #[test]
    fn tool_names_match_their_definitions() {
        let names: Vec<_> = definitions().into_iter().map(|tool| tool.name).collect();
        let expected: Vec<_> = Tool::ALL.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, expected);
        for tool in Tool::ALL {
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
        assert_eq!(Tool::from_name("delete_everything"), None);
    }

    #[test]
    fn toolsets_select_definitions_and_dispatch() {
        let read_only = ToolSet::read_only();
        assert!(read_only.contains(Tool::ReadFile));
        assert!(read_only.contains(Tool::ListDirectory));
        assert!(!read_only.contains(Tool::RunShell));
        assert!(read_only.get("run_shell").is_none());
        assert!(
            read_only
                .get("read_file")
                .is_some_and(|tool| tool.is(Tool::ReadFile))
        );
        assert_eq!(
            read_only
                .definitions()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            ["read_file", "list_directory", "view_image"]
        );
        assert_eq!(ToolSet::default(), ToolSet::all());
        assert_eq!(ToolSet::all().definitions(), definitions());
        assert!(ToolSet::none().definitions().is_empty());
        assert_eq!(
            ToolSet::all()
                .without(Tool::RunShell)
                .without(Tool::DelegateTask),
            read_only
        );
        assert_eq!(
            ToolSet::all().intersect(&ToolSet::from_tools([Tool::ReadFile, Tool::RunShell])),
            ToolSet::from_tools([Tool::ReadFile, Tool::RunShell])
        );
        assert_eq!(
            read_only.intersect(&ToolSet::from_tools([Tool::RunShell])),
            ToolSet::none()
        );
        assert_eq!(
            format!("{read_only:?}"),
            r#"{"read_file", "list_directory", "view_image"}"#
        );
    }

    fn provided_registry() -> Arc<ToolRegistry> {
        use crate::provider::tests::{Fixed, tool};
        use crate::{ToolAccess, ToolApproval};
        ToolRegistry::builder()
            .provider(Arc::new(Fixed {
                name: "memory",
                tools: vec![
                    tool(
                        "memory_get",
                        ToolAccess::ReadOnly,
                        ToolApproval::NotRequired,
                    ),
                    tool(
                        "memory_put",
                        ToolAccess::ReadWrite,
                        ToolApproval::NotRequired,
                    ),
                    tool(
                        "memory_purge",
                        ToolAccess::ReadWrite,
                        ToolApproval::Required,
                    ),
                ],
            }))
            .build()
            .unwrap()
    }

    #[test]
    fn toolsets_hold_provided_tools_after_the_builtins() {
        let registry = provided_registry();
        let set = ToolSet::all().with_provided(Arc::clone(&registry));
        assert_eq!(
            set.names(),
            [
                "read_file",
                "list_directory",
                "run_shell",
                "delegate_task",
                "view_image",
                "memory_get",
                "memory_put",
                "memory_purge"
            ]
        );
        assert_eq!(
            set.definitions()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            set.names()
        );
        let Some(ToolRef::Provided(tool)) = set.get("memory_put") else {
            panic!("memory_put is provided");
        };
        assert_eq!(tool.provider_name(), "memory");
        assert!(set.knows("memory_put"));
        assert!(!set.knows("memory_nope"));
        assert!(set.get("memory_nope").is_none());

        // Without the registry, provided names are unknown, not merely
        // unavailable.
        assert!(!ToolSet::all().knows("memory_put"));
        assert!(ToolSet::all().get("memory_put").is_none());
    }

    #[test]
    fn only_keeps_named_tools_of_the_set_and_ignores_other_names() {
        let registry = provided_registry();
        let set = ToolSet::all().with_provided(Arc::clone(&registry));
        assert!(set.is_complete());
        assert!(ToolSet::all().is_complete());
        let chat = set.only(&["memory_get", "read_file", "memory_nope", "memory_put"]);
        // Set order, not the order the names were given in.
        assert_eq!(chat.names(), ["read_file", "memory_get", "memory_put"]);
        assert!(!chat.is_complete());
        // Narrowing a narrowed set never brings a tool back.
        assert_eq!(
            chat.only(&["run_shell", "memory_get"]).names(),
            ["memory_get"]
        );
        assert_eq!(
            set.only::<&str>(&[]),
            ToolSet::none().with_provided(registry).only::<&str>(&[])
        );
        assert!(set.only::<&str>(&[]).names().is_empty());
    }

    #[test]
    fn read_only_subsets_and_intersections_bound_provided_tools() {
        let registry = provided_registry();
        let set = ToolSet::all().with_provided(Arc::clone(&registry));
        assert_eq!(
            set.read_only_subset().names(),
            ["read_file", "list_directory", "view_image", "memory_get"]
        );
        let narrowed = set.intersect(&ToolSet::none().with_provided(Arc::clone(&registry)));
        assert_eq!(
            narrowed.names(),
            ["memory_get", "memory_put", "memory_purge"]
        );
        // A set without the registry grants none of its tools, from either
        // side of the intersection.
        assert_eq!(set.intersect(&ToolSet::all()), ToolSet::all());
        assert_eq!(ToolSet::all().intersect(&set), ToolSet::all());
        // A different registry with the same tools is a different authority.
        let other = ToolSet::all().with_provided(provided_registry());
        assert_eq!(set.intersect(&other), ToolSet::all());
        assert_eq!(set.intersect(&other).names().len(), Tool::ALL.len());
        assert_ne!(set, other);
    }

    fn tool(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments,
        }
    }

    #[tokio::test]
    async fn blocks_paths_outside_root() {
        let root = &manifest_dir();
        let output = execute_tool(&tool("read_file", json!({"path": "/etc/hosts"})), root).await;
        assert!(output.contains(prepared::OUTSIDE_ROOT), "{output}");
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

    #[tokio::test]
    async fn blocks_symlink_escape_from_root() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        for call in [
            tool("list_directory", json!({"path": "escape"})),
            tool("read_file", json!({"path": "escape/hosts"})),
            tool("list_directory", json!({"path": "../"})),
        ] {
            let output = execute_tool(&call, &root).await;
            assert!(output.contains(prepared::OUTSIDE_ROOT), "{output}");
        }
    }

    #[test]
    fn f4_sensitivity_is_the_same_from_every_cwd() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        std::fs::create_dir_all(root.join("sub/.git")).unwrap();
        std::fs::write(root.join("sub/.git/config"), "[remote] token").unwrap();
        std::fs::create_dir_all(root.join(".aws/project")).unwrap();
        std::fs::write(root.join(".aws/credentials"), "key").unwrap();
        std::fs::write(root.join(".aws/project/main.tf"), "resource").unwrap();
        std::fs::write(root.join("README.md"), "hi").unwrap();
        let sensitive = SensitivePaths::default();
        let read = |path: &str| tool("read_file", json!({"path": path}));

        // A nested repository's config, from the workspace, the repository,
        // and inside its .git directory.
        for (cwd, path) in [
            (root.to_owned(), "sub/.git/config"),
            (root.join("sub"), ".git/config"),
            (root.join("sub/.git"), "config"),
        ] {
            assert_eq!(
                requires_approval(&read(path), &cwd, &sensitive),
                Some(GateReason::SensitivePath),
                "{path} from {}",
                cwd.display()
            );
        }
        // A sensitive directory keeps its ancestry when it becomes the cwd,
        // and a project kept inside one stays sensitive.
        assert_eq!(
            requires_approval(&read("credentials"), &root.join(".aws"), &sensitive),
            Some(GateReason::SensitivePath)
        );
        assert_eq!(
            requires_approval(&read("main.tf"), &root.join(".aws/project"), &sensitive),
            Some(GateReason::SensitivePath)
        );
        // An alias is classified by what it resolves to.
        std::os::unix::fs::symlink("sub/.git/config", root.join("alias")).unwrap();
        assert_eq!(
            requires_approval(&read("alias"), root, &sensitive),
            Some(GateReason::SensitivePath)
        );
        assert_eq!(
            requires_approval(&read("README.md"), root, &sensitive),
            None
        );
    }

    #[tokio::test]
    async fn shell_tool_runs_without_login_profile_and_captures_output() {
        let root = &manifest_dir();
        let output = run_shell(
            &json!({"command": "printf out; printf err >&2; exit 3"}),
            root,
            CancellationFlag::default(),
            Duration::from_secs(60),
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
            .stderr(Stdio::null());
        configure_process_group(&mut process);
        let mut child = process.spawn().unwrap();
        let mut group = ProcessGroupGuard::for_child(&child);
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
        group.kill();
        let _ = child.wait().await;
        group.disarm();
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

    #[tokio::test]
    async fn lists_current_crate_directory() {
        let root = &manifest_dir();
        let output = execute_tool(&tool("list_directory", json!({})), root).await;
        assert!(output.contains("Cargo.toml"));
        assert!(output.contains("src/"));
    }

    #[tokio::test]
    async fn f8_a_stuck_filesystem_call_does_not_hold_the_turn() {
        // Stands in for a read blocked on a slow or hung filesystem.
        let started = std::time::Instant::now();
        let error = run_blocking_within(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(800));
            Ok(())
        })
        .await
        .unwrap_err();
        // The caller is released at the deadline, long before the worker.
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(error.to_string().contains("did not respond"), "{error:#}");
    }

    #[tokio::test]
    async fn f8_reads_and_listings_report_their_bounds() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        std::fs::File::create(root.join("large.bin"))
            .unwrap()
            .set_len(1 << 30)
            .unwrap();
        let output = execute_tool(
            &tool("read_file", json!({"path": "large.bin", "max_bytes": 4})),
            root,
        )
        .await;
        assert!(
            output.ends_with("[output truncated at 4 bytes]"),
            "{output}"
        );

        std::fs::create_dir(root.join("big")).unwrap();
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            std::fs::write(root.join(format!("big/f{index}")), "").unwrap();
        }
        let output = execute_tool(&tool("list_directory", json!({"path": "big"})), root).await;
        assert_eq!(output.lines().count(), MAX_DIRECTORY_ENTRIES + 1);
        assert!(output.ends_with("the rest were not read]"), "{output}");
    }
}
