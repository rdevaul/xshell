use crate::{EdgeKind, Flow, GateOutcome, NodeKind, RegionKind};
use std::fmt::Write;

/// Render a deterministic Graphviz DOT projection of a flow.
pub fn to_dot(flow: &Flow) -> String {
    let mut output = String::from("digraph futureshell_flow {\n");
    output.push_str("  graph [compound=true, rankdir=LR];\n");
    output.push_str("  node [fontname=\"Helvetica\"];\n");
    output.push_str("  edge [fontname=\"Helvetica\"];\n");
    let _ = writeln!(output, "  label=\"{}\";", escape(&flow.name));

    let mut nodes: Vec<_> = flow.nodes.iter().collect();
    nodes.sort_by(|left, right| left.id.cmp(&right.id));
    for node in nodes {
        let label = node.label.as_deref().unwrap_or(&node.id);
        let (kind, shape) = match &node.kind {
            NodeKind::Input => ("input", "oval"),
            NodeKind::Output => ("output", "oval"),
            NodeKind::Task { .. } => ("task", "box"),
            NodeKind::Gate { .. } => ("contract", "diamond"),
            NodeKind::Join { .. } => ("join", "invtriangle"),
            NodeKind::Promote { .. } => ("promote", "folder"),
            NodeKind::Discard => ("discard", "octagon"),
        };
        let _ = writeln!(
            output,
            "  \"{}\" [label=\"{}\\n[{}]\", shape={}];",
            escape(&node.id),
            escape(label),
            kind,
            shape
        );
    }

    let mut regions: Vec<_> = flow.regions.iter().collect();
    regions.sort_by(|left, right| left.id.cmp(&right.id));
    for region in regions {
        let kind = match region.kind {
            RegionKind::Scope => "scope",
            RegionKind::Parallel => "parallel",
            RegionKind::Transaction { .. } => "transaction",
            RegionKind::Loop { .. } => "bounded loop",
        };
        let _ = writeln!(output, "  subgraph \"cluster_{}\" {{", escape(&region.id));
        let _ = writeln!(output, "    label=\"{} [{}]\";", escape(&region.id), kind);
        output.push_str("    color=\"#94a3b8\";\n");
        let mut members = region.nodes.clone();
        members.sort();
        for member in members {
            let _ = writeln!(output, "    \"{}\";", escape(&member));
        }
        output.push_str("  }\n");
    }

    let mut edges: Vec<_> = flow.edges.iter().collect();
    edges.sort_by(|left, right| left.id.cmp(&right.id));
    for edge in edges {
        let (label, attributes) = match &edge.kind {
            EdgeKind::Data => (
                port_label(edge.from.port.as_deref(), edge.to.port.as_deref()),
                "color=\"#334155\"",
            ),
            EdgeKind::Dependency => ("after".into(), "style=dashed, color=\"#64748b\""),
            EdgeKind::Route { outcome } => (
                outcome_label(*outcome).into(),
                match outcome {
                    GateOutcome::Valid => "penwidth=2, color=\"#15803d\"",
                    GateOutcome::Invalid => "penwidth=2, color=\"#b91c1c\"",
                    GateOutcome::Error => "penwidth=2, color=\"#c2410c\"",
                    GateOutcome::Cancelled => "penwidth=2, color=\"#64748b\"",
                },
            ),
            EdgeKind::Feedback { state, .. } => (
                format!("feedback: {state}"),
                "style=dotted, penwidth=2, color=\"#2563eb\", constraint=false",
            ),
        };
        let _ = writeln!(
            output,
            "  \"{}\" -> \"{}\" [label=\"{}\", {}];",
            escape(&edge.from.node),
            escape(&edge.to.node),
            escape(&label),
            attributes
        );
    }
    output.push_str("}\n");
    output
}

fn port_label(from: Option<&str>, to: Option<&str>) -> String {
    match (from, to) {
        (Some(from), Some(to)) if from == to => from.into(),
        (Some(from), Some(to)) => format!("{from} -> {to}"),
        _ => String::new(),
    }
}

fn outcome_label(outcome: GateOutcome) -> &'static str {
    match outcome {
        GateOutcome::Valid => "valid",
        GateOutcome::Invalid => "invalid",
        GateOutcome::Error => "error",
        GateOutcome::Cancelled => "cancelled",
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
