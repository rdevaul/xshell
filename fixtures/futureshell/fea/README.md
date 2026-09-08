# Deterministic FEA vertical-slice fixture

This is the accepted FS0 workflow fixture. It models meshing and solving with
small deterministic shell programs; it does not require gmsh, FEniCS, Python,
network access, or an agent. The fake tools exist to exercise FutureShell's
execution, evidence, transaction, contract, and promotion boundaries—not to
produce physically meaningful results.

Inputs live under `workspace/input`. A successful staged run invokes
`tools/fake-gmsh` and then `tools/fake-fenics`, validates `analysis.json` against
`schemas/fea-result-v1.schema.json`, observes only `mesh.msh` and
`analysis.json` in the change set, and promotes only `analysis.json`.

`scenarios.json` is the acceptance matrix. FS0 specifies each outcome; FS2–FS4
will turn the matrix into runtime integration tests. `expected/analysis.json`
is the byte-for-byte success output. `programs.json` pins the fake executable
identities, typed inputs and outputs, capability envelopes, resource bounds,
schema identity, allowed change set, and promotion selection. The textual
program is `../syntax/valid/minimal-fs0.fsh`.

The tools accept an optional final mode solely for a future fixture harness:
`success`, `fail`, `invalid_json`, `invalid_schema`, `undeclared_write`, or
`slow`. A production FutureShell program does not grant arbitrary environment
or mode control.

Run `./check-fixture.sh` to verify pinned tool/schema hashes, byte-for-byte
success output, task failures, malformed output, schema rejection, and the
undeclared-write signal. Cancellation, promotion conflicts, evidence tampering,
and audit gaps require the later runtime harness and remain specified scenarios
rather than claims about the current implementation.
