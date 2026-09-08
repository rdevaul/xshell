# xshell

**A persistent, network-transparent workspace for humans, AI agents, shells,
and full-screen terminal tools.**

Most agentic shell tools are local chat loops: the conversation belongs to one
terminal, remote machines are separate SSH worlds, and long-running work ends
when the client disconnects. xshell makes the **named session** the durable unit
of work instead. A session carries its agent conversation, model binding,
working directory, execution state, and interactive process. Sessions can live
on this machine or another host and are selected through the same interface.

At one prompt you can:

- describe work in natural language and let an agent use bounded tools;
- prefix input with `$` when you want the ordinary shell directly;
- run pagers, TUIs, and full-screen programs in detachable PTYs;
- leave your full screen editor running while you switch sessions or hosts, or 
  disconnect while a daemon-owned agent turn continues;
- choose a different local or hosted model for each session and machine; and
- enforce host-local approval policy and produce tamper-evident audit records
  at the point where execution occurs.

```text
                         one session catalog and switcher
                                      │
               ┌──────────────────────┴──────────────────────┐
               │                                             │
        Unix socket                                     SSH stdio tunnel
               │                                             │
    xshelld on this host                           xshelld on another host
      ├─ named sessions                              ├─ named sessions
      ├─ local/cloud model                           ├─ different model
      ├─ tools and PTYs                              ├─ tools and PTYs
      └─ audit boundary                              └─ audit boundary
```

xshell is not yet a POSIX shell or a drop-in login-shell replacement. It is a
working alpha of the session and execution fabric needed to become one.

## What works today

| Area | Current capability |
|---|---|
| Agent loop | Streaming Ollama and OpenAI-compatible Chat Completions, including OpenRouter; tool calling with a bounded 64-step loop |
| Shell access | Direct `$` commands, sticky `$$` mode, persistent `cd`, command/path completion, pipelines, colors, pagers, and full-screen PTY applications |
| Sessions | Multiple named sessions per user and host; ephemeral, daemon-lifetime, and durable lifecycle modes; common switching for idle prompts, active agent turns, and interactive processes |
| Network fabric | SSH-connected macOS and Linux hosts in the same session catalog; remote creation, switching, execution, completion, viewing, detach, and manual reconnect |
| Detachment | Daemon-owned agent turns and interactive processes continue after controller disconnect; bounded event and terminal-output replay on return |
| Models | Named profiles, live `//model` switching, per-session bindings, per-model history budgets, and environment-based credentials that are never printed or sent over the session protocol |
| Viewing | Streamed terminal Markdown, tables, a safe reStructuredText subset, policy-driven pagination, and local rendering of text acquired from either local or remote sessions with `//view` |
| Safety | Exact-command approval, remote-host approval ceilings, cwd-confined file tools, sensitive-path gating, execution time/output limits, and process-group cleanup |
| Audit | Separate append-only service, hash-chained JSONL, Ed25519-signed checkpoints, daemon-side execution events, and opt-in bounded byte-for-byte PTY stream capture |

The major pieces that are **not** implemented yet are automatic installation
and service setup on remote hosts, resilient SSH supervision, a shared
`/xshell` filesystem namespace, binary/media viewer plugins and CAD rendering,
multi-user sessions and ACLs, and restoration of an in-flight process across an
`xshelld` restart. FutureShell—the contractual workflow and bounded-rollback
language planned on top of this fabric—is currently a design and roadmap, not
an executable language.

An experimental typed dataflow representation for FutureShell is available in
`crates/xshell-flow`. It models tasks, contract-gated branches, parallel joins,
and bounded feedback loops. See [the Flow IR prototype](docs/futureshell-flow-ir.md).
The `xshell-plan` crate lowers valid flows into deterministic Plan V0 task
templates and can resolve provisional program and predicate catalogs into
pinned plan identities; see [the Plan V0 prototype](docs/futureshell-plan-v0.md).
Current milestone state, blockers, and artifact traceability are maintained in
[FutureShell status](docs/futureshell-status.md).

## Build and run

xshell targets macOS and Linux and requires Rust 1.88 or newer. With Ollama
serving `qwen3:8b` on its default local endpoint:

```sh
git clone https://github.com/rdevaul/xshell.git
cd xshell
cargo run -p xshell-cli
```

The standalone mode keeps execution inside the CLI and is the shortest path to
trying the agent/shell REPL. The three input routes are always explicit:

```text
Explain the architecture of this repository   # send to the active agent
$git status --short                            # run in the ordinary shell
$$rg "TODO" crates                             # enter sticky shell mode
//status                                       # invoke the xshell control plane
```

In sticky shell mode, following prompts begin with an editable `$`. Backspace
over it and submit plain text to return to agent input.

The default tool approval mode is `ask`. Other modes are intentional command
line choices:

```sh
cargo run -p xshell-cli -- --approval ask  # prompt before agent shell tools
cargo run -p xshell-cli -- --approval off  # deny agent shell tools
cargo run -p xshell-cli -- --approval auto # unattended; trusted workspaces only
```

## Models and providers

Copy the example configuration and edit the named profiles:

```sh
mkdir -p ~/.config/xshell
cp config.example.toml ~/.config/xshell/config.toml
```

The example contains Ollama, OpenRouter, and generic OpenAI-compatible
profiles. Credentials remain in environment variables; `api_key_env` contains
the variable's **name**, never its value:

```toml
default_model = "openrouter-qwen"

[models.openrouter-qwen]
provider = "openai"
model = "YOUR_OPENROUTER_MODEL"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
max_history_bytes = 131072
```

```sh
export OPENROUTER_API_KEY='your-key-here'
cargo run -p xshell-cli -- --profile openrouter-qwen
```

Within xshell, `//model list` shows configured profiles and `//model NAME`
switches the active session to a fresh conversation. Clearing history on a
model switch prevents context supplied to one provider from silently crossing
into another. In daemon-backed sessions, the credential variable is resolved
in the `xshelld` environment on the machine doing the work.

## Persistent local sessions

Enable the fabric and start the per-user session daemon:

```toml
[session_fabric]
enabled = true
required = true
default_session = "default"
max_approval = "ask"
pty_escape = "ctrl-]"
```

```sh
cargo run -p xshell-session --bin xshelld -- \
  --config ~/.config/xshell/config.toml

# In another terminal:
cargo run -p xshell-cli -- --session default
```

Useful session commands include:

```text
//sessions                            # catalog sessions on every connected host
//history                             # list input entered in this client
//new bees --durable                  # durable conversation and cwd
//new ornithopter --model local-qwen  # create with a selected model
//switch bees                         # switch by local session name
//switch local:default                # explicitly select this host
//resume                              # return to this session's active work
//stop                                # stop its agent turn or interactive process
//detach                              # leave a persistent session running and exit
//close                               # delete it and fall back to the previous session
//quit                                # detach and exit without deleting the session
```

Lifecycle modes differ deliberately:

| Mode | After controller detach | After daemon restart |
|---|---|---|
| `ephemeral` | deleted | deleted |
| `daemon` | retained | deleted |
| `durable` | retained | conversation, model, and cwd restored |

Active agent turns and interactive processes survive controller disconnect but
not daemon restart. Durable session snapshots are written atomically and are
restored detached.

## One fabric across hosts

Install the same xshell build on a remote macOS or Linux host, make `xshelld`
available on its non-interactive SSH `PATH`, and start its per-user daemon.
From the same xshell checkout and revision used by the controller:

```sh
cargo install --locked --path crates/xshell-session
xshelld --config ~/.config/xshell/config.toml
```

The second command runs the daemon in the foreground, which is convenient for
initial testing; leave that terminal open. Ensure `$HOME/.cargo/bin` is on the
remote account's non-interactive SSH `PATH`. For zsh, put the following in
`~/.zshenv` (not only `~/.zshrc`):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Verify discovery from the controlling host before connecting:

```sh
ssh rich@mini.local 'command -v xshelld && xshelld --version'
```

Then connect using the SSH identity and policy you already use:

```text
//connect rich@mini.local
//connect rich@mini.local --session cad
//sessions
//switch mini:cad
//switch local:bees
```

xshell invokes `ssh -T` and `xshelld serve-stdio`; it opens no inbound service
and does not replace OpenSSH authentication, host-key verification,
`~/.ssh/config`, or agent handling. Each remote daemon enforces its own model
credentials, sensitive-path policy, and maximum approval level. Only sessions
marked `fabric` are exported; `host_only` sessions remain private to that host.

Remote command and path completion is evaluated by the remote daemon against
its inherited `PATH` and the session cwd without sourcing shell startup files,
aliases, functions, or native completion frameworks.

Automatic remote bootstrap, service installation, reconnection supervision,
and SSH connection multiplexing remain planned work. See the
[session-fabric design](docs/session-fabric.md).

## Full terminal applications without a second abstraction

A directly entered `$` command in a fabric-backed interactive session runs as
that session's interactive process. It receives a real PTY, so commands such as
these behave normally:

```text
$cat data.json | jq | less
$htop
$emacs -nw design-notes.md
```

The process belongs to the session; it is not a separately managed user-facing
object. The default `Ctrl-]` router is also available at the xshell prompt and
while an agent turn is streaming or waiting for approval, so long-running work
never traps the controller in one session. In a PTY, its control bytes are not
sent to the process:

| Sequence | Action |
|---|---|
| `Ctrl-] s` | Open the unified session picker; `n` creates a session on the current host |
| `Ctrl-] d` | Detach to the xshell session prompt |
| `Ctrl-] l` | Return to the previously focused session |
| `Ctrl-] n` / `Ctrl-] p` | Cycle sessions |
| `Ctrl-] q` | Stop the current agent turn or interactive process |
| `Ctrl-] ?` | Show key help |
| `Ctrl-] Ctrl-]` | Send a literal prefix byte to a focused PTY |

Switching releases focus without cancelling work or answering a pending
approval; returning to the session replays and resumes it. Daemon and durable
sessions retain the process across stream or controller disconnects and keep up
to 1 MiB of output for replay. xshell forwards terminal bytes and resize events
rather than emulating a terminal, preserving curses and Emacs behavior. See the
[PTY design and trust boundary](docs/pty.md).

## Rendering and `//view`

Agent Markdown is rendered incrementally as headings, paragraphs, lists,
quotes, links, tables, inline code, and fenced blocks. Prose wraps to terminal
width while source conversation and audit records retain the model's original
text. Model output is stripped of terminal control sequences before display;
`NO_COLOR` is honored.

```toml
[rendering]
markdown = "auto" # auto, always, never
color = "auto"    # auto, always, never
# width = 100
```

The modular viewer uses the same renderer for files:

```text
//view README.md
//view docs/design.rst
//view --as markdown notes.txt
//view --paginate long-report.md
//view --no-paginate short-notes.md
```

The active session host acquires the source and the controller renders it
locally, so `//view` behaves consistently across SSH without opening a viewer
port. Markdown and a safe reStructuredText subset are built in. Acquisition is
bounded to regular UTF-8 files of at most 4 MiB and records content metadata in
the audit trail. Long views are paged automatically on interactive terminals;
short or redirected output remains inline. Pagination can be configured by
presentation class or viewer and overridden for one command. The configured
pager is an argument vector executed directly rather than a shell command;
xshell's default is `less -R -X` in secure mode.

```toml
[view]
pagination = "auto" # auto, always, never
pager = ["less", "-R", "-X"]

[view.classes.text]
pagination = "auto"

[view.viewers.markdown]
pagination = "always"
```

Binary/media plugins, inline images, F3D integration, and multimodal-agent
attachments are next-stage work. See the
[viewer architecture](docs/viewers.md).

## Safety and auditability

xshell is agentic infrastructure, not a sandbox. Its controls make authority
visible and bounded:

- `read_file` and `list_directory` are confined to the session cwd after
  canonical and symlink resolution.
- Credential-like paths such as `.env`, `.ssh/**`, private keys,
  `.git/config`, and Terraform state are approval-gated by default.
- Agent shell tools display an escaped, byte-faithful command before approval;
  `y` runs it, `n` denies it and continues the turn, and `q` aborts the turn.
- Shell tools use a non-login shell, bounded captured output, a 60-second
  timeout, and a separate process group that is killed as a unit.
- A remote daemon's `max_approval` ceiling can make a controller's requested
  policy stricter, never more permissive.

For tamper-evident records, `xshell-auditd` writes append-only, hash-chained
JSONL with periodic and final Ed25519-signed checkpoints. `xshelld` records at
the execution boundary, including detached agent work. Interactive-process
lifecycle is recorded at the daemon when auditing is enabled; byte-for-byte
input and output capture is an explicit, bounded opt-in because it can include
passwords and other sensitive terminal data.

```toml
[audit]
enabled = true
required = true
socket = "/ABSOLUTE/PRIVATE/PATH/audit.sock"
directory = "/ABSOLUTE/PRIVATE/PATH/audit"
terminal_stream = false
terminal_stream_max_bytes = 16777216
```

A same-user development deployment protects logs from accidental modification,
not from commands running with that user identity. Tamper resistance requires
running the audit service as a dedicated OS account with a protected directory.
Federated or public timestamp witnessing is designed for later addition. See
the [audit design, deployment guidance, and verifier](docs/auditing.md).

## Direction

The implemented session fabric is the substrate for the broader xshell vision:
transparent cross-host resources, agentic CAD workflows around
[yapCAD](https://github.com/rdevaul/yapCAD), richer viewer plugins, and
FutureShell programs with explicit agent selection, resource budgets,
deterministic contractual evidence, selective state promotion, and bounded
rollback guarantees.

- [Current system specification](xshell-specification.md)
- [Implementation plan](xshell-implementation-plan.md)
- [FutureShell status and traceability](docs/futureshell-status.md)
- [FutureShell roadmap](docs/futureshell-roadmap.md)
- [FutureShell implementation plan](docs/futureshell-implementation-plan.md)

xshell is under active development. Review agent tool requests, keep recoverable
copies of important data, and do not assume unimplemented isolation guarantees.

Licensed under the [MIT License](LICENSE).
