# FutureShell status

**Last reviewed:** 2026-11-05

**Overall status:** Pre-alpha design and planning prototype. FutureShell is not
an executable language or runtime.

This page is the current progress ledger for FutureShell. The
[roadmap](futureshell-roadmap.md) is the canonical product and semantics plan;
the [implementation plan](futureshell-implementation-plan.md) expands its
architecture, acceptance criteria, and backlog. The
[xshell development roadmap](xshell-development-roadmap.md) governs
cross-product delivery order and currently prioritizes shared xshell lifecycle,
authorization, and shared-workflow proofs before the remaining FS1 frontend
work. The [agentic CAD plan](agentic-cad-plan.md) is a parallel xshell
workstream whose headless mode overlaps FutureShell's agent-task surface; the
[harmonized direction](#harmonized-direction-headless-runs-and-futureshell)
below records how the two relate. The [original concept note](futureshell.md)
is historical context rather than a current specification.

## Current implementation

Two provisional crates exercise the authoring and planning model:

- `xshell-flow` parses and validates JSON Flow IR fixtures, computes a
  presentation-independent semantic hash, and renders Graphviz DOT;
- `xshell-plan` lowers valid flows into deterministic Plan V0 task and loop
  templates, normalizes capabilities, detects a conservative set of write
  conflicts, and resolves program and predicate dependencies from provisional
  caller-supplied catalogs.

Four Flow fixture families cover a linear pipeline, contract branch, parallel
join, and bounded refinement loop. Their golden Plan V0 artifacts test stable
lowering and hashing. [ADR 0004](adr/0004-explicit-canonical-plan-encoding.md)
fixes explicit canonical Plan V0 bytes and a domain-separated hash, with exact
golden vectors for every plan fixture. These plans are inspectable but are not
authorized or executable; their JSON form remains an inspection format.

The existing xshell execution engine, audit service, session fabric, platform
support, and adapters are reusable substrate. None currently implements the
FutureShell scheduler, transactional workspace, contract evaluator, receipt
verifier, or agent gateway.

## Recent shared work (2026-09-13 to 2026-11-05)

No FutureShell milestone artifact has landed in this interval, so the
milestone ledger is unchanged. The shared xshell work below advances FutureShell
prerequisites:

| Area | Work | FutureShell relevance |
|---|---|---|
| Lifecycle and cancellation | F1–F3 corrections; cancelled process groups are re-killed until empty (#95) | FS2 cancellation acceptance ("kills complete process groups") builds on this |
| Prepared file access | M1.3: one opened object shared by authorization and acquisition, canonical-path sensitivity, bounded reads (F4, F5, F8; #98) | The pattern FS3 manifests and FS4 artifact evidence need: decide and act on the same object |
| Provider and audit bounds | Only finished provider responses dispatch tools (F9, #97); bounded audit I/O without a global lock (F6, #99) | Required-audit failure stopping at the next pre-action boundary is an FS2 acceptance item |
| Turn budgets | Whole-turn budget shared with subagents (#84), raised counts (#101), configurable limits with a `relaxed` profile and a daemon ceiling (#107) | First implementation of the FS5 autonomy budget; per-session, global, byte and worker budgets (rest of F10) remain open |
| Subagents | `delegate_task` with per-agent toolsets under the parent's budget (#86) | Start of the `xshell-session` connector's capability narrowing |
| Agentic CAD plan | Images, configurable limits, headless mode, and approval policy planned and decided (#105, #106) | See the harmonized direction below |
| Headless runs | `xshell run` with versioned JSONL events and exit codes (#108); `--detach`, `wait`, `status`, `result`, `cancel` (#109) | Surface of the FS5 `xshell-session` connector |
| Approval policy | Daemon policy with stricter-wins client overlays, `prompt`/`block` sensitivity classes, and an approval channel (#110) | First form of the M2 decision function and gateway policy |

The headless stack is a set of xshell features, not FutureShell milestones;
it is recorded in the ledger as FS5 connector substrate.

## Milestone ledger

| Milestone | State | Implemented evidence | Exit blockers | Next proof point |
|---|---|---|---|---|
| FS0 — Semantics and threat model | **Complete** | [FS0 package](futureshell-fs0.md), [review record](futureshell-fs0-review.md), accepted ADRs, reviewed language examples, threat/guarantee model, schema drafts, platform probe, and deterministic FEA matrix | None; schema stability and runtime proofs belong to later milestones | Preserve the accepted semantics while FS1 proves the textual toolchain and canonical plan identity |
| FS1 — Language toolchain and dry-run planner | **Prototype only** | Flow IR validation and hashing; Plan V0 lowering, explicit canonical encoding and golden hash vectors; capability normalization, basic conflict checks, pure fixture-catalog resolution | Text parser, spans and diagnostics, formatter, module loader and source verification, `xshell-run check\|fmt\|plan`, conformance and fuzz corpora | One textual FEA program produces a fully resolved stable plan without execution or network access |
| FS2 — Deterministic local task runtime | **Not started** | Reusable process-group cleanup, prepared operations, bounded audit I/O | Authorized-plan input, scheduler, argv-native runner integration, shell blocks, structured concurrency, resource bounds, task evidence, authorization preview | A concurrent local workflow produces attributable typed evidence and cancels complete process groups |
| FS3 — Transactional workspaces | **Not started** | Design and acceptance matrix only | Portable staging backend, manifests, change sets, preview, discard, selective promotion, conflict detection, journal and recovery | Invalid work leaves the destination unchanged; accepted allowlisted outputs are hash-verified after promotion |
| FS4 — Deterministic contracts and receipts | **Not started** | Contract expressions represented in Flow IR and Plan V0 only | Predicate evaluator, evidence model, clause reports, taint policy, receipts, audit binding and offline verifier | The deterministic FEA slice promotes only schema-valid `analysis.json` and rejects every specified failure case |
| FS5 — Agent tasks | **Planned** | Provider adapters; `delegate_task` subagents with per-agent toolsets and a shared turn budget ([subagents design](subagents-design.md)); configurable turn limits; headless runs, job control, approval policy and approval channel (#108–#110) | Agent gateway, connector contract, lifecycle and state policy, capability mediation, autonomy and usage accounting | Agent-produced artifacts cannot exceed the authorized envelope or bypass a deterministic contract |
| FS6 — Session-fabric execution | **Planned** | Existing local/remote session fabric is potential transport substrate | Plan-fragment protocol, remote authorization, transaction retention, artifact transfer and per-host receipts | Mixed local/remote execution reports honest independent host outcomes |
| FS7 — Hardening and engineering workflows | **Planned** | Product direction only | Policy profiles, packaging, optimized isolation, LSP, provenance, retention, CAD/yapCAD/FEA libraries | Representative macOS and Linux workflows have reviewed assurance and recovery behavior |

States describe FutureShell-specific completion, not the maturity of reusable
xshell components.

## Harmonized direction: headless runs and FutureShell

Headless `xshell run` and FutureShell both run unattended engineering work,
but they make different promises. A headless run is one supervised agent turn:
it is bounded, audited, and policy-filtered, but its results are accepted by
whatever script reads them. A FutureShell program makes acceptance itself
deterministic: staged writes, typed evidence, contracts and selective
promotion. Headless mode is therefore the near-term path for CAD automation
and the future surface of the FS5 `xshell-session` connector. It does not
replace FS2–FS4.

### Concept mapping

| Headless / CAD plan | FutureShell counterpart | Gap to close |
|---|---|---|
| `xshell run`, `--detach`, `wait`, `status`, `cancel` | Connector `open`, `send`, detach/attach, `status`, `cancel` ([implementation plan §9.2](futureshell-implementation-plan.md#92-connector-contract)) | Usage reporting and stable operation IDs: jobs are addressed by session, not turn |
| JSONL event schema v1 | Connector event stream | A presentation contract, not evidence (H1) |
| Exit codes 0–7 | `xshell-run` command exit codes | One shared outcome vocabulary (H2) |
| Approval policy with daemon base and client overlay | M2 decision function; gateway policy (§9.3); capability envelope (§9.5) | Policy identity and decision source are not audited (H3) |
| Turn limits and the `relaxed` profile | Autonomy budget: model rounds and tool dispatches (§9.4) | Usage and assurance labels in the result (H4) |
| Approval channel (`--approval-channel FD`) | Gateway human-decision events; `on_exhausted request_approval` | Decisions should carry an authorization identity (H3) |
| Sensitivity `block` class | Read-capability denial | Covers agent read tools only, not `run_shell` (H6) |
| In-memory event journal | Durable task evidence | Results are lost on daemon restart (H5) |

### Harmonization commitments

- **H1 — Events are presentation; the audit log is evidence.** The JSONL
  stream is rendered by the client and may be replayed, truncated or
  reordered. FutureShell contracts consume typed audit events, never JSONL.
  The headless `result` record should carry the identifiers needed to find
  the turn's audit records (session, turn, and audit sequence range or
  checkpoint), so a supervisor can later verify instead of trusting the
  stream.
- **H2 — One outcome vocabulary.** Exit codes 0–7 keep their headless
  meanings across all xshell binaries. `xshell-run` adds codes for its own
  outcomes, such as contract invalid, promotion conflict, recovery required,
  and verification failed, without reusing 0–7 for other meanings. Define the
  table once in shared code.
- **H3 — The approval policy is the first M2 decision function.** The
  call-level `deny`/`prompt`/`run` decision, the stricter-wins overlay, and
  the daemon ceiling are the shape M2 needs. To become the M2 boundary, the
  audit trail must record the effective mode and limits, the identity (hash)
  of the base and overlay policies, and each decision's source (policy rule,
  approval channel, interactive user, or mode default). Approvals should
  bind to the prepared operation, as M1.3 does for reads. In FutureShell the
  static capability envelope is checked first; the approval policy can only
  narrow it, never widen it.
- **H4 — Turn limits are the autonomy budget.** Use FutureShell's vocabulary
  in durable schemas: provider requests are *model rounds* and dispatched
  tool calls are *tool dispatches*. The `result` record should report usage
  against each limit, labelled with its assurance (`enforced` for counts and
  time, `measured` or `unavailable` for tokens). The remaining F10 budgets
  should land before relaxed unattended runs are common.
- **H5 — Durable job identity.** FutureShell task identity is
  `(plan hash, task, iteration path, attempt)`. Headless job commands should
  accept a turn ID, as the CAD plan originally specified, and the terminal
  result should be durable (F7), so that `wait` and `result` survive a daemon
  restart. Until then the connector cannot claim durable resume.
- **H6 — Pattern policy is not isolation.** Shell patterns match literal
  words and cannot see what an interpreter or script does. `run_shell` keeps
  ambient authority and must never be reported as `isolated`. The CAD plan's
  open question on sandboxing `run_shell` and FutureShell's `isolated`
  provider are one design problem, to be solved once.
- **H7 — Gate CAD outputs with deterministic checks.** A supervisor that
  accepts agent-produced CAD artifacts should check them with deterministic
  verifiers (schema, hashes, expected files) rather than the model's answer.
  This is the FS4 pattern, and the deterministic FEA fixture is the
  migration target for such scripts.

### Review record for #108–#110

- **Deny-pattern bypass (fixed in #110).** Review found that commands such
  as `rm -rf "build;"`, `$(echo rm) -rf build`, `{rm,-rf,build}` and
  `FOO=1 rm -rf build` decided `run` under `denied = ["run_shell: rm *"]` in
  `auto` mode. Splitting at separators inside quotes dropped unparsable
  parts, and substitutions, expansions and assignments hid the command word.
  The policy now fails closed: with `run_shell` denials, a command that
  cannot be split into parts read as written is prompted in `ask` mode and
  denied otherwise. Wrappers such as `xargs rm` remain the documented
  literal-word limit (H6).
- **Audit binding (open, H3).** The client overlay is applied but not
  audited, and `ToolDecision` audit records do not say what decided.
- **Plan drift (open, H5).** The CAD plan specifies job commands on a turn
  ID and `wait` reading from the session snapshot. The implementation uses
  the session and the in-memory journal. Either update the plan or close the
  gap.

## Traceability

| Area | Plan | Current artifact | Verification | Status |
|---|---|---|---|---|
| Product semantics and milestone exits | [Roadmap](futureshell-roadmap.md) | Roadmap sections 1–14 | [FS0 review record](futureshell-fs0-review.md) and later milestone exits | FS0 baseline accepted |
| Detailed architecture and backlog | [Implementation plan](futureshell-implementation-plan.md) | Implementation plan sections 2–17 | Milestone acceptance criteria | Draft for review |
| Flow authoring graph | Roadmap §5.4; implementation plan §2 | [`crates/xshell-flow`](../crates/xshell-flow), [Flow IR notes](futureshell-flow-ir.md), `fixtures/futureshell/flows` | `cargo test -p xshell-flow` | Executable provisional prototype |
| Checked plan lowering | Implementation plan §5 | [`crates/xshell-plan`](../crates/xshell-plan), [Plan V0 notes](futureshell-plan-v0.md), [ADR 0004](adr/0004-explicit-canonical-plan-encoding.md), `fixtures/futureshell/plans` | Exact canonical byte/hash vectors; `cargo test -p xshell-plan` | Canonical Plan V0 identity implemented; broader planner remains provisional |
| Language and source conformance | Implementation plan §4 | [Language reference](futureshell-language.md), paired human/agent-authored FEA sources, `fixtures/futureshell/syntax` | [FS0 language review](futureshell-fs0-review.md); later parser/formatter properties, diagnostics and fuzzing | FS0 semantics accepted; implementation pending |
| Program and predicate resolution | Implementation plan §§5.1 and 16 | Plan V0 resolver and `fixtures/futureshell/catalogs` | Resolver fixtures bind declared pinned identities without side effects and reject interface, predicate, and capability mismatches | Executable provisional prototype |
| Transaction and promotion safety | Roadmap §7; implementation plan §6 | [Threat model](futureshell-threat-model.md), [platform spike](futureshell-platform-spikes.md), change-set schema, promotion ADR | [FS0 security review](futureshell-fs0-review.md); later FS3 acceptance matrix | Requirements accepted; implementation pending |
| Evidence, contracts and receipts | Roadmap §§8–9; implementation plan §8 | [Schema package](futureshell-schemas.md), FEA contract/failure matrix, provisional contract DTOs | FS0 invariant traceability; FS4 failure matrix and offline verification | FS0 semantics accepted; implementation pending |
| Agent target and policy semantics | Roadmap §5.3; implementation plan §9 | Agent, connector, gateway, autonomy and usage schema drafts; language lifecycle semantics | FS0 language/security review; FS5 connector conformance | FS0 semantics accepted; implementation pending |
| Headless agent runs (connector substrate) | [Agentic CAD plan](agentic-cad-plan.md) §5; this page, H1–H7 | #108–#110, [headless runs](headless.md), [approval policy](session-fabric.md#approval-policy) | End-to-end CLI tests against a real `xshelld` | Shared xshell feature; FS5 alignment tracked by H1–H7 |

## Immediate sequence

1. Record policy identity, the client overlay, and each decision's source in
   the audit trail (H3), and resolve the job-identity drift (H5).
2. Finish the remaining F10 budgets (per-session, global, byte, worker)
   before relaxed unattended runs are common, then F7 transactional
   persistence, which H5 also needs.
3. Establish the M2 authorization/provider boundary, starting from the
   approval policy's decision function. Grants bind to the prepared
   operations produced by M1.3.
4. Prove one bounded structured-plan workflow under the personal and
   controlled profiles as the narrow M5 slice: an authorized Plan V0 input,
   argv-native tasks, a staged workspace, one contract and selective
   promotion. This advances FS2–FS4 but does not complete them.
5. Resume `xshell-language` and `xshell-run check|fmt|plan`; the textual
   frontend remains required before FS1 can be declared complete.

FS2 through FS4 form the first useful local release. Agent and remote execution
remain later milestones so they do not enter the trusted transaction and
contract core at the same time. Headless runs serve CAD automation in the
meantime under the limits stated in H6 and H7.

## Updating this page

Update the review date, milestone ledger, traceability table, and immediate
sequence whenever a FutureShell milestone artifact lands. A milestone advances
only when its documented exit or acceptance condition is demonstrated; landing
a prototype or reusable xshell substrate is not sufficient by itself.
