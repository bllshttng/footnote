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

/// `fno mux block pipe ...` against the same hermetic session (x-fe8f).
fn block(scratch: &Scratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["mux", "block"])
        .args(args)
        .env("FNO_MUX_DIR", &scratch.0)
        .env("FNO_AGENTS_HOME", agents_home(scratch))
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs")
}

#[test]
fn agent_edge_block_pipe_reads_guards_and_lands() {
    // The block-pipe composition end to end: source block text lands in the
    // target pane's input (happy path, incl. the NotFound-registry -> proceed
    // branch and the JSON receipt); a nonexistent block propagates exit 14;
    // a working-badged target refuses with exit 15; --force overrides.
    let scratch = Scratch::new("agent_edge_block_pipe");
    let dir = scratch.0.to_str().unwrap().to_string();

    // Source pane: emit OSC 133 C/D markers around the output so it captures
    // ONE real typed, completed block (block pipe refuses markerless-implicit
    // and still-open blocks). It then sleeps so the pane stays live.
    let src = pane(
        &scratch,
        &[
            "run",
            "--cwd",
            &dir,
            "--",
            "/bin/sh",
            "-c",
            "printf '\\033]133;C\\ahi-from-a\\n\\033]133;D;0\\a'; sleep 300",
        ],
    );
    assert!(
        src.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&src.stderr)
    );
    let from = stdout(&src);
    // Target pane: `cat` echoes every byte that lands, so the grid proves it.
    let dst = pane(&scratch, &["run", "--cwd", &dir, "--", "/bin/cat"]);
    assert!(dst.status.success());
    let to = stdout(&dst);

    // Let the source's output reach its grid before reading the block.
    let settled = pane(
        &scratch,
        &["wait", &from, "--pattern", "hi-from-a", "--timeout", "10"],
    );
    assert_eq!(settled.status.code(), Some(10), "source output on the grid");

    // Happy path (no registry file at all -> the guard's NotFound branch).
    let piped = block(&scratch, &["pipe", "--from", &from, "--to", &to, "--json"]);
    assert!(
        piped.status.success(),
        "pipe stderr: {:?}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_str(&stdout(&piped)).unwrap();
    assert!(receipt["bytes"].as_u64().unwrap() > 0);
    assert_eq!(receipt["forced"], serde_json::json!(false));
    assert_eq!(
        receipt["block_seq"],
        serde_json::json!(0),
        "first typed block"
    );
    let landed = pane(
        &scratch,
        &["wait", &to, "--pattern", "hi-from-a", "--timeout", "10"],
    );
    assert_eq!(
        landed.status.code(),
        Some(10),
        "piped text lands in the target pane"
    );

    // A nonexistent block: EXIT_BLOCK_UNAVAILABLE propagates verbatim.
    let gone = block(
        &scratch,
        &["pipe", "--from", &from, "--to", &to, "--block", "99"],
    );
    assert_eq!(gone.status.code(), Some(14), "evicted/nonexistent block");

    // A working-badged agent on the target pane: the idle guard refuses with
    // the typed exit and the --force hint. No-ttl report, so it never decays
    // into a false green mid-test.
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"busy-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{to}}},
                 "inside_leg":{{"state":"working","seq":1,
                                "received_at":"2026-07-02T00:00:00Z"}}}}"#
        ),
    );
    // The guard is server-side now: it reads the server's own `self.agents`,
    // refreshed from the registry on an interval. Sync on the server observing
    // the busy badge (via an attached client's layout) before piping, else the
    // send can beat the ingest tick and land in the not-yet-busy pane.
    let to_id: u64 = to.parse().unwrap();
    let mut client = FakeClient::attach(&scratch.main_sock(), 30, 100, &dir);
    wait_agents(&mut client, 10, "server observes busy target", |a| {
        a.iter()
            .any(|r| r.pane_id == Some(to_id) && r.badge == Some(AgentBadge::Working))
    });
    let refused = block(&scratch, &["pipe", "--from", &from, "--to", &to]);
    assert_eq!(refused.status.code(), Some(15), "guard refuses busy target");
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        err.contains("--force"),
        "refusal names the override: {err:?}"
    );

    // --force bypasses the guard (and only the guard).
    let forced = block(&scratch, &["pipe", "--from", &from, "--to", &to, "--force"]);
    assert!(
        forced.status.success(),
        "forced pipe stderr: {:?}",
        String::from_utf8_lossy(&forced.stderr)
    );

    client.detach();
    kill_server(&scratch);
}

#[test]
fn agent_edge_inject_vs_typing_interlock() {
    // 4a-G3 (US3): while the relay holds a claimed agent pane, human Input
    // bounces with the `busy: relay` notice and PaneSend lands unbroken;
    // release (explicit or holder-death) lets typing resume. The pane runs
    // `cat` so every byte that actually reaches the PTY echoes on the grid.
    let scratch = Scratch::new("agent_edge_claim");
    let dir = scratch.0.to_str().unwrap().to_string();

    let run = pane(
        &scratch,
        &["run", "--claim", "--cwd", &dir, "--", "/bin/cat"],
    );
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let pane_id: u64 = stdout(&run).parse().unwrap();
    let id = pane_id.to_string();

    // The attach anchors to the squad's first tab, so Input targets the cat pane.
    let mut client = FakeClient::attach(&scratch.main_sock(), 30, 100, &dir);
    client.wait(10, "attach layout", |c| c.layout.as_ref().map(|_| ()));

    // A real killable holder process.
    let mut holder = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let claim = pane(&scratch, &["claim", &id, "--pid", &holder.id().to_string()]);
    assert!(
        claim.status.success(),
        "claim stderr: {:?}",
        String::from_utf8_lossy(&claim.stderr)
    );

    // AC3-UI: the keystroke bounces - notice arrives, nothing echoes.
    client.input(b"TYPED-DURING-CLAIM");
    client.wait(10, "busy notice", |c| {
        c.notices.iter().any(|n| n == "busy: relay").then_some(())
    });

    // The injection burst rides PaneSend and arrives unbroken.
    let send = pane(&scratch, &["send", &id, "--text", "INJECTED-BYTES"]);
    assert!(send.status.success());
    let text = client.wait(10, "injected bytes on the grid", |c| {
        c.frames
            .get(&pane_id)
            .map(fno::vt::frame_text)
            .filter(|t| t.contains("INJECTED-BYTES"))
    });
    assert!(
        !text.contains("TYPED-DURING-CLAIM"),
        "bounced keystrokes must never reach the pane: {text:?}"
    );

    // A second live holder is refused; re-acquire by the same pid is not.
    let steal = pane(
        &scratch,
        &["claim", &id, "--pid", &std::process::id().to_string()],
    );
    assert_eq!(steal.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&steal.stderr).contains("held by pid"),
        "steal stderr: {:?}",
        String::from_utf8_lossy(&steal.stderr)
    );

    // AC3-FR: holder death releases without any explicit release - typing
    // resumes on the next contested keystroke.
    holder.kill().unwrap();
    holder.wait().unwrap();
    client.input(b"TYPED-AFTER-DEATH");
    client.wait(10, "typing resumes after holder death", |c| {
        c.frames
            .get(&pane_id)
            .map(fno::vt::frame_text)
            .filter(|t| t.contains("TYPED-AFTER-DEATH"))
    });

    // Explicit release path: claim again, release, type.
    let claim2 = pane(
        &scratch,
        &["claim", &id, "--pid", &std::process::id().to_string()],
    );
    assert!(claim2.status.success());
    let rel = pane(&scratch, &["release", &id]);
    assert!(rel.status.success());
    client.input(b"TYPED-AFTER-RELEASE");
    client.wait(10, "typing resumes after release", |c| {
        c.frames
            .get(&pane_id)
            .map(fno::vt::frame_text)
            .filter(|t| t.contains("TYPED-AFTER-RELEASE"))
    });

    // AC3-EDGE: a general pane (no --claim) never consults the interlock.
    let general = pane(&scratch, &["run", "--cwd", &dir, "--", "/bin/cat"]);
    assert!(general.status.success());
    let gid = stdout(&general);
    let refused = pane(
        &scratch,
        &["claim", &gid, "--pid", &std::process::id().to_string()],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("not claim-eligible"),
        "general-pane claim stderr: {:?}",
        String::from_utf8_lossy(&refused.stderr)
    );

    client.detach();
    kill_server(&scratch);
}
