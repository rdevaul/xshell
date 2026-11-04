# CLI integration tests: design

**Status:** Proposed
**Updated:** 4 November 2026

## 1. Problem

The `xshell` binary is the part of the system people use, and it is the
least tested. `xshell-cli` has about 6,300 lines and only unit tests. Every
test of the daemon (`crates/xshell-session/tests/service.rs`) talks to
`xshelld` through `SessionClient` and never runs the CLI.

The CLI's riskiest code is exactly what unit tests cannot reach:

- **The main input loop** (`main.rs`): reading a line, routing it, and
  synchronizing state with the daemon after every command.
- **Terminal handling:** raw mode, the escape prefix (`ctrl-]` by default),
  approval prompts, and the hand-off between rustyline and a running turn or
  interactive process.
- **Daemon state and local state:** the CLI keeps a copy of the session's
  model, working directory, and conversation, and must agree with the daemon
  about which one is current.

The bug fixed in #100 shows the cost. Detaching from a running turn with
`ctrl-] d` and then running `//sessions` ended the CLI. When the turn had
already finished, the same path silently overwrote the turn's conversation.
It needed a terminal, an escape sequence, a running daemon, and a turn in a
particular state. Its regression tests had to use a fake session service
inside the CLI's own module, because nothing could drive the real binary.

## 2. Goals

1. Run the real `xshell` binary on a real pseudo-terminal against a real
   `xshelld`, with a scripted model provider. Keystrokes, including the escape
   prefix and approval answers, go in; the rendered screen comes out.
2. Make the tests deterministic: no `sleep`-based synchronization, every wait
   bounded, and test-controlled timing for anything that must "still be
   running".
3. Assert on both sides: what the user sees, and what the daemon holds (its
   session list, activity, and conversation), read through `SessionClient`.
4. Run in the existing `cargo test --workspace --all-targets` gate on macOS,
   Linux x86_64, and Linux arm64, with no extra setup and no network.
5. Produce a useful failure: the final screen, the raw output, and the
   daemon's stderr.

## 3. Non-goals

- **Re-testing the daemon:** daemon behaviour stays covered by
  `xshell-session/tests`. These tests cover the CLI and its interaction with
  the daemon.
- **Pixel-exact rendering:** assertions match text on the screen model, not
  exact escape sequences, colours, or cursor positions, unless a test is
  specifically about them.
- **Real providers, real SSH servers, or the network.**
- **Replacing unit tests:** logic that can be tested in isolation stays tested
  that way.

## 4. Approach

### 4.1 Where the tests live

They go in `crates/xshell-cli/tests/`, one file per area (`sessions.rs`,
`turns.rs`, `escape.rs`, ...), with shared code in `tests/support/mod.rs`.

- **`xshell`** comes from `env!("CARGO_BIN_EXE_xshell")`.
- **`xshelld` and `xshell-auditd`** live in other packages, so Cargo provides
  no `CARGO_BIN_EXE_` variable for them. The harness looks for them next to
  `xshell` and, if they are missing, builds them with `cargo build -p ...`.
  `xshell-session/tests/service.rs` (`auditd_binary`) already does this for
  `xshell-auditd`.
- **Dev-dependencies:** `nix` (already a workspace dependency, for `openpty`)
  and `vt100` for the screen model.

A separate `xshell-e2e` crate is not needed yet. If the fake provider and
daemon helpers later serve both these tests and the daemon tests, move them
into a `publish = false` `xshell-testkit` crate.

### 4.2 Driving a terminal

The harness opens a pseudo-terminal with `nix::pty::openpty` at a fixed size
(100×30), and spawns `xshell` with the PTY's slave side as stdin, stdout and
stderr. A `pre_exec` hook calls `setsid()` and `TIOCSCTTY`, so the slave
becomes the CLI's controlling terminal; `xshell-pty::spawn_child` does the
same. To the CLI it is an ordinary interactive terminal:
`controller_is_terminal()` is true, rustyline uses its full editor, and the
escape prefix is read in raw mode.

A reader thread copies the master side's output into a `vt100::Parser` and a
raw byte log. Tests interact through a small API:

```rust
let mut cli = fabric.cli().profile("test").approval(Approval::Ask).spawn();
cli.expect_prompt();                         // waits for "› " at the cursor line
cli.line("run the job");                    // types the text and Enter
cli.expect("approve run_shell");            // waits until the screen contains it
cli.keys(b"\x1d");                          // ctrl-]
cli.keys(b"d");
cli.expect_prompt();
cli.line("//sessions");
cli.expect("agent running");
assert!(cli.is_running());                  // the #100 crash ended the process here
```

- **`expect(text)`** waits until the screen contains `text` or the deadline
  passes (default 10 seconds). On timeout it panics with the screen, the last
  4 KiB of raw output, and the daemon's stderr.
- **`expect_prompt()`** matches the prompt's fixed shape, so tests do not
  depend on the host alias or path.
- **`exit_status(within)`** waits for the process to exit, for the tests
  about leaving the CLI.

The screen model handles cursor movement, line editing, and redraws, so
assertions do not depend on how rustyline repaints.

### 4.3 Isolation

Each test gets a temporary directory, and its own:

- **`HOME`, `XDG_CONFIG_HOME` and `XDG_STATE_HOME`,** so nothing touches the
  developer's configuration, history, or sessions;
- **configuration:** an explicit `--config` file whose `[session_fabric]`
  section names a socket and state directory inside the test directory;
- **`xshelld` instance**, started with `--no-user-config`, `--config`,
  `--state-directory` and `--socket`, and killed when the test ends;
- **environment:** `env_clear()`, then only `PATH`, `TERM=xterm-256color`,
  `LANG=C.UTF-8`, `SHELL=/bin/sh`, and `NO_COLOR=1`, so a developer's shell
  profile and colours cannot change the output.

The CLI and the daemon each run in their own process group, which the
harness kills on drop, including after a panic.

### 4.4 A scripted provider

Tests use a fake OpenAI-compatible endpoint on `127.0.0.1:0`, configured as
a model profile. It generalizes the existing `serve_sse` helper in two ways:

- **Scripted turns:** a queue of responses, each text or tool calls, built
  with the existing `sse_text` and `sse_tool_call` shapes. Every request is
  recorded, so a test can assert what the CLI and daemon sent: for example,
  that a resumed session's history includes the previous turn.
- **Gates:** a response can be held until the test releases it, or released
  in parts, for example the first half of the stream, then the rest. This
  makes "the turn is still running" a state the test controls, not a race.
  Detaching from a running turn, cancellation, and a turn finishing while
  detached all need it.

```rust
let provider = FakeProvider::start();
let gate = provider.push_gated(text("job finished"));
// ... start the turn, detach, check state ...
gate.release();
fabric.wait_for_activity(&session, Activity::Idle);
```

### 4.5 Reading daemon state

`fabric.client()` returns a `SessionClient` on the test daemon's socket.
Tests use it to wait for daemon state instead of guessing timing
(`wait_for_activity`), and to assert on what the daemon holds after the CLI
acts. Asserting that the daemon's conversation still contains the turn after
the detach-and-sync sequence is the check that would have caught #100's data
loss.

### 4.6 Remote sessions without SSH

The CLI starts remote connections with `Command::new("ssh")`, found through
`PATH`. The harness can put a stand-in `ssh` script first on `PATH`. The
script drops the `-T`, `--` and destination arguments and runs the rest
locally against a second test daemon: for example, `xshelld serve-stdio`,
the remote probe, and the PTY proxy. This tests `//connect`, remote session
switching, and remote interactive processes without an SSH server. The
destination argument picks which test daemon answers, so one test can use
two "hosts".

### 4.7 Deadlines and flakiness

- No fixed sleeps. Every wait polls a condition (screen text, process exit,
  or daemon state) with a deadline.
- Default deadlines are generous (10 seconds): macOS CI runners are much
  slower than development machines. A test that measures time asserts wide
  bounds, as the F6 tests do.
- When a step must happen while something is in progress, a provider gate or
  a long-running command (`sleep 30`, killed at the end) holds it open.

## 5. Scenarios

Priority 1 covers bug classes that have happened, or that the code makes
likely. Priority 2 covers journeys every release should keep working.

### Priority 1

| Scenario | Checks |
|---|---|
| Detach from a running turn with `ctrl-] d`, then run `//sessions` (#100) | The CLI keeps running, the listing shows the turn running, and the daemon's conversation is untouched |
| A turn finishes while detached | The next prompt reports it, the CLI adopts the daemon's conversation, and the next sync does not overwrite it |
| `//resume` a detached turn | Output produced while detached is redrawn, and the turn's completion is followed |
| Approval prompt: `y`, `n`, `q` | Each decision reaches the daemon; `q` aborts the turn; a denied command never runs (marker file) |
| `ctrl-] q` during a turn | The turn is cancelled, and the prompt and daemon both return to idle |
| An interactive `$` command, then `ctrl-] d` and `//resume` | The terminal is restored after detaching, and resuming reattaches with recent output |
| CLI exit and restart | Sessions survive. Restarting reattaches the default session with its history, including a turn still running when the CLI exited |
| Daemon unavailable or restarted mid-session | The CLI reports it clearly instead of crashing, and recovers or exits cleanly as configured |

### Priority 2

| Scenario | Checks |
|---|---|
| `//new`, `//switch`, `//close`, and `ctrl-] n/p/l/s` | Switching carries each session's model, cwd, and history, and the prompt shows the right session |
| `$ cd` and `//model` | Local changes are synchronized to the daemon and shown in the prompt |
| A change during a running turn | Refused with a message, not applied, and the CLI keeps running |
| `//view` | A file is rendered or paged without leaving the terminal in raw mode |
| Standalone mode (session fabric disabled) | Agent turns and `$` commands run in the CLI process |
| Required audit with a real `xshell-auditd` | The CLI's own records appear in a log that `verify_log` accepts |
| `//connect` through the stand-in `ssh` | Remote sessions list, switch, and run turns; remote interactive processes relay |

## 6. Plan

1. **Harness and the two #100 scenarios.** Add `tests/support` (PTY
   process, screen, fabric, fake provider with gates), and port the #100
   regression from the fake-service unit tests to the real binary. This is
   the proof the harness works on macOS and Linux CI. Done means both
   scenarios pass on all three CI targets and fail with #100 reverted.
2. **The rest of priority 1.** One file per area. Each new CLI bug fix after
   this lands with a scenario here.
3. **Priority 2.** The stand-in `ssh` script and the audited fabric come last;
   both reuse daemon-test helpers that may justify extracting
   `xshell-testkit` at that point.

**Cost:** each scenario starts one daemon and one CLI, about 0.5–2 seconds.
Twenty scenarios add well under a minute to CI, and the tests within a file
run in parallel.

## 7. Alternatives considered

- **Refactor the main loop behind traits and test it in-process** (inject
  the editor, terminal, and session client). This is worth doing for some
  logic, and #100's unit tests took a step in that direction. But the bugs
  this design targets sit in the real terminal, real escape handling, and the
  real process boundary, which an in-process seam replaces. Keep it as a
  complement, not the main approach.
- **Run the CLI with piped stdin, not a terminal.** `scripts/tests` already
  does this for `//quit`. With piped stdin the CLI sees no terminal: there
  is no raw mode, no escape prefix, no approval prompt in the terminal, and
  `$` commands do not use a PTY. That leaves out the code that breaks.
- **`expectrl` or `rexpect`.** These spawn on a PTY and match output, but
  they match raw streams rather than a screen model, and add dependencies for
  what is about 300 lines of harness. A thin harness on `nix` plus `vt100`
  gives screen assertions and full control of process groups and terminal
  size.
- **A full terminal emulator such as tmux in CI.** It adds an external
  dependency and its own timing, for no gain over an in-process screen model.

## 8. Open questions

1. **Default deadline:** 10 seconds is a guess for macOS CI. The first phase
   should measure real scenario times on each runner and set the default
   from them.
2. **Terminal size and wrapping:** do any scenarios need a narrow terminal?
   Prompt wrapping and backlog redraw at 40 columns are plausible bug sites.
3. **Platform gating:** whether Linux arm64 runs the full set or only
   priority 1, if total CI time becomes a concern.
4. **Testkit timing:** whether to extract `xshell-testkit` in phase 1, so
   the daemon tests' `serve_sse` and the new provider share code from the
   start.
