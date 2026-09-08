# FutureShell Plan V0 prototype

**Status:** Implemented planning and resolution prototype; not an authorized
runtime format

**Implementation:** `crates/xshell-plan`

**Project progress:** [FutureShell status and traceability](futureshell-status.md)

Plan V0 is the first checked lowering target for FutureShell Flow IR. It makes
task eligibility, value selection, region ownership, bounded-loop state,
capabilities, and unresolved execution dependencies explicit without running a
program or probing the environment.

```text
Flow IR -> validate -> lower -> immutable Plan V0 -> resolve -> authorize
```

Plan V0 remains distinct from the authoring graph and the future authorized
runtime plan. Flow IR retains editor structure. Plan V0 contains deterministic
templates and normalized policy inputs. Its pure resolver can bind program and
predicate identities from caller-supplied provisional catalogs, after which
authorization can compare a fully resolved plan with invoker and host policy.

## Task identity and order

Every Flow node becomes one task template. A concrete runtime task will be
identified by:

```text
(plan hash, task template id, loop iteration path, attempt number)
```

Tasks serialize in deterministic topological order, with lexicographic IDs
breaking ties. Serialization order does not force otherwise independent tasks
to run sequentially. Feedback edges are represented in loop state and excluded
from ordinary topological ordering.

## Readiness and activation

A task has three independent readiness components:

- `data`: predecessor values, using `all` for ordinary nodes and `any` for
  `any` and `first_valid` joins;
- `after`: ordering-only predecessors, all of which must finish;
- `activate_on_any`: contract routes, any one of which can activate the task.

An empty activation set means always active. Flow V0 expresses route
conjunction through an explicit downstream gate or join instead of assigning
implicit AND semantics to multiple route edges.

Plan V0 records activation but does not yet define the complete runtime status
algebra for skipped branches. Totality analysis for required outputs reached
through only some outcomes remains future work.

## Loop lowering

Bindings crossing loop boundaries carry an explicit selection mode:

| Selection | Meaning |
|---|---|
| `direct` | Producer and consumer are outside a loop. |
| `loop_initial` | An outside value initializes a loop consumer. |
| `current_iteration` | Producer and consumer use the same iteration. |
| `previous_iteration` | Named feedback from the previous iteration. |
| `loop_final` | An outside consumer receives the accepted final value. |

Each loop template contains a deterministic body order, positive iteration
bound, exit gate and outcome, exhaustion policy, and named states. Every state
identifies its type, target port, outside initial producer, and inside feedback
producer.

Plan V0 rejects nested loops and direct bindings between different loop
regions. Those require an explicit nested iteration-path design.

## Regions and transactions

Region membership becomes a root-to-leaf `region_path`. Multiple regions
containing one task must form a lexical parent chain. The innermost transaction
becomes the task's transaction owner. Plan V0 records ownership and normalized
workspace roots but does not stage files or claim rollback protection.

## Capabilities and conflicts

Read and write capabilities use `/`-separated workspace-relative paths.
Lowering removes `.` components and rejects absolute paths, `..`, empty
components, and backslash separators. Lists are sorted and deduplicated per
task and across the plan.

The first conflict pass rejects obvious overlapping write scopes between
unordered tasks. It compares exact paths, parent/child paths, and literal
prefixes before globs conservatively. Ordered tasks may share a scope. Direct
alternatives selected by different outcomes of one gate are recognized as
mutually exclusive.

This is not complete glob-intersection or route-dominance analysis. Runtime
change sets remain necessary for data-dependent and undeclared overlap.

## Resolution blockers

Lowering records every unresolved program source and unchecked predicate
signature. Such a plan is inspectable and hashable but has
`resolution.fully_resolved: false`.

The resolver accepts an `xshell.program-catalog/v0` and an
`xshell.predicate-catalog/v0`. Program manifests declare a source SHA-256,
default and named entrypoints, typed interfaces, and required capabilities.
Predicate definitions declare a version, implementation SHA-256, and exact
JSON argument signature. The resolver:

- selects the requested or default entrypoint;
- requires task input and output ports to match its typed interface exactly;
- requires every program capability to appear in the Flow task's declared
  envelope, using exact normalized capability entries in V0;
- rejects missing, extra, or incorrectly typed predicate arguments;
- embeds program-manifest, source, predicate-definition, and implementation
  identities in the plan; and
- rebuilds the blocker set, allowing a completely covered plan to report
  `resolution.fully_resolved: true`.

Missing definitions remain blockers so catalogs may be supplied incrementally.
Definitions that are present but malformed or incompatible are errors. Catalog
declaration order does not affect resolution or hashes.

The library resolver is side-effect-free: it consumes already-decoded values
and performs no filesystem, process, provider, or network access. The CLI reads
only the Flow and catalog paths explicitly supplied by the user. The fixture
catalogs contain declared source and implementation hashes even though their
programs do not exist yet; a future module loader and authorization policy must
verify source bytes and catalog provenance. A fully resolved Plan V0 is still
not an authorized or executable plan.

## Semantic hash

The plan hash uses domain-separated SHA-256 over normalized Plan V0 JSON. It
covers the Flow hash, tasks, bindings, contracts, regions, capabilities,
resources, and resolution state. Layout, labels, and declaration ordering do
not change it; capability, resource, program, contract, and graph-semantic
changes do.

Normalized JSON is still a prototype encoding, not a durable canonical-format
commitment.

## Try it

```sh
cargo run -p xshell-plan -- build fixtures/futureshell/flows/linear.json
cargo run -p xshell-plan -- build fixtures/futureshell/flows/bounded-loop.json --json
cargo run -p xshell-plan -- hash fixtures/futureshell/flows/branch.json
cargo run -p xshell-plan -- build fixtures/futureshell/flows/branch.json \
  --program-catalog fixtures/futureshell/catalogs/programs.json \
  --predicate-catalog fixtures/futureshell/catalogs/predicates.json
cargo test -p xshell-plan
```

Readable golden artifacts for all four lowered Flow fixtures and the fully
resolved branch fixture live under `fixtures/futureshell/plans`.

The next increment is the FS0 specification package: minimal language and
typed-interface semantics, the threat model and guarantee matrix, draft
evidence/change-set/receipt schemas, and the deterministic FEA fixture. Runtime
values must never be able to expand the resolved static capability envelope.
