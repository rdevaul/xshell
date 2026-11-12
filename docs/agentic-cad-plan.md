# Agentic CAD readiness: plan and level of effort

**Status:** Accepted, with the decisions recorded in §8
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
| A. Images and multimodal models | 15–22 engineer-days | 24–36 |
| B. Configurable and relaxed limits | 7–10 | 7–10 |
| C. Headless mode | 16–24 | 19–29 |
| **Total** | **38–56 days (about 8–11 weeks)** | **50–75 days (about 10–15 weeks)** |

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

**Status:** A1 is implemented. `ChatMessage.images` holds validated
`ImageRef`s; `sessions.json` is format 2 (format 1 still loads); audit is
format 6 and protocol 7. Until A3, both adapters refuse a request that
carries images rather than send only its text.

A3 is implemented. Model profiles have `vision = true|false` (default
false), carried to the daemon on the session's model binding (session
protocol 22). Before each provider request the engine reads the images the
request refers to from the session's store, newest first, up to 64 MiB per
request. Both adapters share one layout: a user message carries its images
(OpenAI-compatible `image_url` data-URL parts; Ollama's `images` array),
and images on a tool result go in a `user` message after the block of tool
results, naming the call. Without vision, or when an image's bytes cannot be
read, the model gets a placeholder such as `[image: renders/bracket.png,
1568x1176, not shown: this model is not configured for images]`, so a
session switched between models never fails on its history. Behaviour on
specific servers (vLLM, TabbyAPI, Ollama) still needs checking per model;
nothing produces images until A4.

A2's store is implemented: `ImageStore` in `xshell-execution`, one
directory per session under `images/` in the daemon's state directory.
Each session has its own store, so a session can only reference images
stored for it, never another session's by its hash; the registry refuses
client history that references anything else. Reads are verified against
the reference's hash and length, and symlinks and hard links are refused.
Limits per session: 20 MiB per image, 512 MiB and 2,048 images in all.
Collection runs at daemon start (orphan stores, unreferenced images) and
when a session ends, never on a history update, where an uploaded image
not yet sent would look unreferenced. Protocol upload moved to A4: the
daemon must decode an upload to trust its dimensions, which is ingestion,
so the session protocol bump lands with A4 and `//attach`.

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

**Sources.** The first release supports attaching images and letting the
agent view image files. A configurable render toolset (A7) follows.

- **`//attach PATH`:** the user attaches an image to their next message.
- **`view_image` agent tool:** read-only, and gated like `read_file` for
  sensitive paths. The agent asks to see a file. This covers renders and plots
  that its own shell commands produced.
- **Render toolset (A7):** a `render` agent tool that turns CAD files and
  packages into images. The configuration chooses the renderer from the
  file or package type, so F3D, yapCAD, or any other command can be added
  without code changes:

  ```toml
  [[render.renderers]]
  name = "f3d"
  match = ["*.stl", "*.step", "*.stp", "*.obj", "*.glb", "*.gltf"]
  command = ["f3d", "--output={output}", "--resolution=1600,1200", "{input}"]
  timeout = "120s"

  [[render.renderers]]
  name = "yapcad-package"
  match = ["*.ycpkg"]
  command = ["yapcad", "render", "{input}", "--out", "{output_dir}"]
  ```

  - **Selection:** the first renderer whose patterns match is used, unless
    the call names one.
  - **Execution:** commands are argument vectors with placeholders, never
    shell strings. They run in a fresh output directory, under the
    renderer's time limit and in their own process group. Every image they
    produce goes through the ingestion path above, and a render manifest
    (renderer, command, input hash, outputs) is recorded.
  - **Approval:** the tool is governed by the approval policy like any other.
    A renderer listed in the policy can be always-allowed, so routine renders
    don't prompt.
  - **Missing renderers:** the tool says which program is missing.

**Adapters:**

- **OpenAI-compatible:** a message with images is sent as content parts:
  `{"type":"text"}` followed by `{"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}`.
  Chat-completions `tool` messages accept only text. An image from a tool
  is therefore sent as a short text tool result, followed by a `user`
  message that carries the image and names the tool call it came from.
- **Ollama:** images go in the message's `images` array as base64.
- **Both providers in the first release:** OpenAI-compatible endpoints and
  Ollama vision models are both required.
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
plan. Batch it with the headless protocol changes (§5.1) into a single protocol
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
| A2 | Content-addressed image store: bounds, GC (protocol upload moved to A4) | 3–5 |
| A3 | Adapters: OpenAI content parts, tool-image follow-up message, Ollama `images`, `vision` capability, per-server verification | 3–4 |
| A4 | Ingestion: prepared-access read, decode, bound, downscale, re-encode; `//attach` and its protocol upload; `view_image` tool | 4–6 |
| A5 | Token estimates and image-aware compaction | 2–3 |
| A6 | *Optional:* terminal inline display and `//view` for images | 3–5 |
| A7 | *Follow-on:* configurable render toolset, selected by file or package type; manifests; CAD fixture tests | 6–9 |
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

**Status:** B1 and B2 are implemented. Progress events and compaction by
default under relaxed limits are not; see "Deferred" below.

**Limit profiles in configuration.** `default` and `relaxed` are built in
with the values below. A table named after one overrides the fields it
sets, and any other name defines a profile that starts from `default`:

```toml
[agent]
limits = "default"                # profile used when nothing else chooses one

[agent.limit_profiles.relaxed]    # built in as: 2000 requests, 5000 tool calls,
wall_time = "12h"                 # 128 per response, 8h, 30m shell, 50 continues

[agent.limit_profiles.cad-render]
shell_timeout = "45m"
```

- **Selecting a profile:** `--limits relaxed` on the command line, `//limits
  relaxed` during a session, or `limits = "relaxed"` on a model profile.
  `//limits` with no argument shows the profile in effect and its values.
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
- **Long-run hygiene:** the event journal keeps its bounds; clients that
  fall behind read the final state from the snapshot rather than the journal
  (§5).
- **Deferred:**
  - *Progress events* (budget used, elapsed time) move to C2, where the
    headless event schema that consumes them is defined.
  - *Compaction by default under relaxed limits* needs the model's context
    size, which only a model profile knows. Until then, long runs should set
    `context_tokens` or `max_history_bytes` on their model profile.

**Multi-turn tasks are out of scope.** A task that continues across many
turns under one budget is a special case of persistent goals
(`xshell-md-continuity.md`). Goals will be implemented properly first, and
cross-turn task budgets built on them; neither is estimated here.

**Starting values.** The relaxed profile above is the starting point. It
will be adjusted once real CAD jobs run under it, which today's limits
prevent.

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
- **Execution:** runs always go through `xshelld`, so they get durability,
  audit, the daemon's ceilings, and survival of the client. A missing daemon
  is exit code 6.
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

- **Approvals:** a call's outcome is the combination of the approval
  mode (`off`, `ask`, `auto`) and an approval policy (§5.2). In a headless
  run, a call that would prompt goes to the approval channel
  (`--approval-channel FD`): each `approval_needed` event waits for a JSON
  decision written to that descriptor. With no channel the call is denied.
  `--approval auto` takes effect only when the daemon's ceiling permits it,
  and is recorded in audit.
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

### 5.2 Approval policy

An approval policy has three optional lists. An entry names a tool
(`read_file`, `render`), or `run_shell` with a command pattern matched
against the parsed argument vector (`run_shell: yapcad export *`).

```toml
always_allowed = ["read_file", "list_directory", "render", "run_shell: yapcad export *"]
allowed        = ["run_shell: yapcad *", "run_shell: f3d *", "run_shell: git status"]
denied         = ["run_shell: rm *", "run_shell: git push *"]
```

The policy applies to every turn, interactive and headless; only the
channel a prompt goes to differs. Each call is decided by the first rule
that matches:

| # | The call | `off` | `ask` | `auto` |
|---|---|---|---|---|
| 1 | matches `denied` | deny | deny | deny |
| 2 | matches neither `always_allowed` nor `allowed`, and an `allowed` list exists | deny | deny | deny |
| 3 | matches `always_allowed` | deny | run | run |
| 4 | matches `allowed` | deny | prompt | run |
| 5 | is unlisted, and there is no `allowed` list | today's rules | today's rules | today's rules |

- **"Today's rules":** read-only tools on non-sensitive paths run in every
  mode. Gated calls (shell commands, sensitive reads) are denied in `off`,
  prompt in `ask`, and run in `auto`.
- **Denied wins:** a `denied` entry overrides any `always_allowed` or
  `allowed` entry it overlaps. Each override is reported when the policy is
  loaded: as a warning in the CLI and `xshelld` logs, an audit record, and a
  `warning` event in headless output.
- **Unmatched calls:** with an `allowed` list, a call matching no list is
  denied, so the list is exhaustive.
- **Shell patterns:** patterns match the parsed argument vector, not the raw
  command string, so quoting cannot smuggle a second command past a
  pattern. A command that does not parse as one simple command (pipelines,
  `;`, `&&`, substitutions) matches only a bare `run_shell` entry.
- **Subagents:** they inherit the policy, as they already inherit the
  toolset and approval mode.

**Daemon policy and client overlays.** The daemon's operator writes a policy
with the same structure in the `xshelld` configuration, as fine-grained as
needed. A client or headless run may supply its own policy as an overlay,
but it can never loosen the daemon's:

- **Mode:** the requested mode is first clamped to the daemon's `max_approval`.
- **Per-call decision:** each policy decides the call by the table above,
  using `deny`, `prompt` or `run`.
- **Combination:** the stricter of the two decisions wins (`deny` is
  stricter than `prompt`, which is stricter than `run`). With no client
  overlay, the daemon's decision stands.

So a client's `always_allowed` entry runs a call without a prompt only if
the daemon's policy would also run it. A client's `denied` entry always takes
effect. Whenever the overlay asks for more than the daemon allows, the
stricter result applies and a warning is reported.

**Sensitive and blocked paths.** The daemon's configuration describes
sensitivity in classes, each with an action:

```toml
[[session_fabric.sensitivity]]
action = "prompt"     # read-only tools need approval for these paths
patterns = [".env", ".env.*", ".git/config", ".aws/**", ".ssh/**"]

[[session_fabric.sensitivity]]
action = "block"      # agent tools can never read these paths
patterns = ["*.pem", "*.key", "*.p12", "*.pfx", "id_rsa", "id_ed25519"]
```

- **`prompt`:** the call is gated: it needs approval in `ask`, is denied in
  `off`, and runs in `auto`. A policy's `always_allowed` entry does not skip
  that prompt. Only the daemon's own policy can always-allow a sensitive
  path, and only by naming it (`read_file: config/.env`); a client overlay
  never can.
- **`block`:** every read through agent tools is denied in every mode,
  whatever either policy says. That covers `read_file`, `list_directory`,
  `view_image`, and render inputs.
- **Matching:** patterns are matched against the canonical path of the
  opened object, as for sensitive paths today (F4). The existing
  `sensitive_paths` list becomes the `prompt` class, so current
  configurations keep working.
- **Shell commands:** sensitivity classes govern the agent's read tools. A
  shell command such as `cat id_rsa` is governed by the policy's `run_shell`
  rules and the approval mode, not by the classes. Truly blocking such
  access needs operating-system permissions or the sandboxing follow-up
  below.

**Compound commands and scripts.** Pipelines, `;`, `&&`, command
substitution, and scripts run through an interpreter must not become a way
around the policy. The first step treats every `run_shell` call as subject
to the policy:

- A compound command matches only a bare `run_shell` entry.
- With `run_shell` deny patterns, a command that cannot be split into parts
  read as written (substitutions, separators inside quotes, expanding
  command words) is prompted in `ask` mode and denied otherwise, so it can
  never slip past a denial unattended.
- A pattern that allows an interpreter or a script (`python *`,
  `bash -c *`, `./build.sh`) is documented as allowing whatever that
  interpreter or script does.

Making shell access safe beyond that is a separate design topic (§8).

### 5.3 Tasks and estimates

**Status:** C1–C5 are implemented: `xshell run`, `--detach`, `wait`,
`status`, `result`, `cancel`, the approval policy with client overlays and
sensitivity classes, and the approval channel (see [headless runs](headless.md)
and [approval policy](session-fabric.md#approval-policy)). Progress events,
deferred from B, remain for the schema.

| ID | Task | Days |
|---|---|---|
| C1 | `xshell run`: prompt sources, sessions, daemon execution, exit codes | 2–3 |
| C2 | Versioned JSONL event schema, final `result` object, text mode | 3–4 |
| C3 | Approval policy (§5.2) in the daemon for all turns: daemon policy with client overlays, sensitivity classes with `prompt` and `block`, shell-pattern matching on parsed arguments, override warnings, approval channel | 6–9 |
| C4 | `--detach`, `wait`, `status`, `result`, `cancel`; signal handling; recovery from the snapshot | 3–5 |
| C5 | Tests with piped stdio (no terminal needed) and schema documentation | 2–3 |
| C6 | *Optional:* attachments (`--attach`) and an artifact manifest in the result (needs A) | 2–3 |
| C7 | *Optional:* a small Python client wrapper with examples | 1–2 |
| | **Core (C1–C5)** | **16–24** |

**Risk:** the approval policy matcher is security-relevant: command
patterns are easy to get wrong. Matching on parsed argument vectors,
treating compound commands as unmatched, and denying unmatched calls when
an `allowed` list exists keep its failure mode closed.

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
| 4 | A7, then optional A6 and C7 | 2.5–4 weeks | Configurable CAD render toolset; inline display; Python wrapper |

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

## 8. Decisions and open questions

### Decided (4 November 2026)

1. **Vision models:** the first release supports both OpenAI-compatible
   endpoints and Ollama vision models.
2. **Image sources:** start with attaching images and viewing image files.
   A configurable render toolset that selects renderers by CAD file or
   package type (A7) follows.
3. **Headless approvals:** both a policy file and an approval channel. A
   call's outcome combines the policy with the approval mode, as specified
   in §5.2.
4. **Relaxed defaults:** start with the values in §4.1, and adjust after
   running real CAD jobs.
5. **Multi-turn task runs:** dropped from this plan. Persistent goals come
   first, and task budgets build on them.
6. **Standalone headless:** not supported. Headless runs always require
   `xshelld`.

7. **Daemon and client policies:** the daemon's policy is authoritative and
   can be as fine-grained as needed. A client's policy is an overlay, and
   the stricter decision wins for every call (§5.2).
8. **Sensitivity:** the daemon's configuration describes sensitive paths in
   classes. `prompt` requires approval; `block` denies agent reads
   altogether, for example of private keys and certificates (§5.2).
9. **Compound commands and scripts:** must not be an escape hatch. The first
   step subjects every `run_shell` call to the policy, and compound commands
   match only a bare `run_shell` entry.

10. **Listed tools in `off` mode:** rules 3 and 4 apply literally. A listed
    tool is denied in `off` mode even if it is read-only, like `read_file`.
    The policy can then express such a restriction whenever one is needed,
    rather than the implementation ruling it out in advance.

### Still open

1. **Shell safety beyond pattern matching** (a design topic, not a blocker).
   Patterns cannot see what a script or interpreter does once it runs.
   Options to explore:
   - static analysis of compound commands, matching each part against the
     policy;
   - an operating-system sandbox for `run_shell`: Landlock or bubblewrap on
     Linux, a sandbox profile on macOS, restricting files and network to
     what the policy allows;
   - running shell tools as a separate, less-privileged user.

   This deserves its own design document before headless runs with broad
   `run_shell` allowances become common.
