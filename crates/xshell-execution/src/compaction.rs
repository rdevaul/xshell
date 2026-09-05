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

use serde::{Deserialize, Serialize};
use xshell_core::{ChatMessage, MessageRole};

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
}

/// Approximate serialized size of one message: content plus tool-call
/// arguments. Close enough to wire bytes to serve as a budget; exact token
/// counts are model-specific and not worth a tokenizer dependency here.
pub fn message_bytes(message: &ChatMessage) -> usize {
    message.content.len()
        + message
            .tool_calls
            .iter()
            .map(|call| call.name.len() + call.arguments.to_string().len())
            .sum::<usize>()
}

pub fn history_bytes(history: &[ChatMessage]) -> usize {
    history.iter().map(message_bytes).sum()
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

/// Drop the oldest whole turns until the history fits in `max_bytes`.
///
/// The most recent turn is always kept even if it alone exceeds the budget;
/// removing it would leave the model with no idea what was just asked.
#[derive(Debug, Clone)]
pub struct MaxBytesCompactor {
    pub max_bytes: usize,
}

impl Compactor for MaxBytesCompactor {
    fn name(&self) -> &'static str {
        "max_history_bytes"
    }

    fn compact(&self, history: &mut Vec<ChatMessage>) -> Option<CompactionReport> {
        let bytes_before = history_bytes(history);
        if bytes_before <= self.max_bytes {
            return None;
        }
        let (prefix, starts) = turn_starts(history);
        if starts.len() < 2 {
            // Only the prefix and at most one turn: nothing removable.
            return None;
        }
        // Find the earliest turn we can start from such that
        // prefix + that turn onward fits, keeping at least the last turn.
        let prefix_bytes = history_bytes(&history[..prefix]);
        let mut keep_from_turn = starts.len() - 1;
        for (turn_index, &start) in starts.iter().enumerate() {
            if prefix_bytes + history_bytes(&history[start..]) <= self.max_bytes {
                keep_from_turn = turn_index;
                break;
            }
        }
        if keep_from_turn == 0 {
            return None;
        }
        let cut = starts[keep_from_turn];
        let messages_before = history.len();
        history.drain(prefix..cut);
        Some(CompactionReport {
            compactor: self.name().to_owned(),
            messages_before,
            messages_after: history.len(),
            bytes_before,
            bytes_after: history_bytes(history),
            turns_removed: keep_from_turn,
        })
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

/// Configuration surface for choosing a compactor. Additional strategies
/// (progressive summarization, token-aware budgets) plug in here without
/// changing callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// Drop oldest whole turns once the history exceeds this many bytes of
    /// content and tool-call arguments. `None` disables compaction.
    pub max_history_bytes: Option<usize>,
}

impl CompactionConfig {
    pub fn build(&self) -> Box<dyn Compactor> {
        match self.max_history_bytes {
            Some(max_bytes) if max_bytes > 0 => Box::new(MaxBytesCompactor { max_bytes }),
            _ => Box::new(NoCompaction),
        }
    }

    /// Layer a per-model budget over this (session-wide) default. The right
    /// history depth depends on the model's context window, so a model
    /// profile may set its own `max_history_bytes`; when it does, that wins.
    pub fn for_model(&self, model_max_history_bytes: Option<usize>) -> Self {
        Self {
            max_history_bytes: model_max_history_bytes.or(self.max_history_bytes),
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
    fn never_removes_the_only_or_last_turn() {
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
    fn works_without_a_system_message() {
        let mut h: Vec<ChatMessage> = (0..3).flat_map(|n| turn(n, false)).collect();
        let report = MaxBytesCompactor { max_bytes: 1 }.compact(&mut h).unwrap();
        assert_eq!(report.turns_removed, 2);
        assert_eq!(h[0].role, MessageRole::User);
        assert!(h[0].content.starts_with("question 2"));
    }

    #[test]
    fn model_budget_overrides_session_default() {
        let session = CompactionConfig {
            max_history_bytes: Some(1 << 20),
        };
        assert_eq!(session.for_model(None).max_history_bytes, Some(1 << 20));
        assert_eq!(session.for_model(Some(4096)).max_history_bytes, Some(4096));
        // A model may also explicitly disable compaction with 0.
        assert_eq!(session.for_model(Some(0)).build().name(), "none");
        assert_eq!(
            CompactionConfig::default()
                .for_model(Some(4096))
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
                max_history_bytes: Some(0)
            }
            .build()
            .name(),
            "none"
        );
        assert_eq!(
            CompactionConfig {
                max_history_bytes: Some(4096)
            }
            .build()
            .name(),
            "max_history_bytes"
        );
    }
}
