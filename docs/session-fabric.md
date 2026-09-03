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
with protocol version 9. The daemon
returns a connection-scoped client UUID and its stable host ID, host alias, and
OS user. Requests and responses are bounded at 64 MiB.
Protocol versions are exact rather than negotiated across incompatible
schemas. Any protocol bump therefore requires upgrading and restarting
`xshelld` on the controller and every connected remote host before the new CLI
can attach. Protocol v9 adds the `tool_skipped` execution event, emitted for
each tool call that was never evaluated because the user aborted the turn at an
earlier call in the same response.

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

## SSH transport

The client starts `ssh -T -- DEST xshelld serve-stdio` for control traffic.
OpenSSH retains control of destination parsing, host-key verification,
`~/.ssh/config`, agent use, and authentication. No agent forwarding is enabled
by xshell. The remote helper
automatically reads `$XSHELL_CONFIG` or `~/.config/xshell/config.toml` when
present, resolves the daemon socket, and proxies protocol requests to that
socket. Stdout contains protocol frames only; diagnostics use stderr.

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
- `pty_start`: start one session-owned terminal job and return its ID and a
  one-time stream ticket.
- `pty_list`: list terminal jobs visible through the current transport.
- `pty_attach`: mint a one-time ticket at a bounded replay offset.
- `pty_claim`: consume a ticket on a dedicated daemon connection before binary
  framing begins.
- `pty_close`: terminate and reap the current session's terminal job.

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
