//! Which workspace a thread belongs to.
//!
//! One resolver, three rungs, in order: the recorded member, then the
//! spawner's workspace (the row's `spawned_by_session` edge followed to the
//! parent registry row, keyed by harness session id), then the project
//! default (`find_by_cwd`, keyed by canonical repo root). A thread's cwd is
//! its own property and decides membership only on the last rung, so a
//! thread spawned into another repo still shows under the workspace it was
//! spawned from.

use super::*;

/// How many spawner hops the lineage walk tolerates before it gives up and
/// lets the project default answer. Bounds a corrupted edge cycle.
const HOP_CAP: u32 = 8;

impl Core {
    /// The workspace of a thread row, asked by every thread-row site
    /// (sideline attribution, portal and attach owner routing, reseat graft,
    /// resume placement) so the row renders where the next gesture places it.
    pub(crate) fn thread_workspace(&self, a: &RegistryAgent) -> Option<u64> {
        self.workspace_at(a, 0)
    }

    fn workspace_at(&self, a: &RegistryAgent, depth: u32) -> Option<u64> {
        self.member_squad_for_agent(a)
            .or_else(|| self.spawner_workspace(a, depth))
            .or_else(|| self.session.find_by_cwd(&a.cwd))
    }

    /// The spawner rung: the row's `spawned_by_session` edge joined to the
    /// parent registry row(s) by harness session id (trimmed, case-insensitive
    /// - the same tolerance `spawned_by_name` applies), skipping a row that
    /// carries the child's own name. A parent pane-hosted on this server
    /// answers where its pane lives NOW (`find_pane`), so the row lands where
    /// ResumeAgent would put it; any other parent answers with its own full
    /// chain one hop deeper. Parents resolving to DIFFERENT workspaces read
    /// as absent: an ambiguous edge is never a confident wrong answer (the
    /// `spawned_by_name` rule).
    fn spawner_workspace(&self, a: &RegistryAgent, depth: u32) -> Option<u64> {
        if depth >= HOP_CAP {
            return None;
        }
        let edge = a.spawned_by_session.as_deref().map(str::trim).unwrap_or("");
        if edge.is_empty() {
            return None;
        }
        let mut squads = self
            .agents
            .iter()
            .filter(|p| p.name != a.name)
            .filter(|p| {
                agent_harness_session_id(p).is_some_and(|sid| sid.trim().eq_ignore_ascii_case(edge))
            })
            .filter_map(|p| match &p.mux {
                Some((sess, pane)) if sess == &self.session_name => self
                    .session
                    .find_pane(*pane)
                    .map(|(sid, _)| sid)
                    .or_else(|| self.workspace_at(p, depth + 1)),
                _ => self.workspace_at(p, depth + 1),
            });
        let first = squads.next()?;
        if squads.any(|s| s != first) {
            return None;
        }
        Some(first)
    }

    fn member_squad_for_agent(&self, agent: &RegistryAgent) -> Option<u64> {
        let exact: Vec<u64> = match (agent.harness.as_deref(), agent_harness_session_id(agent)) {
            (Some(harness), Some(session_id)) => self
                .squad_members
                .iter()
                .filter_map(|(sid, members)| {
                    members
                        .iter()
                        .any(|member| {
                            member.worker.as_deref() == Some(agent.name.as_str())
                                && member.harness.as_deref() == Some(harness)
                                && member.harness_session_id.as_deref() == Some(session_id)
                        })
                        .then_some(*sid)
                })
                .collect(),
            _ => Vec::new(),
        };
        if exact.len() == 1 {
            return exact.first().copied();
        }
        if exact.len() > 1 {
            return None;
        }
        // An AttachAgent persist stores only the attach handle (no worker
        // name), so the name rungs never match it; match the row's own
        // handle, so an explicit placement survives the pane closing.
        let attached: Vec<u64> = match agent.attach_id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => self
                .squad_members
                .iter()
                .filter_map(|(sid, members)| {
                    members
                        .iter()
                        .any(|member| member.attach_id == id)
                        .then_some(*sid)
                })
                .collect(),
            None => Vec::new(),
        };
        if attached.len() == 1 {
            return attached.first().copied();
        }
        if attached.len() > 1 {
            return None;
        }
        let legacy: Vec<u64> = self
            .squad_members
            .iter()
            .filter_map(|(sid, members)| {
                members
                    .iter()
                    .any(|member| {
                        member.worker.as_deref() == Some(agent.name.as_str())
                            && member.harness.is_none()
                            && member.harness_session_id.is_none()
                    })
                    .then_some(*sid)
            })
            .collect();
        (legacy.len() == 1).then(|| legacy[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::empty_core;
    use crate::squad_store::StoredMember;

    fn row(
        name: &str,
        cwd: &str,
        harness: Option<&str>,
        sid: Option<&str>,
        spawned_by: Option<&str>,
    ) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            cwd: cwd.into(),
            harness: harness.map(Into::into),
            harness_session_id: sid.map(Into::into),
            spawned_by_session: spawned_by.map(Into::into),
            ..Default::default()
        }
    }

    fn squad_at(core: &mut Core, id: u64, origin: &str, pane: u64) {
        core.session.add_squad(
            id,
            vec![origin.into()],
            None,
            Tab {
                name: None,
                id,
                root: Node::Leaf(pane),
                focus: pane,
            },
        );
    }

    fn member(worker: Option<&str>, attach_id: &str, sid: Option<&str>) -> StoredMember {
        StoredMember {
            attach_id: attach_id.into(),
            worker: worker.map(Into::into),
            harness: None,
            harness_session_id: sid.map(Into::into),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            pane_id: None,
        }
    }

    #[test]
    fn a_pane_hosted_parents_workspace_outranks_the_childs_cwd() {
        // AC1-HP: the parent's pane lives in workspace 1; the child's own
        // cwd belongs to workspace 3. The spawner rung answers 1.
        let mut core = empty_core();
        squad_at(&mut core, 1, "/footnote", 5);
        squad_at(&mut core, 3, "/other", 6);
        let parent = row(
            "lead",
            "/footnote",
            Some("claude"),
            Some("sid-parent"),
            None,
        );
        let mut child = row(
            "probe",
            "/other",
            Some("codex"),
            Some("sid-child"),
            Some("SID-PARENT"),
        );
        child.mux = None;
        core.agents = vec![parent, child];
        assert_eq!(core.thread_workspace(&core.agents[1].clone()), Some(1));
    }

    #[test]
    fn a_recorded_member_outranks_lineage_and_cwd() {
        // AC2-HP: member in workspace 2, parent in workspace 1, cwd in
        // workspace 3. The recorded member wins.
        let mut core = empty_core();
        squad_at(&mut core, 1, "/footnote", 5);
        squad_at(&mut core, 2, "/two", 7);
        squad_at(&mut core, 3, "/other", 6);
        core.agents = vec![
            row(
                "lead",
                "/footnote",
                Some("claude"),
                Some("sid-parent"),
                None,
            ),
            row(
                "probe",
                "/other",
                Some("codex"),
                Some("sid-child"),
                Some("sid-parent"),
            ),
        ];
        let mut stored = member(Some("probe"), "", Some("sid-child"));
        stored.harness = Some("codex".into());
        core.squad_members.insert(2, vec![stored]);
        assert_eq!(core.thread_workspace(&core.agents[1].clone()), Some(2));
    }

    #[test]
    fn an_orphan_row_falls_to_the_project_default() {
        // AC3-HP: no member, no parent row in the registry, cwd owned by
        // workspace 3. The project default answers.
        let mut core = empty_core();
        squad_at(&mut core, 3, "/other", 6);
        core.agents = vec![row(
            "probe",
            "/other",
            Some("codex"),
            Some("sid-child"),
            Some("sid-ghost"),
        )];
        assert_eq!(core.thread_workspace(&core.agents[0].clone()), Some(3));
    }

    #[test]
    fn an_edge_cycle_ends_at_the_hop_cap_and_falls_to_cwd() {
        // AC4-EDGE: two rows name each other as spawner. The walk ends
        // within the cap; each row falls to its own project default.
        let mut core = empty_core();
        squad_at(&mut core, 3, "/other", 6);
        core.agents = vec![
            row("a", "/other", Some("codex"), Some("sid-a"), Some("sid-b")),
            row("b", "/other", Some("codex"), Some("sid-b"), Some("sid-a")),
        ];
        assert_eq!(core.thread_workspace(&core.agents[0].clone()), Some(3));
        assert_eq!(core.thread_workspace(&core.agents[1].clone()), Some(3));
    }

    #[test]
    fn an_attach_handle_member_anchors_a_paneless_row() {
        // AC5-HP: the stored member carries only the attach handle; the
        // paneless row carries the same handle. The placement holds.
        let mut core = empty_core();
        squad_at(&mut core, 2, "/two", 7);
        let mut probe = row("probe", "/other", None, None, None);
        probe.attach_id = Some("J".into());
        core.agents = vec![probe];
        core.squad_members.insert(2, vec![member(None, "J", None)]);
        assert_eq!(core.thread_workspace(&core.agents[0].clone()), Some(2));
    }

    #[test]
    fn a_legacy_row_without_an_edge_still_groups_by_cwd() {
        // AC8-EDGE: a paneless row with no spawned_by_session whose cwd a
        // workspace owns keeps grouping there - no regression.
        let mut core = empty_core();
        squad_at(&mut core, 1, "/footnote", 5);
        core.agents = vec![row(
            "old",
            "/footnote/sub/dir",
            Some("codex"),
            Some("sid-old"),
            None,
        )];
        assert_eq!(core.thread_workspace(&core.agents[0].clone()), Some(1));
    }

    #[test]
    fn agent_rows_groups_a_foreign_cwd_thread_under_its_spawners_workspace() {
        // AC6-HP, the think-harness-ranking shape: a live paneless codex
        // thread in a repo no workspace owns, its edge naming a claude
        // session pane-hosted in workspace 1. The built row carries 1.
        let mut core = empty_core();
        squad_at(&mut core, 1, "/footnote", 5);
        core.agents = vec![
            row(
                "lead",
                "/footnote",
                Some("claude"),
                Some("sid-parent"),
                None,
            ),
            row(
                "probe",
                "/tools/foreign",
                Some("codex"),
                Some("sid-child"),
                Some("sid-parent"),
            ),
        ];
        let mut lead = core.agents[0].clone();
        lead.mux = Some((core.session_name.clone(), 5));
        core.agents[0] = lead;
        let rows = core.agent_rows();
        let probe = rows.iter().find(|r| r.name == "probe").unwrap();
        assert_eq!(probe.squad, Some(1), "rows: {rows:?}");
    }
}
