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
ack_timeout_ms = 5000          # deadline for each exchange with the service
```

`xshell` uses `enabled`, `required`, `socket`, and `ack_timeout_ms`. `xshelld`
uses those plus `terminal_stream` and `terminal_stream_max_bytes`. The audit daemon uses
`socket`, `directory`, and `checkpoint_interval`. Command-line flags override
the daemon paths.

When `required = true`, xshell refuses to start if the daemon is unavailable.
It also refuses to execute an input that the daemon has not acknowledged.
Events are flushed and synchronized before the daemon acknowledges them.

Every exchange with the audit service, from sending a record to receiving its
acknowledgement, must finish within `ack_timeout_ms`. An audit service that
stops responding therefore cannot freeze xshell:

- `xshelld` keeps one audit connection per xshell session, so a withheld
  acknowledgement delays only that session, and only until the deadline.
  Other sessions, session control, and shutdown carry on.
- A missed deadline counts as a failed record: the record may or may not have
  reached the log. With `required = true` the action it covers is refused and
  later records for that session fail at once. After one deadline interval the
  next record tries a new connection; when it succeeds the log shows a
  `logical_session_attached` record with action `xshelld_execution_resumed`
  that starts a new audit session for the same xshell session.
- With `required = false` records are dropped while the service is
  unresponsive, with a warning, and recording resumes the same way.
- Shutdown closes every audit session in parallel, so a hung service delays it
  by about one deadline per close rather than hanging it.

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
  changes, direct shell completion, interactive-process (PTY) start and
  completion, and history compaction (which turns the model could no longer see).
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
session closure. A `delegate_task` subagent's activity is recorded between
`subagent_started` (its name, task, and tools) and `subagent_finished` (its
outcome); turns run sequentially, so every record in between belongs to the
subagent, and its tool-call ids carry its scope (`delegate-1/...`). View operations record the requested or resolved path,
content hash and size when acquisition succeeded, media type, selected viewer,
and outcome; file contents are not duplicated into the audit log.

### Turn authority and decision basis

Every agent turn starts with a `turn_started` record, written before any of
its tool calls can run. It holds the turn ID, the effective approval mode
and limits (plus what the client asked for, when the session host lowered
it), and both policies: the session host's base policy and the client's
overlay, each as `{"sha256": ..., "policy": ...}`. The hash is SHA-256 over
the policy's JSON encoding, so the recorded content can be checked against
it, and two turns under the same policy share an identity.

Each `tool_decision` record says what decided it in `basis`:

| `kind` | Meaning | Other fields |
|---|---|---|
| `entry` | A policy entry matched; the mode still applies (an allowed call is denied in `off` and prompted in `ask`) | `policy` (`base` or `overlay`), `list` (`denied`, `always_allowed`, `allowed`), `entry` as written |
| `not_listed` | The policy has an `allowed` list and the call matched none of it | `policy` |
| `uncheckable_command` | A shell denial exists and this command could not be checked against it, so it was not run unattended | `policy` |
| `mode_default` | No entry matched: the mode's behaviour without a policy | |
| `not_in_toolset` | The tool is not one this agent may use | |
| `turn_limit` | The turn's limits refused the call before the policy was consulted | |
| `blocked_path` | The call would read a path in the session host's `block` sensitivity class; refused in every mode, before the policy was consulted | |

When the base and overlay disagree, the stricter decision wins and its basis
is recorded; on a tie, the base's. A prompted call also has `approval`, saying
who answered: `interactive`, `approval_channel`, `no_approver` (with a
`detail` such as `no_approval_channel` or `approval_channel_closed`),
`cancelled`, or `unreported`. For a remote client this is the client's own
report: the daemon cannot verify who sat at the terminal, but it keeps only
the known approvers and short reason codes, and refuses reports that
contradict themselves. Only the daemon records `cancelled`, and
`no_approver` only comes with a denial.

Every agent turn ends with a `turn_finished` record: the turn ID, its
`outcome` (`completed`, `failed`, or `cancelled`), a `reason` for a failure
the client can act on (`budget_exhausted`, `audit_unavailable`), and `usage`
against each turn limit (see [headless runs](headless.md#usage)). It is the
turn's last record. A headless run's `result` line names its audit session,
sequence, and hash, so a supervisor can check the run's own account against
the signed log. If the record cannot be written, the turn's outcome stands
and the run reports no audit reference.

Not yet recorded: an approval bound to the prepared operation it approved
(the object a read opened, as M1.3 does for reads), and the identity of the
sensitivity classes in effect.

Audit logs contain prompts, source text, commands, and tool output. They should
be treated as sensitive. At-rest encryption and retention policy are planned
but are not in the initial format.

Direct `$` command input is logged by `xshelld` before execution, and the daemon
records the exit outcome even when the controller has detached or switched
away.

### Terminal-stream capture (opt-in)

By default the audit trail records an interactive process's **lifecycle** — the
command before it starts and its exit outcome — but not the bytes exchanged
with it. The trail exists to hold agents accountable for what they did;
interactive processes are human-driven, and recording every keystroke and
screen update of an interactive session is a decision an operator should make
deliberately, not inherit from turning on agent auditing.

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
policy unchanged. Stream capture is necessarily best-effort after a process
has started: `required = true` prevents startup if the initial lifecycle event
cannot be recorded, but it cannot undo bytes already accepted by a running
process. A later stream-append failure leaves the audit session without a final
checkpoint, making the incomplete capture evident during verification.

Captured streams are as sensitive as anything typed at a terminal — passwords
entered at prompts that disable echo are recorded on the input side. Treat
the audit directory accordingly.

An unclean client or daemon crash leaves a verifiable hash chain but no final
checkpoint. The verifier reports this explicitly rather than claiming the log
is complete.

History-compaction events advanced the audit client/daemon protocol to version
3 and the on-disk audit format to version 2; terminal-stream events advance
them to protocol 4 and format 3; auto-continue events and the `reasoning` and
`finish_reason` fields on agent responses advance them to protocol 5 and format
4; `subagent_started` and `subagent_finished` events advance them to protocol 6
and format 5; the `images` field on `input` and `tool_result` events advances
them to protocol 7 and format 6. Image records hold the hash, media type,
dimensions, byte length, and label, never the image bytes. `turn_started`
and the `basis` and `approval` fields on `tool_decision` advance them to
protocol 8 and format 7. Upgrade and restart `xshell-auditd` together
with `xshelld` and the CLI — a version mismatch is reported at handshake with
the expected and received versions. The verifier remains able to read every
format from version 1 onward.
