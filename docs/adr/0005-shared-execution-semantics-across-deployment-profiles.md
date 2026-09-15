# ADR 0005: Shared execution semantics across deployment profiles

**Status:** Accepted

**Date:** 2026-09-13

## Context

xshell serves both personal and home-lab use and controlled information
environments. Those deployments need different policy owners, enforcement
providers, component sets, and qualification schedules, but they must not
develop different meanings for execution, cancellation, recovery, approval,
or evidence.

The design and security review recorded eleven implementation findings whose
correctness requirements apply to both environments. The companion development
roadmap records decisions D1–D9. This ADR makes those decisions durable before
the affected lifecycle and authorization interfaces become compatibility
surfaces.

## Decision

1. xshell has one core product and development line. A narrowly maintained
   qualification branch may exist, but it must not redefine shared execution
   semantics.
2. Cancellation cleanup, truthful outcomes, bounded operations, and recovery
   bookkeeping are unconditional. No deployment or approval profile may
   disable them.
3. Every model, tool, process, file, view, attachment, and export dispatch
   passes through the same authorization interface. Personal policy may grant
   broadly; it does not bypass that interface.
4. Authority has an explicit owner. Personal policy may be user-owned;
   controlled policy is protected from clients and workloads.
5. Policy permission and execution assurance are separate. A profile name does
   not prove isolation, egress control, or any other provider capability.
6. Data restrictions survive model and deployment-profile changes. Relaxation
   or export requires separate release authority.
7. Providers, cryptographic implementations, identity systems, and packaged
   component sets may differ without changing common operation semantics.
8. Personal convenience is an acceptance condition. Home-lab use does not
   require enterprise identity or mandatory compliance-audit infrastructure.
9. Personal and controlled releases qualify independently from the same shared
   semantics and security-fix line.

The following terms are distinct:

- **deployment profile:** validated policy ownership, required guarantees, and
  permitted providers/components for one deployment class;
- **model profile:** a resolvable model destination and credential reference;
- **policy owner:** the authority permitted to establish or relax policy;
- **principal:** an authenticated person, controller, service, or workload;
- **data domain:** restrictions inherited by source and derived data until an
  authorized release changes them;
- **provider:** the implementation that performs an operation and reports its
  actual enforced capabilities;
- **assurance:** evidence-backed strength of an observed enforcement property.

The `ask`, `auto`, and `off` settings are approval preferences, not deployment
profiles. In particular, `auto` cannot relax provider requirements and `off`
does not establish isolation.

## Lifecycle and storage boundary

Operational execution history and compliance audit evidence are separate.
Operational state determines whether work is accepted, running, stopping,
finished, cancelled, failed, or has an unknown outcome. It must retain observed
effects needed for safe recovery even when optional audit capture is disabled.
Audit evidence authenticates records accepted by the audit boundary and may
have stronger protection and retention requirements.

For the current session modes:

- ephemeral sessions retain operational facts only while attached;
- daemon sessions retain them across controller detach while the daemon lives;
- durable sessions persist recovery-relevant conversation and terminal outcome
  across daemon restart;
- no mode may report a turn as cancelled or failed by erasing already observed
  effects;
- a graceful daemon stop resolves or explicitly marks active outcomes before
  closing audit streams;
- an ungraceful stop may recover as `outcome_unknown`; it must not silently
  present interrupted work as idle success.

The current JSON snapshot store is not the final lifecycle journal. Its
transactional replacement and crash-durability requirements remain finding F7.
Incremental execution facts may use a dedicated journal or transactional store,
but conversation is always a projection of authoritative execution facts rather
than their substitute.

## Exceptions

A future source split requires evidence that a deployment-specific maintenance
or qualification obligation cannot be met with providers, packaging, feature
selection, or a bounded qualification branch. Any exception must identify how
shared security fixes and lifecycle conformance remain synchronized and must be
approved in a superseding ADR.

## Consequences

Near-term work repairs F1–F11 before broadening the FutureShell runtime or
language surface. Personal releases may continue without controlled-deployment
qualification, but neither release path may weaken the common lifecycle tests.
Claims about controlled deployment remain scoped to an actual selected provider
and its deployment evidence.
