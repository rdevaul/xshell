use crate::{
    CapabilitySummary, Plan, PlanContractExpression, PlanPort, ResolutionBlocker,
    ResolvedPredicate, ResolvedProgram, TaskOperation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const PROGRAM_CATALOG_SCHEMA_V0: &str = "xshell.program-catalog/v0";
pub const PREDICATE_CATALOG_SCHEMA_V0: &str = "xshell.predicate-catalog/v0";

const PROGRAM_MANIFEST_HASH_DOMAIN: &[u8] = b"xshell.program-manifest.semantic.v0\0";
const PREDICATE_DEFINITION_HASH_DOMAIN: &[u8] = b"xshell.predicate-definition.semantic.v0\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProgramCatalog {
    pub schema: String,
    #[serde(default)]
    pub programs: Vec<ProgramManifest>,
}

impl Default for ProgramCatalog {
    fn default() -> Self {
        Self {
            schema: PROGRAM_CATALOG_SCHEMA_V0.into(),
            programs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProgramManifest {
    pub source: String,
    pub source_sha256: String,
    pub default_entrypoint: String,
    pub entrypoints: Vec<ProgramEntrypoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProgramEntrypoint {
    pub name: String,
    #[serde(default)]
    pub inputs: Vec<PlanPort>,
    #[serde(default)]
    pub outputs: Vec<PlanPort>,
    #[serde(default)]
    pub capabilities: CapabilitySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PredicateCatalog {
    pub schema: String,
    #[serde(default)]
    pub predicates: Vec<PredicateDefinition>,
}

impl Default for PredicateCatalog {
    fn default() -> Self {
        Self {
            schema: PREDICATE_CATALOG_SCHEMA_V0.into(),
            predicates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PredicateDefinition {
    pub name: String,
    pub version: String,
    pub implementation_sha256: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, PredicateArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PredicateArgument {
    #[serde(rename = "type")]
    pub value_type: PredicateValueType,
    #[serde(default = "default_true")]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PredicateValueType {
    Bool,
    Integer,
    Number,
    String,
    Array,
    Object,
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionDiagnostic {
    pub code: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionError {
    diagnostics: Vec<ResolutionDiagnostic>,
}

impl ResolutionError {
    pub fn diagnostics(&self) -> &[ResolutionDiagnostic] {
        &self.diagnostics
    }
}

impl fmt::Display for ResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "plan resolution failed with {} error(s)",
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

impl std::error::Error for ResolutionError {}

/// Resolve a lowered plan exclusively from caller-supplied definitions.
///
/// This function performs no filesystem, process, provider, or network access.
/// Missing definitions remain visible as resolution blockers. Definitions that
/// are present but malformed or incompatible produce deterministic diagnostics.
pub fn resolve(
    plan: &Plan,
    programs: &ProgramCatalog,
    predicates: &PredicateCatalog,
) -> Result<Plan, ResolutionError> {
    let mut resolver = Resolver::new(plan, programs, predicates);
    resolver.validate_catalogs();
    if !resolver.diagnostics.is_empty() {
        return Err(resolver.finish_error());
    }
    resolver.resolve_programs();
    resolver.resolve_predicates();
    if !resolver.diagnostics.is_empty() {
        return Err(resolver.finish_error());
    }
    resolver.rebuild_blockers();
    Ok(resolver.plan)
}

struct Resolver {
    plan: Plan,
    programs: ProgramCatalog,
    predicates: PredicateCatalog,
    diagnostics: Vec<ResolutionDiagnostic>,
}

impl Resolver {
    fn new(plan: &Plan, programs: &ProgramCatalog, predicates: &PredicateCatalog) -> Self {
        let mut plan = plan.clone();
        clear_resolutions(&mut plan);
        Self {
            plan,
            programs: programs.clone(),
            predicates: predicates.clone(),
            diagnostics: Vec::new(),
        }
    }

    fn validate_catalogs(&mut self) {
        if self.plan.schema != crate::PLAN_SCHEMA_V0 {
            self.error(
                "R0000",
                "plan.schema",
                format!(
                    "unsupported plan schema {:?}; expected {:?}",
                    self.plan.schema,
                    crate::PLAN_SCHEMA_V0
                ),
            );
        }
        if self.programs.schema != PROGRAM_CATALOG_SCHEMA_V0 {
            self.error(
                "R0001",
                "program_catalog.schema",
                format!(
                    "unsupported program catalog schema {:?}; expected {PROGRAM_CATALOG_SCHEMA_V0:?}",
                    self.programs.schema
                ),
            );
        }
        if self.predicates.schema != PREDICATE_CATALOG_SCHEMA_V0 {
            self.error(
                "R0002",
                "predicate_catalog.schema",
                format!(
                    "unsupported predicate catalog schema {:?}; expected {PREDICATE_CATALOG_SCHEMA_V0:?}",
                    self.predicates.schema
                ),
            );
        }
        self.validate_programs();
        self.validate_predicates();
    }

    fn validate_programs(&mut self) {
        let mut sources = BTreeSet::new();
        for program_index in 0..self.programs.programs.len() {
            let path = format!("program_catalog.programs[{program_index}]");
            let source = self.programs.programs[program_index].source.clone();
            if source.trim().is_empty() {
                self.error("R0101", format!("{path}.source"), "source cannot be empty");
            } else if !sources.insert(source.clone()) {
                self.error(
                    "R0102",
                    format!("{path}.source"),
                    format!("duplicate program source {source:?}"),
                );
            }
            let source_sha256 = self.programs.programs[program_index].source_sha256.clone();
            if !is_sha256(&source_sha256) {
                self.error(
                    "R0103",
                    format!("{path}.source_sha256"),
                    "source_sha256 must be 64 lowercase hexadecimal characters",
                );
            }

            let default_entrypoint = self.programs.programs[program_index]
                .default_entrypoint
                .clone();
            let mut names = BTreeSet::new();
            let entrypoint_count = self.programs.programs[program_index].entrypoints.len();
            if entrypoint_count == 0 {
                self.error(
                    "R0104",
                    format!("{path}.entrypoints"),
                    "a program manifest requires at least one entrypoint",
                );
            }
            for entrypoint_index in 0..entrypoint_count {
                let entrypoint_path = format!("{path}.entrypoints[{entrypoint_index}]");
                let name = self.programs.programs[program_index].entrypoints[entrypoint_index]
                    .name
                    .clone();
                if name.trim().is_empty() {
                    self.error(
                        "R0105",
                        format!("{entrypoint_path}.name"),
                        "entrypoint name cannot be empty",
                    );
                } else if !names.insert(name.clone()) {
                    self.error(
                        "R0106",
                        format!("{entrypoint_path}.name"),
                        format!("duplicate entrypoint {name:?}"),
                    );
                }
                self.validate_ports(program_index, entrypoint_index, true, &entrypoint_path);
                self.validate_ports(program_index, entrypoint_index, false, &entrypoint_path);
                self.validate_capabilities(program_index, entrypoint_index, &entrypoint_path);
            }
            if !names.contains(default_entrypoint.as_str()) {
                self.error(
                    "R0107",
                    format!("{path}.default_entrypoint"),
                    format!("default entrypoint {default_entrypoint:?} is not declared"),
                );
            }
            self.programs.programs[program_index]
                .entrypoints
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        self.programs
            .programs
            .sort_by(|left, right| left.source.cmp(&right.source));
    }

    fn validate_ports(
        &mut self,
        program_index: usize,
        entrypoint_index: usize,
        inputs: bool,
        entrypoint_path: &str,
    ) {
        let family = if inputs { "inputs" } else { "outputs" };
        let ports = if inputs {
            &mut self.programs.programs[program_index].entrypoints[entrypoint_index].inputs
        } else {
            &mut self.programs.programs[program_index].entrypoints[entrypoint_index].outputs
        };
        let mut names = BTreeSet::new();
        let mut errors = Vec::new();
        for (index, port) in ports.iter().enumerate() {
            if port.name.trim().is_empty() {
                errors.push((
                    "R0110",
                    format!("{entrypoint_path}.{family}[{index}].name"),
                    "port name cannot be empty".to_owned(),
                ));
            } else if !names.insert(port.name.clone()) {
                errors.push((
                    "R0111",
                    format!("{entrypoint_path}.{family}[{index}].name"),
                    format!("duplicate {family} port {:?}", port.name),
                ));
            }
        }
        ports.sort_by(|left, right| left.name.cmp(&right.name));
        for (code, path, message) in errors {
            self.error(code, path, message);
        }
    }

    fn validate_capabilities(
        &mut self,
        program_index: usize,
        entrypoint_index: usize,
        entrypoint_path: &str,
    ) {
        let capabilities =
            &mut self.programs.programs[program_index].entrypoints[entrypoint_index].capabilities;
        let mut errors = Vec::new();
        for (family, paths) in [
            ("read", &mut capabilities.read),
            ("write", &mut capabilities.write),
        ] {
            for (index, path) in paths.iter_mut().enumerate() {
                match crate::lower::normalize_workspace_path(path, false) {
                    Ok(normalized) => *path = normalized,
                    Err(message) => errors.push((
                        "R0120",
                        format!("{entrypoint_path}.capabilities.{family}[{index}]"),
                        message,
                    )),
                }
            }
        }
        for (family, values) in [
            ("execute", &capabilities.execute),
            ("network", &capabilities.network),
        ] {
            for (index, value) in values.iter().enumerate() {
                if value.trim().is_empty() {
                    errors.push((
                        "R0121",
                        format!("{entrypoint_path}.capabilities.{family}[{index}]"),
                        format!("{family} capability cannot be empty"),
                    ));
                }
            }
        }
        capabilities.normalize();
        for (code, path, message) in errors {
            self.error(code, path, message);
        }
    }

    fn validate_predicates(&mut self) {
        let mut names = BTreeSet::new();
        for (index, predicate) in self.predicates.predicates.iter().enumerate() {
            let path = format!("predicate_catalog.predicates[{index}]");
            if predicate.name.trim().is_empty() {
                self.diagnostics.push(ResolutionDiagnostic {
                    code: "R0201".into(),
                    path: format!("{path}.name"),
                    message: "predicate name cannot be empty".into(),
                });
            } else if !names.insert(predicate.name.as_str()) {
                self.diagnostics.push(ResolutionDiagnostic {
                    code: "R0202".into(),
                    path: format!("{path}.name"),
                    message: format!("duplicate predicate {:?}", predicate.name),
                });
            }
            if predicate.version.trim().is_empty() {
                self.diagnostics.push(ResolutionDiagnostic {
                    code: "R0203".into(),
                    path: format!("{path}.version"),
                    message: "predicate version cannot be empty".into(),
                });
            }
            if !is_sha256(&predicate.implementation_sha256) {
                self.diagnostics.push(ResolutionDiagnostic {
                    code: "R0204".into(),
                    path: format!("{path}.implementation_sha256"),
                    message: "implementation_sha256 must be 64 lowercase hexadecimal characters"
                        .into(),
                });
            }
            for name in predicate.arguments.keys() {
                if name.trim().is_empty() {
                    self.diagnostics.push(ResolutionDiagnostic {
                        code: "R0205".into(),
                        path: format!("{path}.arguments"),
                        message: "predicate argument name cannot be empty".into(),
                    });
                }
            }
        }
        self.predicates
            .predicates
            .sort_by(|left, right| left.name.cmp(&right.name));
    }

    fn resolve_programs(&mut self) {
        let manifests: BTreeMap<_, _> = self
            .programs
            .programs
            .iter()
            .map(|manifest| (manifest.source.clone(), manifest.clone()))
            .collect();
        for task_index in 0..self.plan.tasks.len() {
            let (source, requested_entrypoint) = match &self.plan.tasks[task_index].operation {
                TaskOperation::Program {
                    source, entrypoint, ..
                } => (source.clone(), entrypoint.clone()),
                _ => continue,
            };
            let Some(manifest) = manifests.get(&source) else {
                continue;
            };
            let entrypoint_name = requested_entrypoint
                .as_deref()
                .unwrap_or(&manifest.default_entrypoint);
            let Some(entrypoint) = manifest
                .entrypoints
                .iter()
                .find(|candidate| candidate.name == entrypoint_name)
            else {
                self.error(
                    "R0301",
                    format!(
                        "tasks.{}.operation.entrypoint",
                        self.plan.tasks[task_index].id
                    ),
                    format!("program {source:?} does not declare entrypoint {entrypoint_name:?}"),
                );
                continue;
            };
            let inputs: Vec<_> = self.plan.tasks[task_index]
                .inputs
                .iter()
                .map(|binding| binding.port.clone())
                .collect();
            let task_id = self.plan.tasks[task_index].id.clone();
            let before = self.diagnostics.len();
            if inputs != entrypoint.inputs {
                self.error(
                    "R0302",
                    format!("tasks.{task_id}.inputs"),
                    format!(
                        "task inputs do not match program {source:?} entrypoint {entrypoint_name:?}"
                    ),
                );
            }
            if self.plan.tasks[task_index].outputs != entrypoint.outputs {
                self.error(
                    "R0303",
                    format!("tasks.{task_id}.outputs"),
                    format!(
                        "task outputs do not match program {source:?} entrypoint {entrypoint_name:?}"
                    ),
                );
            }
            let declared_capabilities = self.plan.tasks[task_index].capabilities.clone();
            for (family, required, declared) in
                capability_families(&entrypoint.capabilities, &declared_capabilities)
            {
                for capability in required {
                    if !declared.contains(capability) {
                        self.error(
                            "R0304",
                            format!("tasks.{task_id}.capabilities.{family}"),
                            format!(
                                "program {source:?} requires undeclared {family} capability {capability:?}"
                            ),
                        );
                    }
                }
            }
            if self.diagnostics.len() == before {
                let resolution = ResolvedProgram {
                    manifest_sha256: semantic_hash(PROGRAM_MANIFEST_HASH_DOMAIN, manifest),
                    source_sha256: manifest.source_sha256.clone(),
                    entrypoint: entrypoint_name.into(),
                };
                let TaskOperation::Program {
                    resolution: target, ..
                } = &mut self.plan.tasks[task_index].operation
                else {
                    unreachable!();
                };
                *target = Some(resolution);
            }
        }
    }

    fn resolve_predicates(&mut self) {
        let definitions: BTreeMap<_, _> = self
            .predicates
            .predicates
            .iter()
            .map(|definition| (definition.name.clone(), definition.clone()))
            .collect();
        let mut contracts = std::mem::take(&mut self.plan.contracts);
        for contract in &mut contracts {
            resolve_expression(
                &format!("contracts.{}.expression", contract.id),
                &mut contract.expression,
                &definitions,
                &mut self.diagnostics,
            );
        }
        self.plan.contracts = contracts;
    }

    fn rebuild_blockers(&mut self) {
        let mut blockers = Vec::new();
        for task in &self.plan.tasks {
            if let TaskOperation::Program {
                source,
                resolution: None,
                ..
            } = &task.operation
            {
                blockers.push(ResolutionBlocker::UnresolvedProgram {
                    task: task.id.clone(),
                    source: source.clone(),
                });
            }
        }
        for contract in &self.plan.contracts {
            collect_unresolved_predicates(&contract.id, &contract.expression, &mut blockers);
        }
        blockers.sort();
        blockers.dedup();
        self.plan.resolution.fully_resolved = blockers.is_empty();
        self.plan.resolution.blockers = blockers;
    }

    fn error(&mut self, code: &str, path: impl Into<String>, message: impl Into<String>) {
        self.diagnostics.push(ResolutionDiagnostic {
            code: code.into(),
            path: path.into(),
            message: message.into(),
        });
    }

    fn finish_error(self) -> ResolutionError {
        ResolutionError {
            diagnostics: self.diagnostics,
        }
    }
}

fn resolve_expression(
    path: &str,
    expression: &mut PlanContractExpression,
    definitions: &BTreeMap<String, PredicateDefinition>,
    diagnostics: &mut Vec<ResolutionDiagnostic>,
) {
    match expression {
        PlanContractExpression::Predicate {
            name,
            arguments,
            resolution,
        } => {
            let Some(definition) = definitions.get(name) else {
                return;
            };
            let before = diagnostics.len();
            for (argument, specification) in &definition.arguments {
                match arguments.get(argument) {
                    Some(value) if !specification.value_type.matches(value) => {
                        diagnostics.push(ResolutionDiagnostic {
                            code: "R0401".into(),
                            path: format!("{path}.arguments.{argument}"),
                            message: format!(
                                "predicate {name:?} argument {argument:?} must be {}",
                                specification.value_type
                            ),
                        });
                    }
                    None if specification.required => {
                        diagnostics.push(ResolutionDiagnostic {
                            code: "R0402".into(),
                            path: format!("{path}.arguments"),
                            message: format!("predicate {name:?} requires argument {argument:?}"),
                        });
                    }
                    _ => {}
                }
            }
            for argument in arguments.keys() {
                if !definition.arguments.contains_key(argument) {
                    diagnostics.push(ResolutionDiagnostic {
                        code: "R0403".into(),
                        path: format!("{path}.arguments.{argument}"),
                        message: format!(
                            "predicate {name:?} does not declare argument {argument:?}"
                        ),
                    });
                }
            }
            if diagnostics.len() == before {
                *resolution = Some(ResolvedPredicate {
                    definition_sha256: semantic_hash(PREDICATE_DEFINITION_HASH_DOMAIN, definition),
                    version: definition.version.clone(),
                    implementation_sha256: definition.implementation_sha256.clone(),
                });
            }
        }
        PlanContractExpression::All { clauses } | PlanContractExpression::Any { clauses } => {
            for (index, clause) in clauses.iter_mut().enumerate() {
                resolve_expression(
                    &format!("{path}.clauses[{index}]"),
                    clause,
                    definitions,
                    diagnostics,
                );
            }
        }
        PlanContractExpression::Not { clause } => {
            resolve_expression(&format!("{path}.clause"), clause, definitions, diagnostics)
        }
    }
}

fn clear_resolutions(plan: &mut Plan) {
    for task in &mut plan.tasks {
        if let TaskOperation::Program { resolution, .. } = &mut task.operation {
            *resolution = None;
        }
    }
    for contract in &mut plan.contracts {
        clear_expression_resolution(&mut contract.expression);
    }
}

fn clear_expression_resolution(expression: &mut PlanContractExpression) {
    match expression {
        PlanContractExpression::Predicate { resolution, .. } => *resolution = None,
        PlanContractExpression::All { clauses } | PlanContractExpression::Any { clauses } => {
            for clause in clauses {
                clear_expression_resolution(clause);
            }
        }
        PlanContractExpression::Not { clause } => clear_expression_resolution(clause),
    }
}

fn collect_unresolved_predicates(
    contract: &str,
    expression: &PlanContractExpression,
    blockers: &mut Vec<ResolutionBlocker>,
) {
    match expression {
        PlanContractExpression::Predicate {
            name,
            resolution: None,
            ..
        } => blockers.push(ResolutionBlocker::UncheckedPredicate {
            contract: contract.into(),
            predicate: name.clone(),
        }),
        PlanContractExpression::Predicate { .. } => {}
        PlanContractExpression::All { clauses } | PlanContractExpression::Any { clauses } => {
            for clause in clauses {
                collect_unresolved_predicates(contract, clause, blockers);
            }
        }
        PlanContractExpression::Not { clause } => {
            collect_unresolved_predicates(contract, clause, blockers)
        }
    }
}

fn capability_families<'a>(
    required: &'a CapabilitySummary,
    declared: &'a CapabilitySummary,
) -> [(&'static str, &'a Vec<String>, &'a Vec<String>); 4] {
    [
        ("read", &required.read, &declared.read),
        ("write", &required.write, &declared.write),
        ("execute", &required.execute, &declared.execute),
        ("network", &required.network, &declared.network),
    ]
}

impl PredicateValueType {
    fn matches(self, value: &Value) -> bool {
        match self {
            Self::Bool => value.is_boolean(),
            Self::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            Self::Number => value.is_number(),
            Self::String => value.is_string(),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
            Self::Null => value.is_null(),
        }
    }
}

impl fmt::Display for PredicateValueType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bool => "a boolean",
            Self::Integer => "an integer",
            Self::Number => "a number",
            Self::String => "a string",
            Self::Array => "an array",
            Self::Object => "an object",
            Self::Null => "null",
        })
    }
}

fn semantic_hash<T: Serialize>(domain: &[u8], value: &T) -> String {
    let bytes = serde_json::to_vec(value).expect("validated catalog definition can be encoded");
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn default_true() -> bool {
    true
}
