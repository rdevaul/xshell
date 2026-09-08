# FutureShell FS0 schema package

**Status:** Provisional semantic schemas; not stable wire formats

The machine-readable bundle at
`fixtures/futureshell/schemas/fs0.schema.json` uses JSON Schema 2020-12 and
defines the initial capability, evidence, change-set, clause-report, receipt,
agent-target, connector-capability, gateway-policy, autonomy-grant, and usage
report shapes. The bundle exists to make examples and reviews precise before
canonical encoding is selected.

All durable objects carry an exact `schema` discriminator. Unknown versions and
unknown fields fail closed. SHA-256 values are lowercase hex. IDs are opaque,
bounded UTF-8 strings; runtimes assign them and language code cannot fabricate
handles from IDs. Timestamps are evidence metadata, never canonical ordering.

Plans and receipts must eventually define canonical byte encoding separately
from this human-readable JSON representation. Secret values, raw credentials,
unbounded output, prompts containing secrets, and artifact bytes never belong
in these objects. Content is referenced by hash, size, media type, and optional
policy-approved locator.

## Evidence trust

Evidence `issuer` and `assurance` determine which predicates may consume it.
An `observed` fact is produced at a trusted runtime boundary; `reported` comes
from a connector/provider; `advisory` is an untrusted judgment. The predicate
catalog, host policy, and receipt verifier decide whether a fact is eligible.

## Change sets and promotion

Change sets are finalized, canonically ordered manifests relative to one
filesystem checkpoint. A rename is only an advisory pairing of delete/add
identities. Promotion correctness uses individual before/after identities.
Unsupported metadata is explicit and prevents automatic promotion unless policy
names an exception.

## Receipts

Receipts summarize identity, authorization, evidence references, clause report,
taints, and promotion outcome. They do not embed large evidence or artifacts.
Authenticity initially uses a receipt hash bound into a signed audit checkpoint;
the schema leaves direct signatures optional pending key lifecycle design.

## Agent policy objects

Agent targets distinguish one-shot, managed, and external persistent state.
Connector capabilities state which lifecycle, mediation, resume, checkpoint,
and usage features actually exist. Gateway policy constrains program requests;
an autonomy grant is a separately identified authorization whose counters never
reset silently. Usage fields each carry `enforced`, `provider_enforced`,
`measured`, `estimated`, or `unavailable` assurance.

These schemas are an FS0 review artifact, not permission to implement against
them as a compatibility commitment. Changes remain expected until the FS0 gate
and canonical-encoding decision.

## Persistent-state rules

Lifecycle, state ownership, and close disposition are independent fields but
only these initial combinations are valid:

| Lifecycle | Owner | Open/reuse | Close | Mutation treatment |
|---|---|---|---|---|
| `one_shot` | `xshell` | `new` | `dispose` | State exists only for the task |
| `managed` | `xshell` | `new`, `attach_named`, or `reuse_named` | `detach` or `retain` under a positive retention bound | Recorded outside the filesystem transaction |
| `persistent` | `connector` or `external` | `attach_named` or `reuse_named` | `detach` or `retain`, never implicit disposal | `deny`, explicit taint, or verified connector checkpoint |

Planning rejects any other combination. A named or persistent target is not
rollback-safe merely because its filesystem artifacts are staged. Source may
request a lifecycle, but the resolved profile and gateway policy supply the
state policy; source cannot weaken the mutation or close policy. `detach`
ends FutureShell's attachment, `retain` keeps xshell-managed state under its
lease, and `dispose` irreversibly destroys only xshell-owned one-shot state.
