//! The x-2774 session-truth family: the reaper asks the session, not only
//! the node. Moved verbatim from gc_receipts (file budget: gc_receipts grew
//! past the shrink-only line; test motion is the sanctioned shrink). Same
//! fixtures: shared `pub(super)` helpers resolve through the globs.
use super::*;

use super::gc_receipts::*;

use crate::gc_sweep::{self, GcSummary};

// ── x-2774: the reaper asks the session, not only the node ──────────────

/// An in-review node with the given session ids joined through sessions[].
fn x2774_open_node(id: &str, status: &str, sids: &[&str]) -> Value {
    let rows: Vec<Value> = sids
        .iter()
        .map(|sid| {
            json!({
                "phase": "do",
                "harness": "claude",
                "session_id": sid,
                "started_at": "2026-09-01T01:00:00Z",
            })
        })
        .collect();
    json!({
        "id": id,
        "status": status,
        "sessions": rows,
    })
}

/// A spawn-origin claude registry row with a pinned short id.
fn x2774_spawn(name: &str, short_id: &str, sid: &str) -> state::RegistryEntry {
    let mut e = ask_row(name, None);
    e.short_id = short_id.into();
    e.harness = Some("claude".into());
    e.harness_session_id = Some(sid.into());
    e.origin = Some("spawn".into());
    e
}

/// The x-2774 sweep harness: production graph read over a staged graph.json,
/// staged transcripts, stop confirmed, no tree.
#[allow(clippy::too_many_arguments)]
fn x2774_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace: i64,
    dry_run: bool,
    transcripts: impl Fn(&state::RegistryEntry) -> Option<Vec<std::path::PathBuf>>,
    agents: crate::claude_roster::ClaudeAgentsSnapshot,
) -> GcSummary {
    gc_sweep::run(
        home,
        emitter,
        grace,
        dry_run,
        7,
        &gc_sweep::read_graph_entries,
        &transcripts,
        &staged_ages(&transcripts),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &move || agents.clone(),
        &|_| (None, None),
        &|_| None,
    )
}

/// Change 1 + 2: a spawn row whose node reads in_review and whose harness
/// state reads done retires with a basis naming the session state, and the
/// agent_row_reaped event carries it. `working` keeps; a failed roster read
/// keeps (fail closed).
#[test]
fn x2774_terminal_harness_state_releases_an_open_work_row() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "q.jsonl", 2 * 3600);
    stage_graph(
        dir.path(),
        json!([x2774_open_node("N1", "in_review", &["s-term"])]),
    );
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(x2774_spawn("row-term", "t-term", "s-term"));
    })
    .unwrap();
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-term") => Some(vec![quiet.clone()]),
        _ => None,
    };

    // The inverse FIRST (a dry run mutates nothing, and the apply pass below
    // retires the row): a working state keeps under open work, and the keep
    // names the reader (change 4).
    let working_agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("t-term", Some("working")),
    ]);
    let summary = x2774_sweep(&home, &emitter, 900, true, picks, working_agents);
    assert!(summary.retired.is_empty(), "{summary:?}");
    assert_eq!(
        summary.kept_open_work,
        vec![(
            "t-term".to_string(),
            "N1".to_string(),
            "in_review".to_string(),
            "sessions".to_string()
        )]
    );

    // A roster read that fails outright keeps everything (change 1).
    let unreadable = crate::claude_roster::ClaudeAgentsSnapshot::unknown("staged: unreadable");
    let summary = x2774_sweep(&home, &emitter, 900, true, picks, unreadable);
    assert!(summary.retired.is_empty(), "{summary:?}");
    assert_eq!(summary.kept_open_work.len(), 1, "{summary:?}");

    // Terminal roster state: the row retires on the session question. APPLY
    // mode, run LAST: the basis is an effect-outcome artifact, so
    // the claim is proved where the effects run - and the retirement drops
    // the row from the registry.
    let done_agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("t-term", Some("done")),
    ]);
    let summary = x2774_sweep(&home, &emitter, 900, false, picks, done_agents);
    assert_eq!(summary.retired.len(), 1, "{summary:?}");
    assert!(
        summary.retired[0]
            .1
            .starts_with("session terminal: harness state done (via sessions); node N1 in_review"),
        "basis names the session state: {summary:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Change 2: a parent whose own roster state is terminal is not held by its
/// descendants. The lineage guard's harm needs a RUNNING parent; a terminal
/// parent retires, its live child stays in the registry, and surface
/// removal never runs for the parent while it reads working.
#[test]
fn xb7f8_a_terminal_parent_is_not_held_by_its_descendants() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "parent.jsonl", 2 * 3600);
    let fresh = quiet_transcript(transcripts.path(), "child.jsonl", 10);
    stage_graph(
        dir.path(),
        json!([{
            "id": "NP",
            "status": "done",
            "sessions": [
                {"phase": "do", "harness": "claude", "session_id": "s-parent", "started_at": "2026-09-01T01:00:00Z", "ended_at": "2026-09-01T02:00:00Z"}
            ]
        }]),
    );
    state::update_registry(&home.registry_json(), |r| {
        let mut child = x2774_spawn("row-child", "t-child", "s-child");
        child.spawned_by_session = Some("s-parent".into());
        r.entries
            .push(x2774_spawn("row-parent", "t-parent", "s-parent"));
        r.entries.push(child);
    })
    .unwrap();
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-parent") => Some(vec![quiet.clone()]),
        Some("s-child") => Some(vec![fresh.clone()]),
        _ => None,
    };

    // Terminal parent: the lineage guard yields, the parent retires, the
    // child stays.
    let done = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("t-parent", Some("done")),
    ]);
    let summary = x2774_sweep(&home, &emitter, 900, false, picks, done);
    assert_eq!(summary.retired.len(), 1, "{summary:?}");
    assert_eq!(summary.retired[0].0, "t-parent", "{summary:?}");
    assert!(summary.kept_live_descendants.is_empty(), "{summary:?}");
    let registry = crate::state::load_registry(&home.registry_json()).unwrap();
    assert!(
        registry.entries.iter().any(|e| e.name == "row-child"),
        "the live child stays in the registry"
    );

    // Working parent: the guard holds exactly as today. Restore the parent
    // row the acting run above removed.
    state::update_registry(&home.registry_json(), |r| {
        r.entries
            .push(x2774_spawn("row-parent", "t-parent", "s-parent"));
    })
    .unwrap();
    let working = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("t-parent", Some("working")),
    ]);
    let summary = x2774_sweep(&home, &emitter, 900, false, picks, working);
    assert!(summary.retired.is_empty(), "{summary:?}");
    assert_eq!(
        summary.kept_live_descendants,
        vec![("t-parent".to_string(), "t-child".to_string())],
        "{summary:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Change 3: one node, two spawn rows. Older quiet, newer live: the OLDER
/// retires naming the live peer, the newer keeps under active. Both quiet:
/// neither retires on the supersession path.
#[test]
fn x2774_supersession_and_its_fail_closed_corner() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "old.jsonl", 2 * 3600);
    let fresh = quiet_transcript(transcripts.path(), "new.jsonl", 100);
    stage_graph(
        dir.path(),
        json!([x2774_open_node("N1", "in_review", &["s-old", "s-new"])]),
    );
    state::update_registry(&home.registry_json(), |r| {
        let mut older = x2774_spawn("row-old", "t-old", "s-old");
        older.created_at = "2026-09-09T22:00:00Z".into();
        let mut newer = x2774_spawn("row-new", "t-new", "s-new");
        newer.created_at = "2026-09-09T23:00:00Z".into();
        r.entries.push(older);
        r.entries.push(newer);
    })
    .unwrap();
    let older_quiet = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-old") => Some(vec![quiet.clone()]),
        Some("s-new") => Some(vec![fresh.clone()]),
        _ => None,
    };
    let summary = x2774_sweep(&home, &emitter, 900, false, older_quiet, no_agents());
    assert_eq!(
        summary.retired,
        vec![(
            "t-old".to_string(),
            "superseded on N1 by live peer row-new (created 2026-09-09T23:00:00Z)".to_string()
        )],
        "{summary:?}"
    );
    assert_eq!(
        summary.kept_open_work,
        vec![(
            "t-new".to_string(),
            "N1".to_string(),
            "in_review".to_string(),
            "sessions".to_string()
        )],
        "the live newest row keeps under its own open-work shield: {summary:?}"
    );

    let both_quiet = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-old") | Some("s-new") => Some(vec![quiet.clone()]),
        _ => None,
    };
    // Run 1 (acting) retired and removed t-old; restore it for the
    // both-quiet world.
    state::update_registry(&home.registry_json(), |r| {
        let mut older = x2774_spawn("row-old", "t-old", "s-old");
        older.created_at = "2026-09-09T22:00:00Z".into();
        r.entries.push(older);
    })
    .unwrap();
    let summary = x2774_sweep(&home, &emitter, 900, false, both_quiet, no_agents());
    assert!(summary.retired.is_empty(), "{summary:?}");
    assert_eq!(summary.kept_open_work.len(), 2, "{summary:?}");
    std::fs::remove_dir_all(home.root()).ok();
}

/// Change 6: the node status lags a recorded merge. The row retires with a
/// basis naming the recorded merge, not the lagging status. No recorded
/// merge_status keeps the row under open work, as today.
#[test]
fn x2774_a_recorded_merge_releases_a_lagging_open_node() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "lag.jsonl", 2 * 3600);
    let mut node = x2774_open_node("N1", "in_progress", &["s-lag"]);
    node["merge_status"] = json!("merged");
    node["sessions"][0]["ended_at"] = json!("2026-09-09T12:00:00Z");
    stage_graph(dir.path(), json!([node]));
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(x2774_spawn("row-lag", "t-lag", "s-lag"));
    })
    .unwrap();
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-lag") => Some(vec![quiet.clone()]),
        _ => None,
    };
    let summary = x2774_sweep(&home, &emitter, 900, false, picks, no_agents());
    assert_eq!(
        summary.retired,
        vec![(
            "t-lag".to_string(),
            "node N1 in_progress; recorded merge_status merged".to_string()
        )],
        "{summary:?}"
    );
    let mut plain = x2774_open_node("N1", "in_progress", &["s-lag"]);
    plain["merge_status"] = Value::Null;
    stage_graph(dir.path(), json!([plain]));
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(x2774_spawn("row-lag", "t-lag", "s-lag"));
    })
    .unwrap();
    let summary = x2774_sweep(&home, &emitter, 900, true, picks, no_agents());
    assert!(summary.retired.is_empty(), "{summary:?}");
    assert_eq!(summary.kept_open_work.len(), 1, "{summary:?}");
    std::fs::remove_dir_all(home.root()).ok();
}

/// Change 4: an open-work keep names the provenance source that resolved
/// its node. A name-route row resolves N2 (statuses map), the sessions
/// join answers nothing, and the keep reads `read via name`.
#[test]
fn x2774_open_work_keeps_name_their_reader() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "name.jsonl", 2 * 3600);
    stage_graph(dir.path(), json!([x2774_open_node("N2", "in_review", &[])]));
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(x2774_spawn("target-N2", "t-name", "s-name"));
    })
    .unwrap();
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-name") => Some(vec![quiet.clone()]),
        _ => None,
    };
    let summary = x2774_sweep(&home, &emitter, 900, true, picks, no_agents());
    assert_eq!(
        summary.kept_open_work,
        vec![(
            "t-name".to_string(),
            "N2".to_string(),
            "in_review".to_string(),
            "name".to_string()
        )],
        "{summary:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Change 5: one staged world judged twice. Dry and acting agree row for
/// row on every read-only policy hold, and the permitted divergences are
/// each asserted by name: dry_run_unverified (dry only: rows with
/// provable death advance to the unevaluated active-surface gate) and
/// needs_live_stop (dry only: no positive stop evidence - the codex row's
/// stop is as unevaluatable as the claude one), plus the freshness
/// re-check (acting only, covered by
/// activity_arriving_in_the_apply_window_keeps_the_row in gc.rs), and a
/// pane row the precheck calls NeedsKill (dry only;
/// x58a5_dry_and_acting_agree_on_a_gone_pid_pane_row asserts the pane
/// buckets agree row for row).
#[test]
fn x2774_dry_and_acting_agree_row_for_row() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 2 * 3600);
    let fresh = quiet_transcript(transcripts.path(), "fresh.jsonl", 10);
    let done_node = json!({
        "id": "ND",
        "status": "done",
        "sessions": [
            {"phase": "do", "harness": "codex", "session_id": "s-done", "started_at": "2026-09-01T01:00:00Z", "ended_at": "2026-09-01T02:00:00Z"},
            {"phase": "do", "harness": "claude", "session_id": "s-nostop", "started_at": "2026-09-01T01:00:00Z", "ended_at": "2026-09-01T02:00:00Z"},
            {"phase": "do", "harness": "claude", "session_id": "s-donefresh", "started_at": "2026-09-01T01:00:00Z", "ended_at": "2026-09-01T02:00:00Z"}
        ]
    });
    stage_graph(
        dir.path(),
        json!([
            done_node,
            x2774_open_node("N1", "in_review", &["s-term", "s-work"])
        ]),
    );
    state::update_registry(&home.registry_json(), |r| {
        let mut done = x2774_spawn("row-done", "t-done", "s-done");
        done.harness = Some("codex".into());
        r.entries.push(done);
        r.entries.push(x2774_spawn("row-term", "t-term", "s-term"));
        r.entries.push(x2774_spawn("row-work", "t-work", "s-work"));
        let mut nostop = x2774_spawn("row-nostop", "t-nostop", "s-nostop");
        nostop.harness = Some("claude".into());
        r.entries.push(nostop);
        r.entries
            .push(x2774_spawn("row-donefresh", "t-donefresh", "s-donefresh"));
    })
    .unwrap();
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-done") => Some(vec![quiet.clone()]),
        Some("s-donefresh") => Some(vec![fresh.clone()]),
        Some("s-nostop") | Some("s-term") | Some("s-work") => Some(vec![quiet.clone()]),
        _ => None,
    };
    let roster = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("t-term", Some("done")),
        crate::claude_roster::ClaudeAgentRow::new("t-work", Some("working")),
        crate::claude_roster::ClaudeAgentRow::new("t-nostop", Some("working")),
        crate::claude_roster::ClaudeAgentRow::new("t-donefresh", Some("done")),
    ]);
    let dry = x2774_sweep(&home, &emitter, 900, true, &picks, roster.clone());
    let acting = x2774_sweep(&home, &emitter, 900, false, &picks, roster);
    assert_eq!(
        dry.kept_open_work, acting.kept_open_work,
        "open-work bucket agrees"
    );
    assert_eq!(dry.kept_active, acting.kept_active, "active bucket agrees");
    assert_eq!(
        dry.kept_no_provenance, acting.kept_no_provenance,
        "provenance bucket agrees"
    );
    assert_eq!(
        dry.kept_open_do_row, acting.kept_open_do_row,
        "the staging re-read agrees row for row"
    );
    assert_eq!(
        dry.kept_graph_unreadable, acting.kept_graph_unreadable,
        "the graph read agrees row for row"
    );
    // The one permitted boundary: dry-run reports effect-only
    // uncertainty. A row with provable death evidence advances to the
    // active-surface gate, which a rehearsal cannot evaluate, so it is
    // named in dry_run_unverified and never planted in retired. Apply
    // records the effect outcome.
    assert!(dry.retired.is_empty(), "{:?}", dry.retired);
    let mut dry_unverified: Vec<&str> = dry
        .dry_run_unverified
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    dry_unverified.sort_unstable();
    assert_eq!(
        dry_unverified,
        vec!["t-donefresh", "t-term"],
        "positive stop evidence advances to the unevaluated active-surface gate"
    );
    assert!(
        dry.dry_run_unverified
            .iter()
            .all(|(_, gate)| gate == "active-surface removal was not evaluated"),
        "{:?}",
        dry.dry_run_unverified
    );
    let mut dry_waiting: Vec<&str> = dry
        .needs_live_stop
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    dry_waiting.sort_unstable();
    assert_eq!(
        dry_waiting,
        vec!["t-done", "t-nostop"],
        "no-evidence rows wait: the codex row's stop is as unevaluatable as \
         the claude one"
    );
    let mut acting_ids: Vec<&str> = acting.retired.iter().map(|(id, _)| id.as_str()).collect();
    acting_ids.sort_unstable();
    assert_eq!(
        acting_ids,
        vec!["t-done", "t-donefresh", "t-nostop", "t-term"],
        "apply records the effect outcome: every row whose gates confirmed \
         retired"
    );
    // x-b7f8 change 1: the early fire is named (apply's basis; the dry run
    // carries the gate name instead of a basis).
    assert!(
        acting
            .retired
            .iter()
            .any(|(id, basis)| id == "t-donefresh" && basis.contains("session terminal")),
        "{:?}",
        acting.retired
    );
    assert_eq!(
        acting.needs_live_stop.len(),
        0,
        "{:?}",
        acting.needs_live_stop
    );
    assert_eq!(acting.stop_refused.len(), 0, "{:?}", acting.stop_refused);
    assert_eq!(
        acting.dry_run_unverified.len(),
        0,
        "apply never reports dry-run uncertainty: {:?}",
        acting.dry_run_unverified
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// x-b7f8 change 3: for a terminal row the apply-window re-check asks one
/// question - did the session write since classification. A DECREASING
/// fresh age is a new write: the row keeps and the stop never fires. (The
/// 274-to-275 case - age equal or older, retire - is covered by the
/// dry/acting agreement extension above.)
#[test]
fn xb7f8_activity_arriving_in_the_apply_window_keeps_a_terminal_row() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    stage_graph(
        dir.path(),
        json!([{
            "id": "NW",
            "status": "done",
            "sessions": [
                {"phase": "do", "harness": "claude", "session_id": "s-wrote", "started_at": "2026-09-01T01:00:00Z", "ended_at": "2026-09-01T02:00:00Z"}
            ]
        }]),
    );
    state::update_registry(&home.registry_json(), |r| {
        r.entries
            .push(x2774_spawn("row-wrote", "t-wrote", "s-wrote"));
    })
    .unwrap();
    // The age seam answers 274 in the classification batch and 3 on the
    // apply-window re-read: the session wrote between the two reads.
    let counter = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let c2 = std::rc::Rc::clone(&counter);
    let age_many = move |entries: &[&state::RegistryEntry]| {
        let n = c2.get();
        c2.set(n + 1);
        entries
            .iter()
            .map(|e| (crate::gc::row_handle(e), Some(if n == 0 { 274 } else { 3 })))
            .collect::<std::collections::HashMap<String, Option<i64>>>()
    };
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-wrote") => Some(vec![]),
        _ => None,
    };
    let roster = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("t-wrote", Some("done")),
    ]);
    let roster_for_sweep = roster.clone();
    let summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        false,
        7,
        &gc_sweep::read_graph_entries,
        &picks,
        &age_many,
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &move || roster_for_sweep.clone(),
        &|_| (None, None),
        &|_| None,
    );
    assert!(
        summary.retired.is_empty(),
        "the stop never fires while activity arrived: {summary:?}"
    );
    assert_eq!(
        summary.kept_active,
        vec![("t-wrote".to_string(), 3)],
        "the decreasing re-read keeps: {summary:?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// x-58a5: a pane row whose pid is a reaped child (ESRCH) and whose codex
/// session has no rollout in the store. The precheck answers Unprovable,
/// so the dry run files it under stop_refused instead of promising the
/// retirement, and the acting run's real pane stop refuses with the SAME
/// detail. Neither retired list names it. This is the positive version of
/// the measured defect: the dry run listed bp-a238 under retired while the
/// real sweep refused it.
#[test]
fn x58a5_dry_and_acting_agree_on_a_gone_pid_pane_row() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "pane.jsonl", 2 * 3600);
    stage_graph(
        dir.path(),
        json!([{
            "id": "NP",
            "status": "done",
            "sessions": [
                {"phase": "do", "harness": "codex", "session_id": "s-pane-x58a5", "started_at": "2026-09-01T01:00:00Z", "ended_at": "2026-09-01T02:00:00Z"}
            ]
        }]),
    );
    // A child that has already been reaped: its pid reads ESRCH to every
    // later probe, the gone-pid fact the row carries.
    let mut child = std::process::Command::new("true")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    state::update_registry(&home.registry_json(), |r| {
        let mut pane = x2774_spawn("row-pane", "t-pane", "s-pane-x58a5");
        pane.harness = Some("codex".into());
        pane.substrate = Some("pane".into());
        pane.pid = Some(pid);
        pane.pid_start_time = None;
        r.entries.push(pane);
    })
    .unwrap();
    let picks = |e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
        Some("s-pane-x58a5") => Some(vec![quiet.clone()]),
        _ => None,
    };
    let dry = x2774_sweep(&home, &emitter, 900, true, &picks, no_agents());
    let acting = x2774_sweep(&home, &emitter, 900, false, &picks, no_agents());
    assert_eq!(
        dry.stop_refused, acting.stop_refused,
        "both runs refuse with an identical detail: {:?} vs {:?}",
        dry.stop_refused, acting.stop_refused
    );
    assert_eq!(dry.stop_refused.len(), 1, "{:?}", dry.stop_refused);
    assert!(
        dry.stop_refused[0].1.contains("is gone (ESRCH)")
            && dry.stop_refused[0].1.contains("codex"),
        "the refusal names the pid fact and the holder read: {:?}",
        dry.stop_refused
    );
    assert!(!dry.retired.iter().any(|(id, _)| id == "t-pane"));
    assert!(!acting.retired.iter().any(|(id, _)| id == "t-pane"));
    std::fs::remove_dir_all(home.root()).ok();
}

/// Change 7: the stale-do-row settle tests whether an additional PR is
/// OPEN by RECORDED state, not whether one exists. Recorded merged does
/// not hold; recorded open does not settle; unrecorded does not settle
/// (absence is never read as merged).
#[test]
fn x2774_additional_pr_openness_decides_the_settle() {
    let merged = json!({"number": 1522, "url": "https://github.com/o/r/pull/1522", "merge_status": "merged"});
    let open =
        json!({"number": 1600, "url": "https://github.com/o/r/pull/1600", "merge_status": "open"});
    let unrecorded = json!({"number": 1601, "url": "https://github.com/o/r/pull/1601"});

    // All recorded merged: the row settles.
    let entries = vec![done_node(
        "NA",
        json!("merged"),
        json!([merged.clone()]),
        vec![open_do_row("claude", "sess-a")],
    )];
    let stale = gc_sweep::stale_open_do_rows(&entries);
    assert_eq!(stale.len(), 1, "{stale:?}");

    // One recorded-open additional PR: the do row is NOT settled.
    let entries = vec![done_node(
        "NB",
        json!("merged"),
        json!([merged.clone(), open]),
        vec![open_do_row("claude", "sess-b")],
    )];
    let stale = gc_sweep::stale_open_do_rows(&entries);
    assert!(stale.is_empty(), "{stale:?}");

    // One unrecorded additional PR: NOT settled - absence is never merged.
    let entries = vec![done_node(
        "NC",
        json!("merged"),
        json!([merged, unrecorded]),
        vec![open_do_row("claude", "sess-c")],
    )];
    let stale = gc_sweep::stale_open_do_rows(&entries);
    assert!(stale.is_empty(), "{stale:?}");
}
