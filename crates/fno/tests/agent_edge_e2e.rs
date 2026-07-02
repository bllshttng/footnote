//! G2 agent-edge end-to-end (4a, brief US1/US2): spawn-to-badge round trip.
//!
//! Drives the Rust side of the agent edge against a live hermetic session
//! (own `FNO_MUX_DIR` + `FNO_AGENTS_HOME` tempdirs): a fake agent (script
//! child) runs as a mux pane, the REGISTRY FILE is stubbed directly for the
//! row + inside-leg report states (the daemon is not under test - the file
//! is the contract the server's reader parses), and the sideline rows arrive
//! in `Layout.agents` at a fake attached client. The Python spawn path
//! (registry write + `pane run`) is exercised by cli pytest (test_spawn_pane).

mod common;
use common::{FakeClient, Scratch};

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use fno::proto::{AgentBadge, AgentRow};

/// A hermetic agents home next to the mux dir; the server's registry reader
/// resolves it via `FNO_AGENTS_HOME` (inherited by the self-spawned server).
fn agents_home(scratch: &Scratch) -> PathBuf {
    scratch.0.join("agents-home")
}

fn pane(scratch: &Scratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["mux", "pane"])
        .args(args)
        .env("FNO_MUX_DIR", &scratch.0)
        .env("FNO_AGENTS_HOME", agents_home(scratch))
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn kill_server(scratch: &Scratch) {
    let _ = Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["mux", "kill-server"])
        .env("FNO_MUX_DIR", &scratch.0)
        .output();
}

/// Write the registry file the reader parses. Minimal rows: the reader is
/// tolerant by design (it needs name/cwd/status/mux/inside_leg, not the
/// whole schema).
fn write_registry(scratch: &Scratch, rows: &str) {
    let home = agents_home(scratch);
    std::fs::create_dir_all(&home).unwrap();
    let tmp = home.join("registry.json.tmp");
    std::fs::write(
        &tmp,
        format!(r#"{{"schema_version": 6, "agents": [{rows}]}}"#),
    )
    .unwrap();
    std::fs::rename(tmp, home.join("registry.json")).unwrap();
}

/// Wait (bounded) until the client's latest Layout satisfies `pred` over its
/// agent rows.
fn wait_agents(
    client: &mut FakeClient,
    secs: u64,
    what: &str,
    pred: impl Fn(&[AgentRow]) -> bool,
) -> Vec<AgentRow> {
    client.wait(secs, what, |c| {
        c.layout
            .as_ref()
            .map(|l| l.agents.clone())
            .filter(|a| pred(a))
    })
}

#[test]
fn agent_edge_spawn_to_badge_round_trip() {
    // The full lattice on one live pane: spawn (script child) -> registry row
    // with the mux ref -> liveness row in Layout -> inside-leg report ->
    // badge -> TTL'd stale report ages out -> child exit -> exited row that
    // a still-live report can never resurrect (AC2-*, AC1-UI).
    let scratch = Scratch::new("agent_edge_badge");
    let dir = scratch.0.to_str().unwrap().to_string();

    // A fake agent: a shell that sleeps (long enough to outlive assertions).
    let run = pane(
        &scratch,
        &["run", "--cwd", &dir, "--", "/bin/sh", "-c", "sleep 300"],
    );
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let pane_id: u64 = stdout(&run).parse().expect("machine-readable pane id");

    // A keeper pane: the attach client joins the EXISTING squad without
    // spawning a shell, so without this the agent pane's kill would end the
    // whole session (last-pane-exit rule) before the exited row can render.
    let keeper = pane(
        &scratch,
        &["run", "--cwd", &dir, "--", "/bin/sh", "-c", "sleep 300"],
    );
    assert!(keeper.status.success());

    // Registry row: mux-hosted in THIS session, no report yet.
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"fake-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{pane_id}}}}}"#
        ),
    );

    // Attach a fake client from the same cwd (same squad as the agent pane).
    let mut client = FakeClient::attach(&scratch.main_sock(), 30, 100, &dir);

    // AC1-UI: the spawned agent row is visible under its squad, liveness-only
    // (no badge until the first in-TTL report).
    let rows = wait_agents(&mut client, 10, "liveness row", |a| {
        a.iter()
            .any(|r| r.name == "fake-agent" && r.pane_id == Some(pane_id) && !r.exited)
    });
    let row = rows.iter().find(|r| r.name == "fake-agent").unwrap();
    assert_eq!(row.badge, None, "no report yet -> liveness-only");
    assert!(
        row.squad.is_some(),
        "pane-hosted row renders under its squad"
    );

    // AC2-HP: an inside-leg report (no ttl: never self-ages) -> fact badge.
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"fake-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{pane_id}}},
                 "inside_leg":{{"state":"blocked","seq":1,"reason":"perm prompt",
                                "received_at":"2026-07-02T00:00:00Z"}}}}"#
        ),
    );
    wait_agents(&mut client, 10, "blocked badge", |a| {
        a.iter().any(|r| {
            r.name == "fake-agent"
                && r.badge == Some(AgentBadge::Blocked)
                && r.reason.as_deref() == Some("perm prompt")
        })
    });

    // AC2-ERR: a TTL'd report whose stamp is ancient ages to liveness-only
    // (the hook died; the badge must not pin a stale `working`).
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"fake-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{pane_id}}},
                 "inside_leg":{{"state":"working","seq":2,
                                "received_at":"2020-01-01T00:00:00Z","ttl_ms":60000}}}}"#
        ),
    );
    wait_agents(&mut client, 10, "TTL lapse ages badge", |a| {
        a.iter()
            .any(|r| r.name == "fake-agent" && r.badge.is_none() && !r.exited)
    });

    // AC2-EDGE: the pane child exits while the registry still carries a
    // LIVE-TTL badge -> the row shows exited (fact beats report).
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"fake-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{pane_id}}},
                 "inside_leg":{{"state":"working","seq":3,
                                "received_at":"2026-07-02T00:00:00Z"}}}}"#
        ),
    );
    let kill = pane(&scratch, &["kill", &pane_id.to_string()]);
    assert!(kill.status.success());
    wait_agents(&mut client, 10, "exit beats badge", |a| {
        a.iter()
            .any(|r| r.name == "fake-agent" && r.exited && r.badge.is_none())
    });

    // AC2-FR: a NEWER still-live report for the dead pane's session never
    // resurrects the row - the pane set is authoritative.
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"fake-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{pane_id}}},
                 "inside_leg":{{"state":"working","seq":9,
                                "received_at":"2026-07-02T00:00:00Z"}}}}"#
        ),
    );
    // The row set is unchanged (still exited), so no new Layout may arrive;
    // pump then assert on the latest snapshot.
    client.pump(Duration::from_secs(3));
    let rows = client
        .layout
        .as_ref()
        .map(|l| l.agents.clone())
        .unwrap_or_default();
    let row = rows.iter().find(|r| r.name == "fake-agent").unwrap();
    assert!(
        row.exited,
        "a dead pane's row must never resurrect (AC2-FR)"
    );
    assert_eq!(row.badge, None);

    client.detach();
    kill_server(&scratch);
}

#[test]
fn agent_edge_watch_only_rows_match_squad_by_cwd_else_catch_all() {
    // AC5-FR (dual-run render half): non-pane agents (bg/headless/worker
    // rows - no mux ref) surface as watch-only rows, squad-matched by cwd;
    // an unmatched cwd lands in the catch-all (squad: None). A row hosted in
    // ANOTHER session is skipped entirely (that session's server owns it).
    let scratch = Scratch::new("agent_edge_watch_only");
    let dir = scratch.0.to_str().unwrap().to_string();

    // A pane to give the session a squad keyed to `dir`.
    let run = pane(
        &scratch,
        &["run", "--cwd", &dir, "--", "/bin/sh", "-c", "sleep 300"],
    );
    assert!(run.status.success());

    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"bg-here","provider":"claude","cwd":"{dir}","status":"live"}},
               {{"name":"bg-elsewhere","provider":"codex","cwd":"/nowhere/else","status":"exited"}},
               {{"name":"other-session","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"not-this-one","pane_id":1}}}}"#
        ),
    );

    let mut client = FakeClient::attach(&scratch.main_sock(), 30, 100, &dir);
    let rows = wait_agents(&mut client, 10, "watch-only rows", |a| {
        a.iter().any(|r| r.name == "bg-here") && a.iter().any(|r| r.name == "bg-elsewhere")
    });

    let here = rows.iter().find(|r| r.name == "bg-here").unwrap();
    assert!(
        here.squad.is_some(),
        "cwd-matched row renders under the squad"
    );
    assert_eq!(here.pane_id, None, "watch-only rows carry no pane");
    let elsewhere = rows.iter().find(|r| r.name == "bg-elsewhere").unwrap();
    assert_eq!(elsewhere.squad, None, "unmatched cwd -> catch-all");
    assert!(elsewhere.exited, "registry-exited row renders exited");
    assert!(
        !rows.iter().any(|r| r.name == "other-session"),
        "a row mux-hosted in another session is not this server's to render"
    );

    client.detach();
    kill_server(&scratch);
}
