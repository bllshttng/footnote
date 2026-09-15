//! One owner for healing a registry row whose mux pane ref is dead.
//!
//! Eight readers key on the stored `mux` ref (`keystroke_lane`, `_deliver_live`,
//! the codex thread predicates, resume). When the pane the ref names is gone
//! and the row's codex thread is still loaded in the app-server, the FACT is
//! stale, so the fix heals the fact once here and every reader stays as it is.
//! The verdict is data, never an exit code.

use crate::daemon::PaneProbe;
use crate::paths::AgentsHome;
#[cfg(test)]
use crate::state::RegistryEntry;
use crate::state::{self};
use serde::Serialize;

/// What the probe + the loaded-thread list proved about a row's mux ref.
/// Serialized verbatim as the `mail-inject --lane-heal` one JSON output line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneVerdict {
    pub verdict: &'static str,
    pub reason: Option<&'static str>,
    pub pane: Option<PaneRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRef {
    pub session: String,
    pub pane_id: u64,
}

fn verdict(name: &'static str, reason: Option<&'static str>, pane: Option<PaneRef>) -> LaneVerdict {
    LaneVerdict {
        verdict: name,
        reason,
        pane,
    }
}

/// Probe the row's pane ref and, on a dead pane whose codex thread is loaded
/// in the app-server, optionally rebind the row to the thread lane.
///
/// `probe` is the pane presence probe (`run_mux_pane_probe` in production);
/// `loaded` answers which codex threads the app-server has loaded. The seams
/// keep the tests free of a mux and a daemon. Rebind (`rebind = true`) writes
/// under [`state::update_registry`] and only when the row's mux STILL equals
/// the probed ref, so a concurrent rebind wins and this call stands down.
pub fn heal_dead_pane_binding(
    home: &AgentsHome,
    session_id: &str,
    rebind: bool,
    probe: &dyn Fn(&str, u64) -> PaneProbe,
    loaded: &dyn Fn() -> Result<Vec<String>, &'static str>,
) -> LaneVerdict {
    let registry_path = home.registry_json();
    let Ok(registry) = state::load_registry(&registry_path) else {
        return verdict("unmeasurable", Some("registry-unreadable"), None);
    };
    let Some(row) = registry
        .entries
        .iter()
        .find(|e| e.harness_session_id.as_deref() == Some(session_id))
    else {
        return verdict("no-mux-ref", None, None);
    };
    let Some(mux) = row.mux.clone() else {
        return verdict("no-mux-ref", None, None);
    };
    let pane = PaneRef {
        session: mux.session.clone(),
        pane_id: mux.pane_id,
    };
    match probe(&mux.session, mux.pane_id) {
        PaneProbe::Present => return verdict("live-pane", None, Some(pane)),
        PaneProbe::Unknown => {
            return verdict("unmeasurable", Some("mux-probe-unknown"), Some(pane))
        }
        PaneProbe::Absent => {}
    }
    if row.harness.as_deref() != Some("codex") {
        // A claude dead pane keeps its existing heal owners; nothing is written.
        return verdict("dead-pane", Some("not-codex"), Some(pane));
    }
    let loaded_ids = match loaded() {
        Ok(ids) => ids,
        Err(reason) => return verdict("unmeasurable", Some(reason), Some(pane)),
    };
    if !loaded_ids.iter().any(|id| id == session_id) {
        return verdict("dead-pane", Some("thread-not-loaded"), Some(pane));
    }
    if !rebind {
        // Resume uses this mode: it rebinds to a viewport pane itself, so the
        // verdict only reports the deliverable thread lane.
        return verdict("dead-pane-loaded", None, Some(pane));
    }
    let mut wrote = false;
    let write =
        state::update_registry(&registry_path, |r| {
            let Some(target) = r.entries.iter_mut().find(|e| {
                e.name == row.name && e.harness_session_id.as_deref() == Some(session_id)
            }) else {
                return;
            };
            // A concurrent resume may have rebound the row; only the probed ref
            // is ours to clear.
            if target.mux.as_ref() != Some(&mux) {
                return;
            }
            target.mux = None;
            target.pid = None;
            target.pid_start_time = None;
            target.substrate = Some("thread".to_string());
            target.host_mode = Some("interactive".to_string());
            wrote = true;
        });
    if write.is_err() || !wrote {
        return verdict("unmeasurable", Some("row-changed"), None);
    }
    crate::client_verbs::append_agents_event(
        &crate::client_verbs::trace_events_path(home),
        "agent_lane_rebound",
        &[
            ("name", serde_json::Value::String(row.name.clone())),
            (
                "session_id",
                serde_json::Value::String(session_id.to_string()),
            ),
            (
                "from_session",
                serde_json::Value::String(mux.session.clone()),
            ),
            ("from_pane_id", serde_json::json!(mux.pane_id)),
            (
                "to",
                serde_json::Value::String("codex-app-server-thread".to_string()),
            ),
        ],
    );
    verdict("rebound-thread", None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_pane_row(name: &str, session: &str, mux_session: &str, pane: u64) -> RegistryEntry {
        let mut e = RegistryEntry {
            name: name.to_string(),
            ..Default::default()
        };
        e.harness = Some("codex".to_string());
        e.harness_session_id = Some(session.to_string());
        e.mux = Some(state::MuxRef {
            session: mux_session.to_string(),
            pane_id: pane,
        });
        e.pid = Some(4242);
        e.pid_start_time = Some(777);
        e.substrate = Some("pane".to_string());
        e.host_mode = Some("interactive".to_string());
        e
    }

    fn push_row(home: &AgentsHome, row: RegistryEntry) {
        state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();
    }

    fn read_row(home: &AgentsHome, session: &str) -> Option<RegistryEntry> {
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.harness_session_id.as_deref() == Some(session))
    }

    fn loaded_ok(ids: &[&str]) -> impl Fn() -> Result<Vec<String>, &'static str> {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        move || Ok(ids.clone())
    }

    fn tmp_home(tag: &str) -> AgentsHome {
        let dir = std::env::temp_dir().join(format!("fno-lane-heal-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        AgentsHome::at(dir)
    }

    #[test]
    fn rebind_clears_the_mux_ref_and_stamps_the_thread_lane() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("rebind");
        push_row(&home, codex_pane_row("w1", "sess-1", "main", 2179));
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let v = heal_dead_pane_binding(
            &home,
            "sess-1",
            true,
            &absent,
            &loaded_ok(&["sess-1", "sess-2"]),
        );
        assert_eq!(v.verdict, "rebound-thread");
        let row = read_row(&home, "sess-1").unwrap();
        assert!(row.mux.is_none());
        assert!(row.pid.is_none());
        assert!(row.pid_start_time.is_none());
        assert_eq!(row.substrate.as_deref(), Some("thread"));
        assert_eq!(row.host_mode.as_deref(), Some("interactive"));
        let events =
            std::fs::read_to_string(crate::client_verbs::trace_events_path(&home)).unwrap();
        assert!(events.contains("agent_lane_rebound"));
        assert!(events.contains("2179"));
    }

    #[test]
    fn no_rebind_reports_the_loaded_thread_and_writes_nothing() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("no-rebind");
        push_row(&home, codex_pane_row("w1", "sess-1", "main", 2179));
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let v = heal_dead_pane_binding(&home, "sess-1", false, &absent, &loaded_ok(&["sess-1"]));
        assert_eq!(v.verdict, "dead-pane-loaded");
        assert_eq!(v.pane.as_ref().unwrap().pane_id, 2179);
        let row = read_row(&home, "sess-1").unwrap();
        assert!(row.mux.is_some());
    }

    #[test]
    fn no_row_or_no_mux_ref_reads_no_mux_ref() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("no-row");
        let present = |_s: &str, _p: u64| PaneProbe::Present;
        let v = heal_dead_pane_binding(&home, "ghost", true, &present, &loaded_ok(&[]));
        assert_eq!(v.verdict, "no-mux-ref");
        push_row(&home, {
            let mut e = codex_pane_row("w2", "sess-2", "main", 5);
            e.mux = None;
            e.pid = None;
            e.pid_start_time = None;
            e
        });
        let v = heal_dead_pane_binding(&home, "sess-2", true, &present, &loaded_ok(&["sess-2"]));
        assert_eq!(v.verdict, "no-mux-ref");
    }

    #[test]
    fn a_live_pane_never_heals() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("live");
        push_row(&home, codex_pane_row("w1", "sess-1", "main", 2179));
        let present = |_s: &str, _p: u64| PaneProbe::Present;
        let v = heal_dead_pane_binding(&home, "sess-1", true, &present, &loaded_ok(&["sess-1"]));
        assert_eq!(v.verdict, "live-pane");
        assert!(read_row(&home, "sess-1").unwrap().mux.is_some());
    }

    #[test]
    fn an_unknown_probe_is_fail_closed() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("unknown");
        push_row(&home, codex_pane_row("w1", "sess-1", "main", 2179));
        let unknown = |_s: &str, _p: u64| PaneProbe::Unknown;
        let v = heal_dead_pane_binding(&home, "sess-1", true, &unknown, &loaded_ok(&["sess-1"]));
        assert_eq!(v.verdict, "unmeasurable");
        assert_eq!(v.reason, Some("mux-probe-unknown"));
        assert!(read_row(&home, "sess-1").unwrap().mux.is_some());
    }

    #[test]
    fn a_dead_non_codex_pane_is_left_for_its_existing_owners() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("not-codex");
        let mut row = codex_pane_row("c1", "sess-1", "main", 2179);
        row.harness = Some("claude".to_string());
        push_row(&home, row);
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let v = heal_dead_pane_binding(&home, "sess-1", true, &absent, &loaded_ok(&["sess-1"]));
        assert_eq!(v.verdict, "dead-pane");
        assert_eq!(v.reason, Some("not-codex"));
        assert!(read_row(&home, "sess-1").unwrap().mux.is_some());
    }

    #[test]
    fn an_unreadable_loaded_list_and_a_missing_thread_stay_dead() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("loaded-err");
        push_row(&home, codex_pane_row("w1", "sess-1", "main", 2179));
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let err = || Err("io-error");
        let v = heal_dead_pane_binding(&home, "sess-1", true, &absent, &err);
        assert_eq!(v.verdict, "unmeasurable");
        assert_eq!(v.reason, Some("io-error"));
        assert!(read_row(&home, "sess-1").unwrap().mux.is_some());

        let v = heal_dead_pane_binding(&home, "sess-1", true, &absent, &loaded_ok(&["other"]));
        assert_eq!(v.verdict, "dead-pane");
        assert_eq!(v.reason, Some("thread-not-loaded"));
    }

    #[test]
    fn a_row_that_changed_under_us_is_not_written() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("row-changed");
        push_row(&home, codex_pane_row("w1", "sess-1", "main", 2179));
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        // Simulate a concurrent rebind: the loaded read mutates the row before
        // the heal's own write re-finds it, so the probed ref no longer holds.
        let racer_home = home.clone();
        let racer = move || {
            state::update_registry(&racer_home.registry_json(), |r| {
                if let Some(e) = r
                    .entries
                    .iter_mut()
                    .find(|e| e.harness_session_id.as_deref() == Some("sess-1"))
                {
                    e.mux = None;
                }
            })
            .unwrap();
            Ok(vec!["sess-1".to_string()])
        };
        let v = heal_dead_pane_binding(&home, "sess-1", true, &absent, &racer);
        assert_eq!(v.verdict, "unmeasurable");
        assert_eq!(v.reason, Some("row-changed"));
    }
}
