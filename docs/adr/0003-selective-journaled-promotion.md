# ADR 0003: Selective journaled promotion

**Status:** Accepted

**Date:** 2026-09-10

## Decision

Promotion applies only explicitly selected paths after contract, policy,
assurance, taint, declared-output, and destination-conflict checks pass. It
compares each destination identity to the baseline, writes durable intent
before replacement, syncs affected directories, and verifies final identities.
Recovery follows deterministic idempotent journal transitions.

Promotion is not an atomic multi-file commit. A crash can leave some paths
promoted and requires recovery. Deletes and permission changes require explicit
selection. External effects are outside this protocol.

## Consequences

Receipts report exact selected object identities and `promoted`, `conflicted`,
or `recovery_required` outcomes. Workflows requiring atomic external commit
must use a separately specified typed adapter.
