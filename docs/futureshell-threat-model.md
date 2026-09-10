# FutureShell threat model and guarantee matrix

**Status:** FS0 reviewed baseline; implementation claims remain milestone-gated

## 1. Scope and security objective

FutureShell surrounds fallible tools and probabilistic agents with deterministic
planning, explicit authority, staged filesystem state, evidence-backed
contracts, selective promotion, and verifiable receipts. Its objective is to
make granted authority, observed work, durable results, and guarantee limits
inspectable. It does not make arbitrary computation trustworthy or arbitrary
external effects reversible.

This model covers one invocation from source loading through plan, local or
remote authorization, task execution, contract evaluation, promotion/discard,
audit binding, and receipt verification. Provider infrastructure, host
administration, kernel compromise, physical attacks, and correctness of the
scientific algorithm are outside the trusted computing base unless a workflow
adds independent verification.

## 2. Assets

- destination workspace contents and metadata;
- staged inputs, outputs, manifests, and promotion journals;
- program, executable, predicate, schema, plan, evidence, and receipt identity;
- credentials, prompts, context, private artifacts, and provider responses;
- host, session, agent, connector, policy, and authorization identity;
- audit ordering, completeness, signing keys, and retained evidence;
- resource availability and configured cost/autonomy ceilings;
- operator understanding of actual versus unavailable assurance.

## 3. Actors and assumptions

The invoker may make mistakes but controls initial source selection and approval.
Program authors, imported modules, subprocesses, agent output, remote hosts,
connectors, tools, artifacts, and workspace contents may be malicious. A host
policy is trusted only on its own host. The audit service is trusted for records
it acknowledges. **AUDIT-1:** required audit acknowledgement gates the next
irreversible action. Platform isolation is trusted only after a capability
probe and provider-specific tests.

The operating-system kernel, filesystem implementation, and FutureShell binary
are trusted on each execution host. A receipt from another host is trusted only
after signature/checkpoint verification under an accepted host identity. Hashes
provide integrity binding, not truth, confidentiality, freshness, or scientific
validity.

## 4. Trust boundaries

1. **Source boundary (`SOURCE-1`):** bytes and imported modules enter a pure,
   bounded parser and checker that use only declared inputs.
2. **Authorization boundary (`AUTH-1`):** an immutable resolved plan is compared with
   invoker and host policy; runtime values cannot enlarge it.
3. **Process boundary:** untrusted code receives selected OS authority.
4. **Workspace boundary:** staged state is separated from the destination;
   isolation may or may not prevent ambient writes.
5. **Agent gateway boundary:** prompts, tools, credentials, usage, approvals,
   and state cross into a connector or provider.
6. **Host boundary:** plan fragments and artifacts cross an authenticated
   transport and are independently authorized.
7. **Evidence boundary:** runtime observations become immutable typed facts.
8. **Promotion boundary:** accepted staged objects become destination state.
9. **Audit/receipt boundary:** claims are bound to an ordered signed history.

## 5. Threats and required controls

| Threat | Required control | Residual limitation |
|---|---|---|
| Parser input triggers execution or network access | Pure bounded parser/checker; no executable imports or ambient lookup | Availability still depends on enforced input limits |
| Runtime value expands authority | Static capability envelope in hashed plan; subset check at every dynamic dispatch | Coarse grants may still authorize harmful in-scope actions |
| Executable is substituted | Record requested and resolved identity; hash before spawn; stronger providers bind opened object to execution | Path/hash-before-spawn alone has a replacement race |
| Task writes undeclared staged paths | Final typed change set plus allowlist contract | Detection occurs after the write inside staging |
| Task writes destination or host paths directly | `isolated` provider or explicit ambient-write taint at `staged` assurance | Portable staging alone cannot prevent ambient writes |
| Symlink, hard-link, mount, or path traversal escapes staging | Descriptor-relative no-follow walk, mount policy, link checks, normalized relative paths | Platform/filesystem behavior must be tested and reported |
| Concurrent destination edit is overwritten | Compare destination to baseline immediately before per-path replacement | Multi-file promotion is crash-recoverable, not atomic |
| Crash yields ambiguous promotion | Durable intent journal, deterministic transitions, idempotent recovery and final verification | Some paths may be promoted before recovery completes |
| Agent claims a tool ran | Hard predicates use mediated dispatch and process evidence, never prompt text | Opaque/unmediated agents support advisory evidence only |
| Agent exceeds autonomy or spend | Gateway-enforced round/tool/time bounds and assurance-labelled token/cost accounting | Provider-reported token or price data may be delayed/estimated |
| Persistent agent memory changes | Explicit lifecycle/state owner and state-mutation taint | Native memory rollback requires connector support |
| Credential or private artifact leaks | Named secret references, egress classification, connector policy, redaction | A granted recipient can retain disclosed data |
| Remote coordinator over-authorizes host | Remote host reauthorizes its own plan fragment | Compromised execution host can forge unsigned local claims |
| Receipt/evidence is edited or replayed | Content hashes, task/plan/transaction/host identity, nonce/time policy, signed audit binding | Freshness depends on verifier policy and trusted keys |
| Audit silently drops required event | Acknowledged required events gate the next action | Already completed external effects cannot be undone |
| Contract consumes live mutable state | Pure evaluator receives finalized immutable evidence/change-set bundle | Incorrect trusted evidence can still yield an incorrect result |
| Model review authorizes promotion | Model judgment is advisory only; deterministic clauses and policy gate promotion | Human override remains an explicit higher-risk authorization |

## 6. Guarantee matrix

| Claim | `staged` | `isolated` | `external` |
|---|---|---|---|
| Task receives a separate working tree | Required | Required | Provider-attested equivalent |
| Changes inside that tree can be discarded | Required | Required | Provider-attested artifact discard |
| Destination workspace cannot be written during execution | **Not guaranteed** | Enforced and tested | Outside provider boundary |
| Writes outside declared roots are denied | **Not guaranteed**; tainted | Enforced and tested | Provider-attested policy |
| Network/process/resource restrictions | Measured or unavailable unless separately enforced | Provider reports enforced subset | Provider-attested subset |
| Selective promotion checks baseline conflicts | Required locally | Required locally | Required when importing returned artifacts |
| Arbitrary external side effects roll back | Never | Never | Never unless a typed transactional adapter says otherwise |
| Cross-host all-or-nothing commit | Never | Never | Never in initial releases |

**ASSURANCE-1:** Every plan states the minimum required level. Preflight fails
closed when a hard requirement cannot be enforced. Receipts distinguish
`enforced`, `provider_enforced`, `measured`, `estimated`, and `unavailable`;
user approval cannot relabel weaker assurance.

## 7. Filesystem invariants

- **FS-RUNTIME-1:** Runtime-owned state and journals are outside task-writable
  roots.
- **FS-WALK-1:** Source walks never follow a link outside the declared root or silently cross
  a mount boundary.
- **FS-MANIFEST-1:** Baseline and result manifests use normalized relative paths and canonical
  ordering; mutation while hashing retries within a bound and then fails.
- **FS-SPECIAL-1:** Special files and unsupported metadata fail or produce a policy-visible
  unsupported entry; they are never silently copied or promoted.
- **FS-LINK-1:** Hard links never connect staged writable files to destination
  files.
- **PROMOTE-1:** Promotion selects explicit paths, revalidates destination identities, writes
  intent durably before replacement, syncs affected directories, and verifies
  final identities.
- **PROMOTE-2:** Deletion and permission changes require explicit selection.

## 8. Contract and evidence invariants

- **EVIDENCE-1:** Every event binds schema version, plan, transaction, task, iteration, attempt,
  host, and monotonic event identity where applicable.
- **EVIDENCE-2:** Process success requires observed exit evidence, not absence
  of an error.
- **EVIDENCE-3:** Executable, argv, output, artifact, and changeset identities
  are separate.
- **CONTRACT-1:** Evidence is finalized before contract evaluation and cannot be added by the
  evaluator.
- **CONTRACT-2:** Clause reports preserve `pass`, `fail`, and `error`; errors never coerce to
  pass.
- **PROMOTE-3:** Promotion records the exact accepted clause report and selected
  object hashes.
- **EVIDENCE-4:** Large bytes remain external content-addressed artifacts; the receipt records
  hash, size, media type, and storage assurance.

## 9. Review-to-test traceability

Acceptance here approves requirements, not implementation claims. Each
rollback, isolation, or promotion statement is gated by a named invariant and
the milestone that must prove it.

| Invariant | Accepted requirement | Required verification |
|---|---|---|
| `SOURCE-1` | Parsing, formatting, checking, and planning do not execute code, contact providers, or search ambient paths | FS1 parser side-effect tests and hostile-input fuzzing |
| `AUTH-1` | Runtime values and child tasks cannot enlarge the hashed static capability envelope | FS1 subset properties; FS2 dispatch-denial integration tests |
| `FS-RUNTIME-1` | Tasks cannot write runtime state, journals, or signing material | FS3 adversarial root and permission tests |
| `FS-WALK-1` | Walks remain beneath the declared root and obey the mount policy | FS3 descriptor-relative symlink, replacement-race, and mount tests |
| `FS-MANIFEST-1` | Manifest identities are canonical and mutation races fail closed | FS3 ordering, Unicode/case, and concurrent-mutation tests |
| `FS-SPECIAL-1` | Unsupported files or metadata are visible and block automatic promotion by default | FS3 special-file and metadata fixtures |
| `FS-LINK-1` | Staging never creates a writable hard-link path into the destination | FS3 inode/link-count and mutation tests on macOS and Linux |
| `PROMOTE-1` | Promotion is selective, conflict checked, journaled, synced, recoverable, and finally verified | FS3 crash injection at every journal transition plus destination-race tests |
| `PROMOTE-2` | Deletes and permission changes require explicit selection | FS3 change-kind authorization matrix |
| `PROMOTE-3` | Receipts bind accepted clauses and promoted object identities | FS4 receipt substitution and offline verification tests |
| `EVIDENCE-1` through `EVIDENCE-4` | Evidence is attributable, typed, bounded, and content-addressed | FS2 evidence tests; FS4 tamper, replay, and retention tests |
| `CONTRACT-1` and `CONTRACT-2` | Contract input is closed before evaluation and errors never pass | FS4 pure-evaluator properties and FEA failure matrix |
| `ASSURANCE-1` | `staged` never implies write confinement; `isolated` requires provider-specific denial tests | FS3 cross-platform provider conformance tests |
| `AUDIT-1` | Required audit acknowledgement gates the next irreversible action | FS2/FS4 audit-loss injection at every pre-action boundary |

## 10. Non-rollback examples

Rollback does not retract an email, API mutation, database write, payment,
credential disclosure, print job, device motion, remote persistent-agent memory,
or write made through ambient host authority. A workflow may use a typed adapter
with verified prepare/commit or compensation, but compensation is a new effect,
not rollback. These effects must be denied, isolated, or surfaced as taints
before promotion policy runs.

## 11. Failure posture

Unknown schema versions, predicates, capabilities, assurance claims, journal
states, or receipt signature algorithms fail closed. Cancellation, timeout,
audit failure, verifier error, undeclared change, executable mismatch,
destination conflict, and incomplete evidence prevent automatic promotion.
Staged data is retained only under an explicit bounded lease for inspection or
recovery; otherwise it is discarded.

## 12. FS0 security review record

- [x] Source/import and catalog provenance boundaries are accepted under
  `SOURCE-1` and `AUTH-1`.
- [x] Descriptor-relative path handling, symlink/hard-link behavior, mount
  crossings, case folding, Unicode normalization, and mutation races are
  accepted under the `FS-*` and `PROMOTE-*` invariants.
- [x] Journal transitions and crash injection are required by `PROMOTE-1`.
- [x] Every assurance claim maps to a milestone test through the table above.
- [x] Hard contracts cannot consume prompt assertions or untrusted claims under
  `CONTRACT-1` and `CONTRACT-2`.
- [x] Secret values are excluded from plans, diagnostics, hashes, and receipts;
  FS1 and FS5 must test redaction and reference-only handling.
- [x] Remote and agent state changes are external effects unless a typed,
  verified provider supplies a stronger guarantee.
- [x] The deterministic FEA success and failure matrix covers the required FS0
  outcomes; unimplemented cases remain explicitly assigned to FS2–FS4.

The review disposition and accepted limitations are recorded in the
[FS0 review record](futureshell-fs0-review.md). These checks approve the model
as an implementation baseline; they do not claim that FS1–FS4 enforcement
exists.
