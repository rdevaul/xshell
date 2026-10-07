# xshell session fabric

This document describes the local execution increment of the session fabric. The
wire types in `xshell-session` are authoritative; incompatible changes require
a protocol-version increment.

## Current boundary

`xshelld` is a per-host, per-OS-user execution and state service. It owns
session identity, attachment arbitration, model adapters, agent/tool loops,
approval rendezvous, shell execution, conversation snapshots, and durable
serialization. The attached `xshell` CLI is a controller and renderer. The
standalone CLI retains an in-process execution path when the fabric is
disabled.

The client and daemon exchange newline-delimited JSON over a Unix-domain
socket or an authenticated SSH stdio proxy. The first request must be `open`
with protocol version 11. The daemon
returns a connection-scoped client UUID and its stable host ID, host alias, and
OS user. Requests and responses are bounded at 64 MiB.
Set `session_fabric.host_alias` to a short, distinct name for each machine; if
it is omitted, `xshelld` advertises the operating-system hostname. The
`--host-alias` daemon argument overrides the configured value. If two connected
daemons nevertheless advertise the same alias, the controller appends the
shortest distinguishing host-ID prefix (for example `Mac.lan#12ab34cd`) in
session listings, completion, selectors, and the interactive picker.
Protocol versions are exact rather than negotiated across incompatible
schemas. Any protocol bump therefore requires upgrading and restarting
`xshelld` on the controller and every connected remote host before the new CLI
can attach. Protocol v11 makes session activity self-describing: the session
catalog distinguishes idle prompts, agent turns, and interactive processes,
including the process command and stream attachment state. Protocol v10 added
the `history_compacted` execution event. Protocol
v9 added the `tool_skipped` event, emitted for each tool call that was never
evaluated because the user aborted the turn at an earlier call in the same
response.

## Approval policy ceiling

The daemon executes agent tools, so the daemon owns the upper bound on how
much unattended execution it permits. `session_fabric.max_approval` (default
`ask`) is the most permissive policy applied to any turn. A client requesting
a more permissive policy is clamped and informed through the `turn_started`
event's `requested_approval` field; the CLI prints a one-line notice. This
matters once a controller on one host submits turns to `xshelld` on another:
the remote operator's configuration, not the controller's flag, decides
whether shell tools may run without a prompt there.

## Sensitive-path policy

`session_fabric.sensitive_paths` lists glob patterns for files an agent must
not read or list without a human decision, even though `read_file` and
`list_directory` are otherwise automatic. The daemon evaluates the policy
against the canonical path relative to the session cwd, so symlinks and `..`
cannot dodge it. A match is reported through `approval_requested` with
`reason = "sensitive_path"` (shell tools report `"shell_execution"`) and then
follows the turn's approval policy exactly like a shell tool. Omitting the key
selects the built-in defaults; an empty list disables the check.

## History compaction

Every provider request carries the full conversation, so an unbounded history
costs tokens on every step and eventually exceeds the model's context window.
`session_fabric.compaction` selects a compaction strategy. It runs before the
first provider request, after staging the new user prompt, and again after a
successfully completed turn. The pre-request pass lets a restored session or a
newly selected smaller-context model recover immediately. A failed or
cancelled turn restores the exact history from before that turn.

Strategies implement the `Compactor` trait in `xshell-execution` and operate
on whole turns — a `user` message through its final `assistant` reply,
including any tool calls and results — because OpenAI-compatible APIs reject a
`tool` result whose calling `assistant` message is missing. The leading
`system` message is always preserved, as is the most recent turn.

The built-in strategy is `max_history_bytes`: drop the oldest whole turns until
content plus tool-call arguments fit the budget. Bytes are a deliberate proxy
for tokens; exact counts are model-specific and not worth a tokenizer
dependency. Progressive summarization is a planned second implementation and
plugs in behind the same trait and config surface.

The budget is resolved per model. `session_fabric.compaction.max_history_bytes`
is the session-wide default; a model profile's own `max_history_bytes` takes
precedence because the appropriate depth follows that model's context window.
The value travels with the session's model binding, so `//model` switches and
remote daemons apply the budget of the model actually in use. A profile may
set `0` to disable compaction for that model.

Each compaction is reported to the client as a `history_compacted` execution
event and recorded in the audit log, so a reader of the trail can see from
which point the model no longer had the full transcript. A durable session
restored with a smaller budget than it was saved under is brought within
budget before its first provider request.

## Identity and attachment

- A session has a stable UUID. Its display name is unique within one daemon's
  `(host, user)` namespace.
- One client may hold the owner/controller attachment at a time.
- `operator` and `viewer` roles exist in the protocol but are rejected until
  explicit multi-user ACL sessions are implemented.
- Creating or switching sessions atomically acquires the destination before
  releasing the previous session, so a failed operation leaves the current
  attachment intact.
- Unexpected socket disconnect has detach semantics.

The daemon socket is mode `0600`; the state directory and host ID are mode
`0700` and `0600`. The daemon additionally refuses to start if the socket's
parent directory or the state directory is a symlink, is not owned by the
daemon's user, or is group/world writable, and it rejects any connection whose
peer UID (`SO_PEERCRED` / `getpeereid`) differs from its own. No API key values cross the session protocol. A model
binding stores only the name of the credential environment variable.

## Lifecycle modes

| Mode | On detach | On daemon restart |
|---|---|---|
| `ephemeral` | Deleted | Deleted |
| `daemon` | Retained | Deleted |
| `durable` | Retained | Restored from disk |

Durable state is written to `sessions.json` through a same-directory temporary
file, `fsync`, and atomic rename. Restored sessions are always detached.
The `xshelld serve-stdio` boundary exports only `fabric` descriptors and rejects
remote create, attach, switch, and named-close operations involving `host_only`
sessions.

## Per-user service management

`xshelld service` installs and supervises the daemon through the platform's own
per-user service manager, so a host keeps a session fabric across logins without
anyone leaving a terminal open.

| Command | Effect |
|---|---|
| `xshelld service install` | Write the unit, activate it, and report readiness |
| `xshelld service status` | Report the unit's state and whether the daemon answers |
| `xshelld service start` / `stop` | Load or unload an installed unit |
| `xshelld service restart` | Replace the daemon, draining its work first |
| `xshelld service uninstall` | Stop the service and remove the unit |

| Platform | Manager | Unit |
|---|---|---|
| macOS | `launchd` user agent | `~/Library/LaunchAgents/com.xshell.xshelld.plist` |
| Linux | `systemd --user` | `~/.config/systemd/user/xshelld.service` |

Everything is per-user. There is no system-wide unit, no root, and no inbound
network port, which is the same trust boundary the daemon's peer-credential
check already assumes. Units are written `0600`.

The unit runs the installing binary by absolute path and pins the configuration
file with `--config`. A launch agent or user unit does not inherit the shell
environment that normally supplies `XSHELL_CONFIG`, so an unpinned unit would
silently run the daemon against different settings than the user's own
invocations. Reinstall after moving the binary or changing which configuration
file is active.

`restart` asks the service manager to signal the running daemon rather than
start a second one, so the daemon performs its own graceful drain — the SIGTERM
path that stops admission, finishes agent, shell, and PTY work within a
deadline, and closes audit last. A restart still interrupts work the daemon
owns, including sessions belonging to other controllers attached to that host.

Both service managers return before they have finished acting: `launchctl
kickstart` reports success while the process is still being spawned through
`xpcproxy`, and `launchctl bootout` returns while the old service is still
listed. Each verb therefore waits for the state it asked for, bounded, rather
than reading status once.

A running process is not a reachable daemon — `xshelld` binds its socket after
launch — so `install`, `start`, `restart`, and a running `status` additionally
report whether the socket completes a handshake. Anything driving this remotely
needs that distinction before it tries to connect.

### Surviving logout

A per-user service is torn down with the user's session unless the platform is
told otherwise, which matters most for the case this exists to serve: a daemon
installed over SSH is useless if it dies with the installing connection.

- **Linux.** `systemd --user` stops the user manager when the last session ends.
  `install` reports this and names the fix: `loginctl enable-linger USER`.
- **macOS.** `launchd` user agents run only while the user has an active login
  session. A Mac with nobody logged in will not keep `xshelld` running, and
  there is no per-agent equivalent of lingering. `install` says so rather than
  implying a guarantee the platform does not offer.

## SSH transport

The client starts `ssh -T -- DEST xshelld serve-stdio` for control traffic.
OpenSSH retains control of destination parsing, host-key verification,
`~/.ssh/config`, agent use, and authentication. No agent forwarding is enabled
by xshell. The remote helper
automatically reads `$XSHELL_CONFIG` or `~/.config/xshell/config.toml` when
present, resolves the daemon socket, and proxies protocol requests to that
socket. Stdout contains protocol frames only; diagnostics use stderr.

`xshelld probe` is the read-only discovery boundary for bootstrap tooling. It
emits one versioned JSON object containing the installed binary version, the
protocol version supported by that binary, and one of these running-daemon
states:

- `ready`, with the daemon protocol version, stable host ID, host alias, and
  OS user;
- `incompatible`, when the daemon rejects the probing binary's protocol;
- `rejected`, for another structured handshake rejection; or
- `unavailable`, when the configured Unix socket cannot be reached or does not
  complete a valid handshake.

An unavailable daemon is a successful probe result rather than a command
failure: the executable was discovered and returned actionable state. The
probe never creates, attaches, lists, or changes a session, and it does not
start or replace the daemon. Local probe I/O has a two-second deadline, so the
`//connect` bootstrap flow can inspect state before asking for installation,
upgrade, or service-start approval.

The session client can now run this probe over SSH with a bounded 64 KiB
response and a ten-second end-to-end deadline. It turns discovery into one of
six explicit controller actions:

| Remote state | Required action |
|---|---|
| `xshelld` absent | install |
| installed binary uses another protocol | upgrade or downgrade |
| compatible binary, daemon unavailable | start |
| compatible binary, daemon incompatible | restart |
| compatible binary and daemon | connect |
| structured non-version rejection | stop and report |

The decision itself is read-only. `//connect` preflights a remote host, proceeds
when it is ready, and otherwise reports the exact repair needed. It falls back
to the legacy direct stdio connection when an older helper cannot produce a
valid probe, preserving compatibility with already working deployments.

### Authorized remote repair

Two of those actions — **start** and **restart** — place no new bytes on the
remote host. They run an already-installed, already-compatible binary's own
[`service`](#per-user-service-management) verb, using the SSH authority the
controller exercises on every connect. `//connect` can therefore offer to
perform them, while install and upgrade remain out of scope pending the
deployment authorization contract.

The flow is: probe, describe, ask, repair once, re-probe, then connect. The host
is re-probed rather than assumed fixed, because a service manager reporting
success is not the same fact as a daemon answering the protocol — the remote
`service` verb waits for a handshake, and the controller confirms it
independently.

Authorization is explicit and separate from the agent approval policy.
`--approval off` expresses trust in the model, not authority to restart another
host's daemon, so a repair always prompts. A non-interactive controller cannot
answer and is told what to run instead of having the repair performed for it.
Declining is an ordinary outcome: it reports the same diagnosis and manual fix
the user would get if repair were unavailable.

The prompt states the blast radius before asking. A restart ends work the remote
daemon owns, **including sessions held by other controllers attached to that
host**. Only one repair attempt is made; repeating a repair that did not take
would just wait out the same timeout again.

The remote command is a fixed argument vector (`xshelld service start` or
`xshelld service restart`) passed to `ssh` after `--`, with the destination as a
separate argument. Nothing from the probe response reaches it. The repair is
bounded at 64 KiB of output and a sixty-second deadline — longer than the
read-only probe because it waits for the service manager to settle and for the
daemon to accept a connection. Remote output is sanitized before display.

An `xshelld` older than the `service` subcommand cannot be repaired this way;
the failure says so and names the manual fix.

The trusted-artifact foundation is implemented separately from remote
mutation. `xshell-release` accepts a configured HTTPS manifest URL and pinned
Ed25519 public key, verifies the signed release and exact controller
version/protocol, selects one of the detected macOS or static-musl Linux
targets, downloads with strict bounds, and verifies artifact size and SHA-256.
The release workflow builds both supported architectures for both operating
systems and signs their common manifest. `//connect` does not yet acquire or
install those bytes; presenting the verified release and applying it atomically
remain the next increment.

The proxy is deliberately stateless. Killing the SSH process closes its daemon
client connection, applying ordinary detach semantics while daemon-owned work
continues. The CLI keeps one connection per discovered host, aggregates their
catalogs, and accepts `HOST:SESSION` selectors in the same `//switch` path used
locally. New remote sessions use the remote user's home directory; local
filesystem resolution is never applied to a remote cwd.

The selector alias `local:NAME` resolves only against the controller's local
Unix-socket connection, even when another host is active. Command and path
completion for remote sessions uses a separate, unattached protocol connection
that the CLI uses only for completion. Completion scans the daemon's inherited
`PATH` and the active session cwd, refreshing its command catalog at most once
every 30 seconds, with a one-second daemon deadline and strict input, scan,
candidate, and filename limits. Control-character names are excluded and
insertions are shell-escaped. It does not launch a shell, evaluate input,
expand variables, or source shell startup/completion scripts.

This increment requires a compatible `xshelld` to already be installed and its
daemon to be running. Signed bootstrap installation, richer compatibility
negotiation, reconnect backoff, and SSH connection multiplexing remain future
work.

## Protocol operations

- `list`: return descriptors visible to this local daemon client.
- `create`: create and attach a session with initial model, cwd, and history.
- `attach`: attach only when the connection is currently detached.
- `switch`: atomically move the connection's attachment.
- `update`: replace the attached session's model, cwd, and history snapshot.
- `detach`: release control and apply lifecycle policy.
- `close`: delete a detached session or the caller's attached session.
- `submit`: start one daemon-owned agent or shell turn for the attached session.
- `events`: long-poll sequenced turn events from a replay cursor.
- `approve`: answer a particular turn/tool approval rendezvous.
- `cancel`: cancel the active turn by stable turn ID.
- `snapshot`: obtain completed model, cwd, and conversation state.
- `complete_shell`: return bounded executable/path candidates for a session.
- `view_source`: return a bounded UTF-8 resource resolved against the attached
  session cwd, with media type, length, and SHA-256 metadata.
- `pty_start`: start one session-owned interactive process and return its internal PTY ID and a
  one-time stream ticket.
- `pty_attach`: mint a one-time ticket at a bounded replay offset.
- `pty_claim`: consume a ticket on a dedicated daemon connection before binary
  framing begins.
- `pty_close`: terminate and reap the current session's interactive process.

`view_source` requires the requesting connection to own the active session.
The SSH proxy additionally rejects requests for `host_only` sessions. The
daemon reads only regular files, enforces a 4 MiB limit and three-second
deadline, and returns text through the existing authenticated stdio tunnel.
Rendering remains local to the controller.

PTY creation requires the current attachment, and the SSH control proxy applies
the same `fabric` visibility check used for completion and viewing. The CLI then
starts `ssh -T -- DEST xshelld serve-pty-stdio`, submits the one-time ticket on
stdin, and switches to bounded binary frames. A ticket authorizes exactly one
stream claim. Stream closure detaches without terminating daemon-lifetime or
durable jobs; an explicit close, closing the owning session, or detaching an
ephemeral session terminates them. A 1 MiB per-job ring supports offset-based
replay. Protocol bounds and the terminal escape trust policy are documented in
[the PTY design](pty.md).

The CLI keeps a per-connection navigation history. After `//close` deletes the
current session, it first attempts to attach the previously visited session,
then the most recently active available session. It exits only when no session
remains. `//quit` instead detaches without deleting the current session.

One turn may run per session. Events are sequence-numbered and retained in a
bounded in-memory journal (8,192 events and 16 MiB per session). Disconnecting
detaches the controller but does not cancel daemon-lifetime or durable work.
On reattachment, the client replays missed events and then follows live output.
An approval-required turn remains paused until an attached controller answers
or cancellation occurs. Ephemeral sessions retain their existing delete-on-
detach behavior and therefore cancel in-flight work when detached.

Completed durable state is atomically checkpointed as before. The event
journal and active process are not yet restored across daemon restart. Future
context storage will use an append/checkpoint model rather than rewriting
large histories.

Execution credentials are resolved only in the daemon environment. Protocol
model bindings contain an environment-variable name, never its value.

## Reserved evolution

Further federation work will add signed bootstrap installation, connection
health/reconnect state, and discovery without exposing transcript contents.

Execution events are currently mirrored into the audit stream by an attached
client. Moving that append responsibility into `xshelld` is required before
unattended remote execution can claim complete audit coverage.

Multi-user sessions will be a distinct access mode with an explicit ACL.
Owner, operator, and viewer authorization will be enforced by the daemon, with
operator commands restricted by session policy and every action attributed to
an authenticated principal.

Context management will add status, explicit compaction, checkpoint, restore,
and fork operations. Summaries will retain provenance to source messages, and
all compaction/checkpoint actions will be auditable.
