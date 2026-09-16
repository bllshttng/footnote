//! The spawn-edge kind families for the retirement sweep: a PEER handoff
//! never holds its spawner, a live CHILD holds, an exited CHILD releases.
//! Split out of `gc_receipts` (file budget); shared fixtures resolve
//! through the glob.

use super::*;
use super::{no_agents, quiet_transcript, uniform_ages};
use crate::gc_sweep::{self, GcSummary};

/// One staged sweep for these tests: every row's transcript answer is the
/// one staged quiet file (2h past the 900s grace), stop confirmed, no
/// tree, empty roster. Rows are otherwise retire-eligible.
fn lineage_sweep(home: &AgentsHome, emitter: &EventEmitter) -> GcSummary {
    let store = home.root().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let quiet = quiet_transcript(&store, "q.jsonl", 2 * 3600);
    gc_sweep::run(
        home,
        emitter,
        900,
        false,
        7,
        &crate::gc_sweep::read_graph_entries,
        &move |_| Some(vec![quiet.clone()]),
        &uniform_ages(2 * 3600),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::Removed,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (Some(true), Some(true)),
        &|_| None,
    )
}

/// A claude origin-spawn row with a session id and an old created_at, so
/// classification reads retire-eligible.
fn parent_row(name: &str, sid: &str) -> state::RegistryEntry {
    let mut e = state::RegistryEntry::default();
    e.name = name.into();
    e.short_id = name.into();
    e.origin = Some("spawn".into());
    e.harness = Some("claude".into());
    e.harness_session_id = Some(sid.into());
    e.created_at = "2026-09-01T00:00:00Z".into();
    e
}

/// An origin-spawn child row naming `parent_sid` as its spawner.
fn child_row(name: &str, sid: &str, parent_sid: &str) -> state::RegistryEntry {
    let mut e = state::RegistryEntry::default();
    e.name = name.into();
    e.short_id = name.into();
    e.origin = Some("spawn".into());
    e.harness = Some("zai".into());
    e.harness_session_id = Some(sid.into());
    e.spawned_by_session = Some(parent_sid.into());
    e.created_at = "2026-09-01T00:00:00Z".into();
    e
}

/// AC1-HP: a finished blueprint parent (uncrowned, idle, otherwise
/// retire-eligible) with a live busy handoff row (`sob-t-`) naming its
/// session retires; a handoff never holds its spawner.
#[test]
fn a_peer_handoff_child_never_holds_its_finished_parent() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-demo",
            "status": "idea",
            "project": "p",
        }]),
    );
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    crate::state::update_registry(&home.registry_json(), |r| {
        r.entries.push(parent_row("bp-x-demo-repro", "s-bp-demo"));
        r.entries
            .push(child_row("sob-t-x-demo-glm", "s-child", "s-bp-demo"));
    })
    .unwrap();
    let summary = lineage_sweep(&home, &emitter);
    assert!(
        summary
            .retired
            .iter()
            .any(|(id, _)| id == "bp-x-demo-repro"),
        "{:?}",
        summary.retired
    );
    assert!(
        summary.kept_live_descendants.is_empty(),
        "{:?}",
        summary.kept_live_descendants
    );
}

/// AC1-EDGE: a live busy joiner (`jn-t-`, and the legacy `j-` spelling)
/// naming its session holds its retire-eligible lead.
#[test]
fn a_live_joiner_holds_its_retire_eligible_lead() {
    for name in ["jn-t-x-demo-1", "j-x-demo-1"] {
        let (dir, home) = staged_graph_home();
        stage_graph(
            dir.path(),
            json!([{
                "id": "x-demo",
                "status": "idea",
                "project": "p",
            }]),
        );
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        crate::state::update_registry(&home.registry_json(), |r| {
            r.entries.push(parent_row("t-x-demo-lead", "s-lead"));
            let mut joiner = child_row(name, "s-j1", "s-lead");
            joiner.status = crate::AgentStatus::Busy;
            r.entries.push(joiner);
        })
        .unwrap();
        let summary = lineage_sweep(&home, &emitter);
        assert!(
            !summary.retired.iter().any(|(id, _)| id == "t-x-demo-lead"),
            "{name}: {:?}",
            summary.retired
        );
        assert!(
            summary
                .kept_live_descendants
                .iter()
                .any(|(id, holder)| id == "t-x-demo-lead" && holder == name),
            "{name}: {:?}",
            summary.kept_live_descendants
        );
    }
}

/// AC1-ERR: the same lead with the joiner at status `exited` retires: an
/// exited joiner can hold no live process.
#[test]
fn an_exited_joiner_releases_its_lead() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-demo",
            "status": "idea",
            "project": "p",
        }]),
    );
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    crate::state::update_registry(&home.registry_json(), |r| {
        r.entries.push(parent_row("t-x-demo-lead", "s-lead"));
        let mut joiner = child_row("jn-t-x-demo-1", "s-j1", "s-lead");
        joiner.status = crate::AgentStatus::Exited;
        r.entries.push(joiner);
    })
    .unwrap();
    let summary = lineage_sweep(&home, &emitter);
    assert!(
        summary.retired.iter().any(|(id, _)| id == "t-x-demo-lead"),
        "{:?}",
        summary.retired
    );
}

/// AC2-HP: a crowned parent is kept by its crown gate, and its court row
/// (a live busy row naming the king's session) answers as a live CHILD.
#[test]
fn a_crowned_parent_is_kept_and_its_court_reads_as_child() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-demo2",
            "status": "idea",
            "project": "p",
        }]),
    );
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut king = parent_row("king-x-demo", "s-king");
        king.crown_level = Some(1);
        r.entries.push(king);
        let mut court = child_row("node-x-demo2-g2", "s-court", "s-king");
        court.status = crate::AgentStatus::Busy;
        r.entries.push(court);
    })
    .unwrap();
    let summary = lineage_sweep(&home, &emitter);
    assert!(
        !summary.retired.iter().any(|(id, _)| id == "king-x-demo"),
        "{:?}",
        summary.retired
    );
    assert!(
        summary.kept_crowned.iter().any(|id| id == "king-x-demo"),
        "{:?}",
        summary.kept_crowned
    );
}
