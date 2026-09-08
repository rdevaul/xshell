# FutureShell status

**Last reviewed:** 2026-09-06

**Overall status:** Pre-alpha design and planning prototype. FutureShell is not
an executable language or runtime.

This page is the current progress ledger for FutureShell. The
[roadmap](futureshell-roadmap.md) is the canonical product and semantics plan;
the [implementation plan](futureshell-implementation-plan.md) expands its
architecture, acceptance criteria, and backlog. The
[original concept note](futureshell.md) is historical context rather than a
current specification.

## Current implementation

Two provisional crates exercise the authoring and planning model:

- `xshell-flow` parses and validates JSON Flow IR fixtures, computes a
  presentation-independent semantic hash, and renders Graphviz DOT;
- `xshell-plan` lowers valid flows into deterministic Plan V0 task and loop
  templates, normalizes capabilities, detects a conservative set of write
  conflicts, and reports unresolved program and predicate dependencies.

Four fixture families cover a linear pipeline, contract branch, parallel join,
and bounded refinement loop. Their golden Plan V0 artifacts test stable
lowering and hashing. These plans are inspectable but are not authorized or
executable. Normalized JSON is a prototype encoding, not a stable wire or disk
format.

The existing xshell execution engine, audit service, session fabric, platform
support, and adapters are reusable substrate. None currently implements the
FutureShell scheduler, transactional workspace, contract evaluator, receipt
verifier, or agent gateway.

## Milestone ledger

| Milestone | State | Implemented evidence | Exit blockers | Next proof point |
|---|---|---|---|---|
| FS0 — Semantics and threat model | **In progress** | Roadmap, implementation plan, Flow/Plan semantic spikes | Language reference, threat model and guarantee matrix, schema drafts, platform spikes, accepted deterministic FEA fixture, review gates | FS0 specification package is reviewed and its example failure paths are fully described |
| FS1 — Language toolchain and dry-run planner | **Prototype only** | Flow IR validation and hashing; Plan V0 lowering, hashing, capability normalization, basic conflict checks, fixtures | Text parser, spans and diagnostics, formatter, module loader, resolver, canonical plan encoding, `xshell-run check\|fmt\|plan`, conformance and fuzz corpora | One textual FEA program produces a fully resolved stable plan without execution or network access |
| FS2 — Deterministic local task runtime | **Not started** | Reusable xshell process and audit infrastructure only | Scheduler, argv-native runner integration, shell blocks, structured concurrency, resource bounds, task evidence, authorization preview | A concurrent local workflow produces attributable typed evidence and cancels complete process groups |
| FS3 — Transactional workspaces | **Not started** | Design and acceptance matrix only | Portable staging backend, manifests, change sets, preview, discard, selective promotion, conflict detection, journal and recovery | Invalid work leaves the destination unchanged; accepted allowlisted outputs are hash-verified after promotion |
| FS4 — Deterministic contracts and receipts | **Not started** | Contract expressions represented in Flow IR and Plan V0 only | Predicate evaluator, evidence model, clause reports, taint policy, receipts, audit binding and offline verifier | The deterministic FEA slice promotes only schema-valid `analysis.json` and rejects every specified failure case |
| FS5 — Agent tasks | **Planned** | Existing provider adapters are potential connector substrate | Agent gateway, connector contract, lifecycle and state policy, capability mediation, autonomy and usage accounting | Agent-produced artifacts cannot exceed the authorized envelope or bypass a deterministic contract |
| FS6 — Session-fabric execution | **Planned** | Existing local/remote session fabric is potential transport substrate | Plan-fragment protocol, remote authorization, transaction retention, artifact transfer and per-host receipts | Mixed local/remote execution reports honest independent host outcomes |
| FS7 — Hardening and engineering workflows | **Planned** | Product direction only | Policy profiles, packaging, optimized isolation, LSP, provenance, retention, CAD/yapCAD/FEA libraries | Representative macOS and Linux workflows have reviewed assurance and recovery behavior |

States describe FutureShell-specific completion, not the maturity of reusable
xshell components.

## Traceability

| Area | Plan | Current artifact | Verification | Status |
|---|---|---|---|---|
| Product semantics and milestone exits | [Roadmap](futureshell-roadmap.md) | Roadmap sections 1–14 | FS0 review gates | Draft plan of record |
| Detailed architecture and backlog | [Implementation plan](futureshell-implementation-plan.md) | Implementation plan sections 2–17 | Milestone acceptance criteria | Draft for review |
| Flow authoring graph | Roadmap §5.4; implementation plan §2 | [`crates/xshell-flow`](../crates/xshell-flow), [Flow IR notes](futureshell-flow-ir.md), `fixtures/futureshell/flows` | `cargo test -p xshell-flow` | Executable provisional prototype |
| Checked plan lowering | Implementation plan §5 | [`crates/xshell-plan`](../crates/xshell-plan), [Plan V0 notes](futureshell-plan-v0.md), `fixtures/futureshell/plans` | `cargo test -p xshell-plan` | Executable provisional prototype |
| Language and source conformance | Implementation plan §4 | None | Parser/formatter properties, invalid-source diagnostics and fuzzing | Not started |
| Program and predicate resolution | Implementation plan §§5.1 and 16 | Plan V0 resolution blockers | Resolver fixtures must bind pinned identities without side effects | Next implementation increment |
| Transaction and promotion safety | Roadmap §7; implementation plan §6 | Design only | FS3 acceptance matrix | Not started |
| Evidence, contracts and receipts | Roadmap §§8–9; implementation plan §8 | Contract expression DTOs only | FS4 failure matrix and offline verification | Not started |

## Immediate sequence

1. Add a pure program-manifest resolver, fixture program manifests, and a typed
   contract-predicate catalog. Resolved capabilities must not expand the Flow
   declaration.
2. Complete the FS0 language reference, threat model, guarantee matrix, schema
   drafts, deterministic FEA fixture, and platform spike reports.
3. Hold the FS0 language and security review gates and record durable decisions
   as short architecture decision records.
4. Select canonical serialization only after candidate encodings have been
   tested against the fixture corpus.
5. Complete FS1 from textual source through `xshell-run check`, `fmt`, and
   `plan` before starting an executable runtime.

FS2 through FS4 form the first useful local release. Agent and remote execution
remain later milestones so they do not enter the trusted transaction and
contract core at the same time.

## Updating this page

Update the review date, milestone ledger, traceability table, and immediate
sequence whenever a FutureShell milestone artifact lands. A milestone advances
only when its documented exit or acceptance condition is demonstrated; landing
a prototype or reusable xshell substrate is not sufficient by itself.
