use crate::gc_sweep::GraphRead;

pub struct PlanningSignals {
    pub assignments: Vec<(String, String)>,
    pub closed: Vec<String>,
    pub plan_written: Vec<String>,
}

pub fn signals(graph: &GraphRead, sid: &str, name: &str) -> Option<PlanningSignals> {
    let sid = sid.to_ascii_lowercase();
    let is_planning = graph
        .phases
        .get(&sid)
        .is_some_and(|phases| phases.iter().any(|p| p == "blueprint" || p == "think"))
        || crate::naming::is_blueprint_name(name);
    if !is_planning {
        return None;
    }
    Some(PlanningSignals {
        assignments: graph.index.get(&sid).cloned().unwrap_or_default(),
        closed: graph
            .closed_planning
            .get(&sid)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default(),
        plan_written: graph
            .plan_written
            .get(&sid)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default(),
    })
}

pub fn unfinished(
    assignments: &[(String, String)],
    closed: &[String],
    plan_written: &[String],
    turn_ended: bool,
) -> Option<(String, String)> {
    assignments.iter().find_map(|(node, status)| {
        let moved_on = crate::gc::PLANNING_MOVED_ON_STATUSES.contains(&status.as_str());
        let marked = closed.contains(node) || plan_written.contains(node);
        let complete = crate::gc::PLANNING_COMPLETE_STATUSES.contains(&status.as_str()) && marked;
        if moved_on || complete || turn_ended {
            None
        } else {
            Some((node.clone(), status.clone()))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn non_planning_phase_is_not_a_planning_lane() {
        let graph = GraphRead {
            phases: HashMap::from([(String::from("s1"), vec![String::from("execute")])]),
            ..Default::default()
        };
        assert!(signals(&graph, "s1", "worker").is_none());
    }

    #[test]
    fn closed_assignment_is_finished_but_unmarked_live_assignment_is_not() {
        assert_eq!(
            unfinished(
                &[(String::from("x-a"), String::from("ready"))],
                &[String::from("x-a")],
                &[],
                false,
            ),
            None
        );
        assert_eq!(
            unfinished(
                &[(String::from("x-a"), String::from("ready"))],
                &[],
                &[],
                false,
            ),
            Some((String::from("x-a"), String::from("ready")))
        );
    }
}
