# Anthropic provider: design

**Status:** Proposed. Nothing in this document is implemented.
**Updated:** 22 November 2026, against `e07d8c5`.

## 1. Summary

xshell reaches models through two adapters: Ollama's native API and OpenAI-compatible Chat
Completions (`crates/xshell-adapters/src/ollama.rs`, `openai.rs`). Anthropic's models are
reachable today only through a translating service such as OpenRouter. That path loses prompt
caching control, thinking replay and accurate usage. It also adds a routing hop between the
harness and the model.

This document adds a third adapter, `provider = "anthropic"`, that speaks the Anthropic Messages
API (`POST /v1/messages`, streamed). It covers:

- the request and stream mapping (§4);
- three changes the engine needs (§5):
  - opaque, signed reasoning blocks in history;
  - append-only history for models that check it;
  - token usage reporting;
- credentials that refresh during a long run, owned by the session host (§6);
- prompt caching (§7);
- errors and retries (§8);
- a staged plan (§11).

The wire behaviour follows OpenClaw's Anthropic transport, which is known to work in production.
It was read at `openclaw/openclaw@d2915cd` (2026-11-23), in
`packages/ai/src/transports/anthropic-*.ts` and `packages/ai/src/providers/anthropic-*.ts`.
Where the current API differs from what OpenClaw sends, this document follows the API and says
so.

## 2. Why

- **A clean model comparison.** CADbakeoff compares harnesses with the model held fixed, for
  example Claude Code and xshell on Claude Opus 5.5. Through OpenRouter, the xshell side differs
  in four ways:
  - the API route;
  - possibly the host;
  - caching behaviour;
  - whether thinking survives a tool round trip.

  A native adapter removes those confounds.
- **Long agent turns need the provider's features.** A CAD build turn can run for hours inside
  one `xshell run`, with hundreds of model rounds. Three things make that affordable and correct:
  - prompt caching (reads are 0.05× input on Opus 5.5);
  - thinking blocks replayed with their signatures;
  - server-side context management.

  None of these survive translation to Chat Completions.
- **Gateways speak Anthropic's API.** Organisations commonly front Anthropic with a gateway that
  exposes the Messages API under a custom base URL and accepts short-lived tokens. xshell cannot
  use such a gateway today. It has neither the protocol nor a credential that refreshes.

## 3. Goals and non-goals

**Goals**

- Stream Messages API responses, with tools, images, thinking and caching, for the Claude 4.6+
  and 5.x families.
- Work against `api.anthropic.com` and against any gateway that serves the same API under another
  base URL.
- Keep every existing guarantee:
  - credentials never printed, never sent over the session protocol, and never in a shell's
    environment (F17, F18, F23);
  - bounded streams and error bodies;
  - truthful failure records;
  - provider text kept out of system records (F16).
- Report token usage, including cache reads and writes, in the `result` line's `usage`.

**Non-goals**

- Amazon Bedrock (SigV4) and Google Vertex AI (ADC) authentication. The request mapping is shared,
  so a later variant needs only an auth layer (§12).
- Anthropic server tools (web search, code execution), Managed Agents, the Files API and batches.
- OAuth "Claude subscription" tokens. OpenClaw supports them by presenting as Claude Code (fixed
  system blocks, Claude Code tool names, a `claude-cli` user agent). xshell will not impersonate
  another client.

## 4. Wire protocol

### 4.1 Endpoint and headers

- **URL.** `POST {base_url}/v1/messages`. If `base_url` already ends in `/v1`, only `/messages` is
  appended, as in OpenClaw (`anthropic-transport-stream.ts:62-65`). The default base is
  `https://api.anthropic.com`.
- **Fixed headers.** `content-type: application/json`, `accept: text/event-stream`, and
  `anthropic-version`. The version comes from the profile's `api_version`, default `2023-06-01`.
  The request mapping below never sends `temperature`, `top_p` or `top_k`, so it is also valid
  under `2026-10-21`, which rejects them.
- **Credential header.** `x-api-key` by default. `auth = "bearer"` sends `Authorization: Bearer`
  instead, which is what most gateways issuing short-lived tokens expect.
- **Beta header.** `anthropic-beta` carries only what the request uses (§5.2, §7), plus any betas
  the profile lists. A profile can set `send_betas = false` for a gateway that rejects the header.
  OpenClaw sends no betas to proxies (`:219-220`). xshell makes this explicit instead of guessing
  from the host name.
- **Endpoint security.** The credential checks stay exactly as they are:
  - `check_credential_endpoint` (https, or http only to loopback);
  - no redirects followed (`xshell-adapters/src/lib.rs:46-90`).

### 4.2 Request body

| Field | Source | Notes |
|---|---|---|
| `model` | profile `model` | Sent as configured |
| `max_tokens` | profile `max_output_tokens`, default 32000 | Streaming makes large values safe. The 5.x models allow 128K |
| `system` | the leading `system` message | An array of text blocks. Never a bare string, so a cache breakpoint can be placed on it (§7) |
| `messages` | history, mapped as below | |
| `tools` | `ChatRequest.tools` | `{name, description, input_schema, eager_input_streaming: true}`. Sorted by name, so the set is stable for caching |
| `tool_choice` | omitted (`auto`) | Forced choice (`any`, `tool`) returns 400 on Opus 5.5, Sonnet 5.5 and Fable 5.1. xshell never forces a tool |
| `thinking` | profile `thinking` | `{type: "adaptive", display}`, or omitted. Opus 5.5 rejects `disabled` and `budget_tokens` at every effort level |
| `output_config.effort` | profile `effort` | `low` … `max`. Opus 5.5 defaults to `medium` and the other current models to `high`, so a profile should always set it. Comparisons depend on it |
| `stream` | `true` | Always |
| `metadata.user_id` | optional profile field | An opaque id. Never the session name |

**Messages** (from `ChatMessage`, `xshell-core/src/lib.rs:128`):

- **`user`** becomes a user message. Its content is a text block followed by image blocks
  (§4.4).
- **`assistant`** becomes an assistant message whose blocks are:
  - the stored provider blocks (§5.1), unchanged and in order;
  - or, when there are none, a `text` block followed by one `tool_use {id, name, input}` per
    `ToolCall`.

  Empty text blocks are not sent.
- **`tool`** results become `tool_result {tool_use_id, content, is_error}` blocks.
  - Every result that answers one assistant message goes into a single user message, in call
    order. Splitting parallel results across messages teaches the model to stop calling tools in
    parallel.
  - A result that carries images uses an array of text and image blocks.
  - Empty output becomes `"(no output)"`, as in OpenClaw (`anthropic-messages.ts:125`).
- **Text that follows tool results** in the same user turn goes into that same user message,
  after the `tool_result` blocks. The API needs every `tool_use` answered in the very next user
  message, and nothing may come between.
- **`is_error`.** The engine marks a result as an error when a call is denied, fails to run or
  times out. A non-zero shell exit is not an error: the model reads the exit status in the output.
  `ToolResult` needs a flag for this; today it is text only.

**History repair.** The engine already keeps tool calls and results paired: compaction never
splits them (`xshell-execution/src/compaction.rs:151`), and an aborted turn records its skipped
calls. The adapter checks the pairing anyway before sending, as OpenClaw does
(`src/agents/transport-message-transform.ts:150-180`). A `tool_use` with no result gets a
synthetic `is_error` result saying the call was interrupted. The repair is logged as a `warning`
event.

**Tool ids.** Ids from other providers (a session that switched model) are rewritten to
`[A-Za-z0-9_-]{1,64}`, and their results are remapped to match (`anthropic-tool-projection.ts:82-84`).

### 4.3 Streaming

The adapter reads `data:` lines with the existing bounded reader (`stream_lines`,
`xshell-adapters/src/lib.rs:187`). The pending-line, total-response, idle and duration limits all
apply. Events are handled as follows:

| Event | Handling |
|---|---|
| `message_start` | Record the message id and the initial `usage` |
| `content_block_start` | Open a block by index: `text`, `thinking`, `redacted_thinking`, `tool_use` |
| `content_block_delta` | Handled by delta type, one row each below |
| … `text_delta` | Append to the block, and emit `AgentEvent::TextDelta` |
| … `thinking_delta` | Append to the block |
| … `signature_delta` | Buffer. Attached only at `content_block_stop`, so an interrupted stream never stores a partial signature (OpenClaw lifecycle `:208-210`) |
| … `input_json_delta` | Append to the tool call's partial JSON, within `MAX_TOOL_ARGUMENT_BYTES` |
| `content_block_stop` | Close the block. A tool call's JSON is parsed here and must be an object. Otherwise the result is `InvalidToolArguments`, which the engine already handles |
| `message_delta` | `stop_reason`, `stop_details`, final `usage` |
| `message_stop` | End. A stream that ends without it is `InvalidResponse("stream ended before message_stop")` |
| `ping` | Ignored |
| `error` | `StreamError` with the provider's text, capped (F16 rules unchanged) |
| anything else | Ignored, for forward compatibility |

**Eager tool input.** With `eager_input_streaming`, the server no longer validates tool input,
so a truncated stream can leave invalid JSON. OpenClaw repairs string literals (`:640-650`).
xshell does not repair. It refuses the call as `InvalidToolArguments`, and the model sees the
refusal and retries. A stop reason of `max_tokens` with an open tool call is reported as such,
telling the operator to raise `max_output_tokens`.

**`stop_reason` mapping** to `AssistantResponse.finish_reason`, which the engine's auto-continue
logic reads (`engine.rs:582`):

| `stop_reason` | `finish_reason` | Engine behaviour |
|---|---|---|
| `end_turn`, `stop_sequence` | `stop` | Turn may end |
| `tool_use` | `tool_calls` | Run tools |
| `max_tokens`, `model_context_window_exceeded` | `length` | Not retried, as today |
| `pause_turn` | `pause` | Resend unchanged, counted against `auto_continues` (server tools only; not expected without them) |
| `refusal` | `refusal` | Turn fails with a fixed reason. The category from `stop_details` goes in the record; the explanation text is the provider's, so it is capped and quoted |
| `compaction` | `stop` | See §5.2 |
| unknown | `InvalidResponse` | |

**Refusals and fallbacks.** Opus 5.5 can decline a request (HTTP 200, `stop_reason: "refusal"`).
The API offers server-side fallback to another model (`fallbacks: "default"`, beta
`server-side-fallback-2026-07-01`). xshell does not enable it by default. A run that silently
switches model would corrupt a model comparison, and the fallback model cannot read the thinking
blocks. A profile can opt in with `fallbacks = "default"`. A response from a fallback model is
recorded with the model that answered: the `model` in `message_start`, kept in the turn record.

### 4.4 Images

- Images are sent as `{type: "image", source: {type: "base64", media_type, data}}`.
- Ingestion already re-encodes every image to PNG or JPEG, at most 1568 px on the long edge
  (`docs/headless.md`). Both types are accepted, and 1568 px is the size the API itself scales to.
- `vision = false` keeps the existing text placeholder.
- The adapter enforces a per-request total of decoded image bytes. The default is 32 MiB, matching
  the API's request limit. It fails the request with `Unsupported` rather than dropping images.

## 5. Engine and core changes

### 5.1 Reasoning blocks are opaque and signed

`ChatMessage.reasoning` is an `Option<String>` (`xshell-core/src/lib.rs:136`). That is enough for
vLLM's `reasoning_content`, but not for Anthropic:

- **Thinking must be replayed exactly.** A `thinking` block's `signature`, and a
  `redacted_thinking` block's `data`, must come back unchanged on later requests. That includes
  the assistant message that called a tool, whose thinking the API needs when the result returns.
- **The thinking text may be empty.** On Opus 5.5 the default `display` is `omitted`, so `thinking`
  is an empty string. Its signature still carries the reasoning.

Add an optional, provider-tagged field to `ChatMessage`:

```rust
/// Content blocks exactly as the provider returned them, for replay to the same provider.
/// Opaque to everything but the adapter that wrote it.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub provider_blocks: Option<ProviderBlocks>,

pub struct ProviderBlocks {
    pub provider: String,          // "anthropic"
    pub model: String,             // the model that produced them
    pub blocks: serde_json::Value, // the assistant message's content array
}
```

- **The adapter replays them** when `provider` matches and `model` matches. Fable 5.1 and Opus 5.5
  blocks are also readable by certain other models, but the API drops unreadable blocks itself,
  unbilled. So the adapter may always send them to the same provider, and it drops them only for
  another provider.
- **`reasoning` keeps the readable summary** (with `display = "summarized"`) for transcripts and
  `//history`. It is never sent back to an Anthropic model.
- **Other providers ignore `provider_blocks`** and use `content`, `tool_calls` and `reasoning` as
  today.
- **Size.** Signatures are large. The bytes count toward `max_history_bytes` like any other
  content, so compaction sees them.
- **Sessions.** The field is absent in older session files and travels in the session protocol.
  Like `images`, it needs a protocol version bump.
- **Audit.** The audit log records the response's text and tool calls, not the blocks. Signatures
  are not evidence of anything a reader can check.

### 5.2 Append-only history for models that check it

**What the API now checks.** Claude Opus 5.5, Sonnet 5.5, Haiku 5.5 and Fable 5.1 bind each
thinking block to the conversation prefix that produced it: the top-level `system`, the tool set,
and every earlier message. This is called *preserved thinking*. If any of that prefix is edited
afterwards, every later thinking block becomes invalid:

- an earlier message removed or changed;
- an old tool result elided;
- the system prompt rebuilt between requests.

The consequence depends on the account. Accounts created on or after 2026-08-31 get a 400.
Older accounts have the blocks silently dropped, and the model loses its reasoning. Either way
the prompt cache misses from the edit onward.

**Where xshell edits its history today:**

1. **History compaction.**
   - `compact_to` drops whole earlier turns, then replaces oversized tool results with
     `[xshell: tool result elided by history compaction]` (`compaction.rs:62`, `:153`, `:252`).
   - In a headless run the whole job is one turn, so only the elision step can act. Every
     elision is a mid-history edit.
2. **Per-turn context (S6, #166).** It is appended to the leading system message of every request
   in a turn (`engine.rs:1528`). It is stable within a turn but rebuilt for the next one, which
   edits the system prompt between turns.
3. **Dropping reasoning.** Compaction also removes a turn's reasoning, last of all
   (`compaction.rs:596`). Removing reasoning from the middle of the history is an edit.

**The change.** A model profile gets `history = "append_only"`. The Anthropic adapter's
descriptor reports it by default for the models above. Under it:

- **Client-side compaction never changes a message once it has been sent.**
  - Turns are not dropped, even from the front. Each thinking block is bound to every message
    before it, so dropping the oldest turn invalidates every block after it. Only a leading run
    of thinking blocks may be removed.
  - Tool results are not elided.
  - When the history reaches its budget, the engine asks the provider to compact instead.
- **The Anthropic adapter compacts on the server.** It sends the `compact_20260112` context edit
  (beta `compact-2026-01-12`) when `server_compaction` is on. The trigger is the profile's
  `compact_at_tokens`, default 150K.
  - A `compaction` block comes back in the response. It is kept in `provider_blocks` and
    replayed. The API then treats everything before it as replaced.
  - The engine reports a `history_compacted` event with `kind = "provider"` and the token counts
    from `usage`.
  - xshell's own copy of the history is not shortened, so `//history` and the transcript still
    have all of it. What is sent is the latest compaction block and the messages after it.
- **Tool-result clearing as an alternative.** The `clear_tool_uses_20250919` context edit (beta
  `context-management-2025-06-27`) clears old tool results on the server. Unlike client-side
  elision, it is append-safe. A profile chooses `context_management = "compact" | "clear_tool_uses"
  | "none"`.
- **Per-turn context moves out of the system prompt.**
  - It is sent as a mid-conversation system message: `{role: "system", content, clear_at:
    "next_user_message"}`, appended after the turn's user message (beta
    `mid-conversation-system-clear-at-2026-08-21`).
  - Turn-scoped messages are rendered for one turn and then stay in the transcript, cleared.
    Earlier copies are never deleted.
  - Without the beta, the context is a text block at the end of the turn's user message, again
    never removed.
  - The top-level `system` stays exactly the session's system prompt for the session's life.
- **Overflow.** If the provider still reports a context overflow, the turn fails with
  `ContextOverflow`, as today. The engine does not retry it by editing history under
  `append_only`.

The 1M context window of the current models leaves a large margin. Most turns never compact.

**As built (server compaction, the adapter part of P6).**
- A profile sets `context_management = "compact"` and, optionally, `compact_at_tokens` (default
  150000, at least 50000) and `compact_instructions`. The default is `"none"`.
  `clear_tool_uses` is not built.
- Each request then carries a `compact_20260112` edit with an `input_tokens` trigger. The
  adapter adds `compact-2026-01-12` to `anthropic-beta`, so compaction cannot be combined with
  `send_betas = false`.
- The `compaction` block, which streams as one `compaction_delta`, is kept in `provider_blocks`
  with the rest of the response and replayed as received. The API ignores everything before it.
  The history is sent in full: xshell's copy is never shortened, so the request stays
  append-only and signed thinking keeps passing the prefix check.
- Usage is the sum over `usage.iterations`, because the top-level counts leave out the
  summarization step. `TokenUsage.compactions` counts the compactions a response carried, and
  appears in the per-response and per-turn usage. The audit record keeps the token counts but
  not this count.
- A block with a null `content` means the API wrote no summary and compacted nothing. The
  documented cause is that the model called a tool instead of summarizing. After one such block,
  the adapter sends a summarization prompt that rules out tools on that adapter's later
  requests, unless the profile set its own prompt. On Opus 5.5 a custom prompt summarizes
  without the earlier thinking, so the API's default prompt is tried first.
- Not built: the engine's `history = "append_only"` mode and the per-turn context as a
  turn-scoped message. A one-turn headless run needs neither: client compaction is off when
  `max_history_bytes = 0`, and the turn's context does not change within the turn.

### 5.3 Token usage

`AssistantResponse` gains an optional `usage`:

```rust
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,     // cache_creation_input_tokens
    pub cache_write_1h: u64,  // cache_creation.ephemeral_1h_input_tokens, when reported
}
```

- **Collection.** The Anthropic adapter fills it from `message_start` and then `message_delta`.
  The `message_delta` value is cumulative and replaces the earlier one. The OpenAI adapter can
  fill input and output from `usage` when the server sends it.
- **Reporting.** The engine sums it across the turn and its subagents. The headless `result`
  line's `usage.tokens` then becomes `{"input", "output", "cache_read", "cache_write", "assurance":
  "measured"}`, which today is always `unavailable` (`budget.rs:391`). The assurance is
  `measured`, not `enforced`: the provider counted the tokens and xshell does not limit them.
- **No dollar cost.** xshell does not compute one. Prices change and gateways bill differently. A
  runner computes cost from tokens and its own price table.
- **No token limit in this change.** A `max_tokens` turn limit can come later, on top of these
  counts.

## 6. Credentials

### 6.1 Sources

A profile names one credential source:

| Key | Meaning |
|---|---|
| `api_key_env = "NAME"` | As today: read once from the session host's environment |
| `api_key_command = ["argv", ...]` | Run on the session host. Its standard output, trimmed, is the credential. Run again on expiry or when the endpoint rejects the credential |

`api_key_command` exists for gateways that issue short-lived tokens, such as the one CADbakeoff's
cells use. Claude Code's `apiKeyHelper` does the same job.

### 6.2 Who may run the command

A client never supplies a command. If it could, it would be asking the daemon to run a program
outside every approval and sandbox.

- `api_key_command` is read only from the session host's own `[models.NAME]` profiles.
- A client's `ModelBinding` refers to the profile by name. `CredentialRoutes`
  (`xshell-session/src/credentials.rs`) gains a fourth route kind,
  `(provider, base_url, profile)`. It accepts a binding only if the daemon has that profile with
  the same provider and endpoint.
- The binding never carries the argv. The error message names neither the argv nor the profile's
  credential, as today.

### 6.3 Running it

- **Process.** The argv runs directly, never through a shell, with a minimal environment: the F18
  allowlist plus any `api_key_command_env` names the profile lists. stdin is null, and stdout is
  capped at 16 KiB.
- **Time limit.** 30 s. A failure fails the model request with a fixed message that includes the
  exit status, never the output.
- **Outside the agent's containment.** It is the session host's own credential process, so
  contained shell commands ([shell sandbox](shell-sandbox.md), proposed in #167) never apply to
  it. It is never visible to agent tools. It is also separate from that design's named secrets,
  which put a credential in a contained command's environment. A command source like this one
  could later supply those secrets too.
- **Cache.** The credential is cached in memory. It is refreshed after `api_key_ttl` (default 5
  min, matching Claude Code's helper default), and once on a 401 before the request fails.
- **Protection.** The value is registered with `protect_credential_variable`'s mechanism. That
  keeps it out of every shell environment and out of every error message and record.
- **Audit.** A refresh is an audit event, `credential_refreshed`: the profile, never the value.

### 6.4 Headers

`auth = "x-api-key" | "bearer"` (§4.1). Whatever is chosen, the credential is attached in exactly
one place, after `check_credential_endpoint`.

## 7. Prompt caching

Caching is a prefix match over `tools`, then `system`, then `messages`. A change anywhere
invalidates everything after it. The API allows at most four `cache_control` breakpoints per
request.

- **Breakpoints.** `{type: "ephemeral"}`, with `ttl: "1h"` when the profile sets
  `cache_ttl = "1h"`. They are placed:
  1. on the last tool definition;
  2. on the last system block;
  3. on the last block of the newest user message, which for a tool loop is its last
     `tool_result`;
  4. on the last block of the user message before it. The API looks back only a limited
     number of blocks for an earlier cache entry, and one message of parallel tool results can
     exceed that.

  OpenClaw places them the same way, minus its session-shared system split
  (`anthropic-payload-policy.ts:290-366`).
- **Stability.**
  - Tools are sorted by name.
  - The system prompt does not change within a session (§5.2).
  - Tool definitions serialise deterministically: the workspace builds `serde_json` without
    `preserve_order`, so object keys are always written sorted.
  - Effort is fixed by the profile. Changing it mid-session invalidates the messages cache.
- **TTL.** The default 5 min suits a tool loop: requests arrive seconds apart, and a 17-minute
  shell command is what risks an expiry. With `cache_ttl = "auto"`, the adapter uses `1h`
  whenever the previous tool call ran longer than four minutes. That costs 2× on writes, against
  re-reading the whole prefix uncached.
- **As built (P5).**
  - Three markers are used, not four. The one on the last system block also caches the tools,
    which render before it; the last tool carries the marker only when there is no system prompt.
    The other two go on the last block of the newest user message and of the user message before
    it.
  - `cache_ttl` is `"5m"` (the default), `"1h"` or `"off"`. `"auto"` needs the engine to tell the
    adapter how long the last tool ran, and is left for later.
  - Every marker has the same lifetime, which keeps the API's rule that longer-lived entries come
    first.
  - Within one turn every request is the previous one plus new messages, which a test checks. A
    new turn still changes the system prompt, because the engine appends the turn's context to it
    (`engine.rs:1528`). So a new turn re-reads the tools and system prompt uncached once; a
    one-turn headless run is not affected. Moving that context into `messages` is part of P6.
- **Verification.** `cache_read` in §5.3 shows whether caching works. As built, the headless
  client warns at the end of a run that made two or more model rounds, wrote cache entries and read
  none back: the prompt changed between requests. The warning is the client's, since the engine has
  no warning event; the per-response `usage` in the JSONL stream locates the round where reads
  stopped.

## 8. Errors and retries

| Response | Handling |
|---|---|
| Transport failure before any response | Retry once, as today (`send_with_retry`) |
| 400 `invalid_request_error` | `Http`, not retried. A body matching `prompt is too long` or `input length and max_tokens exceed context limit` becomes `ContextOverflow` (extending `context_overflow`, `lib.rs:285`) |
| 401, 403 | Refresh the credential once if it comes from a command (§6.3), then `Http` |
| 413 `request_too_large` | `ContextOverflow` with no counts |
| 429 `rate_limit_error`, 529 `overloaded_error`, 500, 502, 503, 504 | Retry with backoff: 1 s base, ×2, jitter 0.5–1.5, 30 s cap per wait (60 s for 429), at most 6 retries or 120 s of waiting. `retry-after` and `retry-after-ms` raise the wait, up to the cap |
| `error` event mid-stream | `StreamError`, not retried: output may already have been shown |

- **Retries are recorded.** Each retry is a `warning` event and an audit record, with the status
  and the wait.
- **Retries count against the turn.** They count toward `model_rounds`, and the turn's wall time
  includes the waits. A long overload therefore ends in `budget_exhausted`, not a hang.

OpenClaw's figures are similar (`failover-retry-controller.ts:34-71`: 1 s base, 30 s cap, 90 s
total, 8 retries). xshell takes a slightly longer total, because an unattended run loses far more
to a failed turn than to a wait.

## 9. Configuration

```toml
[models.opus]
provider = "anthropic"
model = "claude-opus-5-5"
base_url = "https://gateway.example.net/anthropic"   # default https://api.anthropic.com
auth = "bearer"                                      # default "x-api-key"
api_key_command = ["/usr/local/bin/gateway-token"]   # or api_key_env = "ANTHROPIC_API_KEY"
api_key_ttl = "5m"
api_version = "2023-06-01"
effort = "high"                                      # always set: the default differs by model
thinking = "adaptive"                                # or "omit"
thinking_display = "summarized"                      # "omitted" (API default), "summarized"
max_output_tokens = 64000
context_tokens = 1000000
vision = true
history = "append_only"                              # default for the models in §5.2
context_management = "compact"                       # "compact", "clear_tool_uses", "none"
compact_at_tokens = 150000
cache_ttl = "auto"                                   # "5m", "1h", "auto"
betas = []                                           # added to those the request needs
send_betas = true
fallbacks = "none"                                   # "default" opts in (§4.3)
limits = "cad"
```

- **Validation happens when the profile loads:**
  - `thinking = "disabled"` is refused for models that reject it;
  - an `effort` the model does not list is refused.

  The model table lives in the adapter and is small. Unknown models are allowed, and the API's
  400 then reports the problem.
- **Recording.** The applied effort, thinking setting and API version are recorded in
  `turn_started` and the audit log. A comparison can then show what each side actually sent.

## 10. Testing

- **Unit tests** against recorded SSE streams, served by the existing loopback test server
  (`lib.rs:474`):
  - text only;
  - parallel tool calls;
  - thinking with signature;
  - redacted thinking;
  - a stream cut before `message_stop`;
  - a signature cut mid-block;
  - invalid eager tool JSON;
  - `refusal`;
  - `max_tokens` with an open tool call;
  - each error status;
  - `retry-after`.
- **Request snapshots:**
  - the exact JSON for representative histories (tool results grouped, text after results,
    images, replayed blocks, breakpoints placed);
  - a test that no sampling parameter is ever sent;
  - a test that the request is byte-identical across two builds of the same history (cache
    stability).
- **The append-only property.** For a long synthetic turn under `history = "append_only"`, every
  request's messages must be a prefix of the next request's, except at a compaction block.
- **Live conformance**, opt-in with a key in the environment:
  - one tool round trip with thinking replayed, sent with `thinking.block_binding.
    prefix_mismatch_behavior = "error"` (beta `thinking-binding-controls-2026-08-01`), so a
    history edit fails the test even on an older account;
  - a second identical request that must report `cache_read > 0`.
- **Credentials:**
  - `api_key_command` refresh on TTL and on 401;
  - a command that hangs, fails or prints too much;
  - a client binding that names a profile the daemon does not have, or the right profile with
    another endpoint.

## 11. Staged plan

| Stage | Content | Effort |
|---|---|---|
| P1 (done) | `provider = "anthropic"`: request mapping, streaming, `stop_reason` mapping, images, `api_key_env`, both auth headers, `api_version`, effort and thinking settings, error classes and retry. Session protocol and `CredentialRoutes` accept the provider | 4–6 days |
| P2 (done) | `provider_blocks` in core, session files and protocol; thinking replay; tool-id rewrite and history repair | 2–3 days |
| P3 (done) | Usage: `TokenUsage`, sums per turn, `result.usage.tokens`, audit record | 1–2 days |
| P4 | `api_key_command` with TTL and 401 refresh; daemon-only route; audit event | 2–3 days |
| P5 (done) | Caching: breakpoints, stability tests, `cache_ttl`, zero-read warning | 1–2 days |
| P6 (adapter part done) | Append-only history: compaction mode, server compaction or tool-result clearing, per-turn context as a turn-scoped system message, live conformance test | 4–6 days |

- **What each stage enables.**
  - P1–P3 are enough for a short comparison run.
  - A multi-hour run needs P4, because gateway tokens expire, and P6, because S6 context and
    tool-result elision would otherwise edit history.
  - P5 is what makes the cost comparable.
- **Ordering.** P6 can start in parallel with P2. It changes the engine, not the adapter.

## 12. Open questions

1. **Bedrock and Vertex.** Both serve this request shape behind their own auth (SigV4, Google
   ADC). Is either worth an auth variant, or is a gateway enough?
2. **A cost ceiling.** Usage makes a token or cost turn limit possible. Claude Code has
   `--max-budget-usd`. Should xshell add `max_tokens` (or input plus output) to the turn budget,
   and leave dollars to runners?
3. **Effort parity.** For a harness comparison, what effort does Claude Code send by default for
   Opus 5.5? It should be read from the gateway's request log, not assumed, and xshell's profile
   set to match.
4. **Mid-conversation effort.** Opus 5.5 accepts a per-message effort change without a cache reset
   (beta `mid-conversation-output-config-2026-07-01`). Is `//effort` worth adding?
5. **`display` default.** Should thinking summaries be on by default, so `//history` and
   transcripts show the reasoning? They cost nothing extra in thinking, but they add output
   tokens.
