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
- function/tool calling with an eight-step agent loop;
- cwd-confined `read_file` and `list_directory` tools;
- approval-gated `run_shell` calls with timeout and output limits;
- tab completion for executables, paths, and `//` commands;
- persistent cwd changes through `$cd`.

Persistent daemons, SSH sessions, artifact rendering, and yapCAD integration
are specified but not implemented yet.

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

Within xshell:

```text
Explain this project                 # agent message
$git status --short                  # ordinary shell command
$cd crates/xshell-core               # persistently change xshell's cwd
//status                              # inspect the active session
//tools                               # inspect tools exposed to the agent
//help                                # list control commands
//quit                                # exit
```

Press Tab after `$` to complete commands from `PATH` or filesystem paths.
Completion intentionally provides a portable baseline rather than loading the
user's complete zsh/bash plugin and completion environment.

## Tool safety

Agent file tools are confined to the current xshell working directory after
canonical path resolution, including symlink resolution. Read-only file tools
run automatically and are shown in the transcript. Every agent-requested shell
command displays the exact command and, in the default `ask` mode, requires
explicit confirmation. `--approval auto` removes that confirmation and should
only be used in a trusted workspace; `--approval off` denies shell tools.
Shell tools are non-interactive, time out after 60 seconds, and have bounded
output.

The working directory is therefore a meaningful trust boundary. Avoid starting
xshell in a directory containing secrets you do not want the configured model
provider to receive.

This is an early prototype; review tool requests and use it only on data you
can recover.

See [the specification](xshell-specification.md) and
[the implementation plan](xshell-implementation-plan.md) for the intended
system.
