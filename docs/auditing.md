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
```

`xshell` uses `enabled`, `required`, and `socket`. The daemon uses `socket`,
`directory`, and `checkpoint_interval`. Command-line flags override the daemon
paths.

When `required = true`, xshell refuses to start if the daemon is unavailable.
It also refuses to execute an input that the daemon has not acknowledged.
Events are flushed and synchronized before the daemon acknowledges them.

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

Direct `$` commands currently inherit the terminal's stdout and stderr. Their
input and exit outcome are logged, but their byte-for-byte terminal output is
not yet captured. Complete capture without breaking interactive terminal
programs requires the planned PTY execution layer.

An unclean client or daemon crash leaves a verifiable hash chain but no final
checkpoint. The verifier reports this explicitly rather than claiming the log
is complete.

View-operation events advance the audit client/daemon protocol to version 2.
Upgrade and restart `xshell-auditd` together with the CLI; the on-disk audit
format remains version 1.
