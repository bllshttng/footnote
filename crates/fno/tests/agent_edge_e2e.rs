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

use fno::proto::{AgentBadge, AgentRow, Command as MuxCommand};

/// A hermetic agents home next to the mux dir; the server's registry reader
/// resolves it via `FNO_AGENTS_HOME` (inherited by the self-spawned server).
fn agents_home(scratch: &Scratch) -> PathBuf {
    scratch.0.join("agents-home")
}

fn worker_bin() -> PathBuf {
    static WORKER_BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    WORKER_BIN
        .get_or_init(|| {
            let target_dir = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fno-agents/target")
                });
            let path = target_dir.join("debug/fno-agents-worker");
            if !path.is_file() {
                let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
                let manifest =
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fno-agents/Cargo.toml");
                let status = Command::new(cargo)
                    .args(["build", "--manifest-path"])
                    .arg(manifest)
                    .args(["--bin", "fno-agents-worker"])
                    .status()
                    .expect("cargo builds the keeper worker");
                assert!(status.success(), "keeper worker build failed: {status}");
            }
            assert!(
                path.is_file(),
                "keeper worker binary missing: {}",
                path.display()
            );
            path
        })
        .clone()
}

fn pane(scratch: &Scratch, args: &[&str]) -> Output {
    pane_at(scratch, &scratch.0, args)
}

fn pane_at(scratch: &Scratch, mux: &PathBuf, args: &[&str]) -> Output {
    scratch
        .command()
        .args(["mux", "pane"])
        .args(args)
        .env("FNO_MUX_DIR", mux)
        .env("FNO_AGENTS_HOME", agents_home(scratch))
        .env("FNO_AGENTS_WORKER_BIN", worker_bin())
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn kill_server(scratch: &Scratch) {
    let _ = scratch.command().args(["mux", "kill-server"]).output();
}

struct ChildGuard(u32);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // SAFETY: this test owns the keeper-hosted child PID it recorded.
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGKILL);
        }
    }
}

fn pane_list_at(scratch: &Scratch, mux: &PathBuf) -> Vec<serde_json::Value> {
    let out = pane_at(scratch, mux, &["ls", "--json"]);
    assert!(
        out.status.success(),
        "pane ls stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout(&out)).expect("pane ls JSON")
}

fn kill_server_at(scratch: &Scratch, mux: &PathBuf) {
    let _ = scratch
        .command()
        .env("FNO_MUX_DIR", mux)
        .args(["mux", "kill-server"])
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
fn agent_edge_keeper_row_detach_restarts_and_reattaches_same_child() {
    let scratch = Scratch::new("agent_edge_keeper_detach");
    let mux = PathBuf::from(format!("/tmp/fno-k-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&mux);
    let dir = scratch.0.to_str().unwrap().to_string();
    let run = pane_at(
        &scratch,
        &mux,
        &[
            "run",
            "--worker",
            "keeper-worker",
            "--cwd",
            &dir,
            "--",
            "/bin/sh",
            "-c",
            "printf 'keeper-positive-marker\\n'; sleep 300",
        ],
    );
    assert!(
        run.status.success(),
        "worker run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr),
    );
    let original_pane: u64 = stdout(&run).parse().expect("worker pane id");
    let keeper_list = pane_at(&scratch, &mux, &["keeper", "list", "--json"]);
    assert!(keeper_list.status.success());
    let keeper_list: serde_json::Value = serde_json::from_str(&stdout(&keeper_list)).unwrap();
    assert!(
        keeper_list.as_array().unwrap().iter().any(|keeper| {
            keeper["argv"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "FNO_AGENT_SELF=keeper-worker")
        }),
        "keeper Identify carries the worker identity: {keeper_list}"
    );
    let listed = pane_list_at(&scratch, &mux);
    let child_pid = listed
        .iter()
        .find(|entry| entry["pane_id"] == original_pane)
        .and_then(|entry| entry["child_pid"].as_u64())
        .expect("pane ls positively reports the worker child pid") as u32;
    let _child = ChildGuard(child_pid);
    assert_eq!(
        unsafe { libc::kill(child_pid as libc::pid_t, 0) },
        0,
        "the recorded child is live before detach"
    );

    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"keeper-worker","provider":"codex","harness":"codex",
                 "harness_session_id":"keeper-session","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{original_pane}}}}}"#
        ),
    );
    let socket = mux.join("main.sock");
    let mut client = FakeClient::attach(&socket, 30, 100, &dir);
    wait_agents(&mut client, 10, "keeper pane row", |rows| {
        rows.iter().any(|row| {
            row.name == "keeper-worker" && row.pane_id == Some(original_pane) && !row.exited
        })
    });

    client.cmd(MuxCommand::DetachPane {
        pane: original_pane,
    });
    let detached = wait_agents(&mut client, 10, "live paneless detached row", |rows| {
        rows.iter().any(|row| {
            row.name == "keeper-worker"
                && row.pane_id.is_none()
                && !row.exited
                && row.no_pane_reason.is_some()
        })
    });
    let detached_row = detached
        .iter()
        .find(|row| row.name == "keeper-worker")
        .unwrap();
    assert_eq!(
        detached_row.no_pane_reason,
        Some(fno::proto::AgentNoPaneReason::LivePaneless),
        "the paneless interval has a positive live-session marker"
    );
    assert_eq!(
        unsafe { libc::kill(child_pid as libc::pid_t, 0) },
        0,
        "row detach leaves the keeper-owned child alive"
    );
    let stored = std::fs::read_to_string(agents_home(&scratch).join("squads.json"))
        .expect("detach writes the squad store");
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert!(
        stored["squads"][0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| { member["worker"] == "keeper-worker" && member["detached"] == true }),
        "stored worker identity and detached marker: {stored}"
    );

    client.detach();
    kill_server_at(&scratch, &mux);

    // A new pane boots a fresh server. Its startup sweep must re-adopt the
    // detached keeper before restore, without requiring the old pane id.
    let bootstrap = pane_at(
        &scratch,
        &mux,
        &["run", "--cwd", &dir, "--", "/bin/sh", "-c", "sleep 300"],
    );
    assert!(
        bootstrap.status.success(),
        "bootstrap run stderr: {:?}",
        String::from_utf8_lossy(&bootstrap.stderr)
    );
    let mut restarted = FakeClient::attach(&socket, 30, 100, &dir);
    wait_agents(&mut restarted, 15, "re-adopted live paneless row", |rows| {
        rows.iter()
            .any(|row| row.name == "keeper-worker" && row.pane_id.is_none() && !row.exited)
    });
    assert_eq!(
        unsafe { libc::kill(child_pid as libc::pid_t, 0) },
        0,
        "fresh server re-adoption preserves the same child"
    );

    restarted.cmd(MuxCommand::ResumeAgent {
        name: "keeper-worker".into(),
    });
    let reattached = wait_agents(&mut restarted, 10, "re-attached keeper row", |rows| {
        rows.iter()
            .any(|row| row.name == "keeper-worker" && row.pane_id.is_some() && !row.exited)
    });
    let reattached_pane = reattached
        .iter()
        .find(|row| row.name == "keeper-worker")
        .and_then(|row| row.pane_id)
        .expect("reattached row names its pane");
    let final_list = pane_list_at(&scratch, &mux);
    let final_pid = final_list
        .iter()
        .find(|entry| entry["pane_id"] == reattached_pane)
        .and_then(|entry| entry["child_pid"].as_u64())
        .expect("reattached pane reports a child pid");
    assert_eq!(
        final_pid, child_pid as u64,
        "reattach did not respawn the worker"
    );

    let _ = pane_at(&scratch, &mux, &["kill", &reattached_pane.to_string()]);
    restarted.detach();
    kill_server_at(&scratch, &mux);
    let _ = std::fs::remove_dir_all(&mux);
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
    scratch
        .command()
        .args(["mux", "block"])
        .args(args)
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
    // into a false green mid-test. NO client is attached here on purpose: the
    // server-side guard reads the registry FRESH per send, so it must refuse a
    // busy target even in a headless session where the overlay reader is parked
    // (the exact regression the guard would have had if it trusted self.agents).
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"busy-agent","provider":"claude","cwd":"{dir}","status":"live",
                 "mux":{{"session":"main","pane_id":{to}}},
                 "inside_leg":{{"state":"working","seq":1,
                                "received_at":"2026-07-02T00:00:00Z"}}}}"#
        ),
    );
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
    // --raw: this drives a `cat` pane and asserts the literal bytes reach the
    // grid, which is the byte-level interlock this test exists for. A default
    // send now envelopes for an agent recipient and refuses a pane no registry
    // row claims, so the enveloped lane is not the subject here (node x-3a64).
    let send = pane(
        &scratch,
        &["send", &id, "--text", "INJECTED-BYTES", "--raw"],
    );
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

/// x-2f03 end-to-end: N live claude sessions in the daemon roster (the real
/// bare-list shape `claude agents --json` emits) render as N sideline rows at
/// an attached client; terminal-catalog sessions (state done/stopped/failed)
/// do not render - roster presence means attachable. The harness isolates
/// `FNO_CLAUDE_DAEMON_DIR` to `<scratch>/iso-daemon`, so planting the fixture
/// there IS the live roster the server's reader polls; the hermetic registry
/// stays empty, so every live roster session must surface as a watch-only
/// foreign row (attach_id set, pane_id None) - no registry row to own or
/// suppress it.
#[test]
fn agent_edge_bare_list_roster_renders_every_live_session() {
    let raw = include_str!("testdata/roster-bare-list.json");
    let items: serde_json::Value = serde_json::from_str(raw).unwrap();
    let items = items.as_array().unwrap();
    let terminal = |v: &serde_json::Value| {
        v["state"]
            .as_str()
            .is_some_and(fno::agents_view::is_terminal_state)
    };
    let live: Vec<&serde_json::Value> = items.iter().filter(|v| !terminal(v)).collect();
    let n = live.len();
    assert!(
        n < items.len(),
        "capture must carry terminal items to prove they skip"
    );
    let expected: Vec<(String, String)> = live
        .iter()
        .map(|v| {
            let sid = v["sessionId"].as_str().unwrap();
            (
                v["name"].as_str().unwrap().to_string(),
                sid.split('-').next().unwrap().to_string(),
            )
        })
        .collect();

    let scratch = Scratch::new("agent_edge_bare_list_roster");
    let dir = scratch.home_cwd();

    // Plant the roster BEFORE the server boots: the reader's first tick then
    // parses the bare list from the start.
    let daemon_dir = scratch.0.join("iso-daemon");
    std::fs::create_dir_all(&daemon_dir).unwrap();
    std::fs::write(daemon_dir.join("roster.json"), raw).unwrap();

    // Boot the hermetic server (and a keeper pane so the session outlives
    // the attach).
    let run = pane(
        &scratch,
        &["run", "--cwd", &dir, "--", "/bin/sh", "-c", "sleep 300"],
    );
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );

    let mut client = FakeClient::attach(&scratch.main_sock(), 30, 100, &dir);

    // The marker: N sessions in the roster -> exactly N watch-only sideline
    // rows, each attachable under its own short id. Scoped to DEFAULT-account
    // rows (account: None): the planted iso-daemon roster is the default
    // daemon dir, while isolated-account rosters are a separate union source
    // (the host's real config can name one - x-c914 - and the harness does
    // not sandbox the PWD config candidate).
    let is_default_roster = |r: &AgentRow| r.attach_id.is_some() && r.account.is_none();
    let rows = wait_agents(&mut client, 15, "N roster session rows", |a| {
        a.iter().filter(|r| is_default_roster(r)).count() == n
    });
    let roster_rows: Vec<&AgentRow> = rows.iter().filter(|r| is_default_roster(r)).collect();
    assert_eq!(roster_rows.len(), n, "one row per roster session");
    for r in &roster_rows {
        assert_eq!(r.pane_id, None, "roster sessions are watch-only rows");
        assert!(!r.exited, "a roster-listed session is live/attachable");
    }
    let got: std::collections::BTreeSet<(String, String)> = roster_rows
        .iter()
        .map(|r| (r.name.clone(), r.attach_id.clone().unwrap_or_default()))
        .collect();
    let want: std::collections::BTreeSet<(String, String)> = expected.into_iter().collect();
    assert_eq!(got, want, "names and attach ids key off the capture");

    client.detach();
    kill_server(&scratch);
}
