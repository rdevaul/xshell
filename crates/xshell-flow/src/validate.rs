use crate::{ContractExpression, EdgeKind, Flow, Node, NodeKind, RegionKind, ValueType};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    diagnostics: Vec<Diagnostic>,
}

impl ValidationError {
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "flow validation failed with {} error(s)",
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

impl std::error::Error for ValidationError {}

struct Validator<'a> {
    flow: &'a Flow,
    diagnostics: Vec<Diagnostic>,
    nodes: BTreeMap<&'a str, &'a Node>,
    regions: BTreeMap<&'a str, &'a crate::Region>,
}

pub fn validate(flow: &Flow) -> Result<(), ValidationError> {
    let mut validator = Validator {
        flow,
        diagnostics: Vec::new(),
        nodes: BTreeMap::new(),
        regions: BTreeMap::new(),
    };
    validator.run();
    if validator.diagnostics.is_empty() {
        Ok(())
    } else {
        Err(ValidationError {
            diagnostics: validator.diagnostics,
        })
    }
}

impl<'a> Validator<'a> {
    fn run(&mut self) {
        if self.flow.schema != crate::FLOW_SCHEMA_V0 {
            self.error(
                "F0001",
                "schema",
                format!(
                    "unsupported schema {:?}; expected {:?}",
                    self.flow.schema,
                    crate::FLOW_SCHEMA_V0
                ),
            );
        }
        self.check_identifier("name", &self.flow.name);
        self.check_interface();
        self.index_contracts();
        self.index_nodes();
        self.index_regions();
        self.check_nodes();
        self.check_regions();
        self.check_edges();
        self.check_required_inputs();
        self.check_region_parent_cycles();
        self.check_execution_cycles();
    }

    fn check_interface(&mut self) {
        self.check_ports("interface.inputs", &self.flow.interface.inputs);
        self.check_ports("interface.outputs", &self.flow.interface.outputs);
    }

    fn index_contracts(&mut self) {
        let mut seen = BTreeSet::new();
        for (index, contract) in self.flow.contracts.iter().enumerate() {
            let path = format!("contracts[{index}]");
            self.check_identifier(&format!("{path}.id"), &contract.id);
            if !seen.insert(contract.id.as_str()) {
                self.error(
                    "F0101",
                    format!("{path}.id"),
                    format!("duplicate contract id {:?}", contract.id),
                );
            }
            self.check_contract_expression(&format!("{path}.expression"), &contract.expression);
        }
    }

    fn index_nodes(&mut self) {
        for (index, node) in self.flow.nodes.iter().enumerate() {
            let path = format!("nodes[{index}].id");
            self.check_identifier(&path, &node.id);
            if self.nodes.insert(&node.id, node).is_some() {
                self.error("F0201", path, format!("duplicate node id {:?}", node.id));
            }
        }
    }

    fn index_regions(&mut self) {
        for (index, region) in self.flow.regions.iter().enumerate() {
            let path = format!("regions[{index}].id");
            self.check_identifier(&path, &region.id);
            if self.regions.insert(&region.id, region).is_some() {
                self.error(
                    "F0301",
                    path,
                    format!("duplicate region id {:?}", region.id),
                );
            }
        }
    }

    fn check_nodes(&mut self) {
        let contracts: BTreeSet<_> = self
            .flow
            .contracts
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        for (index, node) in self.flow.nodes.iter().enumerate() {
            let path = format!("nodes[{index}]");
            self.check_ports(&format!("{path}.inputs"), &node.inputs);
            self.check_ports(&format!("{path}.outputs"), &node.outputs);
            self.check_sorted_unique_strings(
                &format!("{path}.capabilities.read"),
                &node.capabilities.read,
            );
            self.check_sorted_unique_strings(
                &format!("{path}.capabilities.write"),
                &node.capabilities.write,
            );
            self.check_sorted_unique_strings(
                &format!("{path}.capabilities.execute"),
                &node.capabilities.execute,
            );
            self.check_sorted_unique_strings(
                &format!("{path}.capabilities.network"),
                &node.capabilities.network,
            );

            match &node.kind {
                NodeKind::Input => {
                    if !node.inputs.is_empty() {
                        self.error("F0202", &path, "input nodes cannot have input ports");
                    }
                    if !same_ports(&node.outputs, &self.flow.interface.inputs) {
                        self.error(
                            "F0203",
                            &path,
                            "input node outputs must match the flow inputs",
                        );
                    }
                }
                NodeKind::Output => {
                    if !node.outputs.is_empty() {
                        self.error("F0204", &path, "output nodes cannot have output ports");
                    }
                    if !same_ports(&node.inputs, &self.flow.interface.outputs) {
                        self.error(
                            "F0205",
                            &path,
                            "output node inputs must match the flow outputs",
                        );
                    }
                }
                NodeKind::Task { program } => {
                    if program.source.trim().is_empty() {
                        self.error(
                            "F0206",
                            format!("{path}.program.source"),
                            "program source cannot be empty",
                        );
                    }
                }
                NodeKind::Gate { contract } => {
                    if !contracts.contains(contract.as_str()) {
                        self.error(
                            "F0207",
                            format!("{path}.contract"),
                            format!("unknown contract {contract:?}"),
                        );
                    }
                }
                NodeKind::Join { .. } | NodeKind::Promote { .. } | NodeKind::Discard => {}
            }
        }

        let input_count = self
            .flow
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Input))
            .count();
        let output_count = self
            .flow
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Output))
            .count();
        if input_count != 1 {
            self.error(
                "F0208",
                "nodes",
                format!("a flow must have exactly one input node; found {input_count}"),
            );
        }
        if output_count != 1 {
            self.error(
                "F0209",
                "nodes",
                format!("a flow must have exactly one output node; found {output_count}"),
            );
        }
    }

    fn check_regions(&mut self) {
        for (index, region) in self.flow.regions.iter().enumerate() {
            let path = format!("regions[{index}]");
            if let Some(parent) = &region.parent
                && !self.regions.contains_key(parent.as_str())
            {
                self.error(
                    "F0302",
                    format!("{path}.parent"),
                    format!("unknown parent region {parent:?}"),
                );
            }
            let mut members = BTreeSet::new();
            for (member_index, member) in region.nodes.iter().enumerate() {
                if !self.nodes.contains_key(member.as_str()) {
                    self.error(
                        "F0303",
                        format!("{path}.nodes[{member_index}]"),
                        format!("unknown node {member:?}"),
                    );
                }
                if !members.insert(member) {
                    self.error(
                        "F0304",
                        format!("{path}.nodes[{member_index}]"),
                        format!("duplicate region member {member:?}"),
                    );
                }
            }
            if let RegionKind::Loop {
                max_iterations,
                until,
                ..
            } = &region.kind
            {
                if *max_iterations == 0 {
                    self.error(
                        "F0305",
                        format!("{path}.max_iterations"),
                        "loop must allow at least one iteration",
                    );
                }
                match self.nodes.get(until.gate.as_str()) {
                    Some(node) if matches!(node.kind, NodeKind::Gate { .. }) => {
                        if !members.contains(&until.gate) {
                            self.error(
                                "F0306",
                                format!("{path}.until.gate"),
                                "loop exit gate must be a member of the loop region",
                            );
                        }
                    }
                    Some(_) => self.error(
                        "F0307",
                        format!("{path}.until.gate"),
                        "loop exit must reference a gate node",
                    ),
                    None => self.error(
                        "F0308",
                        format!("{path}.until.gate"),
                        format!("unknown gate {:?}", until.gate),
                    ),
                }
            }
        }
    }

    fn check_edges(&mut self) {
        let mut edge_ids = BTreeSet::new();
        for (index, edge) in self.flow.edges.iter().enumerate() {
            let path = format!("edges[{index}]");
            self.check_identifier(&format!("{path}.id"), &edge.id);
            if !edge_ids.insert(edge.id.as_str()) {
                self.error(
                    "F0401",
                    format!("{path}.id"),
                    format!("duplicate edge id {:?}", edge.id),
                );
            }
            let from_node = self.nodes.get(edge.from.node.as_str()).copied();
            let to_node = self.nodes.get(edge.to.node.as_str()).copied();
            if from_node.is_none() {
                self.error(
                    "F0402",
                    format!("{path}.from.node"),
                    format!("unknown node {:?}", edge.from.node),
                );
            }
            if to_node.is_none() {
                self.error(
                    "F0403",
                    format!("{path}.to.node"),
                    format!("unknown node {:?}", edge.to.node),
                );
            }

            match &edge.kind {
                EdgeKind::Data | EdgeKind::Feedback { .. } => {
                    let source_type = from_node.and_then(|node| {
                        self.output_type(node, edge.from.port.as_deref(), &format!("{path}.from"))
                    });
                    let destination_type = to_node.and_then(|node| {
                        self.input_type(node, edge.to.port.as_deref(), &format!("{path}.to"))
                    });
                    if let (Some(source), Some(destination)) = (source_type, destination_type)
                        && source != destination
                    {
                        self.error(
                            "F0404",
                            &path,
                            format!(
                                "port type mismatch: {source:?} cannot flow to {destination:?}"
                            ),
                        );
                    }
                }
                EdgeKind::Dependency => {
                    self.require_portless(edge.from.port.as_deref(), &format!("{path}.from"));
                    self.require_portless(edge.to.port.as_deref(), &format!("{path}.to"));
                }
                EdgeKind::Route { .. } => {
                    self.require_portless(edge.from.port.as_deref(), &format!("{path}.from"));
                    self.require_portless(edge.to.port.as_deref(), &format!("{path}.to"));
                    if let Some(node) = from_node
                        && !matches!(node.kind, NodeKind::Gate { .. })
                    {
                        self.error(
                            "F0405",
                            format!("{path}.from"),
                            "route edges must originate at a gate node",
                        );
                    }
                }
            }

            if let EdgeKind::Feedback { loop_region, state } = &edge.kind {
                if state.trim().is_empty() {
                    self.error(
                        "F0406",
                        format!("{path}.state"),
                        "feedback state name cannot be empty",
                    );
                }
                match self.regions.get(loop_region.as_str()) {
                    Some(region) if matches!(region.kind, RegionKind::Loop { .. }) => {
                        let members: BTreeSet<_> =
                            region.nodes.iter().map(String::as_str).collect();
                        if !members.contains(edge.from.node.as_str())
                            || !members.contains(edge.to.node.as_str())
                        {
                            self.error(
                                "F0407",
                                &path,
                                "feedback endpoints must both belong to its loop region",
                            );
                        }
                    }
                    Some(_) => self.error(
                        "F0408",
                        format!("{path}.loop_region"),
                        "feedback must reference a loop region",
                    ),
                    None => self.error(
                        "F0409",
                        format!("{path}.loop_region"),
                        format!("unknown loop region {loop_region:?}"),
                    ),
                }
            }
        }
    }

    fn check_required_inputs(&mut self) {
        let mut incoming: BTreeMap<(&str, &str), (usize, usize)> = BTreeMap::new();
        for edge in &self.flow.edges {
            if matches!(edge.kind, EdgeKind::Data | EdgeKind::Feedback { .. })
                && let Some(port) = edge.to.port.as_deref()
            {
                let counts = incoming.entry((&edge.to.node, port)).or_default();
                if matches!(edge.kind, EdgeKind::Feedback { .. }) {
                    counts.1 += 1;
                } else {
                    counts.0 += 1;
                }
            }
        }
        for (node_index, node) in self.flow.nodes.iter().enumerate() {
            for (port_index, port) in node.inputs.iter().enumerate() {
                let (ordinary, feedback) = incoming
                    .get(&(node.id.as_str(), port.name.as_str()))
                    .copied()
                    .unwrap_or_default();
                let path = format!("nodes[{node_index}].inputs[{port_index}]");
                if !port.optional && ordinary + feedback == 0 {
                    self.error(
                        "F0410",
                        path,
                        "required input port has no incoming data edge",
                    );
                } else if ordinary > 1 || feedback > 1 || (feedback > 0 && ordinary != 1) {
                    self.error(
                        "F0411",
                        path,
                        "input ports accept one producer, or one initial producer plus one loop feedback producer",
                    );
                }
            }
        }
    }

    fn check_region_parent_cycles(&mut self) {
        for region in &self.flow.regions {
            let mut seen = BTreeSet::new();
            let mut current = Some(region.id.as_str());
            while let Some(id) = current {
                if !seen.insert(id) {
                    self.error(
                        "F0310",
                        format!("regions.{}.parent", region.id),
                        "region parent relationship contains a cycle",
                    );
                    break;
                }
                current = self.regions.get(id).and_then(|item| item.parent.as_deref());
            }
        }
    }

    fn check_execution_cycles(&mut self) {
        let node_ids: BTreeSet<_> = self.nodes.keys().copied().collect();
        let mut degree: BTreeMap<&str, usize> = node_ids.iter().map(|id| (*id, 0)).collect();
        let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for edge in &self.flow.edges {
            if matches!(edge.kind, EdgeKind::Feedback { .. })
                || !node_ids.contains(edge.from.node.as_str())
                || !node_ids.contains(edge.to.node.as_str())
            {
                continue;
            }
            outgoing
                .entry(&edge.from.node)
                .or_default()
                .push(&edge.to.node);
            *degree.entry(&edge.to.node).or_default() += 1;
        }
        let mut queue: VecDeque<_> = degree
            .iter()
            .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
            .collect();
        let mut visited = 0;
        while let Some(node) = queue.pop_front() {
            visited += 1;
            for successor in outgoing.get(node).into_iter().flatten() {
                let successor_degree = degree.get_mut(successor).expect("indexed successor");
                *successor_degree -= 1;
                if *successor_degree == 0 {
                    queue.push_back(successor);
                }
            }
        }
        if visited != node_ids.len() {
            self.error("F0501", "edges", "ordinary data, dependency, and route edges must form a DAG; use bounded loop feedback edges for cycles");
        }
    }

    fn check_contract_expression(&mut self, path: &str, expression: &ContractExpression) {
        match expression {
            ContractExpression::Predicate { name, .. } => {
                if name.trim().is_empty() {
                    self.error(
                        "F0102",
                        format!("{path}.name"),
                        "predicate name cannot be empty",
                    );
                }
            }
            ContractExpression::All { clauses } | ContractExpression::Any { clauses } => {
                if clauses.is_empty() {
                    self.error(
                        "F0103",
                        path,
                        "contract conjunctions and disjunctions cannot be empty",
                    );
                }
                for (index, clause) in clauses.iter().enumerate() {
                    self.check_contract_expression(&format!("{path}.clauses[{index}]"), clause);
                }
            }
            ContractExpression::Not { clause } => {
                self.check_contract_expression(&format!("{path}.clause"), clause)
            }
        }
    }

    fn output_type(&mut self, node: &Node, port: Option<&str>, path: &str) -> Option<ValueType> {
        self.port_type(&node.outputs, port, path, "output")
    }

    fn input_type(&mut self, node: &Node, port: Option<&str>, path: &str) -> Option<ValueType> {
        self.port_type(&node.inputs, port, path, "input")
    }

    fn port_type(
        &mut self,
        ports: &[crate::Port],
        port: Option<&str>,
        path: &str,
        direction: &str,
    ) -> Option<ValueType> {
        let Some(port) = port else {
            self.error(
                "F0412",
                path,
                format!("data and feedback endpoints require a {direction} port"),
            );
            return None;
        };
        match ports.iter().find(|candidate| candidate.name == port) {
            Some(port) => Some(port.value_type.clone()),
            None => {
                self.error("F0413", path, format!("unknown {direction} port {port:?}"));
                None
            }
        }
    }

    fn require_portless(&mut self, port: Option<&str>, path: &str) {
        if port.is_some() {
            self.error(
                "F0414",
                path,
                "dependency and route endpoints cannot name a port",
            );
        }
    }

    fn check_ports(&mut self, path: &str, ports: &[crate::Port]) {
        let mut names = BTreeSet::new();
        for (index, port) in ports.iter().enumerate() {
            self.check_identifier(&format!("{path}[{index}].name"), &port.name);
            if !names.insert(port.name.as_str()) {
                self.error(
                    "F0601",
                    format!("{path}[{index}].name"),
                    format!("duplicate port name {:?}", port.name),
                );
            }
        }
    }

    fn check_sorted_unique_strings(&mut self, path: &str, values: &[String]) {
        let mut seen = BTreeSet::new();
        for (index, value) in values.iter().enumerate() {
            if value.trim().is_empty() {
                self.error(
                    "F0602",
                    format!("{path}[{index}]"),
                    "capability value cannot be empty",
                );
            } else if !seen.insert(value.as_str()) {
                self.error(
                    "F0603",
                    format!("{path}[{index}]"),
                    format!("duplicate capability {value:?}"),
                );
            }
        }
    }

    fn check_identifier(&mut self, path: &str, value: &str) {
        let valid = value.chars().enumerate().all(|(index, character)| {
            if index == 0 {
                character == '_' || character.is_ascii_alphabetic()
            } else {
                character == '_'
                    || character == '-'
                    || character == '.'
                    || character.is_ascii_alphanumeric()
            }
        });
        if value.is_empty() || !valid {
            self.error("F0604", path, format!("invalid identifier {value:?}"));
        }
    }

    fn error(&mut self, code: &'static str, path: impl Into<String>, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            code,
            path: path.into(),
            message: message.into(),
        });
    }
}

fn same_ports(left: &[crate::Port], right: &[crate::Port]) -> bool {
    let normalize = |ports: &[crate::Port]| {
        let mut ports = ports.to_vec();
        ports.sort_by(|left, right| left.name.cmp(&right.name));
        ports
    };
    normalize(left) == normalize(right)
}
