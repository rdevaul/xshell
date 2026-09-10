# FutureShell FS0 review record

**Date:** 2026-09-10

**Disposition:** Accepted as the FS1 implementation baseline

This review closes the FS0 language and security gates. It approves bounded
language semantics, explicit guarantee limits, and testable requirements. It
does not approve the provisional JSON representation as a stable format or
claim that the FutureShell runtime, isolation, transactions, contracts, or
receipts exist.

## Language review

The human-authored `minimal-fs0.fsh` and independently agent-authored
`agent-authored-fs0.fsh` express the same bounded FEA workflow: two argv-native
tasks, explicit resource and capability envelopes, a staged transaction,
deterministic contract clauses, selective promotion, and discard on invalidity.
Comments, formatting, and explicit `none` versus omitted capability families do
not affect the intended workflow semantics.

The review found and resolved one specification defect: the grammar required
`resources` and `capabilities` syntactically even though the invalid fixture
expects a parsed task to receive all three missing-bound diagnostics. The
grammar now accepts absent blocks and the checker requirements remain strict.
It also fixes the binding-derived task naming rule used by the FEA contract.

Accepted language constraints:

- source loading and planning are pure and bounded;
- `run` is argv-native and `shell` remains explicit and higher risk;
- authority is declared, normalized, and monotonic;
- child lifetime is lexical and detached transactional writers are forbidden;
- contracts consume only finalized typed evidence;
- agent syntax is plan-only until FS5.

## Security review

The threat model accepts malicious source, modules, processes, agents,
connectors, remote hosts, artifacts, and workspace contents within its stated
host-kernel trust boundary. The review accepts `staged` only as disposable-tree
assurance. It does not imply destination or ambient-write confinement.
`isolated` remains unavailable until a selected provider passes its exact
cross-platform denial tests.

Filesystem, promotion, evidence, contract, assurance, authorization, source,
and audit requirements now have stable invariant identifiers and milestone
test assignments in the threat model. Promotion is deliberately selective and
per-path, conflict checked, journaled, and recoverable; it is not described as
an atomic multi-file commit. External effects are taints unless governed by a
separate typed transactional adapter.

## Durable decisions

- [ADR 0001](adr/0001-pure-source-and-planning-boundary.md) fixes the pure,
  declared-input source and planning boundary.
- [ADR 0002](adr/0002-explicit-assurance-levels.md) fixes the staged, isolated,
  and external assurance vocabulary without inference-based upgrades.
- [ADR 0003](adr/0003-selective-journaled-promotion.md) fixes selective,
  journaled, non-atomic promotion semantics.

## Residual work

FS1 must prove parser purity, diagnostic bounds, formatter properties, plan
determinism, capability containment, and canonical encoding. FS2–FS4 must prove
runtime, workspace, contract, audit, and receipt invariants before those
guarantees can appear in execution receipts.
