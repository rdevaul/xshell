use std::path::{Path, PathBuf};
use xshell_flow::{Flow, JoinStrategy, NodeKind};
use xshell_plan::{
    PlanArtifact, PlanGateOutcome, ReadinessMode, ResolutionBlocker, TaskOperation, ValueSelection,
};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/futureshell")
}

fn load_flow(name: &str) -> Flow {
    let source = std::fs::read(fixture_root().join("flows").join(name)).unwrap();
    serde_json::from_slice(&source).unwrap()
}

fn lower(name: &str) -> xshell_plan::Plan {
    xshell_plan::lower(&load_flow(name)).unwrap()
}

#[test]
fn golden_plan_artifacts_are_stable_and_readable() {
    for (flow_name, plan_name) in [
        ("linear.json", "linear.plan.json"),
        ("branch.json", "branch.plan.json"),
        ("parallel-join.json", "parallel-join.plan.json"),
        ("bounded-loop.json", "bounded-loop.plan.json"),
    ] {
        let plan = lower(flow_name);
        let actual = serde_json::to_value(plan.artifact().unwrap()).unwrap();
        let expected_source = std::fs::read(fixture_root().join("plans").join(plan_name)).unwrap();
        let expected: serde_json::Value = serde_json::from_slice(&expected_source).unwrap();
        assert_eq!(actual, expected, "plan snapshot changed for {flow_name}");

        let decoded: PlanArtifact = serde_json::from_slice(&expected_source).unwrap();
        assert!(decoded.verify_hash().unwrap());
        assert_eq!(decoded.plan_hash, plan.hash().unwrap().to_string());
        assert_eq!(decoded.plan, plan);
    }
}

#[test]
fn artifact_hash_verification_detects_tampering() {
    let mut artifact = lower("linear.json").artifact().unwrap();
    assert!(artifact.verify_hash().unwrap());
    artifact.plan.tasks[0].resources.timeout_ms = Some(1);
    assert!(!artifact.verify_hash().unwrap());
}

#[test]
fn presentation_and_declaration_order_do_not_change_a_plan() {
    let original = load_flow("linear.json");
    let mut edited = original.clone();
    edited.annotations.insert("ui.zoom".into(), 2.into());
    edited.nodes[0].label = Some("Start here".into());
    edited.nodes.reverse();
    edited.edges.reverse();
    edited.regions.reverse();
    for node in &mut edited.nodes {
        node.inputs.reverse();
        node.outputs.reverse();
    }

    let original = xshell_plan::lower(&original).unwrap();
    let edited = xshell_plan::lower(&edited).unwrap();
    assert_eq!(original, edited);
    assert_eq!(original.hash().unwrap(), edited.hash().unwrap());
}

#[test]
fn semantic_changes_change_the_plan_hash() {
    let original = load_flow("branch.json");
    let original_hash = xshell_plan::lower(&original).unwrap().hash().unwrap();

    let mut resource_change = original.clone();
    resource_change
        .nodes
        .iter_mut()
        .find(|node| node.id == "analyze")
        .unwrap()
        .resources
        .timeout_ms = Some(42);

    let mut capability_change = original.clone();
    capability_change
        .nodes
        .iter_mut()
        .find(|node| node.id == "analyze")
        .unwrap()
        .capabilities
        .network
        .push("solver.example:443".into());

    let mut contract_change = original.clone();
    let xshell_flow::ContractExpression::All { clauses } =
        &mut contract_change.contracts[0].expression
    else {
        panic!("fixture contract changed")
    };
    let xshell_flow::ContractExpression::Predicate { arguments, .. } = &mut clauses[1] else {
        panic!("fixture predicate changed")
    };
    arguments.insert("schema".into(), "fea-result-v2".into());

    for changed in [resource_change, capability_change, contract_change] {
        assert_ne!(
            original_hash,
            xshell_plan::lower(&changed).unwrap().hash().unwrap()
        );
    }
}

#[test]
fn loop_plan_makes_iteration_selection_explicit() {
    let plan = lower("bounded-loop.json");
    let loop_template = &plan.loops[0];
    assert_eq!(loop_template.id, "refine");
    assert_eq!(loop_template.max_iterations, 8);
    assert_eq!(
        loop_template.body_order,
        ["analyze", "acceptable_gate", "repair"]
    );
    assert_eq!(loop_template.states[0].name, "model");

    let analyze = plan.tasks.iter().find(|task| task.id == "analyze").unwrap();
    assert_eq!(analyze.loop_context.as_deref(), Some("refine"));
    assert!(
        analyze.inputs[0]
            .sources
            .iter()
            .any(|source| matches!(&source.selection, ValueSelection::LoopInitial { .. }))
    );
    assert!(
        analyze.inputs[0]
            .sources
            .iter()
            .any(|source| matches!(&source.selection, ValueSelection::PreviousIteration { .. }))
    );

    let finalize = plan
        .tasks
        .iter()
        .find(|task| task.id == "finalize")
        .unwrap();
    assert!(matches!(
        &finalize.inputs[0].sources[0].selection,
        ValueSelection::LoopFinal { .. }
    ));
}

#[test]
fn routes_and_join_readiness_are_explicit() {
    let branch = lower("branch.json");
    let publish = branch
        .tasks
        .iter()
        .find(|task| task.id == "publish")
        .unwrap();
    assert_eq!(publish.readiness.activate_on_any.len(), 1);
    assert_eq!(publish.readiness.activate_on_any[0].gate, "accept");
    assert_eq!(
        publish.readiness.activate_on_any[0].outcome,
        PlanGateOutcome::Valid
    );

    let mut parallel = load_flow("parallel-join.json");
    let combine = parallel
        .nodes
        .iter_mut()
        .find(|node| node.id == "combine")
        .unwrap();
    combine.kind = NodeKind::Join {
        strategy: JoinStrategy::Any,
    };
    let plan = xshell_plan::lower(&parallel).unwrap();
    let combine = plan.tasks.iter().find(|task| task.id == "combine").unwrap();
    assert_eq!(combine.readiness.data.mode, ReadinessMode::Any);
}

#[test]
fn obvious_unordered_write_conflicts_are_rejected() {
    let mut flow = load_flow("parallel-join.json");
    flow.nodes
        .iter_mut()
        .find(|node| node.id == "lint")
        .unwrap()
        .capabilities
        .write
        .push("shared/**".into());
    flow.nodes
        .iter_mut()
        .find(|node| node.id == "test")
        .unwrap()
        .capabilities
        .write
        .push("shared/report.json".into());

    let error = xshell_plan::lower(&flow).unwrap_err();
    assert!(
        error
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "P0401")
    );
}

#[test]
fn mutually_exclusive_branches_may_share_a_write_scope() {
    let mut flow = load_flow("branch.json");
    for id in ["publish", "repair"] {
        flow.nodes
            .iter_mut()
            .find(|node| node.id == id)
            .unwrap()
            .capabilities
            .write
            .push("candidate/**".into());
    }
    xshell_plan::lower(&flow).unwrap();
}

#[test]
fn unsafe_paths_and_zero_resource_bounds_are_rejected() {
    let mut flow = load_flow("linear.json");
    let mesh = flow
        .nodes
        .iter_mut()
        .find(|node| node.id == "mesh")
        .unwrap();
    mesh.capabilities.write = vec!["../outside".into()];
    mesh.resources.timeout_ms = Some(0);
    let error = xshell_plan::lower(&flow).unwrap_err();
    assert!(error.diagnostics().iter().any(|item| item.code == "P0201"));
    assert!(error.diagnostics().iter().any(|item| item.code == "P0204"));
}

#[test]
fn unresolved_execution_dependencies_are_visible_blockers() {
    let plan = lower("branch.json");
    assert!(!plan.resolution.fully_resolved);
    assert!(plan.resolution.blockers.iter().any(|blocker| matches!(
        blocker,
        ResolutionBlocker::UnresolvedProgram { task, .. } if task == "analyze"
    )));
    assert!(plan.resolution.blockers.iter().any(|blocker| matches!(
        blocker,
        ResolutionBlocker::UncheckedPredicate { contract, .. } if contract == "valid_analysis"
    )));
    assert!(plan.tasks.iter().any(|task| matches!(
        &task.operation,
        TaskOperation::Gate { contract } if contract == "valid_analysis"
    )));
}
