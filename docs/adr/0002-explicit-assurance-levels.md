# ADR 0002: Explicit assurance levels

**Status:** Accepted

**Date:** 2026-09-10

## Decision

Workspace assurance uses exactly `staged`, `isolated`, or `external`.
`staged` means a separate disposable tree but does not promise write
confinement. `isolated` is emitted only by a selected local provider that has
passed the denial and resource tests for every enforced claim. `external`
preserves provider-attested guarantees without relabeling them as local facts.

Evidence separately reports `enforced`, `provider_enforced`, `measured`,
`estimated`, or `unavailable`. Approval cannot upgrade an assurance label.

## Consequences

Capability probes may report available mechanisms but cannot select a stronger
level by inference. Preflight fails when a plan requires stronger assurance
than the selected provider can prove.
