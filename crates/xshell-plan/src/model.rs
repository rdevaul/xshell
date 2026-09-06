use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PLAN_SCHEMA_V0: &str = "xshell.plan/v0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: String,
    pub name: String,
    pub flow: FlowIdentity,
    pub interface: PlanInterface,
    pub contracts: Vec<PlanContract>,
    pub tasks: Vec<TaskTemplate>,
    pub regions: Vec<RegionTemplate>,
    pub loops: Vec<LoopTemplate>,
    pub capabilities: CapabilitySummary,
    pub resolution: ResolutionStatus,
}

impl Plan {
    pub fn hash(&self) -> Result<crate::PlanHash, serde_json::Error> {
        crate::hash::plan_hash(self)
    }

    pub fn artifact(&self) -> Result<PlanArtifact, serde_json::Error> {
        Ok(PlanArtifact {
            plan_hash: self.hash()?.to_string(),
            plan: self.clone(),
        })
    }
}

impl PlanArtifact {
    pub fn verify_hash(&self) -> Result<bool, serde_json::Error> {
        Ok(self.plan.hash()?.to_string() == self.plan_hash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanArtifact {
    pub plan_hash: String,
    #[serde(flatten)]
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FlowIdentity {
    pub schema: String,
    pub semantic_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanContract {
    pub id: String,
    pub expression: PlanContractExpression,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PlanInterface {
    pub inputs: Vec<PlanPort>,
    pub outputs: Vec<PlanPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanPort {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: PlanValueType,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanValueType {
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
        item: Box<PlanValueType>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanContractExpression {
    Predicate {
        name: String,
        #[serde(default)]
        arguments: BTreeMap<String, Value>,
    },
    All {
        clauses: Vec<PlanContractExpression>,
    },
    Any {
        clauses: Vec<PlanContractExpression>,
    },
    Not {
        clause: Box<PlanContractExpression>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskTemplate {
    /// Stable within a plan. Runtime identity is `(plan_hash, id, iteration, attempt)`.
    pub id: String,
    pub operation: TaskOperation,
    pub inputs: Vec<InputBinding>,
    pub outputs: Vec<PlanPort>,
    pub readiness: Readiness,
    pub region_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<String>,
    pub capabilities: CapabilitySummary,
    pub resources: PlanResources,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskOperation {
    Input,
    Output,
    Program {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entrypoint: Option<String>,
    },
    Gate {
        contract: String,
    },
    Join {
        strategy: PlanJoinStrategy,
    },
    Promote {
        paths: Vec<String>,
    },
    Discard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputBinding {
    pub port: PlanPort,
    pub sources: Vec<ValueBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValueBinding {
    pub from: PortReference,
    pub selection: ValueSelection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct PortReference {
    pub task: String,
    pub port: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueSelection {
    Direct,
    LoopInitial { loop_region: String },
    CurrentIteration { loop_region: String },
    LoopFinal { loop_region: String },
    PreviousIteration { loop_region: String, state: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Readiness {
    /// Ordinary predecessor tasks whose bound values participate in readiness.
    pub data: DataReadiness,
    /// Ordering-only predecessor tasks.
    pub after: Vec<String>,
    /// Empty means always active. Otherwise any matching route activates the task.
    pub activate_on_any: Vec<RouteCondition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DataReadiness {
    pub mode: ReadinessMode,
    pub tasks: Vec<String>,
}

impl Default for DataReadiness {
    fn default() -> Self {
        Self {
            mode: ReadinessMode::All,
            tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessMode {
    All,
    Any,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct RouteCondition {
    pub gate: String,
    pub outcome: PlanGateOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegionTemplate {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub kind: PlanRegionKind,
    pub direct_members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanRegionKind {
    Scope,
    Parallel,
    Transaction { workspace: String },
    Loop,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoopTemplate {
    pub id: String,
    pub body_order: Vec<String>,
    pub max_iterations: u32,
    pub until: RouteCondition,
    pub on_exhausted: PlanExhaustionPolicy,
    pub states: Vec<LoopState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoopState {
    pub name: String,
    pub value_type: PlanValueType,
    pub target: PortReference,
    pub initial: PortReference,
    pub feedback: PortReference,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PlanJoinStrategy {
    All,
    Any,
    FirstValid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PlanGateOutcome {
    Valid,
    Invalid,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PlanExhaustionPolicy {
    Fail,
    YieldLast,
    Discard,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PlanResources {
    pub timeout_ms: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub output_bytes: Option<u64>,
    pub attempts: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitySummary {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub execute: Vec<String>,
    pub network: Vec<String>,
}

impl CapabilitySummary {
    pub(crate) fn normalize(&mut self) {
        normalize_strings(&mut self.read);
        normalize_strings(&mut self.write);
        normalize_strings(&mut self.execute);
        normalize_strings(&mut self.network);
    }

    pub(crate) fn extend(&mut self, other: &Self) {
        self.read.extend(other.read.iter().cloned());
        self.write.extend(other.write.iter().cloned());
        self.execute.extend(other.execute.iter().cloned());
        self.network.extend(other.network.iter().cloned());
        self.normalize();
    }
}

impl From<&xshell_flow::Capabilities> for CapabilitySummary {
    fn from(capabilities: &xshell_flow::Capabilities) -> Self {
        let mut result = Self {
            read: capabilities.read.clone(),
            write: capabilities.write.clone(),
            execute: capabilities.execute.clone(),
            network: capabilities.network.clone(),
        };
        result.normalize();
        result
    }
}

impl From<&xshell_flow::FlowInterface> for PlanInterface {
    fn from(interface: &xshell_flow::FlowInterface) -> Self {
        Self {
            inputs: interface.inputs.iter().map(PlanPort::from).collect(),
            outputs: interface.outputs.iter().map(PlanPort::from).collect(),
        }
    }
}

impl From<&xshell_flow::Port> for PlanPort {
    fn from(port: &xshell_flow::Port) -> Self {
        Self {
            name: port.name.clone(),
            value_type: PlanValueType::from(&port.value_type),
            optional: port.optional,
        }
    }
}

impl From<&xshell_flow::ValueType> for PlanValueType {
    fn from(value_type: &xshell_flow::ValueType) -> Self {
        match value_type {
            xshell_flow::ValueType::Bool => Self::Bool,
            xshell_flow::ValueType::Int => Self::Int,
            xshell_flow::ValueType::String => Self::String,
            xshell_flow::ValueType::Path => Self::Path,
            xshell_flow::ValueType::Evidence => Self::Evidence,
            xshell_flow::ValueType::ContractReport => Self::ContractReport,
            xshell_flow::ValueType::Receipt => Self::Receipt,
            xshell_flow::ValueType::Artifact { media_type, schema } => Self::Artifact {
                media_type: media_type.clone(),
                schema: schema.clone(),
            },
            xshell_flow::ValueType::List { item } => Self::List {
                item: Box::new(Self::from(item.as_ref())),
            },
        }
    }
}

impl From<&xshell_flow::ContractExpression> for PlanContractExpression {
    fn from(expression: &xshell_flow::ContractExpression) -> Self {
        match expression {
            xshell_flow::ContractExpression::Predicate { name, arguments } => Self::Predicate {
                name: name.clone(),
                arguments: arguments.clone(),
            },
            xshell_flow::ContractExpression::All { clauses } => Self::All {
                clauses: clauses.iter().map(Self::from).collect(),
            },
            xshell_flow::ContractExpression::Any { clauses } => Self::Any {
                clauses: clauses.iter().map(Self::from).collect(),
            },
            xshell_flow::ContractExpression::Not { clause } => Self::Not {
                clause: Box::new(Self::from(clause.as_ref())),
            },
        }
    }
}

impl From<xshell_flow::JoinStrategy> for PlanJoinStrategy {
    fn from(strategy: xshell_flow::JoinStrategy) -> Self {
        match strategy {
            xshell_flow::JoinStrategy::All => Self::All,
            xshell_flow::JoinStrategy::Any => Self::Any,
            xshell_flow::JoinStrategy::FirstValid => Self::FirstValid,
        }
    }
}

impl From<xshell_flow::GateOutcome> for PlanGateOutcome {
    fn from(outcome: xshell_flow::GateOutcome) -> Self {
        match outcome {
            xshell_flow::GateOutcome::Valid => Self::Valid,
            xshell_flow::GateOutcome::Invalid => Self::Invalid,
            xshell_flow::GateOutcome::Error => Self::Error,
            xshell_flow::GateOutcome::Cancelled => Self::Cancelled,
        }
    }
}

impl From<xshell_flow::ExhaustionPolicy> for PlanExhaustionPolicy {
    fn from(policy: xshell_flow::ExhaustionPolicy) -> Self {
        match policy {
            xshell_flow::ExhaustionPolicy::Fail => Self::Fail,
            xshell_flow::ExhaustionPolicy::YieldLast => Self::YieldLast,
            xshell_flow::ExhaustionPolicy::Discard => Self::Discard,
        }
    }
}

impl From<&xshell_flow::Resources> for PlanResources {
    fn from(resources: &xshell_flow::Resources) -> Self {
        Self {
            timeout_ms: resources.timeout_ms,
            memory_bytes: resources.memory_bytes,
            output_bytes: resources.output_bytes,
            attempts: resources.attempts,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolutionStatus {
    pub fully_resolved: bool,
    pub blockers: Vec<ResolutionBlocker>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionBlocker {
    UnresolvedProgram { task: String, source: String },
    UncheckedPredicate { contract: String, predicate: String },
}

fn normalize_strings(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}
