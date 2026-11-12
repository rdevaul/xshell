# Tool providers: design

**Status:** proposed; stages S1, S2 and S4 implemented (`crates/xshell-execution/src/provider.rs`, `crates/xshell-session/src/daemon.rs`).
**Updated:** 12 November 2026

## 1. Summary

The engine offers four built-in tools, held in a closed `Tool` enum. A
**tool provider** lets trusted, compiled-in Rust code add more tools, such as
`memory_get` or `send_message` for a chat gateway built on `xshelld`. Every
provided tool goes through the same toolset check, approval policy, approval
prompt, audit records, turn budget and output limit as a built-in tool.

Providers are not a way to plug in tools found at run time. A provider is a
value the binary building the engine registers when it starts. The registry
of provided tools cannot change after that, and its hash is recorded with
every audited turn. Untrusted tool sources, MCP servers in particular, stay
behind the [MCP bridge](mcp-bridge.md).

## 2. Why

Some programs embed xshell's engine and need tools that belong to them, not
to the shell. A multi-user chat assistant needs per-user memory reads and
writes, message delivery and job scheduling. The only current option is a
helper command run through `run_shell`, which works badly:

| Concern | Helper command through `run_shell` | Provided tool |
|---|---|---|
| Arguments | Shell quoting, which models often get wrong | A JSON object checked against the tool's schema |
| Approval | Gated like any shell command, so it always prompts unless a word rule allows it | The tool declares whether it needs approval |
| Policy | Word rules on the command line | The tool's name, as for built-in tools (`memory_put`) |
| Confinement | Runs with the shell's full authority, so a tool for one user's memory can read another's | The provider decides what the call can touch, and the agent never gets a shell for it |
| Subagents | `run_shell` is only available with `inherit` | Read-only tools are offered to `read_only` subagents |
| Session identity | `run_shell` does not pass the session today (MCP bridge §6.3) | The call carries its session id, call id and agent |

## 3. Relationship to the MCP bridge

MCP bridge §2 rejects making MCP native to the engine. Providers keep the
properties that rejection protects:

| MCP bridge §2 concern | How providers keep it |
|---|---|
| The tool set would become open and defined by a server | The set is fixed when the engine is built. Providers are compiled in, and nothing is discovered at run time. |
| Server descriptions would be a standing prompt-injection channel | Descriptions are written by the trusted code that registers them, like the built-in ones |
| A tool could change after it was approved | A registry cannot change. Different definitions make a different registry with a different identity, recorded in audit. |
| Policy would need a new rule kind per server | Policy uses the existing tool-name rules |
| Credentials would live in the engine | No change. A provider holds whatever credentials its own code needs, as any part of the binary does. |
| Audit, approvals and headless runs would need MCP-specific events | Provided calls emit the same events as built-in calls |

An MCP server should never be registered as a provider. Use the bridge.

## 4. Design

### 4.1 The contract

```rust
#[async_trait]
pub trait ToolProvider: Send + Sync {
    fn name(&self) -> &str;                    // recorded in audit
    fn tools(&self) -> Vec<ProvidedTool>;      // read once, at registration
    async fn call(&self, tool: &str, context: ToolContext) -> anyhow::Result<String>;
}

pub struct ProvidedTool {
    pub definition: ToolDefinition,  // name, description, JSON schema
    pub access: ToolAccess,          // ReadOnly | ReadWrite
    pub approval: ToolApproval,      // NotRequired | Required
}

pub struct ToolContext {
    pub arguments: Map<String, Value>, // always a JSON object
    pub cwd: PathBuf,
    pub session_id: Option<String>,    // set by xshelld
    pub agent: Option<String>,         // "delegate-1" for a subagent
    pub call_id: String,
    pub cancellation: CancellationFlag,
}
```

`access` and `approval` are the only facts authorization uses. Each is
declared separately, because they are independent: a memory write is
read-write but needs no approval, while a read of a private store may be
read-only but need approval.

### 4.2 The registry

`ToolRegistry::builder().provider(..).build()` validates the whole set and
returns an `Arc<ToolRegistry>`:

- Names are 1–64 characters of `a-z`, `0-9` and `_`, starting with a
  letter, the same shape as the built-in names. Policy rules, audit records
  and the model all use them.
- A name may not repeat a built-in tool or another provided tool, and a
  provider may not be registered twice.
- A description must be non-empty and at most 4 KiB. The parameter schema
  must be an object schema of at most 16 KiB as compact JSON: both are sent
  with every provider request.
- A registry holds at most 64 tools (`MAX_PROVIDED_TOOLS`). Every offered
  tool's definition is sent on every provider request, so this bounds that
  standing cost.

**Identity.** The registry's identity is a lowercase-hex SHA-256 over a
canonical JSON encoding (keys sorted at every level) of each tool's provider,
name, description, schema, access and approval, in order. Order is included
because it is the order the model sees. An empty registry has no identity.

### 4.3 Toolsets

`ToolSet` still bounds both what an agent is offered and what it may
dispatch. It now holds built-in tools plus tools from one registry, as a bit
per tool:

- `ToolSet::all()`, `read_only()`, `none()` and `from_tools(..)` hold
  built-in tools only, so every existing caller keeps its meaning.
- `set.with_provided(registry)` adds every tool in the registry.
- `read_only_subset()` keeps the read-only tools of both kinds.
- `intersect(&other)` keeps provided tools only when both sets use the
  same registry (`Arc::ptr_eq`). Another registry with identical
  definitions is still a different authority.
- Definitions are ordered with built-in tools first, in `Tool::ALL` order,
  then provided tools in registry order. The order is stable, so
  provider-side prompt caching keeps working.

### 4.4 Authorization: one path

A provided call follows the same steps in `run_agent_turn_with_budget` as a
built-in call:

1. **Budget:** charged as a tool call.
2. **Toolset:** a call outside the agent's set is refused before
   approval. A tool the engine knows but did not offer is reported as "not
   available to this agent", and any other name as "unknown tool".
3. **Gate:** `ToolApproval::Required` produces the new
   `GateReason::ToolDeclared`, which is treated exactly like `run_shell`'s
   gate: prompted in `ask` mode, run in `auto`, denied in `off`. The new
   reason is a wire change, so this change moves to session protocol 20.
   `NotRequired` tools are ungated, like the read-only built-in tools,
   with one exception: **`off` denies every `ReadWrite` tool**, gated or
   not, so `off` keeps meaning the agent changes nothing. A `ReadOnly`
   tool that needs no approval runs in every mode.
4. **Policy:** the existing tool-name rules apply unchanged.
   `always_allowed = ["memory_get"]`, `allowed = ["memory_put"]` (which
   prompts in `ask` mode) and `denied = ["memory_purge"]` all behave as they
   do for built-in tools, and an `allowed` list denies unlisted provided
   tools. The base-plus-overlay rule (the stricter decision wins) also
   applies.
5. **Approval and audit:** the same `ApprovalRequested` and `ToolDecision`
   events, and the same audit records.
6. **Execution:** see §4.6.

Argument patterns such as `memory_put: personal/*` are not supported yet
(§6, S3). `Rule::parse` rejects a pattern on any tool other than `run_shell`
and the path tools, so writing one fails loudly instead of matching nothing.

### 4.5 Subagents

`delegate_task` with `tools: "read_only"` gets the parent's read-only subset,
including read-only provided tools. With `"inherit"` it gets the parent's
whole set. In both cases `delegate_task` itself is removed. Built-in tools
behave exactly as before. A provided call from a subagent carries the
subagent's scope in `ToolContext::agent`.

### 4.6 Execution

- The call runs as its own Tokio task with an owned `ToolContext`, so a
  provider panic becomes a tool error ("provider panicked") instead of
  unwinding the turn's thread.
- It is bounded by the turn's per-call time limit (`shell_timeout`, from
  the limit profile). On timeout the task is aborted and the model is told.
- Cancelling the turn aborts the task at once. Providers must therefore be
  cancellation-safe. `ToolContext::cancellation` is there for work the
  provider moved off the task, such as a `spawn_blocking` job.
- Output is truncated at the engine's tool-output limit (64 KiB). An `Err`
  becomes `tool error: …`. Arguments that are not a JSON object are refused
  before the provider is called.
- Every executed provided call counts as tool activity, so a failed turn
  does not roll back history over effects a provider may have had.
- A call stopped by the time limit or by cancellation may already have had
  effects, and the model may try it again. Make writes idempotent where
  you can, for example by keying them on `ToolContext::call_id` or on a
  key in the arguments.

### 4.7 Audit

The audit `TurnStarted` record gains an optional `provided_tools` field,
written only when the registry is not empty:

```json
"provided_tools": {
  "sha256": "…",
  "tools": [{"provider": "memory", "name": "memory_purge",
             "access": "read_write", "approval": "required"}]
}
```

Records from engines without providers are unchanged and hash as before. The
field is covered by the record's signature like the rest of the turn's
authority. Descriptions and schemas are not repeated in every turn record. A
reviewer checks `sha256` against the definitions in the binary that
registered them (§7, question 2).

When a session limits its agent to some tools (§5.2), `TurnStarted` also
carries `offered_tools`: the names of the tools the agent was offered, in
the order the model saw them. It is absent when the agent was offered every
built-in and provided tool.

### 4.8 What a provider is responsible for

- **Validating arguments** against its schema. The engine checks only that
  they are an object, and adds no JSON Schema dependency.
- **Confining its own effects.** The engine's path confinement and
  sensitive-path policy apply to the built-in file tools, which open paths
  through xshell. A memory provider must itself keep one session's user out
  of another user's files.
- **Not blocking** the async runtime.
- **Telling the truth in `tools()`.** The registry's identity is computed
  from it.

## 5. Where providers are registered

`DaemonPolicy` has a new `tools: Arc<ToolRegistry>` field. `xshelld` builds
every agent turn with `ToolSet::all().with_provided(registry)` and the
turn's session id, and passes the registry to the audit record. The stock
`xshelld` registers no providers (`ToolRegistry::empty()`), so nothing
changes for existing deployments. The standalone CLI likewise has none.

### 5.1 Building a daemon with providers

The daemon is a library, `xshell_session::daemon`, and the `xshelld` binary
is `Daemon::new().main()`. A program that compiles in providers builds the
same daemon:

```rust
use std::sync::Arc;
use xshell_execution::ToolRegistry;
use xshell_session::daemon::Daemon;

fn main() -> anyhow::Result<()> {
    let tools = ToolRegistry::builder()
        .provider(Arc::new(my_memory::Provider::new()))
        .build()?;
    Daemon::new().with_tools(tools).main()
}
```

The result accepts the same command line and configuration file as `xshelld`
and has every command: serving, `probe`, `serve-stdio`, `serve-pty-stdio`
and `service`. It speaks the same session protocol, so `xshell` clients
connect to it unchanged. `Daemon::run_with_args` takes an explicit command
line, for programs that build one themselves. At startup the daemon prints
the provided tools and the first 12 characters of the registry identity, so
an operator can match a running build against its audit records.

`main` and `run_with_args` run the daemon as the whole process: they install
the SIGTERM and SIGINT handlers that stop it, print to stdout, and exit the
process on `--help` or an argument error. Call them from a program that is
the daemon, not from a thread inside a larger one.

Things to know when replacing `xshelld` with your own build:

- **Service unit.** `service install` writes the same per-user unit as
  `xshelld` (`com.xshell.xshelld` on macOS, `xshelld.service` on Linux),
  pointing at the binary that ran the command. Installing your build
  replaces a stock `xshelld` service, and the reverse. One daemon per user
  is the intended state.
- **Remote connections.** `//connect` runs `xshelld probe` and
  `xshelld serve-stdio` by name on the remote host. Those commands only
  proxy to the daemon's socket, so a stock `xshelld` of the same version on
  the remote `PATH` can serve your build there. Otherwise, install your
  binary under the name `xshelld`.
- **Versions.** Your build and its clients must use the same session
  protocol version, as for `xshelld`.

Programs that do not want the whole daemon can still build an
`ExecutionCoordinator` with their own `DaemonPolicy`, or call
`run_agent_turn` directly.

### 5.2 Per-session tool selections

One daemon often serves agents that should not have the same tools. A chat
gateway, for example, gives each user a session with memory tools but no
`run_shell`. A session's creation can name the tools its agent may use:

```rust
client.create(SessionCreation {
    // ...
    tools: SessionTools::Only(vec![
        "read_file".into(),
        "memory_get".into(),
        "memory_put".into(),
    ]),
})?;
```

On the wire this is `"tools": {"only": ["read_file", "memory_get", "memory_put"]}`.
Omitting the field, or sending `"all"`, gives every tool the daemon offers.

- **Only narrows.** A selection can name built-in and provided tools. The
  agent is offered exactly those, and a call to any other tool is refused as
  "not available to this agent" before policy or approval, as for any tool
  outside a toolset. Subagents are bounded by the session's tools, because
  they are bounded by their parent's. The approval mode and policy still
  apply to the tools that remain. `Only([])` leaves the agent no tools.
- **Checked at creation.** A name the daemon does not offer, or a repeated
  name, makes creation fail, so a typo cannot silently leave an agent
  without a tool.
- **Fixed for the session's life.** A selection cannot be changed later, so
  a session's authority never grows after it is created. To change a
  user's tools, create a new session.
- **Persisted.** Durable sessions keep their selection across daemon
  restarts. If a later build no longer offers a selected tool, a restored
  session simply does not get it. The session state file moves to format 3,
  so an older daemon refuses the file instead of dropping selections, which
  would give those sessions every tool.
- **Visible.** The selection is part of the session descriptor (`tools`,
  absent for `all`), so catalogs show it, and each turn's audit record
  names the offered tools (§4.7).

The selection is set by the client that creates the session. Clients are
trusted the way they already are: they connect as the daemon's own OS user,
over its socket or over SSH. A selection lets a trusted client, such as a
gateway, give less authority to the agents it runs on others' behalf. It is
not a defense against the client itself.

The `xshell` CLI does not yet offer a way to choose a selection; a
`--tools` option on `//new` and `xshell run` would be a small follow-up.

## 6. Staged plan

| Stage | Content | Status |
|---|---|---|
| S1 | Provider trait, registry, toolset, engine dispatch, gate reason, delegation, `DaemonPolicy.tools`, session id, audit field | Done (#121) |
| S2 | **Embeddable daemon.** `xshelld` moved into the library as `xshell_session::daemon::Daemon`, with `with_tools` (§5.1) | Done (#122) |
| S3 | **Argument-scoped policy.** An optional `ToolProvider::policy_subject(tool, arguments) -> Option<String>` returning the string an argument pattern matches, so rules such as `memory_put: shared/*` work | When a provider needs it |
| S4 | **Per-session tool selections.** A session's creation names the built-in and provided tools its agent may use (§5.2) | Done |
| S5 | **Model-facing errors and output.** Optional structured results (images through the content-addressed image store, A1–A4) | After A4 |

Out-of-process providers are not planned. A tool that has to run out of
process should be an MCP server behind the bridge.

## 7. Open questions

1. **Time limit.** Provided calls reuse `shell_timeout`. Should they get a
   separate `tool_timeout` in the limit profiles?
2. **Full definitions in audit.** Should the daemon write each tool's
   description and schema once, at startup or in the first turn of each
   audit log, so `sha256` can be verified from the log alone?
3. **Read-only and sensitive data.** Should a provider be able to mark one
   call sensitive (prompting like a `prompt`-class path) based on its
   arguments, or is `ToolApproval::Required` on a separate tool enough?
