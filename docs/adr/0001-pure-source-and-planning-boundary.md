# ADR 0001: Pure source and planning boundary

**Status:** Accepted

**Date:** 2026-09-10

## Decision

Parsing, formatting, checking, import resolution, Flow lowering, catalog
resolution, and plan construction operate only on declared bounded inputs.
They never execute user code, invoke providers, search ambient paths, or read
undeclared project files. Module and catalog identities are content bound in
the resolved plan.

## Consequences

FS1 commands can run safely as offline analysis. Convenience discovery that
would make plan identity environment-dependent is rejected. Dynamic values may
narrow a checked plan but cannot enlarge its authority.
