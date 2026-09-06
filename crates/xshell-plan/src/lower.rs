use crate::{
    CapabilitySummary, DataReadiness, FlowIdentity, InputBinding, LoopState, LoopTemplate,
    PLAN_SCHEMA_V0, Plan, PlanContract, PlanRegionKind, PortReference, Readiness, ReadinessMode,
    RegionTemplate, ResolutionBlocker, ResolutionStatus, RouteCondition, TaskOperation,
    TaskTemplate, ValueBinding, ValueSelection,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use xshell_flow::{Edge, EdgeKind, Flow, Node, NodeKind, Region, RegionKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiagnostic {
    pub code: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    diagnostics: Vec<PlanDiagnostic>,
}

impl PlanError {
    pub fn diagnostics(&self) -> &[PlanDiagnostic] {
        &self.diagnostics
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "plan lowering failed with {} error(s)",
            self.diagnostics.len()
        )?;
        for diagnostic in &self.diagnostics {
            writeln!(
                formatter,
                "{} at {}: {}",
                diagnostic.code, diagnostic.path, diagnostic.message
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for PlanError {}

pub fn lower(flow: &Flow) -> Result<Plan, PlanError> {
    if let Err(error) = flow.validate() {
        return Err(PlanError {
            diagnostics: error
                .diagnostics()
                .iter()
                .map(|diagnostic| PlanDiagnostic {
                    code: format!("P-{}", diagnostic.code),
                    path: diagnostic.path.clone(),
                    message: diagnostic.message.clone(),
                })
                .collect(),
        });
    }

    let mut planner = Planner::new(flow);
    let region_paths = planner.region_paths();
    planner.check_capability_paths();
    planner.check_loop_boundaries(&region_paths);
    if !planner.diagnostics.is_empty() {
        return Err(planner.finish_error());
    }

    let order = topological_order(flow, None);
    let mut tasks = Vec::with_capacity(flow.nodes.len());
    for node_id in order {
        let node = planner.nodes[node_id.as_str()];
        tasks.push(planner.lower_task(node, &region_paths));
    }

    let loops = planner.lower_loops();
    let regions = planner.lower_regions();
    crate::conflict::check_write_conflicts(flow, &tasks, &mut planner.diagnostics);
    if !planner.diagnostics.is_empty() {
        return Err(planner.finish_error());
    }

    let mut capabilities = CapabilitySummary::default();
    for task in &tasks {
        capabilities.extend(&task.capabilities);
    }

    let mut contracts: Vec<_> = flow
        .contracts
        .iter()
        .map(|contract| PlanContract {
            id: contract.id.clone(),
            expression: (&contract.expression).into(),
        })
        .collect();
    contracts.sort_by(|left, right| left.id.cmp(&right.id));

    let mut interface = crate::PlanInterface::from(&flow.interface);
    interface
        .inputs
        .sort_by(|left, right| left.name.cmp(&right.name));
    interface
        .outputs
        .sort_by(|left, right| left.name.cmp(&right.name));

    let mut blockers: Vec<_> = tasks
        .iter()
        .filter_map(|task| match &task.operation {
            TaskOperation::Program { source, .. } => Some(ResolutionBlocker::UnresolvedProgram {
                task: task.id.clone(),
                source: source.clone(),
            }),
            _ => None,
        })
        .collect();
    for contract in &flow.contracts {
        collect_predicate_blockers(&contract.id, &contract.expression, &mut blockers);
    }
    blockers.sort();
    blockers.dedup();

    Ok(Plan {
        schema: PLAN_SCHEMA_V0.into(),
        name: flow.name.clone(),
        flow: FlowIdentity {
            schema: flow.schema.clone(),
            semantic_sha256: flow
                .semantic_hash()
                .expect("validated flow can be encoded")
                .to_string(),
        },
        interface,
        contracts,
        tasks,
        regions,
        loops,
        capabilities,
        resolution: ResolutionStatus {
            fully_resolved: blockers.is_empty(),
            blockers,
        },
    })
}

struct Planner<'a> {
    flow: &'a Flow,
    nodes: BTreeMap<&'a str, &'a Node>,
    regions: BTreeMap<&'a str, &'a Region>,
    diagnostics: Vec<PlanDiagnostic>,
}

impl<'a> Planner<'a> {
    fn new(flow: &'a Flow) -> Self {
        Self {
            flow,
            nodes: flow
                .nodes
                .iter()
                .map(|node| (node.id.as_str(), node))
                .collect(),
            regions: flow
                .regions
                .iter()
                .map(|region| (region.id.as_str(), region))
                .collect(),
            diagnostics: Vec::new(),
        }
    }

    fn finish_error(self) -> PlanError {
        PlanError {
            diagnostics: self.diagnostics,
        }
    }

    fn region_paths(&mut self) -> BTreeMap<String, Vec<String>> {
        let mut result = BTreeMap::new();
        for node in &self.flow.nodes {
            let direct: Vec<_> = self
                .flow
                .regions
                .iter()
                .filter(|region| region.nodes.contains(&node.id))
                .collect();
            for (left_index, left) in direct.iter().enumerate() {
                for right in direct.iter().skip(left_index + 1) {
                    if !self.is_ancestor(&left.id, &right.id)
                        && !self.is_ancestor(&right.id, &left.id)
                    {
                        self.error(
                            "P0101",
                            format!("nodes.{}.regions", node.id),
                            format!(
                                "node belongs to unrelated regions {:?} and {:?}; regions must form a lexical parent chain",
                                left.id, right.id
                            ),
                        );
                    }
                }
            }

            let mut all = BTreeSet::new();
            for region in direct {
                let mut current = Some(region.id.as_str());
                while let Some(id) = current {
                    all.insert(id.to_owned());
                    current = self.regions[id].parent.as_deref();
                }
            }
            let mut path: Vec<_> = all.into_iter().collect();
            path.sort_by_key(|id| (self.region_depth(id), id.clone()));

            let loops: Vec<_> = path
                .iter()
                .filter(|id| matches!(self.regions[id.as_str()].kind, RegionKind::Loop { .. }))
                .cloned()
                .collect();
            if loops.len() > 1 {
                self.error(
                    "P0102",
                    format!("nodes.{}.regions", node.id),
                    "Plan V0 does not support nested loop regions",
                );
            }
            result.insert(node.id.clone(), path);
        }
        result
    }

    fn is_ancestor(&self, candidate: &str, region: &str) -> bool {
        let mut current = self
            .regions
            .get(region)
            .and_then(|region| region.parent.as_deref());
        while let Some(id) = current {
            if id == candidate {
                return true;
            }
            current = self
                .regions
                .get(id)
                .and_then(|region| region.parent.as_deref());
        }
        false
    }

    fn region_depth(&self, region: &str) -> usize {
        let mut depth = 0;
        let mut current = self.regions[region].parent.as_deref();
        while let Some(id) = current {
            depth += 1;
            current = self.regions[id].parent.as_deref();
        }
        depth
    }

    fn check_capability_paths(&mut self) {
        for node in &self.flow.nodes {
            for (family, paths) in [
                ("read", &node.capabilities.read),
                ("write", &node.capabilities.write),
            ] {
                for (index, path) in paths.iter().enumerate() {
                    if let Err(message) = normalize_workspace_path(path, false) {
                        self.error(
                            "P0201",
                            format!("nodes.{}.capabilities.{family}[{index}]", node.id),
                            message,
                        );
                    }
                }
            }
            if let NodeKind::Promote { paths } = &node.kind {
                for (index, path) in paths.iter().enumerate() {
                    if let Err(message) = normalize_workspace_path(path, false) {
                        self.error(
                            "P0202",
                            format!("nodes.{}.paths[{index}]", node.id),
                            message,
                        );
                    }
                }
            }
            for (field, value) in [
                ("timeout_ms", node.resources.timeout_ms),
                ("memory_bytes", node.resources.memory_bytes),
                ("output_bytes", node.resources.output_bytes),
            ] {
                if value == Some(0) {
                    self.error(
                        "P0204",
                        format!("nodes.{}.resources.{field}", node.id),
                        "resource bounds must be greater than zero",
                    );
                }
            }
            if node.resources.attempts == Some(0) {
                self.error(
                    "P0204",
                    format!("nodes.{}.resources.attempts", node.id),
                    "resource bounds must be greater than zero",
                );
            }
        }
        for region in &self.flow.regions {
            if let RegionKind::Transaction { workspace } = &region.kind
                && let Err(message) = normalize_workspace_path(workspace, true)
            {
                self.error("P0203", format!("regions.{}.workspace", region.id), message);
            }
        }
    }

    fn check_loop_boundaries(&mut self, paths: &BTreeMap<String, Vec<String>>) {
        for edge in &self.flow.edges {
            let from_loop = self.loop_for(&edge.from.node, paths);
            let to_loop = self.loop_for(&edge.to.node, paths);
            if !matches!(edge.kind, EdgeKind::Feedback { .. })
                && from_loop.is_some()
                && to_loop.is_some()
                && from_loop != to_loop
            {
                self.error(
                    "P0301",
                    format!("edges.{}", edge.id),
                    "Plan V0 cannot bind values directly between different loop regions",
                );
            }
        }

        for region in &self.flow.regions {
            let RegionKind::Loop { .. } = region.kind else {
                continue;
            };
            let members: BTreeSet<_> = region.nodes.iter().map(String::as_str).collect();
            let feedback_edges: Vec<_> = self
                .flow
                .edges
                .iter()
                .filter(|edge| {
                    matches!(
                        &edge.kind,
                        EdgeKind::Feedback { loop_region, .. } if loop_region == &region.id
                    )
                })
                .collect();
            let mut states = BTreeSet::new();
            for feedback in feedback_edges {
                let EdgeKind::Feedback { state, .. } = &feedback.kind else {
                    unreachable!();
                };
                if !states.insert(state.as_str()) {
                    self.error(
                        "P0302",
                        format!("edges.{}.state", feedback.id),
                        format!("duplicate feedback state {state:?} in loop {:?}", region.id),
                    );
                }
                let initial: Vec<_> = self
                    .flow
                    .edges
                    .iter()
                    .filter(|edge| {
                        matches!(edge.kind, EdgeKind::Data)
                            && edge.to == feedback.to
                            && !members.contains(edge.from.node.as_str())
                    })
                    .collect();
                if initial.len() != 1 {
                    self.error(
                        "P0303",
                        format!("edges.{}", feedback.id),
                        format!(
                            "loop feedback state {state:?} requires exactly one initial producer outside the loop; found {}",
                            initial.len()
                        ),
                    );
                }
            }
        }
    }

    fn lower_task(&self, node: &Node, paths: &BTreeMap<String, Vec<String>>) -> TaskTemplate {
        let loop_context = self.loop_for(&node.id, paths);
        let transaction = paths[&node.id]
            .iter()
            .rev()
            .find(|id| {
                matches!(
                    self.regions[id.as_str()].kind,
                    RegionKind::Transaction { .. }
                )
            })
            .cloned();

        let mut inputs: Vec<_> = node
            .inputs
            .iter()
            .map(|port| {
                let mut sources: Vec<_> = self
                    .flow
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.to.node == node.id
                            && edge.to.port.as_deref() == Some(port.name.as_str())
                            && matches!(edge.kind, EdgeKind::Data | EdgeKind::Feedback { .. })
                    })
                    .map(|edge| self.lower_value_binding(edge, paths))
                    .collect();
                sources.sort_by(|left, right| {
                    left.selection
                        .cmp(&right.selection)
                        .then_with(|| left.from.cmp(&right.from))
                });
                InputBinding {
                    port: port.into(),
                    sources,
                }
            })
            .collect();
        inputs.sort_by(|left, right| left.port.name.cmp(&right.port.name));

        let mut outputs: Vec<crate::PlanPort> = node.outputs.iter().map(Into::into).collect();
        outputs.sort_by(|left, right| left.name.cmp(&right.name));

        let mut data: Vec<_> = self
            .flow
            .edges
            .iter()
            .filter(|edge| edge.to.node == node.id && matches!(edge.kind, EdgeKind::Data))
            .map(|edge| edge.from.node.clone())
            .collect();
        sort_deduplicate(&mut data);
        let mut after: Vec<_> = self
            .flow
            .edges
            .iter()
            .filter(|edge| edge.to.node == node.id && matches!(edge.kind, EdgeKind::Dependency))
            .map(|edge| edge.from.node.clone())
            .collect();
        sort_deduplicate(&mut after);
        let mut activate_on_any: Vec<_> = self
            .flow
            .edges
            .iter()
            .filter_map(|edge| {
                if edge.to.node != node.id {
                    return None;
                }
                match edge.kind {
                    EdgeKind::Route { outcome } => Some(RouteCondition {
                        gate: edge.from.node.clone(),
                        outcome: outcome.into(),
                    }),
                    _ => None,
                }
            })
            .collect();
        activate_on_any.sort();
        activate_on_any.dedup();

        let operation = match &node.kind {
            NodeKind::Input => TaskOperation::Input,
            NodeKind::Output => TaskOperation::Output,
            NodeKind::Task { program } => TaskOperation::Program {
                source: program.source.clone(),
                entrypoint: program.entrypoint.clone(),
            },
            NodeKind::Gate { contract } => TaskOperation::Gate {
                contract: contract.clone(),
            },
            NodeKind::Join { strategy } => TaskOperation::Join {
                strategy: (*strategy).into(),
            },
            NodeKind::Promote { paths } => {
                let mut paths: Vec<_> = paths
                    .iter()
                    .map(|path| normalize_workspace_path(path, false).expect("checked path"))
                    .collect();
                sort_deduplicate(&mut paths);
                TaskOperation::Promote { paths }
            }
            NodeKind::Discard => TaskOperation::Discard,
        };

        TaskTemplate {
            id: node.id.clone(),
            operation,
            inputs,
            outputs,
            readiness: Readiness {
                data: DataReadiness {
                    mode: match &node.kind {
                        NodeKind::Join {
                            strategy:
                                xshell_flow::JoinStrategy::Any | xshell_flow::JoinStrategy::FirstValid,
                        } => ReadinessMode::Any,
                        _ => ReadinessMode::All,
                    },
                    tasks: data,
                },
                after,
                activate_on_any,
            },
            region_path: paths[&node.id].clone(),
            transaction,
            loop_context,
            capabilities: normalized_capabilities(&node.capabilities),
            resources: (&node.resources).into(),
        }
    }

    fn lower_value_binding(
        &self,
        edge: &Edge,
        paths: &BTreeMap<String, Vec<String>>,
    ) -> ValueBinding {
        let from_loop = self.loop_for(&edge.from.node, paths);
        let to_loop = self.loop_for(&edge.to.node, paths);
        let selection = match &edge.kind {
            EdgeKind::Feedback { loop_region, state } => ValueSelection::PreviousIteration {
                loop_region: loop_region.clone(),
                state: state.clone(),
            },
            EdgeKind::Data => match (from_loop.as_deref(), to_loop.as_deref()) {
                (None, None) => ValueSelection::Direct,
                (None, Some(loop_region)) => ValueSelection::LoopInitial {
                    loop_region: loop_region.to_owned(),
                },
                (Some(left), Some(right)) if left == right => ValueSelection::CurrentIteration {
                    loop_region: left.to_owned(),
                },
                (Some(loop_region), None) => ValueSelection::LoopFinal {
                    loop_region: loop_region.to_owned(),
                },
                _ => unreachable!("cross-loop binding rejected before lowering"),
            },
            _ => unreachable!("only value edges become bindings"),
        };
        ValueBinding {
            from: port_reference(&edge.from),
            selection,
        }
    }

    fn lower_regions(&self) -> Vec<RegionTemplate> {
        let mut regions: Vec<_> = self
            .flow
            .regions
            .iter()
            .map(|region| {
                let kind = match &region.kind {
                    RegionKind::Scope => PlanRegionKind::Scope,
                    RegionKind::Parallel => PlanRegionKind::Parallel,
                    RegionKind::Transaction { workspace } => PlanRegionKind::Transaction {
                        workspace: normalize_workspace_path(workspace, true).expect("checked path"),
                    },
                    RegionKind::Loop { .. } => PlanRegionKind::Loop,
                };
                let mut direct_members = region.nodes.clone();
                sort_deduplicate(&mut direct_members);
                RegionTemplate {
                    id: region.id.clone(),
                    parent: region.parent.clone(),
                    kind,
                    direct_members,
                }
            })
            .collect();
        regions.sort_by(|left, right| left.id.cmp(&right.id));
        regions
    }

    fn lower_loops(&self) -> Vec<LoopTemplate> {
        let mut loops = Vec::new();
        for region in &self.flow.regions {
            let RegionKind::Loop {
                max_iterations,
                until,
                on_exhausted,
            } = &region.kind
            else {
                continue;
            };
            let members: BTreeSet<_> = region.nodes.iter().map(String::as_str).collect();
            let mut states = Vec::new();
            for feedback in &self.flow.edges {
                let EdgeKind::Feedback { loop_region, state } = &feedback.kind else {
                    continue;
                };
                if loop_region != &region.id {
                    continue;
                }
                let initial = self
                    .flow
                    .edges
                    .iter()
                    .find(|edge| {
                        matches!(edge.kind, EdgeKind::Data)
                            && edge.to == feedback.to
                            && !members.contains(edge.from.node.as_str())
                    })
                    .expect("checked loop initial value");
                let target_node = self.nodes[feedback.to.node.as_str()];
                let target_port = feedback.to.port.as_deref().expect("validated value port");
                let value_type = target_node
                    .inputs
                    .iter()
                    .find(|port| port.name == target_port)
                    .expect("validated input port")
                    .value_type
                    .to_owned();
                states.push(LoopState {
                    name: state.clone(),
                    value_type: (&value_type).into(),
                    target: port_reference(&feedback.to),
                    initial: port_reference(&initial.from),
                    feedback: port_reference(&feedback.from),
                });
            }
            states.sort_by(|left, right| left.name.cmp(&right.name));
            loops.push(LoopTemplate {
                id: region.id.clone(),
                body_order: topological_order(self.flow, Some(&members)),
                max_iterations: *max_iterations,
                until: RouteCondition {
                    gate: until.gate.clone(),
                    outcome: until.outcome.into(),
                },
                on_exhausted: (*on_exhausted).into(),
                states,
            });
        }
        loops.sort_by(|left, right| left.id.cmp(&right.id));
        loops
    }

    fn loop_for(&self, node: &str, paths: &BTreeMap<String, Vec<String>>) -> Option<String> {
        paths[node]
            .iter()
            .rev()
            .find(|id| matches!(self.regions[id.as_str()].kind, RegionKind::Loop { .. }))
            .cloned()
    }

    fn error(&mut self, code: &str, path: String, message: impl Into<String>) {
        self.diagnostics.push(PlanDiagnostic {
            code: code.into(),
            path,
            message: message.into(),
        });
    }
}

fn port_reference(endpoint: &xshell_flow::Endpoint) -> PortReference {
    PortReference {
        task: endpoint.node.clone(),
        port: endpoint.port.clone().expect("validated value endpoint"),
    }
}

fn normalized_capabilities(capabilities: &xshell_flow::Capabilities) -> CapabilitySummary {
    let mut result = CapabilitySummary {
        read: capabilities
            .read
            .iter()
            .map(|path| normalize_workspace_path(path, false).expect("checked path"))
            .collect(),
        write: capabilities
            .write
            .iter()
            .map(|path| normalize_workspace_path(path, false).expect("checked path"))
            .collect(),
        execute: capabilities.execute.clone(),
        network: capabilities.network.clone(),
    };
    result.normalize();
    result
}

fn normalize_workspace_path(path: &str, allow_dot: bool) -> Result<String, String> {
    if path.starts_with('/') {
        return Err(format!("workspace path {path:?} must be relative"));
    }
    if path.contains('\\') {
        return Err(format!("workspace path {path:?} must use '/' separators"));
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" => {
                return Err(format!(
                    "workspace path {path:?} contains an empty component"
                ));
            }
            "." => {}
            ".." => return Err(format!("workspace path {path:?} escapes its workspace")),
            component => components.push(component),
        }
    }
    if components.is_empty() {
        if allow_dot {
            Ok(".".into())
        } else {
            Err(format!("workspace path {path:?} does not select a path"))
        }
    } else {
        Ok(components.join("/"))
    }
}

fn topological_order(flow: &Flow, members: Option<&BTreeSet<&str>>) -> Vec<String> {
    let included = |node: &str| members.is_none_or(|members| members.contains(node));
    let mut degree: BTreeMap<&str, usize> = flow
        .nodes
        .iter()
        .filter(|node| included(&node.id))
        .map(|node| (node.id.as_str(), 0))
        .collect();
    let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &flow.edges {
        if matches!(edge.kind, EdgeKind::Feedback { .. })
            || !included(&edge.from.node)
            || !included(&edge.to.node)
        {
            continue;
        }
        outgoing
            .entry(&edge.from.node)
            .or_default()
            .push(&edge.to.node);
        *degree.entry(&edge.to.node).or_default() += 1;
    }
    for successors in outgoing.values_mut() {
        successors.sort();
    }
    let mut ready: BTreeSet<_> = degree
        .iter()
        .filter_map(|(node, degree)| (*degree == 0).then_some(*node))
        .collect();
    let mut order = Vec::with_capacity(degree.len());
    while let Some(node) = ready.pop_first() {
        order.push(node.to_owned());
        for successor in outgoing.get(node).into_iter().flatten() {
            let degree = degree.get_mut(successor).expect("known successor");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(successor);
            }
        }
    }
    order
}

fn sort_deduplicate(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn collect_predicate_blockers(
    contract: &str,
    expression: &xshell_flow::ContractExpression,
    blockers: &mut Vec<ResolutionBlocker>,
) {
    match expression {
        xshell_flow::ContractExpression::Predicate { name, .. } => {
            blockers.push(ResolutionBlocker::UncheckedPredicate {
                contract: contract.into(),
                predicate: name.clone(),
            });
        }
        xshell_flow::ContractExpression::All { clauses }
        | xshell_flow::ContractExpression::Any { clauses } => {
            for clause in clauses {
                collect_predicate_blockers(contract, clause, blockers);
            }
        }
        xshell_flow::ContractExpression::Not { clause } => {
            collect_predicate_blockers(contract, clause, blockers);
        }
    }
}
