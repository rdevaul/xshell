# MCP bridge: design and level of effort

**Status:** §1–§5 accepted, with the decisions recorded in §5; §6 (large
local stacks) proposed. §6.3 proposes amendments to the accepted §3.1
(session identity) and §3.7 (result files), marked where they apply.
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

Tools written as Rust code and compiled into the binary that builds the
engine are a separate seam with none of these concerns; see
[tool providers](tool-providers.md). MCP servers are never registered that
way.

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
  per-broker cap on running instances bounds the cost. *(Amendment
  proposed in §6.3: `run_shell` sets no such variable today, and a plain
  session variable could be forged; identity comes from a per-turn token
  the broker verifies with `xshelld`.)*
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
  printed. *(Amendment proposed in §6.3: agent read tools cannot open the
  state directory, so saved content is retrieved with `xshell-mcp fetch`.)*
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

**Status:** Proposed, 10 November 2026; revised after review the same day.
No §5 decision changes. §6.3 amends the accepted §3.1 and §3.7, which the
rest of this section builds on.

### 6.1 The case

The design above is shaped by small remote services (a GitHub server, a
documents server). The first concrete integration target is a different
shape: **mechatron**, an agentic mechatronics platform that is a stack of
local MCP servers (assembly graph, geometry kernel, COTS catalog, dynamics
simulation, viewer, slicer, and others). Its gateway exposes the whole stack
as one Streamable HTTP endpoint (`http://127.0.0.1:3100/mcp` by default),
routes `tools/call` by tool-name prefix, and isolates clients by
`Mcp-Session-Id`.

The figures below were read from mechatron's source and documentation at
`21ae43309`; they cannot be checked from this repository, and should be
re-read when the work starts.

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

### 6.2 What the broker can enforce

Everything the broker enforces (the allowlist, pins, bound arguments, the
agent-tool ban) holds only for calls that go through it. A server xshell
does not launch is reachable without the broker:

- **`auth = "none"` on loopback:** any process running as the user can
  reach the server directly, including the agent through `run_shell`
  (`curl http://127.0.0.1:3100/mcp …`). The broker's controls are then
  **advisory**: they shape what a cooperating agent does, and the approval
  policy's word rules cannot stop a different command from making the same
  request.
- **`auth = "bearer"`:** the server refuses a request without the token, and
  only the broker holds it. The remaining gap is §3.3's: a shell running as
  the same user can still try to read the broker's keychain item.
- **Full enforcement** needs the shell sandbox (CAD plan §8) with no agent
  network access to the server's port, or the broker running as a separate
  user (§5, open question 1).

Consequences:

- A server whose configuration lists `agent_tools` (G6) or binds arguments
  to select server-side state (G4), typically because it holds a human-owned
  world, **must use `auth = "bearer"`**. With `auth = "none"`, such a
  configuration is an error.
- `xshell-mcp servers` reports each server's enforcement as `enforced`
  (sandboxed, or bearer with the shell sandboxed off its port), `token`
  (bearer only), or `advisory` (`auth = "none"`), so nobody mistakes a
  loopback server for a confined one.

### 6.3 Amendments to the accepted design

**§3.1: session identity comes from a verified per-turn token.** §3.1 has
the CLI learn its session "from the environment `run_shell` provides".
`run_shell` sets no such variable today, and if it set a plain session
name or ID, the agent could run `XSHELL_SESSION=other xshell-mcp call …` and
reach another session's instance or world. Instead:

- When a turn starts, `xshelld` creates a random token for that turn and
  records it with the session ID, the turn ID, and the turn's tool deadline
  (G3). The engine puts the token, and only the token, in every `run_shell`
  command's environment (`XSHELL_MCP_TOKEN`).
- The CLI sends the token with each request. The broker resolves it with
  `xshelld` over the supervision channel and gets the session ID, turn ID,
  and deadline back. An unknown or expired token is refused with a new exit
  code, `unauthorized` (10), and a reason; the broker never takes a session
  from anything the CLI asserts.
- The token is revoked when the turn ends. A turn sees only its own token,
  so it cannot name another session.
- **Limit:** a process running as the same user can read another process's
  environment (`/proc/PID/environ` on Linux, debugging APIs on macOS), so a
  same-user agent could in principle take a concurrent turn's token. As with
  §3.3, closing this needs the shell sandbox or a separate broker user; the
  token still prevents the trivial forgery, and every resolution is audited
  (G6).

**§3.7: saved content is fetched through the CLI.** §3.7 saves binary
resources under the session's state directory, and the first draft of this
section did the same for large results. Agent read tools cannot open those
files: `read_file` refuses paths outside the working directory
(`OUTSIDE_ROOT` in `xshell-execution/src/prepared.rs`), and §3.3 puts broker
state in a `block` class. So:

- Saved content gets a result ID, printed with its size and type in place of
  the content.
- `xshell-mcp fetch RESULT_ID [--offset N] [--limit N] [--pointer /json/path]`
  prints a bounded, sanitized slice: a byte range, or the value at a JSON
  pointer for structured results.
- `xshell-mcp fetch RESULT_ID --save PATH` copies it into the workspace. That
  write is the agent's own `run_shell` command, so the approval policy
  governs it, and nothing is written into the workspace unless asked.
- Result IDs are scoped to the session (through the token) and expire with
  the server instance.

### 6.4 Gaps and proposed changes

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
  socket), and only for servers with no `agent_tools` and no bound
  arguments (§6.2). A non-loopback URL with `auth = "none"` is a
  configuration error.
- §3.4 sandboxing does not apply to a server xshell does not launch;
  `servers` reports its enforcement level (§6.2).

**G2. Hundreds of tools.**

- **Allowlist patterns.** `tools` accepts `*` globs on dotted names
  (`"assembly.*"`, `"viewer.render_*"`), and a `deny_tools` list applies after
  it. Patterns are expanded against the server's current list.
- **Pin only what is allowed.** Under §3.5 as written, any change to a
  server's tool list blocks every call, which on a stack under active
  development would happen constantly. Instead, the pin covers the expanded
  allowed set: the allowed tools' names and canonical definitions. A change
  to a tool outside it blocks nothing. A new tool that matches a pattern
  changes the allowed set, so it still needs `xshell mcp approve` before it
  can be called.
- **Filtered, paged listing.** `xshell-mcp list SERVER [--prefix P]
  [--grep WORD] [--page N]` keeps each listing well inside the 64 KiB tool
  output bound (at about 120 bytes a line, 520 one-line entries already
  approach it). `list` with no filter on a large server prints counts per
  prefix and a hint, not every tool.
- **Readable approvals.** `xshell mcp approve` on a large list shows counts
  of added, removed, and changed allowed tools and their names first, then
  the full diff on request.

**G3. Long calls and cancellation.** The design does not define timeouts or
cancellation, and `run_shell` ends a command with a group-wide `SIGKILL`
(`xshell-execution/src/process.rs`), so the CLI cannot clean up after itself.

- **The broker treats a closed client connection as cancellation.** When the
  CLI's socket closes before the result is written, the broker sends
  `notifications/cancelled` for that request to the server and records the
  call as *cancelled, outcome unknown* (G6 says where). A server may already
  have applied the effect; xshell reports that honestly rather than as "not
  run" (roadmap D2, M1.2).
- **The deadline comes from `xshelld`, not the CLI.** The CLI cannot know how
  much of the turn's `shell_timeout` remains. The broker gets the command's
  deadline when it resolves the token (§6.3) and ends the call at the
  deadline minus a margin (`cancel_margin`, default 5 s), so it can cancel
  the call and the CLI can print the reason before `run_shell`'s kill.
  `xshell-mcp call --timeout DURATION` and a per-server `call_timeout` can
  only shorten that. Long CAD calls use the `relaxed` limit profile (30 min)
  or a configured one.
- **Progress keeps nothing alive.** Progress notifications stay logged only
  (§3.6); they do not extend a deadline.
- **Concurrency.** Today one turn's tool calls run one after another, but a
  user or script can call the broker concurrently. The broker allows
  concurrent requests to one instance up to a per-server `max_in_flight`
  (default 4) and queues the rest; a queued call's deadline includes its
  wait.

**G4. Server-side state selected by arguments.** §3.1's isolation ("server
instances are per session") holds for servers the broker starts. A shared
external server cannot be given one instance per session, and mechatron
selects its world by an argument, not an instance. Add **bound arguments**:

```toml
[mcp.servers.mechatron.bind]
session = "xshell/{session_id}"   # {session_id} is the xshell session's ID
```

- **Bind to the session ID, not its name.** Names can be reused: closing a
  session and creating another with the same name must not inherit the old
  world. Templates may use `{session_name}` for readability only alongside
  `{session_id}` (`"xshell/{session_name}-{session_id}"`); a template with
  the name alone is a configuration error. The ID comes from the verified
  token (§6.3), never from the CLI.
- The broker sets each bound argument on every call and refuses (exit 2) a
  call that supplies a different value. The agent cannot name another world,
  including the human's `default` one, by mistake or by injection.
- **Tools without the bound argument are refused.** A stateful tool with no
  `session` parameter probably acts on the server's default world, which is
  the human's live session. On a server with bound arguments, the broker
  refuses any tool whose schema lacks every bound argument unless the
  configuration lists it in `unbound_tools` (for example, stateless catalog
  lookups).
- The broker also keeps one `Mcp-Session-Id` per xshell session per server,
  so a server that isolates by MCP session gets the same per-session
  separation.
- Where neither applies, `servers` marks the server `state: shared`, so the
  user knows sessions can see each other's effects through it.
- Bound arguments require `auth = "bearer"` (§6.2); otherwise the agent can
  reach the default world directly.

**G5. Large results.** A turn report or graph dump can exceed the 64 KiB
bound. Rather than truncate structured output mid-object, the broker saves
an over-limit sanitized result and prints its result ID, size, and the first
part of the result; the agent reads the rest with `xshell-mcp fetch`
(§6.3).

**G6. Calls that run another agent, and where broker records go.**
`harness.turn` hands the request to mechatron's own model loop, with its own
model, credentials, and spend, so its work is not under xshell's approval
policy, turn budget, or audit (roadmap M7: a connector must not bypass
shared authorization because it supplies its own agent loop).

- Such tools are **denied by default**: a server's configuration lists them
  under `agent_tools`, and a pattern allowlist (G2) never admits them; only
  an explicit entry in `tools` does. A server with `agent_tools` requires
  `auth = "bearer"` (§6.2).
- **The broker is an audit client.** The broker is a separate process, and
  xshell's audit today sees only the `run_shell` call that ran the CLI. The
  broker therefore opens its own connection to `xshell-auditd` and records,
  for every call, the server, tool, argument hash, outcome (including
  *cancelled, outcome unknown*, G3), and the session and turn IDs from the
  token, so records join the turn's own. Calls to `agent_tools` are recorded
  as delegated to an external agent. A marker printed by the CLI on stdout
  is not used: any command can print one.
- **Headless results.** The broker reports each external-agent call to
  `xshelld` over the channel it uses to resolve tokens, and `xshelld` adds
  them to the turn's headless `result` record (`external_agent_calls`). The
  documentation states that their model spend and internal tool calls are
  outside xshell's budget and audit.
- For benchmarking, this is also a measurement concern: an agent that hands
  the design to mechatron's harness is measuring that harness, not itself.

**G7. Structured arguments.** Mechatron tools take nested JSON arguments, and
`--json-file` (§3.2) avoids quoting for them. This is the case most likely
to produce the quoting errors that would justify `McpCall` (§3.8). The
broker counts argument-parse failures (exit 2) per server in its audit
records, so the §5 decision to start with the CLI can be revisited with
evidence.

### 6.5 Conformance tests

Against a fixture server in the repository (no mechatron dependency): a
loopback Streamable HTTP server that generates about 500 tools with dotted
names, takes a `session` argument on most tools, has stateful tools without
it, one tool that sleeps until cancelled, one that returns a 1 MiB
structured result, and one listed as an agent tool.

| Test | Pass condition |
|---|---|
| Loopback `auth = "none"` | Connects and reports `advisory`; the same entry with a non-loopback URL, bound arguments, or `agent_tools` is a configuration error |
| Bearer secret | Token reaches the server; it is absent from every file and environment the agent can read; a direct request without it is refused by the fixture |
| Forged or stale token | A made-up token, another turn's revoked token, or none at all is refused; no call reaches the server |
| Pattern allowlist and pinning | `assembly.*` admits only matching tools; a new matching tool is refused with `pin_mismatch` until approved; a change to a non-allowed tool blocks nothing |
| Filtered listing | Every `list` output, including the unfiltered summary, stays under the tool output bound |
| `SIGKILL` mid-call | The server receives `notifications/cancelled`; audit records the call as cancelled with unknown outcome |
| Deadline | A call running past the token's deadline minus the margin is cancelled by the broker, and the CLI prints the reason before `run_shell`'s timeout |
| Bound argument | Set when omitted; a different supplied value is refused; a new session reusing a closed session's name gets a different world |
| Unbound stateful tool | Refused unless listed in `unbound_tools` |
| Large result | Output is bounded and names a result ID; `fetch` with a range, a JSON pointer, and `--save` returns the right content |
| Agent tool | Refused under a pattern allowlist; when listed explicitly, audit records it as external-agent and the headless `result` lists it |

### 6.6 Estimate

| # | Task | LoE (days) | Lands with |
|---|---|---|---|
| L1 | `auth = "bearer" / "none"`, loopback and enforcement-level checks, `set-secret` | 1 | M2 |
| L2 | Pattern allowlists, `deny_tools`, pinning the allowed set, filtered and paged `list`, approval summary | 2 | M1, M4 |
| L3 | Disconnect-as-cancel, broker-side deadlines with margin, `max_in_flight` | 2–3 | M1 |
| L4 | Per-turn tokens: issue and revoke in `xshelld`, engine environment, broker resolution (§6.3) | 2–3 | M1 |
| L5 | Bound arguments on session ID, `unbound_tools`, per-session `Mcp-Session-Id` | 1–1.5 | M1 |
| L6 | Result IDs and `xshell-mcp fetch` for large results and binary resources (§6.3) | 1–1.5 | M1 |
| L7 | Broker as audit client; `agent_tools` default-deny; `external_agent_calls` in headless results | 1.5–2 | M4 |
| L8 | Fixture server and the §6.5 tests | 2–2.5 | M1–M4 |
| | **Total** | **12.5–16.5** | |

L3, L4 and L6 are not mechatron-specific: they fix the accepted design for
any slow server, any session, and any binary resource, so they belong in M1
regardless of this case. With §6, the bridge totals about 30–42
engineer-days.

### 6.7 Open questions

1. **Bound arguments versus server-side isolation.** Bound arguments make a
   server's world selection enforceable without the server's help. Should
   the bridge prefer them, or ask servers to isolate by `Mcp-Session-Id` and
   treat argument binding as a stopgap?
2. **Where `agent_tools` is declared.** Configuration only (as above), or
   also inferred from a server's documentation? Inference would be a server
   hint, which §3.5 treats as untrusted.
3. **Token scope.** One token per turn (as above), or one per `run_shell`
   command, which would narrow what a token read from another process's
   environment allows, at the cost of a `xshelld` round trip per command?
4. **Image results from renders.** Mechatron's viewer returns renders; those
   need A1–A4 and the §3.7 image path, which this section assumes rather than
   changes.
