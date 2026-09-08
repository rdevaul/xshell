# FutureShell FS0 platform spikes

**Status:** Reproducible capability probe; not an isolation implementation

The FS0 spike asks a narrower question than FS3 implementation: which native
mechanisms can accelerate a portable staged-workspace design, and which
security claims require a separate enforcement provider? The executable probe
is [`scripts/futureshell-workspace-probe.sh`](../scripts/futureshell-workspace-probe.sh).
CI runs it on the existing macOS and Ubuntu jobs so results stay visible as
runner images change.

## Probe contract

The probe creates a new private temporary directory, copies or clones a regular
file into a staged tree, detects a symlink without following it, and performs a
same-filesystem temporary-file replacement. It reports filesystem, clone,
native-sandbox, and resource-control mechanisms. The first three operations
must succeed; an accelerator or sandbox mechanism may honestly report as
unavailable.

This is deliberately not a proof of descriptor-relative traversal, confinement,
durable journaling, crash recovery, or atomic multi-file promotion. Those need
provider code and adversarial FS3 tests before the `isolated` label is valid.

## APFS observation

The probe was run on 2026-09-08 on macOS 26.6.2 arm64 with an APFS data volume.
The staged copy, symlink inspection, and same-filesystem replacement passed;
`cp -c` selected clonefile-backed copying. The host exposes process rlimits.
Whether the deprecated `sandbox-exec` command happens to be present is only a
capability observation and is not a supported FutureShell isolation design.

APFS cloning is an optimization: baseline and staged files must never share a
writable hard link, and correctness must be identical when cloning is absent.
Promotion still needs descriptor-relative no-follow walks, before-identity
checks, a durable intent journal, directory synchronization, and final hash
verification.

## Linux observation

The same probe runs on the repository's Ubuntu x86-64 and arm64 CI hosts.
Linux reports the actual filesystem type, whether a reflink succeeds, whether
an unprivileged user namespace can actually be created, and whether cgroup v2
controllers are visible. Reflink and user-namespace availability can vary by
runner policy, so neither is required for the probe to pass.

The portable backend therefore starts with bounded copying plus rename-based
per-path replacement. A future Linux isolation provider may combine mount and
user namespaces, Landlock or another reviewed LSM boundary, seccomp, and
cgroup v2. Presence checks are insufficient: the provider must test the exact
denials and limits it reports as enforced.

## Design result

| Concern | Portable `staged` baseline | Candidate acceleration/enforcement | Claim permitted now |
|---|---|---|---|
| Workspace creation | Bounded regular-file copy | APFS clonefile; Linux reflink | Separate disposable tree |
| Path safety | Normalized relative paths; reject special files | Descriptor-relative no-follow APIs | Design invariant only |
| Promotion | Per-path baseline comparison and replacement | Platform-specific rename primitives | Design invariant only |
| Crash safety | Durable intent journal and recovery states | Filesystem-specific sync tuning | Design invariant only |
| Write confinement | None | Reviewed OS sandbox provider | No `isolated` claim |
| CPU/memory/process limits | Report unavailable or measured | rlimits; cgroup v2; platform provider | Mechanism presence only |
| Network denial | None | Reviewed OS sandbox/provider boundary | No denial claim |

FS0 concludes that `staged` is the portable initial assurance level. `isolated`
must be a separately selected provider whose receipt names enforcement tests;
the existence of APFS clones, namespaces, rlimits, cgroups, or sandbox commands
does not upgrade assurance by itself.
