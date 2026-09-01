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
with protocol version 3. The daemon
returns a connection-scoped client UUID and its stable host ID, host alias, and
OS user. Requests and responses are bounded at 64 MiB.

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
`0700` and `0600`. No API key values cross the session protocol. A model
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

The client starts `ssh -T -- DEST xshelld serve-stdio`. OpenSSH retains control
of destination parsing, host-key verification, `~/.ssh/config`, agent use, and
authentication. No agent forwarding is enabled by xshell. The remote helper
automatically reads `$XSHELL_CONFIG` or `~/.config/xshell/config.toml` when
present, resolves the daemon socket, and proxies protocol requests to that
socket. Stdout contains protocol frames only; diagnostics use stderr.

The proxy is deliberately stateless. Killing the SSH process closes its daemon
client connection, applying ordinary detach semantics while daemon-owned work
continues. The CLI keeps one connection per discovered host, aggregates their
catalogs, and accepts `HOST:SESSION` selectors in the same `//switch` path used
locally. New remote sessions use the remote user's home directory; local
filesystem resolution is never applied to a remote cwd.

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
