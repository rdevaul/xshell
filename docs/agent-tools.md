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
- **The file** is never opened for writing in place. The content goes to a new temporary file
  in the same directory, which is flushed to disk (`fsync`) and then moved to the file's name:
  by `renameat(2)` when the file existed when the call was prepared, keeping its permission
  bits (never set-id bits), or by `linkat(2)` when it did not, which fails if a file (or a
  symlink) has appeared there since, during the approval wait for example. So the file changes
  all at once or not at all, a symlink there is never written through, and other hard links to
  the old file keep the old content. A symlink at the file is refused when the call is
  prepared; anything other than a regular file is refused. Extended attributes and ACLs of a
  replaced file are not carried over.
- **Policy.** The approval policy and the sensitive-path policy see the canonical path the file
  will have. Path rules name the write tools as they name `read_file`
  (`denied = ["write_file: .git/**"]`), and a `read_file:` denial also denies `edit_file`,
  which reads the file it changes. A write is gated as `file write`, or as `sensitive path`
  when the path is sensitive. Writes count as changing state, so the `off` mode refuses them. A
  `block` path, which includes xshell's own state, is refused in every mode.
- **Write-sensitive paths.** Besides the sensitive paths, a write to a file that runs code or
  changes how tools behave is gated as `sensitive path`: shell start-up files (`.bashrc`,
  `.zshenv`, `.profile` and the rest), `.gitconfig`, `.git/config`, `.git/hooks/**`,
  `.ssh/**`, and a few more (`WRITE_SENSITIVE_PATTERNS` in `sensitive.rs`). Reads of them are
  not affected. **A sensitive write prompts even in `auto` mode**; with nobody to answer, as in
  a headless run, it is denied. An `always_allowed` rule from the host's own policy that names
  the path still lets it run.
- **The approval prompt** shows, above the usual line, the canonical path and whether the file
  is created, replaced or changed, the content's size, and a bounded, escaped preview: the first
  lines of the content, or the old and new text of an edit.
- **Edits** read at most 8 MiB of UTF-8 text and write the result as above. `old_string` must
  match exactly; when it occurs more than once the call fails unless `replace_all` is set. An
  edit whose result would be over 8 MiB is refused before it is built. A failed edit leaves
  the file unchanged.
- **Limits.** Content is at most 8 MiB per call, though tool arguments are capped at 1 MiB
  before that. A filesystem call that has not answered in 30 s is reported as failed, but the
  work may still finish.

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
  newest 64 KiB, and says how much it skipped; skipped output is not shown later. With
  `wait_seconds` (at most 1800) it first waits for the job to end, but never past the turn's
  wall time. Once an ended job's output has all been read, it is freed, and ended jobs keep at
  most 8 MiB of output between them, the oldest freed first.
- **Notices.** When a job ends, the root agent's next tool result gets a line saying so, once.
  A subagent's results never carry them.
- **The end of a response.** If the root agent ends a response while a background job it started
  is still running, the engine emits `waiting_for_jobs` (listing them), and waits for the next
  job to end. It then continues the turn with a message saying what ended (an `auto_continued`
  event with reason `background_job_ended`). If the wall time runs out while it waits, the turn
  ends with the agent's last answer. A job that never ends, such as a dev server, therefore
  holds the turn until its wall time; kill it with `job_kill`, or Ctrl-C the turn.
- **Leftovers.** A foreground command can leave processes running in its group, for example
  `build > log 2>&1 &`, which holds none of the command's pipes and so is not killed when the
  command returns. If the group still has members when the command exits, xshell adopts it as a
  job: the result says so, `job_kill` stops it, and its end is reported. Its output went where
  the command sent it. The same holds for a background job: when its shell exits with members
  of its group still running (`server >log 2>&1 &`), they are adopted as a leftover job. At
  most 16 leftover groups are watched at once; beyond that a group is killed at once.
- **The end of the turn.** Jobs belong to the root turn and are shared with its subagents.
  Every job still running, leftovers included, is killed when the turn ends, however it ends.
  In an interactive session a job therefore does not outlive the message that started it.

A leftover group is checked once a second until it empties, and is never signalled after that.
That leaves the window between one check and the group's ID being reused, which needs the
system to wrap its process IDs within about a second.

## Not built

- Jobs that outlive a turn, with notices delivered in a later turn.
- A full diff in the approval prompt; the preview is bounded.
- Audit records of the jobs killed at the end of a turn, and of a write's canonical path.
