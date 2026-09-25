//! The revival gate test family: the resume gesture, the held-pane focus,
//! the spawn-site choke, and the payload/answer readers. Wired from
//! revival_gate.rs (the parent is shrink-only); the shared builders resolve
//! through `crate::server::tests`.

use super::*;
use crate::proto::Command;
use crate::server::resume_argv::{set_resume_program, ResumeProgramGuard};
use crate::server::tests::{
    client_with_rx, drain_notices, empty_core, exited_claude_row, leaf_tab,
};
use crate::spawn_journal::HeldWorker;

const REFUSAL: &str = "spawn-gate: refused on max_live (7 of 7)";

fn dead_codex_row(name: &str, sid: &str, cwd: &str) -> crate::agents_view::RegistryAgent {
    let mut row = exited_claude_row(name, None);
    row.harness = Some("codex".into());
    row.harness_session_id = Some(sid.into());
    row.cwd = cwd.into();
    row
}

/// The production-shaped refusal the override answers with, built through
/// the same reader the real ask feeds.
fn refused_reason(name: &str) -> String {
    read_gate_answer(
        r#"{"status":"refused","receipt":{"reason":"max_live"}}"#,
        REFUSAL,
        name,
    )
    .unwrap_err()
}

fn squad_with_shell(core: &mut super::super::Core, tag: &str) -> (u64, String) {
    core.shells = vec!["/bin/cat".into()];
    let cwd = std::env::temp_dir().join(format!("fno-revival-{tag}"));
    std::fs::create_dir_all(&cwd).unwrap();
    let shell = core
        .spawn_pane(24, 80, cwd.to_string_lossy().as_ref())
        .unwrap();
    core.session.add_squad(
        7,
        vec![cwd.to_string_lossy().into_owned()],
        None,
        leaf_tab(70, shell),
    );
    (shell, cwd.to_string_lossy().into_owned())
}

#[test]
fn gesture_refusal_names_the_gate_verdict_and_the_escape() {
    let _guard = GateOverrideGuard;
    set_gate_override(GateOverride::Refuse(refused_reason("t-codex-dead")));
    let mut core = empty_core();
    let (shell, cwd) = squad_with_shell(&mut core, "refuse");
    core.agents = vec![dead_codex_row("t-codex-dead", "codex-dead-session", &cwd)];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.staged_resume_argv = Some(vec!["/bin/cat".into(), "codex-dead-session".into()]);

    core.command(
        1,
        Command::ResumeAgent {
            name: "t-codex-dead".into(),
        },
    );

    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("spawn-gate: refused on max_live"),
        "{notices}"
    );
    assert!(
        notices.contains("FNO_SPAWN_GATE=0 fno agents resume t-codex-dead"),
        "{notices}"
    );
    assert!(
        core.panes.keys().all(|&p| p == shell),
        "no pane spawns on a refusal"
    );
    assert!(core.worker_pane.is_empty());
    core.reap_pane(shell);
}

#[test]
fn gesture_ask_charges_the_rows_own_parent_and_account() {
    let _guard = GateOverrideGuard;
    set_gate_override(GateOverride::Ask);
    let mut core = empty_core();
    let (shell, cwd) = squad_with_shell(&mut core, "ask");
    let mut row = exited_claude_row("t-claude-dead", Some("claude-dead-session"));
    row.harness = Some("claude".into());
    row.cwd = cwd;
    row.spawned_by_session = Some("k1".into());
    row.account = Some("makers".into());
    core.agents = vec![row];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);

    core.command(
        1,
        Command::ResumeAgent {
            name: "t-claude-dead".into(),
        },
    );

    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("resume t-claude-dead: waiting for the spawn gate"),
        "{notices}"
    );
    let asks = take_asks();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].name, "t-claude-dead");
    assert_eq!(asks[0].caller_session.as_deref(), Some("k1"));
    assert_eq!(asks[0].account.as_deref(), Some("makers"));
    assert!(
        core.panes.keys().all(|&p| p == shell),
        "no pane before the answer"
    );
    core.reap_pane(shell);
}

#[test]
fn gate_answer_stages_an_admission_the_replay_spends() {
    let _guard = GateOverrideGuard;
    let _prog = ResumeProgramGuard;
    set_resume_program(&["/bin/cat"]);
    let mut core = empty_core();
    let (shell, cwd) = squad_with_shell(&mut core, "replay");
    core.agents = vec![dead_codex_row("t-codex-dead", "codex-dead-session", &cwd)];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.staged_resume_argv = Some(vec!["/bin/cat".into(), "codex-dead-session".into()]);

    core.on_revival_gate_answered(
        1,
        "t-codex-dead".into(),
        Ok(()),
        Box::new(super::ResumeReplay::Gesture {
            name: "t-codex-dead".into(),
        }),
    );

    let notices = drain_notices(&mut rx).join("\n");
    assert!(notices.contains("resumed t-codex-dead"), "{notices}");
    let new_panes: Vec<u64> = core
        .panes
        .keys()
        .filter(|&&p| p != shell)
        .copied()
        .collect();
    assert_eq!(new_panes.len(), 1, "the replay spawned the pane");
    assert!(
        core.revival_admission.is_none(),
        "the admission is spent at the spawn"
    );
    for pid in new_panes {
        core.reap_pane(pid);
    }
    core.reap_pane(shell);
}

#[test]
fn gate_answer_refusal_notices_and_starts_nothing() {
    let _guard = GateOverrideGuard;
    let mut core = empty_core();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);

    core.on_revival_gate_answered(
        1,
        "t-codex-dead".into(),
        Err(REFUSAL.into()),
        Box::new(super::ResumeReplay::Gesture {
            name: "t-codex-dead".into(),
        }),
    );

    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("resume t-codex-dead refused: spawn-gate: refused on max_live"),
        "{notices}"
    );
    assert!(core.revival_admission.is_none());
}

#[test]
fn choke_refuses_a_spawn_with_no_staged_admission() {
    let _guard = GateOverrideGuard;
    set_gate_override(GateOverride::Ask);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let facts = HeldWorker {
        name: "w1".into(),
        harness: "codex".into(),
        harness_session_id: "sid".into(),
        cwd: "/tmp".into(),
    };
    let err = core
        .resume_worker_into(&facts, 0, None, 24, 80, None, None)
        .unwrap_err();
    assert!(err.contains("spawn gate not asked"), "{err}");
    assert!(core.panes.is_empty(), "the choke spawns nothing");
}

#[test]
fn held_focus_refusal_keeps_the_seat_held() {
    let _guard = GateOverrideGuard;
    set_gate_override(GateOverride::Refuse(REFUSAL.into()));
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(24, 80, "/tmp").unwrap();
    core.session
        .add_squad(1, vec!["/tmp".into()], None, leaf_tab(1, pid));
    core.held_workers.insert(
        pid,
        HeldWorker {
            name: "held-worker".into(),
            harness: "codex".into(),
            harness_session_id: "held-session".into(),
            cwd: "/tmp".into(),
        },
    );
    core.agents = vec![dead_codex_row("held-worker", "held-session", "/tmp")];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);

    core.command(1, Command::FocusPane(pid));

    assert!(core.panes.contains_key(&pid), "the held shell stays");
    assert!(
        core.held_workers.contains_key(&pid),
        "a gate refusal keeps the seat held for a later retry"
    );
    assert!(
        core.panes[&pid]
            .vt
            .text()
            .contains("was not resumed: spawn-gate: refused on max_live"),
        "the seat names the refusal: {}",
        core.panes[&pid].vt.text()
    );
    assert!(
        drain_notices(&mut rx)
            .join("\n")
            .contains("spawn-gate: refused on max_live"),
        "the client sees the verdict"
    );
    assert!(core.worker_pane.is_empty());
    core.reap_pane(pid);
}

#[test]
fn read_gate_answer_maps_the_three_answer_shapes() {
    assert!(read_gate_answer(r#"{"status":"admitted","gate_key":null}"#, "", "w1").is_ok());
    let refused = read_gate_answer(
        r#"{"status":"refused","receipt":{"reason":"max_live"}}"#,
        "spawn-gate: refused on max_live (7 of 7)",
        "w1",
    )
    .unwrap_err();
    assert!(
        refused.starts_with("spawn-gate: refused on max_live"),
        "{refused}"
    );
    assert!(
        refused.contains("FNO_SPAWN_GATE=0 fno agents resume w1"),
        "{refused}"
    );
    let fallback = read_gate_answer(
        r#"{"status":"refused","receipt":{"reason":"ram_floor"}}"#,
        "",
        "w1",
    )
    .unwrap_err();
    assert!(fallback.starts_with("ram_floor"), "{fallback}");
    let unparseable = read_gate_answer("not json", "", "w1").unwrap_err();
    assert!(
        unparseable.starts_with("spawn gate unavailable"),
        "{unparseable}"
    );
}

#[test]
fn gate_payload_carries_hold_false_and_the_pane_substrate() {
    let payload = gate_payload("w1", Some("k1"), Some("makers"), false, 42);
    assert_eq!(payload["mode"], "gate");
    assert_eq!(payload["hold"], false);
    assert_eq!(payload["substrate"], "pane");
    assert_eq!(payload["caller_session"], "k1");
    assert_eq!(payload["account"], "makers");
    assert_eq!(payload["holder_pid"], 42);
    let bare = gate_payload("w2", None, None, false, 7);
    assert!(bare["caller_session"].is_null());
    assert!(bare["account"].is_null());
}

#[test]
fn read_probe_headroom_reads_slots_cap_and_refusals() {
    let ok = read_probe_headroom(r#"{"verdict":"accepted","slots":5,"max_live":7}"#).unwrap();
    assert_eq!(ok.left, 2);
    assert_eq!(ok.slots, 5);
    assert_eq!(ok.cap, 7);
    let refused = read_probe_headroom(
        r#"{"verdict":"refused","reason":"ram_floor","message":"RAM floor breached"}"#,
    )
    .unwrap_err();
    assert_eq!(refused, "spawn gate: RAM floor breached");
    let unknown = read_probe_headroom(r#"{"verdict":"unknown","reason":"lane_count_unavailable"}"#)
        .unwrap_err();
    assert!(unknown.starts_with("spawn gate unreadable"), "{unknown}");
}
