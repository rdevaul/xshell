use std::path::{Path, PathBuf};
use std::process::Command;
use xshell_flow::{ContractExpression, Flow, NodeKind};
use xshell_plan::{
    PlanArtifact, PlanContractExpression, PredicateCatalog, ProgramCatalog, ResolutionBlocker,
    TaskOperation,
};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/futureshell")
}

fn load<T: serde::de::DeserializeOwned>(relative: &str) -> T {
    let source = std::fs::read(fixture_root().join(relative)).unwrap();
    serde_json::from_slice(&source).unwrap()
}

fn branch() -> Flow {
    load("flows/branch.json")
}

fn program_catalog() -> ProgramCatalog {
    load("catalogs/programs.json")
}

fn predicate_catalog() -> PredicateCatalog {
    load("catalogs/predicates.json")
}

fn resolved_branch() -> xshell_plan::Plan {
    let plan = xshell_plan::lower(&branch()).unwrap();
    xshell_plan::resolve(&plan, &program_catalog(), &predicate_catalog()).unwrap()
}

#[test]
fn fixture_catalogs_fully_resolve_without_program_sources() {
    let plan = resolved_branch();
    assert!(plan.resolution.fully_resolved);
    assert!(plan.resolution.blockers.is_empty());

    let analyze = plan.tasks.iter().find(|task| task.id == "analyze").unwrap();
    let TaskOperation::Program {
        resolution: Some(program),
        ..
    } = &analyze.operation
    else {
        panic!("analyze should be resolved")
    };
    assert_eq!(program.entrypoint, "main");
    assert_eq!(
        program.source_sha256,
        program_catalog().programs[0].source_sha256
    );
    assert_eq!(program.manifest_sha256.len(), 64);

    let predicate_resolutions: Vec<_> = plan
        .contracts
        .iter()
        .flat_map(|contract| predicate_resolutions(&contract.expression))
        .collect();
    assert_eq!(predicate_resolutions.len(), 2);
    assert!(predicate_resolutions.iter().all(|hash| hash.len() == 64));
}

#[test]
fn resolved_plan_matches_golden_artifact() {
    let plan = resolved_branch();
    let expected: PlanArtifact = load("plans/branch.resolved.plan.json");
    assert!(expected.verify_hash().unwrap());
    assert_eq!(plan.artifact().unwrap(), expected);
}

#[test]
fn absent_definitions_remain_visible_blockers() {
    let lowered = xshell_plan::lower(&branch()).unwrap();
    let plan = xshell_plan::resolve(
        &lowered,
        &ProgramCatalog::default(),
        &PredicateCatalog::default(),
    )
    .unwrap();
    assert_eq!(plan, lowered);
    assert!(!plan.resolution.fully_resolved);
    assert!(plan.resolution.blockers.iter().any(|blocker| matches!(
        blocker,
        ResolutionBlocker::UnresolvedProgram { task, .. } if task == "analyze"
    )));
    assert!(plan.resolution.blockers.iter().any(|blocker| matches!(
        blocker,
        ResolutionBlocker::UncheckedPredicate { predicate, .. }
            if predicate == "task.succeeded"
    )));
}

#[test]
fn catalog_order_does_not_change_resolution_or_hash() {
    let lowered = xshell_plan::lower(&branch()).unwrap();
    let programs = program_catalog();
    let predicates = predicate_catalog();
    let original = xshell_plan::resolve(&lowered, &programs, &predicates).unwrap();

    let mut reordered_programs = programs;
    reordered_programs.programs.reverse();
    for program in &mut reordered_programs.programs {
        program.entrypoints.reverse();
        for entrypoint in &mut program.entrypoints {
            entrypoint.inputs.reverse();
            entrypoint.outputs.reverse();
        }
    }
    let mut reordered_predicates = predicates;
    reordered_predicates.predicates.reverse();
    let reordered =
        xshell_plan::resolve(&lowered, &reordered_programs, &reordered_predicates).unwrap();

    assert_eq!(original, reordered);
    assert_eq!(original.hash().unwrap(), reordered.hash().unwrap());
}

#[test]
fn pinned_definition_changes_change_the_plan_hash() {
    let lowered = xshell_plan::lower(&branch()).unwrap();
    let programs = program_catalog();
    let predicates = predicate_catalog();
    let original = xshell_plan::resolve(&lowered, &programs, &predicates).unwrap();

    let mut changed_programs = programs.clone();
    changed_programs.programs[0].source_sha256 = "a".repeat(64);
    let program_change = xshell_plan::resolve(&lowered, &changed_programs, &predicates).unwrap();

    let mut changed_predicates = predicates;
    changed_predicates.predicates[0].implementation_sha256 = "b".repeat(64);
    let predicate_change = xshell_plan::resolve(&lowered, &programs, &changed_predicates).unwrap();

    assert_ne!(original.hash().unwrap(), program_change.hash().unwrap());
    assert_ne!(original.hash().unwrap(), predicate_change.hash().unwrap());
}

#[test]
fn incompatible_program_interfaces_and_capabilities_fail_closed() {
    let lowered = xshell_plan::lower(&branch()).unwrap();
    let mut programs = program_catalog();
    let analyze = programs
        .programs
        .iter_mut()
        .find(|program| program.source == "analyze.fsh")
        .unwrap();
    analyze.entrypoints[0].inputs[0].optional = true;
    analyze.entrypoints[0]
        .capabilities
        .execute
        .push("gmsh".into());

    let error = xshell_plan::resolve(&lowered, &programs, &predicate_catalog()).unwrap_err();
    assert!(error.diagnostics().iter().any(|item| item.code == "R0302"));
    assert!(error.diagnostics().iter().any(|item| item.code == "R0304"));
}

#[test]
fn predicate_arguments_are_checked_exactly() {
    let mut flow = branch();
    let ContractExpression::All { clauses } = &mut flow.contracts[0].expression else {
        panic!("fixture contract changed")
    };
    let ContractExpression::Predicate { arguments, .. } = &mut clauses[0] else {
        panic!("fixture predicate changed")
    };
    arguments.insert("task".into(), 7.into());
    arguments.insert("extra".into(), true.into());
    let lowered = xshell_plan::lower(&flow).unwrap();

    let error =
        xshell_plan::resolve(&lowered, &program_catalog(), &predicate_catalog()).unwrap_err();
    assert!(error.diagnostics().iter().any(|item| item.code == "R0401"));
    assert!(error.diagnostics().iter().any(|item| item.code == "R0403"));
}

#[test]
fn explicit_entrypoint_overrides_the_manifest_default() {
    let mut flow = branch();
    let NodeKind::Task { program } = &mut flow
        .nodes
        .iter_mut()
        .find(|node| node.id == "analyze")
        .unwrap()
        .kind
    else {
        panic!("fixture task changed")
    };
    program.entrypoint = Some("review".into());

    let mut programs = program_catalog();
    let analyze = programs
        .programs
        .iter_mut()
        .find(|program| program.source == "analyze.fsh")
        .unwrap();
    let mut review = analyze.entrypoints[0].clone();
    review.name = "review".into();
    analyze.entrypoints.push(review);

    let plan = xshell_plan::resolve(
        &xshell_plan::lower(&flow).unwrap(),
        &programs,
        &predicate_catalog(),
    )
    .unwrap();
    let analyze = plan.tasks.iter().find(|task| task.id == "analyze").unwrap();
    let TaskOperation::Program {
        resolution: Some(resolution),
        ..
    } = &analyze.operation
    else {
        panic!("analyze should be resolved")
    };
    assert_eq!(resolution.entrypoint, "review");
}

#[test]
fn malformed_catalogs_are_rejected_before_resolution() {
    let mut lowered = xshell_plan::lower(&branch()).unwrap();
    lowered.schema = "xshell.plan/v99".into();
    let mut programs = program_catalog();
    programs.programs[0].source_sha256 = "not-a-hash".into();
    programs.programs.push(programs.programs[0].clone());
    let mut predicates = predicate_catalog();
    predicates.schema = "xshell.predicate-catalog/v99".into();

    let error = xshell_plan::resolve(&lowered, &programs, &predicates).unwrap_err();
    assert!(error.diagnostics().iter().any(|item| item.code == "R0000"));
    assert!(error.diagnostics().iter().any(|item| item.code == "R0002"));
    assert!(error.diagnostics().iter().any(|item| item.code == "R0102"));
    assert!(error.diagnostics().iter().any(|item| item.code == "R0103"));
}

#[test]
fn cli_accepts_catalogs_for_build_and_hash() {
    let root = fixture_root();
    let output = Command::new(env!("CARGO_BIN_EXE_xshell-plan"))
        .arg("build")
        .arg(root.join("flows/branch.json"))
        .arg("--program-catalog")
        .arg(root.join("catalogs/programs.json"))
        .arg("--predicate-catalog")
        .arg(root.join("catalogs/predicates.json"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Fully resolved: true"));
    assert!(stdout.contains("Resolved programs: 3/3"));
    assert!(stdout.contains("Resolved predicates: 2/2"));

    let output = Command::new(env!("CARGO_BIN_EXE_xshell-plan"))
        .arg("hash")
        .arg(root.join("flows/branch.json"))
        .arg("--program-catalog")
        .arg(root.join("catalogs/programs.json"))
        .arg("--predicate-catalog")
        .arg(root.join("catalogs/predicates.json"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        resolved_branch().hash().unwrap().to_string()
    );
}

fn predicate_resolutions(expression: &PlanContractExpression) -> Vec<&str> {
    match expression {
        PlanContractExpression::Predicate {
            resolution: Some(resolution),
            ..
        } => vec![resolution.definition_sha256.as_str()],
        PlanContractExpression::Predicate { .. } => Vec::new(),
        PlanContractExpression::All { clauses } | PlanContractExpression::Any { clauses } => {
            clauses.iter().flat_map(predicate_resolutions).collect()
        }
        PlanContractExpression::Not { clause } => predicate_resolutions(clause),
    }
}
