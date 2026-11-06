# Headless runs

`xshell run` runs one agent turn without a terminal and reports how it ended.
It is meant for scripts: a build step, a CAD pipeline, or a supervisor that
starts many runs.

```sh
xshell run --limits relaxed --output jsonl "Export the bracket to STEP" > run.jsonl
echo "exit: $?"
```

## Requirements

Headless runs always go through `xshelld`. Set `session_fabric.enabled = true`
and start the daemon (see [session fabric](session-fabric.md)). The daemon
runs the turn, records it in the audit log, and applies its approval and limit
ceilings. If the client is killed, a turn in an ordinary session keeps
running in the daemon (an `--ephemeral` session ends with its client).

## Usage

```text
xshell [SHARED OPTIONS] run [--output text|jsonl] [--ephemeral] [--prompt-file FILE] [PROMPT]
```

- **Prompt:** the positional argument, `--prompt-file FILE`, or stdin when
  neither is given and stdin is not a terminal.
- **Session:**
  - By default, each run creates its own session, named `run-<time>-<id>`, which
    stays in the daemon afterwards.
  - `--session NAME` runs in that session instead, creating it if needed. An
    existing session keeps its own model and working directory.
  - `--ephemeral` uses a session that is removed when the run ends.
- **Shared options:** `--config`, `--profile`, `--provider`, `--model`,
  `--base-url`, `--api-key-env`, `--system-prompt`, `--session`, `--cwd`,
  `--approval`, and `--limits` work as they do for interactive `xshell`,
  before or after `run`.
- **Output:** `--output text` (the default) prints the final answer on stdout
  and progress on stderr. `--output jsonl` prints one JSON object per line on
  stdout, as described below.
- **Signals:** SIGINT or SIGTERM cancels the turn. The run still reports the
  outcome and exits with code 4.

## Approvals

A headless run has nobody at a terminal to answer an approval prompt.

| `--approval` | Gated calls (shell commands, sensitive reads) |
|---|---|
| `off` | Denied by the daemon |
| `ask` (default) | Denied by the run, with an `approval_decided` event whose `source` is `no_approval_channel`; the model is told and the turn continues |
| `auto` | Run, if the daemon's `max_approval` allows `auto`; otherwise lowered, with a warning |

An approval policy file and an approval channel, for finer control, are
planned ([agentic CAD plan](agentic-cad-plan.md), §5.2).

## Exit codes

| Code | Outcome | Meaning |
|---|---|---|
| 0 | `completed` | The turn finished |
| 1 | `failed` | The turn failed for another reason (provider error, for example) |
| 2 | `usage` | The command line or configuration is invalid |
| 3 | `budget_exhausted` | A turn limit ran out |
| 4 | `cancelled` | The turn was cancelled, by a signal or another client |
| 5 | `approval_denied` | The turn failed after the run denied an approval |
| 6 | `unavailable` | The daemon could not be reached, or required auditing failed |

## JSON Lines schema (version 1)

Every line is an object with an `event` field. The first line is
`run_started` and the last is `result`; both carry `schema: 1`. Fields that
would be empty are left out. Events from a subagent carry an `agent` field
naming it.

New event types and new optional fields may be added without changing the
schema version; consumers should ignore what they do not recognise. An
incompatible change increments `schema`.

| Event | Fields | Meaning |
|---|---|---|
| `run_started` | `schema`, `session`, `session_id`, `turn_id`, `limits_profile` | The turn was accepted |
| `turn_started` | `approval`, `limits`, `requested_approval`?, `requested_limits`? | The daemon started the turn with this approval mode and these limits; the `requested_*` fields appear when it lowered what was asked |
| `text` | `text` | Streamed response text |
| `assistant_message` | `content`, `tool_calls`, `finish_reason`? | One complete model response |
| `auto_continued` | `reason` | The turn continued automatically after an unfinished response |
| `tool_requested` | `call_id`, `name`, `arguments` | The model asked for a tool |
| `approval_needed` | `call_id`, `name`, `arguments`, `reason`? | A gated call needs a decision |
| `approval_decided` | `call_id`, `decision`, `source` | The run's own decision on that call |
| `tool_decision` | `call_id`, `decision` | The decision the daemon applied |
| `tool_skipped` | `call_id`, `name` | A call not evaluated because the turn was aborted |
| `tool_result` | `call_id`, `name`, `result` | A tool's output |
| `history_compacted` | `report` | The conversation was shrunk to fit its budget |
| `subagent_started` | `subagent`, `task`, `tools` | A `delegate_task` call started a subagent |
| `subagent_finished` | `subagent`, `outcome`, `message`? | That subagent ended |
| `turn_aborted` | | The turn was aborted at an approval |
| `cancel_requested` | | The run received a signal and asked the daemon to cancel |
| `warning` | `message` | Something worth knowing that did not end the run |
| `turn_completed` | | The turn finished |
| `turn_failed` | `message`, `reason`? | The turn failed; `reason` is `budget_exhausted` or `audit_unavailable` when known |
| `turn_cancelled` | | The turn was cancelled |
| `result` | `schema`, `outcome`, `exit_code`, `answer`, `message`?, `session`?, `session_id`?, `turn_id`?, `tool_calls`, `approvals_denied` | How the run ended; `answer` is the final text of the main agent |

A run that fails before its turn starts (bad configuration, no daemon) prints
only a `result` line.

## Examples

Run and act on the outcome in a shell script:

```sh
if answer=$(xshell run --ephemeral "Summarise build.log"); then
  printf '%s\n' "$answer" > summary.txt
else
  echo "run failed with exit code $?" >&2
fi
```

Read the event stream from Python:

```python
import json, subprocess

run = subprocess.run(
    ["xshell", "run", "--output", "jsonl", "--limits", "relaxed",
     "Export the bracket to STEP and render it"],
    capture_output=True, text=True,
)
events = [json.loads(line) for line in run.stdout.splitlines()]
result = events[-1]
print(result["outcome"], result["answer"])
for event in events:
    if event["event"] == "tool_requested":
        print("tool:", event["name"], event["arguments"])
```
