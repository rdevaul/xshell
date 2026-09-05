# xshell audit service

The audit subsystem separates event collection from the interactive shell. An
xshell process sends events over a session-scoped Unix socket to
`xshell-auditd`; the daemon assigns sequence numbers and timestamps, writes the
records durably, and produces signed checkpoints.

## Security boundary

Running `xshell-auditd` as the same account as xshell is useful for development
and functional testing, but it is **not tamper-resistant**. A command with that
account's permissions can modify or delete the daemon's files.

A protected deployment requires:

- a dedicated `xshell-audit` service account;
- an audit directory owned and writable only by that account;
- a Unix socket whose group permits xshell users to connect;
- users added to that group;
- the daemon started by launchd or systemd rather than from xshell;
- a trusted copy of `signing-key.pub` stored outside the audit directory.

The client connection is marked close-on-exec. Shell commands and agent tools
therefore do not inherit the capability for the current audit session. Another
process with socket access may open a distinct session, but it cannot append to
an existing session by guessing its UUID.

Compromise of the audit daemon, its signing account, root, or the kernel is
outside the local protection boundary. Detecting rollback by a privileged
administrator requires an independent witness.

## Configuration

xshell and the daemon can read the same TOML structure:

```toml
[audit]
enabled = true
required = true
socket = "/run/xshell-audit/audit.sock"
directory = "/var/lib/xshell-audit"
checkpoint_interval = 16
terminal_stream = false        # opt-in byte-for-byte PTY capture (xshelld)
terminal_stream_max_bytes = 16777216
```

`xshell` uses `enabled`, `required`, and `socket`. `xshelld` uses those plus
`terminal_stream` and `terminal_stream_max_bytes`. The audit daemon uses
`socket`, `directory`, and `checkpoint_interval`. Command-line flags override
the daemon paths.

When `required = true`, xshell refuses to start if the daemon is unavailable.
It also refuses to execute an input that the daemon has not acknowledged.
Events are flushed and synchronized before the daemon acknowledges them.

## Who records what

Audit events are recorded by the process that performs the action, so the
record cannot be skipped by disconnecting a client or lost to a bounded replay
journal.

- **Standalone CLI** (session fabric disabled): the CLI executes everything and
  records everything.
- **Session fabric enabled**: `xshelld` reads the same `[audit]` section,
  opens one audit session per xshell session on first use, and records the
  execution-boundary events itself — agent and `$` input, model responses and
  errors, tool requests, approval decisions, tool results, working-directory
  changes, direct shell completion, terminal-job (PTY) start and completion,
  and history compaction (which turns the model could no longer see).
  A detached turn is audited exactly as an attached one. With `required = true`,
  `xshelld` refuses to start without a reachable audit service, refuses to accept
  a turn whose input cannot be recorded, and stops an in-flight turn at the next
  tool boundary if an append fails. The attached CLI records only what the
  daemon cannot see: its own startup configuration, `//` control commands,
  logical session attach and detach, model profile switches, and view
  operations. Daemon-recorded events are not duplicated by the CLI on replay.

The daemon's audit session for an xshell session is finalized with a signed
checkpoint when that session is closed with `//close`. A daemon that exits
without closing its sessions leaves verifiable chains without final
checkpoints, which the verifier reports.

## Log format

Each session is stored as `sessions/<session-id>.jsonl`. Record bodies contain:

- format and session identifiers;
- a daemon-assigned sequence number and timestamp;
- the peer UID observed by the daemon;
- the previous record hash;
- a typed audit event.

The record hash is SHA-256 over a domain separator and the serialized record
body. Checkpoints contain the current sequence and chain head and are signed
with Ed25519. Checkpoints have their own sequence and previous-checkpoint hash,
so copied, removed, or reordered checkpoint entries are detectable as well. A
final checkpoint distinguishes a cleanly closed log from a log whose tail may
have been removed.

Every checkpoint also contains a blinded, domain-separated SHA-256 witness
commitment. A future peer, RFC 3161, transparency-log, or OpenTimestamps backend
can publish that commitment without changing the on-disk format or revealing
the checkpoint itself.

The daemon writes the latest signed checkpoints to `checkpoints.jsonl` as a
local rollback reference. Strong rollback detection will require copying those
commitments or receipts to an independent system.

## Verification

Keep a trusted copy of the public key before relying on the logs:

```sh
cp /var/lib/xshell-audit/signing-key.pub /path/to/trusted/xshell-audit.pub
```

Verify a session with:

```sh
xshell-audit-verify \
  --public-key /path/to/trusted/xshell-audit.pub \
  /var/lib/xshell-audit/sessions/SESSION-ID.jsonl
```

Verification checks record hashes, sequence continuity, previous-hash links,
checkpoint state, witness commitments, signing-key identity, Ed25519
signatures, and the final-checkpoint marker.

## Captured events and current limitations

The initial implementation records session configuration (never API
credentials), user input, cwd and model changes, model responses, model errors,
tool calls, approval decisions, tool results, direct shell completion, and
session closure. View operations record the requested or resolved path,
content hash and size when acquisition succeeded, media type, selected viewer,
and outcome; file contents are not duplicated into the audit log.

Audit logs contain prompts, source text, commands, and tool output. They should
be treated as sensitive. At-rest encryption and retention policy are planned
but are not in the initial format.

Direct `$` command input is logged by `xshelld` before execution, and the daemon
records the exit outcome even when the controller has detached or switched
away.

### Terminal-stream capture (opt-in)

By default the audit trail records a terminal job's **lifecycle** — the command
before it starts and its exit outcome — but not the bytes exchanged with it.
The trail exists to hold agents accountable for what they did; terminal jobs
are human-driven, and recording every keystroke and screen update of an
interactive session is a decision an operator should make deliberately, not
inherit from turning on agent auditing.

Set `terminal_stream = true` in `[audit]` to also capture the byte stream.
`xshelld` then records `terminal_stream` events from the same buffer that
feeds terminal replay: `direction` is `input` (bytes the job actually accepted
from the operator) or `output` (bytes the job wrote); `offset` is the position
within that direction's stream for the job; `data` is standard base64 of the
raw bytes, escape sequences included. Capture is bounded per job by
`terminal_stream_max_bytes` (default 16 MiB, `0` for no bound). When the budget
is exhausted, capture stops, offsets keep advancing so the recorded prefix
stays faithful, and a final record with `direction = "summary"` states how
many bytes were not recorded before the job's completion is logged.

Whether capture was enabled is written into each audit session's first
record (`logical_session_attached.terminal_stream`), so a reader can tell
"nothing was typed" from "typing was not recorded". Because the stream is
recorded by `xshelld`, it is captured for detached jobs and cannot be
suppressed by a client. A stream-record failure stops stream capture for that
job with one warning; lifecycle records continue to follow the `required`
policy unchanged.

Captured streams are as sensitive as anything typed at a terminal — passwords
entered at prompts that disable echo are recorded on the input side. Treat
the audit directory accordingly.

An unclean client or daemon crash leaves a verifiable hash chain but no final
checkpoint. The verifier reports this explicitly rather than claiming the log
is complete.

History-compaction events advanced the audit client/daemon protocol to version
3 and the on-disk audit format to version 2; terminal-stream events advance
them to protocol 4 and format 3. Upgrade and restart `xshell-auditd` together
with `xshelld` and the CLI — a version mismatch is reported at handshake with
the expected and received versions. The verifier remains able to read every
format from version 1 onward.
