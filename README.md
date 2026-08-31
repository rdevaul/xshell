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
//tools                               # inspect tools exposed to the agent
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

## Tool safety

Agent file tools are confined to the current xshell working directory after
canonical path resolution, including symlink resolution. Read-only file tools
run automatically and are shown in the transcript. Every agent-requested shell
command displays the exact command and, in the default `ask` mode, requires
explicit confirmation. `--approval auto` removes that confirmation and should
only be used in a trusted workspace; `--approval off` denies shell tools.
Shell tools are non-interactive, time out after 60 seconds, and have bounded
output.

At an approval prompt, `y` executes the requested command, `n` or Enter denies
that command while allowing the agent turn to continue, and `q` aborts the
entire agent turn and returns to the xshell prompt.

The working directory is therefore a meaningful trust boundary. Avoid starting
xshell in a directory containing secrets you do not want the configured model
provider to receive.

This is an early prototype; review tool requests and use it only on data you
can recover.

See [the specification](xshell-specification.md) and
[the implementation plan](xshell-implementation-plan.md) for the intended
system.
