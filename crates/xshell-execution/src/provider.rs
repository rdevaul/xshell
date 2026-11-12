//! Tool providers: tools that trusted, compiled-in code adds to the engine.
//!
//! The built-in tools are a closed [`Tool`] enum. A [`ToolProvider`] adds
//! more without opening that enum to anything discovered at run time: a
//! provider is a Rust value that the binary building the engine registers
//! when it starts, the registry is fixed from then on, and every provided
//! tool goes through the same toolset check, policy, approval, audit, budget,
//! and output bound as a built-in one. Untrusted tool sources such as MCP
//! servers stay behind the MCP bridge (`docs/mcp-bridge.md`); see
//! `docs/tool-providers.md` for the design.

use crate::engine::CancellationFlag;
use crate::tools::{Tool, truncate};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use xshell_core::{ToolCall, ToolDefinition};

/// Most tools one registry may provide. Every offered tool's definition is
/// sent with every provider request, so this bounds that standing cost; it
/// also lets a [`ToolSet`](crate::ToolSet) hold provided tools in one word.
pub const MAX_PROVIDED_TOOLS: usize = 64;
/// Longest provided tool name, and longest provider name.
const MAX_NAME_BYTES: usize = 64;
/// Longest tool description. Descriptions are sent on every request.
const MAX_DESCRIPTION_BYTES: usize = 4096;
/// Longest parameter schema, as compact JSON. Schemas are sent on every
/// request too, so they are bounded like descriptions.
const MAX_SCHEMA_BYTES: usize = 16 * 1024;

/// Whether a tool can change anything. Read-only tools are offered to
/// `read_only` subagents; read-write tools only to agents given `inherit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAccess {
    ReadOnly,
    ReadWrite,
}

impl ToolAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }
}

/// Whether a call needs a human decision when no approval policy decides
/// it. `Required` tools are gated like `run_shell`: prompted in `ask` mode,
/// run in `auto`, denied in `off`. A policy can still require approval for a
/// `NotRequired` tool by listing it in `allowed`, or deny it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApproval {
    NotRequired,
    Required,
}

impl ToolApproval {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Required => "required",
        }
    }
}

/// One tool a provider offers: the definition the model sees and the two
/// facts the engine's authorization depends on.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvidedTool {
    pub definition: ToolDefinition,
    pub access: ToolAccess,
    pub approval: ToolApproval,
}

/// Everything a provider is told about one call. Owned, so the call can run
/// as its own task: a provider that panics fails its call, not the turn.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// The call's arguments; always a JSON object. Validating them against
    /// the tool's schema is the provider's job.
    pub arguments: Map<String, Value>,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session the turn belongs to, when the engine runs in a daemon.
    pub session_id: Option<String>,
    /// The subagent making the call (`delegate-1`), or `None` for the root
    /// agent.
    pub agent: Option<String>,
    /// The call's id, as recorded in events and audit.
    pub call_id: String,
    /// Set when the turn is cancelled. The engine also stops awaiting the
    /// call then, so this matters only to work the provider moved off the
    /// task, such as a `spawn_blocking` job that can stop early.
    pub cancellation: CancellationFlag,
}

/// Trusted code that adds tools to the engine.
///
/// [`tools`](Self::tools) is read once, when the registry is built, and must
/// describe exactly what [`call`](Self::call) does: the registry's identity,
/// recorded with every audited turn, is computed from it.
///
/// A call runs on the turn's async runtime, bounded by the turn's per-call
/// time limit and dropped when the turn is cancelled, so it must be
/// cancellation-safe and must not block the thread; move blocking work to
/// `tokio::task::spawn_blocking`. Output beyond the engine's tool-output
/// bound is truncated. An `Err` is reported to the model as a tool error.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// A short name for this provider, recorded in audit records.
    fn name(&self) -> &str;

    /// The tools this provider offers, in the order they are advertised.
    fn tools(&self) -> Vec<ProvidedTool>;

    /// Run `tool`, one of the names [`tools`](Self::tools) returned.
    async fn call(&self, tool: &str, context: ToolContext) -> Result<String>;
}

/// A provided tool and the provider that runs it.
pub struct RegisteredTool {
    provider: Arc<dyn ToolProvider>,
    provider_name: String,
    tool: ProvidedTool,
}

impl RegisteredTool {
    pub fn name(&self) -> &str {
        &self.tool.definition.name
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn definition(&self) -> &ToolDefinition {
        &self.tool.definition
    }

    pub fn access(&self) -> ToolAccess {
        self.tool.access
    }

    pub fn approval(&self) -> ToolApproval {
        self.tool.approval
    }
}

impl std::fmt::Debug for RegisteredTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredTool")
            .field("provider", &self.provider_name)
            .field("name", &self.name())
            .field("access", &self.tool.access)
            .field("approval", &self.tool.approval)
            .finish()
    }
}

/// Every provided tool one engine can offer. Built once and never changed:
/// a different tool set is a different registry, with a different identity.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    identity: Option<String>,
}

impl ToolRegistry {
    /// The registry with no provided tools, shared by every engine that
    /// registers none.
    pub fn empty() -> Arc<Self> {
        static EMPTY: OnceLock<Arc<ToolRegistry>> = OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| Arc::new(Self::default())))
    }

    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// The provided tools, in advertisement order.
    pub fn tools(&self) -> &[RegisteredTool] {
        &self.tools
    }

    /// A lowercase-hex SHA-256 over the canonical encoding of every provided tool: its
    /// provider, name, description, schema, access, and approval, in order.
    /// `None` when the registry is empty. Audit records carry it, so a
    /// reviewer can tell which tool definitions a turn's agent was offered.
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        self.tools.iter().position(|tool| tool.name() == name)
    }

    pub(crate) fn get(&self, index: usize) -> Option<&RegisteredTool> {
        self.tools.get(index)
    }
}

/// Collects providers and checks them as a whole: names valid and unique
/// across built-in and provided tools, definitions well formed, and no more
/// than [`MAX_PROVIDED_TOOLS`].
#[derive(Default)]
pub struct ToolRegistryBuilder {
    providers: Vec<Arc<dyn ToolProvider>>,
}

impl ToolRegistryBuilder {
    pub fn provider(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    pub fn build(self) -> Result<Arc<ToolRegistry>> {
        let mut tools = Vec::new();
        let mut names = HashSet::new();
        let mut providers = HashSet::new();
        for provider in self.providers {
            let provider_name = provider.name().to_owned();
            validate_name("provider", &provider_name)?;
            if !providers.insert(provider_name.clone()) {
                bail!("provider {provider_name:?} is registered twice");
            }
            for tool in provider.tools() {
                validate_tool(&provider_name, &tool)?;
                let name = tool.definition.name.clone();
                if Tool::from_name(&name).is_some() {
                    bail!("provider {provider_name:?}: {name:?} is a built-in tool");
                }
                if !names.insert(name.clone()) {
                    bail!("provider {provider_name:?}: tool {name:?} is provided twice");
                }
                tools.push(RegisteredTool {
                    provider: Arc::clone(&provider),
                    provider_name: provider_name.clone(),
                    tool,
                });
            }
        }
        if tools.len() > MAX_PROVIDED_TOOLS {
            bail!(
                "{} provided tools exceed the limit of {MAX_PROVIDED_TOOLS}",
                tools.len()
            );
        }
        let identity = (!tools.is_empty()).then(|| registry_identity(&tools));
        Ok(Arc::new(ToolRegistry { tools, identity }))
    }
}

/// Names are what policy rules, audit records, and the model use, so they
/// follow the built-in tools' shape: lowercase ASCII words joined by `_`.
fn validate_name(kind: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= MAX_NAME_BYTES
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        bail!(
            "{kind} name {name:?} must be 1 to {MAX_NAME_BYTES} characters of a-z, 0-9, and _, \
starting with a letter"
        );
    }
    Ok(())
}

fn validate_tool(provider: &str, tool: &ProvidedTool) -> Result<()> {
    let name = &tool.definition.name;
    validate_name("tool", name)
        .map_err(|error| anyhow::anyhow!("provider {provider:?}: {error}"))?;
    let description = &tool.definition.description;
    if description.trim().is_empty() {
        bail!("provider {provider:?}: tool {name:?} has no description");
    }
    if description.len() > MAX_DESCRIPTION_BYTES {
        bail!(
            "provider {provider:?}: tool {name:?} has a description longer than \
{MAX_DESCRIPTION_BYTES} bytes"
        );
    }
    // Calls carry a JSON object of arguments, so the schema must describe one.
    let is_object_schema = tool
        .definition
        .parameters
        .get("type")
        .and_then(Value::as_str)
        == Some("object");
    if !is_object_schema {
        bail!("provider {provider:?}: tool {name:?} parameters must be an object schema");
    }
    let schema_bytes =
        serde_json::to_vec(&tool.definition.parameters).map_or(usize::MAX, |encoded| encoded.len());
    if schema_bytes > MAX_SCHEMA_BYTES {
        bail!(
            "provider {provider:?}: tool {name:?} has a parameter schema larger than \
{MAX_SCHEMA_BYTES} bytes"
        );
    }
    Ok(())
}

fn registry_identity(tools: &[RegisteredTool]) -> String {
    let encoded: Vec<Value> = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "provider": tool.provider_name,
                "name": tool.name(),
                "description": tool.tool.definition.description,
                "parameters": tool.tool.definition.parameters,
                "access": tool.tool.access.as_str(),
                "approval": tool.tool.approval.as_str(),
            })
        })
        .collect();
    let mut bytes = Vec::new();
    write_canonical(&Value::Array(encoded), &mut bytes);
    hex::encode(Sha256::digest(&bytes))
}

/// JSON with object keys sorted at every level, so the identity does not
/// depend on how a schema was built or on serde_json's map ordering.
fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).expect("strings always serialize"));
                out.push(b':');
                write_canonical(value, out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend(serde_json::to_vec(scalar).expect("scalars always serialize")),
    }
}

/// What the engine knows about the call, beyond the call itself.
pub(crate) struct CallScope<'a> {
    pub cwd: &'a std::path::Path,
    pub session_id: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub call_id: &'a str,
    pub cancellation: CancellationFlag,
    pub timeout: Duration,
}

/// Run a provided tool and return its result for the model.
///
/// The call runs as its own task so a provider panic becomes a tool error,
/// and the task is aborted when the turn is cancelled or the time limit
/// passes, so a provider cannot hold a turn open past either.
pub(crate) async fn execute_provided(
    tool: &RegisteredTool,
    call: &ToolCall,
    scope: CallScope<'_>,
) -> String {
    let Some(arguments) = call.arguments.as_object() else {
        return format!("tool error: {} arguments must be a JSON object", call.name);
    };
    let context = ToolContext {
        arguments: arguments.clone(),
        cwd: scope.cwd.to_owned(),
        session_id: scope.session_id.map(str::to_owned),
        agent: scope.agent.map(str::to_owned),
        call_id: scope.call_id.to_owned(),
        cancellation: scope.cancellation.clone(),
    };
    let provider = Arc::clone(&tool.provider);
    let name = tool.name().to_owned();
    let mut task = tokio::spawn(async move { provider.call(&name, context).await });
    let outcome = tokio::select! {
        joined = tokio::time::timeout(scope.timeout, &mut task) => joined,
        () = scope.cancellation.wait() => {
            task.abort();
            return format!("tool error: {} cancelled with the agent turn", call.name);
        }
    };
    match outcome {
        Ok(Ok(Ok(output))) => truncate(output),
        Ok(Ok(Err(error))) => format!("tool error: {error:#}"),
        Ok(Err(join_error)) if join_error.is_panic() => {
            format!(
                "tool error: {} failed unexpectedly (provider panicked)",
                call.name
            )
        }
        Ok(Err(_)) => format!("tool error: {} was stopped", call.name),
        Err(_elapsed) => {
            task.abort();
            format!(
                "tool error: {} did not finish within {} s",
                call.name,
                scope.timeout.as_secs()
            )
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) struct Fixed {
        pub name: &'static str,
        pub tools: Vec<ProvidedTool>,
    }

    #[async_trait]
    impl ToolProvider for Fixed {
        fn name(&self) -> &str {
            self.name
        }

        fn tools(&self) -> Vec<ProvidedTool> {
            self.tools.clone()
        }

        async fn call(&self, tool: &str, context: ToolContext) -> Result<String> {
            match tool {
                "fail" => bail!("it failed"),
                "panic" => panic!("provider bug"),
                "hang" => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                "big" => Ok("x".repeat(200 * 1024)),
                _ => Ok(format!("{tool} {}", Value::Object(context.arguments))),
            }
        }
    }

    pub(crate) fn tool(name: &str, access: ToolAccess, approval: ToolApproval) -> ProvidedTool {
        ProvidedTool {
            definition: ToolDefinition {
                name: name.into(),
                description: format!("The {name} tool."),
                parameters: json!({"type": "object", "properties": {}}),
            },
            access,
            approval,
        }
    }

    fn registry(tools: &[&str]) -> Result<Arc<ToolRegistry>> {
        ToolRegistry::builder()
            .provider(Arc::new(Fixed {
                name: "fixed",
                tools: tools
                    .iter()
                    .map(|name| tool(name, ToolAccess::ReadOnly, ToolApproval::NotRequired))
                    .collect(),
            }))
            .build()
    }

    #[test]
    fn empty_registry_has_no_identity() {
        assert!(ToolRegistry::empty().is_empty());
        assert_eq!(ToolRegistry::empty().identity(), None);
        assert_eq!(registry(&[]).unwrap().identity(), None);
    }

    #[test]
    fn rejects_names_that_collide_or_are_malformed() {
        for (tools, message) in [
            (&["run_shell"][..], "is a built-in tool"),
            (&["memory_get", "memory_get"][..], "provided twice"),
            (&["Memory"][..], "must be 1 to"),
            (&["memory-get"][..], "must be 1 to"),
            (&["9lives"][..], "must be 1 to"),
            (&[""][..], "must be 1 to"),
        ] {
            let error = registry(tools).unwrap_err().to_string();
            assert!(error.contains(message), "{tools:?}: {error}");
        }
    }

    #[test]
    fn rejects_a_provider_registered_twice() {
        let provider = Arc::new(Fixed {
            name: "fixed",
            tools: vec![],
        });
        let error = ToolRegistry::builder()
            .provider(provider.clone())
            .provider(provider)
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("registered twice"));
    }

    #[test]
    fn rejects_non_object_schemas_and_empty_descriptions() {
        let mut bad_schema = tool("a", ToolAccess::ReadOnly, ToolApproval::NotRequired);
        bad_schema.definition.parameters = json!({"type": "string"});
        let mut no_description = tool("b", ToolAccess::ReadOnly, ToolApproval::NotRequired);
        no_description.definition.description = "  ".into();
        let mut huge_schema = tool("c", ToolAccess::ReadOnly, ToolApproval::NotRequired);
        huge_schema.definition.parameters = json!({
            "type": "object",
            "description": "x".repeat(MAX_SCHEMA_BYTES),
        });
        for (bad, message) in [
            (bad_schema, "object schema"),
            (no_description, "no description"),
            (huge_schema, "parameter schema larger than"),
        ] {
            let error = ToolRegistry::builder()
                .provider(Arc::new(Fixed {
                    name: "fixed",
                    tools: vec![bad],
                }))
                .build()
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn caps_the_number_of_provided_tools() {
        let names: Vec<String> = (0..=MAX_PROVIDED_TOOLS).map(|i| format!("t{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(registry(&names[..MAX_PROVIDED_TOOLS]).is_ok());
        let error = registry(&names).unwrap_err();
        assert!(error.to_string().contains("exceed the limit"));
    }

    #[test]
    fn identity_tracks_definitions_not_key_order() {
        let a = registry(&["memory_get", "memory_put"]).unwrap();
        let b = registry(&["memory_get", "memory_put"]).unwrap();
        assert_eq!(a.identity(), b.identity());
        assert_eq!(a.identity().unwrap().len(), 64);

        let reordered = registry(&["memory_put", "memory_get"]).unwrap();
        assert_ne!(a.identity(), reordered.identity());

        let mut changed = tool("memory_get", ToolAccess::ReadOnly, ToolApproval::Required);
        changed.definition.description = "The memory_get tool.".into();
        let changed = ToolRegistry::builder()
            .provider(Arc::new(Fixed {
                name: "fixed",
                tools: vec![changed],
            }))
            .build()
            .unwrap();
        let unchanged = registry(&["memory_get"]).unwrap();
        assert_ne!(changed.identity(), unchanged.identity());

        let mut ordered = Vec::new();
        write_canonical(&json!({"b": 1, "a": {"d": 2, "c": [3]}}), &mut ordered);
        assert_eq!(ordered, br#"{"a":{"c":[3],"d":2},"b":1}"#);
    }

    fn scope(timeout: Duration, cancellation: CancellationFlag) -> CallScope<'static> {
        CallScope {
            cwd: std::path::Path::new("/"),
            session_id: Some("s1"),
            agent: None,
            call_id: "call_1",
            cancellation,
            timeout,
        }
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments,
        }
    }

    async fn run(name: &str, arguments: Value, timeout: Duration) -> String {
        let registry = registry(&["echo", "fail", "panic", "hang", "big"]).unwrap();
        let tool = registry.get(registry.position(name).unwrap()).unwrap();
        execute_provided(
            tool,
            &call(name, arguments),
            scope(timeout, CancellationFlag::default()),
        )
        .await
    }

    #[tokio::test]
    async fn results_errors_and_panics_become_tool_results() {
        let long = Duration::from_secs(30);
        assert_eq!(run("echo", json!({"k": 1}), long).await, r#"echo {"k":1}"#);
        assert_eq!(run("fail", json!({}), long).await, "tool error: it failed");
        assert!(
            run("panic", json!({}), long)
                .await
                .contains("provider panicked")
        );
        assert!(
            run("echo", json!([1]), long)
                .await
                .contains("must be a JSON object")
        );
        let big = run("big", json!({}), long).await;
        assert!(big.ends_with("[tool output truncated]"));
        assert!(big.len() < 100 * 1024);
    }

    #[tokio::test]
    async fn a_hung_call_is_bounded_by_the_time_limit() {
        let result = run("hang", json!({}), Duration::from_millis(50)).await;
        assert!(result.contains("did not finish"), "{result}");
    }

    #[tokio::test]
    async fn cancelling_the_turn_stops_a_hung_call() {
        let registry = registry(&["hang"]).unwrap();
        let tool = registry.get(0).unwrap();
        let cancellation = CancellationFlag::default();
        let canceller = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        });
        let result = execute_provided(
            tool,
            &call("hang", json!({})),
            scope(Duration::from_secs(60), cancellation),
        )
        .await;
        assert!(result.contains("cancelled"), "{result}");
    }
}
