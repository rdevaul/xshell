# XSHELL.md: durable handoff and goals

**Status:** Proposed

**Date:** 2026-10-26

**Revised:** 2026-11-02, for the whole-turn budget (#84) and `delegate_task`
subagents (#86), with TOML front matter, per-request injection, checked goal
completion, and the daemon protocol.

**Author:** Richard DeVaul

## Summary

A session's conversation is not a reliable place to keep the state of
long-running work. Compaction drops old turns, a context overflow can end a
turn, and a session can be closed or lost. This proposal adds an `XSHELL.md`
file in a session's working directory that records the current state of the
work and serves as a durable handoff to a later session. A new `//goal`
command records a goal in the same file, with completion criteria, a TTL, and
a state, so the agent can keep working toward it across turns and sessions.

Because the file's contents are sent to the model with every request,
`XSHELL.md` is an instruction channel. The design therefore treats every file as untrusted
until a person has reviewed it, and it tracks the exact content a person
approved by hash, in the audit trail when one is available.

## Goals

- Continuity: a new session in the same directory can pick up where the last
  one stopped, even after compaction or context loss.
- Persistent goals with explicit completion criteria and a bounded budget.
- No silent instruction injection: content the user has not reviewed never
  reaches the model.
- Every activation, trust decision, and edit is recorded where the audit
  trail can see it.

## Non-goals

- Confining what agent shell commands can touch. `run_shell` is not
  sandboxed today and this proposal does not change that (see
  [Security considerations](#security-considerations)).
- Sharing one active file across hosts. Trust is per host and per OS user,
  like `xshelld` itself.
- Summarizing history. Progressive summarization is separate work behind the
  `Compactor` trait; `XSHELL.md` complements it.

## Terms

- **Session root:** the canonical absolute directory a session's `XSHELL.md`
  lives in. Set when the file is activated.
- **Active:** the file's contents are injected into the model's context and
  the agent may update it. At most one file is active per session.
- **Trusted hash:** the SHA-256 of content a person approved, or that xshell
  itself last wrote. A file is eligible for activation without review only if
  its current hash equals its trusted hash.
- **Trust ledger:** the durable record of trusted hashes, keyed by canonical
  absolute path.

## File format

The file is Markdown with a TOML front-matter block between `+++` lines, the
same format as xshell's configuration, parsed with the `toml` crate the
workspace already uses. xshell owns the front matter; the body is free-form
handoff notes.

```markdown
+++
xshell = 1

[goal]
framing = """
Migrate the session store from JSON files to SQLite without losing
existing sessions."""
turns_used = 7
state = "in_progress"
updated = 2026-10-26T18:04:11Z

[goal.ttl]
turns = 30

[[goal.criteria]]
description = "Existing sessions load after upgrade."
check = "cargo test -p xshell-session --test service upgrade"

[[goal.criteria]]
description = "The workspace tests pass."
check = "cargo test --workspace"
+++

## Current state
...

## Next steps
...

## Decisions and notes
...
```

The front matter is optional: a file with no goal is still a handoff file.
Unknown keys, and an `xshell` version newer than this build understands, are
rejected rather than ignored, so a future version's fields are never silently
dropped by an older xshell. Such a file is not activated: xshell names the key
or version it does not understand and offers to set the file aside (see
[Session start](#session-start)).

TOML rather than YAML: the configuration is already TOML, `deny_unknown_fields`
gives the rejection rule for free, and the common Rust YAML crate is archived
and would be flagged by `cargo audit`.

## Session start

When a session starts, xshell looks for `XSHELL.md` in the session's working
directory only. It does not search parent or child directories.

| What xshell finds | What the user is asked |
|---|---|
| A file whose hash matches the trust ledger | Activate it, or set it aside (options below) |
| A file with no ledger entry, or a different hash | Review it before it can be activated (see [Review](#review)) |
| No file | Whether to create one; a created file is active and trusted |

For an existing file, the choices are:

1. **Activate:** read it, inject it, and let the agent update it.
2. **Ignore and back up:** rename it to `XSHELL.md.bak.<UTC timestamp>` and
   continue with no active file.
3. **Start fresh:** back it up as in 2 and create a new, active `XSHELL.md`.

Backups are never overwritten; the timestamp makes each name unique.

Sessions with nobody to answer, such as scripted runs and sessions that
start detached in the daemon, follow `--xshell-md=ask|activate|ignore`,
with the default taken from configuration. In non-interactive use, `ask`
behaves as `ignore`. `activate` never applies to an untrusted file: review
always needs a person.

## Review

An untrusted file is one whose current hash is not the trusted hash for its
path, either because xshell has never seen it (a cloned repository, an
unpacked archive) or because something other than xshell changed it.

Review shows the user the content that will be injected. When the ledger has a
previous trusted version, xshell shows a diff against it instead, since the
diff is what the user needs to judge. The user then approves (the file becomes
trusted at its current hash and is activated) or declines (it stays inactive
and untouched, or is backed up, at the user's choice).

A repository may legitimately ship an `XSHELL.md`, for example with
instructions for a project's tools. Such a file goes through review like any
other on first use in each clone. For most projects, `XSHELL.md` belongs in
`.gitignore`; xshell can offer to add it when it creates the file inside a git
working tree.

## Daemon and remote sessions

The file lives on the host where the session runs, which for a daemon session
may be another machine. The daemon owns the file's state; the client only
presents prompts:

- **Protocol.** New session-protocol requests report the file's status (path,
  current hash, trusted hash, and the review content or diff, fetched from the
  host through the daemon) and carry the user's decision: activate, approve
  after review, set aside, or start fresh. Every decision names the hash it
  was made about. If the file's hash has changed since, the daemon refuses
  the decision and the client shows the new content for review. This bumps
  `SESSION_PROTOCOL_VERSION`.
- **Session state.** The session root, the active path, and its trusted hash
  are part of the session descriptor, so every client sees the same state and
  durable sessions save it.
- **Restore.** When the daemon restores a durable session, or a client
  reattaches, the file's hash is checked again before it is used. A mismatch
  deactivates it and requires review.
- **Detached starts.** A session created with nobody attached follows
  `--xshell-md` as described above.

The logic belongs on the daemon path. The standalone CLI should run the same
code in-process rather than a second implementation, in line with the planned
`SessionBackend` consolidation.

## Trust ledger

The ledger is only as strong as the place it is stored: an attacker who can
rewrite the recorded hashes can make any file look approved.

### Audit-backed (preferred)

When auditing is enabled, trust decisions are audit events, recorded by the
process that makes them:

- `xshell_md_trusted { path, sha256, reason }`, where `reason` is `reviewed`,
  `created`, `written`, or `edited`;
- `xshell_md_revoked { path, sha256, reason }`, for example
  `external_change` or `backed_up`.

Audit logs are per session (`sessions/<id>.jsonl`) and, in a protected
deployment, readable only by the audit service account. `xshelld` therefore
cannot rebuild the ledger by reading the logs itself. Instead, `xshell-auditd`
keeps the current trusted hash for each path as it appends these events, and
answers a new protocol query, `trusted_hash(path)`. `xshelld` asks it on
startup and before each activation. Because the answer is derived from the
hash-chained, signed logs, it can be re-derived and checked by
`xshell-audit-verify`. The index itself is a cache that `xshell-auditd` can
rebuild from the logs at startup.

This adds an audit protocol version and an on-disk format version, like
earlier event additions. It also changes the audit service's role: today
`xshelld` only appends to it, and with this change `xshelld` also asks it
questions. The query answers only with a hash for a path, derived from
records `xshelld` wrote, but the audit service becomes something activation
depends on, and its protocol and threat model documentation must say so.

### Which ledger is authoritative

The ledger is chosen by configuration, not by availability. When auditing is
enabled, the audit-backed ledger is the only one consulted, including with
`required = false`. If the audit service cannot be reached:

- no file is activated without review;
- a review approval activates the file for the current session only, because
  the trust decision cannot be recorded, so the next session reviews again;
- a revocation that cannot be recorded still deactivates the file, which then
  stays untrusted until the audit service records a new trust decision.

xshell never falls back to the local store while auditing is enabled. A local
store that is updated only while the audit service is down would disagree
with it once the service returns: a revocation recorded only locally, such as
an explicit revoke of an unchanged file, would be lost. Requiring review is
the safe failure.

### Local store (no auditing)

Without auditing, `xshelld` keeps the ledger in a store it manages
under its state directory, written atomically and updated with every trust
event. This store is writable by the same account that runs agent shell
commands, so it provides **no tamper resistance**: it prevents accidents and
casual injection, not a determined local attacker. Status output and the docs
say so plainly, as `docs/auditing.md` does for a same-account audit daemon.

## Writing the file

### Agent tool

The agent gets a dedicated tool, `update_handoff`, that replaces the body of
the active file. It cannot touch the front matter; goal state changes go
through `report_goal` (see [Persistent goals](#persistent-goals)). It is not
gated by the approval policy: it can only replace the body of a file the user
activated, and every write is recorded. Each write:

1. takes the per-path writer lock (below);
2. checks that the file on disk still has the trusted hash, reading it through
   the descriptor-bound prepared file access from roadmap M1.3 rather than a
   separate read path. If it does not,
   the write is refused, the file is revoked as `external_change`, and the
   user is told;
3. writes a temporary file in the same directory and renames it into place;
4. records the new hash as trusted (`reason = written`).

A write that goes around the tool, for example a `run_shell` command that
edits `XSHELL.md`, changes the hash without a trust record. The next check
detects it and revokes the file. That is the intended result: only xshell's
own writes keep a file trusted.

### One writer

`xshelld` holds a mutex per canonical path. Two sessions may not both have
the same file active: a second session that starts in the same root is
offered a read-only view of the file, or no file, and is told which session
holds it. Because `xshelld` is per host and per user, the lock lives there;
the standalone CLI takes an advisory lock file next to the ledger.

### `//vixshell`

`//vixshell` is the human editing path, modeled on `visudo`:

1. xshell copies the file to a temporary file on the **host**, the machine
   where the session runs, so the edit happens where the session fabric
   records it, including for remote sessions.
2. It opens the copy in `$VISUAL`, falling back to `$EDITOR`.
3. On save, it validates the front matter, then atomically replaces the file
   and records the new hash as trusted (`reason = edited`). Invalid front
   matter reopens the editor with the error, as `visudo` does.

The temporary copy survives a dropped connection. On reconnect, xshell offers
to resume or discard the unfinished edit.

Run with no active file, `//vixshell` edits (or creates) `XSHELL.md` in the
working directory and then asks whether to activate the result. Content the
user just wrote counts as reviewed.

## Context injection

An active file's contents are added to every provider request as a system
message after the session's own system prompt, delimited and labeled with the
file's path and hash. They are assembled at request time from the content
that passed the hash check and are never stored in the session's history.

Storing the file in history does not work. The history's first message is the
session's own system prompt, which durable sessions save and compaction keeps.
Writing the file there would go stale after every `update_handoff` and would
save file contents into session state. Adding it per request means the model
always sees the current trusted content, compaction cannot remove it, and no
re-injection step is needed. The engine takes it through `TurnPolicy`, as a
context source consulted before each request, so the daemon, which owns the
file for daemon sessions, supplies it.

The engine already assembles each request this way. Since #94,
`run_agent_turn_with_budget` folds execution and recovery records into the
leading `system` message of the request copy, never the stored history, so
records stay where they were written and are compacted away with their turns.
The handoff belongs at the same step, after that merge, so the request still
starts with exactly one `system` message.

The injected content counts against the model's context: the session's
compaction budget is reduced by its size. Above a configurable cap,
`xshell_md.max_bytes`, activation is refused with a message suggesting the
user trim it with `//vixshell`.

The agent is told in its system prompt that the file is its durable memory
for the session root and that it should update it before long or risky steps
and when it finishes a unit of work.

## Changing directories

The session root and everything below it count as inside the session:

- Moving into a subdirectory keeps the root's file active. An `XSHELL.md`
  deeper in the tree is not activated or offered.
- Moving outside the root deactivates the file. xshell tells the user that the
  working directory changed and that the file is no longer active. It does
  not look for, or offer to create, a file in the new directory.
- Moving back inside the root offers to reactivate the file only if its hash
  still matches the trusted hash. If it changed in the meantime, the user is
  warned and the file must go through review before it can be activated.

Each agent `run_shell` call already runs as a separate `sh -c` process started
in the session cwd, so an agent `cd` cannot change the session's directory
today. Only the user moves the session: with `cd`, or with a client request
that sets the working directory (the protocol's `Update` today). The root
check applies to every path that changes the session's cwd. That the agent
cannot move the session must be kept, and a test should pin it.

## Persistent goals

### Commands

- `//goal <framing>` starts a goal. xshell then asks for, or accepts as
  arguments, the completion criteria and a TTL.
- `//goal status` shows the goal, its state, and the TTL used.
- `//goal pause` and `//goal resume` suspend and continue it.
- `//goal confirm` and `//goal reject [note]` settle a `reported_satisfied`
  goal (see [Completion](#completion)).
- `//goal clear` ends it; the final state is kept in the file's history
  section.

A goal requires an active `XSHELL.md`. Starting one without an active file
offers to create one. There is one goal per file.

### States

| State | Meaning |
|---|---|
| `unclear` | The criteria are not specific enough for the agent to work toward on its own |
| `in_progress` | The agent is working toward the goal |
| `paused` | The user suspended the goal; it can be resumed |
| `reported_satisfied` | The agent reports the criteria met, but at least one has no check; waiting for the user to confirm or reject |
| `satisfied` | Every criterion's check passed, or the user confirmed a reported result |
| `unsatisfied` | The goal is clear but has not been met, and work stopped (TTL used up, or the agent reports it cannot proceed) |

When a goal is created, the model is asked once to assess the criteria. If it
finds them insufficient, the goal starts as `unclear`, the agent asks the user
what is missing, and no autonomous work happens until the criteria are
revised.

The agent reports progress with a `report_goal` tool carrying a proposed state
and evidence, for example which criteria it checked and how. xshell applies
only valid transitions, records each one in the front matter and the audit
trail, and moves to `unsatisfied` itself when the TTL runs out.

### Completion

A model's report is a claim, not evidence. This follows the FS5 rule that
prompt claims never satisfy execution predicates. Each criterion may name a
`check` command. When the agent reports `satisfied`:

- xshell runs each criterion's check in the parent turn, under the session's
  approval policy exactly as a `run_shell` call would be, and records each
  result in the audit trail;
- if every criterion has a check and every check passed, the goal becomes
  `satisfied`;
- if a check fails, the goal stays `in_progress` and the agent is told which
  check failed and its output;
- if any criterion has no check, the goal becomes `reported_satisfied` and no
  further turns start until the user runs `//goal confirm` (to `satisfied`) or
  `//goal reject [note]` (back to `in_progress`, with the note given to the
  agent).

### Working toward a goal

Continuing a goal means the daemon starting turns with no client input, which
xshell does not do today. The existing auto-continue mechanism is not a basis
for it: it only nudges a model within one turn, at most twice. While a goal is
`in_progress`, when a turn ends without the goal reaching `satisfied`,
`reported_satisfied`, or `unsatisfied`, the daemon submits a continuation turn
itself:

- **Admission.** A continuation is admitted like any submission: refused while
  another turn is active or during shutdown. Under `ask`, continuations start
  only while a client is attached, since a prompt nobody can answer would only
  stall; otherwise the goal pauses with that reason. Unattended progress needs
  `auto` within the daemon's `max_approval` ceiling.
- **Audit.** A continuation is recorded at the execution boundary as an audit
  `Input` event with route `goal` and the continuation prompt, as user input
  is.
- **Budget.** Each continuation is its own root turn with a fresh
  `TurnBudget` (#84). The real bound on a goal is therefore its TTL in turns
  times the per-turn limits (by default 64 provider requests, 128 tool calls,
  and 60 minutes per turn).
- **Failures.** A failed or budget-exhausted turn counts against the TTL.
  Three consecutive failed turns move the goal to `unsatisfied` rather than
  retrying the same failure until the TTL runs out.

### TTL

The TTL's base unit is **turns**: the number of root agent turns the goal may
consume, counting the turn that created it and every continuation turn the
daemon starts. Auto-continue nudges within one turn are not separate turns. The format leaves room for
wall-clock time (`ttl.wall_clock: 2h`) and estimated cost (`ttl.cost_usd`),
with the goal stopping at whichever limit comes first. Cost requires per-model
pricing in the model profile and usage reporting from the adapters, which
today stream only text, and is a later phase.

## Subagents

`update_handoff` and `report_goal` change the session's durable state, so a
`delegate_task` subagent never receives them, even with `tools = "inherit"`.
They are removed from every subagent toolset, as `delegate_task` itself is. A
subagent does not see the handoff either: it starts from a fresh history by
design, and the parent can put whatever the subagent needs into its task.
Goal checks run in the parent's turn, never a subagent's.

## Audit events

Recorded by the process that performs the action:

- `xshell_md_trusted` and `xshell_md_revoked`, as above;
- `xshell_md_activated` and `xshell_md_deactivated { reason }`, where
  `reason` is `left_root`, `user`, or `session_closed`;
- `xshell_md_backed_up { from, to }`;
- `xshell_md_edit_started` and `xshell_md_edit_finished { outcome }` for
  `//vixshell`;
- `goal_created`, `goal_state_changed { from, to, evidence }`, and
  `goal_ttl_exhausted`;
- `goal_check { criterion, command, outcome }` for each completion check;
- continuation turns as `Input` events with route `goal`.

File contents are not duplicated into the log, only paths and hashes, as for
view operations.

## Configuration

```toml
[xshell_md]
# What to do at session start with nobody to answer: ask, activate, or ignore.
startup = "ask"
# Largest file that may be activated.
max_bytes = 32768
# Offer to add XSHELL.md to .gitignore when creating it in a git working tree.
offer_gitignore = true
```

## Security considerations

- **Injection:** content reaches the model only after a person approved that
  exact hash, or after xshell wrote it. Repositories, archives, and other
  processes cannot introduce instructions without passing review.
- **Ledger strength:** the audit-backed ledger inherits the audit service's
  protection boundary (`docs/auditing.md`). The local fallback is
  same-account and not tamper-resistant.
- **Agent shell access:** `run_shell` is unconfined. An agent can write the
  file directly, which revokes it; with the local fallback, it could also
  rewrite the ledger. Path-confining agent shell commands needs a sandboxing
  provider and is outside this proposal. The ledger store and
  `XSHELL.md.bak.*` should be added to the default sensitive-path patterns so
  `read_file` and `list_directory` access is at least gated.
- **Time of check to time of use:** the hash is checked and the file read
  once, under the writer lock, through M1.3's descriptor-bound prepared file
  access, and the injected content is the content that was hashed, not a
  second read.

## Open questions

1. ~~Should a `satisfied` report need the user's confirmation, or a configured
   check command to pass, rather than the model's word alone?~~ Resolved: a
   criterion's check must pass, or the user must confirm (see
   [Completion](#completion)).
2. When a second session starts in an active root, is a read-only view useful,
   or should it simply see no file?
3. What should `xshell_md.max_bytes` default to, and should it scale with the
   model's `context_tokens`?
4. Should the ledger let a user revoke trust for a path explicitly, for
   example `//vixshell --revoke`?

## Phases

1. File format, startup flow and review (including the session-protocol
   requests), local ledger, per-request injection, `update_handoff`, and
   directory-change handling.
2. Audit-backed ledger: new events, the `xshell-auditd` index and
   `trusted_hash` query, and verifier support.
3. `//vixshell`, including reconnect recovery.
4. `//goal` with turn TTLs, `report_goal`, completion checks, and
   daemon-started continuation turns.
5. Wall-clock and cost TTLs.
