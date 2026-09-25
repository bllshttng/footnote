//! Which crown owns an open question. The attention arm calls [`Router::route`]
//! once per delivered page and freezes the answer into the page frontmatter
//! (the user's "at write time" rule): a crown crowned later never sees older
//! pages in its check-in. Asker facts join the registry by session id first
//! and treat the name as an alias. Nothing is guessed: an unmeasured asker
//! fact stays `None` (the page renders `unknown`) and a routing fact the walk
//! found nothing for stays `None` (the page renders `none`).

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::attention::AttentionItem;
use crate::state::{load_registry, Registry};
use crate::territory::{self, Crown};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Routing {
    pub session_name: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub epic: Option<String>,
    pub crown: Option<String>,
    pub king: Option<String>,
}

pub struct Router {
    entries_by_id: HashMap<String, Value>,
    /// Rung-2 member epic -> (canonical scope, holder).
    epic_crowns: HashMap<String, (String, String)>,
    /// Rung-0/1 canonical project -> (canonical scope, holder).
    project_crowns: HashMap<String, (String, String)>,
    /// Workspace path basename -> canonical project name.
    projects_by_basename: HashMap<String, String>,
    registry: Registry,
}

impl Router {
    /// Read the graph, the live crowns and the workspace map from `cwd` and
    /// the registry cache at `registry_path`. A graph or registry fault is
    /// `Err` naming the source: delivery waits a beat rather than stamping
    /// pages with no crown.
    pub fn load(cwd: &Path, registry_path: &Path) -> Result<Router, String> {
        let entries = territory::graph_entries(cwd).map_err(|e| format!("attention_route: {e}"))?;
        let crowns =
            territory::live_crowns(registry_path).map_err(|e| format!("attention_route: {e}"))?;
        // An absent alias map is a normal shape, not a fault.
        let projects = crate::king_board::project_map(cwd).unwrap_or_default();
        let workspace = territory::workspace_paths(cwd);
        Ok(Router::from_parts(entries, crowns, projects, workspace, {
            load_registry(registry_path)
                .map_err(|e| format!("attention_route: registry unreadable ({e})"))?
        }))
    }

    pub fn from_parts(
        entries: Vec<Value>,
        crowns: Vec<Crown>,
        projects: HashMap<String, String>,
        workspace: HashMap<String, String>,
        registry: Registry,
    ) -> Router {
        let mut entries_by_id = HashMap::new();
        for e in &entries {
            if let Some(id) = e.get("id").and_then(Value::as_str) {
                entries_by_id.insert(id.to_string(), e.clone());
            }
        }
        let mut epic_crowns = HashMap::new();
        let mut project_crowns = HashMap::new();
        for crown in &crowns {
            for member in crown
                .scope
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let slot = if crown.level >= 2 {
                    &mut epic_crowns
                } else {
                    &mut project_crowns
                };
                slot.entry(member.to_string())
                    .or_insert_with(|| (crown.scope.clone(), crown.holder.clone()));
            }
        }
        // basename(workspace path) -> canonical project, aliases resolved.
        let mut projects_by_basename = HashMap::new();
        for (alias, canonical) in &projects {
            if let Some(path) = workspace.get(alias) {
                if let Some(base) = path.rsplit('/').next() {
                    if !base.is_empty() {
                        projects_by_basename
                            .entry(base.to_string())
                            .or_insert_with(|| canonical.clone());
                    }
                }
            }
        }
        Router {
            entries_by_id,
            epic_crowns,
            project_crowns,
            projects_by_basename,
            registry,
        }
    }

    pub fn route(&self, item: &AttentionItem) -> Routing {
        let mut out = Routing::default();
        // Asker facts: the live row by session id first, the handle as alias.
        let row = item
            .asker
            .as_ref()
            .and_then(|a| a.session_id.as_deref())
            .filter(|s| !s.is_empty())
            .and_then(|sid| {
                self.registry.entries.iter().find(|e| {
                    e.harness_session_id.as_deref() == Some(sid)
                        || e.session_id.as_deref() == Some(sid)
                })
            })
            .or_else(|| {
                item.asker
                    .as_ref()
                    .map(|a| a.handle.as_str())
                    .filter(|h| !h.is_empty())
                    .and_then(|h| self.registry.find(h))
            });
        if let Some(row) = row {
            out.session_name = Some(row.name.clone());
            out.harness = row.harness.clone();
            out.model = row.model.clone();
        } else if let Some(a) = &item.asker {
            // No registry row: the handle is all we know about the name.
            if !a.handle.trim().is_empty() {
                out.session_name = Some(a.handle.clone());
            }
            out.harness = a.harness.clone();
        }

        // Node: the first block, else the item's own node unless placeholder.
        let node = item
            .blocks
            .first()
            .cloned()
            .or_else(|| item.node.clone())
            .filter(|n| !n.is_empty() && n != "none");

        if let Some(node) = &node {
            let chain = self.parent_chain(node);
            out.epic = chain
                .iter()
                .filter(|id| self.entry_type(id) == Some("epic"))
                .next_back()
                .cloned();
            // The first chain id a rung-2 crown lists wins.
            if let Some(id) = chain.iter().find(|id| self.epic_crowns.contains_key(*id)) {
                let (scope, holder) = &self.epic_crowns[id];
                out.crown = Some(scope.clone());
                out.king = Some(holder.clone());
            }
        }

        if out.crown.is_none() {
            // With a node: the node's graph project. Without: the asker's
            // live crown, else its row's node, else the workspace basename
            // of the item's own project.
            let mut candidates: Vec<String> = Vec::new();
            if let Some(node) = &node {
                if let Some(project) = self.entry_project(node) {
                    candidates.push(project);
                }
            }
            if candidates.is_empty() {
                if let Some(row) = row {
                    if let Some(scope) = row.crown_scope.as_deref() {
                        let canon = crate::event_store::canonical_scope(scope);
                        if !canon.is_empty() {
                            out.crown = Some(canon.clone());
                            out.king = self.crown_holder(&canon);
                        }
                    }
                    if out.crown.is_none() {
                        if let Some(row_node) = row.node.as_deref() {
                            if !row_node.is_empty() && row_node != "none" {
                                let chain = self.parent_chain(row_node);
                                out.epic = chain
                                    .iter()
                                    .filter(|id| self.entry_type(id) == Some("epic"))
                                    .next_back()
                                    .cloned();
                                if let Some(id) =
                                    chain.iter().find(|id| self.epic_crowns.contains_key(*id))
                                {
                                    let (scope, holder) = &self.epic_crowns[id];
                                    out.crown = Some(scope.clone());
                                    out.king = Some(holder.clone());
                                } else if let Some(project) = self.entry_project(row_node) {
                                    candidates.push(project);
                                }
                            }
                        }
                    }
                }
            }
            // The basename fallback the plan names for a node-less item:
            // the rung-1 crown whose workspace path basename matches.
            if out.crown.is_none() && candidates.is_empty() && !item.project.is_empty() {
                candidates.push(item.project.clone());
            }
            if out.crown.is_none() {
                for project in &candidates {
                    let canon = self.projects_by_basename.get(project).unwrap_or(project);
                    if let Some((scope, holder)) = self.project_crowns.get(canon) {
                        out.crown = Some(scope.clone());
                        out.king = Some(holder.clone());
                        break;
                    }
                }
                if out.crown.is_none() && node.is_none() && row.is_none() {
                    // Last resort: the only rung-0/1 crown when exactly one is live.
                    let project_crowns: Vec<_> = self.project_crowns.values().collect();
                    if project_crowns.len() == 1 {
                        let (scope, holder) = project_crowns[0];
                        out.crown = Some(scope.clone());
                        out.king = Some(holder.clone());
                    }
                }
            }
        }
        out.king = match (&out.crown, &out.king) {
            (Some(_), k) => k.clone(),
            (None, _) => None,
        };
        out
    }

    /// The node then each parent, nearest first; stops on a repeat (a cycle)
    /// or after 64 steps.
    fn parent_chain(&self, start: &str) -> Vec<String> {
        let mut chain = vec![start.to_string()];
        let mut seen: HashMap<String, ()> = chain.iter().map(|i| (i.clone(), ())).collect();
        let mut cursor = start.to_string();
        for _ in 0..64 {
            let Some(parent) = self
                .entries_by_id
                .get(&cursor)
                .and_then(|e| e.get("parent"))
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty() && !seen.contains_key(*p))
            else {
                break;
            };
            cursor = parent.to_string();
            seen.insert(cursor.clone(), ());
            chain.push(cursor.clone());
        }
        chain
    }

    fn entry_type(&self, id: &str) -> Option<&str> {
        self.entries_by_id
            .get(id)
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
    }

    fn entry_project(&self, id: &str) -> Option<String> {
        self.entries_by_id
            .get(id)
            .and_then(|e| e.get("project"))
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
    }

    fn crown_holder(&self, canonical: &str) -> Option<String> {
        self.epic_crowns
            .get(canonical)
            .or_else(|| self.project_crowns.get(canonical))
            .map(|(_, holder)| holder.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{Asker, AttentionItem};
    use crate::claims::test_env_lock;
    use serde_json::json;

    fn entry(id: &str, kind: &str, parent: Option<&str>, project: &str) -> Value {
        let mut e = json!({"id": id, "type": kind, "project": project});
        if let Some(p) = parent {
            e["parent"] = json!(p);
        }
        e
    }

    fn crowns() -> Vec<Crown> {
        vec![
            Crown {
                scope: "x-b".into(),
                level: 2,
                holder: "king-b".into(),
                holder_session: None,
            },
            Crown {
                scope: "fno".into(),
                level: 1,
                holder: "king-fno".into(),
                holder_session: None,
            },
        ]
    }

    fn router(entries: Vec<Value>, crowns: Vec<Crown>) -> Router {
        Router::from_parts(
            entries,
            crowns,
            HashMap::from([("fno".to_string(), "fno".to_string())]),
            HashMap::from([("fno".to_string(), "/ws/fno".to_string())]),
            Registry::default(),
        )
    }

    fn item(blocks: Vec<String>, node: Option<&str>, project: &str) -> AttentionItem {
        AttentionItem {
            id: "q-test".into(),
            kind: "question".into(),
            title: "t".into(),
            body: None,
            project: project.into(),
            priority: "normal".into(),
            created_at: "2026-09-23T00:00:00Z".into(),
            deadline: None,
            on_silence: None,
            class: None,
            blocks,
            subject: None,
            asker: None,
            node: node.map(|n| n.to_string()),
            blocked_because: None,
            options_rationale: None,
            recommendation: None,
            options: vec![],
            unknowns: None,
            reversible: None,
            cost_if_wrong: None,
            meanwhile: None,
            ready: true,
            missing: vec![],
            state: "open".into(),
        }
    }

    #[test]
    fn ac2_hp_node_under_crowned_epic_routes_to_it() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let r = router(
            vec![
                entry("x-a", "epic", None, "fno"),
                entry("x-b", "epic", Some("x-a"), "fno"),
                entry("x-c", "feature", Some("x-b"), "fno"),
            ],
            crowns(),
        );
        let routing = r.route(&item(vec!["x-c".into()], None, "fno"));
        assert_eq!(routing.crown.as_deref(), Some("x-b"), "AC2-HP");
        assert_eq!(routing.king.as_deref(), Some("king-b"), "AC2-HP");
        assert_eq!(routing.epic.as_deref(), Some("x-a"), "AC2-HP");
    }

    #[test]
    fn ac2_edge_no_node_falls_through_asker_row_to_basename_to_none() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let entries = vec![
            entry("x-a", "epic", None, "fno"),
            entry("x-b", "epic", Some("x-a"), "fno"),
            entry("x-c", "feature", Some("x-b"), "fno"),
        ];
        // An asker whose registry row holds node x-c routes like x-c.
        let mut reg = Registry::default();
        reg.entries.push(crate::state::RegistryEntry {
            name: "worker-1".into(),
            node: Some("x-c".into()),
            ..Default::default()
        });
        let r = Router::from_parts(
            entries.clone(),
            crowns(),
            HashMap::from([("fno".to_string(), "fno".to_string())]),
            HashMap::from([("fno".to_string(), "/ws/fno".to_string())]),
            reg,
        );
        let mut it = item(vec![], None, "fno");
        it.asker = Some(Asker {
            handle: "worker-1".into(),
            session_id: None,
            harness: None,
            rank: None,
            live: None,
            reach: None,
        });
        let routing = r.route(&it);
        assert_eq!(
            routing.crown.as_deref(),
            Some("x-b"),
            "AC2-EDGE asker row node"
        );
        assert_eq!(routing.epic.as_deref(), Some("x-a"));

        // An asker with no row: the rung-1 crown whose workspace basename
        // matches the item's project.
        let r = router(entries.clone(), crowns());
        let routing = r.route(&item(vec![], None, "fno"));
        assert_eq!(
            routing.crown.as_deref(),
            Some("fno"),
            "AC2-EDGE workspace basename"
        );
        assert_eq!(routing.king.as_deref(), Some("king-fno"));

        // With no live crown at all: crown, king and epic are None.
        let r = router(entries, vec![]);
        let routing = r.route(&item(vec![], None, "fno"));
        assert_eq!(routing.crown, None, "AC2-EDGE no crown");
        assert_eq!(routing.king, None);
        assert_eq!(routing.epic, None);
    }

    #[test]
    fn ac2_err_parent_cycle_returns_within_bound_and_falls_back() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let r = router(
            vec![
                entry("x-c", "feature", Some("x-d"), "fno"),
                entry("x-d", "feature", Some("x-c"), "fno"),
            ],
            crowns(),
        );
        let routing = r.route(&item(vec!["x-c".into()], None, "fno"));
        assert_eq!(
            routing.crown.as_deref(),
            Some("fno"),
            "AC2-ERR cycle falls back to the project crown"
        );
        assert_eq!(routing.king.as_deref(), Some("king-fno"));
        assert_eq!(
            routing.epic, None,
            "no epic in a cycle with no crowned epic"
        );
    }

    #[test]
    fn asker_facts_join_by_session_id_first_and_stay_none_unmeasured() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut reg = Registry::default();
        reg.entries.push(crate::state::RegistryEntry {
            name: "row-name".into(),
            session_id: Some("aaaaaaaa".into()),
            harness_session_id: Some("bbbbbbbb".into()),
            harness: Some("claude".into()),
            model: Some("glm-5.3-flash".into()),
            ..Default::default()
        });
        // An alias row the handle would also match; the session id must win.
        reg.entries.push(crate::state::RegistryEntry {
            name: "aaaaaaaa".into(),
            ..Default::default()
        });
        let r = Router::from_parts(vec![], crowns(), HashMap::new(), HashMap::new(), reg);
        let mut it = item(vec![], None, "fno");
        it.asker = Some(Asker {
            handle: "aaaaaaaa".into(),
            session_id: Some("bbbbbbbb".into()),
            harness: None,
            rank: None,
            live: None,
            reach: None,
        });
        let routing = r.route(&it);
        assert_eq!(routing.session_name.as_deref(), Some("row-name"));
        assert_eq!(routing.harness.as_deref(), Some("claude"));
        assert_eq!(routing.model.as_deref(), Some("glm-5.3-flash"));

        // No registry row: the handle stands in as the session name, the
        // item's harness carries over, and the model stays unmeasured.
        let r = Router::from_parts(
            vec![],
            vec![],
            HashMap::new(),
            HashMap::new(),
            Registry::default(),
        );
        let mut it2 = item(vec![], None, "fno");
        it2.asker = Some(Asker {
            handle: "deadbeef".into(),
            session_id: None,
            harness: Some("codex".into()),
            rank: None,
            live: None,
            reach: None,
        });
        let routing = r.route(&it2);
        assert_eq!(routing.session_name.as_deref(), Some("deadbeef"));
        assert_eq!(routing.harness.as_deref(), Some("codex"));
        assert_eq!(routing.model, None);
    }

    #[test]
    fn placeholder_node_is_ignored() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let r = router(vec![], crowns());
        let routing = r.route(&item(vec![], Some("none"), "fno"));
        assert_eq!(
            routing.crown.as_deref(),
            Some("fno"),
            "node=none falls through"
        );
    }
}
