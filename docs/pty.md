# Transient PTY execution

Directly entered `$` commands use the `xshell-pty` crate when stdin and stdout
are terminals. The controller owns PTYs for local sessions; `xshelld` owns PTYs
for remote sessions. Agent-requested shell tools remain in the bounded,
noninteractive execution path.

`xshell-pty` creates a new pseudoterminal, starts the configured login shell in
a new session with the PTY slave as its controlling terminal, and switches the
controller terminal to raw mode for the lifetime of the command. The relay:

- forwards input and output as bytes without UTF-8 conversion;
- applies controller window-size changes to the PTY;
- lets the slave line discipline deliver Ctrl-C and other terminal-generated
  signals to the foreground command;
- restores the original controller terminal settings on success or failure;
- kills the PTY process group and reaps its leader if setup, relaying, or the
  controlling connection fails.

If stdin or stdout is redirected, xshell retains the inherited-stdio fallback.
The built-in `cd` behavior remains in the CLI so working-directory changes can
be synchronized back to the session service.

## Remote transport

Session protocol v6 adds `pty_start`, `pty_exchange`, and `pty_close`. The
active, authenticated session connection starts a PTY against that session's
cwd, then exchanges bounded binary input and output, terminal dimensions, and
exit status through the existing SSH stdio proxy. No inbound port or second
remote service is exposed.

The daemon enforces one PTY per session, 64 active PTYs globally, 64 KiB command
and input limits, 256 KiB output chunks, dimensions from 1 through 1,000, and a
maximum 250 ms exchange wait. Terminal type strings are strictly validated.
The SSH proxy permits PTY creation only for `fabric` sessions, and a PTY ID can
be operated only by the daemon connection that created it.

Remote exchanges currently use the versioned JSON request/response channel.
This is binary-safe but intentionally bounded rather than a high-throughput raw
byte tunnel. PTY IDs and accepted-input counts provide the lifecycle boundary
needed for later replay and reattachment.

## Current persistence boundary

Local and remote PTYs are transient. A local controller failure or remote
session-transport disconnect terminates and reaps the command. Disconnect
survival, output replay, reattachment, input arbitration, and job-control
suspension/resumption are the next milestone. PTY activity appears as
`running` in the session catalog, but transient PTY bytes are not added to the
durable turn journal.

PTY output is trusted terminal output. It may contain cursor movement,
alternate-screen, hyperlink, clipboard, or other terminal escape sequences;
xshell cannot sanitize those sequences without breaking interactive programs.
Run interactive commands only on hosts and in directories whose programs and
data you trust. Audit logging records the command and terminal outcome but does
not currently duplicate the mixed PTY byte stream.

The initial acceptance target is:

```text
$cat file.json | jq | less
```

Persistent full-screen applications such as `emacs -nw` remain a later
acceptance target because they require reattachment, redraw, richer terminal
capability negotiation, and an explicit escape-sequence policy.
