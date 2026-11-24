# Agent file and job tools

Besides `read_file`, `list_directory`, `view_image`, `run_shell` and `delegate_task`, the
engine offers four built-in tools:

| Tool | What it does | Gated |
|---|---|---|
| `write_file` | Create a text file or replace one, creating missing parent directories | always, like `run_shell` |
| `edit_file` | Replace exact text in an existing text file; unique unless `replace_all` | always |
| `job_output` | A background job's status and its output since the last read; can wait for it to end | no |
| `job_kill` | Kill a background job's process group | no |

`run_shell` takes `background: true` to start a job and return its id at once.

## Writes

Writes run inside xshelld, not inside whatever contains the session's shell commands. They are
confined the way reads are (`crates/xshell-execution/src/prepared.rs`):

- **The directory.** The deepest existing directory on the path is opened by the same walk as
  a read: one component at a time beneath the session cwd, with `openat(2)` and `O_NOFOLLOW`.
  A symlink on the way is resolved by the walk and must stay inside the root.
- **Missing directories** are named when the call is prepared and created only when it runs,
  after approval. Each is opened with `O_NOFOLLOW` as it is created, so one swapped for a
  symlink in between fails.
- **The file** is opened with `O_NOFOLLOW`. A symlink there is refused, both when the call is
  prepared and if one appears before it runs; it is never written through. Anything other than
  a regular file is refused.
- **Policy.** The approval policy and the sensitive-path policy see the canonical path the file
  will have. A write is gated as `file write`, or as `sensitive path` when the path is
  sensitive. Writes count as changing state, so the `off` mode refuses them. A `block` path,
  which includes xshell's own state, is refused in every mode.
- **Edits** read the file through the same open descriptor they write it with, at most 8 MiB of
  UTF-8 text. `old_string` must match exactly; when it occurs more than once the call fails
  unless `replace_all` is set. A failed edit leaves the file unchanged.
- **Limits.** Content is at most 8 MiB per call.

Subagents started with `tools: read_only` get neither write tool.

## Background jobs

A job is a process group xshell started. xshell runs outside any sandbox that contains the
commands, so it can always stop a job, even where the commands cannot signal one another: under
the macOS profile each command is its own sandbox, and signals do not cross sandboxes
(`crates/xshell-execution/src/jobs.rs`).

- **Starting.** `run_shell` with `background: true` starts the command as `run_shell` would:
  the same runner, environment and process group. It returns the job id. At most 8 jobs run
  at once. A job has no time limit of its own; the turn's wall time bounds it.
- **Output.** stdout and stderr are kept together, in arrival order, up to the last 1 MiB.
  `job_output` returns the status and what the job wrote since the last read, at most the
  newest 64 KiB, and says how much it skipped. With `wait_seconds` (at most 1800) it first waits
  for the job to end.
- **Notices.** When a job ends, the next tool result gets a line saying so, once.
- **The end of a response.** If the root agent ends a response while a background job it started
  is still running, the engine waits for the next job to end. It then continues the turn with a
  message saying what ended (an `auto_continued` event with reason `background_job_ended`). If
  the wall time runs out while it waits, the turn ends with the agent's last answer.
- **Leftovers.** A foreground command can leave processes running in its group, for example
  `build > log 2>&1 &`, which holds none of the command's pipes and so is not killed when the
  command returns. If the group still has members when the command exits, xshell adopts it as a
  job: the result says so, `job_kill` stops it, and its end is reported. Its output went where
  the command sent it.
- **The end of the turn.** Jobs belong to the root turn and are shared with its subagents.
  Every job still running, leftovers included, is killed when the turn ends, however it ends.
  In an interactive session a job therefore does not outlive the message that started it.

A leftover group is checked once a second until it empties, and is never signalled after that.
That leaves the window between one check and the group's ID being reused, which needs the
system to wrap its process IDs within about a second.

## Not built

- Jobs that outlive a turn, with notices delivered in a later turn.
- A file-change preview (diff) in the approval prompt.
