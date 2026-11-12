# xshell security finding ledger

**Updated:** 2 November 2026

**Authority:** The evidence and acceptance conditions in the
[design, security, and usability review](xshell-design-security-usability-review.md)
remain authoritative. This page tracks implementation; it does not rewrite the
historical review.

| ID | Tracking | Priority | Package | State | Required regression evidence |
|---|---|---|---|---|---|
| F1 | [#51](https://github.com/rdevaul/xshell/issues/51) | P1 | M1.1 effects and recovery | Corrected | Completed tool effect and terminal failure survive reload and appear in the next request |
| F2 | [#52](https://github.com/rdevaul/xshell/issues/52) | P1 | M1.2 cancellation and shutdown | Corrected | Agent and captured-shell pipelines/grandchildren cannot write delayed markers after cancellation |
| F3 | [#53](https://github.com/rdevaul/xshell/issues/53) | P1 | M1.2 cancellation and shutdown | Corrected | SIGTERM drains agent, shell, and PTY work before audit closure; forced interruption is recovered as uncertain |
| F4 | [#54](https://github.com/rdevaul/xshell/issues/54) | P1 | M1.3 prepared file access | Corrected | Sensitivity is stable across cwd changes, nesting, aliases, and sensitive ancestors |
| F5 | [#55](https://github.com/rdevaul/xshell/issues/55) | P1 | M1.3 prepared file access | Corrected | Replacement hooks cannot substitute sensitive or outside-root bytes after authorization |
| F6 | [#56](https://github.com/rdevaul/xshell/issues/56) | P1 | M1.4 service bounds | Corrected | A non-acknowledging audit peer cannot freeze unrelated sessions, control, or shutdown |
| F7 | [#57](https://github.com/rdevaul/xshell/issues/57) | P2 | M1.1 effects and recovery | Open | Injected write/sync/rename failures leave response, memory, and restarted state consistent |
| F8 | [#58](https://github.com/rdevaul/xshell/issues/58) | P2 | M1.3 prepared file access | Corrected | File and directory acquisition remain bounded before allocation and control stays responsive |
| F9 | [#59](https://github.com/rdevaul/xshell/issues/59) | P2 | M1.4 provider bounds | Corrected | Incomplete/error/refusal streams cannot produce executable tool calls |
| F10 | [#60](https://github.com/rdevaul/xshell/issues/60) | P2 | M1.4 resource bounds | Open (per-turn and daemon-wide budgets landed) | Maximal repeated batches stop at deterministic request, tool, byte, time, and worker budgets |
| F11 | [#61](https://github.com/rdevaul/xshell/issues/61) | P2 | M1.5 directory consistency | Corrected | Files cannot become cwd; invalid updates preserve the previous cwd in every mode |

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

F10 is partly corrected (#84): every agent turn, including its subagents,
draws from one budget of provider requests, tool calls, tool calls per
response, and elapsed time, and a maximal repeated batch stops at a
deterministic limit. The daemon also bounds turns running at once
(`max_concurrent_turns`, which bounds turn workers) and sessions kept
(`max_sessions`), so detached work cannot accumulate without bound, and each
turn bounds the bytes of tool output it returns to the model
(`tool_output_bytes`). A session's budget across turns and a bound on retained
history without compaction remain, so F10 stays open.

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
  a sensitive directory stays sensitive.
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

The [development roadmap](xshell-development-roadmap.md) remains the
cross-product sequencing authority.
