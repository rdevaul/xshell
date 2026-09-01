# xshell: macOS and Linux Implementation Plan

**Companion document:** `xshell-specification.md`
**Planning assumption:** local-first, SSH-only connectivity, with CAD visualization as an early workflow.

## Delivery principles

- Build a reliable local single-host experience before distributed filesystem semantics.
- Keep the trusted computing base small: SSH plus a narrow, versioned xshell protocol.
- Favor capability discovery and explicit degradation over provider-specific guesses.
- Treat render output as a reproducible artifact with provenance, not a transient screenshot.
- Ship macOS and Linux together from the shared core; isolate service, keychain, terminal, and packaging differences behind platform interfaces.

## Proposed technical shape

Use a compiled, single-binary core (Rust is a strong fit for terminal UX, SSH/process control, and safe concurrent streams) with a local IPC/RPC protocol for adapters and viewer services. Keep agent adapters and renderers as independently versioned plugins or subprocesses.

```text
xshell CLI/TUI
  ├─ input router ($ shell, // control, agent message)
  ├─ session client + multiplexer
  ├─ SSH transport
  ├─ policy/approval engine + audit client
  └─ artifact client

xshelld (per host, per user)
  ├─ session registry and lifecycle
  ├─ adapter supervisor
  ├─ share/resource broker
  ├─ artifact staging and hashing
  └─ local RPC/SSH stdio endpoint

xshell-auditd (dedicated service account)
  ├─ durable append-only event writer
  ├─ hash chain + signed checkpoints
  └─ blinded witness commitments

xshell-viewer (local)
  ├─ sandboxed renderer runners
  ├─ F3D renderer adapter
  └─ derived-artifact / render-manifest producer
```

The exact language should be validated with a brief spike. A Python implementation is viable for an early prototype, particularly for yapCAD integration, but the host daemon, terminal, and transport boundary should not depend on the Python environment used by CAD projects.

## Platform support matrix

| Area | macOS | Linux |
|---|---|---|
| Service persistence | `launchd` user agent | `systemd --user` unit |
| Secure secret reference | Keychain | Secret Service/keyring, with file-based opt-in fallback |
| SSH | System OpenSSH | System OpenSSH |
| Terminal enhancements | iTerm2/Kitty where available | Kitty/WezTerm and other detected protocols |
| Viewer launch | App bundle or standalone binary | Desktop entry/standalone binary |
| CAD runtime | User-managed Conda/Mamba environment | User-managed Conda/Mamba environment |
| Packaging | signed/notarized universal or arch-specific package | deb/rpm/tarball initially, distro packages later |

The baseline must work in a plain terminal with no inline image protocol and on a headless Linux host; rendering may happen on the connected local client rather than the remote host.

## Phases

### Current implementation checkpoint

The local foundation of Phase 3 is implemented: a versioned Unix-socket
protocol, stable host/session identities, a single-controller registry,
ephemeral/daemon/durable lifecycles, local session commands, model/cwd/chat
snapshot restoration, daemon-owned agent and shell turns, approval rendezvous,
cancellation, bounded event replay after reconnect, authenticated SSH stdio
transport, multi-host catalogs, and cross-host switching. Signed remote
bootstrap, reconnect supervision, daemon-side audit appends, and platform
service installation remain Phase 3 work.

The first terminal-UX increment below is also implemented: protocol v4 provides
bounded executable and path completion against the active local or remote
session without evaluating shell code.

### Terminal interaction and rendering plan of record

Terminal UX will advance in this order:

1. **Safe remote completion (implemented).** Add a bounded protocol request
   that discovers executable names and filesystem paths on the session host. It
   must parse tokens without evaluating them, must not source shell completion
   frameworks or interactive startup files, and must apply candidate,
   directory-entry, input-size, and response-size limits.
2. **Agent Markdown rendering (structure implemented).** Render model responses
   incrementally by complete Markdown block, with terminal-width wrapping,
   fenced-code syntax highlighting, ANSI sanitization, color/NO_COLOR policy,
   and a plain-text fallback. Shell output remains verbatim unless formatting
   is explicitly requested. The first increment supplies safe block/inline
   rendering and fenced-code presentation; language-aware syntax highlighting
   remains the next rendering increment.
3. **PTY-backed user shell commands.** Give directly entered `$` commands a
   pseudoterminal while agent-requested tools remain noninteractive and
   pipe-backed. Forward binary-safe output, input, resize, signals, and terminal
   restoration. `cat file.json | jq | less` is the first acceptance case.
4. **Persistent full-screen PTYs.** Preserve and reattach interactive PTYs,
   including redraw after reconnect. Truecolor, alternate screen, resize,
   bracketed paste, mouse input, signals, and an explicit terminal-escape trust
   policy are required; `emacs -nw` with a real user configuration is the
   acceptance target.

Native zsh/bash completion frameworks remain opt-in future work because their
scripts execute user and third-party code. Remote completion initially covers
only executable names and paths derived by `xshelld` without shell evaluation.

### Phase 0 — Architecture spikes and contract (1–2 weeks)

Deliverables:

- Repository, CI for macOS and Linux, style/lint/test baseline, threat model.
- Written protocol v1: handshake, version negotiation, session operations, streamed events, capability document, approval request, artifact transfer.
- Prototype input classifier and transcript renderer.
- Validate SSH stdio/subsystem approach, reconnect behavior, and terminal capability detection.
- Validate F3D invocation on target macOS/Linux architectures using representative STL and STEP assets.

Exit criteria: a local prototype can classify all three input forms; a client and dummy daemon negotiate protocol and stream an event over SSH; F3D produces a PNG and a complete manifest from a known artifact.

### Phase 1 — Local agentic shell (2–4 weeks)

Deliverables:

- `xshell` TUI/REPL with `plain text`, `$`, and `//` routes.
- Config loader and onboarding for a local OpenAI-compatible/Ollama-style adapter.
- External process adapter contract and capability display.
- Approval UI, conservative defaults, and append-only local audit log.
- `$` execution through the configured user shell with cwd and environment handling.
- `//status`, `//agent`, `//help`, `//view` for text/image/PDF.

Exit criteria: a user can use xshell as a local daily engineering terminal without losing normal shell access; blocked tool requests and approvals are clear and tested.

### Phase 2 — Artifact and CAD visualization path (2–4 weeks)

Deliverables:

- Content-addressed artifact staging with SHA-256 hashes and metadata.
- Local viewer/runners and render-manifest schema.
- F3D renderer plugin for STL, OBJ, PLY, GLB/GLTF, and available CAD formats; clear availability/error reporting.
- Inline-image protocol adapters with an external-viewer fallback.
- `//attach` flow: selected source artifact plus bounded derived render/metadata passed to a multimodal-capable agent.
- yapCAD workspace profile: project detection, configured command templates, output discovery, `.ycpkg` manifest inspection/validation hooks.

Acceptance example: from a yapCAD project, the user asks the agent to export a design, invokes `//view exports/design.step`, receives a labeled rendered PNG with a manifest, and can attach that image plus selected metadata to the active agent.

### Phase 3 — SSH session fabric (3–5 weeks)

Deliverables:

- `xshelld` per-user daemon and session registry.
- SSH handshake with strict host-key behavior and normal `~/.ssh/config` compatibility.
- `//connect`, `//new`, `//sessions`, and `//switch`; detach/reconnect semantics.
- Explicit remote bootstrap flow using signed, version-pinned releases.
- Per-platform service installers: `launchd` and `systemd --user`.
- Remote artifact streaming to the local viewer; no inbound service port.

Exit criteria: two machines can resume a named session across SSH; the remote agent may differ from the local agent; a remote CAD export renders locally with the same provenance guarantees as a local one.

### Phase 4 — Controlled sharing and multi-session UX (3–5 weeks)

Deliverables:

- Grant model, principal identity, expiry, policy inspection, and revocation.
- `//share` / `//unshare` and read-only streamed browsing under the logical `/xshell` namespace.
- Explicit uploads/downloads and approved remote write operations.
- Session switcher; optional terminal-native panes only where the terminal/host capability allows it.
- Complete audit viewer/export and policy regression tests.

Do not add a FUSE/macFUSE filesystem in this phase. Evaluate it only after the resource model has real user validation.

### Phase 5 — Hardening and ecosystem adapters (ongoing)

Deliverables:

- Provider adapters for supported agent systems with documented lifecycle and permission semantics.
- Sandboxing hardening for renderers and untrusted document/media parsers.
- Signed update channel, compatibility guarantees, migrations, telemetry only with opt-in.
- Optional FUSE/macFUSE projection of explicitly shared resources.
- Visual regression test framework for CAD rendering, including reference assets and renderer-version tolerances.
- Explicit multi-user sessions with ACL-backed owner/operator/viewer roles, restricted operator commands, principal-aware approvals, and per-principal auditing.
- Context inspection, auditable compaction, checkpoints, restore, and session forks.

## Workstreams and ownership boundaries

| Workstream | Key decisions / risks |
|---|---|
| Terminal UX | Prompt grammar, streaming/interrupt rules, copy/paste behavior, accessibility, fallback rendering |
| Session daemon | Protocol compatibility, restart behavior, state durability, concurrent clients |
| Security | SSH trust, release verification, policy semantics, audit integrity, secrets redaction |
| Agent adapters | Stable neutral contract without flattening provider safety behavior |
| Artifacts/viewer | Sandboxing, formats, cache quota, rendering determinism, multimodal attachment bounds |
| CAD/yapCAD | Conda environment discovery, export conventions, BREP availability reporting, visual test fixtures |
| Release engineering | macOS signing/notarization, Linux packaging, auto-update trust chain |

## Testing strategy

- Unit tests for parser routing, configuration, policy evaluation, protocol encoding, manifests, and path/share validation.
- Integration tests that launch an isolated local daemon over SSH localhost.
- macOS and Linux CI jobs for every shared-core change; native service lifecycle tests in platform runners where available.
- End-to-end tests with a fake adapter that exercises streaming, tool approvals, failures, and reconnects.
- Artifact security tests for malformed assets, hostile filenames, archive traversal, render timeouts, and resource limits.
- CAD fixture suite containing small, redistributable STL/STEP/DXF and `.ycpkg` samples. Assert manifest structure and image similarity with renderer/version-aware tolerances—not exact pixels across every GPU/OS combination.

## Decisions to resolve before Phase 1 completion

1. Confirm the core language/runtime after the Phase 0 SSH/TUI spike.
2. Define the first supported agent endpoint and its durable-session requirements.
3. Select the initial viewer distribution model (bundled native app versus standalone binary).
4. Set the minimum supported macOS and Linux distribution/GLIBC versions.
5. Decide whether local adapters are trusted in-process components or always supervised subprocesses; prefer subprocess isolation initially.
6. Define data-retention defaults for transcripts, audit records, and artifact caches.

## Milestone-based prioritization

The first externally useful release is the end of Phase 2: a local agentic engineering shell that can execute conventional commands safely and give both people and multimodal agents reproducible views of yapCAD/CAD outputs. The first networked release is the end of Phase 3. Cross-host writable shared filesystem behavior is intentionally deferred until its permission and failure semantics have been proven.
