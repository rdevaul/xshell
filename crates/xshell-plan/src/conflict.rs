use crate::{PlanDiagnostic, RouteCondition, TaskTemplate};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use xshell_flow::{EdgeKind, Flow};

pub(crate) fn check_write_conflicts(
    flow: &Flow,
    tasks: &[TaskTemplate],
    diagnostics: &mut Vec<PlanDiagnostic>,
) {
    let reachability = Reachability::new(flow);
    for (left_index, left) in tasks.iter().enumerate() {
        for right in tasks.iter().skip(left_index + 1) {
            if left.capabilities.write.is_empty() || right.capabilities.write.is_empty() {
                continue;
            }
            if reachability.ordered(&left.id, &right.id)
                || mutually_exclusive(
                    &left.readiness.activate_on_any,
                    &right.readiness.activate_on_any,
                )
            {
                continue;
            }
            for left_path in &left.capabilities.write {
                for right_path in &right.capabilities.write {
                    if path_specs_overlap(left_path, right_path) {
                        diagnostics.push(PlanDiagnostic {
                            code: "P0401".into(),
                            path: format!("tasks.{}.capabilities.write", left.id),
                            message: format!(
                                "unordered tasks {:?} and {:?} have overlapping write capabilities {:?} and {:?}",
                                left.id, right.id, left_path, right_path
                            ),
                        });
                    }
                }
            }
        }
    }
}

fn mutually_exclusive(left: &[RouteCondition], right: &[RouteCondition]) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && left.iter().all(|left| {
            right
                .iter()
                .all(|right| left.gate == right.gate && left.outcome != right.outcome)
        })
}

fn path_specs_overlap(left: &str, right: &str) -> bool {
    let left_prefix = literal_prefix(left);
    let right_prefix = literal_prefix(right);
    if left_prefix.is_empty() || right_prefix.is_empty() {
        return true;
    }
    component_prefix(&left_prefix, &right_prefix) || component_prefix(&right_prefix, &left_prefix)
}

fn literal_prefix(specification: &str) -> Vec<&str> {
    specification
        .split('/')
        .take_while(|component| !component.contains(['*', '?', '[', ']']))
        .collect()
}

fn component_prefix(prefix: &[&str], path: &[&str]) -> bool {
    prefix.len() <= path.len() && prefix.iter().zip(path).all(|(left, right)| left == right)
}

struct Reachability<'a> {
    outgoing: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> Reachability<'a> {
    fn new(flow: &'a Flow) -> Self {
        let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for edge in &flow.edges {
            if !matches!(edge.kind, EdgeKind::Feedback { .. }) {
                outgoing
                    .entry(&edge.from.node)
                    .or_default()
                    .push(&edge.to.node);
            }
        }
        Self { outgoing }
    }

    fn ordered(&self, left: &str, right: &str) -> bool {
        self.reaches(left, right) || self.reaches(right, left)
    }

    fn reaches(&self, start: &str, target: &str) -> bool {
        let mut queue = VecDeque::from([start]);
        let mut seen = BTreeSet::new();
        while let Some(node) = queue.pop_front() {
            if !seen.insert(node) {
                continue;
            }
            for successor in self.outgoing.get(node).into_iter().flatten() {
                if *successor == target {
                    return true;
                }
                queue.push_back(successor);
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlanGateOutcome;

    #[test]
    fn recognizes_obvious_path_overlap() {
        assert!(path_specs_overlap("generated/**", "generated/model.step"));
        assert!(path_specs_overlap("output", "output/report.json"));
        assert!(path_specs_overlap("**", "anything"));
        assert!(!path_specs_overlap("mesh.msh", "analysis.json"));
        assert!(!path_specs_overlap("left/**", "right/**"));
    }

    #[test]
    fn route_disjunctions_are_exclusive_only_when_every_pair_conflicts() {
        let condition = |gate: &str, outcome| RouteCondition {
            gate: gate.into(),
            outcome,
        };
        assert!(mutually_exclusive(
            &[condition("gate", PlanGateOutcome::Valid)],
            &[condition("gate", PlanGateOutcome::Invalid)]
        ));
        assert!(!mutually_exclusive(
            &[
                condition("gate", PlanGateOutcome::Valid),
                condition("other", PlanGateOutcome::Valid),
            ],
            &[condition("gate", PlanGateOutcome::Invalid)]
        ));
    }
}
