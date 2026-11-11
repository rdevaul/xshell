# Subagents: design and staged plan

Status: steps 1–3 **implemented** — per-agent toolsets and scoped tool-call
identities (#83), the shared whole-turn budget (#84), and the blocking
`delegate_task` tool (#86). Step 4, child sessions, is design only and waits
on protocol version negotiation and the M2 authorization contract.

This document assesses what the xshell harness needs in order to create and
manage subagents — agents started by another agent to do a bounded piece of
work — and proposes a staged path that converges with FutureShell FS5
(agent tasks) rather than inventing a second agent-lifecycle model.

## Goals and non-goals

Goals:

- An agent can hand a bounded task to a child agent and receive its result.
- A child never has more authority than its parent: same or narrower tools,
  same or stricter approval, same sensitive-path policy, cwd inside the
  parent's policy root.
- Every child action is attributable in events and in the audit trail, and
  approvals for child actions reach the human attached to the parent.
- Cancellation, failure recording, and recovery keep the guarantees M1
  established for ordinary turns (D2: core correctness is unconditional).
- Total work per user request stays bounded no matter how agents nest.

Non-goals for this plan:

- Cross-host subagents (a daemon acting as a client of another daemon).
  That belongs to FS6.
- External agent runtimes (Hermes, Codex, …). Those arrive through the FS5
  connector contract; this design should become its `xshell-session`
  connector, not a competitor to it.
- Token or cost budgets. Adapters do not report usage yet; see
  [Budgets](#budgets).

## What the harness already provides

| Existing piece | Where | Why it matters for subagents |
|---|---|---|
| One shared agent loop | `run_agent_turn`, `crates/xshell-execution/src/engine.rs` | A subagent is the same call with its own history, adapter, and policy; no second loop |
| `TurnObserver` trait | `engine.rs` | A child observer can wrap the parent's to tag events and forward approvals |
| `CancellationFlag` | `engine.rs` | Checked before every provider request and around every tool; sharing the parent's flag cascades cancel |
| Approvals keyed by `(turn_id, call_id)`, pending set | `crates/xshell-session/src/execution.rs` | The daemon can already hold several pending approvals |
| Bounded tool output (64 KiB) | `crates/xshell-execution/src/tools.rs` | A child's final answer cannot flood the parent context |
| Sessions with model, cwd, history, journal, audit stream, persistence, detach | `xshell-session` | Almost everything a background agent needs |
| `ApprovalPolicy::clamp_to` | `engine.rs` | The pattern for "a child may ask for less, never more" |

## Gaps

1. **No tool registry.** Tools were a fixed list (`definitions()`), a fixed
   dispatch `match`, and name-based gating. A subagent needs a per-agent
   toolset: a read-only child, a child without the spawn tool (depth cap),
   and the spawn tool itself. *Addressed by step 1.*
2. **Tool-call identities collide.** The Ollama adapter synthesizes
   `ollama-call-{n}`, restarting at 0 for every response; the OpenAI adapter
   falls back to `openai-call-{n}` when a server omits ids. Within one turn
   the same id already appears more than once in events and audit records,
   and a parent and child would contend for the same approval key.
   *Addressed by step 1.*
3. **The engine cannot build a child adapter.** It receives a constructed
   adapter; a child that uses another model profile needs a resolver from
   profile name to binding, resolved where credentials live (the daemon in
   fabric mode). *Open:* step 3 reuses the parent's adapter, so a subagent
   always runs on the parent's model.
4. **Events carry no agent identity.** `ExecutionEvent`, `SessionEventKind`,
   and `AuditEvent` need an agent path. The audit format must keep adding
   fields with `skip_serializing_if` so earlier records re-hash unchanged.
   *Addressed by step 3* with a wrapper event and bracketing audit records
   rather than a per-event field; see
   [Events, audit, and protocol](#events-audit-and-protocol).
5. **Budgets multiply.** Each `run_agent_turn` permitted 64 steps; nesting
   made that 64 × 64. *Addressed by step 2.*
6. **Sequential tool execution.** Parallel children need concurrent dispatch
   of calls within one response and an approval queue in the CLI that names
   the requesting agent. *Open.*
7. **Recovery is per session.** If the daemon dies mid-turn, the in-flight
   marker names only the parent operation; recovery text must say a child
   was running and what it was allowed to do. *Open:* a subagent runs inside
   its parent's operation, so recovery reports the parent's operation as
   `outcome_unknown` without naming the subagent.
8. **Two execution paths.** Standalone (`LocalObserver`) and daemon
   (`DaemonObserver` plus the CLI follower) each render, audit, and approve.
   Every subagent UI change is made twice until the standalone path is
   collapsed into the daemon path behind a `SessionBackend` trait (see the
   2026-11-05 review). *Open:* step 3 was implemented in both paths.
9. **Protocol lockstep.** `SESSION_PROTOCOL_VERSION` must match exactly, so
   new event variants force simultaneous upgrades on all connected hosts.
   *Open:* step 3 moved the protocol to version 14; version-range
   negotiation should land before step 4.

## Design

### Authority model

A child's authority is the intersection of its parent's authority and its
request:

| Dimension | Rule |
|---|---|
| Tools | `child.tools ⊆ parent.tools`; the spawn tool is removed unless depth allows another level |
| Approval | `child.approval = requested.clamp_to(parent.approval)` |
| Sensitive paths | Inherited unchanged; a child cannot remove patterns |
| Working directory | The parent's cwd. Read-only tools are confined beneath it by M1.3's prepared walk, and sensitivity is judged on absolute paths, so a child sees exactly the parent's file policy |
| Model | Any profile the parent's daemon could resolve; credentials never cross the protocol |
| Budget | Drawn from the root turn's shared budget, never a fresh one |

Under the M2 authorization contract, "actor" in the operation request becomes
the agent path (`root/delegate-1/...`), so a child's request is authorized by
the same interface as any other dispatch (D3).

### Tool-call identity (step 1)

Provider-issued ids stay untouched in conversation history, because some
providers validate the ids they get back (length or alphabet limits). The
engine derives a separate **scoped id** that is used everywhere outside the
provider conversation: execution events, approval prompts and replies, and
audit records.

- Within a turn, the first occurrence of a provider id keeps it; a repeat
  becomes `id~2`, `id~3`, …; an empty id becomes `call~N`.
- An agent with a scope (`TurnPolicy::agent_scope`) prefixes every scoped id
  with `scope/`, so ids from different agents in one root turn never
  collide. The root agent has no scope, so single-agent ids are unchanged
  unless they repeat.

The scoped id is the only id a client or auditor sees, so it is the approval
key and the audit correlation key. The provider id appears only in history.

### Toolsets (step 1)

`ToolSet` is a small value type over a closed `Tool` enum (`read_file`,
`list_directory`, `run_shell`, and since step 3 `delegate_task`).
`TurnPolicy::tools` selects which tools are **advertised** to the model and
which are **dispatchable**. A call to a tool outside the set — including a
hallucinated tool name — is answered with a tool error without approval or
execution, and does not count as observed tool activity, so a turn containing
only such calls can still be rolled back. The default set is every tool, so
since step 3 every agent is offered `delegate_task`; `ToolSet::read_only()`
holds only `read_file` and `list_directory`.

The enum stays closed on purpose: adding the delegate tool in step 3 was one
variant, and keeping dispatch exhaustive lets the compiler find every place
that must handle it. A dynamic registry can replace it when FS5 connectors
need runtime-registered tools. Further agent tools, such as `update_handoff`
and `report_goal` from the [XSHELL.md proposal](xshell-md-continuity.md),
must be removed from every subagent toolset the way `delegate_task` is.

### Delegation (step 3): blocking child turns

A `delegate_task` tool:

```json
{"task": "…", "tools": "read_only" | "inherit"}
```

runs a nested turn (`run_agent_turn_with_budget`, boxed because it recurses)
with:

| Aspect | Child receives |
|---|---|
| History | A fresh one: the default system prompt plus subagent instructions, then `task` as its user message |
| Model | The parent's adapter, reused; the parent is not using it while a tool runs |
| Tools | `read_only` (the default): `read_file`, `list_directory`. `inherit`: the parent's tools. Either is intersected with the parent's set and loses `delegate_task`, so depth is 1 |
| Approval, sensitive paths, compaction | The parent's, unchanged |
| Budget and cancellation | The root turn's, shared |
| Scope | `<parent-scope>/delegate-<n>`, so its call ids are `delegate-<n>/<id>` |

A `SubagentObserver` wraps every child event in `ExecutionEvent::Subagent`
for the parent observer and forwards approvals to it, so the human attached
to the parent decides them under the child's scoped call id. The parent
observer brackets the child with `SubagentStarted` and `SubagentFinished`.
The child's final assistant message is the tool result (truncated like any
tool output); its transcript reaches the parent only as events and audit
records, never the parent's history.

**Gating.** `delegate_task` is not gated itself. The child can never exceed
the parent's authority (its tools are a subset, its approval policy is the
parent's), so every gated action the child attempts is approved on its own,
exactly as if the parent had attempted it. Under `off`, a child's shell calls
are denied just as the parent's would be. A start-of-delegation prompt would
add a decision without adding protection.

**Outcomes.** The parent records the delegation as executed tool activity, so
its turn is never rolled back over a child's effects. The tool result states
the outcome: the answer when `completed`, otherwise a sentence for
`budget_exhausted`, `cancelled`, `failed`, or `aborted`. When the user answers
`q` to one of the child's prompts, the child stops and the parent's turn is
aborted too, with its remaining calls skipped, as for one of its own tools.
Budget exhaustion in the child also leaves the parent unable to continue,
since the budget is shared.

**Rendering and audit.** Both CLI paths tag child output with
`[delegate-<n>]`, show the child's tool requests, prompts, and results, and do
not stream the child's text, whose answer arrives as the tool result. The
audit trail gains `SubagentStarted` and `SubagentFinished` (audit format 5,
audit protocol 6); every record between them belongs to the child, and its
tool-call ids carry the scope. The session protocol is version 14.

**Deferred.** Choosing a different model for the child (a `model_profile`
argument) needs a profile resolver on the daemon side, where credentials
live; parallel children need concurrent dispatch within one response.

### Child sessions (step 4)

For background or persistent agents, a child is a daemon session with
`parent: {session_id, turn_id, call_id}`. Parent tools `spawn_agent`,
`send_to_agent`, `wait_agent`, `list_agents`, and `cancel_agent` map onto
existing coordinator operations (create, submit, events, cancel). This
reuses detach, replay, per-child audit streams, and durability. New work:
generated child names, ephemeral children closed with their parent,
`//sessions` grouping, approval routing to the parent's controller, parent
link records in the audit trail, and budgets across parent and children.
This is designed as FS5's `xshell-session` connector so its lifecycle
(one-shot, managed, persistent) follows the FS5 vocabulary.

### Budgets

A root turn owns one budget, shared by reference with every descendant:
model rounds, tool dispatches, child starts, and wall time.

Step 2 implements this as `TurnLimits` (on `TurnPolicy`) and `TurnBudget`
(clones share atomic counters) in `xshell-execution`:

| Limit | Default | Counts | On exhaustion |
|---|---|---|---|
| `provider_requests` | 128 | Every provider request, including auto-continues and the overflow retry | Turn fails before the request |
| `tool_calls` | 256 | Every evaluated call: approved, denied, or refused by toolset | This and the remaining calls in the response are answered with a refusal; turn fails |
| `tool_calls_per_response` | 64 | Calls in one response | Calls past the limit are refused; the turn continues so the model can resend |
| `wall_time` | 60 min | Elapsed time since the root turn started | Checked before every request and call, and enforced during provider requests; a running tool is bounded by its own timeout |
| `shell_timeout` | 60 s | One `run_shell` call | The command's process group is killed and the call reports the timeout |
| `auto_continues` | 2 | Automatic continuations per agent | The turn ends with the response it has |
| `tool_output_bytes` | 8 MiB | Bytes of tool results returned to the model | The crossing result is kept; the turn fails at its next request or call |

The defaults are the built-in `default` profile; limit profiles are
configurable, and the daemon clamps them to its `max_limits`
([session fabric](session-fabric.md#turn-budget)).

Exhaustion fails the turn with a typed `BudgetExhausted` error and follows the
usual failure rules: rolled back if no tool ran, otherwise kept with an
execution record. `run_agent_turn` builds a fresh budget from the policy;
`run_agent_turn_with_budget` lets a child draw from its root's budget and
ignores the child policy's own limits. The 64-request default preserves the
former 64-step limit.

Exhaustion inside a subagent is reported as the subagent's
`budget_exhausted` outcome, and the parent then fails on its own next charge
because the budget is shared. Per-session and global budgets from F10 (#60)
remain open. Token and cost limits wait for usage reporting from adapters
(`AgentEvent` carries only text deltas today); until then budgets count
requests, calls, and time, never cost.

### Events, audit, and protocol

As implemented in step 3:

- `ExecutionEvent` gained `SubagentStarted { agent, task, tools }`,
  `Subagent { agent, event }` (a wrapper around every event the subagent
  emits), and `SubagentFinished { agent, outcome, message }`, with
  `innermost()` / `into_innermost()` to unwrap. A wrapper was chosen over an
  optional agent field on every variant so existing variants and their
  consumers stay unchanged.
- The daemon registers a subagent's approval from the innermost
  `ApprovalRequested`, under the subagent's scoped call id, so the existing
  `approve` request works unchanged.
- Both CLI paths, and the session redraw on switch (`backlog.rs`), tag
  subagent output with `[delegate-<n>]` and leave the subagent's streamed text
  to arrive as the tool result.
- `AuditEvent` gained `SubagentStarted` and `SubagentFinished`. Turns run
  sequentially, so every record between them belongs to the subagent, and
  its tool-call ids also carry the scope.
- Versions: session protocol 14, audit protocol 6, audit format 5.

Step 4 should add protocol version-range negotiation first, as the review
recommends, so a child-session protocol does not force every connected host
to upgrade at once.

## Staged plan

| Step | Deliverable | Depends on | Exit evidence | Status |
|---|---|---|---|---|
| 1 | `ToolSet` on `TurnPolicy`; scoped tool-call ids in events/approvals/audit | — | Unoffered and unknown tools are refused without approval or execution; repeated provider ids get distinct scoped ids; scoped approvals round-trip; history keeps provider ids | Done (#83) |
| 2 | Whole-turn shared budget (part of M1.4) | — | Nested and sequential steps draw from one budget; exhaustion reported truthfully | Done (#84) |
| 3 | `delegate_task` blocking child turn, both execution paths | 1, 2; ideally the `SessionBackend` collapse | Child cannot exceed parent tools/approval/paths; cancel cascades; approvals reach the parent's human; child transcript audited; depth 1 enforced | Done (#86), without the `SessionBackend` collapse |
| 4 | Child sessions as the FS5 `xshell-session` connector | 3, protocol version range, M2 operation contract | Detached children survive controller disconnect; closing a parent closes ephemeral children; recovery names interrupted children | Planned |

## Open questions

- Should a child's transcript be retained in operational history (visible
  via `//history`) or only in the audit trail?
- Should approval of `delegate_task` under `ask` pre-authorize some of the
  child's read-only work, or should every child gate apply independently?
  (Step 3 answers: delegation is not gated, and every child gate applies
  independently.)
- What is the default per-root-turn budget for personal profiles, and is it
  user-configurable per model profile? (Step 2 set fixed defaults; they are
  not configurable yet.)
