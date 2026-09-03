# xshell

`xshell` is an agent-first, network-aware interactive shell for working with
local and remote AI agents while retaining direct access to an ordinary shell.

This repository is at the first local prototype stage. Input is routed as:

- plain text: send to the configured agent;
- `$command`: execute with the user's login shell;
- `//command`: invoke the xshell control plane.

The current prototype supports:

- streamed responses from Ollama and generic OpenAI-compatible Chat
  Completions endpoints;
- function/tool calling with a bounded 64-step agent loop;
- cwd-confined `read_file` and `list_directory` tools;
- approval-gated `run_shell` calls with timeout and output limits;
- tab completion for executables, paths, and `//` commands;
- named model profiles with live switching via `//model`;
- named local sessions with daemon-lifetime or durable persistence;
- optional fail-closed audit logging through a separate signing daemon;
- persistent cwd changes through `$cd`.

SSH federation, artifact rendering, and yapCAD integration are specified but
not implemented yet.

## Build and run

Install Rust, then run:

```sh
cargo run -p xshell-cli
```

The default configuration expects Ollama at `http://127.0.0.1:11434` and model
`qwen3:8b`. Override it with flags or environment variables:

```sh
XSHELL_MODEL=qwen3:4b cargo run -p xshell-cli

# Ask before agent-requested shell commands (default).
cargo run -p xshell-cli -- --approval ask

# Run every tool without prompting. Use only in a trusted workspace.
cargo run -p xshell-cli -- --approval auto

# Allow read-only tools but deny all agent-requested shell commands.
cargo run -p xshell-cli -- --approval off

cargo run -p xshell-cli -- \
  --provider openai \
  --base-url https://api.openai.com \
  --model YOUR_MODEL \
  --api-key-env OPENAI_API_KEY
```

Prefix a command with `$` for a single shell input. Prefix it with `$$` to
enter sticky shell mode: subsequent input lines begin with an editable `$`.
Backspace over that `$` and submit plain text to return to agent input.

## Agent response rendering

On a terminal, xshell renders streamed agent Markdown as readable headings,
paragraphs, lists, quotes, tables, links, inline code, and fenced code blocks.
Prose wraps to the detected terminal width while code is left structurally
intact. Rendering is performed only by the client: conversation history,
session state, and audit records retain the model's original response. Shell
stdout and stderr also remain verbatim.

Model output is stripped of terminal control sequences before display. ANSI
styling is enabled only for a terminal and is disabled whenever `NO_COLOR` is
set. Configure the policy in `config.toml`:

```toml
[rendering]
markdown = "auto" # auto, always, or never
color = "auto"    # auto, always, or never
# width = 100      # optional; valid range is 20..512
```

The equivalent one-run overrides are `--markdown`, `--color`,
`XSHELL_MARKDOWN`, and `XSHELL_COLOR`. `markdown = "never"` preserves the
model's Markdown source layout but still removes terminal control sequences.
When output is redirected, both policies default to `never` through their
`auto` setting.

The same rendering engine powers the modular `//view` command:

```text
//view README.md
//view docs/design.rst
//view --as markdown notes.txt
```

Paths are resolved on the active session host, so the command behaves the same
for local and SSH sessions. The first built-in viewers support Markdown and a
safe reStructuredText subset. Text acquisition is limited to 4 MiB and records
the resolved path, media type, byte length, SHA-256 hash, selected viewer, and
outcome in the audit log. See [the viewer architecture](docs/viewers.md).

## Model profiles and OpenRouter

Copy the example configuration, then edit its models to suit the providers you
use:

```sh
mkdir -p ~/.config/xshell
cp config.example.toml ~/.config/xshell/config.toml
```

The example includes local Ollama, OpenRouter, and OpenAI-compatible profiles.
API keys are never stored in the configuration: each profile names the
environment variable from which xshell should read its key. `api_key_env` must
contain the literal variable name (for example, `OPENROUTER_API_KEY`), never the
key itself. For OpenRouter:

```sh
export OPENROUTER_API_KEY='your-key-here'
cargo run -p xshell-cli -- --profile openrouter-free
```

Model and status output reports only whether credentials are set; it never
prints the environment variable's name or value. If a configured credential is
missing, xshell stops before sending a request rather than making an anonymous
request that will fail at the provider.

You can also try OpenRouter without creating a configuration file:

```sh
export OPENROUTER_API_KEY='your-key-here'
cargo run -p xshell-cli -- \
  --provider openai \
  --base-url https://openrouter.ai/api/v1 \
  --model openrouter/free \
  --api-key-env OPENROUTER_API_KEY
```

`openrouter/free` is convenient for a connectivity test, but the model selected
by OpenRouter may not support tools. Configure a specific OpenRouter model that
supports tool calling when using xshell's agent tools.

Within xshell:

```text
Explain this project                 # agent message
$git status --short                  # ordinary shell command
$cd crates/xshell-core               # persistently change xshell's cwd
//status                              # inspect the active session
//model                               # inspect the active model profile
//model list                          # list configured profiles
//model openrouter-free               # switch profile and clear chat history
//sessions                            # list sessions on this host
//new bees --durable                  # create and enter a durable session
//new robot --model local-qwen        # create with a chosen model profile
//switch bees                         # restore its model, cwd, and conversation
//detach                              # preserve the session and exit
//close                               # delete it and return to the previous session
//tools                               # inspect tools exposed to the agent
//view README.md                      # render a file on the active session host
//help                                # list control commands
//quit                                # exit
```

Press Tab after `$` to complete commands from `PATH` or filesystem paths.
Completion intentionally provides a portable baseline rather than loading the
user's complete zsh/bash plugin and completion environment.

Switching model profiles deliberately clears the conversation history. This
prevents context given to a local model from being sent to a cloud provider
without an explicit new message. CLI flags and their corresponding environment
variables override values in the startup profile.

## Named local and SSH sessions

Named sessions are served by `xshelld` over a per-user Unix socket. Enable the
`[session_fabric]` section in the configuration, then start the service before
the CLI:

```sh
cargo run -p xshell-session --bin xshelld -- --config config.example.toml
cargo run -p xshell-cli -- --config config.example.toml --session bees
```

The default `//new NAME` lifecycle is `--daemon`: it survives detach and client
disconnects but not daemon restart. Use `--durable` to serialize the session's
model binding, working directory, and conversation history, or `--ephemeral`
to remove it at detach. `--fabric` marks a session for later SSH federation;
`--host-only` keeps it out of that future export. In this first increment all
sessions remain local and single-user, and only one interactive client may
control a session at a time.

`//quit` and Ctrl-D leave the current persistent session available for later
attachment. `//close` deletes the current session; xshell returns to the
previously visited available session, or otherwise the most recently active
available session. Closing the last session exits the CLI.

When the session fabric is enabled, `xshelld` owns model requests, agent tool
loops, approvals, and `$` command execution. The CLI renders its sequenced
event stream and sends approval decisions. A daemon-lifetime or durable turn
continues if the client disconnects; reattachment replays the bounded event
journal before resuming live output. Completed durable session state survives
daemon restart, but an in-flight turn does not yet survive daemon restart.

Credential environment variables named by a model profile must be available
to the `xshelld` process. They are not sent to or resolved by an attached CLI.
This distinction becomes important once the CLI and daemon are on different
hosts. Likewise, when auditing is enabled, `xshelld` records execution events
(input, model output, tool calls, approvals, shell completion) itself at the
point of execution; see [the audit design](docs/auditing.md#who-records-what).

To connect another macOS or Linux host, install `xshelld` somewhere on that
host's non-interactive SSH `PATH`, configure and start its per-user daemon, then
run:

```text
//connect rich@mini.local
//connect rich@mini.local --session cad
//sessions
//switch laptop:bees
//switch local:default
```

`//connect` invokes the system `ssh` command with PTY allocation disabled, so
the user's normal `~/.ssh/config`, agent, host-key policy, and authentication
prompts apply. The remote command is `xshelld serve-stdio`; it opens no network
port and proxies the versioned protocol to the remote user's Unix socket.
Connected hosts remain available in the common session catalog and switcher.
`local:NAME` always selects the session on the controller's Unix-socket host,
regardless of that machine's configured host alias.
Only sessions marked `fabric` are listed or attachable through this transport.
If the selected name does not exist, xshell creates it in the remote user's
home directory using the current model binding. Automatic remote installation
and version negotiation beyond the protocol handshake are not implemented yet.
Shell command and path completion is evaluated by the remote daemon against
its inherited `PATH` and the active session cwd. It intentionally does not
source native zsh/bash completion frameworks, startup scripts, aliases, or
functions.

Directly entered commands in daemon-backed local and remote sessions run as
session-owned terminal jobs. This supports colors, pagers, and full-screen
programs; input, output, resize events, and terminal-generated signals are
relayed byte-for-byte. Closing a controller or PTY stream leaves a terminal job
running for daemon-lifetime and durable sessions, with up to 1 MiB of output
available for replay. Ephemeral sessions still terminate their jobs on detach.

While a terminal job has focus, `Ctrl-]` is the default configurable command
prefix: `d` detaches to the current session's REPL, `s` opens the session
switcher, `l` selects the last session, `n`/`p` cycle, `q` terminates, `?` shows
help, and a second `Ctrl-]` sends the prefix literally. Sessions without a
terminal job appear as `[REPL]` targets; selecting one activates that session
and returns to its xshell prompt. At the REPL, `//terminal` reattaches the
current job, and `//switch HOST:SESSION` automatically resumes a running job on
the selected session. `//terminal list` and `//terminal kill` inspect or
terminate jobs. Configure the prefix with `session_fabric.pty_escape`.

Protocol v8 uses a dedicated authenticated binary stream locally and over SSH.
See [the terminal-job design and trust boundary](docs/pty.md).

See [the session-fabric protocol and current boundary](docs/session-fabric.md).

## Audit logging

The audit service records xshell interactions in hash-chained JSONL logs and
creates periodic and final Ed25519-signed checkpoints. Each checkpoint includes
a blinded commitment intended for future peer federation or public timestamp
anchoring.

For a local functional test, enable the `[audit]` section in
`config.example.toml`, then start the daemon before xshell:

```sh
cargo run -p xshell-audit --bin xshell-auditd -- \
  --directory "$HOME/.local/state/xshell/audit" \
  --socket "$HOME/.local/state/xshell/audit/audit.sock"

cargo run -p xshell-cli -- --config config.example.toml
```

Both daemons refuse directories they do not own or that are group/world
writable, so avoid shared locations such as `/tmp`, where another local user
could pre-create the path.

This same-user development setup is not protected from shell commands. A
tamper-resistant installation must run the daemon under a dedicated OS account
with an audit directory inaccessible to xshell and its child processes. See
[the audit design and deployment notes](docs/auditing.md), including the
current stdout/stderr capture limitation and example launchd/systemd units.

## Tool safety

Agent file tools are confined to the current xshell working directory after
canonical path resolution, including symlink resolution. Read-only file tools
run automatically and are shown in the transcript. Every agent-requested shell
command displays the exact command and, in the default `ask` mode, requires
explicit confirmation. `--approval auto` removes that confirmation and should
only be used in a trusted workspace; `--approval off` denies shell tools.
When a session daemon executes turns, its `session_fabric.max_approval`
setting (default `ask`) caps whatever the CLI requests, so a remote host's
operator decides whether unattended shell execution is allowed there.
Shell tools are non-interactive, run in a plain (non-login) shell so the
user's profile is not sourced for model-authored commands, execute in their
own process group, and have bounded output. After 60 seconds the entire process
group is killed, so background jobs and pipelines cannot outlive the timeout.
The approval prompt shows the command with control characters, newlines, and
invisible Unicode formatting rendered as visible escapes, so the text you
approve is exactly the text the shell will receive.

At an approval prompt, `y` executes the requested command, `n` or Enter denies
that command while allowing the agent turn to continue, and `q` aborts the
entire agent turn and returns to the xshell prompt.

The working directory is therefore a meaningful trust boundary. Avoid starting
xshell in a directory containing secrets you do not want the configured model
provider to receive. As a backstop, reads and listings of paths that match
`session_fabric.sensitive_paths` (defaults cover `.env`, private keys,
`.ssh/`, `.aws/`, `.git/config`, Terraform state, and similar) are promoted
from automatic to approval-gated: `ask` prompts with the reason, `off` denies,
and `auto` still runs them. Matching uses the resolved path, so a symlink
with an innocent name does not bypass it.

This is an early prototype; review tool requests and use it only on data you
can recover.

See [the specification](xshell-specification.md) and
[the implementation plan](xshell-implementation-plan.md) for the intended
system.
