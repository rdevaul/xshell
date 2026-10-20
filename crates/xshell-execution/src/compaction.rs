//! Conversation-history compaction.
//!
//! Every provider request carries the whole history, so an unbounded history
//! costs tokens on every step and eventually exceeds the model's context. A
//! [`Compactor`] decides what to drop (or, in future implementations, what to
//! summarize) before a provider request and after a turn has completed.
//!
//! Compaction never splits an existing turn. A turn's messages
//! (`user`, `assistant` with tool calls, the matching `tool` results, further
//! `assistant` steps) form a unit that OpenAI-compatible APIs require to be
//! internally consistent: a `tool` result whose calling `assistant` message
//! has been removed is a protocol error. Implementations therefore operate on
//! whole turns, and the leading `system` message is always preserved.
//!
//! When whole-turn eviction cannot reach the budget on its own — the usual
//! case mid-turn, where the single in-flight turn is what overflowed — the
//! built-in compactor falls back to eliding the *content* of the largest tool
//! results, oldest first, and then the model's reasoning replay. Eliding
//! content keeps every message in place, so no `tool_call_id` is ever
//! orphaned.

use serde::{Deserialize, Serialize};
use xshell_core::{ChatMessage, MessageRole};

/// Bytes of prompt text assumed per token when a declared context window is
/// converted into a byte budget. Deliberately pessimistic: code, JSON, and CJK
/// tokenize worse than prose, and a guard that overestimates shrinks history
/// too early rather than overflowing the provider.
pub const BYTES_PER_TOKEN: usize = 3;
/// Token allowance for everything conversation history does not cover: tool
/// definitions, chat-template markup, and role markers.
pub const REQUEST_OVERHEAD_TOKENS: usize = 1024;

/// The request carries the tool definitions on every call, so their size is
/// overhead rather than history. `REQUEST_OVERHEAD_TOKENS` alone is a guess;
/// this measures the real serialized schema once so that growing the tool
/// surface cannot silently push an "in budget" request over the window. The
/// constant stays as a floor for chat-template markup we cannot measure.
fn tool_schema_overhead_tokens() -> usize {
    static OVERHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *OVERHEAD.get_or_init(|| {
        serde_json::to_string(&crate::tools::definitions())
            .map(|schema| estimate_tokens(schema.len()))
            .unwrap_or(0)
            .max(REQUEST_OVERHEAD_TOKENS)
    })
}
/// A derived budget is never smaller than this. A model declared with a window
/// barely larger than the overhead still gets a usable conversation instead of
/// a zero budget that silently disables compaction.
pub const MINIMUM_HISTORY_TOKENS: usize = 256;
/// Tokens held back for the model's own reply when deriving a budget, unless
/// the profile says otherwise.
pub const DEFAULT_RESERVE_OUTPUT_TOKENS: usize = 1024;
/// Share of the usable window given to conversation history.
const HISTORY_NUMERATOR: usize = 9;
const HISTORY_DENOMINATOR: usize = 10;
/// Floor for the deeper shrink performed after a provider rejection.
const MINIMUM_HISTORY_BYTES: usize = 1024;
/// Replacement text for an elided tool result. Messages are never removed, so
/// `tool_call_id` pairing survives; only the payload goes.
const ELIDED_MARKER: &str = "[xshell: tool result elided by history compaction]";
const TRUNCATED_MARKER: &str = "\n[xshell: tool result truncated to fit the history budget]";
/// Replacement text for a tool result dropped because the provider rejected
/// the request carrying it and nothing else could shrink.
const OVERFLOW_DROPPED_MARKER: &str =
    "[xshell: tool result dropped because the request exceeded the model's context window]";

/// What a compaction pass did, for transcripts and audit records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionReport {
    pub compactor: String,
    pub messages_before: usize,
    pub messages_after: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub turns_removed: usize,
}

/// Strategy for keeping a conversation history within budget.
///
/// Implementations must preserve the first message when it is a `system`
/// message and must only remove whole turns (see module docs). They may also
/// insert synthetic messages, which is how a summarizing compactor would
/// replace removed turns with a précis.
pub trait Compactor: Send + Sync {
    /// Stable identifier recorded in [`CompactionReport::compactor`].
    fn name(&self) -> &'static str;

    /// Compact `history` in place. Return `None` when nothing was changed.
    fn compact(&self, history: &mut Vec<ChatMessage>) -> Option<CompactionReport>;

    /// Byte budget enforced by [`Compactor::compact`], or `None` when the
    /// strategy cannot say in advance. Callers use it to skip work for
    /// histories that are already within budget.
    fn max_bytes(&self) -> Option<usize> {
        None
    }

    /// Shrink harder than [`Compactor::compact`] after the provider still
    /// rejected the request for being too long. The default repeats the normal
    /// pass, which is a no-op for a compactor that is already at budget;
    /// strategies that can degrade further override this.
    fn compact_overflow(&self, history: &mut Vec<ChatMessage>) -> Option<CompactionReport> {
        self.compact(history)
    }
}

/// Approximate serialized size of one message: content plus tool-call
/// arguments. Close enough to wire bytes to serve as a budget; exact token
/// counts are model-specific and not worth a tokenizer dependency here.
pub fn message_bytes(message: &ChatMessage) -> usize {
    message.content.len()
        + message.reasoning.as_ref().map_or(0, String::len)
        + message
            .tool_calls
            .iter()
            .map(|call| call.name.len() + call.arguments.to_string().len())
            .sum::<usize>()
}

pub fn history_bytes(history: &[ChatMessage]) -> usize {
    history.iter().map(message_bytes).sum()
}

/// Rough token count for `bytes` of prompt text. Use [`BYTES_PER_TOKEN`].
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes.saturating_add(BYTES_PER_TOKEN - 1) / BYTES_PER_TOKEN
}

pub fn estimate_history_tokens(history: &[ChatMessage]) -> usize {
    estimate_tokens(history_bytes(history))
}

/// Split `history` into `(system_prefix_len, turn_boundaries)`. A turn starts
/// at each `user` message. Messages before the first `user` message (normally
/// exactly one `system` message) form the prefix and are never removed.
fn turn_starts(history: &[ChatMessage]) -> (usize, Vec<usize>) {
    let mut starts = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if message.role == MessageRole::User {
            starts.push(index);
        }
    }
    let prefix = starts.first().copied().unwrap_or(history.len());
    (prefix, starts)
}

/// Replace the payload of oversized tool results, oldest first, then drop
/// reasoning replay, until `history` fits in `max_bytes`. Returns the number of
/// payloads elided. Message order and identity are untouched, so every `tool`
/// result stays paired with the `assistant` message that called it.
fn elide_oversized_bodies(history: &mut [ChatMessage], max_bytes: usize) -> usize {
    let mut total = history_bytes(history);
    if total <= max_bytes {
        return 0;
    }
    let mut changed = 0;
    // The newest tool result is handled last, and truncated rather than
    // elided. Replacing the result the model just asked for with a marker
    // tells it nothing, so it re-runs the same call and can burn the whole
    // turn (MAX_AGENT_STEPS) reading the same file again.
    let newest_tool = history.iter().rposition(|m| m.role == MessageRole::Tool);
    for (index, message) in history.iter_mut().enumerate() {
        if total <= max_bytes || Some(index) == newest_tool {
            if total <= max_bytes {
                break;
            }
            continue;
        }
        if message.role != MessageRole::Tool || message.content.len() <= ELIDED_MARKER.len() {
            continue;
        }
        total -= message.content.len();
        message.content = ELIDED_MARKER.to_owned();
        total += message.content.len();
        changed += 1;
    }
    // Truncating the newest result keeps a useful prefix, so it is preferred
    // over discarding reasoning replay.
    if total > max_bytes
        && let Some(index) = newest_tool
    {
        let message = &mut history[index];
        let budget = max_bytes.saturating_sub(total - message_bytes(message));
        if message.content.len() > budget && budget > TRUNCATED_MARKER.len() {
            let mut cut = (budget - TRUNCATED_MARKER.len()).min(message.content.len());
            while cut > 0 && !message.content.is_char_boundary(cut) {
                cut -= 1;
            }
            let before = message.content.len();
            message.content.truncate(cut);
            message.content.push_str(TRUNCATED_MARKER);
            total = total - before + message.content.len();
            changed += 1;
        }
    }
    // Reasoning replay is the largest remaining consumer for thinking models
    // and is not needed to keep the conversation coherent.
    if total > max_bytes {
        for message in history.iter_mut() {
            if total <= max_bytes {
                break;
            }
            if let Some(reasoning) = message.reasoning.take() {
                total -= reasoning.len();
                changed += 1;
            }
        }
    }
    changed
}

/// Replace every tool result payload in `messages` with a marker and drop
/// their reasoning replay, regardless of any budget.
///
/// This is the last resort for a turn that already ran tools when the
/// provider rejects it and the compactor cannot shrink further (always the
/// case under [`NoCompaction`]). The turn cannot be rolled back, because its
/// tool effects happened, but keeping the payloads would send the same
/// oversized request on every later turn. Messages stay in place, so every
/// `tool_call_id` stays paired. Returns the number of messages changed.
pub fn drop_tool_payloads(messages: &mut [ChatMessage]) -> usize {
    let mut changed = 0;
    for message in messages.iter_mut() {
        let mut touched = message.reasoning.take().is_some();
        if message.role == MessageRole::Tool && message.content != OVERFLOW_DROPPED_MARKER {
            message.content = OVERFLOW_DROPPED_MARKER.to_owned();
            touched = true;
        }
        if touched {
            changed += 1;
        }
    }
    changed
}

/// Drop the oldest whole turns until the history fits in `max_bytes`.
///
/// The most recent turn is always kept even if it alone exceeds the budget;
/// removing it would leave the model with no idea what was just asked. When
/// that surviving turn is itself over budget, its oversized tool results are
/// elided instead (see [`elide_oversized_bodies`]).
#[derive(Debug, Clone)]
pub struct MaxBytesCompactor {
    pub max_bytes: usize,
}

impl MaxBytesCompactor {
    /// Evict whole turns, then elide oversized payloads, until `history` fits
    /// in `max_bytes`.
    fn compact_to(
        &self,
        history: &mut Vec<ChatMessage>,
        max_bytes: usize,
    ) -> Option<CompactionReport> {
        let bytes_before = history_bytes(history);
        if bytes_before <= max_bytes {
            return None;
        }
        let (prefix, starts) = turn_starts(history);
        let messages_before = history.len();
        let mut turns_removed = 0;
        if starts.len() >= 2 {
            // Find the earliest turn we can start from such that
            // prefix + that turn onward fits, keeping at least the last turn.
            let prefix_bytes = history_bytes(&history[..prefix]);
            let mut keep_from_turn = starts.len() - 1;
            for (turn_index, &start) in starts.iter().enumerate() {
                if prefix_bytes + history_bytes(&history[start..]) <= max_bytes {
                    keep_from_turn = turn_index;
                    break;
                }
            }
            if keep_from_turn > 0 {
                let cut = starts[keep_from_turn];
                history.drain(prefix..cut);
                turns_removed = keep_from_turn;
            }
        }
        let elided = elide_oversized_bodies(history, max_bytes);
        if turns_removed == 0 && elided == 0 {
            return None;
        }
        Some(CompactionReport {
            compactor: self.name().to_owned(),
            messages_before,
            messages_after: history.len(),
            bytes_before,
            bytes_after: history_bytes(history),
            turns_removed,
        })
    }
}

impl Compactor for MaxBytesCompactor {
    fn name(&self) -> &'static str {
        "max_history_bytes"
    }

    fn max_bytes(&self) -> Option<usize> {
        Some(self.max_bytes)
    }

    fn compact(&self, history: &mut Vec<ChatMessage>) -> Option<CompactionReport> {
        self.compact_to(history, self.max_bytes)
    }

    fn compact_overflow(&self, history: &mut Vec<ChatMessage>) -> Option<CompactionReport> {
        // The steady-state pass has usually already run; halving the budget
        // lets eviction take more turns and elision reach deeper results.
        let halved = (self.max_bytes / 2).max(MINIMUM_HISTORY_BYTES);
        self.compact_to(history, halved)
    }
}

/// A compactor that never compacts. The default when no budget is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCompaction;

impl Compactor for NoCompaction {
    fn name(&self) -> &'static str {
        "none"
    }

    fn compact(&self, _history: &mut Vec<ChatMessage>) -> Option<CompactionReport> {
        None
    }
}

/// Model-side budget inputs that override or bound the session default.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelBudget {
    /// Byte budget from the model profile, overriding the session default.
    pub max_history_bytes: Option<usize>,
    /// The model's context window in tokens, declared by the profile.
    pub context_tokens: Option<usize>,
    /// Tokens reserved for the model's reply, overriding
    /// [`DEFAULT_RESERVE_OUTPUT_TOKENS`].
    pub reserve_output_tokens: Option<usize>,
}

/// Configuration surface for choosing a compactor. Additional strategies
/// (progressive summarization, token-aware budgets) plug in here without
/// changing callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// Drop oldest whole turns once the history exceeds this many bytes of
    /// content and tool-call arguments. `None` defers to the model's declared
    /// context window; `Some(0)` disables compaction outright.
    pub max_history_bytes: Option<usize>,
    /// The model's context window in tokens. When set, the enforced budget is
    /// additionally capped by what that window can actually hold, so a
    /// `max_history_bytes` that is too large for the model cannot overflow it.
    pub context_tokens: Option<usize>,
    /// Tokens held back for the model's own reply when `context_tokens` is
    /// declared. Defaults to [`DEFAULT_RESERVE_OUTPUT_TOKENS`].
    pub reserve_output_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_history_bytes: None,
            context_tokens: None,
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        }
    }
}

impl CompactionConfig {
    /// The byte budget actually enforced, or `None` when nothing is enforced.
    ///
    /// A declared `context_tokens` is converted to bytes through
    /// [`BYTES_PER_TOKEN`] after subtracting the reply reserve and request
    /// overhead. Because that conversion is a conservative estimate rather than
    /// a tokenizer reading, the provider's own rejection remains the final
    /// authority — see the overflow retry in `run_agent_turn`.
    pub fn effective_max_bytes(&self) -> Option<usize> {
        let derived = self.context_tokens.map(|tokens| {
            let usable = tokens
                .saturating_sub(self.reserve_output_tokens)
                .saturating_sub(tool_schema_overhead_tokens())
                .max(MINIMUM_HISTORY_TOKENS);
            usable
                .saturating_mul(HISTORY_NUMERATOR)
                .saturating_div(HISTORY_DENOMINATOR)
                .saturating_mul(BYTES_PER_TOKEN)
        });
        match (self.max_history_bytes, derived) {
            // An explicit 0 is an opt-out and wins over any derived budget.
            (Some(0), _) => None,
            (Some(bytes), Some(derived)) => Some(bytes.min(derived)),
            (Some(bytes), None) => Some(bytes),
            (None, Some(derived)) => Some(derived),
            (None, None) => None,
        }
    }

    pub fn build(&self) -> Box<dyn Compactor> {
        match self.effective_max_bytes() {
            Some(max_bytes) if max_bytes > 0 => Box::new(MaxBytesCompactor { max_bytes }),
            _ => Box::new(NoCompaction),
        }
    }

    /// Layer a per-model budget over this (session-wide) default. The right
    /// history depth depends on the model's context window, so a model profile
    /// may set its own values; when it does, those win.
    pub fn for_model(&self, model: ModelBudget) -> Self {
        Self {
            max_history_bytes: model.max_history_bytes.or(self.max_history_bytes),
            context_tokens: model.context_tokens.or(self.context_tokens),
            reserve_output_tokens: model
                .reserve_output_tokens
                .unwrap_or(self.reserve_output_tokens),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xshell_core::ToolCall;

    fn turn(n: usize, with_tool: bool) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user(format!(
            "question {n} {}",
            "x".repeat(40)
        ))];
        if with_tool {
            let call = ToolCall {
                id: format!("call-{n}"),
                name: "read_file".into(),
                arguments: json!({"path": format!("file-{n}.txt")}),
            };
            messages.push(ChatMessage::assistant_with_tools("", vec![call.clone()]));
            messages.push(ChatMessage::tool_result(&call, "contents ".repeat(5)));
        }
        messages.push(ChatMessage::assistant_with_tools(
            format!("answer {n} {}", "y".repeat(40)),
            Vec::new(),
        ));
        messages
    }

    fn history(turns: usize) -> Vec<ChatMessage> {
        let mut h = vec![ChatMessage::system("You are helpful.")];
        for n in 0..turns {
            h.extend(turn(n, n % 2 == 0));
        }
        h
    }

    fn is_well_formed(history: &[ChatMessage]) -> bool {
        // System first (if any), and every tool result follows an assistant
        // message that made that call.
        let mut seen_calls = std::collections::HashSet::new();
        for (i, m) in history.iter().enumerate() {
            match m.role {
                MessageRole::System if i != 0 => return false,
                MessageRole::Assistant => {
                    for c in &m.tool_calls {
                        seen_calls.insert(c.id.clone());
                    }
                }
                MessageRole::Tool
                    if !m
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| seen_calls.contains(id)) =>
                {
                    return false;
                }
                _ => {}
            }
        }
        true
    }

    #[test]
    fn under_budget_is_untouched() {
        let mut h = history(3);
        let before = h.clone();
        assert!(
            MaxBytesCompactor { max_bytes: 1 << 20 }
                .compact(&mut h)
                .is_none()
        );
        assert_eq!(h, before);
    }

    #[test]
    fn evicts_oldest_whole_turns_and_keeps_system() {
        let mut h = history(6);
        let total = history_bytes(&h);
        let budget = total / 2;
        let report = MaxBytesCompactor { max_bytes: budget }
            .compact(&mut h)
            .expect("over budget must compact");
        assert_eq!(h[0].role, MessageRole::System);
        assert!(history_bytes(&h) <= budget, "still over budget");
        assert!(report.turns_removed >= 1);
        assert_eq!(report.bytes_after, history_bytes(&h));
        assert!(is_well_formed(&h), "orphaned tool result after compaction");
        // The newest turn survives intact.
        assert!(h.last().unwrap().content.starts_with("answer 5"));
        assert!(h.iter().any(|m| m.content.starts_with("question 5")));
        // The oldest turn is gone.
        assert!(!h.iter().any(|m| m.content.starts_with("question 0")));
    }

    #[test]
    fn keeps_the_last_turn_and_truncates_its_oversized_tool_result() {
        // One in-flight turn whose tool result alone blows the budget: this is
        // the mid-turn overflow case that whole-turn eviction cannot reach.
        let call = ToolCall {
            id: "call-live".into(),
            name: "read_file".into(),
            arguments: json!({"path": "big.txt"}),
        };
        let mut h = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("read the log"),
            ChatMessage::assistant_with_tools("", vec![call.clone()])
                .with_reasoning(Some("look at the file first".into())),
            ChatMessage::tool_result(&call, "s".repeat(8192)),
        ];
        let report = MaxBytesCompactor { max_bytes: 1024 }
            .compact(&mut h)
            .expect("an oversized current turn must compact");

        assert_eq!(report.turns_removed, 0);
        assert_eq!(h.len(), 4, "elision never removes messages");
        assert!(history_bytes(&h) <= 1024, "still over budget");
        assert!(is_well_formed(&h), "elision must not orphan tool results");
        // The newest result is truncated, not elided: the model still sees a
        // real prefix of the file it asked for, so it does not re-run the call.
        assert_eq!(h[3].role, MessageRole::Tool);
        assert_eq!(h[3].tool_call_id.as_deref(), Some("call-live"));
        assert!(h[3].content.contains(TRUNCATED_MARKER));
        assert!(h[3].content.starts_with('s'), "a useful prefix survives");
        assert!(h[3].content.len() < 8192);
        assert_eq!(h[2].tool_calls.len(), 1);
        assert_eq!(h[2].reasoning.as_deref(), Some("look at the file first"));
        // The user's own message is never elided.
        assert_eq!(h[1].content, "read the log");
    }

    #[test]
    fn elision_reaches_reasoning_only_after_tool_results() {
        let call = ToolCall {
            id: "call-live".into(),
            name: "read_file".into(),
            arguments: json!({"path": "big.txt"}),
        };
        let newer = ToolCall {
            id: "call-newer".into(),
            name: "read_file".into(),
            arguments: json!({"path": "newer.txt"}),
        };
        let mut h = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("read the log"),
            ChatMessage::assistant_with_tools("", vec![call.clone()])
                .with_reasoning(Some("r".repeat(4096))),
            ChatMessage::tool_result(&call, "s".repeat(4096)),
            ChatMessage::assistant_with_tools("", vec![newer.clone()]),
            ChatMessage::tool_result(&newer, "t".repeat(4096)),
        ];
        // Budget below one marker: everything shrinkable must go.
        let report = MaxBytesCompactor { max_bytes: 1 }
            .compact(&mut h)
            .expect("over budget must compact");
        assert!(report.bytes_after < report.bytes_before);
        assert_eq!(h[3].content, ELIDED_MARKER, "older results go first");
        assert!(h[2].reasoning.is_none(), "reasoning is elided last");
        assert_eq!(h[5].content.len(), 4096, "the newest result is exempt");
        assert!(is_well_formed(&h));
    }

    #[test]
    fn a_toolless_oversized_turn_is_left_alone() {
        let mut h = history(1);
        assert!(MaxBytesCompactor { max_bytes: 1 }.compact(&mut h).is_none());
        assert_eq!(h.len(), history(1).len());

        let mut h = history(4);
        let report = MaxBytesCompactor { max_bytes: 1 }.compact(&mut h).unwrap();
        assert_eq!(report.turns_removed, 3);
        assert_eq!(h[0].role, MessageRole::System);
        assert!(h[1].content.starts_with("question 3"));
        assert!(is_well_formed(&h));
    }

    #[test]
    fn overflow_pass_shrinks_deeper_than_the_steady_state_pass() {
        // The steady-state pass stops as soon as the budget is met, so it
        // leaves the newest payloads intact. After the provider rejects the
        // request anyway, the overflow pass must be able to reach deeper.
        let first = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.txt"}),
        };
        let second = ToolCall {
            id: "call-2".into(),
            name: "read_file".into(),
            arguments: json!({"path": "b.txt"}),
        };
        let mut h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("read both"),
            ChatMessage::assistant_with_tools("", vec![first.clone()]),
            ChatMessage::tool_result(&first, "a".repeat(4000)),
            ChatMessage::assistant_with_tools("", vec![second.clone()]),
            ChatMessage::tool_result(&second, "b".repeat(4000)),
        ];
        let compactor = MaxBytesCompactor { max_bytes: 5000 };
        compactor.compact(&mut h).expect("must compact");
        let after_steady = history_bytes(&h);
        assert_eq!(h[3].content, ELIDED_MARKER, "oldest payload goes first");
        assert_eq!(h[5].content.len(), 4000, "newest payload is spared");

        let report = compactor
            .compact_overflow(&mut h)
            .expect("the overflow pass must shrink further");
        assert!(history_bytes(&h) < after_steady);
        assert!(
            h[5].content.contains(TRUNCATED_MARKER),
            "the overflow pass truncates the newest result rather than eliding it"
        );
        assert!(h[5].content.starts_with('b'), "a prefix survives");
        assert!(is_well_formed(&h));
        assert_eq!(report.bytes_after, history_bytes(&h));
    }

    #[test]
    fn works_without_a_system_message() {
        let mut h: Vec<ChatMessage> = (0..3).flat_map(|n| turn(n, false)).collect();
        let report = MaxBytesCompactor { max_bytes: 1 }.compact(&mut h).unwrap();
        assert_eq!(report.turns_removed, 2);
        assert_eq!(h[0].role, MessageRole::User);
        assert!(h[0].content.starts_with("question 2"));
    }

    #[test]
    fn budget_derived_from_the_context_window() {
        // 262144-token window, default reserve and overhead:
        // (262144 - 1024 - 1024) * 0.9 * 3 bytes.
        let config = CompactionConfig {
            max_history_bytes: None,
            context_tokens: Some(262_144),
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        };
        let expected = (262_144 - 2 * 1024) * 9 / 10 * BYTES_PER_TOKEN;
        assert_eq!(config.effective_max_bytes(), Some(expected));
        assert_eq!(config.build().name(), "max_history_bytes");
    }

    #[test]
    fn a_byte_budget_too_large_for_the_model_is_clamped() {
        // The exact mistake this guardrail exists for: a byte budget that
        // reads like a token count. The declared window wins.
        let config = CompactionConfig {
            max_history_bytes: Some(262_144),
            context_tokens: Some(8_192),
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        };
        let derived = (8_192 - 2 * 1024) * 9 / 10 * BYTES_PER_TOKEN;
        assert_eq!(config.effective_max_bytes(), Some(derived));
    }

    #[test]
    fn an_explicit_zero_budget_still_disables_compaction() {
        let config = CompactionConfig {
            max_history_bytes: Some(0),
            context_tokens: Some(262_144),
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        };
        assert_eq!(config.effective_max_bytes(), None);
        assert_eq!(config.build().name(), "none");
    }

    #[test]
    fn a_tiny_context_window_still_gets_a_usable_budget() {
        let config = CompactionConfig {
            max_history_bytes: None,
            context_tokens: Some(1_500),
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        };
        assert_eq!(
            config.effective_max_bytes(),
            Some(MINIMUM_HISTORY_TOKENS * 9 / 10 * BYTES_PER_TOKEN)
        );
    }

    #[test]
    fn an_absurd_context_window_saturates_instead_of_overflowing() {
        let config = CompactionConfig {
            max_history_bytes: None,
            context_tokens: Some(usize::MAX),
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        };
        assert!(
            config.effective_max_bytes().unwrap_or(0) >= usize::MAX / 4,
            "a huge declared window must saturate, not wrap around to a tiny budget"
        );
    }

    #[test]
    fn estimates_are_conservative() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(BYTES_PER_TOKEN), 1);
        assert_eq!(estimate_tokens(BYTES_PER_TOKEN + 1), 2);
        let h = vec![ChatMessage::user("a".repeat(300))];
        assert_eq!(estimate_history_tokens(&h), 100);
    }

    #[test]
    fn model_budget_overrides_session_default() {
        let session = CompactionConfig {
            max_history_bytes: Some(1 << 20),
            context_tokens: None,
            reserve_output_tokens: DEFAULT_RESERVE_OUTPUT_TOKENS,
        };
        assert_eq!(
            session.for_model(ModelBudget::default()).max_history_bytes,
            Some(1 << 20)
        );
        assert_eq!(
            session
                .for_model(ModelBudget {
                    max_history_bytes: Some(4096),
                    ..Default::default()
                })
                .max_history_bytes,
            Some(4096)
        );
        // A model may also explicitly disable compaction with 0.
        assert_eq!(
            session
                .for_model(ModelBudget {
                    max_history_bytes: Some(0),
                    ..Default::default()
                })
                .build()
                .name(),
            "none"
        );
        // The window and reserve travel with the model too.
        let specialized = session.for_model(ModelBudget {
            max_history_bytes: None,
            context_tokens: Some(32_768),
            reserve_output_tokens: Some(2048),
        });
        assert_eq!(specialized.context_tokens, Some(32_768));
        assert_eq!(specialized.reserve_output_tokens, 2048);
        assert_eq!(
            CompactionConfig::default()
                .for_model(ModelBudget {
                    max_history_bytes: Some(4096),
                    ..Default::default()
                })
                .build()
                .name(),
            "max_history_bytes"
        );
    }

    #[test]
    fn config_selects_strategy() {
        assert_eq!(CompactionConfig::default().build().name(), "none");
        assert_eq!(
            CompactionConfig {
                max_history_bytes: Some(0),
                ..Default::default()
            }
            .build()
            .name(),
            "none"
        );
        assert_eq!(
            CompactionConfig {
                max_history_bytes: Some(4096),
                ..Default::default()
            }
            .build()
            .name(),
            "max_history_bytes"
        );
    }
}
