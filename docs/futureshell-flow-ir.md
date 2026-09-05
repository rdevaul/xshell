# FutureShell Flow IR prototype

**Status:** Executable v0 prototype; not a stable wire format

**Implementation:** `crates/xshell-flow`

FutureShell Flow IR is a typed authoring graph shared by future textual and
graphical frontends. It is not an authorized execution plan. Both frontends
will lower to this representation, and the semantic checker will subsequently
lower a valid flow into an immutable plan.

```text
FutureShell text ----\
                      > Flow IR -> checked plan -> authorization -> runtime
graphical editor ----/
```

Generating FutureShell source from a flow remains useful for inspection and
export, but execution must not depend on generating and reparsing source text.

## v0 model

A flow contains:

- a typed external interface;
- named deterministic contract expressions;
- nodes with typed input and output ports;
- data, dependency, contract-route, and loop-feedback edges;
- lexical scope, parallel, transaction, and bounded-loop regions;
- editor annotations that do not affect semantic identity.

The initial node kinds are `input`, `output`, `task`, `gate`, `join`,
`promote`, and `discard`. A task points to a FutureShell program source and
declares its capability and resource envelope. A gate references a pure
contract expression. Route edges carry one of the closed gate outcomes
`valid`, `invalid`, `error`, or `cancelled`.

Contracts decide whether results may flow. Promotion remains a separate policy
operation so an intermediate successful contract does not accidentally make
staged files durable.

## Loop invariant

Ordinary data, dependency, and route edges must form a DAG. A graphical cycle
is legal only when its back edge is a `feedback` edge owned by a `loop` region.
Every loop declares:

- a positive maximum iteration count;
- a gate and outcome that terminate the loop;
- an explicit exhaustion policy;
- named feedback state.

The authoring graph can therefore show a natural cycle while the checked plan
contains a bounded template. Runtime iteration will instantiate that template
with deterministic identities, producing a finite execution DAG and receipt.

Each feedback destination accepts exactly one initial data producer and one
feedback producer. Unrestricted back edges are rejected.

## Semantic identity

The prototype provides a domain-separated SHA-256 semantic hash. It sorts
declarations, ports, region members, and capability lists, and excludes:

- canvas and editor annotations;
- node labels;
- source declaration order.

The v0 implementation uses normalized JSON bytes. This is suitable for
prototyping equivalence but is not yet the stable canonical encoding promised
for FutureShell plans and receipts. A schema revision may replace it.

## Try it

Four fixtures cover a linear pipeline, contract branch, parallel join, and
bounded refinement loop:

```sh
cargo run -p xshell-flow -- check fixtures/futureshell/flows/linear.json
cargo run -p xshell-flow -- hash fixtures/futureshell/flows/bounded-loop.json
cargo run -p xshell-flow -- normalize fixtures/futureshell/flows/branch.json
cargo run -p xshell-flow -- dot fixtures/futureshell/flows/bounded-loop.json > flow.dot
dot -Tsvg flow.dot > flow.svg
cargo test -p xshell-flow
```

Validation returns stable diagnostic codes and collects independent errors.
The current checks cover schema and identifier validity, uniqueness, port
existence and type compatibility, required inputs, node/contract/region
references, loop bounds and membership, feedback ownership, region-parent
cycles, and ordinary execution cycles.

## Deliberate omissions

The prototype does not yet:

- parse textual FutureShell;
- lower Flow IR into the immutable runtime plan;
- execute nodes or evaluate contracts;
- define artifact ownership and fan-out materialization;
- define join merge functions beyond the scheduling strategies;
- model nested-flow interface manifests or generic record types;
- assign transaction semantics across loop iterations;
- preserve unknown fields for forward compatibility.

Those omissions keep this slice focused on testing the core representation
before its syntax and durable encoding become compatibility commitments.
