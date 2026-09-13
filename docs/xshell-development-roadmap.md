# xshell development roadmap: daily driver and controlled deployment

**Updated:** 11 September 2026

**Status:** agreed architectural direction; implementation milestones below are planned, not completed.

**Companion:** [Design, security, and usability review](xshell-design-security-usability-review.md).

**Implementation baseline:** review of revision `4ca0839e971cb8ac485cab2e781a5d7fe54ee94a` on 10 September 2026.

## Mandate and scope

Build one product that is convenient for daily home-lab work and deployable as AI/programmatic orchestration infrastructure inside a controlled information environment. Maintain one development line, common execution semantics, and common tool interfaces. Deployment profiles select authority and required guarantees; providers enforce those guarantees; distributions select and qualify components.

A personal release must not wait for enterprise deployment qualification. A controlled distribution must not inherit user-controlled escape hatches merely because they are convenient for a personal installation. Reliable cancellation, honest failure records, bounded operations, and recovery apply to both.

This is the updated cross-product prioritization and release plan. Where sequencing conflicts, it supersedes the ordering in the [original xshell implementation plan](../xshell-implementation-plan.md) and the FS1-first sequence in [FutureShell status](futureshell-status.md). Those documents retain their detailed backlog and historical completion records. The [FutureShell roadmap](futureshell-roadmap.md), [implementation plan](futureshell-implementation-plan.md), and accepted ADRs remain the semantics and detailed acceptance references. No existing FS milestone becomes complete because a milestone here is delivered.

In particular, an experimental runtime may consume a bounded, stable structured plan before the full language frontend is complete. It must preserve pure planning, authorization, and assurance requirements. Completing the textual language toolchain remains necessary to claim FS1 completion.

## Decisions to preserve

| ID | Decision | Implementation consequence |
|---|---|---|
| D1 | One core product and development line; no permanent ITARxshell fork | Shared fixes and lifecycle tests; qualified maintenance branches may exist |
| D2 | Core correctness is unconditional | No profile may disable cancellation cleanup, truthful outcomes, or required recovery bookkeeping |
| D3 | Every dispatch passes through the same authorization interface | Personal policy may auto-grant; controlled policy may deny; neither uses a bypass path |
| D4 | Authority has an explicit owner | Personal configuration is user-owned; controlled policy is protected from clients and workloads |
| D5 | Policy and execution assurance are distinct | A profile name never establishes isolation; required capabilities must be demonstrated by the selected provider |
| D6 | Data restrictions survive model/profile changes | Controlled-to-unrestricted movement requires explicit release authority, not a mode toggle |
| D7 | Provider and package differences are allowed | Cryptography, isolation, credential delivery, identity integration, and component availability may differ without redefining tools |
| D8 | Convenience is an acceptance condition | Personal onboarding and routine work need no enterprise identity or mandatory audit-service installation |
| D9 | Releases qualify independently | Ship personal improvements regularly; controlled packages require their own deployment evidence |

Use the existing Rust workspace as the modular starting point. Keep orchestration semantics shared while placing enforceable trust boundaries between workloads and protected policy, credentials, state, and audit services where the deployment requires it. Do not turn every crate into a service or create new crates solely to mirror this document.

## Delivery map

| Milestone | Dependency | Deliverable | Lead responsibility* |
|---|---|---|---|
| M0 — Record contracts and baseline | None | Architecture decisions, conformance matrix, issue backlog | Maintainer / architecture |
| M1 — Repair common guarantees | M0 baseline; fixes may start immediately | F1–F11 corrections and regression evidence | Execution / session maintainers |
| M2 — Common policy and provider boundary | M0; integrate with M1 lifecycle | Personal profile using shared authorization; controlled policy contract | Policy / execution maintainers |
| M3 — Daily-driver release | M1, M2 | Convenient local and SSH workflows with reliable recovery | CLI / session / release maintainers |
| M4 — Controlled deployment foundation | M1, M2; target-environment decisions | One enforced deployment on synthetic data | Security / platform / deployment owner |
| M5 — Shared workflow proof | M1, M2; controlled acceptance also needs M4 | One deterministic local plan through both profiles | Plan / runtime / artifact maintainers |
| M6 — Distribution qualification | Personal: M3; controlled: M4, M5 | Separate release gates and reproducible package evidence | Release / deployment owner |
| M7 — Expansion | Relevant M6 gate | Qualified additions to language, connectors, viewers, federation | Feature owner plus boundary owner |

*These are responsibility areas, not assigned people or a requirement for separate teams. Name an accountable owner when opening each milestone. Sequence by acceptance evidence rather than speculative calendar dates. M3 and M4 are independent workstreams; M5's personal experiment can advance while the controlled provider is being completed.

## M0 — Record the architecture and establish the baseline

**Work:**

- Record D1–D9 as a repository ADR, including the conditions that could justify a future exception to the single-line policy.
- Define the terms deployment profile, model profile, policy owner, principal, data domain, provider, and assurance. Keep deployment profiles separate from `ask`/`auto`/`off` approval preferences.
- Open tracked work for F1–F11 using the review's evidence and acceptance conditions. Preserve original probes as historical evidence; turn them into assertions of corrected behavior when fixes land.
- Create a mode-conformance matrix covering standalone, local daemon, SSH daemon, captured shell, agent tools, and PTYs. Mark unsupported combinations explicitly.
- Choose the storage/lifecycle contract for each persistence mode. Optional compliance audit and operational execution history must be separate concepts.

**Exit:** one documented architecture decision, a mapped correction backlog, and an executable/mock-backed conformance plan. The 185 passing tests from the original review are historical baseline evidence, not proof that the findings are fixed.

## M1 — Repair common lifecycle, access, and resource guarantees

Deliver small changes with focused tests rather than a single hardening rewrite.

| Package | Findings | Implementation work | Acceptance evidence |
|---|---|---|---|
| M1.1 — Effects and recovery | F1, F7 | Preserve completed tool facts on failure; transactional metadata updates; incremental durable execution facts; storage synchronization contract | Effect followed by provider failure survives reload; injected storage failures leave response/live/restarted state consistent |
| M1.2 — Cancellation and shutdown | F2, F3 | Cancellation-aware execution; process-group cleanup guards; stop admission before draining workers and closing audit | Cancel agent tools, captured pipelines, PTYs, and shutdown mid-work; no untracked delayed effects; forced stop reports uncertainty |
| M1.3 — Prepared file access | F4, F5, F8 | Stable policy root independent of cwd; sensitivity across ancestry; descriptor-bound authorization and acquisition; bounded reads/enumeration | Cwd/nested-path tests and deterministic replacement hooks return no unapproved bytes; large-file/directory tests remain bounded |
| M1.4 — Service and provider bounds | F6, F9, F10 | Audit/control I/O deadlines; avoid global locks across I/O; completion-aware provider parsing; whole-turn/session/global budgets | Stalled peers leave control usable; incomplete responses execute nothing; excessive batches hit deterministic limits |
| M1.5 — Directory and shell consistency | F11 | Shared directory validation for create/update/restore/cd; defined handling of simple versus compound shell commands | A file cannot become cwd; failed changes retain the old cwd; mode matrix exercises compound commands |

Start M1.1–M1.3 first, while correcting small independent defects such as F11 when practical. Do not defer ordinary shell cleanup until an isolation provider exists. Do not describe process-group cleanup as confinement of deliberately escaping hostile workloads.

**Exit:** F1–F11 have evidence of correction or an explicit narrower disposition justified against the reproduced behavior; original reproductions no longer demonstrate the defect. Relevant checks pass on supported macOS/Linux targets. No profile weakens these fixes.

## M2 — Establish one authorization and execution-provider contract

**Work:**

- Define an operation request carrying actor, session, policy identity/revision, data domain, requested capability, target/resource identity, and resource limits as applicable.
- Resolve requests into prepared operations; bind approvals to those operations. Revalidate policy/expiry at dispatch. A caller cannot manufacture a grant by setting a protocol field.
- Make the authority boundary own provider dispatch for model calls, file access, shell/argv execution, and data transfer. Completion, viewing, attachment, and export must have explicit data-access policy too.
- Introduce a service-resolved model catalog. Personal users can register their own destinations. Controlled clients select approved IDs without supplying a replacement endpoint or credential binding.
- Replace generic session-state replacement over time with explicit, version-checked operations: change directory, select model, reset conversation, and submit work. Define migration/version negotiation; reject unsupported security requirements without silently downgrading.
- Implement an ambient personal execution provider and an interface for enforced providers. Report the actual capabilities and assurance; missing required capabilities deny execution.
- Keep cryptographic and credential interfaces narrow and driven by actual operations. Inventory application TLS, audit signing, SSH, and inherited storage before selecting any controlled cryptographic implementation.
- Define policy-change behavior for active work, pending approvals, expiry, and revocation. A revoked grant stops new dispatch; already started effects receive the documented cancellation/reconciliation behavior.

**Personal path:** supply validated defaults, a local policy owner, optional audit, and an explicit ambient-authority choice. Do not require an identity server or separate policy service merely to start the REPL. Standalone mode invokes the common policy library in process.

**Controlled path:** specify a protected policy owner and data-domain bindings. User approval cannot override mandatory host restrictions. Disabling audit or switching to ambient execution is unavailable when the deployment requires those controls. Prevent unlabeled export and profile changes that strip existing restrictions.

**Exit:** the same mock operation passes through the same authorization/dispatch path under both policies; it executes under a permitted personal policy and is denied under a restrictive policy. Tests cover endpoint substitution, forged/expired grants, policy revision changes, unauthorized relaxation, and mandatory-provider failure. Typed conformance tests—not only UI behavior—establish the boundary.

## M3 — Ship a daily-driver increment

**Work:**

- Provide a documented short setup path for a configured local or hosted model, with no mandatory enterprise setup. Expose effective authority and persistence without crowding the prompt.
- Preserve easy shell and PTY access. Publish exactly what persists across commands: cwd, environment, shell state, conversation, and processes.
- Add user-owned reusable grants where useful, scoped by host/session/workspace, capability, and lifetime. Make scope inspection and revocation straightforward. Every use still goes through M2.
- Improve SSH status/reconnect behavior using bounded I/O and submission identities. Reattach to existing work after an interrupted connection; never retry a side effect merely because an acknowledgement was lost.
- Make stopping, detached, waiting for approval, replay truncated, and recovery required distinguishable. Keep session/model/host context visible when returning from another task.
- Record startup, input response, switching latency, setup steps, and approval count on a documented reference setup. Establish numeric performance budgets from that baseline before declaring the release ready.

**Acceptance journeys:**

| Journey | Pass condition |
|---|---|
| Fresh personal setup | Reach a working agent/shell prompt using the documented path without enterprise identity or mandatory audit setup |
| Routine trusted work | A user-selected scoped grant eliminates repeated prompts for that scope; out-of-scope requests still follow policy |
| Mixed agent and terminal work | Start work, switch sessions, return to a PTY, resize, detach, and resume with correct context and terminal restoration |
| Stop a mistaken operation | UI distinguishes cancellation request from cleanup completion; effects and uncertainty match M1 evidence |
| Home-lab SSH interruption | Reconnect finds the same session/work identity and does not repeat a mutation |
| Unavailable model or daemon | Return a useful, bounded error; any personal fallback is explicit and never silently transfers work to another host/provider |

**Exit:** these journeys pass with local mocks in automation and representative manual terminal/SSH runs; performance and interaction budgets are recorded and met. M4/M5 controlled qualification is not a dependency for this release.

## M4 — Implement one controlled deployment foundation

Use synthetic data until the selected environment is authorized to handle its real data.

**Work:**

- Select one execution OS/provider and an approved inference topology. Keep the personal controller's broader platform support.
- Separate workload authority from protected runtime state, policy, credentials, and audit signing material. Define authenticated controllers/workloads, authorization, administrative access, and revocation.
- Enforce filesystem, process/resource, and network restrictions outside workload-authored commands. Exercise direct shell/network clients as well as xshell's adapters.
- Implement data-domain checks for prompts, derived artifacts, transcripts, logs, controller attachment/viewing, and exports. Defaults preserve restrictions; release requires explicit authority.
- Add required audit acknowledgement, protected retention/export, key provisioning/rotation, health monitoring, and bounded outage behavior. Optional payload capture must not undermine necessary evidence.
- Produce the deployment control-allocation matrix: each claim has an owner, enforcement location, inherited dependencies, and acceptance evidence. Select the applicable regulatory/contract baseline with the deployment owner; the profile name is not a compliance claim.

**Exit:** synthetic-data tests deny unauthorized endpoint changes, shell egress, outside-root writes, access to service credentials/state, unauthorized controllers, policy downgrades, and export to an unrestricted domain. Audit loss, expired authority, resource pressure, and unavailable isolation produce the documented bounded failure. Evidence identifies the actual platform and provider versions. Nothing is labeled `isolated` solely because a mechanism is installed.

## M5 — Prove one shared workflow end to end

**Work:** stabilize a bounded executable plan input and run one pinned local program with declared inputs and outputs, deterministic evidence, one contract predicate, selective promotion, and crash recovery. Use an existing redistributable fixture where possible; avoid making a large scientific toolchain a prerequisite for testing the orchestration contract.

Run the same logical workload under the personal and controlled profiles. Shared tool/result/lifecycle semantics remain identical; authority and evidence of enforcement may differ. An ambient personal run must expose external-effect limitations and cannot claim stronger assurance than it has.

**Exit:** a successful run promotes only selected validated outputs. Failure tests cover bad contract evidence, cancellation, destination conflicts, undeclared outputs, interrupted promotion, and lost acknowledgement. Idempotent recovery converges without pretending that external effects are reversible or exactly-once. The controlled run additionally passes M4 denials. Both report actual assurance and retained evidence.

This slice advances portions of FS2–FS4; it does not complete their entire acceptance matrix. The full language frontend, expanded agent connectors, and distributed execution follow demonstrated need. Preserve the FS0 semantics and pure planning boundary throughout.

## M6 — Package and qualify without forking

| Personal release gate | Controlled distribution gate |
|---|---|
| M1–M3 acceptance; straightforward installation; supported-platform conformance | M1–M2 and M4–M5 acceptance; selected deployment/platform qualification |
| User-owned profiles; clear optional dependencies and authority | Approved component manifest; protected policy; no silent provider downgrade |
| Regular feature releases and shared security fixes | Pinned dependencies/build configuration, provenance, migration and rollback evidence |
| Published compatibility and recovery behavior | Deployment control allocation, incident procedures, retention/key management, operator authorization |

Use the same source revision for a paired qualification run, with separately identified build/component manifests. Controlled packages may omit unnecessary connectors and use a different cryptographic implementation. Build-time omission reduces the assessed surface; runtime policy still governs permitted operations.

Where validated cryptography is required, record the module certificate, exact version/configuration, approved operation, and applicable platform evidence for each relevant service. Do not claim qualification because of a vendor name. Personal packages may retain their ordinary cryptographic implementation.

Maintain a shared security-fix path with a defined controlled-release requalification process. A constrained maintenance branch is allowed; duplicated execution semantics are not. Tests cover supported profiles and package component sets rather than every arbitrary combination of feature flags.

**Exit:** a personal package is independently releasable after its gate. A controlled package is releasable for its specified deployment only after its gate and required organizational approval. Document supported upgrades, state migration, rollback limits, and security-fix ownership for each.

## M7 — Expand against demonstrated contracts

Add language features, connectors, viewers, and remote workflow execution incrementally. Each proposal must identify its user journey, new authority/data paths, dependency footprint, assurance limitations, and required conformance tests. A feature can ship in the personal distribution before it is included in a controlled package.

Do not allow a connector to bypass shared authorization because it supplies its own agent loop. External/native agent state must have an explicit owner and honest rollback limits. Remote hosts independently authorize work; cross-host outcomes remain per-host unless a separately specified protocol proves stronger guarantees.

## Decisions and deadlines

| Decision | Responsible role | Needed by | Work that can proceed meanwhile |
|---|---|---|---|
| Lifecycle journal/storage and session migration contract | Session/runtime maintainer | M1.1 design | Other M1 fixes and mock conformance |
| Common policy schema, principal model, and grant lifecycle | Policy/runtime maintainer | M2 implementation | M1 and personal journey baselines |
| First controlled OS, inference location, and isolation mechanism | Deployment/security owner | M4 implementation | Ambient provider and M3 |
| Contract/data baseline, admin roles, retention, and cryptographic requirements | Deployment owner with relevant specialists | M4 control allocation, before real-data pilot | Synthetic-data implementation/tests |
| Target concurrency, recovery objectives, and personal UX budgets | Product/runtime owner | Respective M1/M3/M4 exit gates | Baseline measurement and bounded defaults |
| Controlled package components and qualification cadence | Release/deployment owner | M6 qualification | Shared releases and component inventory |

## Immediate actionable backlog

1. Land this review/roadmap and record D1–D9 in an ADR; create the F1–F11 tracking entries.
2. Convert F1–F3 probes into lifecycle regression tests and repair history preservation, cancellation, and shutdown ordering.
3. Implement prepared bounded file reads and stable sensitivity semantics for F4/F5/F8; correct cwd validation in the same boundary where appropriate.
4. Remove blocking global audit I/O and add deadline/failure tests; then correct stream-completion and whole-turn budget handling.
5. Draft the M2 operation/grant/provider contract and run one operation through personal and restrictive mock policies.
6. Baseline the M3 home-lab journeys while the controlled deployment owner selects the M4 target environment.

These are implementation tasks, not actions performed by this document update. Keep the review's finding IDs attached to fixes and evidence. When implementation begins, update the older plan/status entry points to link here and reconcile their immediate sequences; do not overwrite historical test or completion claims.
