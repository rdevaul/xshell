# MCP bridge: design and level of effort

**Status:** §1–§5 accepted, with the decisions recorded in §5; §6 (large
local stacks) proposed
**Updated:** 10 November 2026

## 1. Summary

Many useful services are only available as Model Context Protocol (MCP)
servers. This plan lets xshell agents use them without making MCP part of
the engine. A separate client, `xshell-mcp`, talks to MCP servers through a
broker process, `xshell-mcpd`, that `xshelld` supervises, and agents reach it the way they reach any
program: through `run_shell`. The broker holds every credential, runs each
local server in a sandbox, and pins each server's tool list, so the agent
can ask for calls but can never see a token or see a changed tool list it
has not been approved for.

| Part | LoE (engineer-days) |
|---|---|
| M1. Broker, stdio transport, `xshell-mcp` CLI | 7–10 |
| M2. Streamable HTTP transport and OAuth | 5–7 |
| M3. Per-server sandbox profiles | 3–5 |
| M4. Tool-list pinning, policy integration, documentation | 2–3 |
| **Total** | **17–25 days (about 3.5–5 weeks)** |

The figures are for one engineer who knows the codebase, and include tests,
documentation, and review fixes. Add 2 days for a security review of
credential storage and the broker socket. Image results depend on
workstream A of the [agentic CAD plan](agentic-cad-plan.md) (A1–A4); without
it, images are saved to files and only their paths are printed.

## 2. Why not native MCP

| Concern | Native MCP in the engine | Bridge |
|---|---|---|
| Tool set | `Tool` is a closed enum and `ToolSet` a `u8` bitset (`xshell-execution/src/tools.rs`); MCP would make the set open and server-defined | Unchanged; MCP is a program `run_shell` runs |
| Descriptions | Every server's tool descriptions enter the system prompt on every turn, a standing prompt-injection channel | Descriptions reach the model only when it runs `describe`, as untrusted tool output |
| Changed tools | A server can change a tool's description or behaviour after it was approved, and the model sees the new one silently | Tool lists are pinned by hash; a change stops calls until the user approves it (§3.5) |
| Policy | Policy rules would need a new rule kind for each server's tools | Existing `run_shell` word rules apply, plus a broker-side allowlist |
| Credentials | Tokens live wherever the engine runs tools | Tokens live only in the broker |
| Audit, approvals, headless | Need MCP-specific events | Already cover `run_shell` calls |

The protocol also includes features that work against xshell's model:
*sampling* lets a server send prompts to the user's model, and
*elicitation* lets a server ask the user questions mid-call. The bridge does
not offer either (§3.6).

## 3. Design

### 3.1 Components

```
agent ──run_shell──▶ xshell-mcp (CLI) ──Unix socket──▶ xshell-mcpd (broker, supervised by xshelld)
                                                         ├── stdio server, one per session (sandboxed child)
                                                         └── HTTP server (OAuth tokens held here)
```

Everything right of the socket runs on the machine where the session's
daemon runs, as §3.10 explains.

- **`xshell-mcp`:** a small client binary, new crate `xshell-mcp`. It holds
  no state and no credentials; it formats requests and prints results.
- **Broker:** a separate process, `xshell-mcpd`, that `xshelld` starts,
  restarts on failure, and stops with itself. It listens on a socket in the
  daemon's private state directory next to `xshelld.sock`. Keeping it out
  of `xshelld` means a misbehaving server or transport bug cannot take
  down sessions, and the broker can later run as a different user from the
  agent's shell (§3.3).
- **Server instances are per session.** The broker starts a server on a
  session's first call to it, keeps that instance for the session's later
  calls (many servers are slow to start), and stops it after an idle
  timeout or when the session closes. Sessions never share an instance, so
  a stateful server (a browser, a database connection) cannot carry state
  from one session to another. The CLI identifies its session from the
  environment `run_shell` provides; a call without one is refused. A
  per-broker cap on running instances bounds the cost.
- **What is shared across sessions:** configuration, tool-list pins, and
  login tokens, which belong to the user on that machine rather than to
  one session.

### 3.2 The CLI

The client is generic: it reads tool definitions from the server at run
time, so nothing is generated or regenerated when a server changes.

| Command | Output |
|---|---|
| `xshell-mcp servers` | Configured servers, their transport, and whether they are logged in |
| `xshell-mcp list SERVER` | The server's allowed tools, one line each: name and the first line of the description |
| `xshell-mcp describe SERVER TOOL` | The tool's description and input schema |
| `xshell-mcp call SERVER TOOL --json '{...}'` | Calls the tool with a JSON object |
| `xshell-mcp call SERVER TOOL --arg key=value ...` | Calls the tool with arguments built from flags; values are typed by the input schema |
| `xshell-mcp call SERVER TOOL --json-file PATH` | Reads the arguments from a file, avoiding shell quoting |

Exit codes follow the `xshell run` pattern: 0 success, 1 the tool reported
an error (`isError`), 2 usage (unknown tool, arguments that fail the
schema), 6 broker unavailable, and new codes for `auth_required` (8) and
`pin_mismatch` (9). Each failure prints a one-line reason the model can act
on, such as "github: login required; ask the user to run
`xshell mcp login github`".

### 3.3 Credentials and login

- **Login:** `xshell mcp login SERVER` runs interactively, by the user. For
  HTTP servers it follows the MCP authorization flow: OAuth 2.1 with PKCE,
  protected-resource and authorization-server metadata discovery, and
  dynamic client registration when the server supports it (otherwise a
  client ID from configuration). The browser opens on the user's machine.
- **Storage:** tokens go in the macOS Keychain or the Linux secret service,
  under an item only the broker reads. Static secrets for stdio servers
  (API keys passed in the environment) are stored the same way and injected
  into the child's environment by the broker; they never appear in
  configuration files the agent can read.
- **Files:** any token cache or broker state on disk lives under the daemon's
  state directory and is listed in a `block` sensitivity class by default, so
  agent read tools cannot open it in any mode.
- **Refresh:** the broker refreshes tokens itself. It never starts a browser
  flow on its own.
- **Headless runs:** an expired or missing login fails the *call*, not the
  turn: `xshell-mcp` exits with `auth_required` and the model is told to ask
  the user to log in. The turn continues, as with any failed command.
- **Limits of same-user isolation:** the agent's shell runs as the same user
  as the broker. Keychain access control prompts the user when another
  program asks for the broker's item, but a shell running as the user can
  still try. Isolating credentials completely needs the shell sandbox or a
  separate user (§8 of the CAD plan); this design does not depend on it,
  but becomes stronger with it.

### 3.4 Sandboxing local servers

Each stdio server is launched by the broker under a sandbox whose limits
come from its configuration entry:

- **macOS:** a generated `sandbox-exec` profile.
- **Linux:** bubblewrap where available, Landlock otherwise.
- **Declared needs:** readable and writable paths, network hosts (or
  `network = "none"`), and environment variables. Anything else is denied.

This is a smaller problem than sandboxing `run_shell` (CAD plan §8, "Shell
safety beyond pattern matching"), because each server's needs are known in
advance and written down. It is also a useful first use of the same
machinery, and the profiles it produces can inform that design.

A server whose configuration declares no limits runs with `sandbox =
"none"` only if the configuration says so explicitly; otherwise it does not
start.

### 3.5 Tool-list pinning

- At login, or the first time a server starts, the broker records a hash of
  its canonical tool list: names, descriptions, input schemas, and
  annotations.
- On every later start, and whenever the server sends a `tools/list_changed`
  notification, the broker hashes the list again. On a mismatch it refuses
  calls with `pin_mismatch` until the user runs `xshell mcp approve SERVER`,
  which shows a diff and records the new hash.
- **Annotations are untrusted.** A server's `readOnlyHint` or
  `destructiveHint` is only a hint from the server. Only the configuration
  can mark a tool read-only, and nothing in xshell's policy uses the server's
  hints.

### 3.6 Supported subset of MCP

| Feature | Supported | Notes |
|---|---|---|
| Tools | Yes | `tools/list`, `tools/call` |
| Resources | Linked reads only | `xshell-mcp read SERVER URI`, only for a URI a tool returned as a `resource_link` earlier in the same session (§3.7). Listing, templates, and subscriptions are v2 |
| Prompts | No | Server-written prompt templates are not needed |
| Sampling | Refused | The broker declares no sampling capability and rejects requests |
| Elicitation | No | A server that needs it returns an error |
| Progress and logging notifications | Logged only | Written to the broker's log; not shown to the model |

### 3.7 Results

- **Text** content is printed to stdout, one block after another.
- **`structuredContent`** is printed as JSON (with `--output json`, the whole
  result is printed as one JSON object).
- **Images** are ingested into the workstream A content-addressed store
  through the same bounded decode and re-encode as `view_image`, and printed
  as a reference (`[image <sha256>: github/get_screenshot]`). The engine
  attaches referenced images to the tool's follow-up message, as for
  `view_image`. This is the dependency on A1–A4.
- **Embedded resources** are printed as their text; binary resources are
  saved to a file under the session's state directory and the path is
  printed.
- **Resource links** are printed as their URI and name, and the broker
  remembers the URI for that session's server instance. `xshell-mcp read`
  accepts only remembered URIs. Some servers return large outputs this way
  rather than inline, so tool use needs this much; general resource
  browsing does not, and waits for v2.
- **Sanitization applies to every piece of content**, from a tool result or
  a linked read alike, before anything is printed:
  - text must be valid UTF-8, and terminal control sequences and other
    control characters except newline and tab are removed, so a server
    cannot drive the user's terminal or hide text from the reviewer;
  - binary content is never printed: it is saved to a file, and images go
    through image ingestion;
  - a MIME type from the server is a label only; content is checked for
    what it actually is;
  - every result, and every saved file, has a size limit.
- **Size:** output is bounded like every tool's (64 KiB today); the broker
  truncates and says so.

### 3.8 Policy integration

Two layers decide what an agent may call:

1. **Broker allowlist (hard limit).** Each server's configuration lists the
   tools that may be called at all. The broker enforces it, so a call that
   reaches the socket some other way, for example from a script, is still
   limited to those tools.
2. **Approval policy (who decides).** `run_shell` word rules decide which
   calls run, prompt, or are denied, exactly as for any command:

```toml
[session_fabric.approval_policy]
always_allowed = [
  "run_shell: xshell-mcp list *",
  "run_shell: xshell-mcp describe *",
  "run_shell: xshell-mcp call github list_issues *",
  "run_shell: xshell-mcp call github get_issue *",
]
allowed = ["run_shell: xshell-mcp call github *"]   # prompts in ask mode
denied  = ["run_shell: xshell-mcp call github delete_* *"]
```

Rules match the parsed argument vector, so arguments cannot change which
tool a call names. They cannot see inside the arguments, though: a rule can
allow `create_issue`, but not `create_issue` on one repository only.

**Later option: a closed `McpCall` tool.** A single new variant,
`Tool::McpCall`, taking `{server, tool, arguments}`, would keep the tool set
closed (one variant, not one per MCP tool) and would:

- pass arguments as structured JSON, avoiding shell quoting that models
  often get wrong;
- allow rules like `mcp_call: github/create_issue`, a new rule kind in
  `policy.rs`;
- attach image results directly instead of through a printed reference;
- be marked read-only per tool by configuration, so read-only MCP tools
  could follow "today's rules" for read-only tools.

The cost is engine work (a variant, a rule kind, dispatch, audit records)
and one more tool advertised to every model. The recommendation is to ship
the CLI through `run_shell` first, which needs no engine changes and is
useful to people and scripts too, and add `McpCall` if quoting errors or
policy granularity become a problem in practice.

### 3.9 Configuration

```toml
[mcp]
enabled = false
# Stop a server after this long without a call.
idle_timeout_seconds = 600

[mcp.servers.github]
transport = "http"
url = "https://api.githubcopilot.com/mcp/"
# Tools that may be called at all; the broker refuses the rest.
tools = ["list_issues", "get_issue", "create_issue", "add_comment"]
# Tools the configuration declares read-only (server hints are ignored).
read_only = ["list_issues", "get_issue"]

[mcp.servers.filesystem-docs]
transport = "stdio"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/docs"]
tools = ["read_file", "list_directory", "search_files"]
[mcp.servers.filesystem-docs.sandbox]
read = ["/srv/docs"]
write = []
network = "none"
# Names of secrets the broker injects from the keychain into the environment.
# secrets = ["DOCS_API_KEY"]
```

### 3.10 Remote sessions

The broker, its servers, and its tokens all live on the machine where the
session's daemon runs, and every call is made from that machine. An agent
on a build host calls that host's broker, never one on the user's laptop.
This matters because:

- a server may exist only on that machine, such as a local service or a
  tool that works on its files;
- that machine may sit in a segmented network that reaches resources the
  user's machine cannot, and the reverse.

Consequences:

- **Configuration is per daemon.** Each machine's daemon configuration
  declares its own servers; nothing is forwarded from the controller.
- **Login happens on the daemon's machine.** `xshell mcp login SERVER` run
  from a controller is carried out by the remote broker. The OAuth redirect
  goes to a loopback listener on the remote machine, so xshell forwards
  that port to the user's browser over the session connection, the same
  way an SSH port forward would. Where a server supports the OAuth device
  authorization grant, that is used instead, and no forwarding is needed.
- **Tokens never travel.** They are stored on the remote machine and are
  never sent to the controller, and the controller's tokens are never sent
  to the remote.

## 4. Tasks and estimates

| # | Task | LoE (days) |
|---|---|---|
| M1 | `xshell-mcp` crate and CLI; `xshell-mcpd` broker supervised by `xshelld`, with socket, per-session instances, lazy start, idle timeout, instance cap; stdio transport; sanitized result printing; linked resource reads; exit codes | 7–10 |
| M2 | Streamable HTTP transport; `xshell mcp login`, including on a remote daemon (forwarded loopback redirect, device grant); OAuth 2.1 with PKCE, metadata discovery, dynamic registration; keychain and secret-service storage; refresh | 5–7 |
| M3 | Sandbox profiles from configuration: `sandbox-exec`, bubblewrap, Landlock; secret injection; tests that a server cannot reach undeclared paths or hosts | 3–5 |
| M4 | Tool-list pinning and `xshell mcp approve`; broker allowlist; default `block` class for broker state; policy examples; user documentation | 2–3 |
| | **Total** | **17–25** |

Image results (§3.7) are a small addition once A1–A4 land, about 1 day.

**Risks:**

- **Server variety.** Servers differ in how closely they follow the
  specification, especially in OAuth discovery and registration. Test
  against several real servers before calling M2 done.
- **Same-user credential access** (§3.3). The keychain prompt is the only
  barrier between the agent's shell and the broker's tokens until shell
  sandboxing exists.
- **Policy granularity.** Word rules see the tool name but not its
  arguments; `McpCall` or a per-tool argument filter would be needed for
  finer control.
- **Tool output as injection.** MCP results are as untrusted as any command
  output. The approval policy remains the defence.
- **`sandbox-exec`** is deprecated on macOS, though still used widely; a
  replacement may be needed later.

## 5. Decisions and open questions

### Decided (9 November 2026)

1. **The CLI through `run_shell` first.** `McpCall` stays a later option
   (§3.8).
2. **The broker is a separate process,** `xshell-mcpd`, supervised by
   `xshelld` (§3.1).
3. **Tokens live on the daemon's machine, and calls are made from it**
   (§3.10). A server may only be reachable from that machine, or from the
   network segment it sits in.
4. **Server instances are per session** (§3.1).
5. **Resources:** v1 reads only URIs a tool returned as a `resource_link`,
   through the same sanitization as tool results (§3.7). Resource listing,
   templates, and subscriptions are v2.

### Still open

1. **Running the broker as a separate user.** A separate process makes
   this possible, and it would close the same-user credential gap (§3.3).
   It needs an install step that creates the user, and it should be
   decided together with the shell sandbox (CAD plan §8).

## 6. Large local stacks (proposed)

**Status:** Proposed, 10 November 2026. Nothing here changes a §5 decision;
each item extends the accepted design for a case it does not yet cover.

### 6.1 The case

The design above is shaped by small remote services (a GitHub server, a
documents server). The first concrete integration target is a different
shape: **mechatron**, an agentic mechatronics platform that is a stack of
local MCP servers (assembly graph, geometry kernel, COTS catalog, dynamics
simulation, viewer, slicer, and others). Its gateway exposes the whole stack
as one Streamable HTTP endpoint (`http://127.0.0.1:3100/mcp` by default),
routes `tools/call` by tool-name prefix, and isolates clients by
`Mcp-Session-Id`. Observed at mechatron `21ae43309`:

| Property | Value | Why it matters here |
|---|---|---|
| Transport | Streamable HTTP on loopback, started outside xshell | No broker-launched instance, no sandbox profile, no OAuth |
| Authentication | Static bearer key (`MECHATRON_API_KEYS`), or none in development mode | §3.3 covers only OAuth for HTTP servers |
| Tool count | About 520 documented tools (assembly about 305, viewer about 125, sim about 36, others) | Name-by-name allowlists and an unfiltered `list` do not scale |
| Call duration | `harness.turn` is documented to block for minutes; meshing, simulation, and rendering calls can also be long | Calls can outlive the default 60 s `run_shell` timeout and need defined cancellation |
| Server-side state | A durable design world selected by an explicit `session` argument on every call; the `default` world is a human's live session | Isolation depends on an argument, not on the instance |
| Own agent loop | `harness.turn` / `harness.submit_turn` run mechatron's model loop with its own model credentials and spend | A tool call that is itself an agent (roadmap M7) |

The driving use is a benchmark of CAD agents (CADbakeoff), where xshell is
the model-agnostic reference harness and mechatron would be one of the
toolsets the agent drives, but none of the requirements below are
benchmark-specific: any large local stack (a robotics, EDA, or lab-automation
platform) has the same shape.

### 6.2 Gaps and proposed changes

**G1. Static-token and loopback HTTP servers.** §3.3 assumes an HTTP server
uses the MCP authorization flow. Add two cases:

```toml
[mcp.servers.mechatron]
transport = "http"
url = "http://127.0.0.1:3100/mcp"
auth = "bearer"            # "oauth" (default for http), "bearer", or "none"
secret = "MECHATRON_API_KEY"   # bearer: keychain item the broker sends as the token
```

- `bearer`: the user stores the token with `xshell mcp set-secret SERVER`;
  the broker reads it from the keychain and sends it as
  `Authorization: Bearer`. It never appears in a file the agent can read or
  in the child environment, as §3.3 already requires for stdio secrets.
- `none`: allowed only for a loopback URL (`127.0.0.0/8`, `::1`, or a Unix
  socket). A non-loopback URL with `auth = "none"` is a configuration error,
  so an unauthenticated server is never reached across a network by
  accident.
- §3.4 sandboxing does not apply to a server xshell does not launch; `list`
  and `servers` say so (`sandbox: external`), so nobody reads a loopback
  server as confined.

**G2. Hundreds of tools.**

- **Allowlist patterns.** `tools` accepts `*` globs on dotted names
  (`"assembly.*"`, `"viewer.render_*"`), and a `deny_tools` list applies after
  it. Patterns are expanded against the pinned list (§3.5), and the pin
  records the expanded set, so a new tool matching a pattern still needs
  `xshell mcp approve` before it can be called.
- **Filtered, paged listing.** `xshell-mcp list SERVER [--prefix P]
  [--grep WORD] [--page N]` keeps each listing well inside the 64 KiB tool
  output bound (at about 120 bytes a line, 520 one-line entries already
  approach it). `list` with no filter on a large server
  prints counts per prefix and a hint, not every tool.
- **Readable approvals.** `xshell mcp approve` on a large list shows counts
  of added, removed, and changed tools and the changed names first, then the
  full diff on request; an actively developed stack changes its list often.

**G3. Long calls and cancellation.** The design does not define timeouts or
cancellation, and `run_shell` ends a command with a group-wide `SIGKILL`
(`xshell-execution/src/process.rs`), so the CLI cannot clean up after itself.

- **The broker treats a closed client connection as cancellation.** When the
  CLI's socket closes before the result is written, the broker sends
  `notifications/cancelled` for that request to the server and records the
  call as *cancelled, outcome unknown*. A server may already have applied
  the effect; xshell reports that honestly rather than as "not run" (roadmap
  D2, M1.2).
- **Call timeout.** `xshell-mcp call --timeout DURATION`, defaulting to a
  per-server `call_timeout` in configuration. The CLI refuses a timeout longer
  than the session's `shell_timeout`, so the broker, not `run_shell`'s kill,
  ends an over-long call and the model gets a clear reason. Long CAD calls
  use the `relaxed` limit profile (30 min) or a configured one.
- **Progress keeps nothing alive.** Progress notifications stay logged only
  (§3.6); they do not extend a timeout.
- **Concurrency.** Today one turn's tool calls run one after another, but a
  user or script can call the broker concurrently. The broker allows
  concurrent requests to one instance up to a per-server `max_in_flight`
  (default 4) and queues the rest; a queued call's timeout includes its wait.

**G4. Server-side state selected by arguments.** §3.1's isolation ("server
instances are per session") holds for servers the broker starts. A shared
external server cannot be given one instance per session, and mechatron
selects its world by an argument, not an instance. Add **bound arguments**:

```toml
[mcp.servers.mechatron.bind]
session = "xshell/{session}"   # {session} is the xshell session name
```

- The broker sets each bound argument on every call whose schema has it, and
  refuses (exit 2) a call that supplies a different value. The agent cannot
  name another world, including the human's `default` one, by mistake or by
  injection.
- The broker also keeps one `Mcp-Session-Id` per xshell session per server,
  so a server that isolates by MCP session gets the same per-session
  separation.
- Where neither applies, `servers` marks the server `state: shared`, so the
  user knows sessions can see each other's effects through it.

**G5. Large results.** A turn report or graph dump can exceed the 64 KiB
bound. Rather than truncate structured output mid-object, the broker writes
an over-limit sanitized result to a file under the session's state
directory, as it already does for binary resources (§3.7), and prints its
path, size, and the first part of the result. Agent read tools reach that
file under the normal read policy.

**G6. Calls that run another agent.** `harness.turn` hands the request to
mechatron's own model loop, with its own model, credentials, and spend, so
its work is not under xshell's approval policy, turn budget, or audit
(roadmap M7: a connector must not bypass shared authorization because it
supplies its own agent loop).

- Such tools are **denied by default**: a server's configuration lists them
  under `agent_tools`, and a pattern allowlist (G2) never admits them; only
  an explicit entry in `tools` does.
- When allowed, the audit record and headless `result` mark the call as
  delegated to an external agent, and the documentation states that its
  model spend and internal tool calls are outside xshell's budget and audit.
- For benchmarking, this is also a measurement concern: an agent that hands
  the design to mechatron's harness is measuring that harness, not itself.

**G7. Structured arguments.** Mechatron tools take nested JSON arguments, and
`--json-file` (§3.2) avoids quoting for them. This is the case most likely
to produce the quoting errors that would justify `McpCall` (§3.8). Record
argument-parse failures (exit 2) per server in the broker log, so the §5
decision to start with the CLI can be revisited with evidence.

### 6.3 Conformance tests

Against a fixture server in the repository (no mechatron dependency): a
loopback Streamable HTTP server that generates about 500 tools with dotted
names, takes a `session` argument, has one tool that sleeps until cancelled,
and one that returns a 1 MiB structured result.

| Test | Pass condition |
|---|---|
| Loopback `auth = "none"` | Connects; the same entry with a non-loopback URL is a configuration error |
| Bearer secret | Token reaches the server; it is absent from every file and environment the agent can read |
| Pattern allowlist and pinning | `assembly.*` admits only matching tools; a new matching tool is refused with `pin_mismatch` until approved |
| Filtered listing | Every `list` output, including the unfiltered summary, stays under the tool output bound |
| `SIGKILL` mid-call | The server receives `notifications/cancelled`; the call is recorded as cancelled with unknown outcome |
| Timeout above `shell_timeout` | Refused with a reason before the call is sent |
| Bound argument | Set when omitted; a different supplied value is refused; two sessions never see each other's world |
| Large result | Written to a file; the printed output is bounded and names the path |
| Agent tool | Refused under a pattern allowlist; when listed explicitly, audit marks the call as external-agent |

### 6.4 Estimate

| # | Task | LoE (days) | Lands with |
|---|---|---|---|
| L1 | `auth = "bearer" / "none"`, loopback check, `set-secret` | 1 | M2 |
| L2 | Pattern allowlists, `deny_tools`, filtered and paged `list`, approval summary | 1.5–2 | M1, M4 |
| L3 | Disconnect-as-cancel, call timeouts, `max_in_flight` | 2–3 | M1 |
| L4 | Bound arguments and per-session `Mcp-Session-Id` | 1–1.5 | M1 |
| L5 | Over-limit results to file | 0.5–1 | M1 |
| L6 | `agent_tools` default-deny and audit marking | 1 | M4 |
| L7 | Fixture server and the §6.3 tests | 1.5–2 | M1–M4 |
| | **Total** | **8.5–11.5** | |

L3 is not mechatron-specific: every server with slow calls needs it, so it
belongs in M1 regardless of this case. With §6, the bridge totals about
26–37 engineer-days.

### 6.5 Open questions

1. **Bound arguments versus server-side isolation.** Bound arguments make a
   server's world selection enforceable without the server's help. Should
   the bridge prefer them, or ask servers to isolate by `Mcp-Session-Id` and
   treat argument binding as a stopgap?
2. **Where `agent_tools` is declared.** Configuration only (as above), or
   also inferred from a server's documentation? Inference would be a server
   hint, which §3.5 treats as untrusted.
3. **Image results from renders.** Mechatron's viewer returns renders; those
   need A1–A4 and the §3.7 image path, which this section assumes rather than
   changes.
