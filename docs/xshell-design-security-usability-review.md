# xshell design, security, and usability review

**Code reviewed:** 10 September 2026

**Design updated:** 11 September 2026 — incorporates the agreed home-lab and controlled-environment mandate.

**Revision:** `4ca0839e971cb8ac485cab2e781a5d7fe54ee94a`

**Target:** one product serving both a convenient home-lab/daily-driver environment and production AI/programmatic orchestration in controlled information environments, including applicable ITAR/CUI deployments.

**Companion:** [Dual-mandate development roadmap](xshell-development-roadmap.md). The implementation findings and test results below remain evidence from the original reviewed revision; this update changes design direction and priorities, not their verification status.

## Assessment

Keep the named-session abstraction, separation between execution and presentation, host-local policy concept, and FutureShell's explicit assurance levels. Develop them in one modular codebase with a shared execution model. Personal convenience and controlled deployment are equal product goals; neither should become a permanent downstream fork of the other.

For daily use, xshell should offer quick setup, user-selected models, ordinary shell access, dependable remote sessions, and low interaction overhead. Ambient user authority and optional auditing can be intentional personal-deployment choices. For controlled deployment, policy ownership and enforcement must move beyond the requesting user's authority. The current alpha is not an adequate enforcement boundary for that deployment. This does not make enterprise identity, mandatory audit, or validated cryptography prerequisites for using or releasing the personal product.

Correct cancellation, honest failure records, bounded operations, and reliable recovery are shared obligations. The original review's most urgent problems—forgetting completed effects, allowing work to continue after stop, and leaving commands running after daemon shutdown—harm both audiences. A broad personal policy must never disable those correctness guarantees.

### Agreed product and architecture decision

Maintain one development line, one execution lifecycle, and common tool semantics. Use deployment profiles to select authority and required guarantees, implementation providers to enforce them, and packaging to constrain available components. Separate personal and controlled release packages are permitted; a long-lived `ITARxshell` source fork is not the plan. One codebase does not require one process or a monolithic trusted computing base.

| Dimension | Personal / home lab | Controlled deployment |
|---|---|---|
| Policy owner | User; may grant broad authority | Administrator; protected from clients and workloads |
| Model selection | User-managed destinations and credentials | Service-resolved approved profiles and credential bindings |
| Execution authority | Ambient authority permitted and identified | Selected provider enforces required filesystem, process, and network restrictions |
| Audit | Optional or local; explicit capture/retention settings | Required, independently protected, bounded failure behavior |
| Shared behavior | Same cancellation, recovery, approval binding, and tool semantics | Same guarantees, with additional authorization constraints |
| Distribution | Convenient defaults and broad supported integrations | Approved component set, qualified dependencies, deployment evidence |

Both profiles must use the same authorization path. The personal policy may grant an operation automatically; it must not bypass the policy interface. Controlled policy is a protected service-side constraint, not a user-changeable `controlled = true` flag. Requests may narrow authority but cannot weaken the host's requirements. Missing mandatory capabilities must block the affected execution, without a fallback to a less constrained provider.

Keep policy authority separate from execution assurance: a personal user can choose isolation, and a controlled profile does not automatically establish the `isolated` guarantee. Provider capability evidence determines assurance. Avoid a combinatorial collection of loosely related booleans; supply a few validated profiles with explicit, bounded overrides.

Existing controlled sessions, conversation history, derived artifacts, and logs cannot become unrestricted by changing a profile name. Policy relaxation and cross-domain release require separate authority and explicit data handling. For a personal session, policy editing should remain convenient and transparent.

Cryptographic providers may differ by package. Where a deployment requires a FIPS-validated module, its exact version, approved configuration, and applicable operating environment matter; an approved algorithm or library brand is not sufficient. Cover application TLS, audit signing, external SSH, and inherited storage services in the deployment inventory. This is a packaging/provider concern, not a reason to fork the execution model. [NIST CMVP guidance](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs).

### Priorities

| Priority | Work | Release gate |
|---|---|---|
| Shared alpha, now | Correct F1–F11; preserve core behavior across local, daemon, and remote modes | Trustworthy daily-driver foundation |
| Shared architecture, early | Define common authorization, execution-provider, profile ownership, and data-domain interfaces | Avoid later redesign without imposing enterprise setup on home users |
| Daily-driver track | Easy onboarding, session recovery, clear shell semantics, low approval friction, bounded SSH failure behavior | Personal releases can ship independently of controlled qualification |
| Controlled track | Enforced isolation/egress, protected identity/policy/credentials/audit, deployment control allocation | Required before a controlled-data pilot |
| Common workflow proof | Small deterministic local workflow with evidence, selective promotion, and recovery | Foundation for broader FutureShell runtime work |
| Expansion | Additional connectors, viewers, language features, and federation | Shared conformance plus deployment-specific qualification |

## What is working well

- **Sessions are a good durable user abstraction.** Conversation, cwd, model binding, and active work belong together. Detaching without cancelling is a valuable distinction.
- **The crate boundaries are largely sensible.** Adapters, execution, audit, sessions, PTYs, presentation, and pure planning are separated. Reusing the execution engine across local and daemon modes is preferable to independent agent loops.
- **Several security details are already deliberate:** same-user socket peer checks, escaped tool approvals, terminal-control filtering for model text, bounded provider responses, draining bounded shell pipes, single-use PTY tickets, and signed audit checkpoints.
- **FutureShell's guarantee vocabulary is unusually clear.** `staged`, `isolated`, and `external` avoid conflating copying with confinement. Selective promotion is explicitly not an atomic multi-file transaction. Preserve that honesty in the UI and receipts. See [docs/adr/0002-explicit-assurance-levels.md:1](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/docs/adr/0002-explicit-assurance-levels.md#L1) and [docs/adr/0003-selective-journaled-promotion.md:1](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/docs/adr/0003-selective-journaled-promotion.md#L1).
- **The tests exercise real services and failure boundaries.** The macOS/Linux/ARM CI matrix, MSRV check, and dependency audit job are appropriate investments. A passing suite nevertheless misses the defects below.

## Architectural recommendations across both deployment profiles

### 1. Give data movement its own authority model

`ModelBinding` carries a client-supplied provider, base URL, and credential environment-variable name. The daemon accepts session creation/update and constructs an adapter from those values. Its configuration loader consumes session and audit configuration; it does not resolve model selection against an authoritative host-owned model catalog. See [crates/xshell-session/src/model.rs:216](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/model.rs#L216), [crates/xshell-session/src/bin/xshelld.rs:457](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/bin/xshelld.rs#L457), [crates/xshell-session/src/bin/xshelld.rs:700](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/bin/xshelld.rs#L700), and [crates/xshell-session/src/execution.rs:320](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/execution.rs#L320).

This is consistent with a trusted same-user tool. It is insufficient for organizational policy: a named profile is descriptive metadata, not an authorization boundary. Clearing conversation on model switch is a helpful privacy feature, but cannot prevent an unauthorized initial endpoint or later tool-driven disclosure.

**Proposed design:** bind each session to a policy identity and data domain. In personal deployment, the user can maintain that policy and register destinations. In controlled deployment, an administrator owns it and the daemon resolves approved provider IDs into endpoint, model, credential reference, and permitted data classes. Controlled clients request a profile ID; they cannot substitute URL or credential binding. Validate the policy before every provider dispatch, tool dispatch, artifact export, view transfer, and controller attachment. Bind policy version and destination identity into execution evidence.

Treat source files, prompts, generated content, tool output, summaries, filenames, audit payloads, and terminal output as potential controlled data. Derived artifacts should inherit restrictions until an authorized release process changes them. A filename-based secret filter is useful defense in depth, but cannot classify CUI or export-controlled technical data.

When the selected policy requires restricted egress, enforce it outside the agent process as well as at the model gateway. Otherwise an approved `run_shell` can use its own network client. Explicitly control redirects, proxy configuration, DNS/destination policy, and plaintext HTTP exceptions; a loopback inference endpoint should be an intentional deployment choice. Test denial using synthetic marked data and a local unauthorized receiver, including attempted model-profile substitution and direct shell networking.

### 2. Separate people, controllers, services, and workloads

The daemon, tools, and usually audit client share the invoking OS identity. `run_shell` inherits the daemon environment; non-login invocation does not remove already inherited credentials. `ask` mediates shell dispatch but grants arbitrary shell authority after approval. `off` denies gated agent tools, while the human shell route remains available. These are interaction policies, not workload isolation. See [crates/xshell-execution/src/tools.rs:202](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L202) and [crates/xshell-execution/src/engine.rs:27](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L27).

For controlled production, introduce authenticated human and workload principals, project/session authorization, revocation, and explicit administrative policy ownership. SSH remains a useful transport, but transport authentication alone is not project authorization. `host_only` filtering in the SSH proxy is visibility control; a user with unrestricted SSH access can connect directly to the underlying same-user service.

For restricted workloads, run under an enforceable provider with a minimal environment, designated filesystem mounts, process/resource limits, and permitted network destinations. Keep policy, credential material, audit signing keys, and runtime state outside workload-writable authority. Prefer structured executable/argv tools for common operations; retain arbitrary shell as an explicit broad capability. Approving exact shell bytes cannot pin scripts, PATH resolution, shell startup behavior, or mutable files referenced by those bytes.

The first controlled deployment should select one supported execution platform and prove its isolation properties. A cross-platform controller is compatible with a narrower initial execution backend. Do not delay this decision until after the language/runtime interfaces assume ambient access is normal.

### 3. Make execution state authoritative; derive conversations from it

The existing session snapshot combines user-facing conversation and operational state, but completed effects are not durable facts independent of a successful turn. Meanwhile the protocol lets an attached client replace model, cwd, and history together. See [crates/xshell-session/src/protocol.rs:31](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/protocol.rs#L31) and findings F1/F7.

Introduce an append-only execution journal with turn/attempt/tool identities and states such as `accepted`, `authorized`, `started`, `finished`, `cancel_requested`, `cancelled`, and `outcome_unknown`. Record intent before effects and observed results afterward. Conversation becomes a projection of those facts; a provider failure must never erase execution history. A database or carefully specified journal is an implementation option, not a requirement to build a general event-sourcing framework.

Use idempotency keys for submissions and explicit retry semantics. A lost acknowledgement must not silently turn “resume” into a second execution. Arbitrary external effects cannot be made exactly-once merely by journaling; require an idempotent adapter or report an uncertain outcome that needs reconciliation.

Avoid a generic client `Update` as the long-term mutation API. Use explicit operations such as change-directory, select-approved-model, and reset-conversation with revision checks and audit events. This reduces accidental stale-state overwrite and makes policy enforcement reviewable.

### 4. Treat audit as protected operational evidence and sensitive data

The separate signing service and required pre-action acknowledgement are good foundations. However, signed JSONL authenticates the records accepted by the service; it does not prove that every real action was mediated or that client assertions are true. The existing same-user caveat in the README is correct.

Controlled production needs independent protection of the audit service and signing material, trusted-key provisioning/rotation, retention and incident export, monitoring of gaps, and access control for audit consumers. Include actor, controller, host, policy revision, turn/attempt, approval authority, provider destination, and result/artifact identity. Separate factual execution evidence from model narrative and client assertions.

Prompts and tool results are currently logged as raw content. Disabling PTY stream capture does not disable those other sensitive payloads. Define a payload policy: minimal metadata in the operational index; necessary raw evidence in protected storage with a bounded retention/access policy. Redaction should be schema-aware and must not quietly invalidate the meaning of signed evidence. Plan backup and incident preservation together with deletion/retention rules.

Personal users should be able to disable optional audit capture without losing the execution facts required for the selected session persistence mode. An operational recovery journal and a compliance audit log serve different purposes. Move audit I/O out of global session locks, and give it bounded failure behavior; see F6. A required-audit outage should produce an observable controlled stop, not an indefinitely frozen daemon.

### 5. Keep FutureShell small enough to prove

The pure planner and explicit evidence/assurance model should remain. The main roadmap risk is building a language, workflow scheduler, agent gateway, distributed session fabric, and transaction engine before one narrow end-to-end guarantee is demonstrated.

Use the existing Flow/Plan prototypes, once their executable-input contract is stabilized, to implement a small vertical slice: a pinned local program, declared inputs, enforced execution authority, immutable evidence, one deterministic predicate, selective promotion, and injected crash recovery. Include one failing path at each boundary. Prove the execution contract through a bounded structured plan first; do not make the full language frontend a prerequisite for that experiment. This changes the earlier FS1-first implementation sequence, not the accepted language semantics or the criteria for declaring FS1 complete. A new source language should be justified by workflows that are materially clearer than structured plan input. Run the slice through both deployment profiles, reporting the authority each provider actually enforces.

Keep plan validity, catalog resolution, executable identity, runtime authorization, observed evidence, and scientific correctness separate. A declared source hash or successful predicate lookup is not verification of the executable eventually launched. Current prototype status is accurately documented; do not upgrade its claims through naming or UI badges.

Pull data-domain, secret-reference, principal, and destination-policy interfaces forward from later agent/connector milestones. The current fabric already transmits prompts and executes workloads. Those boundaries affect the core model even before FutureShell runs. See [docs/futureshell-status.md:1](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/docs/futureshell-status.md#L1) and [docs/futureshell-threat-model.md:1](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/docs/futureshell-threat-model.md#L1).

## Specific implementation findings

**P1:** correct promptly because an existing control or lifecycle promise can fail. **P2:** important reliability/usability correction before broader deployment. Priorities are engineering priorities, not CVSS scores. “Reproduced” means exercised locally; “inspection” means a traced code path without a full fault-injection demonstration.

### F1 — P1: failed turns forget completed effects

**Evidence:** [crates/xshell-execution/src/engine.rs:280](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L280), [crates/xshell-execution/src/engine.rs:321](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L321), [crates/xshell-execution/src/engine.rs:411](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L411), and [crates/xshell-session/src/execution.rs:399](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/execution.rs#L399). **Reproduced.**

A scripted provider returned a shell call writing a marker, then failed on its next request. The marker remained, but history exactly equalled the pre-turn history. Cancellation and the 64-step limit also restore old history; the daemon only persists execution state on the success branch.

**Impact:** the next turn can repeat a completed mutation, lose an approval/result record from its context, or act on a false understanding of the workspace. Audit records may retain evidence, but the operational conversation does not recover it.

**Correction:** retain completed tool calls/results and append an explicit interrupted/failed-turn record. Repair incomplete conversation protocol pairs without discarding observed effects. Persist execution facts incrementally and expose uncertain effects on reconnect/restart. Only roll back staged conversational changes when no effects occurred.

**Acceptance:** perform one mutation, fail the next provider request, reload the session, and confirm the next model request includes the mutation and failure. Repeat for cancellation and the step limit.

### F2 — P1: stop does not interrupt an active agent shell tool

**Evidence:** [crates/xshell-execution/src/engine.rs:399](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L399) and [crates/xshell-execution/src/tools.rs:202](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L202). **Reproduced.**

Cancellation is checked around provider requests and between tools, but `execute_tool(...).await` has no cancellation branch. In a probe, the shell announced readiness, cancellation was signalled, and a delayed write still executed. A shell tool may continue until its 60-second timeout.

The captured direct-shell path also lacks a dedicated process group, despite being cancellable by dropping its future. Killing the immediate child is not sufficient process-tree cleanup. This second path was inspected, not separately reproduced.

**Correction:** pass cancellation into tool execution and use a process-group guard that kills and reaps on cancellation, timeout, errors, and future drop. Do not rely on adding `select!` alone: dropping the existing future only guarantees immediate-child cleanup. For hostile workloads, use the selected OS isolation/resource provider as the stronger process containment boundary.

**Acceptance:** cancel pipelines and shell grandchildren in both execution paths; no delayed marker may appear. Report “stopping” until cleanup completes, then distinguish confirmed cancellation from unknown outcome.

### F3 — P1: clean daemon shutdown leaves work running

**Evidence:** [crates/xshell-session/src/bin/xshelld.rs:251](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/bin/xshelld.rs#L251). **Reproduced.**

The shutdown handler finalizes audit sessions and calls `std::process::exit(0)` without first stopping/joining execution workers. A local protocol probe submitted a direct shell command, waited for it to start, sent SIGTERM to `xshelld`, and observed a marker written after the daemon exited with status 0.

**Impact:** restart can restore an apparently idle session while old work continues. With audit enabled, the shutdown ordering can finalize the log before surviving work ends; that audit-enabled variant is an inference from the code, not a separate runtime test.

**Correction:** stop accepting submissions, cancel active work, terminate/reap tracked processes, record terminal outcomes or uncertainty, flush session state, then finalize audit and exit. Bound the shutdown interval and specify forced-stop recovery.

**Acceptance:** SIGTERM during agent tools, captured shell work, and PTYs leaves no untracked work; final audit closure follows lifecycle resolution. Exercise SIGKILL separately and report interrupted/unknown work on recovery.

### F4 — P1: sensitive-path policy changes with cwd and misses nested credential directories

**Evidence:** [crates/xshell-execution/src/sensitive.rs:83](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/sensitive.rs#L83) and [crates/xshell-execution/src/tools.rs:107](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L107). **Reproduced.**

Slash-containing patterns match only the complete cwd-relative path. `sub/.git/config` is not gated from the parent workspace. The same file is gated as `.git/config` from `sub`, then ungated as `config` when cwd is `sub/.git`. A directory such as `.aws` similarly loses its ancestry when it becomes cwd.

**Correction:** define a stable authorization root independent of navigational cwd. Apply sensitive-directory classification to resolved ancestry and match intended default directory rules at every depth. Document the semantics of administrator-supplied patterns separately. Prefer positive allowed-data policy for controlled environments.

**Acceptance:** the same inode has the same sensitivity across cwd changes and nested repositories; include `.aws/credentials`, `.git/config`, symlink aliases, and project roots inside sensitive directories.

### F5 — P1: authorization and file acquisition are separate path resolutions

**Evidence:** [crates/xshell-execution/src/tools.rs:107](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L107), [crates/xshell-execution/src/tools.rs:145](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L145), and [crates/xshell-execution/src/tools.rs:314](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L314). **Reproduced boundary substitution; filesystem confinement race identified by inspection.**

A probe classified `notes.txt` as ungated, replaced it with a symlink to an in-root `.env`, and then executed the same call. The fake secret was returned. The check and execution independently resolve paths; there is no object binding. Separately, canonicalizing a path and later opening it leaves a replacement window for directory components, so canonicalization alone does not prove confinement against concurrent mutation.

**Correction:** prepare a bounded read using descriptor-relative traversal and a stable opened object, then authorize that prepared operation. Read from the authorized descriptor; reject or reauthorize identity changes. Define hard-link and mount behavior. If approval is intended to authorize exact content, snapshot/hash that content as well: an open descriptor alone does not make bytes immutable.

**Acceptance:** controlled hooks swap the final file and ancestor directories between preparation and execution. Neither unapproved sensitive bytes nor outside-root bytes may be returned. The existing static symlink-escape tests are insufficient.

### F6 — P1: a stalled audit connection can block all audited sessions and shutdown

**Evidence:** [crates/xshell-audit/src/client.rs:22](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-audit/src/client.rs#L22), [crates/xshell-audit/src/client.rs:85](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-audit/src/client.rs#L85), [crates/xshell-session/src/audit.rs:238](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/audit.rs#L238). **Inspection.**

Audit reads/writes have byte bounds but no socket deadlines. `SessionAuditHandle::append` holds the global audit-session mutex while waiting for acknowledgement. A connected service that stops replying can block unrelated sessions and audit shutdown. Synchronous session-client I/O also lacks general deadlines, so the CLI's controller polling cannot guarantee responsiveness while a request is blocked.

**Correction:** use bounded per-session audit queues and acknowledgement deadlines without holding a global map lock across I/O. Required audit failures must latch a failed state and stop new effects. Give control/SSH operations explicit deadlines and cancellation, preserving long polling as a bounded operation. Specify recovery after the audit service returns.

**Acceptance:** a fake service accepts a connection but never acknowledges; actions stop within the configured deadline, the controller stays usable, and other sessions can expose their status or shut down.

### F7 — P2: snapshot persistence is not a transactional state update

**Evidence:** [crates/xshell-session/src/registry.rs:150](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/registry.rs#L150), [crates/xshell-session/src/registry.rs:213](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/registry.rs#L213), [crates/xshell-session/src/registry.rs:252](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/registry.rs#L252), [crates/xshell-session/src/registry.rs:286](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-session/src/registry.rs#L286). **Inspection.**

Create/update/close mutate the registry before `persist()` succeeds. A disk failure can return an error with the mutation still present in memory. For example, create can leave a record attached to the connection even though its caller never receives a successful creation response. The file is synced before rename, but the containing directory is not synced afterward. Every persistence pass serializes all durable sessions while the registry is locked.

**Correction:** stage metadata mutations and commit them only after storage success, or use a transactional store. Sync the containing directory according to the supported platform's durability contract. Persist per-session/incremental state to avoid cross-session latency. Keep already executed effects distinct from metadata rollback.

**Acceptance:** inject write, sync, and rename failures into create/update/close; the response, live registry, and restarted state agree. Crash after replacement and verify documented recovery. Check behavior with large histories across many sessions.

### F8 — P2: file and directory limits apply after unbounded acquisition

**Evidence:** [crates/xshell-execution/src/tools.rs:160](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L160) and [crates/xshell-execution/src/tools.rs:176](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/tools.rs#L176). **Inspection.**

`read_file` uses `std::fs::read` before truncating to the requested maximum. A large regular file can consume large memory despite a tiny `max_bytes`. Directory enumeration collects and sorts every entry before applying the 500-entry cap. Both execute synchronously inside the agent runtime.

**Correction:** read at most `max_bytes + 1` through the authorized handle, preserving a truncation flag. Bound directory enumeration before collection; use a cursor or explicitly defined partial ordering. Put potentially blocking filesystem work in bounded workers with an appropriate deadline strategy. Async timeout alone cannot interrupt an arbitrary blocking filesystem operation.

**Acceptance:** requesting one byte from a large sparse regular file has bounded memory/I/O; huge directories do not allocate in proportion to all entries; a slow filesystem does not freeze the control plane.

### F9 — P2: incomplete model streams can yield executable tool calls

**Evidence:** [crates/xshell-adapters/src/openai.rs:181](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-adapters/src/openai.rs#L181), [crates/xshell-adapters/src/openai.rs:232](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-adapters/src/openai.rs#L232), [crates/xshell-adapters/src/lib.rs:111](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-adapters/src/lib.rs#L111). **Reproduced.**

The OpenAI-compatible adapter ignores `[DONE]` and does not require completion evidence. A normally closed HTTP body containing one valid tool-call delta, with neither a finish reason nor `[DONE]`, produced a successful tool call. This can happen with a faulty intermediary/provider that ends HTTP successfully but incompletely at the application protocol level. It is not a claim that every broken TCP stream is accepted.

**Correction:** track provider-specific completion state and finish reason. Reject incomplete/error/refusal responses before dispatching tools. Define how supported compatible providers signal completion and test each contract; validate tool-call IDs and response choices too.

**Acceptance:** EOF after a tool delta produces an incomplete response and executes nothing; a valid completed tool response succeeds; provider errors and partial argument streams cannot become executable calls.

### F10 — P2: history and step limits are not a whole-turn resource budget

**Evidence:** [crates/xshell-execution/src/engine.rs:283](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L283), [crates/xshell-execution/src/engine.rs:285](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L285), [crates/xshell-execution/src/compaction.rs:78](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/compaction.rs#L78), [crates/xshell-adapters/src/lib.rs:30](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-adapters/src/lib.rs#L30). **Inspection.**

History compaction runs before the first provider request and after a successful turn, not before each tool-loop request. It always preserves the newest turn, even if oversized. Compaction defaults to disabled. A response can contain up to 256 tool calls, while the 64-step limit counts provider rounds. Consequently the advertised bounded loop is not a practical bound on total tools, context, elapsed time, or spend.

**Correction:** enforce independent per-request, per-turn, per-session, and global budgets: tool count, provider calls, context/output bytes, elapsed time, worker count, and retained data. Where cost cannot be enforced exactly, label estimates and enforce a conservative request/token budget. Preserve tool-result consistency when summarizing or truncating active-turn context.

**Acceptance:** repeated maximal tool batches hit a deterministic total budget; oversized latest turns are rejected or explicitly reduced before submission; detached work cannot accumulate unlimited sessions/threads/history.

### F11 — P2: `cd` accepts a regular file as the session cwd

**Evidence:** [crates/xshell-execution/src/engine.rs:438](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-execution/src/engine.rs#L438) and [crates/xshell-cli/src/control.rs:174](https://github.com/rdevaul/xshell/blob/4ca0839e971cb8ac485cab2e781a5d7fe54ee94a/crates/xshell-cli/src/control.rs#L174). **Reproduced for the shared direct-shell implementation; duplicated CLI path inspected.**

`cd file` canonicalizes a regular file successfully and reports a working-directory change. Subsequent operations requiring a directory fail. The session protocol also accepts cwd values without a common directory-validation boundary.

**Correction:** validate that the resolved destination is an accessible directory before committing it, and reuse this validation for CLI, daemon, create, update, and restore. Keep shell syntax treatment consistent between interactive and captured execution; do not classify every command beginning with `cd` as a standalone builtin.

**Acceptance:** `cd` to a file preserves the old cwd and returns a clear error; deleted/restored cwd and compound shell commands have documented, tested behavior.

## Usability and operator confidence

The prompt already displays session, model, cwd, and activity. Extend that foundation with compact, relevant context. Home-lab users should not face an enterprise policy ceremony on every command. Show detailed restrictions on demand and when a decision needs them; keep host, session, and effective authority clear enough to prevent mistakes.

| Situation | Recommended behavior |
|---|---|
| First personal run | Reach a working prompt without mandatory identity infrastructure or audit-service setup; explain persistence and the selected provider |
| Repeating approved personal work | Allow user-owned, scoped reusable grants through the common policy path; expose scope and revocation |
| Entering a controlled session | Display data domain, authenticated host, permitted model destination, and effective policy; never use color as the only distinction |
| Approving an effect | Show host/session/cwd, execution authority, exact request, and relevant egress/effect scope; bind approval to the prepared operation |
| Selecting `off` | Rename/document it as denial of gated agent tools; it currently sounds like approval is switched off and also denies sensitive reads |
| Stopping work | Show request accepted, cleanup underway, then confirmed termination or uncertain outcome |
| Resuming after failure | Summarize completed effects, incomplete operations, and lost output before accepting a retry |
| Replay truncation | State what evidence is missing and provide the authoritative execution record; a byte ring is not full terminal state |
| Switching models | Make conversation reset explicit and offer a separately authorized summary transfer when policy allows it |
| Starting in standalone mode | Clearly identify which persistence, remote, PTY, and audit behaviors apply; publish a mode-conformance matrix |

Two shell expectations deserve explicit treatment: environment changes such as `export` do not become persistent shell state across independently spawned commands, and special handling of `cd` differs from an ordinary long-lived shell. Either support a persistent shell context deliberately or describe xshell as a session orchestrator with shell execution. Avoid implying full shell equivalence.

For service operation, add a machine-readable health/status surface covering active work, pending approvals, audit health, policy version, replay gaps, storage pressure, and recovery-required state. Do not silently fall back to a local or less constrained mode in the controlled deployment profile. A safe default model and an approved destination are separate concepts.

## Deployment and regulatory context

This is an engineering review, not a certification. The applicable requirements must be selected from the actual contract, data categories, export authorizations, system role, and approved operating environment. CUI and ITAR should be represented as distinct policy dimensions.

NIST SP 800-171 Rev. 3 scopes protection to nonfederal components that process, store, transmit, or protect CUI, and covers access control, auditing, authentication, configuration, incident response, and communications protection. Use it as a control-design reference; do not infer the contractually required revision solely from which revision is newest. [NIST publication](https://csrc.nist.gov/pubs/sp/800/171/r3/final).

Where DFARS 252.204-7012 applies, its external-cloud provision requires FedRAMP Moderate-equivalent security and additional incident-related obligations for covered defense information. An endpoint's TLS support or a generic provider claim does not establish that a particular deployment meets those obligations. [DFARS clause](https://www.acquisition.gov/dfars/252.204-7012-safeguarding-covered-defense-information-and-cyber-incident-reporting.).

ITAR's encrypted-data provisions distinguish protected transmission/storage from access to unencrypted technical data by recipients. Model inference that receives plaintext must be evaluated as recipient access; transport encryption alone is not the approval decision. The State Department's rule explanation makes this distinction explicit. [State Department rule and explanation](https://www.pmddtc.state.gov/sys_attachment.do?sys_id=1d508454db82c8505c3070808c961968). Current authorizations and regulatory text need confirmation for the selected deployment; this historical rule explanation is not a complete current applicability analysis.

**Concrete next artifact:** a deployment control-allocation matrix identifying what xshell enforces and what is inherited from the OS, identity provider, network, storage, approved inference service, and operating procedures. Assign an owner and test evidence to every claimed control. Include cryptographic module/validated configuration requirements, administrator/support access, incident handling, and supply-chain provenance; selecting Rust, SSH, TLS, or Ed25519 by itself does not establish those properties.

## Recommended delivery sequence

The [companion roadmap](xshell-development-roadmap.md) is the updated cross-product sequencing plan. It preserves the original findings and FutureShell semantics while making both audiences explicit:

1. **Repair shared correctness first.** Correct F1–F11 and convert the original defect probes into regression tests. These fixes apply to all profiles.
2. **Establish the common authority interfaces.** Separate policy ownership, execution authority, data domain, credentials, and assurance. Implement the personal profile through the same policy path that controlled deployments will use.
3. **Advance two delivery tracks.** Ship daily-driver onboarding, shell/session UX, and reconnect improvements while developing a narrow controlled execution provider and its denial tests. Personal release readiness must not depend on controlled qualification.
4. **Prove one shared local workflow.** Exercise deterministic execution, evidence, selective promotion, and recovery under both profiles before broadening the language/runtime surface.
5. **Package and qualify independently.** Publish convenient personal releases and, when its acceptance conditions are met, a constrained controlled distribution from the shared source line. Expand connectors and federation only with conformance evidence.

Decisions still needed concern the first controlled execution OS, inference location, organizational identity/control baseline, workload scale, recovery objectives, retention, and any required validated cryptographic module. They do not reopen the agreed single-codebase decision or block shared correctness and personal usability work. The roadmap assigns each decision a deadline relative to implementation milestones.

## Verification and review limits

The following test results belong to the 10 September code review. The 11 September revision is a documentation/design update; it does not claim fixes or new runtime validation.

- Read the main specification/README, current fabric and FutureShell design material, and security/lifecycle paths across execution, adapters, sessions, audit, platform, PTY, CLI, and planning code. This is a targeted architecture/code review, not exhaustive line-by-line certification or a penetration test.
- Ran the workspace tests on **Darwin arm64, Rust 1.98.0**. After clearing stale package build artifacts containing paths from an earlier checkout location, the existing suite passed. The initial missing-binary/fixture failures were build-artifact relocation issues, not treated as product defects.
- **185 existing tests passed** with `cargo test --workspace --all-targets --locked`. Clippy passed with `cargo clippy --workspace --all-targets --locked -- -D warnings`; `cargo fmt --all -- --check` passed.
- Ran **six focused probes**, all confirming the described current behaviors: failed-turn history loss, delayed effect after cancellation, cwd-dependent sensitivity, path replacement between check/use, regular-file cwd acceptance, and incomplete model-stream acceptance. The preserved [probe source](review-evidence/review_probe.rs) uses fake content, a local mock provider, and temporary files. These tests assert current defects and must be inverted when converted to regression tests.
- Ran an additional isolated-daemon shutdown probe through protocol v11: the daemon returned exit status 0, then the submitted command wrote its delayed marker. No existing user daemon was stopped.
- No real model credentials, controlled datasets, or remote execution hosts were used. External research used public regulatory queries. Linux behavior, a live multi-host deployment, crash durability, memory exhaustion, and a current dependency-advisory scan were not independently validated here.
- Production source was not changed. The deliverables are this review, the companion roadmap, and the original supporting probe code.

To rerun the six probes in a disposable checkout, copy `docs/review-evidence/review_probe.rs` to `crates/xshell-execution/tests/review_probe.rs`, run `cargo test -p xshell-execution --test review_probe --locked`, and remove that copied test afterward. The full suite's success does not negate these findings: the probes specifically exercise previously uncovered behavior.
