# xshell security finding ledger

**Updated:** 12 November 2026

**Authority:** The evidence and acceptance conditions in the
[design, security, and usability review](xshell-design-security-usability-review.md)
remain authoritative. This page tracks implementation; it does not rewrite the
historical review.

| ID | Tracking | Priority | Package | State | Required regression evidence |
|---|---|---|---|---|---|
| F1 | [#51](https://github.com/rdevaul/xshell/issues/51) | P1 | M1.1 effects and recovery | Corrected | Completed tool effect and terminal failure survive reload and appear in the next request |
| F2 | [#52](https://github.com/rdevaul/xshell/issues/52) | P1 | M1.2 cancellation and shutdown | Corrected (reopened by F17, corrected again) | Agent and captured-shell pipelines/grandchildren cannot write delayed markers after cancellation |
| F3 | [#53](https://github.com/rdevaul/xshell/issues/53) | P1 | M1.2 cancellation and shutdown | Corrected | SIGTERM drains agent, shell, and PTY work before audit closure; forced interruption is recovered as uncertain |
| F4 | [#54](https://github.com/rdevaul/xshell/issues/54) | P1 | M1.3 prepared file access | Corrected | Sensitivity is stable across cwd changes, nesting, aliases, and sensitive ancestors |
| F5 | [#55](https://github.com/rdevaul/xshell/issues/55) | P1 | M1.3 prepared file access | Corrected | Replacement hooks cannot substitute sensitive or outside-root bytes after authorization |
| F6 | [#56](https://github.com/rdevaul/xshell/issues/56) | P1 | M1.4 service bounds | Corrected | A non-acknowledging audit peer cannot freeze unrelated sessions, control, or shutdown |
| F7 | [#57](https://github.com/rdevaul/xshell/issues/57) | P2 | M1.1 effects and recovery | Corrected (incremental writes remain) | Injected write/sync/rename failures leave response, memory, and restarted state consistent |
| F8 | [#58](https://github.com/rdevaul/xshell/issues/58) | P2 | M1.3 prepared file access | Corrected | File and directory acquisition remain bounded before allocation and control stays responsive |
| F9 | [#59](https://github.com/rdevaul/xshell/issues/59) | P2 | M1.4 provider bounds | Corrected | Incomplete/error/refusal streams cannot produce executable tool calls |
| F10 | [#60](https://github.com/rdevaul/xshell/issues/60) | P2 | M1.4 resource bounds | Open (per-turn and daemon-wide budgets landed) | Maximal repeated batches stop at deterministic request, tool, byte, time, and worker budgets |
| F11 | [#61](https://github.com/rdevaul/xshell/issues/61) | P2 | M1.5 directory consistency | Corrected | Files cannot become cwd; invalid updates preserve the previous cwd in every mode |
| F12 | [#128](https://github.com/rdevaul/xshell/issues/128) | P0 | PTY descriptor hygiene | Corrected | A second PTY job, an agent `run_shell` child, and a direct `$` command see no other job's PTY master and cannot type into it |
| F13 | [#129](https://github.com/rdevaul/xshell/issues/129) | P0 | M1.3 prepared file access | Corrected | Case variants (`.ENV`, `.SSH/ID_ED25519`, `server.PEM`, `.GIT/config`) are sensitive, blocked and denied exactly like their stored names on a case-insensitive volume |
| F14 | [#130](https://github.com/rdevaul/xshell/issues/130) | P0 | Audit integrity | Corrected (records after the last checkpoint stay unsigned; per-record signing is #131) | A forged tail after a final checkpoint, duplicate keys and unknown fields fail; a non-round-tripping float verifies; a swapped session file and a rolled-back log (via the index) fail; real format 1, 2 and 9 logs verify |
| F16 | [#132](https://github.com/rdevaul/xshell/issues/132) | P0 | Provider bounds | Corrected | An injected tool name, HTTP error body, or mid-stream error never appears in a `system` message sent to the provider or persisted in history |
| F17 | [#133](https://github.com/rdevaul/xshell/issues/133) | P0 | M1.2 cancellation and shutdown | Corrected (processes that `setsid` away are not contained) | A background job holding the pipes is killed at the timeout, at cancellation (its marker never appears), and after a short grace following a normal exit; the call returns promptly |
| F18 | [#134](https://github.com/rdevaul/xshell/issues/134) | P0 | M1.4 credential bounds | Corrected (allowlist is a constant; configurability is follow-up) | The provider key variable and an arbitrary daemon variable are absent from `run_shell` output |
| F20 | [#136](https://github.com/rdevaul/xshell/issues/136) | P1 | Approval integrity | Corrected | Typeahead before the prompt is discarded on a PTY; "why", "okay" and a `y` with input behind its Enter re-prompt instead of approving; only `y`/`yes` + Enter approves; Enter alone denies |

## Evidence rules

- Keep `review-evidence/review_probe.rs` unchanged as historical reproduction
  source from revision `4ca0839`.
- Corrected behavior belongs in active crate or integration tests named with the
  finding ID in a nearby comment or assertion message.
- A finding becomes **Corrected** only when its original reproduction no longer
  demonstrates the defect and the review's acceptance condition is covered.
- A narrower disposition must document the unsupported mode or guarantee and
  explain why it is not silently downgraded.
- Relevant corrections must pass on supported macOS and Linux targets. Process,
  filesystem, and shutdown corrections require review of failure paths, not
  only successful command execution.

## Planned sequence

1. Complete M0 contracts and the mode matrix.
2. Correct F1, F2, F3, and the independent F11 defect.
3. Implement prepared file operations for F4, F5, and F8. (Done.)
4. Correct audit/provider/resource bounds for F6, F9, and F10. (F6 and F9
   done; F10 partly done.)
5. Complete F7 transactional persistence and crash-durability injection.
   (Done; per-session incremental writes remain.)

F3 has graceful SIGTERM coverage for agent tools, captured shell commands, and
PTYs, including descendant-process cleanup and audit-close ordering. A durable
pre-dispatch marker converts SIGKILL leftovers into an explicit
`outcome_unknown` recovery record. Audit-peer responsiveness is the F6
correction below.

F2 was hardened after its correction (#95). A single group-wide `SIGKILL`
could miss a process whose fork overlapped the signal on macOS, which does not
abort such a fork as Linux does; the F2 captured-shell test caught it on macOS
CI. Cancellation now re-kills the group until it is empty, with a bounded
number of rounds.

F2 was reopened by F17 (#133) and is corrected again. Cancellation and the
timeout raced only the shell leader's exit; once the leader exited, the group
was disarmed and its output pipes were read to EOF with no deadline, so a
background job (`sleep 1000000 &`) held the turn indefinitely, ignored both
timeout and cancellation, and was reported as success, and a cancelled
`(sleep 3; touch marker) & exit 0` still wrote its marker. Both shell paths
(`run_shell` and captured `$` commands) now share one supervisor in
`xshell-execution/src/process.rs` that keeps the group armed until the pipes
close and puts the drain inside the same deadline-and-cancellation `select!`.
On timeout or cancellation it kills and sweeps the group and abandons the
readers instead of awaiting EOF. After a normal exit, a job still holding the
pipes gets a 2-second grace (bounded by the remaining timeout) and is then
killed; the result keeps the exit status and output and says background
processes were killed. A process that leaves the group with `setsid` cannot be
killed by the group kill; it is no longer waited for, but it can keep
running. Evidence: the `f17_*` tests in `tools.rs` and `engine.rs`, including
a `setsid` pipe holder that no longer extends the timeout.

F18 (#134) is corrected. `run_shell` commands no longer inherit the daemon's
environment, which held the provider key named by `api_key_env`. They get an
allowlist (`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`,
`LC_*`, `TERM`, `COLORTERM`, `TMPDIR`, `TZ`), and every registered credential
variable is removed even if allowlisted: the session model's `api_key_env`,
every model profile's in the loaded configuration, and any key an adapter is
built with. User-typed `$` commands keep the daemon's environment but lose the
credential variables too. A shell's own startup files (zsh's `~/.zshenv`) can
still export variables. Evidence:
`f18_run_shell_omits_the_provider_key_and_daemon_variables`. The exact
environment is documented in [session fabric](session-fabric.md#shell-commands-lifetime-and-environment).

F20 (#136) is corrected. The interactive approval prompt read raw bytes and
took the first `y`, `n`, `q` or Enter, skipping everything else, and never
discarded pending input, so "why", "okay" or "sorry" typed while the agent
worked approved the next call at its first `y`. Every prompt that authorizes
an effect (the raw-mode tool approval, the line-mode tool approval used
without a full terminal, and the `//connect` remote-repair prompt) now
discards pending terminal input (`tcflush(TCIFLUSH)`) before it is drawn,
waits 250 ms, and discards again, so keys in flight do not count. In raw mode
approval needs `y` (or `yes`) followed by Enter as the last byte read; `n`,
`q`, Ctrl-C, Ctrl-D and Enter alone answer at once, because they cannot
authorize anything, and Enter alone keeps its documented default, deny. Any
other byte, including `y` followed by anything but Enter, discards pending
input and re-prompts. Backspace edits a partial `yes`. Line-mode prompts
already required a whole `y`/`yes` line; they gain the flushes. Piped stdin
is not a terminal and is read as before. Evidence: the `f20_*` tests in
`xshell-cli/src/approval_keys.rs` and `xshell-pty/src/lib.rs`, the latter on a
real PTY.

F9 is corrected. Both adapters hand tool calls to the engine only from a
response the provider finished normally: the OpenAI-compatible adapter needs
`[DONE]` or a `finish_reason`, and the Ollama adapter needs the final
`done: true` chunk. A response cut off by the output limit, stopped by a
content filter, or finished for an unknown reason dispatches nothing, and so
does one carrying an error mid-stream, a second choice, an unnamed call, or
unparseable arguments. Adapter tests cover each case, and a daemon test shows
a complete-looking `run_shell` call from a stream that ends early never runs.
Text-only responses are not held to this rule because they cannot cause
effects.

F7 is corrected for the session registry. Every mutating operation applies
its change, writes `sessions.json`, and puts the registry back as it was if
the write fails, so the caller's error, the live registry, and a restarted
daemon agree. The write is a synced temporary file, an atomic rename, and an
`fsync` of the directory; a failure to sync the directory after the rename is
reported but does not undo the change, because the disk already holds it.
Changes to sessions that are not durable write nothing. Tests inject write,
sync, and rename failures into create, update, close, and the end of an
execution. Only metadata is rolled back: effects outside the registry are
applied after a successful commit (image removal) or covered by the
in-flight marker (turns). Every persistence pass still writes all durable
sessions; per-session incremental writes remain.

F10 is partly corrected (#84): every agent turn, including its subagents,
draws from one budget of provider requests, tool calls, tool calls per
response, and elapsed time, and a maximal repeated batch stops at a
deterministic limit. The daemon also bounds turns running at once
(`max_concurrent_turns`, which bounds turn workers) and sessions kept
(`max_sessions`), so detached work cannot accumulate without bound, and each
turn bounds the bytes of tool output it returns to the model
(`tool_output_bytes`). A session's budget across turns and a bound on retained
history without compaction remain, so F10 stays open.

F12 is corrected (#128). It was found in the 12 November 2026 security
review of `03dae9a`, not the original review, so its acceptance condition is
the one in the issue. `openpty(3)` set close-on-exec on neither PTY
descriptor, and `xshelld` keeps every interactive job's master open, so every
later child (other sessions' PTY jobs, `run_shell`, `$` turns) inherited every
live master and could type into, or read, another session's terminal. Two
layers now keep descriptors out of children:

- **Close-on-exec at allocation.** `xshell-pty` opens the master with
  `posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC)` and the slave with
  `open(O_CLOEXEC)`, so neither is ever inheritable, even by a fork on another
  thread. Both macOS and Linux honour the flag; a libc that rejected it would
  get the flag set by `fcntl` straight after, a window the second layer covers.
  The slave still becomes the child's stdio, because `dup2` clears
  close-on-exec on descriptors 0 to 2.
- **A sweep in every shell child.** PTY jobs, `run_shell`, direct `$`
  commands, and the SSH helpers run
  `xshell_platform::descriptors::child_hook` between `fork` and `exec`. It
  marks every descriptor above 2 close-on-exec: `close_range(2)` with
  `CLOSE_RANGE_CLOEXEC` on Linux 5.11 and later, the `proc_pidinfo` listing
  into a buffer allocated before the fork on macOS, and otherwise an `fcntl`
  probe of every number below the soft descriptor limit. It marks rather
  than closes so the standard library's exec-error pipe keeps working. This
  also keeps the headless `--approval-channel` descriptor out of agent
  commands. No xshell child is meant to inherit a descriptor above 2.

Evidence: `f12_a_second_pty_job_cannot_reach_the_first_jobs_master` runs two
PTY jobs; the second looks for the first's master by number, lists any
terminal above its stdio, and writes into every descriptor it holds, and the
first job then reads only the line the test sends. Without the correction it
finds the master and the first job reads `INJECTED`.
`f12_a_run_shell_child_cannot_see_a_live_pty_master` and
`f12_a_direct_shell_command_cannot_see_a_live_pty_master` hold a master opened
without close-on-exec, as before the fix, and show the sweep keeps it out of
both captured-shell paths. Unit tests in `xshell-platform` cover the sweep and
its fallback, and `f12_pty_master_and_slave_are_close_on_exec` checks the
allocation.

F4, F5, and F8 are corrected together by M1.3, a single prepared-operation
boundary in `xshell-execution` (`prepared.rs`), because they shared one cause:
authorization and acquisition resolved the path independently.

- **F5.** A read-only tool call is prepared before its approval decision: the
  path is walked from the tool root one component at a time with `openat` and
  `O_NOFOLLOW`, symlinks are resolved by the walk and must stay inside the
  root, and the walk ends holding a descriptor. The decision is about that
  object and the read uses that descriptor. An engine test swaps the file, and
  separately an ancestor directory, between the decision and the read; the
  prepared object is still what is returned. With re-resolution the same test
  returns the secret. Approval binds the object, not its bytes: writes to the
  same file can still change them, and a hard link to a credential under an
  innocent name is classified by that name.
- **F4.** Sensitivity is decided from the object's canonical absolute path.
  Patterns containing `/` match at any depth and a leading `/` anchors one, so
  the same file is classified the same from every cwd, nested repositories'
  `.git/config` and everything under `.aws` are covered, and a project inside
  a sensitive directory stays sensitive. This guarantee was incomplete on
  case-insensitive volumes until F13: the canonical path kept the names as
  spelled, so `.ENV` opened `.env` but matched no pattern.
- **F13.** On a case-insensitive volume (the macOS APFS default) a name in
  any case opens the stored object, and APFS also folds `ſ`, the Kelvin sign,
  `ß` and ligatures such as `ﬁ` to ASCII. The prepared path now records the
  names the filesystem stores where the platform reports them (macOS
  `F_GETPATH`, checked against the open object; a file with several hard
  links keeps the spelled name), and sensitive, `block` and policy `denied`
  path patterns are matched case-folded on every platform, which can only
  classify more paths as sensitive. Policy `always_allowed` and `allowed`
  path patterns match exactly, so folding never widens a grant; on macOS
  they see the stored names, and elsewhere a differently cased request is
  unlisted, which is stricter. Evidence:
  `f13_matching_ignores_case_and_folds_like_a_case_insensitive_volume`,
  `f13_path_denials_ignore_case_and_allows_do_not`,
  `f13_case_variants_classify_like_the_stored_name`,
  `f13_unicode_foldings_classify_like_the_stored_name` and
  `f13_block_prompt_and_policy_denials_hold_under_any_case`. The filesystem
  tests skip, with a message, on a case-sensitive temporary directory.
- **F8.** Reads take at most `max_bytes + 1` bytes from the descriptor (tested
  against a 1 GiB sparse file), listings stop reading entries after their
  limit, and preparation and reads run on a blocking worker with a deadline so
  a stalled filesystem returns an error instead of holding the turn.

F6 is corrected. A hung audit service now delays only the session whose
record it is withholding, and only until a deadline:

- **Deadlines.** Every exchange with the audit service (open, each append and
  its acknowledgement, close) must finish within `audit.ack_timeout_ms`
  (default 5 seconds). The deadline covers the whole exchange, so a peer that
  trickles bytes cannot stretch it. It applies to `xshelld`, its startup
  probe, and the CLI's own audit connection.
- **No global lock across I/O.** Each xshell session has its own audit
  connection behind its own lock. The shared map is locked only to find or
  insert an entry.
- **Fail closed, then recover.** A missed deadline marks the connection
  unusable, because the record may or may not have been written. With
  `required = true` the audited action is refused, and later records fail at
  once rather than waiting again. After one deadline interval the next record
  opens a new connection, recorded as `LogicalSessionAttached` with action
  `xshelld_execution_resumed`. Records between the failure and the resumption
  were not acknowledged; with `required = true` their actions did not run.
- **Bounded shutdown.** Sessions close in parallel, each within its
  deadlines, so a hung service delays shutdown by about one session's close
  rather than hanging it.

Evidence: `f6_a_hung_audit_peer_cannot_freeze_other_sessions_control_or_shutdown`
runs a real `xshelld` against a fake audit service that withholds one
session's acknowledgement. Another session runs a command and the controller
lists sessions while the first waits. The stalled command is refused at its
deadline and never runs, and with the service hung entirely SIGTERM still
exits cleanly. Without the correction, the other session freezes behind the
stalled one. Unit tests in `xshell-session/src/audit.rs` cover the latch and
resumption, optional audit, closing one session, and shutdown with eight
sessions taking one deadline rather than eight.

The review also asks for deadlines on the CLI's own connection to the session
service (local socket and SSH). That is client-side hardening outside F6's
acceptance condition and is not part of this correction.

F16 is corrected (#132). An execution record written after a provider
failure is a `system` message, and it is folded into the system prompt of
every later request. It now states only a fixed class written by xshell (an
HTTP status number, "stream error", "invalid tool arguments", "context
overflow", and so on; `AdapterError::record_summary`), never the provider's
error body, a mid-stream error message, or a tool name the model chose. The
full error still reaches the user and the audit log. Records saved before the
fix lose their quoted detail when merged. Tests: `f16_*` in
`xshell-execution/src/engine.rs` and `xshell-core/src/lib.rs`.

F14 is corrected (#130). Audit format 10 (protocol 11) hashes and signs the
bytes a log stores: the daemon serializes each record and checkpoint body once,
hashes those bytes, and writes them unchanged, and the verifier hashes the
stored body before parsing it. A float that `serde_json` does not reparse
exactly, a different key order, or a different escape can no longer break a
genuine log or hide a change. Every line is parsed strictly, so a repeated key
at any depth (`"command":"ls","command":"rm -rf ~"`) or an unknown field fails.
Any entry after a final checkpoint fails, and the report counts the records
after the last checkpoint, which only the unkeyed chain covers. The verifier
binds a log to its session through the file name or `--session-id`, and
`--index checkpoints.jsonl` detects a log rolled back or replaced behind the
index's latest checkpoint. Formats 1 to 9 are still verified by re-encoding
and reported as `legacy-reencoded`; formats 1 and 2 are re-encoded without
`logical_session_attached.terminal_stream`, which they never wrote, so they
verify again, and float parsing is now exact (`float_roundtrip`), so a format 9
log with such a float verifies too. The verifier still exits non-zero for a
log without a final checkpoint unless `--allow-open` is given. Deleting a
whole session file is visible only by comparing the index with the sessions
directory, and signing each record is tracked in #131. Evidence: the `f14_*`
tests in `xshell-audit/src/verify.rs` and `xshell-audit/tests/verify_cli.rs`,
including fixtures written by the format 1, 2 and 9 writers.

The [development roadmap](xshell-development-roadmap.md) remains the
cross-product sequencing authority.
