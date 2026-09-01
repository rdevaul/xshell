# xshell: Product and Technical Specification

**Status:** Draft for review

**Target platforms:** macOS and Linux
**Primary users:** engineers and designers working with local and remote AI agents, source trees, data, and CAD assets.

## 1. Purpose

`xshell` is an agent-first, network-aware interactive shell. It provides one durable workspace for people and AI agents working across local and remote machines, while preserving direct access to the user's ordinary shell.

The product's defining idea is a **session fabric**: each host owns one or more persistent sessions, each session may use a different agent implementation, and the user can move among them through a single terminal interface.

`xshell` is not intended to emulate POSIX shell syntax or replace an existing shell implementation in its first releases. It is a terminal application that launches and delegates conventional commands to the user's configured login shell.

## 2. Goals

- Make natural-language interaction with a local or remote agent the default REPL action.
- Preserve predictable escape access to the user's usual shell.
- Connect authenticated hosts over SSH and resume persistent `xshell` sessions.
- Support heterogeneous agents through a stable adapter protocol.
- Make engineering artifacts—including source, images, charts, STL, STEP, DXF, and `.ycpkg` packages—easy to inspect and pass to multimodal agents.
- Support safe, explicit sharing of remote resources and files.
- Run well on macOS and mainstream Linux distributions without requiring a cloud control plane.

## 3. Non-goals for the initial release

- Replacing `bash`, `zsh`, or the OS terminal emulator.
- A globally transparent, POSIX-correct distributed filesystem.
- Automatic cross-host credential forwarding or unrestricted remote command execution.
- A general-purpose 3D modeling application.
- Hiding meaningful differences between agent providers, permission policies, or tool capabilities.

## 4. User interaction model

Input is classified before it is sent:

| Input form | Meaning | Example |
|---|---|---|
| Plain text | Send a message to the active session's agent. | `Render the current assembly and check for collisions.` |
| `$…` | Execute the remainder in the configured shell, in the active session's working directory. | `$git status --short` |
| `//…` | Execute an xshell control-plane command. | `//connect rich@mini.local` |

The prompt always displays the active session identity, its host connection state, and the current agent's approval mode. Shell output and agent messages are visibly distinct. An agent tool request is presented with its intended command, working directory, affected resources, and required approval.

## 5. Sessions and agents

A session is a named, persistent execution context with:

- host identity and SSH transport information;
- an agent adapter and its configuration;
- working directory and optional confined root;
- an approval policy and audit log;
- an optional local shell process / environment;
- explicitly shared resource grants.

Session identifiers use `host:session`, for example `laptop:default` or `mini:cad`. The local default session is created on first launch. Sessions are resumable after disconnects and may be supervised by a platform-native user service.

The host's `xshelld` owns live agent adapters, tool loops, shell processes, and
conversation mutation. Clients submit inputs and render a sequenced event
stream. Disconnecting a client does not terminate persistent work; reattachment
replays retained events from the client's last acknowledged sequence. Approval
requests remain bound to a stable turn and tool-call identity.

### 5.1 Identity, visibility, and future multi-user sessions

The initial session model has one owning OS user and one interactive controller. A stable opaque UUID identifies the session; its human-readable name is unique only within a `(host, user)` namespace. Local and remote sessions use the same descriptor and switching operations.

Sessions declare whether they are `host-only` or visible to authenticated fabric clients. Remote catalogs expose metadata only after SSH authentication and never expose prompts, credentials, or transcript content.

A future, explicitly created multi-user session may define an ACL of authenticated principals and roles. The initial reserved roles are `owner`, `operator`, and `viewer`. An operator may use only the session's restricted command/tool surface, while owner-only authority includes arbitrary shell access, policy changes, ACL management, session lifecycle, and model configuration. Every event and approval in such a session records the acting principal. Single-user sessions must not silently become multi-user sessions.

### 5.2 Context management

Conversation context is session-owned state rather than terminal-client state. The protocol reserves context status, compaction, checkpoint, restore, and fork operations. Compaction must be explicit and auditable: the user can inspect token/size pressure, the material selected for retention, the generated summary, and provenance linking the compacted context to its source messages. Provider-initiated truncation must never be presented as successful xshell compaction.

### 5.3 Agent adapter contract

Agents are integrated behind a versioned local RPC contract. A provider adapter declares capabilities rather than claiming a common feature set.

```json
{
  "protocol_version": "1",
  "agent": {"id": "local-ollama", "display_name": "Qwen / Ollama"},
  "capabilities": ["chat", "tool_calls", "filesystem.read", "image.input"],
  "approval_modes": ["ask", "restricted"],
  "working_directory": "/Users/rich/designs"
}
```

Core operations are: send message, stream events, request/cancel tool invocation, attach artifact, obtain session state, and stop/restart the provider. Initial adapters should target a local OpenAI-compatible endpoint or Ollama, plus a generic external-process adapter. Integrations for Berd, Codex, Hermes, and OpenClaw must be built only where their supported API/CLI semantics allow durable sessions and explicit approval boundaries.

## 6. Connection and lifecycle

`//connect <ssh-destination> [--session NAME]` uses the normal SSH configuration, host-key verification, and SSH agent of the invoking user. It first asks a small remote bootstrap program whether a compatible session manager is available.

- If a requested session exists, xshell attaches to it.
- If xshell is installed but the session does not exist, it offers to create it.
- If xshell is absent, it displays the signed release origin, version, checksum/signature result, installation location, and service changes before requesting confirmation to bootstrap.
- Bootstrap is never silent, never uses password capture, and never forwards credentials by default.

Remote transport is SSH port forwarding or a stdio subsystem; no inbound listening port is needed for the MVP. Session persistence is implemented with `launchd` user agents on macOS and `systemd --user` services on Linux. A detached fallback process is acceptable before service installation is approved.

## 7. Resource sharing and filesystem model

Every session has a private local root. `//chroot` changes the session's permitted root only after a user-authenticated, explicit confirmation. It does not bypass operating-system permissions.

Shared resources appear under a logical namespace:

```text
/xshell/<host>/<session>/<share-name>/...
```

This is a capability-backed virtual namespace, not an implicit mount of every remote disk. A share grant records source path, allowed principals, read-only/read-write mode, expiry, and whether agents may access it. `//share` creates or modifies grants; `//unshare` revokes them. Initial releases provide explicit file transfer and streamed reads. A FUSE/macFUSE mount is an optional later feature, not a requirement for correct operation.

## 8. Viewing, rendering, and multimodal artifacts

`//view <path-or-artifact>` opens a local viewer for either a local item or an item securely streamed through the existing SSH transport. Viewer plugins render an artifact in a sandboxed, format-aware process. The viewer returns bounded derived artifacts—such as PNG snapshots, extracted text, dimensions, camera metadata, or a render manifest—that users may attach to an agent.

### 8.1 Initial renderer set

| Artifact | Initial handling |
|---|---|
| Text, Markdown, source, JSON/CSV | Inline rich text, syntax highlighting, tables/plots where appropriate |
| PNG, JPEG, SVG, PDF | Inline image/PDF preview; image attachment to capable agents |
| STL, OBJ, PLY, GLB/GLTF, STEP/IGES | F3D-based render/export to PNG when available; structured metadata and render manifest |
| DXF | F3D render where supported; otherwise a dedicated converter/plugin |
| `.ycpkg` | Validate/extract manifest, show geometry and exports, then render preferred STEP/STL exports |

F3D is an external optional helper rather than a required dependency. xshell probes its availability and records the exact command, renderer version, source artifact hash, camera, lighting, and output hash in the render manifest. This makes agent review and visual-regression workflows reproducible.

Terminal inline graphics are a progressive enhancement. xshell detects supported protocols (for example Kitty graphics or iTerm2 inline images) and otherwise emits a file link plus `//view` instruction. Video and interactive 3D remain viewer responsibilities.

### 8.2 yapCAD workflow

yapCAD is a first-class target workflow, not a bundled dependency. xshell offers a configurable `yapcad` workspace profile that detects a project and supports:

- running DSL or Python models through the configured project environment;
- exporting STL, STEP, and DXF as agent-visible artifacts;
- rendering geometry with F3D or another selected renderer;
- inspecting and validating `.ycpkg` packages;
- attaching generated renders and manifests to capable agents for review.

Full BREP/STEP workflows require the user's yapCAD environment with OpenCascade/pythonocc support; xshell reports the capability state rather than silently falling back to lower-fidelity geometry.

## 9. Safety and trust model

The system separates three independent grants:

| Grant | Controls |
|---|---|
| Transport access | SSH host/user authentication and host identity |
| Resource access | Read/write access to explicitly shared paths and artifacts |
| Agent authority | Tools, shell execution, network use, install permission, and credential use |

Default policy is read-oriented and confirmation-based. Irreversible or broad actions—including deletion, package installation, service installation, modifying shared resources, credential use, or `//chroot`—require explicit confirmation. Each session maintains an audit record of inputs, model interactions, tool requests, approvals, results, and artifact provenance. Records are written by a privilege-separated service, hash-chained, and periodically signed. Checkpoints include blinded commitments that can later be submitted to federated or public witnesses. See [the audit design](docs/auditing.md) for the trust boundary and current capture limitations.

Secrets are not inserted into agent context by default. SSH agent forwarding is opt-in per connection and visibly indicated. Remote agents receive only artifacts and paths permitted by their session policy.

## 10. Core control commands

| Command | Intent |
|---|---|
| `//connect DEST [--session NAME]` | Attach/create a remote session over SSH |
| `//sessions` / `//switch ID` | List and select sessions |
| `//new NAME` | Create a session on the current host |
| `//detach` / `//close` | Detach from or terminate the active session |
| `//context …` | Inspect or manage session context and compaction |
| `//agent [show|set]` | Inspect or select an adapter/configuration |
| `//share PATH …` / `//unshare ID` | Grant or revoke resource access |
| `//chroot PATH` | Set a confined session root after confirmation |
| `//view TARGET` | View a local or remote artifact |
| `//attach TARGET` | Attach an approved artifact to the active agent |
| `//status` | Show connection, capabilities, grants, and policy |
| `//help` | Show command and provider-specific help |

## 11. Configuration and observability

Configuration is user-owned, human-readable, and separated into client, host, session, and adapter scopes. Secrets are referenced through OS keychains or user-selected secret providers, not written into configuration files. `//status --json` and a local diagnostic command provide machine-readable state suitable for support and automation.

Logs distinguish transport events, shell execution, agent events, approval decisions, and artifact/render provenance. Diagnostics redact credentials, prompt contents, and path data unless the user explicitly chooses to export them.

## 12. Success criteria for the MVP

- A user on macOS or Linux can run xshell, message a local configured agent, and run `$` commands in their normal shell.
- Two hosts connected by ordinary SSH can create, detach from, and resume a named remote xshell session.
- An agent adapter can expose its capabilities and request a confirmed tool action.
- A remote STL or STEP artifact can be securely rendered to a reproducible PNG locally through the viewer path and attached to a multimodal-capable agent.
- Users can inspect the active policy, grants, and audit trail without reading implementation logs.
