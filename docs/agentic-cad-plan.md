# Agentic CAD readiness: plan and level of effort

**Status:** Proposed
**Updated:** 4 November 2026

## 1. Summary

This plan makes xshell practical for agent-driven engineering and CAD work.
Three capabilities are missing:

1. **Images:** an agent can see images, from a file the user attaches or a
   render a tool produces, and the model receives them in the format its
   provider expects.
2. **Configurable limits:** long engineering tasks can run under limits
   chosen per task. These range from today's conservative defaults to a
   *relaxed* mode, where elapsed time is the main bound and the count limits
   are greatly raised but still finite.
3. **Headless operation:** scripts can run xshell tasks without a terminal,
   read structured progress and results, and handle approvals and
   cancellation programmatically.

| Workstream | Core LoE | With optional items |
|---|---|---|
| A. Images and multimodal models | 15–22 engineer-days | 23–35 |
| B. Configurable and relaxed limits | 7–10 | 13–19 |
| C. Headless mode | 15–22 | 18–27 |
| **Total** | **37–54 days (about 8–11 weeks)** | **54–81 days (about 11–16 weeks)** |

The figures are for one engineer who knows the codebase, and include tests,
documentation, and review fixes. Add 2–3 days for a security review of
headless approvals and relaxed limits (§6). The recommended order is B, then C, then A
(§7). B unblocks long tasks within about two weeks, and C's event schema and
exit codes should be fixed before images add new event types.

## 2. Where xshell stands

| Area | Current state | Gap |
|---|---|---|
| Message model | `ChatMessage.content` is a `String` (`xshell-core`) | No way to carry an image |
| Providers | OpenAI-compatible and Ollama adapters send text only | Neither adapter sends image parts or `images` |
| Viewing | `//view` reads regular UTF-8 files up to 4 MiB | No image, PDF, or CAD viewing; nothing reaches the model |
| CAD | The original implementation plan's Phase 2 (F3D rendering, `//attach`, yapCAD profile) is not built | Not started |
| Turn limits | `TurnLimits::default()`: 128 provider requests, 256 tool calls, 64 per response, 60 minutes, set in code (#101) | Not configurable; no per-task or relaxed profile |
| Tool time limits | `run_shell` is fixed at 60 s (`SHELL_TOOL_TIMEOUT`) | CAD exports and renders routinely take longer |
| Long tasks | Auto-continue capped at 2 per turn; persistent goals are proposed in `xshell-md-continuity.md`, not built | No supported way to run hundreds of turns toward one task |
| Non-interactive use | The CLI expects a terminal. With piped stdin it runs, but has no structured output, exit-code contract, or approval channel | No headless mode |
| Daemon | Detached turns survive client exit; events are journalled (8,192 events, bounded bytes); `SessionClient` is a Rust API | Scripts in other languages have no supported interface |

The daemon already does most of what headless use needs: turns run without a
client, survive disconnects, and are audited. Headless mode is therefore
mainly a new client, not new execution machinery.

## 3. Workstream A: images and multimodal models

### 3.1 Design

**Message model.** Add `images: Vec<ImageRef>` to `ChatMessage`, with
`#[serde(default)]` so existing sessions and transcripts still load. An
`ImageRef` holds a SHA-256 hash, media type, pixel dimensions, byte length,
and a short label (the source path or tool name). It does not hold the bytes.

**Image store.** The bytes live in a content-addressed store in the session
state directory (`images/<sha256>`). This keeps them out of snapshots,
protocol messages, and the event journal. The store is bounded per session
and garbage-collected with the session. Hashes keep records small and make
duplicate renders free.

**Ingestion.** Every image passes through one function that:

- reads it through the prepared-access path (`prepared.rs`), so confinement
  and the sensitive-path approval gate apply exactly as they do for text
  files;
- decodes it with the `image` crate under hard limits on bytes and pixels,
  rejecting anything else;
- downscales it to a configured maximum edge (for example 1,568 px) and
  re-encodes it as PNG or JPEG, so no original file bytes reach the provider.

Re-encoding also removes metadata and limits the decoder's exposure to a
crafted file.

**Sources:**

- **`//attach PATH`:** the user attaches an image to their next message.
- **`view_image` agent tool:** read-only, and gated like `read_file` for
  sensitive paths. The agent asks to see a file. This covers renders and plots
  that its own shell commands produced.
- **Render tool (optional, A7):** converts STL, STEP, OBJ, or GLB to PNG
  through F3D when it is installed. It reports clearly when F3D is missing,
  and produces the render manifest the original implementation plan
  describes.

**Adapters:**

- **OpenAI-compatible:** a message with images is sent as content parts:
  `{"type":"text"}` followed by `{"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}`.
  Chat-completions `tool` messages accept only text. An image from a tool
  is therefore sent as a short text tool result, followed by a `user`
  message that carries the image and names the tool call it came from.
- **Ollama:** images go in the message's `images` array as base64.
- **Capability flag:** model profiles gain `vision = true|false`. Without it,
  an attach is refused with a clear message, and the `view_image` tool is not
  offered. Behaviour of the specific servers in use (vLLM, TabbyAPI, Ollama)
  must be checked per model during A3. Content-part support differs.

**Context and compaction.** Each image is charged a token estimate
(configurable per profile) against the context budget. Compaction drops old
images before old text, and leaves a placeholder (`[image <hash>: label,
dropped from context]`) so the model knows something was there.

**Remote sessions.** An attach from a controller to a remote session uploads
the bytes through a new bounded protocol operation, and the daemon stores and
hashes them on its own side. This is the first session-protocol change in the
plan. Batch it with the headless protocol changes (§5) into a single protocol
version bump.

**Audit.** Audit records the hash, media type, dimensions, source, and
decision, never the bytes, as `//view` already does.

**Terminal display (optional, A6).** `//view` for images uses the kitty or
iTerm2 inline protocols when the terminal supports them, and otherwise opens
an external viewer or prints the path and dimensions.

### 3.2 Tasks and estimates

| ID | Task | Days |
|---|---|---|
| A1 | `ImageRef` on `ChatMessage`; serde compatibility; persistence and audit format version bumps | 3–4 |
| A2 | Content-addressed image store: bounds, GC, protocol upload for remote sessions | 3–5 |
| A3 | Adapters: OpenAI content parts, tool-image follow-up message, Ollama `images`, `vision` capability, per-server verification | 3–4 |
| A4 | Ingestion: prepared-access read, decode, bound, downscale, re-encode; `//attach`; `view_image` tool | 4–6 |
| A5 | Token estimates and image-aware compaction | 2–3 |
| A6 | *Optional:* terminal inline display and `//view` for images | 3–5 |
| A7 | *Optional:* F3D render tool with manifest, and CAD fixture tests | 5–8 |
| | **Core (A1–A5)** | **15–22** |

**Risks:**
- Provider and server variation in content-part handling: some
  OpenAI-compatible servers reject image parts, or accept them only on user
  messages.
- Decoder attack surface: mitigated by size and pixel limits and re-encoding.
- Images as a prompt-injection channel: text inside a render is as
  untrusted as tool output. The approval policy is the defence, as it is for
  text.

## 4. Workstream B: configurable and relaxed limits

### 4.1 Design

**Limit profiles in configuration:**

```toml
[agent.limits]                    # applies when no profile is named
provider_requests = 128
tool_calls = 256
tool_calls_per_response = 64
wall_time = "60m"
shell_timeout = "60s"
auto_continues = 2

[agent.limits.profiles.relaxed]
provider_requests = 2000
tool_calls = 5000
tool_calls_per_response = 128
wall_time = "8h"
shell_timeout = "30m"
auto_continues = 50
```

- **Selecting a profile:** `--limits relaxed` on the command line, `//limits
  relaxed` during a session, or `limits = "relaxed"` on a model profile.
  `//limits` with no argument shows the active limits and how much of each
  the current turn has used.
- **Relaxed mode:** wall-clock time becomes the governing limit, and the count
  limits are high but finite. Each count still stops a runaway loop
  deterministically (finding F10's concern) rather than letting it run
  until the clock expires.
- **Daemon ceiling:** a `max_limits` setting on `xshelld`, like the existing
  `max_approval`. A client, including a remote one, cannot request more than
  the daemon's operator allows. Requests above the ceiling are clamped, and
  the clamp is reported.
- **Per-tool time limits:** `shell_timeout` replaces the fixed 60 s, with
  a hard upper bound. A tool still runs in its own process group and is
  killed when cancelled or when it overruns.
- **Long-run hygiene:**
  - Compaction is on by default under relaxed limits, because an 8-hour run
    otherwise overflows the context.
  - A periodic progress event (turn number, budget used, elapsed time) lets
    headless clients and attached users see that the run is alive.
  - The event journal keeps its bounds; clients that fall behind read the
    final state from the snapshot rather than the journal (§5).

**Task runs (optional, B4).** Many engineering tasks need many turns, not
one huge turn. The persistent-goals proposal (`xshell-md-continuity.md`)
defines continuation turns toward a goal, with a turn TTL. B4 implements its
minimum, a task that continues until the model declares it complete or a
task-level budget runs out. The budget spans turns: total wall time and
total tool calls for the task, not only per turn.

**Interaction with F10.** Relaxed limits make the remaining F10 work, the
per-session and global budgets, more important: one relaxed headless job
should not starve every other session. Land at least the global concurrent
turn limit before enabling relaxed limits by default anywhere.

### 4.2 Tasks and estimates

| ID | Task | Days |
|---|---|---|
| B1 | Config schema, named profiles, `--limits`, `//limits`, daemon `max_limits` ceiling with clamping | 3–4 |
| B2 | Relaxed semantics, configurable shell timeout, configurable auto-continues, compaction default under relaxed | 2–3 |
| B3 | Progress events, docs, tests | 2–3 |
| B4 | *Optional:* task runs with cross-turn budgets (minimal persistent goals) | 6–9 |
| | **Core (B1–B3)** | **7–10** |

**Risk:** an 8-hour unattended run with auto-approval is the most dangerous
configuration xshell would support. The daemon ceilings, required audit, and
the headless approval rules (§5.1) carry that risk; they are not optional.

## 5. Workstream C: headless mode

### 5.1 Design

**A `run` subcommand** on the existing binary:

```sh
xshell run --session cad-job --limits relaxed --approval-policy policy.toml \
           --output jsonl "Export the bracket to STEP and render it" > events.jsonl
echo $?
```

- **Input:** the prompt comes from the argument, `--prompt-file`, or stdin.
  `--attach image.png` adds images (workstream A). `--session NAME` reuses
  a named session (created if missing), and `--ephemeral` uses a throwaway
  one.
- **Execution:** runs go through `xshelld` by default, so they get durability,
  audit, the daemon's ceilings, and survival of the client. `--standalone`
  runs in-process for environments without a daemon.
- **Output:** `--output text` prints the answer only. `--output jsonl` streams
  one JSON object per event:
  - **Schema:** versioned and documented: `turn_started`, `assistant_text`,
    `tool_requested`, `approval_needed`, `tool_result`, `progress`,
    `turn_completed`, `turn_failed`.
  - **Final record:** a `result` object with the answer, a summary of tool
    calls, artifacts, budget used, and the outcome.
  - **Stability:** the schema is a public contract, independent of the
    internal `ExecutionEvent` type, so internal changes don't break scripts.
- **Exit codes:**

  | Code | Meaning |
  |---|---|
  | 0 | Completed |
  | 1 | Failed |
  | 2 | Usage or configuration error |
  | 3 | A budget limit was exhausted |
  | 4 | Cancelled |
  | 5 | An approval was denied and the task could not continue |
  | 6 | The daemon or audit service is unavailable |

- **Approvals without a terminal:** a headless run must decide every approval
  without a person. There is no silent default to auto. The options are:
  - `--approval deny`: read-only and gated tools are refused.
  - An **approval policy file**: rules that allow or deny by tool name,
    command pattern, and working directory. For example, allow `yapcad
    export *` and `f3d *` under the project, and deny everything else. Rules
    are checked before the daemon's `max_approval` ceiling, which still wins.
  - `--approval-channel FD`: each `approval_needed` event waits for a JSON
    decision written to that descriptor, so a supervising script or a person
    on another channel can decide.
  - `--approval auto`: allowed only when the daemon's ceiling permits it, and
    recorded in audit.
- **Lifecycle:**
  - SIGINT and SIGTERM cancel the turn (or detach, with `--detach-on-signal`).
  - `--detach` prints the turn ID and exits at once.
  - `xshell wait`, `status`, `result`, and `cancel` act on a turn ID, so a
    long job survives the script that started it, and a restarted script can
    pick it up.
  - `wait` reads the final state from the session snapshot, so a client that
    missed events still gets a correct result.
- **Other languages:** the JSONL output and the subcommands are the supported
  interface. Scripts do not need to speak the daemon's protocol.

### 5.2 Tasks and estimates

| ID | Task | Days |
|---|---|---|
| C1 | `xshell run`: prompt sources, sessions, daemon and standalone execution, exit codes | 3–4 |
| C2 | Versioned JSONL event schema, final `result` object, text mode | 3–4 |
| C3 | Headless approvals: deny, policy-file rules, approval channel, ceiling interaction | 4–6 |
| C4 | `--detach`, `wait`, `status`, `result`, `cancel`; signal handling; recovery from the snapshot | 3–5 |
| C5 | Tests with piped stdio (no terminal needed) and schema documentation | 2–3 |
| C6 | *Optional:* attachments (`--attach`) and an artifact manifest in the result (needs A) | 2–3 |
| C7 | *Optional:* a small Python client wrapper with examples | 1–2 |
| | **Core (C1–C5)** | **15–22** |

**Risk:** the approval policy matcher is security-relevant: command
patterns are easy to get wrong. Rules should match parsed argument vectors,
not raw strings, and anything unmatched is denied.

## 6. Cross-cutting work

- **One protocol and format bump.** Image upload, limit selection, and the
  headless lifecycle operations all touch the session protocol. Images change
  the persisted session and audit formats. Design these together and ship one
  session-protocol version bump and one audit-format bump, with migration
  tests for old sessions.
- **Security review.** Before the headless approval policy and relaxed limits
  ship, review them against the threat model. Add their failure modes to the
  findings ledger if the review finds any.
- **Integration tests.** The CLI integration harness proposed in
  `cli-integration-tests-design.md` covers attach, `//limits`, and display.
  Headless tests need no terminal and can start sooner.
- **Documentation.** A user guide for headless runs and its JSONL schema, a
  limits reference, and a vision-model setup guide (which servers and models
  are known to work).

These are included in the estimates above, except the security review: allow
2–3 days for it.

## 7. Sequencing

| Phase | Work | Duration | Delivers |
|---|---|---|---|
| 1 | B1–B3 | 1.5–2 weeks | Configurable limits and the relaxed profile in interactive use |
| 2 | C1–C5 | 3–4.5 weeks | Scripted engineering runs with structured output and safe approvals |
| 3 | A1–A5 (+ C6) | 3–4.5 weeks | Agents see attached images and their own renders; headless runs accept images |
| 4 | Optional: A6, A7, B4, C7 | 3–5 weeks | Inline display, F3D renders, multi-turn task runs |

- **Phase 1 before C:** limits are the smallest workstream and the most
  immediately useful. Headless runs also need them: a script must be able to
  choose its limits.
- **C before A:** C fixes the public event schema and exit codes before A adds
  image events to it.
- **Parallel work:** A and C share little code beyond the protocol bump. With
  two engineers they can run side by side after Phase 1, bringing the core to
  about 6–8 weeks of calendar time.
- **F10:** the global and per-session budgets should land during Phase 2, so
  relaxed headless runs are bounded across sessions before they are common.

## 8. Open questions

1. **Vision models:** which models and servers matter first, for example the
   OpenAI-compatible endpoint in daily use or a local Ollama vision model?
   This decides which content-part dialects A3 must support at launch.
2. **Image sources:** is attaching images and letting the agent view its own
   renders enough to start, or is the F3D render tool (A7) needed in the first
   release?
3. **Headless approvals:** policy file, approval channel, or both at first?
   And should a policy file be allowed to permit arbitrary shell commands at
   all, or only named tools and command prefixes?
4. **Relaxed defaults:** what relaxed limits suit real CAD tasks? Two or three
   representative yapCAD jobs, run with generous limits and their usage
   recorded, would set them better than guesses.
5. **Task runs (B4):** are multi-turn task runs needed in the first release,
   or is one long relaxed turn enough at first?
6. **Standalone headless:** is running without a daemon worth supporting, or
   should headless always require `xshelld`?
