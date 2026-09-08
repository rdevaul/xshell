# FutureShell FS0 specification package

**Status:** Review candidate; the FS0 language and security gates remain open

This page is the index and review record for the first FutureShell milestone.
The package defines the intended semantics precisely enough to review and to
start FS1 implementation; it does not claim that the language, runtime,
isolation, transactions, contracts, or receipts are implemented.

## Package contents

| Review surface | Artifact | What it fixes for FS0 |
|---|---|---|
| Language | [Language reference](futureshell-language.md) and [`syntax` fixtures](../fixtures/futureshell/syntax) | Source identity, minimal grammar, types, execution forms, structured concurrency, transactions, contracts, determinism, diagnostics |
| Security | [Threat model and guarantee matrix](futureshell-threat-model.md) | Assets, trust boundaries, threats, testable invariants, assurance levels, non-rollback claims |
| Data contracts | [Schema notes](futureshell-schemas.md) and [`fs0.schema.json`](../fixtures/futureshell/schemas/fs0.schema.json) | Capabilities, evidence, change sets, clause reports, receipts, agents, gateway policy, autonomy and usage |
| Vertical slice | [Deterministic FEA fixture](../fixtures/futureshell/fea/README.md) | Success path plus task, schema, change-set, cancellation, race, evidence and audit failures |
| Platforms | [Platform spike report](futureshell-platform-spikes.md) and [`workspace-probe.sh`](../scripts/futureshell-workspace-probe.sh) | APFS/Linux staging observations and honest isolation/resource-control capability reporting |

The [roadmap](futureshell-roadmap.md) owns product semantics and milestone exits.
The [implementation plan](futureshell-implementation-plan.md) owns component
boundaries and acceptance criteria. Where this package conflicts with either,
review must resolve the conflict explicitly rather than silently choosing one.

## Review decisions required

Language reviewers must confirm that a human-authored example and an
independently agent-authored example can express the same bounded workflow,
and that parsing never requires execution or ambient discovery. Security
reviewers must accept or revise the path, symlink, mutation-race, journal,
promotion, audit, agent-state, and remote-host invariants in the threat model.

Review outcomes should be recorded as short ADRs for decisions that constrain
the wire format, trusted computing base, or public semantics. Editorial fixes
may land directly in these documents.

## Exit checklist

- [x] Language reference draft and annotated conformance examples exist.
- [x] Threat model and guarantee matrix make rollback limits explicit.
- [x] Provisional typed schema bundle covers every named FS0 policy object.
- [x] Deterministic FEA fixture specifies success and required failure paths.
- [x] Reproducible APFS/Linux staging and platform-capability probe exists.
- [ ] Human and agent-authored language examples receive language review.
- [ ] Security review accepts path, symlink, race and promotion semantics.
- [ ] Every accepted rollback or isolation claim names a testable invariant.

FS0 advances only when the final three boxes are checked with links to the
review record. Schema stability and runtime conformance are later milestones.
