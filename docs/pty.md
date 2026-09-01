# Transient PTY execution

Directly entered `$` commands on the controller or an active local session use
the `xshell-pty` crate when stdin and stdout are terminals. Agent-requested
shell tools remain in the bounded, noninteractive execution path.

`xshell-pty` creates a new pseudoterminal, starts the configured login shell in
a new session with the PTY slave as its controlling terminal, and switches the
controller terminal to raw mode for the lifetime of the command. The relay:

- forwards input and output as bytes without UTF-8 conversion;
- applies controller window-size changes to the PTY;
- lets the slave line discipline deliver Ctrl-C and other terminal-generated
  signals to the foreground command;
- restores the original controller terminal settings on success or failure;
- kills and reaps the child if setup or relaying fails.

If stdin or stdout is redirected, xshell retains the inherited-stdio fallback.
The built-in `cd` behavior remains in the CLI so working-directory changes can
be synchronized back to the session service.

## Current boundary

This first increment is intentionally transient and local. While a PTY command
is active, the controller owns its process and the session daemon does not
journal or preserve it. Disconnect survival, reattachment, input arbitration,
job-control suspension/resumption, and remote PTY transport require a
daemon-owned PTY protocol and are the next milestone. Remote `$` commands
therefore continue to use the existing bounded daemon output stream.

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
