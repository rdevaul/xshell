use crate::SensitivePaths;
use crate::engine::CancellationFlag;
use crate::environment::apply_agent_environment;
use crate::jobs::{ShellJobs, group_alive, status_text};
use crate::prepared::{self, ObjectKind, PreparedObject, WriteTarget};
use crate::process::{
    LINGER_GRACE, ShellEnd, Stream, configure_process_group, restrict_inherited_descriptors,
    supervise_shell,
};
use crate::provider::{RegisteredTool, ToolAccess, ToolRegistry};
use crate::shell::{AmbientRunner, ShellRunner};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
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
    /// Creates or replaces a text file beneath the root.
    WriteFile,
    /// Replaces exact text in an existing file beneath the root.
    EditFile,
    /// Reads a background job's status and new output, optionally waiting.
    JobOutput,
    /// Kills a background job's process group.
    JobKill,
}

impl Tool {
    pub const ALL: [Self; 9] = [
        Self::ReadFile,
        Self::ListDirectory,
        Self::RunShell,
        Self::DelegateTask,
        Self::ViewImage,
        Self::WriteFile,
        Self::EditFile,
        Self::JobOutput,
        Self::JobKill,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListDirectory => "list_directory",
            Self::RunShell => "run_shell",
            Self::DelegateTask => "delegate_task",
            Self::ViewImage => "view_image",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::JobOutput => "job_output",
            Self::JobKill => "job_kill",
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
            // The job tools belong with run_shell, which starts the jobs.
            Self::RunShell
            | Self::DelegateTask
            | Self::WriteFile
            | Self::EditFile
            | Self::JobOutput
            | Self::JobKill => false,
        }
    }

    /// Whether the tool changes files: gated like a shell command, and
    /// refused when the approval mode is `off`.
    pub fn writes_files(self) -> bool {
        matches!(self, Self::WriteFile | Self::EditFile)
    }

    fn bit(self) -> u16 {
        1 << self as u16
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
    builtins: u16,
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
Binary files are refused; use view_image for images. Files that look like credentials or keys \
require user approval."
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
            description:
                "Run a shell command in the current xshell working directory. This always \
requires user approval. A command that runs longer than the shell time limit is killed; run long \
builds, servers or watchers with background: true instead, which returns a job id at once. Read \
a job's output with job_output (which can wait for it to finish) and stop it with job_kill; you \
are told when a job ends. Prefer background: true to `&` or nohup, which leave processes you may \
not be able to stop."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command passed to the user's configured shell"},
                    "background": {"type": "boolean", "description": "Start the command as a background job and return its id without waiting (default false)"}
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
            description:
                "Look at an image file (PNG, JPEG, GIF or WebP) inside the current xshell \
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
        ToolDefinition {
            name: "write_file".into(),
            description: "Create a text file, or replace one, inside the current xshell working \
directory, creating missing parent directories. Requires user approval. Prefer edit_file to \
change part of an existing file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the working directory"},
                    "content": {"type": "string", "description": "The file's complete new content"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "edit_file".into(),
            description: "Replace exact text in an existing text file inside the current xshell \
working directory. old_string must match the file exactly, including whitespace, and must occur \
once unless replace_all is true; include enough surrounding lines to make it unique. Requires \
user approval."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the working directory"},
                    "old_string": {"type": "string", "description": "The exact text to replace"},
                    "new_string": {"type": "string", "description": "The text to put in its place"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "job_output".into(),
            description: "Show a background job's status and the output it produced since you \
last read it. With wait_seconds, wait up to that long for the job to finish first: use it to wait \
for a build instead of sleeping in run_shell."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {"type": "integer", "minimum": 1},
                    "wait_seconds": {"type": "integer", "minimum": 0, "maximum": crate::jobs::MAX_JOB_WAIT.as_secs()}
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "job_kill".into(),
            description: "Stop a background job, or processes a command left running, by killing \
its whole process group."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {"type": "integer", "minimum": 1}
                },
                "required": ["job_id"],
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
    /// The tool creates or changes a file.
    FileWrite,
}

impl std::fmt::Display for GateReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ShellExecution => "shell execution",
            Self::SensitivePath => "sensitive path",
            Self::Policy => "approval policy",
            Self::ToolDeclared => "tool requires approval",
            Self::FileWrite => "file write",
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
    let preparation = match (read_request(call), write_request(call)) {
        (Some(Ok((path, kind))), _) => match prepared::prepare(root, &path, kind, sensitive) {
            Ok(object) => Preparation::Read(object),
            Err(error) => Preparation::Failed(format!("tool error: {error:#}")),
        },
        (_, Some(Ok((path, must_exist)))) => {
            match prepared::prepare_write(root, &path, sensitive, must_exist) {
                Ok(target) => Preparation::Write(target),
                Err(error) => Preparation::Failed(format!("tool error: {error:#}")),
            }
        }
        (Some(Err(error)), _) | (_, Some(Err(error))) => {
            Preparation::Failed(format!("tool error: {error:#}"))
        }
        (None, None) => Preparation::Other,
    };
    gate(call, &preparation)
}

/// A tool call made ready to run: for the read-only tools, the object they
/// will read, opened and classified; for every other tool, nothing.
#[derive(Debug)]
pub(crate) enum Preparation {
    Read(PreparedObject),
    /// Where a write tool will write, resolved but not yet created.
    Write(WriteTarget),
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
    if let Some(request) = write_request(call) {
        let (path, must_exist) = match request {
            Ok(request) => request,
            Err(error) => return Preparation::Failed(format!("tool error: {error:#}")),
        };
        let root = root.to_owned();
        let sensitive = sensitive.clone();
        return match run_blocking(move || {
            prepared::prepare_write(&root, &path, &sensitive, must_exist)
        })
        .await
        {
            Ok(target) if blocked.matches(target.path()) => Preparation::Blocked(format!(
                "tool error: {} is blocked by the session host's policy",
                target.path().display()
            )),
            Ok(target) => Preparation::Write(target),
            Err(error) => Preparation::Failed(format!("tool error: {error:#}")),
        };
    }
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
            Self::Write(target) => Some(target.path()),
            _ => None,
        }
    }
}

impl Preparation {
    /// For a write, the canonical path and what the write does to it, as
    /// shown when the write is put to a person for approval.
    pub(crate) fn write_description(&self) -> Option<String> {
        let Self::Write(target) = self else {
            return None;
        };
        let effect = match (target.must_exist(), target.exists()) {
            (true, _) => "changes the existing file",
            (false, true) => "replaces the existing file",
            (false, false) => "creates a new file",
        };
        Some(format!("{} ({effect})", target.path().display()))
    }
}

/// Why `call` needs a human decision, judged from its preparation.
pub(crate) fn gate(call: &ToolCall, preparation: &Preparation) -> Option<GateReason> {
    match preparation {
        Preparation::Read(object) if object.sensitive() => return Some(GateReason::SensitivePath),
        Preparation::Write(target) if target.sensitive() => {
            return Some(GateReason::SensitivePath);
        }
        _ => {}
    }
    match Tool::from_name(&call.name) {
        Some(Tool::RunShell) => Some(GateReason::ShellExecution),
        Some(tool) if tool.writes_files() => Some(GateReason::FileWrite),
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
        Tool::RunShell
        | Tool::DelegateTask
        | Tool::WriteFile
        | Tool::EditFile
        | Tool::JobOutput
        | Tool::JobKill => None,
    }
}

/// The path a write tool asks for, and whether the file must already exist
/// (`edit_file`), or `None` for any other tool.
fn write_request(call: &ToolCall) -> Option<Result<(String, bool)>> {
    let must_exist = match Tool::from_name(&call.name)? {
        Tool::WriteFile => false,
        Tool::EditFile => true,
        _ => return None,
    };
    Some(required_string(&call.arguments, "path").map(|path| (path.to_owned(), must_exist)))
}

/// Whether a tool is gated regardless of its arguments.
pub fn requires_approval_by_name(name: &str) -> bool {
    Tool::from_name(name).is_some_and(|tool| tool == Tool::RunShell || tool.writes_files())
}

/// Lines shown above an approval prompt for a file write: where it goes,
/// its size, and a bounded preview of the content or of the change. Each
/// line is raw model output; the caller escapes it for the terminal.
pub fn approval_preview(call: &ToolCall, target: Option<&str>) -> Vec<String> {
    const LINES: usize = 12;
    const BYTES: usize = 1200;
    fn excerpt(text: &str, mark: &str, lines: &mut Vec<String>) {
        let mut shown = 0;
        let total = text.lines().count().max(1);
        for (index, line) in text.lines().enumerate() {
            if index == LINES || shown + line.len() > BYTES {
                lines.push(format!("    … {} more lines", total - index));
                return;
            }
            shown += line.len();
            lines.push(format!("  {mark} {line}"));
        }
        if text.is_empty() {
            lines.push(format!("  {mark} (empty)"));
        }
    }
    let text = |key: &str| call.arguments.get(key).and_then(Value::as_str);
    let mut lines = Vec::new();
    match call.name.as_str() {
        "write_file" => {
            let content = text("content").unwrap_or_default();
            lines.push(format!(
                "  to: {}",
                target.unwrap_or(text("path").unwrap_or("?"))
            ));
            lines.push(format!(
                "  content: {} bytes, {} lines",
                content.len(),
                content.lines().count()
            ));
            excerpt(content, "|", &mut lines);
        }
        "edit_file" => {
            lines.push(format!(
                "  in: {}",
                target.unwrap_or(text("path").unwrap_or("?"))
            ));
            if call.arguments.get("replace_all").and_then(Value::as_bool) == Some(true) {
                lines.push("  every occurrence of:".to_owned());
            }
            excerpt(text("old_string").unwrap_or_default(), "-", &mut lines);
            excerpt(text("new_string").unwrap_or_default(), "+", &mut lines);
        }
        _ => {}
    }
    lines
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
                |command| {
                    if call.arguments.get("background").and_then(Value::as_bool) == Some(true) {
                        format!("$ {command} &  (background job)")
                    } else {
                        format!("$ {command}")
                    }
                },
            ),
        "write_file" | "edit_file" => {
            let path = call
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let verb = if call.name == "write_file" {
                "write"
            } else {
                "edit"
            };
            format!("{verb} {path}")
        }
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
    let jobs = ShellJobs::new();
    let output = execute_prepared(
        call,
        preparation,
        root,
        cancellation,
        crate::TurnLimits::default().shell_timeout,
        &AmbientRunner,
        &jobs,
    )
    .await;
    jobs.shut_down();
    output
}

/// Run a prepared call. A read-only tool reads from the object prepared for
/// it, never from a fresh resolution of its path.
pub(crate) async fn execute_prepared(
    call: &ToolCall,
    preparation: Preparation,
    root: &Path,
    cancellation: CancellationFlag,
    shell_timeout: Duration,
    runner: &dyn ShellRunner,
    jobs: &Arc<ShellJobs>,
) -> String {
    let output = match preparation {
        Preparation::Failed(message) | Preparation::Blocked(message) => return message,
        Preparation::Write(target) => match Tool::from_name(&call.name) {
            Some(Tool::WriteFile) => write_file(target, &call.arguments).await,
            Some(Tool::EditFile) => edit_file(target, &call.arguments).await,
            _ => Err(anyhow::anyhow!("{} does not write files", call.name)),
        },
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
                run_shell_under(
                    runner,
                    &call.arguments,
                    root,
                    cancellation,
                    shell_timeout,
                    jobs,
                )
                .await
            }
            Some(Tool::JobOutput) => {
                let wait = call
                    .arguments
                    .get("wait_seconds")
                    .and_then(Value::as_u64)
                    .map_or(Duration::ZERO, Duration::from_secs);
                match job_id(&call.arguments) {
                    Ok(id) => jobs
                        .output(id, wait, &cancellation)
                        .await
                        .map_err(anyhow::Error::msg),
                    Err(error) => Err(error),
                }
            }
            Some(Tool::JobKill) => match job_id(&call.arguments) {
                Ok(id) => jobs.kill(id).await.map_err(anyhow::Error::msg),
                Err(error) => Err(error),
            },
            Some(Tool::WriteFile | Tool::EditFile) => {
                Err(anyhow::anyhow!("{} was not prepared", call.name))
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
        Preparation::Other | Preparation::Write(_) => {
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
            "the filesystem did not respond within {} ms; the operation may still finish",
            deadline.as_millis()
        ),
    }
}

fn read_file(object: PreparedObject, max_bytes: usize) -> Result<String> {
    let path = object.path().to_owned();
    let (bytes, truncated) = object.read(max_bytes)?;
    if looks_binary(&bytes) {
        let hint = if image_kind(&bytes).is_some() {
            "; it is an image: use view_image to look at it"
        } else {
            ""
        };
        bail!("{} is a binary file, not text{hint}", path.display());
    }
    let content = String::from_utf8_lossy(&bytes);
    Ok(if truncated {
        format!("{content}\n[output truncated at {max_bytes} bytes]")
    } else {
        content.into_owned()
    })
}

/// Whether `bytes` (the start of a file) look like binary data rather than
/// text: a NUL byte, or invalid UTF-8 away from the end, where a multi-byte
/// character may have been cut.
fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        Err(error) => error.error_len().is_some() || error.valid_up_to() + 4 < head.len(),
    }
}

fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    [
        (&b"\x89PNG"[..], "PNG"),
        (&b"\xff\xd8\xff"[..], "JPEG"),
        (&b"GIF8"[..], "GIF"),
        (&b"RIFF"[..], "WebP"),
    ]
    .into_iter()
    .find(|(magic, _)| bytes.starts_with(magic))
    .map(|(_, kind)| kind)
}

async fn write_file(target: WriteTarget, arguments: &Value) -> Result<String> {
    let content = required_string(arguments, "content")?.to_owned();
    if content.len() > prepared::MAX_EDIT_BYTES {
        bail!(
            "content is larger than the {}-byte limit",
            prepared::MAX_EDIT_BYTES
        );
    }
    // The path as the agent gave it: shorter than the canonical one, and it
    // does not spell out where the session's directory is.
    let path = required_string(arguments, "path")?.to_owned();
    let bytes = content.len();
    let created = run_blocking(move || target.write(content.as_bytes())).await?;
    let verb = if created { "created" } else { "replaced" };
    Ok(format!("{verb} {path} ({bytes} bytes)"))
}

async fn edit_file(target: WriteTarget, arguments: &Value) -> Result<String> {
    let old = required_string(arguments, "old_string")?.to_owned();
    let new = required_string(arguments, "new_string")?.to_owned();
    let replace_all = arguments
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if old.is_empty() {
        bail!("old_string is empty; use write_file to write a whole file");
    }
    if old == new {
        bail!("old_string and new_string are the same");
    }
    let path = required_string(arguments, "path")?.to_owned();
    let shown = path.clone();
    let count = run_blocking(move || {
        target.edit(|text| {
            let count = text.matches(old.as_str()).count();
            if count == 0 {
                bail!(
                    "old_string was not found in {shown}; it must match exactly, including \
whitespace and indentation"
                );
            }
            if count > 1 && !replace_all {
                bail!(
                    "old_string occurs {count} times in {shown}; include more surrounding text to \
make it unique, or set replace_all"
                );
            }
            // Bound the result before building it: a short old_string with
            // many matches and a long new_string could otherwise ask for more
            // memory than the host has.
            let size = (text.len() as u128 + count as u128 * new.len() as u128)
                - count as u128 * old.len() as u128;
            if size > prepared::MAX_EDIT_BYTES as u128 {
                bail!(
                    "the edit would make {shown} {size} bytes, over the {}-byte limit",
                    prepared::MAX_EDIT_BYTES
                );
            }
            Ok((text.replace(old.as_str(), &new), count))
        })
    })
    .await?;
    let noun = if count == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    Ok(format!("edited {path}: replaced {count} {noun}"))
}

fn job_id(arguments: &Value) -> Result<u32> {
    arguments
        .get("job_id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .context("job_id must be a positive integer")
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

#[cfg(test)]
async fn run_shell(
    arguments: &Value,
    root: &Path,
    cancellation: CancellationFlag,
    timeout: Duration,
) -> Result<String> {
    run_shell_with_grace(arguments, root, cancellation, timeout, LINGER_GRACE).await
}

/// [`run_shell`] started by `runner`.
async fn run_shell_under(
    runner: &dyn ShellRunner,
    arguments: &Value,
    root: &Path,
    cancellation: CancellationFlag,
    timeout: Duration,
    jobs: &Arc<ShellJobs>,
) -> Result<String> {
    run_shell_started_by(
        runner,
        arguments,
        root,
        cancellation,
        timeout,
        LINGER_GRACE,
        Some(jobs),
    )
    .await
}

/// [`run_shell`] with an explicit linger grace, so tests can make
/// cancellation or the timeout the only way a call can end.
#[cfg(test)]
async fn run_shell_with_grace(
    arguments: &Value,
    root: &Path,
    cancellation: CancellationFlag,
    timeout: Duration,
    linger_grace: Duration,
) -> Result<String> {
    run_shell_started_by(
        &AmbientRunner,
        arguments,
        root,
        cancellation,
        timeout,
        linger_grace,
        None,
    )
    .await
}

async fn run_shell_started_by(
    runner: &dyn ShellRunner,
    arguments: &Value,
    root: &Path,
    cancellation: CancellationFlag,
    timeout: Duration,
    linger_grace: Duration,
    jobs: Option<&Arc<ShellJobs>>,
) -> Result<String> {
    let command = required_string(arguments, "command")?;
    let background = arguments
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if background {
        let Some(jobs) = jobs else {
            bail!("background jobs are not available here");
        };
        jobs.can_start().map_err(anyhow::Error::msg)?;
        let child = spawn_shell(runner, command, root)?;
        let id = jobs.start(command, child);
        return Ok(format!(
            "started background job {id}. job_output {id} shows its output (with wait_seconds, \
waits for it to finish); job_kill {id} stops it. You will be told when it ends."
        ));
    }
    let child = spawn_shell(runner, command, root)?;
    let group = child.id().and_then(|pid| i32::try_from(pid).ok());
    let mut stdout = BoundedOutput::default();
    let mut stderr = BoundedOutput::default();
    let end = supervise_shell(
        child,
        &cancellation,
        timeout,
        linger_grace,
        |stream, bytes| match stream {
            Stream::Stdout => stdout.push(bytes),
            Stream::Stderr => stderr.push(bytes),
        },
    )
    .await?;
    let (status, mut note) = match end {
        ShellEnd::Exited(status) => (status, String::new()),
        ShellEnd::ExitedLingering(status) => (
            status,
            "\nnote: the shell exited, but background processes still held its output; \
they were killed and the output collected until then is shown"
                .to_owned(),
        ),
        ShellEnd::Cancelled => {
            bail!("shell tool cancelled; the process group was killed and reaped")
        }
        ShellEnd::TimedOut => bail!(
            "shell tool timed out after {} seconds; the process group was killed and reaped. \
Run long commands with background: true",
            timeout.as_secs()
        ),
    };
    // Processes the command left running in its group (started with `&` and
    // their output redirected) become a job the agent can see and stop.
    if let (Some(jobs), Some(group)) = (jobs, group)
        && group_alive(group)
        && {
            // A member just exiting can still count for a moment.
            tokio::time::sleep(Duration::from_millis(100)).await;
            group_alive(group)
        }
        && let Some(id) = jobs.adopt(command, group)
    {
        note.push_str(&format!(
            "\nnote: processes this command started are still running, as job {id}. \
job_output {id} shows whether they are; job_kill {id} stops them. They are killed when the turn \
ends."
        ));
    }
    Ok(format!(
        "exit status: {}{note}\nstdout:\n{}\nstderr:\n{}",
        status_text(status),
        stdout.render(),
        stderr.render()
    ))
}

/// Start `command` with the user's shell under `runner`, as `run_shell`
/// runs every command: its own process group, piped output, no stdin, and
/// the agent environment only.
fn spawn_shell(
    runner: &dyn ShellRunner,
    command: &str,
    root: &Path,
) -> Result<tokio::process::Child> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // The runner decides how the shell is started: directly, or inside
    // whatever contains this session's commands. Everything below applies
    // to the process it describes.
    let mut process = runner.command(Path::new(&shell), command, root)?;
    // Agent-requested commands run as a plain non-login shell: sourcing the
    // user's login profile on every tool call is slow and replays profile
    // side effects. Direct `$` input, which the user types, keeps `-l`.
    //
    // The command does not inherit the daemon's environment, which holds the
    // provider API key and whatever tokens the daemon was started with. It
    // gets only the allowlist in `crate::environment`, minus every registered
    // credential variable. The shell's own startup files (for zsh,
    // `~/.zshenv`, read even under `-c`) can still export variables.
    //
    // The child is placed in its own process group so that a timeout or a
    // cancellation kills the whole pipeline, background jobs included,
    // rather than only the shell leader.
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_agent_environment(&mut process);
    configure_process_group(&mut process);
    restrict_inherited_descriptors(&mut process);
    // The child is waited on by its supervisor, so the daemon's orphan
    // reaper leaves it alone.
    xshell_platform::children::spawn_waited(|| process.spawn(), tokio::process::Child::id)
        .with_context(|| format!("could not launch shell {shell}"))
}

/// One stream of shell output, of which only a bounded prefix is kept.
/// Output past the limit is still read and discarded: stopping at the limit
/// would block the child on a full pipe and turn a memory bound into a
/// deadlock.
#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedOutput {
    fn push(&mut self, chunk: &[u8]) {
        let remaining = MAX_TOOL_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        let retained = chunk.len().min(remaining);
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.truncated |= retained < chunk.len();
    }

    fn render(&self) -> String {
        let mut output = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            output.push_str("\n[stream output truncated]");
        }
        output
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
    use crate::process::ProcessGroupGuard;
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
                .without(Tool::DelegateTask)
                .without(Tool::WriteFile)
                .without(Tool::EditFile)
                .without(Tool::JobOutput)
                .without(Tool::JobKill),
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
                "write_file",
                "edit_file",
                "job_output",
                "job_kill",
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
        let mut output = BoundedOutput::default();
        for _ in 0..3 {
            output.push(&vec![b'x'; MAX_TOOL_OUTPUT_BYTES]);
        }
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
    async fn f12_a_run_shell_child_cannot_see_a_live_pty_master() {
        use crate::process::descriptor_leak::{leaky_pty_master, probe};
        use std::os::fd::AsRawFd;
        let master = leaky_pty_master();
        let output = run_shell(
            &json!({"command": probe(master.as_raw_fd())}),
            &manifest_dir(),
            CancellationFlag::default(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(output.contains("swept"), "{output}");
        assert!(!output.contains("leak:"), "{output}");
        assert!(!output.contains("tty:"), "{output}");
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
        // The exit code alone, not `ExitStatus`'s own "exit status: 3".
        assert!(output.starts_with("exit status: 3\n"));
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

    /// Wait (generously) for a pid written to `path` by a shell command.
    async fn recorded_pid(path: &Path) -> i32 {
        for _ in 0..1000 {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no pid was recorded in {}", path.display());
    }

    /// Whether `pid` still exists after giving the kernel up to five seconds
    /// to finish killing and reaping it.
    async fn still_alive_after_grace(pid: i32) -> bool {
        for _ in 0..500 {
            // SAFETY: kill with signal 0 only checks for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    /// The issue's reproduction: the shell exits at once, but a background
    /// job keeps its stdout open. The call must return promptly (after the
    /// short linger grace, not the 60 s timeout), say what happened, and
    /// leave no background job behind.
    #[tokio::test]
    async fn f17_a_lingering_background_job_after_a_normal_exit_returns_promptly() {
        let temporary = tempfile::TempDir::new().unwrap();
        let pid_file = temporary.path().join("pid");
        let started = std::time::Instant::now();
        let output = run_shell(
            &json!({"command": format!(
                "sleep 1000000 & echo $! > {}; echo started",
                pid_file.display()
            )}),
            temporary.path(),
            CancellationFlag::default(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
        assert!(output.starts_with("exit status: 0\n"), "{output}");
        assert!(output.contains("background processes still held its output"));
        assert!(output.contains("stdout:\nstarted"), "{output}");
        let pid = recorded_pid(&pid_file).await;
        assert!(
            !still_alive_after_grace(pid).await,
            "F17 background job {pid} survived"
        );
    }

    /// A job still holding the pipe after the timeout must not hold the
    /// call: here the holder has even left the process group with `setsid`,
    /// so the group kill cannot reach it, and the readers are abandoned.
    #[tokio::test]
    async fn f17_the_timeout_holds_even_when_a_pipe_holder_escapes_the_group() {
        if std::process::Command::new("perl")
            .arg("-e1")
            .status()
            .map_or(true, |status| !status.success())
        {
            eprintln!("skipping: perl is not available to call setsid");
            return;
        }
        let temporary = tempfile::TempDir::new().unwrap();
        let pid_file = temporary.path().join("pid");
        let command = format!(
            "perl -MPOSIX -e 'POSIX::setsid(); open(my $f, \">\", \"{}\"); \
print $f $$; close $f; sleep 1000000' & sleep 1000000",
            pid_file.display()
        );
        let started = std::time::Instant::now();
        let error = run_shell(
            &json!({ "command": command }),
            temporary.path(),
            CancellationFlag::default(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        let elapsed = started.elapsed();
        let escapee = recorded_pid(&pid_file).await;
        // SAFETY: the escapee is our own test process; clean it up.
        unsafe { libc::kill(escapee, libc::SIGKILL) };
        assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
        assert!(
            error.to_string().contains("timed out after 1 seconds"),
            "{error}"
        );
    }

    /// Cancelled after the shell has exited but while its background job
    /// still holds the pipe: the job is killed, its delayed effect never
    /// happens, and the call reports the cancellation.
    #[tokio::test]
    async fn f17_cancellation_kills_a_background_job_after_the_shell_exits() {
        let temporary = tempfile::TempDir::new().unwrap();
        let started_file = temporary.path().join("started");
        let marker = temporary.path().join("marker");
        let command = format!(
            "touch {}; (sleep 3; touch {}) & exit 0",
            started_file.display(),
            marker.display()
        );
        let cancellation = CancellationFlag::default();
        let cancel = cancellation.clone();
        let started_for_task = started_file.clone();
        let signal = tokio::spawn(async move {
            while !started_for_task.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        // The grace and timeout are far longer than the test, so only the
        // cancellation can end the call.
        let error = run_shell_with_grace(
            &json!({ "command": command }),
            temporary.path(),
            cancellation,
            Duration::from_secs(120),
            Duration::from_secs(60),
        )
        .await
        .unwrap_err();
        signal.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(error.to_string().contains("cancelled"), "{error}");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !marker.exists(),
            "F17 a cancelled background job still wrote its marker"
        );
    }

    /// The probe from the issue, run in a child copy of this test binary so
    /// the environment can be set without mutating this process's own.
    #[tokio::test]
    async fn f18_run_shell_omits_the_provider_key_and_daemon_variables() {
        const CHILD: &str = "XSHELL_F18_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Registered as the provider credential, and named to fall under
            // the `LC_*` allowlist, so only the credential removal hides it.
            crate::protect_credential_variable("LC_XSHELL_F18_PROVIDER_KEY");
            let output = run_shell(
                &json!({"command": "env"}),
                &manifest_dir(),
                CancellationFlag::default(),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
            assert!(!output.contains("provider-secret-f18"), "{output}");
            assert!(!output.contains("daemon-secret-f18"), "{output}");
            assert!(output.contains("PATH="), "{output}");
            // A user-typed command keeps the daemon's environment but never
            // the provider credential.
            let direct = crate::run_direct_shell("env", &manifest_dir())
                .await
                .unwrap();
            assert!(!direct.stdout.contains("provider-secret-f18"));
            assert!(direct.stdout.contains("daemon-secret-f18"));
            std::fs::write(std::env::var_os(CHILD).unwrap(), "ran").unwrap();
            return;
        }
        let temporary = tempfile::TempDir::new().unwrap();
        let ran = temporary.path().join("ran");
        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::tests::f18_run_shell_omits_the_provider_key_and_daemon_variables",
                "--nocapture",
            ])
            .env(CHILD, &ran)
            .env("LC_XSHELL_F18_PROVIDER_KEY", "provider-secret-f18")
            .env("XSHELL_F18_DAEMON_VARIABLE", "daemon-secret-f18")
            .stdin(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "the F18 probe failed: {status}");
        assert!(ran.exists(), "the F18 probe did not run");
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

    /// F13: on a case-insensitive volume, the `block` class, the `prompt`
    /// class and a policy's path denial all hold under any case spelling.
    #[tokio::test]
    async fn f13_block_prompt_and_policy_denials_hold_under_any_case() {
        use crate::PolicyDecision;
        use crate::{ApprovalPolicy, ApprovalRules, CallFacts, Policy, PolicyConfig};
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        if !prepared::tests::case_insensitive(&root) {
            eprintln!(
                "skipping f13 filesystem test: {} is case-sensitive",
                root.display()
            );
            return;
        }
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        std::fs::write(root.join(".ssh/id_ed25519"), "key").unwrap();
        std::fs::write(root.join("server.pem"), "pem").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "token").unwrap();
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/token"), "t").unwrap();
        let read = |path: &str| tool("read_file", json!({"path": path}));
        let sensitive = SensitivePaths::default();
        let blocked = SensitivePaths::new(["*.pem".to_owned(), "id_ed25519".to_owned()]);

        for path in [
            "server.PEM",
            "SERVER.pem",
            ".SSH/ID_ED25519",
            ".ssh/ID_ed25519",
        ] {
            let preparation = prepare_call(&read(path), &root, &sensitive, &blocked).await;
            assert!(
                matches!(preparation, Preparation::Blocked(_)),
                "f13: {path} must be blocked, got {preparation:?}"
            );
        }
        for path in [".ENV", ".Env", ".GIT/config", ".git/CONFIG"] {
            assert_eq!(
                requires_approval(&read(path), &root, &sensitive),
                Some(GateReason::SensitivePath),
                "f13: {path} must be gated"
            );
        }

        let config = PolicyConfig {
            denied: vec!["read_file: secrets/**".to_owned()],
            ..PolicyConfig::default()
        };
        let rules = ApprovalRules {
            base: Policy::parse(&config).unwrap().0,
            overlay: None,
        };
        for path in ["secrets/token", "SECRETS/token", "Secrets/TOKEN"] {
            let preparation =
                prepare_call(&read(path), &root, &sensitive, &SensitivePaths::none()).await;
            let facts = CallFacts {
                tool: "read_file",
                command: None,
                path: preparation.path(),
                gated: false,
                sensitive: false,
                writes: false,
                cwd: None,
                search_path: None,
            };
            assert!(facts.path.is_some(), "{path} prepares");
            for mode in [
                ApprovalPolicy::Off,
                ApprovalPolicy::Ask,
                ApprovalPolicy::Auto,
            ] {
                assert_eq!(
                    rules.decide(&facts, mode),
                    PolicyDecision::Deny,
                    "f13: {path} must be denied in {mode:?}"
                );
            }
        }
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
        // Text at the start, then a gigabyte of (sparse) zeros: only the
        // start may be read.
        let mut large = std::fs::File::create(root.join("large.txt")).unwrap();
        std::io::Write::write_all(&mut large, b"text").unwrap();
        large.set_len(1 << 30).unwrap();
        let output = execute_tool(
            &tool("read_file", json!({"path": "large.txt", "max_bytes": 4})),
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

    /// Run `call` the way the engine does, with `jobs` shared across calls.
    async fn run_with(call: &ToolCall, root: &Path, jobs: &Arc<ShellJobs>) -> String {
        let preparation =
            prepare_call(call, root, &SensitivePaths::none(), &SensitivePaths::none()).await;
        execute_prepared(
            call,
            preparation,
            root,
            CancellationFlag::default(),
            Duration::from_secs(60),
            &AmbientRunner,
            jobs,
        )
        .await
    }

    #[tokio::test]
    async fn write_and_edit_files_through_dispatch() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        let output = execute_tool(
            &tool(
                "write_file",
                json!({"path": "src/a.py", "content": "x = 1\ny = 1\n"}),
            ),
            root,
        )
        .await;
        assert!(output.starts_with("created "), "{output}");
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.py")).unwrap(),
            "x = 1\ny = 1\n"
        );

        let edit = |old: &str, new: &str, all: bool| {
            tool(
                "edit_file",
                json!({"path": "src/a.py", "old_string": old, "new_string": new, "replace_all": all}),
            )
        };
        let output = execute_tool(&edit("= 1", "= 2", false), root).await;
        assert!(output.contains("occurs 2 times"), "{output}");
        let output = execute_tool(&edit("= 1", "= 2", true), root).await;
        assert!(output.contains("replaced 2 occurrences"), "{output}");
        let output = execute_tool(&edit("x = 2", "x = 3", false), root).await;
        assert!(output.contains("replaced 1 occurrence"), "{output}");
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.py")).unwrap(),
            "x = 3\ny = 2\n"
        );
        let output = execute_tool(&edit("nope", "x", false), root).await;
        assert!(output.contains("was not found"), "{output}");
        let output = execute_tool(
            &tool(
                "edit_file",
                json!({"path": "missing.py", "old_string": "a", "new_string": "b"}),
            ),
            root,
        )
        .await;
        assert!(output.contains("does not exist"), "{output}");
        let output = execute_tool(
            &tool(
                "write_file",
                json!({"path": "../escape.txt", "content": "x"}),
            ),
            root,
        )
        .await;
        assert!(output.contains("outside"), "{output}");
        assert!(
            !temporary
                .path()
                .parent()
                .unwrap()
                .join("escape.txt")
                .exists()
        );
    }

    #[test]
    fn writes_are_gated_and_refused_when_approval_is_off() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        let sensitive = SensitivePaths::new([".env".to_owned()]);
        let write = |path: &str| tool("write_file", json!({"path": path, "content": "x"}));
        assert_eq!(
            requires_approval(&write("notes.txt"), root, &sensitive),
            Some(GateReason::FileWrite)
        );
        assert_eq!(
            requires_approval(&write(".env"), root, &sensitive),
            Some(GateReason::SensitivePath)
        );
        assert!(requires_approval_by_name("edit_file"));
        assert!(!requires_approval_by_name("job_kill"));
        assert!(Tool::WriteFile.writes_files() && !Tool::WriteFile.is_read_only());
        assert!(!Tool::JobOutput.is_read_only() && !Tool::JobOutput.writes_files());
        let facts = crate::CallFacts {
            tool: "write_file",
            command: None,
            path: None,
            gated: true,
            sensitive: false,
            writes: true,
            cwd: Some(root),
            search_path: None,
        };
        assert_eq!(
            crate::default_decision(&facts, crate::ApprovalPolicy::Off),
            crate::PolicyDecision::Deny
        );
        assert_eq!(
            crate::default_decision(&facts, crate::ApprovalPolicy::Auto),
            crate::PolicyDecision::Run
        );
    }

    #[tokio::test]
    async fn binary_files_are_refused_with_a_hint_for_images() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        std::fs::write(root.join("render.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
        std::fs::write(root.join("blob.bin"), b"\x01\x02\0\x03").unwrap();
        std::fs::write(root.join("text.txt"), "caf\u{e9} au lait").unwrap();
        let read = |path: &str| tool("read_file", json!({"path": path}));
        let output = execute_tool(&read("render.png"), root).await;
        assert!(
            output.contains("binary file") && output.contains("view_image"),
            "{output}"
        );
        let output = execute_tool(&read("blob.bin"), root).await;
        assert!(
            output.contains("binary file") && !output.contains("view_image"),
            "{output}"
        );
        assert_eq!(
            execute_tool(&read("text.txt"), root).await,
            "caf\u{e9} au lait"
        );
        // A multi-byte character cut by max_bytes is still text.
        let output = execute_tool(
            &tool("read_file", json!({"path": "text.txt", "max_bytes": 4})),
            root,
        )
        .await;
        assert!(output.contains("[output truncated at 4 bytes]"), "{output}");
    }

    #[tokio::test]
    async fn background_jobs_run_report_and_can_be_waited_for() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        let jobs = ShellJobs::new();
        let output = run_with(
            &tool(
                "run_shell",
                json!({"command": "echo started; sleep 0.3; echo done; exit 4", "background": true}),
            ),
            root,
            &jobs,
        )
        .await;
        assert!(output.starts_with("started background job 1."), "{output}");
        assert!(jobs.background_running());
        let output = run_with(
            &tool("job_output", json!({"job_id": 1, "wait_seconds": 30})),
            root,
            &jobs,
        )
        .await;
        assert!(output.contains("exited with status 4"), "{output}");
        assert!(output.contains("started\ndone"), "{output}");
        assert!(!jobs.background_running());
        // Read once, it is not shown again, and the agent was told by reading.
        let output = run_with(&tool("job_output", json!({"job_id": 1})), root, &jobs).await;
        assert!(output.contains("no new output"), "{output}");
        assert_eq!(jobs.take_notices(), "");
        let output = run_with(&tool("job_output", json!({"job_id": 9})), root, &jobs).await;
        assert!(output.contains("there is no job 9"), "{output}");
    }

    #[tokio::test]
    async fn an_edit_whose_result_would_exceed_the_limit_is_refused() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        std::fs::write(root.join("a.txt"), "a".repeat(1 << 20)).unwrap();
        let output = execute_tool(
            &tool(
                "edit_file",
                json!({"path": "a.txt", "old_string": "a", "new_string": "b".repeat(500_000),
                    "replace_all": true}),
            ),
            root,
        )
        .await;
        assert!(output.contains("over the"), "{output}");
        assert_eq!(
            std::fs::metadata(root.join("a.txt")).unwrap().len(),
            1 << 20
        );
    }

    #[tokio::test]
    async fn processes_a_background_shell_leaves_running_become_a_job_killed_at_turn_end() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        let jobs = ShellJobs::new();
        run_with(
            &tool(
                "run_shell",
                json!({"command": "sleep 60 >/dev/null 2>&1 &", "background": true}),
            ),
            root,
            &jobs,
        )
        .await;
        let group = jobs_group(&jobs, 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while jobs.group_of(2).is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(jobs.group_of(2), Some(group), "the survivors are adopted");
        let notice = jobs.take_notices();
        assert!(
            notice.contains("job 1") && notice.contains("status 0"),
            "{notice}"
        );
        let output = run_with(&tool("job_output", json!({"job_id": 2})), root, &jobs).await;
        assert!(output.contains("leftover processes, running"), "{output}");
        jobs.shut_down();
        assert!(!group_alive(group));
    }

    #[tokio::test]
    async fn a_job_output_wait_ends_at_the_turn_deadline() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        let jobs = ShellJobs::until(tokio::time::Instant::now() + Duration::from_millis(300));
        run_with(
            &tool(
                "run_shell",
                json!({"command": "sleep 30", "background": true}),
            ),
            root,
            &jobs,
        )
        .await;
        let started = std::time::Instant::now();
        let output = run_with(
            &tool("job_output", json!({"job_id": 1, "wait_seconds": 600})),
            root,
            &jobs,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(5), "{output}");
        assert!(output.contains("running"), "{output}");
        jobs.shut_down();
    }

    #[tokio::test]
    async fn a_finished_job_is_reported_once_in_a_notice() {
        let temporary = tempfile::TempDir::new().unwrap();
        let jobs = ShellJobs::new();
        run_with(
            &tool(
                "run_shell",
                json!({"command": "echo hi", "background": true}),
            ),
            temporary.path(),
            &jobs,
        )
        .await;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while jobs.background_running() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let notice = jobs.take_notices();
        assert!(
            notice.contains("job 1 (`echo hi`)") && notice.contains("status 0"),
            "{notice}"
        );
        assert!(notice.contains("job_output 1"), "{notice}");
        assert_eq!(jobs.take_notices(), "");
    }

    #[tokio::test]
    async fn job_kill_and_turn_end_kill_the_whole_group() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        let jobs = ShellJobs::new();
        run_with(
            &tool(
                "run_shell",
                json!({"command": "sleep 60 & sleep 60 & wait", "background": true}),
            ),
            root,
            &jobs,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let group = jobs_group(&jobs, 1);
        assert!(group_alive(group));
        let output = run_with(&tool("job_kill", json!({"job_id": 1})), root, &jobs).await;
        assert!(output.contains("killed"), "{output}");
        assert!(!group_alive(group));

        run_with(
            &tool(
                "run_shell",
                json!({"command": "sleep 60", "background": true}),
            ),
            root,
            &jobs,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let group = jobs_group(&jobs, 2);
        assert!(group_alive(group));
        jobs.shut_down();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!group_alive(group));
        let output = run_with(
            &tool("run_shell", json!({"command": "true", "background": true})),
            root,
            &jobs,
        )
        .await;
        assert!(output.contains("the turn has ended"), "{output}");
    }

    #[tokio::test]
    async fn processes_a_command_leaves_running_become_a_job() {
        let temporary = tempfile::TempDir::new().unwrap();
        let root = temporary.path();
        let jobs = ShellJobs::new();
        let output = run_with(
            &tool(
                "run_shell",
                json!({"command": "sleep 60 > /dev/null 2>&1 &"}),
            ),
            root,
            &jobs,
        )
        .await;
        assert!(output.starts_with("exit status: 0\n"), "{output}");
        assert!(output.contains("still running, as job 1"), "{output}");
        let group = jobs_group(&jobs, 1);
        assert!(group_alive(group));
        let output = run_with(&tool("job_output", json!({"job_id": 1})), root, &jobs).await;
        assert!(output.contains("leftover processes, running"), "{output}");
        let output = run_with(&tool("job_kill", json!({"job_id": 1})), root, &jobs).await;
        assert!(output.contains("killed"), "{output}");
        assert!(!group_alive(group));
        // A command that leaves nothing behind adds no job.
        let output = run_with(&tool("run_shell", json!({"command": "true"})), root, &jobs).await;
        assert!(!output.contains("still running"), "{output}");
    }

    fn jobs_group(jobs: &ShellJobs, id: u32) -> i32 {
        jobs.group_of(id).expect("the job exists")
    }
}
