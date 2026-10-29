use std::collections::HashSet;
use xshell_core::ToolCall;

/// Assigns the ids xshell uses for tool calls outside the provider
/// conversation: execution events, approval keys, and audit records.
///
/// Provider ids are not unique enough for that. The Ollama adapter numbers
/// calls from zero in every response, the OpenAI-compatible adapter does the
/// same when a server omits ids, and agents sharing one root turn draw from
/// unrelated id spaces. Provider ids are still kept unchanged in history,
/// because some providers validate the ids they are sent back.
///
/// Within one turn the first occurrence of an id keeps it, a repeat becomes
/// `id~2`, `id~3`, ..., and an empty id becomes `call~N`. A scope prefixes
/// every id with `scope/` so that agents sharing a root turn never collide.
#[derive(Debug, Default)]
pub(crate) struct CallIds {
    scope: Option<String>,
    seen: HashSet<String>,
}

impl CallIds {
    pub(crate) fn new(scope: Option<&str>) -> Self {
        Self {
            scope: scope.map(str::to_owned),
            seen: HashSet::new(),
        }
    }

    /// A copy of `call` carrying its turn-unique scoped id.
    pub(crate) fn scoped(&mut self, call: &ToolCall) -> ToolCall {
        let (base, first_suffix) = if call.id.is_empty() {
            ("call", 1)
        } else {
            (call.id.as_str(), 2)
        };
        let mut id = if call.id.is_empty() || self.seen.contains(base) {
            (first_suffix..)
                .map(|suffix| format!("{base}~{suffix}"))
                .find(|candidate| !self.seen.contains(candidate))
                .expect("an unused suffix always exists")
        } else {
            base.to_owned()
        };
        self.seen.insert(id.clone());
        if let Some(scope) = &self.scope {
            id = format!("{scope}/{id}");
        }
        ToolCall {
            id,
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "list_directory".into(),
            arguments: json!({}),
        }
    }

    fn ids(scope: Option<&str>, provider_ids: &[&str]) -> Vec<String> {
        let mut ids = CallIds::new(scope);
        provider_ids
            .iter()
            .map(|id| ids.scoped(&call(id)).id)
            .collect()
    }

    #[test]
    fn unique_provider_ids_are_unchanged() {
        assert_eq!(ids(None, &["call_a", "call_b"]), ["call_a", "call_b"]);
    }

    #[test]
    fn repeated_ids_are_disambiguated_across_responses() {
        // Ollama numbers every response's calls from zero.
        assert_eq!(
            ids(
                None,
                &[
                    "ollama-call-0",
                    "ollama-call-0",
                    "ollama-call-1",
                    "ollama-call-0"
                ]
            ),
            [
                "ollama-call-0",
                "ollama-call-0~2",
                "ollama-call-1",
                "ollama-call-0~3"
            ]
        );
    }

    #[test]
    fn a_suffix_never_reuses_an_id_the_provider_already_sent() {
        assert_eq!(ids(None, &["x~2", "x", "x"]), ["x~2", "x", "x~3"]);
    }

    #[test]
    fn empty_ids_are_numbered() {
        assert_eq!(
            ids(None, &["", "", "call~1"]),
            ["call~1", "call~2", "call~1~2"]
        );
    }

    #[test]
    fn scopes_keep_agents_apart() {
        let parent = ids(None, &["ollama-call-0"]);
        let child = ids(Some("delegate-1"), &["ollama-call-0", "ollama-call-0"]);
        assert_eq!(parent, ["ollama-call-0"]);
        assert_eq!(
            child,
            ["delegate-1/ollama-call-0", "delegate-1/ollama-call-0~2"]
        );
    }

    #[test]
    fn scoping_preserves_name_and_arguments() {
        let original = ToolCall {
            id: "a".into(),
            name: "run_shell".into(),
            arguments: json!({"command": "true"}),
        };
        let scoped = CallIds::new(Some("s")).scoped(&original);
        assert_eq!(scoped.name, original.name);
        assert_eq!(scoped.arguments, original.arguments);
    }
}
