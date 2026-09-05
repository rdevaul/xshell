use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const FLOW_SCHEMA_V0: &str = "xshell.flow/v0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub interface: FlowInterface,
    #[serde(default)]
    pub contracts: Vec<Contract>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub regions: Vec<Region>,
    /// Editor-owned data. It is serialized but excluded from semantic hashes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct FlowInterface {
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: ValueType,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueType {
    Bool,
    Int,
    String,
    Path,
    Evidence,
    ContractReport,
    Receipt,
    Artifact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    List {
        item: Box<ValueType>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub id: String,
    pub expression: ContractExpression,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContractExpression {
    Predicate {
        name: String,
        #[serde(default)]
        arguments: BTreeMap<String, Value>,
    },
    All {
        clauses: Vec<ContractExpression>,
    },
    Any {
        clauses: Vec<ContractExpression>,
    },
    Not {
        clause: Box<ContractExpression>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub inputs: Vec<Port>,
    #[serde(default)]
    pub outputs: Vec<Port>,
    #[serde(flatten)]
    pub kind: NodeKind,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub resources: Resources,
    /// Editor-owned data. It is serialized but excluded from semantic hashes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeKind {
    Input,
    Output,
    Task { program: ProgramReference },
    Gate { contract: String },
    Join { strategy: JoinStrategy },
    Promote { paths: Vec<String> },
    Discard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProgramReference {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum JoinStrategy {
    All,
    Any,
    FirstValid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub execute: Vec<String>,
    pub network: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    pub timeout_ms: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub output_bytes: Option<u64>,
    pub attempts: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Edge {
    pub id: String,
    pub from: Endpoint,
    pub to: Endpoint,
    #[serde(flatten)]
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EdgeKind {
    Data,
    Dependency,
    Route { outcome: GateOutcome },
    Feedback { loop_region: String, state: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    Valid,
    Invalid,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Region {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub nodes: Vec<String>,
    #[serde(flatten)]
    pub kind: RegionKind,
    /// Editor-owned data. It is serialized but excluded from semantic hashes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegionKind {
    Scope,
    Parallel,
    Transaction {
        workspace: String,
    },
    Loop {
        max_iterations: u32,
        until: GateOutcomeReference,
        on_exhausted: ExhaustionPolicy,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GateOutcomeReference {
    pub gate: String,
    pub outcome: GateOutcome,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustionPolicy {
    Fail,
    YieldLast,
    Discard,
}
