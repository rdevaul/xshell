# Tool providers: design

**Status:** proposed; stage 1 implemented (`crates/xshell-execution/src/provider.rs`).
**Updated:** 11 November 2026

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
  must be an object schema.
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
   reason is a wire change, so this change moves to session protocol 19.
   `NotRequired` tools are ungated, like the read-only built-in tools.
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

Today a program can use providers by building `ExecutionCoordinator` with
its own `DaemonPolicy`, or by calling `run_agent_turn` directly. A
downstream binary that wants the whole daemon with its own providers needs
stage S2.

## 6. Staged plan

| Stage | Content | Status |
|---|---|---|
| S1 | Provider trait, registry, toolset, engine dispatch, gate reason, delegation, `DaemonPolicy.tools`, session id, audit field | This change |
| S2 | **Embeddable daemon.** Move `xshelld`'s serve loop from `src/bin/xshelld.rs` into the library (for example `xshell_session::daemon::run(options, registry)`) so a downstream binary can register providers and otherwise behave exactly like `xshelld` | Next. A mechanical move. |
| S3 | **Argument-scoped policy.** An optional `ToolProvider::policy_subject(tool, arguments) -> Option<String>` returning the string an argument pattern matches, so rules such as `memory_put: shared/*` work | When a provider needs it |
| S4 | **Per-session toolsets.** A session's creation names which provided tools it gets (always within the registry and within the daemon's policy), so one daemon can give its users different tool sets | Needed for multi-user gateways |
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
