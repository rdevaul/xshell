# FutureShell roadmap

**Status:** Draft plan of record

**Companion documents:** [concept note](futureshell.md),
[implementation plan](futureshell-implementation-plan.md), and
[xshell specification](../xshell-specification.md)

## 1. Vision

FutureShell turns xshell into a network-transparent agent scripting framework.
It combines familiar shell-like composition with explicit agent tasks,
structured concurrency, bounded execution, transactional filesystem state, and
contracts evaluated from deterministic evidence.

An ordinary skill is guidance: an agent may follow it, misunderstand it, or
claim to have completed it. A FutureShell program is executable policy. It
defines what may run, where it may run, which resources it may use, what
evidence completion requires, and which staged results may become durable.

The interactive xshell REPL and FutureShell remain separate products built on
shared infrastructure. The REPL stays optimized for direct human interaction;
the `xshell-run` runtime executes repeatable programs without making the REPL
dependent on an immature language implementation.

## 2. Product principles

1. **Familiar, not bash-compatible.** Comments, pipelines, variables, quoting,
   redirection, exit status, and process composition should feel recognizable
   to shell users. FutureShell has its own grammar and does not inherit bash's
   implicit expansion, startup-file, or evaluation semantics.
2. **Deterministic control surrounds probabilistic work.** An agent may propose
   commands or produce artifacts, but control flow, permissions, contract
   evaluation, and promotion are runtime decisions.
3. **Evidence, not assertion.** A hard contract is satisfied only by typed,
   verifiable evidence. An LLM judgment may be recorded as advisory evidence
   but cannot by itself authorize a commit.
4. **Stage first, promote explicitly.** Filesystem changes occur in a managed
   transaction. Selective promotion is the default; promoting the entire diff
   is an explicit higher-risk operation.
5. **Guarantees have named boundaries.** Files outside the transactional root
   and effects on networks, devices, services, databases, credentials, and
   other hosts are not described as rollback-safe.
6. **Remote execution does not imply distributed atomicity.** Each host owns
   its transaction and produces a signed receipt. Initial releases aggregate
   results without claiming all-or-nothing cross-host commit.
7. **The plan is inspectable before it runs.** Programs can be parsed, checked,
   capability-reviewed, and rendered as an execution plan without invoking an
   agent or subprocess.
8. **macOS and Linux are first-class.** Platform-specific accelerators may
   differ, but assurance levels and observable behavior remain explicit.
9. **Agent identity and autonomy are planned resources.** A program selects an
   agent target, lifecycle, communications connector, approval envelope, and
   enforceable budgets; it never inherits an unspecified ambient agent.

## 3. Non-goals

The initial language and runtime will not:

- parse or execute arbitrary bash programs as FutureShell programs;
- make arbitrary external side effects reversible;
- infer hard contract clauses from natural-language promises;
- silently grant an agent ambient filesystem, network, credential, or shell
  access;
- claim atomic commit across multiple hosts;
- require a FUSE mount or a particular container engine;
- replace the interactive xshell command language.

## 4. Core vocabulary

| Term | Meaning |
|---|---|
| **program** | A parsed FutureShell source file and its imported modules. |
| **plan** | The checked task graph, capabilities, budgets, contracts, and promotion policy derived from a program. |
| **task** | One shell, process, agent, viewer, verifier, transfer, or nested FutureShell operation. |
| **transaction** | A bounded execution scope with a base filesystem state and a private writable staging layer. |
| **checkpoint** | A named reference to a transaction's base or intermediate staged state; it is distinct from an audit signature checkpoint. |
| **change set** | The typed filesystem diff since a checkpoint, including content and relevant metadata hashes. |
| **evidence** | A typed fact emitted by a trusted runtime boundary, such as process exit, executable identity, artifact hash, schema validation, or filesystem diff. |
| **contract** | A deterministic predicate over evidence and declared outputs. |
| **promotion** | Applying selected staged changes to the destination workspace after contract acceptance. |
| **receipt** | A canonical, verifiable statement of plan identity, execution evidence, contract result, change-set identity, and promotion outcome. |
| **capability** | An explicit grant to read, write, execute, connect, use a credential, control a device, or invoke an xshell service. |
| **taint** | A recorded fact that execution could have caused an effect outside the rollback boundary. |
| **agent target** | A selected model endpoint, persistent agent, or managed agentic workflow, including its identity and state owner. |
| **connector** | A versioned communications adapter between the FutureShell agent gateway and an agent system. |
| **gateway policy** | The rules governing which targets and connectors may be used and how messages, tools, approvals, data, state, and budgets cross that boundary. |
| **autonomy budget** | A bounded grant of unattended model rounds and tool dispatches before the task must stop, fail, or request renewed approval. |

Audit checkpoints prove the integrity and ordering of audit records. Filesystem
checkpoints name transaction states. The language and UI must always qualify
which kind is meant.

## 5. Language direction

### 5.1 Source and invocation

Executable scripts use:

```text
#!/usr/bin/env xshell-run
```

`#!xshell` may be accepted as a source marker when a file is passed explicitly,
but it is not a portable executable shebang because kernels expect an
interpreter path.

The initial language includes:

- `#` comments and shell-familiar strings, lists, maps, variables, and
  interpolation;
- typed `let` bindings and immutable values by default;
- argv-native `run` for deterministic process invocation;
- an explicit `shell` form for pipelines and other shell syntax;
- `agent` tasks with prompts, tools, budgets, and capabilities;
- `spawn` and `await` with structured-concurrency rules;
- `transaction`, `checkpoint`, `contract`, `require`, `promote`, and `discard`;
- conditionals, bounded iteration, functions, and imports introduced only as
  required by real workflows;
- xshell control operations through typed runtime APIs, not by reparsing
  interactive `//` command strings.

Argv-native execution is preferred because it avoids an extra shell parser and
records exact executable arguments. `shell` blocks are available when shell
composition is genuinely useful, but their plan and receipt identify the
selected shell and the fact that a command string was evaluated.

### 5.2 Illustrative syntax

The grammar will be specified before implementation; this example communicates
semantics rather than freezing punctuation:

```xshell
#!/usr/bin/env xshell-run

transaction prefea in workspace(".") {
    checkpoint input

    let mesh = await run ["gmsh", "model.geo", "-o", "mesh.msh"] {
        resources { timeout: 5m, memory: 4GiB }
        writes ["mesh.msh"]
    }

    let solve = await run ["fenics-run", "solve.py", "mesh.msh"] {
        requires mesh.success
        writes ["analysis.json"]
    }

    contract valid_analysis {
        require mesh.exit.success
        require solve.exit.success
        require evidence.exec("gmsh").count >= 1
        require evidence.exec("fenics-run").count >= 1
        require file("analysis.json").matches_schema("fea-result-v1")
        require changes.satisfy_allowlist(["mesh.msh", "analysis.json"])

        on valid   { promote ["analysis.json"] }
        on invalid { discard all }
    }
}
```

An agent task uses the same transaction and contract machinery:

```xshell
let proposal = await agent {
    target agent("hermes:cad-engineer")
    connector "hermes-local"
    lifecycle persistent
    model "qwen/qwen3-coder"
    prompt "Generate the solver inputs and run the approved FEA workflow."
    autonomy {
        auto_approve_rounds 4
        auto_approve_tools  12
        on_exhausted        request_approval
    }
    resources {
        timeout       15m
        input_tokens  100_000
        output_tokens 20_000
        cost          2.00 USD
    }
    capabilities {
        read  ["model/**"]
        write ["generated/**", "analysis.json"]
        exec  ["gmsh", "fenics-run"]
        network none
    }
}
```

Agent-produced tool calls do not escape those capabilities. If the platform
cannot enforce a requested isolation level, the runtime fails closed unless
the program and invoking policy explicitly permit a weaker level.

### 5.3 Agent targets, lifecycles, and connectors

An agent declaration specifies more than a prompt. Its checked plan identifies:

- a target by stable profile, model, agent, or workflow identity;
- an optional explicit model and provider revision, subject to gateway policy;
- whether the target is a one-shot conversation, an xshell-managed persistent
  workflow, or an existing externally managed persistent agent;
- the connector used for messages, streaming events, tool proposals,
  approvals, cancellation, status, resume, and artifact exchange;
- context and artifact inputs plus data-egress restrictions;
- tool, filesystem, network, credential, and device capabilities;
- model-round, automatic-tool-dispatch, elapsed-time, token, cost, retry,
  output, and artifact budgets;
- behavior when a budget is exhausted: stop, fail, or request a new explicit
  authorization grant.

One-shot agents begin with a declared context and are disposed at task end.
Managed workflows may be created by the program and retained under an explicit
name and retention policy. Existing persistent agents—such as Hermes or
SybilClaw agents—retain identity and memory owned by their native system.
Changing that external memory is a side effect outside the filesystem
transaction. It produces a taint unless the connector offers a verified
checkpoint/fork/restore operation that the gateway policy accepts.

Agent placement is independent of task placement. A local transaction may use
a cloud model or an agent on another xshell host; a remote task may use a
gateway on the coordinator or execution host. The plan identifies the gateway
location and each prompt, context, artifact, tool, and result boundary that
crosses a host or provider trust boundary.

The agent gateway is the policy enforcement point between FutureShell and
connectors. A connector advertises capabilities and assurance rather than
pretending every agent system has equivalent semantics. The minimum useful
connector contract covers identity, create/attach/detach, send, event stream,
cancel, status, and usage. Tool mediation, durable resume, context fork,
artifact attachment, and authoritative usage/cost reporting are optional
capabilities.

Budgets are described as hard only when the gateway can enforce them. For
example, the gateway can cap model requests and mediated tool dispatches, but
a provider that reports token or cost usage late may support only measured or
estimated spend. The plan shows this before execution, and a contract may
require a particular budget-assurance level.

Gateway policy also controls connector allowlists, endpoint and model
allowlists, secret references, outbound data classes, prompt/artifact size,
tool mediation, approval ceilings, persistent-state mutation, retries,
reconnection, and whether connector-supplied claims qualify as contract
evidence. Opaque text-only connectors can support advisory agent work but
cannot substantiate tool-execution contracts.

A pre-existing persistent agent may retain native tools and authority that the
gateway cannot revoke. In that case the gateway can bound only the operations
it mediates; the unmediated authority is reported as an ambient-agent taint.
Policy may restrict that target to advisory work. A strict workflow instead
uses a scoped one-shot/managed instance with native tools disabled or a
connector that can prove equivalent enforcement.

## 6. Execution model

FutureShell execution has distinct stages:

```text
source -> parse -> type/capability check -> immutable plan -> authorization
       -> stage transaction -> execute tasks -> collect evidence
       -> evaluate contracts -> preview change set -> promote/discard
       -> finalize audit records and receipts
```

The plan receives a content hash. Runtime events bind to that plan hash, task
identity, transaction identity, host identity, and source revision. Dynamic
agent choices may add concrete tool invocations, but only beneath a statically
declared capability envelope and resource budget.

### 6.1 Structured concurrency

`spawn` creates a child task owned by the enclosing scope. A scope cannot
finish while children are unobserved: each child must be awaited, cancelled,
or explicitly detached. Detached work cannot retain a transactional write
capability.

The checker computes declared read/write sets where possible. Concurrent tasks
with overlapping write scopes are rejected or serialized unless the program
selects an explicit conflict policy. Event order and result aggregation remain
deterministic even when task execution is concurrent.

### 6.2 Resource bounds

Each task may constrain wall time, CPU time, memory, process count, output
bytes, artifact bytes, network destinations, model rounds, automatically
approved tool dispatches, retries, input/output tokens, and monetary cost. A
receipt states which bounds were enforced, which were merely measured or
estimated, and which were unavailable on that host or connector. “Bounded”
must never mean “best effort” without an assurance label.

## 7. Transactional filesystem model

### 7.1 Staging instead of backup-on-write

The runtime executes against a private staged view of a declared workspace. It
records a baseline manifest and computes the resulting change set without
modifying the destination workspace during task execution. This captures
writes made by arbitrary subprocesses inside the staged view, unlike wrapping
selected core utilities.

The portable first backend materializes a bounded staging tree, using native
copy-on-write clones or reflinks when available and a verified copy fallback.
Optimized backends may later use Linux overlay facilities, APFS clones, or
other platform mechanisms. Backend choice and assurance are included in the
receipt.

The manifest accounts for regular files, directories, symbolic links,
permissions, and supported metadata. Device nodes, sockets, special files,
mount boundaries, hard-link semantics, case-folding collisions, and extended
attributes require explicit policy. Unsupported cases fail closed rather than
quietly weakening rollback.

Staged regular files must not retain hard links to source-workspace inodes.
Internal hard-link relationships may be reconstructed when the backend can do
so safely. A symbolic link that resolves outside the staged tree is rejected by
default; permitting one requires an explicit external-path capability and
prevents the transaction from claiming that link-mediated writes are
rollback-safe.

### 7.2 Promotion

Selective promotion applies an allowlisted subset of the staged change set.
Before promotion the runtime:

1. verifies the selected staged objects against their evidence hashes;
2. checks that destination paths have not changed since the baseline;
3. presents the exact add/modify/delete/metadata diff required by policy;
4. records authorization when human confirmation is required;
5. applies changes with per-path atomic replacement where the platform allows;
6. verifies and records the resulting destination hashes.

A base-state mismatch produces a conflict; the runtime does not overwrite
concurrent work. Multi-file promotion is crash-recoverable through a promotion
journal, but is not described as universally atomic.

`promote all` is available but carries a higher policy class. If any task was
agent-directed or obtained a capability for effects outside the staged
filesystem, the transaction is tainted. Whole-change-set promotion then
requires explicit policy authorization or human confirmation even when its
contract is valid.

### 7.3 Rollback boundary

Discarding a staging layer reliably removes changes inside that layer. It does
not reverse:

- writes outside the declared workspace;
- network requests or remote service mutations;
- database transactions not integrated through a transactional adapter;
- package/service changes, notifications, prints, device motion, or consumed
  materials;
- actions independently taken by a remote host.

These effects require capabilities and produce taints and evidence. Future
adapters may provide compensating actions, but compensation is not rollback
and is reported separately.

## 8. Contracts and evidence

Contracts are typed expressions over a closed evidence bundle. Initial
evidence predicates cover:

- task start, completion, cancellation, timeout, and exit status;
- resolved executable path, executable content hash, version probe, argv, cwd,
  and a redacted environment description;
- agent request, proposed tool call, policy decision, actual tool dispatch, and
  tool result;
- input, output, and artifact content hashes and sizes;
- filesystem change-set membership and allowlist compliance;
- text/JSON parsing, JSON Schema validation, checksums, and bounded command
  verifiers;
- viewer/render manifests and artifact provenance;
- verified receipt identity from another host.

Evidence about execution must be emitted at the boundary that performs the
operation. An agent saying “I ran gmsh” is transcript content; xshelld recording
the resolved executable immediately before spawn and its observed exit is
execution evidence.

Contract evaluation is pure, deterministic, bounded, and free of ambient I/O.
External checks run as verifier tasks first and contribute typed evidence.
Every clause produces a result with its supporting evidence identifiers, so a
failed contract is explainable rather than merely false.

LLM review may produce a signed or hashed advisory report, such as a visual
assessment of a render. A contract can require that the review occurred and
record its result, but the initial hard predicate set cannot treat the model's
subjective conclusion as sufficient authority to promote state.

## 9. Audit records and receipts

FutureShell extends the audit schema with program, plan, transaction, task,
evidence, contract, change-set, promotion, conflict, and receipt events. Large
outputs remain content-addressed artifacts rather than being duplicated into
the event stream.

The contract evaluator consumes verified typed events through a library API;
it does not grep human-readable logs. Canonical receipts contain at least:

- language/runtime and schema versions;
- program and immutable plan hashes;
- host, session, transaction, checkpoint, and task identities;
- capability grants and actual resource-assurance levels;
- evidence and change-set Merkle roots or canonical hashes;
- each contract clause and result;
- taints, authorization decisions, and promotion outcome;
- references to the enclosing signed audit checkpoints.

The existing audit service remains the append-only witness. A later extension
may have `xshell-auditd` sign compact execution receipts directly or bind their
hashes into signed checkpoints. Public or federated witnessing anchors those
commitments; it does not execute contracts or make unsafe work reversible.

## 10. Network-transparent execution

The coordinator resolves a host/session selector through the existing xshell
fabric and sends a versioned plan fragment, not an interpolated SSH command.
The remote xshelld checks local policy, capabilities, executable availability,
resource enforcement, and transaction-backend support before accepting it.

Each host:

1. stages and owns its transaction;
2. executes its task graph under local policy;
3. evaluates host-local contracts;
4. produces a signed or audit-bound receipt;
5. retains staged outputs until an explicit promotion or discard decision, with
   a bounded expiry.

The coordinator verifies receipts and may evaluate an aggregate contract. It
then issues independent decisions to each host. A crash or partition can yield
mixed outcomes, and the result reports them honestly. Idempotent decision IDs,
leases, status queries, and recovery make this manageable without calling it
atomic. A true distributed prepare/commit protocol remains future research.

## 11. Assurance levels

Every transaction reports one of these initial filesystem assurance levels:

| Level | Meaning |
|---|---|
| `staged` | Tasks use a separate staged tree, and changes made within that tree can be discarded. Ambient writes—including writes back to the destination by absolute path—are possible and are not rollback-safe. |
| `isolated` | Platform enforcement confines filesystem writes to declared writable roots and applies the requested process/network restrictions. |
| `external` | Execution is delegated to a container, VM, or service with its own attested isolation and artifact-return boundary. |

Contracts may require a minimum level. Platform capability discovery must make
the difference visible before authorization. The roadmap does not assume that
macOS and Linux offer identical native sandbox mechanisms.

## 12. Roadmap

### FS0 — Semantics and threat model

Define the grammar, type/value model, task and agent lifecycles, connector and
gateway-policy contract, capability vocabulary, transaction invariants,
contract predicate semantics, evidence schema, receipt schema, budget units,
and assurance levels. Build executable examples from real engineering
workflows, including a minimal yapCAD/FEA example.

**Exit:** the documents answer what is parsed, authorized, executed, evidenced,
committed, and recoverable for every example and failure path.

### FS1 — Language toolchain and dry-run planner

Create the parser, source spans, diagnostics, formatter, AST, semantic checker,
module loader, and immutable plan builder. Implement `xshell-run check`, `fmt`,
and `plan`; none may execute code while processing a program.

**Exit:** valid programs produce stable canonical plans and hashes; invalid or
overbroad capabilities produce precise diagnostics on macOS and Linux.

### FS2 — Deterministic local task runtime

Execute argv-native processes and explicit shell blocks without agents or
filesystem transactions. Add structured concurrency, cancellation, bounded
stdout/stderr/artifacts, resource assurance reporting, and typed task evidence.

**Exit:** a concurrent local workflow is replayable at the plan/evidence level,
and every process is attributable to a plan task and audit event.

### FS3 — Transactional workspaces

Implement the portable staging backend, baseline/change manifests, diff
preview, discard, selective promotion, conflict detection, promotion journal,
and recovery. Add platform acceleration only behind the same backend contract.

**Exit:** arbitrary test subprocesses can add, modify, delete, rename, and
change permitted metadata inside a staged workspace; rejection leaves the
destination unchanged, and accepted allowlisted outputs are verified after
promotion.

### FS4 — Deterministic contracts and receipts

Implement the pure contract evaluator, core predicates, verifier tasks,
evidence references, canonical receipt generation, audit binding, taint policy,
and `xshell-run verify`.

**Exit:** the FEA example commits `analysis.json` only when the required tools
actually ran, exited successfully, produced an allowed change set, and emitted
schema-valid output. Tampered evidence or receipts fail verification.

### FS5 — Agent tasks

Implement the agent gateway and connector contract, then expose xshell agent
adapters, persistent agents, and managed workflows through capability-bounded
`agent` tasks. Bind every proposed and executed tool action to its plan,
transaction, and evidence identity. Add lifecycle/state policy, model and
connector selection, autonomy/time/token/cost budgets, and advisory
model-review evidence.

**Exit:** one-shot and persistent agent targets can be selected explicitly; an
agent may choose how to produce a result but cannot exceed the program's
capability and autonomy envelopes or cause promotion without a valid
deterministic contract. Persistent context mutation and budget assurance are
visible in the receipt.

### FS6 — Session-fabric execution

Add versioned plan submission, task/event streaming, remote capability
discovery, staged-output retention, receipt return, artifact transfer, and
independent promotion/discard decisions over the existing SSH transport.

**Exit:** one program coordinates local and remote tasks, verifies per-host
receipts, and reports partial failure without claiming distributed atomicity.

### FS7 — Hardening and engineering workflows

Add policy profiles, signed module/package provenance, cache and retention
controls, optimized filesystem backends, stronger isolation adapters, IDE/LSP
support, reproducibility tooling, and first-class yapCAD/CAD/FEA libraries.

**Exit:** representative macOS and Linux engineering workflows have documented
assurance, recovery, and portability behavior and can be safely shared as
versioned FutureShell packages.

### Reserved research

- distributed prepare/commit with leases, fencing, and recovery;
- federated or public audit witnessing;
- transactional adapters for databases and other external systems;
- reproducible VM/container execution with stronger attestation;
- content-addressed remote caches and shared resource namespaces;
- proof-carrying or independently reproducible computational results.

## 13. First vertical slice

The first end-to-end demonstration should remain deliberately small:

1. parse and plan one local FutureShell program;
2. stage a temporary workspace containing fixture inputs;
3. run two argv-native deterministic tools;
4. record their executable identities, exits, and output hashes;
5. validate one JSON result against a bundled schema;
6. require that only declared paths changed;
7. selectively promote the JSON result;
8. emit an audit-bound receipt and verify it offline;
9. demonstrate that command failure, schema failure, undeclared writes,
   cancellation, and destination conflicts all leave the destination safe.

Agent execution and remote execution follow this slice rather than entering
the trusted transaction and contract core at the same time.

## 14. Success criteria

FutureShell is ready for serious use when:

- a reviewer can understand a program's capabilities and promotion policy
  without reading prompts;
- a reviewer can identify the selected agent/model, lifecycle, connector,
  gateway policy, autonomy allowance, and budget assurance before execution;
- the runtime never describes a best-effort boundary as enforced isolation;
- contracts produce clause-level deterministic explanations;
- failed work leaves staged state discardable, and workflows requiring proof
  that the destination or other host paths were untouched require `isolated`
  assurance;
- promoted files match the hashes and types accepted by the contract;
- receipts can be verified independently of the live session;
- local and remote tasks use the same language and evidence model;
- macOS and Linux differences appear as explicit capabilities and assurance,
  not hidden behavioral drift.
