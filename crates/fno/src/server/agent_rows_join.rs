//! Which registry row a pane binds to at render time.
//!
//! Pane ids recycle across server restarts (pty.rs), so two registry rows
//! can record one `(session, pane_id)`. First-match bound a LIVE worker
//! behind an exited row that recycled onto the id first (observed live
//! 2026-08-12 on pane 26: an exited codex row at a lower index won the
//! bind, and the live claude worker rendered in the watch-only appendix
//! marked exited). The bind is a total order - live mux, live attach, any
//! mux, any attach - so a live row of EITHER kind outranks an exited one,
//! and the caller's watch-only appendix still renders the loser. Two rows
//! both live (or both exited) within one kind stay first-match: settling
//! those needs a generation token on the mux ref, a schema bump.

use std::collections::HashMap;

use crate::agents_view::RegistryAgent;

/// Position in `agents` of the row that hosts `pid` in `session_name`.
pub(crate) fn bind_agent_to_pane(
    agents: &[RegistryAgent],
    session_name: &str,
    pid: u64,
    attached: &HashMap<String, u64>,
    worker_pane_of: &dyn Fn(&RegistryAgent) -> Option<u64>,
) -> Option<usize> {
    let mux_match = |a: &RegistryAgent| matches!(&a.mux, Some((sess, pane)) if sess == session_name && *pane == pid);
    let attach_match = |a: &RegistryAgent| {
        a.mux.is_none()
            && (a
                .attach_id
                .as_deref()
                .and_then(|id| attached.get(id))
                .copied()
                == Some(pid)
                || worker_pane_of(a) == Some(pid))
    };
    agents
        .iter()
        .position(|a| mux_match(a) && !a.exited)
        .or_else(|| agents.iter().position(|a| attach_match(a) && !a.exited))
        .or_else(|| agents.iter().position(mux_match))
        .or_else(|| agents.iter().position(attach_match))
}

#[cfg(test)]
mod tests {
    use super::bind_agent_to_pane;
    use crate::agents_view::RegistryAgent;

    use std::collections::HashMap;

    fn agent(name: &str, mux: Option<(&str, u64)>, exited: bool) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            mux: mux.map(|(s, p)| (s.to_string(), p)),
            exited,
            ..Default::default()
        }
    }

    #[test]
    fn a_live_row_binds_the_pane_over_an_exited_row_on_one_recycled_id() {
        let agents = vec![
            agent("exited-older", Some(("main", 7)), true),
            agent("live-worker", Some(("main", 7)), false),
        ];
        let bound = bind_agent_to_pane(&agents, "main", 7, &HashMap::new(), &|_| None);
        assert_eq!(bound, Some(1), "the LIVE row binds the pane");
    }

    #[test]
    fn a_live_attach_row_binds_over_an_exited_mux_row() {
        let agents = vec![
            agent("exited-older", Some(("main", 7)), true),
            agent("bg-mapped", None, false),
        ];
        let mut attached = HashMap::new();
        attached.insert("a1".to_string(), 7u64);
        let bound = bind_agent_to_pane(&agents, "main", 7, &attached, &|a: &RegistryAgent| {
            if a.name == "bg-mapped" {
                Some(7)
            } else {
                None
            }
        });
        assert_eq!(bound, Some(1), "the live attach row binds the pane");
    }

    #[test]
    fn no_match_answers_none() {
        let agents = vec![agent("elsewhere", Some(("other", 7)), false)];
        let bound = bind_agent_to_pane(&agents, "main", 7, &HashMap::new(), &|_| None);
        assert_eq!(bound, None);
    }
}
