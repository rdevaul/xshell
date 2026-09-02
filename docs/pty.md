# Persistent terminal jobs

Directly entered `$` commands use daemon-owned PTYs whenever the session fabric
is enabled and stdin/stdout are terminals. Local and remote sessions therefore
share the same lifecycle and switching behavior. Without the fabric, the
controller retains its direct local PTY fallback. Agent-requested shell tools
remain in the bounded, noninteractive execution path.

`xshell-pty` creates a new pseudoterminal, starts the configured login shell in
a new session with the PTY slave as its controlling terminal, and switches the
controller terminal to raw mode while attached. The relay:

- forwards input and output as bytes without UTF-8 conversion;
- applies controller window-size changes to the PTY;
- lets the slave line discipline deliver Ctrl-C and other terminal-generated
  signals to the foreground command;
- restores the original controller terminal settings on success or failure;
- kills and reaps the process group when its terminal job is explicitly
  terminated or its ephemeral session is detached.

If stdin or stdout is redirected, xshell retains the inherited-stdio fallback.
The built-in `cd` behavior remains in the CLI so working-directory changes can
be synchronized back to the session service.

## Daemon ownership and replay

Each session has at most one terminal job. A background worker owns its PTY,
continuously drains output even while no controller is attached, and retains a
1 MiB byte ring with absolute offsets. An attachment resumes from the last
offset remembered by that CLI; a new controller starts at the oldest retained
offset. If output has wrapped, replay begins at the current ring boundary.

Protocol v8 starts a job on the active authenticated control connection and
returns its ID plus a one-time attachment ticket. Local clients claim the ticket
through the daemon Unix socket. Remote clients open a second `ssh -T` process
running `xshelld serve-pty-stdio` and submit the ticket on stdin, never in
process arguments. No inbound port or additional network service is exposed.

The daemon enforces one job per session, 64 jobs globally, 64 KiB command and
input limits, dimensions from 1 through 1,000, and bounded output frames.
Terminal type strings are strictly validated. The SSH control proxy permits
job creation, discovery, and attachment only for `fabric` sessions. Tickets
are single-use bounded secrets, and only one stream may claim a job at a time.

The binary channel uses a one-byte directional tag, a four-byte big-endian
payload length, and a bounded payload. Input is limited to 64 KiB per frame and
output to slightly less than 256 KiB after its absolute offset. Stream close is
an acknowledged detach operation; explicit `pty_close` terminates the job.

## Escape router and switching

The local relay consumes a configurable prefix before ordinary input reaches
the focused PTY. The default is `Ctrl-]`, configured as `pty_escape = "ctrl-]"`
under `[session_fabric]`.

| Sequence | Action |
|---|---|
| `Ctrl-] d` | Detach to the xshell REPL |
| `Ctrl-] s` | Select a session target interactively |
| `Ctrl-] l` | Return to the previously focused session |
| `Ctrl-] n` / `Ctrl-] p` | Cycle to the next/previous session |
| `Ctrl-] q` | Terminate the focused job |
| `Ctrl-] ?` | Show key help |
| `Ctrl-] Ctrl-]` | Send a literal prefix byte |

The keystroke is a local data-plane escape; listing, switching sessions,
minting a fresh ticket, and claiming the selected stream remain authenticated
control-plane operations. Every visible session is a switch target. A session
with a job is marked `[terminal]`; one without a job is marked `[REPL]`, and
selecting it leaves raw mode, activates that session, and restores the xshell
prompt. This means an idle default session remains reachable from a full-screen
program in another session without creating a dummy PTY. At the REPL,
`//terminal` reattaches the current session's job, while `//terminal list` and
`//terminal kill` inspect and terminate jobs.

## Persistence and display boundary

Daemon-lifetime and durable sessions retain terminal jobs across controller and
stream disconnections, but terminal jobs do not yet survive an `xshelld`
restart. Ephemeral sessions terminate jobs on detach. PTY activity appears as
`running` in the session catalog, but terminal bytes are not added to the
durable conversation or audit journal.

PTY output is trusted terminal output. It may contain cursor movement,
alternate-screen, hyperlink, clipboard, or other terminal escape sequences;
xshell cannot sanitize those sequences without breaking interactive programs.
Run interactive commands only on hosts and in directories whose programs and
data you trust.

On attachment xshell reapplies the controller dimensions, which prompts most
curses applications and Emacs to redraw. xshell does not yet emulate VT screen
state, so replay after ring truncation cannot guarantee a perfect full-screen
restoration. A VT snapshot layer or optional tmux-backed provider remains a
future enhancement.
