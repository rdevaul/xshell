# ADR 0004: Explicit canonical plan encoding

**Status:** Accepted

**Date:** 2026-09-10

## Context

Plan V0 originally hashed compact JSON emitted by `serde_json`. The fixture
corpus made that output deterministic in the current implementation, but the
bytes still depended on Rust field declaration order, Serde enum attributes,
and omission behavior. Human-readable JSON remains useful for inspection, but
it is not an appropriate durable identity format.

The encoding spike exercised all four Flow fixture families by lowering them
to plans, then exercised all five Plan artifacts, including the fully resolved
branch. Compact deterministic JSON and an explicit typed binary layout had the
following payload sizes:

| Plan fixture | JSON bytes | Binary bytes |
|---|---:|---:|
| `linear.plan.json` | 3,235 | 1,150 |
| `branch.plan.json` | 4,953 | 1,634 |
| `parallel-join.plan.json` | 3,919 | 1,221 |
| `bounded-loop.plan.json` | 5,689 | 1,935 |
| `branch.resolved.plan.json` | 5,583 | 2,186 |

Both candidates were deterministic on this corpus. The binary candidate was
selected because its layout is defined independently of presentation JSON and
is substantially smaller. Size is a secondary benefit rather than the basis
of identity.

## Decision

Plan hashes use the explicit `xshell.plan.canonical/v1` binary encoding. The
encoding begins with the eight bytes `FSPLAN`, NUL, `0x01`. The remainder is a
typed positional encoding of Plan V0. Plan and nested struct fields appear in
the order fixed by the encoder, not by serialized JSON object order. Enum tags
are explicit `u8` values fixed by the encoder. Adding or reordering Rust fields
or changing Serde attributes does not implicitly change canonical bytes.

The primitive rules are:

- unsigned `u32` and `u64` integers use fixed-width big-endian bytes;
- signed `i64` integers use fixed-width two's-complement big-endian bytes;
- booleans use one byte, `0` or `1`;
- strings are a `u32` byte length followed by their unmodified UTF-8 bytes;
- lists are a `u32` element count followed by elements in semantic plan order;
- optional values use a one-byte tag, `0` for absent and `1` followed by the
  value for present;
- maps use a `u32` entry count and ascending UTF-8 byte order for keys;
- all lengths that do not fit in `u32` fail closed.

Strings receive no implicit Unicode normalization. Composed and decomposed
spellings therefore remain distinct unless an earlier language or lowering
rule explicitly normalizes that field. Paths are encoded as strings after the
plan's path validation and normalization rules.

Predicate arguments retain JSON-compatible values. Their tags are null `0`,
false `1`, true `2`, `u64` `3`, `i64` `4`, IEEE-754 binary64 `5`, string `6`,
array `7`, and object `8`. Float bits are big-endian and negative zero is
normalized to positive zero. Non-representable numbers fail closed. Object
keys use ascending UTF-8 byte order recursively.

Only `xshell.plan/v0` is accepted by this encoding. A future incompatible plan
schema or canonical layout receives a new magic/version and hash domain rather
than silently reinterpreting bytes. The Plan V0 hash is SHA-256 over:

```text
"xshell.plan.semantic.fsplan-v1\0" || canonical_plan_bytes
```

The human-readable `PlanArtifact` JSON carries this hash but is not itself the
hashed representation. Flow IR semantic hashing remains its existing
provisional, presentation-independent JSON mechanism; this decision fixes the
durable checked-plan identity required by FS1.

## Verification

`fixtures/futureshell/plans/canonical-v1-vectors.json` stores the exact
canonical bytes and domain-separated hash for every Plan fixture. Tests also
cover integer endianness, UTF-8 behavior, object-key order, float negative
zero, unsupported schema rejection, declaration-order independence, and
semantic-change sensitivity. The byte vectors are platform-neutral and must
match unchanged on macOS and Linux.

## Consequences

Plan identity no longer depends on `serde_json` output or Serde representation
details. Schema changes now require deliberate codec and vector review. The
explicit encoder is more verbose than generic serialization, but the code and
golden vectors make every compatibility decision reviewable.
