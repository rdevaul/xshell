# FutureShell implementation plan

**Status:** Draft for review

**Companion documents:** [roadmap](futureshell-roadmap.md),
[concept note](futureshell.md), [xshell specification](../xshell-specification.md),
and [current xshell implementation plan](../xshell-implementation-plan.md)

## 1. Objective

Build `xshell-run`, a separate script runtime that reuses xshell's adapters,
execution engine, session fabric, viewers, platform code, and audit service.
The first useful release executes deterministic local workflows in staged
workspaces, evaluates evidence-backed contracts, selectively promotes outputs,
and produces independently verifiable receipts. Agent and remote tasks are
added only after this core is dependable.

This plan deliberately does not wrap BusyBox or selected shell commands to
implement rollback. Such wrappers cannot observe writes made by arbitrary
engineering tools. The filesystem boundary is a staged workspace with an
explicit baseline and promotion protocol.

## 2. Architectural boundaries

```text
FutureShell source
    |
    v
xshell-language  ---- parse, spans, AST, types, diagnostics, formatter
    |
    v
xshell-plan      ---- checked immutable task DAG, capabilities, plan hash
    |
    v
xshell-runtime   ---- structured concurrency, task lifecycle, evidence routing
    |       |       |       \
    |       |       |        +-- xshell-agent-gateway -- connector policy
    |       |        +---------- xshell-contract  -- predicates, receipts
    |       +------------------- xshell-workspace -- stage, diff, promote
    +--------------------------- xshell-execution / adapters / session / view
                             |
                             v
                         xshell-auditd

xshell-run       ---- CLI for check, fmt, plan, run, verify, recover
xshell CLI       ---- remains independently usable; may invoke xshell-run later
xshelld          ---- later accepts checked plan fragments over a new protocol
```

Crate names are provisional until the FS0 design review, but responsibilities
must remain separated. In particular:

- parsing never executes or probes the environment;
- planning may resolve declared configuration but never run user code;
- contracts cannot perform I/O;
- workspace code does not interpret prompts or contract source;
- agent adapters do not decide promotion;
- connectors do not decide their own authority or whether their claims satisfy
  a contract;
- the coordinator does not manufacture remote-host evidence.

## 3. Proposed workspace additions

| Package | Initial responsibility |
|---|---|
| `xshell-language` | Lexer, parser, source spans, AST, diagnostics, formatter, type/value definitions, module loading. |
| `xshell-plan` | Semantic checking, capability normalization, task DAG, static read/write analysis, canonical serialization and plan hashing. |
| `xshell-workspace` | Backend trait, portable staged-tree backend, manifests, change sets, promotion journal, conflict detection and recovery. |
| `xshell-contract` | Evidence schema, deterministic predicates, clause reports, canonical receipts and offline verification. |
| `xshell-runtime` | Task scheduler, cancellation, process runner integration, artifact limits, audit/evidence emission and policy enforcement. |
| `xshell-agent-gateway` | Agent-target resolution, connector interface, lifecycle/state management, usage accounting and gateway-policy enforcement. Added in FS5. |
| `xshell-run` | User-facing binary and subcommands. |

Existing packages should evolve as follows:

- `xshell-core`: share only genuinely common identifiers and bounded value
  types; do not turn it into a catch-all.
- `xshell-execution`: expose argv-native process execution and richer execution
  facts alongside the existing shell and agent paths.
- `xshell-session`: add remote plan/task/transaction messages only in FS6,
  behind a protocol bump.
- `xshell-audit`: add typed FutureShell events and receipt/checkpoint binding
  with explicit protocol and format evolution.
- `xshell-platform`: expose capability probes and carefully scoped helpers for
  clones/reflinks, atomic replacement, durable directory sync, process limits,
  and optional isolation backends.
- `xshell-view`: provide verifier-compatible render manifests as evidence.

Avoid circular dependencies by keeping language and plan types independent of
runtime implementations. Stable wire/disk schemas should use dedicated DTOs
rather than serializing internal AST or scheduler objects.

## 4. Language implementation

### 4.1 FS0 deliverables

Before parser code lands, write a compact language specification containing:

- lexical rules, Unicode policy, comments, shebang handling, strings and
  interpolation;
- identifiers, immutable bindings, values and type conversions;
- argv-native `run` and explicit `shell` semantics;
- task blocks, agent blocks, transactions and checkpoints;
- agent target/model selection, lifecycle, connector, gateway policy, autonomy
  and resource budgets;
- `spawn`/`await`, cancellation and scope-exit behavior;
- contract expressions, verifier tasks and promotion clauses;
- imports, module identity and source hashing;
- errors, exit statuses and command-line exit codes;
- capability declaration and inheritance;
- determinism guarantees and explicitly nondeterministic inputs.

Create a conformance corpus before implementation:

```text
fixtures/futureshell/
  syntax/valid/
  syntax/invalid/
  plans/
  contracts/
  workspaces/
  receipts/
```

Each valid source fixture has a canonical formatted form and expected plan
snapshot. Invalid fixtures identify error codes and source spans, avoiding
brittle full-message snapshots.

### 4.2 Parser strategy

Use a purpose-built lexer and recursive-descent or parser-combinator parser in
Rust. Preserve byte offsets and line/column spans for every syntactic node.
Error recovery should report multiple independent errors without attempting
shell-style execution recovery.

The parser treats `shell` bodies as a distinct embedded language region. It
does not attempt to understand full shell semantics beyond locating the block
and explicit interpolation sites. Argv-native `run` remains the analyzable,
preferred form.

Initial commands:

```text
xshell-run check FILE
xshell-run fmt [--check] FILE...
xshell-run plan [--json] FILE
xshell-run run [OPTIONS] FILE [-- ARGS...]
xshell-run verify RECEIPT
xshell-run recover WORKSPACE
```

`check`, `fmt`, and syntax-only editor operations must never import executable
plugins, run commands, contact providers, or read undeclared project files.

### 4.3 Values and types

Start with a deliberately small set:

- `bool`, bounded `int`, `string`, `duration`, and byte `size`;
- normalized workspace-relative `path` and explicit `external_path`;
- homogeneous `list<T>` and string-keyed `map<T>`;
- `task<T>`, `result<T>`, `artifact`, `evidence`, `changeset`, `contract_result`,
  and `receipt` handles.

Do not add implicit string-to-command, string-to-path, or path-to-external-path
conversions. Values from environment variables, command output, agents, and
remote receipts remain marked as runtime values and cannot alter the static
capability envelope.

## 5. Plan and policy model

### 5.1 Canonical plan

The semantic checker lowers AST into a versioned plan containing:

- source/module content hashes and runtime compatibility range;
- normalized task identifiers and dependency edges;
- target host/session selectors;
- agent/model targets, lifecycle/state ownership, connector requirements and
  gateway-policy references;
- declared read/write/execute/network/credential/device capabilities;
- process, model, autonomy, token, cost and retry budgets plus their required
  assurance levels;
- transaction and checkpoint scopes;
- contract clauses and promotion policies;
- bounded dynamic slots whose values are supplied at runtime.

Canonical encoding must specify key ordering, number encoding, path encoding,
and schema version. The plan hash covers the canonical bytes. Human-readable
JSON is an inspection format; it is not canonical merely because it was
produced by `serde_json`.

### 5.2 Authorization

Planning and authorization are separate. The runtime compares the plan with:

- invoker policy;
- local xshelld policy when daemon-backed;
- session and workspace restrictions;
- available assurance/resource enforcement;
- interactive approval requirements.

The authorization result is itself typed evidence bound to the plan hash. A
remote host reauthorizes its plan fragment locally; a coordinator's approval
cannot exceed remote policy.

### 5.3 Static conflict analysis

The planner normalizes declared workspace paths and detects obvious concurrent
write conflicts. It rejects ambiguous parent/child overlaps unless the tasks
are ordered or an explicit serialization policy is selected. Runtime change
sets catch undeclared or data-dependent overlap that static analysis cannot.

## 6. Transactional workspace implementation

### 6.1 Backend interface

Define a backend trait around semantics, not platform mechanisms:

```rust,ignore
trait WorkspaceBackend {
    fn capabilities(&self) -> WorkspaceCapabilities;
    fn stage(&self, request: StageRequest) -> Result<StagedWorkspace>;
    fn checkpoint(&self, staged: &mut StagedWorkspace, name: &str)
        -> Result<Checkpoint>;
    fn changes(&self, staged: &StagedWorkspace, from: &Checkpoint)
        -> Result<ChangeSet>;
    fn promote(&self, staged: &StagedWorkspace, request: PromotionRequest)
        -> Result<PromotionReceipt>;
    fn discard(&self, staged: StagedWorkspace) -> Result<()>;
    fn recover(&self, journal: &Path) -> Result<RecoveryReport>;
}
```

The real types must avoid exposing unrestricted host paths to language code.
Handles carry transaction identity and cannot be fabricated from strings.

### 6.2 Portable backend first

The first backend supports bounded directory trees:

1. securely walk the source without following escaping symlinks;
2. record a canonical baseline manifest;
3. create a private staging directory on the same filesystem when possible;
4. clone/reflink regular files where supported, otherwise copy with configured
   byte/file limits;
5. execute tasks with the staged directory as their workspace;
6. walk again to produce a typed change set;
7. retain staged content until promotion/discard or lease expiry.

Platform acceleration:

| Capability | macOS | Linux |
|---|---|---|
| Fast file clone | APFS clone where supported | reflink (`FICLONE`) where supported |
| Atomic file replacement | same-filesystem rename | same-filesystem rename |
| Durable metadata update | file and directory sync | file and directory sync |
| Optimized overlay | future backend | future overlay/user-namespace or `fuse-overlayfs` backend |

Copy fallback is valid but may be expensive, so staging limits and an up-front
size estimate are required. No backend should silently cross mount points.

### 6.3 Manifest and change set

Every entry records a normalized relative path, kind, content hash where
applicable, size, executable/permission bits, symbolic-link target, and the
metadata subset supported by the selected policy. Directory traversal order is
canonical. Hashing is streaming and bounded against mutation races; a file
that changes while hashed causes staging or verification to retry within a
small bound and then fail.

Change kinds include add, modify, delete, rename-candidate, symlink change,
permission change, directory add/delete, and unsupported metadata. Rename
detection is advisory; promotion correctness does not depend on it.

### 6.4 Promotion protocol

Promotion uses an intent journal stored outside the staged tree but within a
runtime-owned state directory:

1. acquire a workspace-scoped lock or validate an optimistic generation;
2. re-read destination entries selected for promotion;
3. compare them to baseline identities and report conflicts;
4. write and durably sync the promotion intent;
5. materialize replacement objects beside their destinations;
6. atomically replace individual paths in a deterministic order;
7. sync affected directories;
8. write and sync completion state;
9. verify final identities and emit promotion evidence.

Crash recovery completes or safely reports partially applied promotion from
the journal. The API and UI must call this **crash-recoverable multi-file
promotion**, not atomic multi-file commit.

Deletion and permission changes require explicit inclusion. Selective
promotion never includes an undeclared parent directory merely because one of
its children was selected.

### 6.5 Assurance and isolation

The portable backend guarantees that the task's working tree is a separate
staged tree and that changes made inside it can be discarded. It does not by
itself stop a subprocess from opening an absolute path elsewhere, including a
path back into the destination workspace. Report this as `staged` assurance
and attach an ambient-write taint; do not claim that destination state was
protected from a malicious or mistaken subprocess.

Add isolation providers behind a separate interface. A provider reports which
filesystem, process, network, and resource restrictions it actually enforces.
Potential native, container, or VM mechanisms require platform spikes; the
runtime must not select one based only on executable presence. A program that
requires `isolated` fails before task execution if the host cannot provide it.

## 7. Process and task runtime

### 7.1 Process identity and evidence

Extend the process runner to resolve and open the executable before spawn where
the platform permits, recording:

- requested program and resolved path;
- executable content hash and metadata identity;
- argv as a structured list;
- cwd and workspace transaction identity;
- allowlisted environment keys and a digest of redacted values when required;
- start/end timestamps, exit reason and resource measurements;
- bounded stdout/stderr hashes, retained bytes and truncation state.

The runtime should distinguish “resolved identity before spawn” from stronger
guarantees that the exact opened object was executed. Receipts state the
available assurance.

Shell blocks additionally record shell identity, argv, command source hash,
and interpolation values after redaction. They never source interactive startup
files unless the program explicitly requests and policy permits it.

### 7.2 Scheduler

Use Tokio for asynchronous I/O and process supervision, with a scheduler that
owns every task handle. Required behavior:

- dependency-aware launch;
- lexical structured-concurrency scopes;
- cancellation propagation and bounded shutdown;
- deterministic task/event identifiers assigned from the plan;
- per-task stdout/stderr/artifact bounds;
- transaction cancellation on required audit failure;
- explicit retry policy with a new attempt identity for every execution;
- no background transactional writer after scope exit.

Do not use event arrival order as the canonical result order. Receipts order
tasks by plan identity and attempts by attempt number.

### 7.3 Resource controls

Create a platform-neutral request/report structure before adding enforcement.
Implement wall time and output/artifact limits first. Add process count, memory,
CPU and network controls only with tests proving platform behavior. Unsupported
requested hard limits cause preflight failure; optional limits are marked as
unenforced in evidence.

## 8. Contract engine

### 8.1 Pure evaluator

The contract engine receives:

- a checked contract expression;
- an immutable evidence index;
- a finalized change-set manifest;
- verifier-task results;
- trusted host/plan/transaction identities.

It returns a clause tree containing pass/fail/error, referenced evidence IDs,
and bounded diagnostic values. It has no filesystem, network, process, clock,
randomness, environment, or model access.

Initial predicates:

| Family | Examples |
|---|---|
| Task | completed, succeeded, exit code/signal, not timed out, attempt count |
| Execution | executable hash/path/version, argv match, required tool actually dispatched |
| Filesystem | path exists/type/hash/size, change kind, allowlist, no unsupported metadata |
| Data | UTF-8, JSON parse, JSON Schema, exact hash, bounded regex/value comparison |
| Artifact | provenance link, media type, render manifest, source/output hashes |
| Receipt | valid signature/checkpoint binding, expected host/plan/task, contract result |
| Policy | required assurance reached, capability use subset, absence/presence of taints |

Commands used to verify domain-specific results run as ordinary capability-
bounded verifier tasks. Their outputs become evidence before pure evaluation.

### 8.2 Contract result and promotion

A valid contract is necessary but not always sufficient for promotion. The
promotion policy also evaluates:

- requested selection versus contract-declared outputs;
- destination conflicts;
- assurance level;
- agent/external-side-effect taints;
- user or host policy requiring confirmation;
- receipt/audit availability.

This prevents `on valid { promote all }` from becoming an accidental blanket
authorization after an agent-directed task. Whole promotion is a separate
policy class and should be visibly noisy in plan output.

### 8.3 Receipt format

Define a versioned canonical receipt schema and domain-separated hashes. Keep
large evidence payloads and artifacts out of the receipt; refer to them by
content hash, size, media type, and trusted storage locator. Receipt verification
must work offline with trusted host/audit public keys and supplied artifacts.

Initial receipt authenticity can be obtained by binding its hash into the
existing signed audit checkpoint chain. A direct audit-service receipt
signature may follow once key separation, revocation, and schema versioning are
specified.

## 9. Agent targets, connectors, and gateway policy

Agent tasks begin only after deterministic tasks, workspace transactions, and
contracts share one end-to-end path. The implementation separates agent
identity from transport: a target is the model, persistent agent, or managed
workflow being used; a connector describes how the gateway communicates with
it.

### 9.1 Target and lifecycle model

Define a versioned `AgentTarget` plan type with three initial lifecycle modes:

| Mode | State behavior |
|---|---|
| `one_shot` | Create a bounded conversation from declared context and dispose it when the task ends. |
| `managed` | Create or attach to an xshell-managed workflow with an explicit name, owner, retention period, and reuse policy. |
| `persistent` | Attach to an externally managed agent identity, such as a Hermes or SybilClaw agent, whose native system owns its memory and lifecycle. |

The target includes a stable profile reference and may pin an explicit provider,
model identifier/revision, agent ID, or workflow definition. Gateway policy
decides whether the program may override profile defaults. Plan output must
show resolved and unresolved target fields before authorization.

Persisted agent memory is not part of the filesystem transaction. Sending a
message to a stateful target may mutate that memory even if the FutureShell
transaction is later discarded. Record an external-state taint unless the
connector supports a verified context checkpoint/fork/restore capability and
the selected policy uses it. `detach` and `dispose` are distinct operations;
ending a task must not silently destroy an externally owned agent.

Target placement and gateway placement are separate plan fields. Record
whether the gateway runs with the coordinator, task executor, remote xshelld,
or an external service, and derive explicit data-flow edges for every prompt,
context item, artifact, tool request/result, and response that crosses a trust
boundary.

### 9.2 Connector contract

Create a versioned connector trait or local RPC contract with a capability
document. The minimum interface is conceptually:

```rust,ignore
trait AgentConnector {
    fn descriptor(&self) -> ConnectorDescriptor;
    async fn resolve(&self, target: TargetRequest) -> Result<ResolvedTarget>;
    async fn open(&self, request: OpenAgentRequest) -> Result<AgentHandle>;
    async fn send(&self, handle: &AgentHandle, request: AgentRequest)
        -> Result<AgentEventStream>;
    async fn status(&self, handle: &AgentHandle) -> Result<AgentStatus>;
    async fn cancel(&self, handle: &AgentHandle, operation: OperationId)
        -> Result<CancelOutcome>;
    async fn close(&self, handle: AgentHandle, disposition: Disposition)
        -> Result<CloseOutcome>;
}
```

The concrete protocol must support bounded streaming and stable operation IDs.
Capabilities advertise, rather than assume:

- one-shot create, named create, attach, detach, dispose, reconnect and resume;
- model discovery or explicit model selection;
- structured tool proposals and gateway-mediated tool results;
- approval pause/resume and cancellation;
- context checkpoint, fork, restore and compaction;
- artifact/image attachment and bounded output types;
- authoritative input/output/cached token usage;
- authoritative or estimated cost reporting;
- native task/workflow status and native audit/receipt references.

Initial connectors can adapt the existing OpenAI-compatible and Ollama paths,
then add external-process and xshell-session connectors. Hermes and SybilClaw
connectors should be based on their supported APIs or stable CLIs rather than
screen-scraping interactive sessions. An opaque text-only connector remains
useful for advisory generation but cannot claim structured tool mediation,
durable cancellation, or execution evidence.

Connector code is untrusted with respect to policy. Prefer supervised
subprocesses or narrow local RPC for third-party connectors. Connector events
are validated, bounded, sequence-checked, and attributed before entering the
runtime evidence stream.

A persistent target may retain native tools or autonomous behavior outside the
connector. Capability negotiation must report whether native authority can be
disabled, constrained, observed, or neither. The gateway enforces only the
boundary it controls. Targets with unmediated authority receive an
ambient-agent taint and are advisory-only when policy requires fully mediated
tools.

### 9.3 Gateway policy

`xshell-agent-gateway` resolves agent declarations against user, host, and
session policy. Its policy schema covers:

- connector, endpoint, provider, model, agent and workflow allowlists;
- which fields a program may override and whether versions must be pinned;
- secret references and connector access to them;
- prompt, context, artifact and response size limits;
- data classification and allowed egress destinations;
- lifecycle operations, retention, persistent-memory mutation and context
  checkpoint requirements;
- allowed tool names, argument constraints and the route by which tools execute;
- approval ceiling and bounded automatic-approval grants;
- model rounds, tool dispatches, retries, concurrent requests, elapsed time,
  token usage and monetary cost;
- reconnect, retry, cancellation and ambiguous-outcome behavior;
- which connector-originated facts may enter contract evidence and at what
  assurance level.

The plan contains the requested policy; authorization produces the effective
policy after applying stricter host/session limits. A connector never receives
authority beyond that effective policy.

### 9.4 Autonomy and resource accounting

Avoid the ambiguous word “turn” in durable schemas. Track at least:

- **model round:** one agent request and its terminal response;
- **tool proposal:** one structured action proposed by the agent;
- **tool dispatch:** one proposal actually authorized and sent to an executor;
- **workflow step:** a connector-native step when the connector can identify it;
- **attempt:** one execution try under a retry policy.

An automatic-approval grant includes allowed tool constraints, maximum model
rounds, maximum tool dispatches, elapsed-time deadline, and exhaustion action.
Exhaustion may stop/fail or pause for a new human authorization. Renewing the
grant creates a new audited authorization ID; it does not mutate the original
budget retroactively.

Token and cost limits need assurance labels:

| Assurance | Meaning |
|---|---|
| `enforced` | The gateway can prevent another request or token stream from crossing the bound within documented granularity. |
| `provider_enforced` | The remote provider accepted a hard limit and reports enforcement. |
| `measured` | Authoritative usage arrives too late to prevent bounded overrun, but final usage is known. |
| `estimated` | Usage or price is locally estimated and may differ from billing. |
| `unavailable` | The connector cannot supply meaningful accounting. |

Cost plans pin a currency, price-source identity/version, timestamp policy, and
treatment of cached tokens and provider fees. A strict cost contract may
require provider-enforced or measured usage and fail preflight on a connector
that can only estimate. Cancellation is still required when any live bound is
reached, but the receipt records possible in-flight overrun.

### 9.5 Capability envelope

Lower an agent block into a task containing:

- resolved target, model, lifecycle, connector and state owner;
- prompt and approved context/artifact references;
- gateway-policy reference and effective autonomy grant;
- allowed tool names and per-tool argument constraints;
- read/write path grants within the staged workspace;
- executable allowlist and network/credential grants;
- round, approval, time, token, cost, retry, output and artifact budgets;
- required accounting, connector, execution and isolation assurance.

At runtime, each tool proposal is checked against this envelope before normal
xshell approval policy. Interactive approval may reduce risk or renew a
declared grant but never expands the program's static capability envelope.

### 9.6 Evidence boundary

Add plan/task/attempt/transaction, target, connector, operation, authorization,
model-round and tool-dispatch identifiers to execution events. Emit separate
events for target resolution, attach/create, request, usage, proposal, policy
authorization, human decision, dispatch, observed completion, context mutation,
detach/dispose and artifact registration. A tool proposal alone cannot satisfy
an “executed” predicate.

Connector claims are evidence about what the connector observed. They become
execution evidence only when the gateway or xshelld controlled the relevant
dispatch boundary, or when policy recognizes a connector-native attestation.
If an adapter does not expose trustworthy tool-call structure, it may be used
for advisory text generation but not for execution-evidence contracts.

### 9.7 Agent and connector taint

Transactions containing agent-directed execution are marked. Taint does not
make selective promotion invalid: a deterministic contract and explicit output
selection may still accept results. It raises the policy requirement for whole
promotion and for capabilities with external side effects.

Additional taints record persistent-context mutation, opaque connector
behavior, non-mediated native tools, unverifiable cancellation, unavailable
usage, cost estimation, and ambiguous remote workflow state. Contracts and
promotion policy may reject specific taints or require human acknowledgement.

## 10. Remote execution

### 10.1 Protocol surface

Introduce a distinct versioned FutureShell protocol or a clearly separated
session-protocol capability set for:

- capability/preflight query;
- plan-fragment submission and authorization response;
- transaction create/status/lease renewal;
- task event streaming and cancellation;
- content-addressed input/output transfer;
- evidence and receipt retrieval;
- contract status;
- idempotent promote/discard decision;
- recovery/status query after reconnect.

Do not encode programs as remote shell command strings. Send canonical plan
fragments and content-addressed inputs over the authenticated xshell transport.

### 10.2 Per-host ownership

The target xshelld creates the staged workspace, performs execution, writes
host-local audit events, evaluates local predicates, and constructs the host
receipt. It never sends an unsigned claim for the coordinator to reinterpret
as local evidence.

The coordinator may evaluate an aggregate contract over verified host receipts
and artifacts. Decisions are per-host and idempotent. A decision includes the
transaction ID, expected change-set hash, expected baseline generation, action,
and unique decision ID.

### 10.3 Failure semantics

Explicitly test and report:

- disconnect while tasks run;
- coordinator crash before any decision;
- crash after some hosts promote;
- expired transaction lease;
- duplicate or reordered decisions;
- host restart with staged state present;
- changed destination baseline;
- receipt or artifact mismatch;
- remote policy becoming stricter between plan and execution.

The initial system may leave some hosts promoted and others staged or
discarded. Status and recovery commands must expose this without calling the
aggregate operation atomic.

## 11. Audit evolution

Define FutureShell audit events in a schema review before implementation.
Likely event families:

- program loaded and plan finalized;
- authorization requested/decided;
- transaction staged/checkpointed;
- task/attempt started, output summarized, completed or cancelled;
- agent tool proposed/authorized/dispatched/completed;
- artifact/evidence registered;
- change set finalized;
- contract evaluated with clause results;
- promotion requested/authorized/started/completed/conflicted;
- transaction discarded/expired/recovered;
- receipt finalized and bound to a checkpoint.

Requirements:

- bump wire and disk versions for incompatible tagged variants;
- continue verifying every previously supported disk format;
- keep canonical receipt encoding independent from incidental Rust enum layout;
- redact secret values while retaining evidence that an authorized secret
  reference was used;
- content-address large data and capture retention/availability status;
- stop before irreversible task or promotion boundaries when required auditing
  fails.

## 12. Milestone execution plan

### Milestone FS0 — Specification package

Deliver:

- language reference draft and annotated examples;
- threat model and guarantee matrix;
- capability, evidence, change-set and receipt schema drafts;
- agent target/lifecycle, connector capability, gateway policy, autonomy budget,
  usage accounting and persistent-state semantics;
- filesystem backend spikes on APFS and representative Linux filesystems;
- isolation/resource-control capability spikes on macOS and Linux;
- accepted first vertical-slice fixture.

Review gates:

- security review of path, symlink, race and promotion semantics;
- language review by both human authors and agent-generated example authors;
- no claim of rollback or isolation without a testable invariant.

### Milestone FS1 — Parser, formatter and planner

Deliver:

- `xshell-language`, `xshell-plan`, and `xshell-run check|fmt|plan`;
- source diagnostics and conformance fixtures;
- canonical plan schema/hash implementation;
- capability normalization and static conflict analysis;
- documentation generation for language constructs.

Acceptance:

- parser/formatter round-trip and idempotence properties;
- golden plan hashes stable across macOS/Linux and repeated runs;
- malformed input never executes or performs provider/network access;
- fuzzing finds no parser panic or unbounded allocation under configured input
  limits.

### Milestone FS2 — Local deterministic runtime

Deliver:

- `xshell-runtime` scheduler and argv-native runner;
- explicit shell blocks;
- `spawn`/`await`, cancellation, wall-time and output bounds;
- typed task evidence and audit events;
- plan authorization preview.

Acceptance:

- concurrent tasks respect dependencies and stable result ordering;
- cancellation kills complete process groups and leaves no child writers;
- exact argv and shell identities appear in evidence;
- required audit failure stops at the next pre-action boundary.

### Milestone FS3 — Workspace transactions

Deliver:

- `xshell-workspace` portable backend;
- manifests, change sets, preview, discard and selective promotion;
- destination conflict detection;
- promotion journal, status and recovery command;
- explicit `staged` assurance reporting.

Acceptance matrix:

- add/modify/delete/rename/symlink/permission fixtures;
- nested directories, Unicode names, case-folding collisions and long paths;
- escaping symlinks, hard links, special files and mount boundaries;
- mutation during hash/walk;
- process crash at every promotion journal transition;
- concurrent destination edit before promotion;
- byte/file/count limits and disk-full behavior.

### Milestone FS4 — Contracts and receipts

Deliver:

- `xshell-contract`, core predicates and clause reports;
- JSON Schema verifier task support;
- taint and promotion policy;
- receipt generation, audit binding and offline verification;
- complete deterministic FEA vertical slice.

Acceptance:

- valid run selectively promotes only `analysis.json`;
- missing tool dispatch, nonzero exit, invalid JSON/schema, undeclared write,
  changed executable, tampered evidence, audit gap, conflict and cancellation
  each prevent promotion with a clause-level explanation;
- v1 receipt fixtures remain verifiable after schema evolution tests are added.

### Milestone FS5 — Agent tasks

Deliver:

- `xshell-agent-gateway`, agent-task lowering and runtime adapter bridge;
- versioned connector contract and capability negotiation;
- explicit model/agent/workflow targeting with one-shot, managed and persistent
  lifecycles;
- initial OpenAI-compatible, Ollama, external-process and xshell-session
  connectors, followed by API-supported Hermes and SybilClaw connectors;
- gateway policy for endpoints, models, secrets, egress, tools, lifecycle and
  persistent context;
- capability-constrained tool dispatch;
- model-round, automatic-approval, tool-dispatch, retry, time, token, cost,
  output and artifact budgets with assurance reporting;
- advisory review evidence;
- UI showing requested versus effective policy and actual agent choices/usage.

Acceptance:

- target, connector, lifecycle, state owner, model, budgets and assurance are
  visible in `plan` output before execution;
- one-shot agents are disposed while persistent agents are detached according
  to explicit lifecycle policy;
- persistent context mutation is evidenced and tainted unless protected by an
  accepted connector checkpoint/fork mechanism;
- agent attempts outside read/write/exec/network grants are denied before
  dispatch;
- automatic execution pauses or stops at the exact model-round/tool-dispatch
  grant boundary, and renewal receives a new authorization identity;
- hard token/cost requirements fail preflight on connectors that offer only
  estimated or unavailable accounting;
- prompt claims never satisfy execution predicates;
- valid deterministic evidence can accept selected agent-produced artifacts;
- whole promotion after agent activity follows the elevated policy path;
- disconnect/cancel/audit-failure behavior matches deterministic tasks.

### Milestone FS6 — Remote task fabric

Deliver:

- remote capability preflight and plan-fragment protocol;
- transaction leases/status/recovery;
- task streaming and content-addressed transfer;
- per-host receipt production and coordinator verification;
- aggregate contracts and independent idempotent decisions.

Acceptance:

- macOS-to-Linux and Linux-to-macOS workflows;
- heterogeneous agents and tool availability;
- all partition/crash/duplicate/conflict cases in section 10.3;
- clear mixed-outcome report with no distributed-atomicity claim.

### Milestone FS7 — Hardening and ecosystem

Deliver incrementally:

- optimized workspace and isolation providers;
- LSP/editor integration;
- signed module/package format and dependency lockfile;
- retention, garbage collection and quota management;
- reproducible CAD/viewer evidence;
- yapCAD/FEA standard modules and examples;
- service packaging and migration support for macOS/Linux.

## 13. Testing strategy

### Unit and property testing

- lexer/parser round-trip, formatter idempotence and bounded diagnostics;
- type checking, capability subset and task-DAG cycle/conflict detection;
- path normalization and manifest canonicalization;
- change-set and contract predicate algebra;
- canonical plan/receipt encoding and hash domain separation;
- journal transition state machine and idempotency.

Use property tests for path trees, task DAGs, contract expression trees and
journal interruption points. Fuzz source parsing, manifest decoding, receipt
verification and remote protocol framing.

### Integration testing

- fake deterministic executables with controlled writes and exits;
- fake agent adapter producing allowed and forbidden tool calls;
- connector conformance harnesses covering capability negotiation, lifecycle,
  reconnect, cancellation, usage, malformed/out-of-order events and size
  limits;
- autonomy-boundary tests for model rounds, tool proposals versus dispatches,
  approval renewal, retries, token granularity and in-flight cost overrun;
- persistent-agent tests proving detach/dispose distinction and context-mutation
  taint behavior;
- audit daemon loss/corruption at every action boundary;
- destination writers racing promotion;
- daemon restart with running, staged, decided and partially promoted state;
- SSH localhost plus cross-platform CI where available;
- bounded-output, timeout, cancellation and process-descendant cleanup.

### Security testing

- symlink/hard-link/path traversal and directory replacement races;
- environment/argument/diagnostic secret redaction;
- shell interpolation injection and hostile filenames;
- receipt substitution, replay, truncation and host/plan confusion;
- capability escalation through dynamic values or imported modules;
- resource exhaustion through source, output, file trees and evidence count;
- stale or duplicated remote promotion decisions.

### Compatibility testing

Maintain checked-in fixtures for every released plan, audit, receipt, manifest,
and remote protocol version. New readers must verify supported older disk
formats. Incompatible wire changes must fail during handshake, never after work
has started.

## 14. Operational state and recovery

Store runtime-owned state outside user workspaces, with private permissions and
stable identifiers:

```text
<state>/futureshell/
  transactions/<id>/
    plan.canonical
    baseline.manifest
    staged/
    changes.manifest
    promotion.journal
    status.json
  artifacts/<sha256>/...
  receipts/<id>.receipt
```

Exact layout is not a public API, but durable schema versions are. Staged
transactions have leases and retention policy. Garbage collection never removes
a transaction with an unresolved promotion journal or a receipt-retained
artifact. Recovery is explicit and audited.

## 15. Documentation deliverables

Alongside code, maintain:

- language reference and grammar;
- contract/evidence predicate catalog;
- capability and assurance reference;
- transaction, promotion and crash-recovery semantics;
- receipt schema and offline verification guide;
- macOS/Linux capability matrix;
- remote failure and recovery guide;
- security model with concrete non-rollback examples;
- cookbook containing deterministic, agent-assisted, CAD/yapCAD and multi-host
  workflows.

Examples must state their required assurance and external side effects. Avoid
examples that normalize `promote all` after an agent task.

## 16. Immediate backlog

The first implementation branch should contain documentation and test fixtures,
not a working evaluator. In order:

1. write `docs/futureshell-language.md` with the minimal grammar and examples;
2. write `docs/futureshell-threat-model.md` with assets, actors, boundaries and
   guarantee levels;
3. define draft JSON schemas for canonical plan inspection, evidence, change
   sets, receipts, clause reports, agent targets, connector capabilities,
   gateway policy, autonomy grants and usage reports;
4. create the deterministic FEA fixture without requiring real gmsh/FEniCS;
5. spike secure staged-tree creation and change detection on macOS/Linux;
6. decide canonical serialization after testing candidate encodings;
7. scaffold `xshell-language`, `xshell-plan`, and `xshell-run` only after the
   FS0 review gate.

This order keeps syntax, security claims and durable schema choices reviewable
before they become coupled to runtime code.

## 17. Definition of done for the first release

The first FutureShell release is complete when a user can:

1. inspect and approve the immutable plan and capabilities for a local script;
2. run deterministic processes inside a staged workspace;
3. observe bounded streaming output and cancel safely;
4. receive a complete change-set preview;
5. evaluate deterministic, explainable contract clauses;
6. selectively promote verified outputs without overwriting concurrent edits;
7. discard invalid staged work, with destination-write protection claimed only
   when the selected isolation provider enforces it;
8. verify an audit-bound receipt offline;
9. recover or diagnose any interrupted promotion;
10. obtain the same documented behavior on supported macOS and Linux systems.

Agent and remote tasks are roadmap milestones, not requirements for this first
transaction-and-contract release.
