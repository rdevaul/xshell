# xshell execution mode conformance

**Status:** M0 baseline; unsupported and unverified cells are explicit

**Updated:** 13 September 2026

This matrix identifies which shared lifecycle guarantees each current mode
must satisfy. It is not a claim that all guarantees are implemented today.

| Mode | Authority location | Persistence | Cancellation owner | Required audit behavior | Current conformance |
|---|---|---|---|---|---|
| Standalone agent | CLI process, ambient user | Process-local conversation | CLI turn observer | Optional local audit only | F1/F2 lifecycle corrections implemented; broader profile conformance pending |
| Local daemon agent | Host daemon, ambient user | Ephemeral, daemon, or durable session | Execution coordinator | Daemon execution boundary | F1/F2/F3 corrected; F6/F7 remain |
| SSH daemon agent | Remote host daemon, ambient remote user | Remote daemon/durable session | Remote execution coordinator | Remote daemon boundary | Local-daemon guarantees plus bounded control transport |
| Captured direct shell | CLI or daemon process | Cwd and bounded output according to session mode | Owning turn/coordinator | Start, output policy, and terminal outcome | F2/F3/F11 corrected |
| Agent shell tool | Shared execution engine | Tool facts projected into conversation/session | Owning agent turn | Request, decision, result, and terminal turn outcome | F1/F2 corrected |
| Interactive PTY | Host daemon PTY coordinator | Process may outlive attachment; bounded replay is optional | PTY coordinator | Lifecycle always; stream capture by explicit bounded policy | F3 shutdown and forced-recovery markers implemented |

## Common lifecycle assertions

Every supported mode must demonstrate:

1. accepted work has a stable identity and duplicate active submission is
   rejected;
2. cancellation changes state to stopping until owned cleanup finishes;
3. successful termination, confirmed cancellation, failure, and unknown outcome
   are distinguishable;
4. completed or uncertain effects are not erased by provider, controller,
   storage, or audit failure;
5. process groups owned by captured commands are killed and reaped on
   cancellation, timeout, error, and graceful daemon shutdown;
6. graceful shutdown stops admission before draining work and closes audit only
   after terminal lifecycle resolution;
7. optional audit configuration never disables operational recovery facts;
8. required audit failure prevents new effects and reaches a bounded visible
   state rather than freezing the control plane;
9. cwd changes use the same accessible-directory validation boundary;
10. a profile may strengthen authorization or provider requirements but never
    weaken these assertions.

## Shell semantics

Captured shell commands are independent processes, not a persistent POSIX shell
environment. A command containing shell operators is executed by the selected
shell and is not interpreted as the special `cd` operation. Only an exactly
parsed `cd` command with zero or one argument updates session cwd. Environment
changes such as `export` do not persist into later captured commands.
Durable restore rejects a session whose recorded cwd is missing, inaccessible,
or not a directory; it does not silently substitute another directory.

## Test map

| Assertion | Existing evidence | Required addition |
|---|---|---|
| Duplicate active submission | `rejected_duplicate_submit_is_not_recorded_as_accepted_input` | Preserve across lifecycle refactors |
| Detach and reconnect | session service replay and switching tests | Add lost-acknowledgement identity coverage in M3 |
| Agent effect recovery | Engine and durable-session restart regressions tagged F1 | Add storage-failure coverage under F7 |
| Agent/captured-shell cancellation | Pipeline and grandchild delayed-marker regressions tagged F2 | Repeat on supported macOS/Linux targets |
| Daemon shutdown | SIGTERM all-mode, audit-ordering, and SIGKILL recovery tests tagged F3 | Repeat on supported macOS/Linux targets |
| PTY termination | PTY coordinator and daemon SIGTERM integration tests | Bound audit-stall behavior under F6 |
| Required audit failure | Existing reservation/audit failure test | Add non-acknowledging peer deadline test for F6 |
| Cwd validation | Shared engine/CLI/registry tests tagged F11 | Repeat mode matrix on supported targets |

The controlled deployment adds authorization, data-domain, provider, and
assurance conformance beyond this shared baseline. The personal profile may use
ambient authority but remains subject to every lifecycle assertion above.
