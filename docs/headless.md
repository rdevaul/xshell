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
- **Detaching:** `--detach` starts the turn and exits at once with code 0.
  With text output it prints the session name; follow the turn with the job
  commands below.

## Long-running jobs

A turn runs in the daemon whether or not a client follows it. These commands
act on the session named by `--session`, so a job can outlive the script
that started it, and a restarted script can pick it up:

```sh
session=$(xshell run --detach --approval auto --limits relaxed "Render every part")
xshell status --session "$session"     # idle, agent running, ...
xshell wait --session "$session"       # follow to the end; exit code as for run
xshell result --session "$session"     # the last outcome now, or exit 7 if still running
xshell cancel --session "$session"     # cancel and report (exit 4)
```

- **`wait`** follows the session's active turn to its end, replaying it from
  its first event in the session's journal. With no turn active, it replays
  the last finished turn. SIGINT or SIGTERM stops waiting, leaves the turn
  running, and exits 7.
- **`result`** reports the last finished turn without waiting, or exits 7 if a
  turn is running.
- **`status`** reports the session's activity and exits 0.
- **`cancel`** cancels the active turn, follows it until it ends, and exits 4;
  it exits 1 if no turn is active.
- **Approvals:** `wait` denies approval prompts the same way `run` does. A
  detached turn's prompt waits until `wait` or an interactive client answers
  it, so `run --detach` in `ask` mode warns about this. Use `--approval auto`
  or `off` for unattended jobs.
- **Daemon budgets:** the daemon runs at most `max_concurrent_turns` turns
  and keeps at most `max_sessions` sessions; a run beyond either exits 6.
  A detached turn holds its place until it ends, and each run's session is
  kept unless the run is `--ephemeral`
  (see [daemon budgets](session-fabric.md#daemon-budgets)).
- **Retention:** turns are replayed from the daemon's in-memory event
  journal, which keeps recent turns until its size bound evicts them and does
  not survive a daemon restart. The session and its conversation persist as
  its persistence mode allows.

## Approvals

Each call's outcome combines the approval mode with the daemon's approval
policy and, if given, the run's own (`--approval-policy FILE`); see
[approval policy](session-fabric.md#approval-policy). A run's policy can only
restrict the daemon's.

| `--approval` | Calls the policy leaves to the mode (shell commands, sensitive reads) |
|---|---|
| `off` | Denied by the daemon |
| `ask` (default) | Prompted: answered by the approval channel if there is one, otherwise denied |
| `auto` | Run, if the daemon's `max_approval` allows `auto`; otherwise lowered, with a warning |

**Approval channel.** `--approval-channel FD` (on `run` and `wait`) names a
file descriptor, 3 or above, that the run reads decisions from. For each
`approval_needed` event, write one line:

```json
{"call_id": "call_1", "decision": "approve"}
```

`decision` is `approve`, `deny`, or `abort` (deny and stop the turn). A reply
for another call, an invalid line, or a closed channel denies the prompt;
after the channel closes, later prompts are denied too. Every decision is
reported in an `approval_decided` event whose `source` is `approval_channel`,
`no_approval_channel`, `approval_channel_closed`, `approval_channel_invalid`,
`approval_channel_mismatch`, or `interrupted`. The run also sends the
source to the daemon, which records it in the audit trail (`approval` on the
call's `tool_decision` record: `approval_channel`, or `no_approver` with the
source as its detail).

```python
import json, os, subprocess

read_fd, write_fd = os.pipe()
run = subprocess.Popen(
    ["xshell", "run", "--output", "jsonl", "--approval-channel", str(read_fd),
     "Export the bracket"],
    stdout=subprocess.PIPE, text=True, pass_fds=(read_fd,),
)
os.close(read_fd)
with os.fdopen(write_fd, "w") as channel:
    for line in run.stdout:
        event = json.loads(line)
        if event["event"] == "approval_needed":
            ok = event["name"] == "run_shell" and event["arguments"]["command"].startswith("yapcad ")
            channel.write(json.dumps({"call_id": event["call_id"],
                                      "decision": "approve" if ok else "deny"}) + "\n")
            channel.flush()
```

## Exit codes

| Code | Outcome | Meaning |
|---|---|---|
| 0 | `completed` | The turn finished |
| 1 | `failed` | The turn failed for another reason (provider error, for example) |
| 2 | `usage` | The command line or configuration is invalid |
| 3 | `budget_exhausted` | A turn limit ran out |
| 4 | `cancelled` | The turn was cancelled, by a signal or another client |
| 5 | `approval_denied` | The turn failed after the run denied an approval |
| 6 | `unavailable` | Try again later: the daemon could not be reached, required auditing failed, or a [daemon budget](session-fabric.md#daemon-budgets) is spent |
| 7 | `running` | The turn is still running (`result`, or an interrupted `wait`) |

`run --detach` and `status` exit 0.

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
| `tool_decision` | `call_id`, `decision`, `basis`? | The decision the daemon applied, and what made it (`basis.kind`: `entry`, `not_listed`, `uncheckable_command`, `mode_default`, `not_in_toolset`, `turn_limit`, `blocked_path`; see [auditing](auditing.md#turn-authority-and-decision-basis)) |
| `tool_skipped` | `call_id`, `name` | A call not evaluated because the turn was aborted |
| `tool_result` | `call_id`, `name`, `result` | A tool's output |
| `history_compacted` | `report` | The conversation was shrunk to fit its budget |
| `subagent_started` | `subagent`, `task`, `tools` | A `delegate_task` call started a subagent |
| `subagent_finished` | `subagent`, `outcome`, `message`? | That subagent ended |
| `turn_aborted` | | The turn was aborted at an approval |
| `cancel_requested` | | The client asked the daemon to cancel the turn |
| `detached` | | `run --detach` left the turn running |
| `status` | `schema`, `session`, `session_id`, `activity`, `turn_id`? | The output of `status`; `activity` is `idle`, `agent_running`, `agent_waiting_approval`, `agent_stopping`, or `interactive_process` |
| `warning` | `message` | Something worth knowing that did not end the run |
| `turn_completed` | | The turn finished |
| `turn_failed` | `message`, `reason`? | The turn failed; `reason` is `budget_exhausted` or `audit_unavailable` when known |
| `turn_cancelled` | | The turn was cancelled |
| `result` | `schema`, `outcome`, `exit_code`, `answer`, `message`?, `session`?, `session_id`?, `turn_id`?, `tool_calls`, `approvals_denied` | How the run ended; `answer` is the final text of the main agent |

A run that fails before its turn starts (bad configuration, no daemon) prints
only a `result` line. `wait` and `result` print the replayed turn's events,
starting at `turn_started`, followed by a `result` line.

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
