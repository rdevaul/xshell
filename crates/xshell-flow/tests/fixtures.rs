use std::path::{Path, PathBuf};
use xshell_flow::Flow;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/futureshell/flows")
        .join(name)
}

fn load(name: &str) -> Flow {
    let source = std::fs::read(fixture_path(name)).unwrap();
    serde_json::from_slice(&source).unwrap()
}

#[test]
fn golden_flows_parse_and_validate() {
    let fixtures = [
        (
            "linear.json",
            "ade51a0cd97470d74f4b2046c7647b718c630c0f80a5ee372d1137224ab0e107",
        ),
        (
            "branch.json",
            "313be4f9975f4b8b001d9bfc2bca234577db410a405cdea731c9684a03a49244",
        ),
        (
            "parallel-join.json",
            "4daf05c9f44095c3f5d7728ee142db4157b7622039bb6416a30c1fde0f5f2d90",
        ),
        (
            "bounded-loop.json",
            "ad1094134ec0abe746865016a4be7e29e6f61ecdf99df014d1eebcacc985030e",
        ),
    ];
    for (name, expected_hash) in fixtures {
        let flow = load(name);
        if let Err(error) = flow.validate() {
            panic!("{name} was invalid: {error}");
        }
        assert_eq!(flow.semantic_hash().unwrap().to_string(), expected_hash);
    }
}

#[test]
fn semantic_hash_ignores_layout_labels_and_declaration_order() {
    let original = load("linear.json");
    let mut edited = original.clone();
    edited.annotations.insert("ui.theme".into(), "dark".into());
    edited.nodes[0].label = Some("A prettier input label".into());
    edited.nodes[0]
        .annotations
        .insert("ui.position".into(), serde_json::json!([900, 42]));
    edited.nodes.reverse();
    edited.edges.reverse();
    edited.regions[0].nodes.reverse();

    assert_eq!(
        original.semantic_hash().unwrap(),
        edited.semantic_hash().unwrap()
    );
    assert_eq!(
        xshell_flow::canonical_semantic_bytes(&original).unwrap(),
        xshell_flow::canonical_semantic_bytes(&edited).unwrap()
    );
}

#[test]
fn feedback_is_valid_only_in_a_bounded_loop() {
    let mut flow = load("bounded-loop.json");
    flow.regions.clear();
    let error = flow.validate().unwrap_err();
    assert!(
        error
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "F0409")
    );
}

#[test]
fn ordinary_cycles_are_rejected() {
    let mut flow = load("bounded-loop.json");
    let feedback = flow
        .edges
        .iter_mut()
        .find(|edge| edge.id == "revised_model")
        .unwrap();
    feedback.kind = xshell_flow::EdgeKind::Data;
    let error = flow.validate().unwrap_err();
    assert!(
        error
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "F0501")
    );
}

#[test]
fn incompatible_ports_are_rejected() {
    let mut flow = load("linear.json");
    flow.nodes
        .iter_mut()
        .find(|node| node.id == "solve")
        .unwrap()
        .inputs[0]
        .value_type = xshell_flow::ValueType::String;
    let error = flow.validate().unwrap_err();
    assert!(
        error
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "F0404")
    );
}

#[test]
fn dot_projection_shows_contract_routes_and_loop_feedback() {
    let dot = xshell_flow::to_dot(&load("bounded-loop.json"));
    assert!(dot.contains("shape=diamond"));
    assert!(dot.contains("label=\"valid\""));
    assert!(dot.contains("label=\"invalid\""));
    assert!(dot.contains("label=\"feedback: model\""));
    assert!(dot.contains("label=\"refine [bounded loop]\""));
}
