//! Multi-client e2e (task 3.6): the Phase 3 brief's clamp, per-client-view,
//! session, and mux-verb ACs scripted through the socket seam (fake wire
//! clients with distinct viewports against the real headless server binary)
//! plus plain subprocess runs of the real binary for the mux verbs.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{spawn_server, Absorbed, FakeClient};
use fno::proto::Command;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-mc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn sock(&self) -> PathBuf {
        self.0.join("main.sock")
    }
    fn dir(&self, name: &str) -> PathBuf {
        let d = self.0.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sh_server(scratch: &Scratch) -> common::ServerProc {
    spawn_server(&scratch.sock(), &[("SHELL", "/bin/sh")])
}

/// Run the real `fno` binary with `FNO_MUX_DIR` pointed at this scratch (no
/// TTY - the mux verbs are plain subprocess surfaces).
fn fno_cmd(scratch: &Scratch, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(args)
        .env("FNO_MUX_DIR", &scratch.0)
        .output()
        .unwrap()
}

// -- AC1: two clients, same tab, different sizes ----------------------------

#[test]
fn multiclient_clamp_letterbox_area_and_regrow_on_abrupt_death() {
    let scratch = Scratch::new("clamp");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 40, 120, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    a.wait_layout(10, "solo area", |l| l.area == (40, 120));

    // B joins the same squad (same cwd) at 20x80: the shared tab clamps to
    // the smallest client (AC1-HP) for BOTH clients.
    let mut b = FakeClient::attach(&scratch.sock(), 20, 80, cwd.to_str().unwrap());
    let la = a.wait_layout(10, "a clamped", |l| l.area == (20, 80));
    let lb = b.wait_layout(10, "b clamped", |l| l.area == (20, 80));
    assert_eq!(la.panes, lb.panes, "co-viewers see identical rects");
    // The single pane fills the clamped area exactly.
    let rect = la.panes[0].1;
    assert_eq!((rect.rows, rect.cols), (20, 80));

    // Kernel-winsize proof + identical frames: both clients converge on the
    // same 20x80 grid content.
    a.input(b"echo sz=$(stty size)#\r");
    a.wait_pane_text(15, pane, |t| t.contains("sz=20 80#"));
    b.wait_pane_text(15, pane, |t| t.contains("sz=20 80#"));
    let fa = a.frames.get(&pane).unwrap();
    assert_eq!((fa.rows, fa.cols), (20, 80), "frame is the clamped grid");

    // AC1-ERR: the constraining client dies WITHOUT Detach (socket dropped
    // abruptly). The Gone path recomputes the clamp: the tab regrows to the
    // survivor's size within one layout-change cycle.
    drop(b);
    a.wait_layout(10, "regrown", |l| l.area == (40, 120));
    a.input(b"echo sz2=$(stty size)#\r");
    a.wait_pane_text(15, pane, |t| t.contains("sz2=40 120#"));
}

#[test]
fn multiclient_clamp_below_min_pane_size_recovers_exactly() {
    // AC1-EDGE: a 3-pane tab joined by a client whose viewport cannot fit
    // the tree at min pane size - no panic, no overlap, exact recovery when
    // the tiny client leaves.
    let scratch = Scratch::new("tiny");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    a.wait_layout(10, "first layout", |l| l.panes.len() == 1);
    a.cmd(Command::SplitH);
    a.wait_layout(10, "2 panes", |l| l.panes.len() == 2);
    a.cmd(Command::SplitV);
    let before = a.wait_layout(10, "3 panes", |l| l.panes.len() == 3);

    let mut b = FakeClient::attach(&scratch.sock(), 3, 12, cwd.to_str().unwrap());
    let lb = b.wait_layout(10, "tiny clamp", |l| l.area == (3, 12));
    // Degenerate but sane: no rect exceeds the area, no overlap panic
    // server-side (rects may saturate to zero size - the honest answer).
    for (_, r) in &lb.panes {
        assert!(
            r.y + r.rows <= 3 && r.x + r.cols <= 12,
            "rect out of area: {r:?}"
        );
    }

    // The tiny client leaves cleanly: the layout recovers EXACTLY (same
    // rects, same focus - ratios were never mutated by the clamp) and the
    // server stayed interactive through the squeeze.
    b.detach();
    drop(b);
    let after = a.wait_layout(10, "recovered", |l| l.area == (24, 80));
    assert_eq!(
        after.panes, before.panes,
        "exact recovery after the squeeze"
    );
    assert_eq!(after.focus, before.focus);
    a.input(b"echo alive#\r");
    a.wait_pane_text(15, after.focus, |t| t.contains("alive#"));
}

// -- AC2: two clients, different views --------------------------------------

#[test]
fn multiclient_independent_views_frames_for_viewers_only_and_reanchor() {
    let scratch = Scratch::new("views");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane_a = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    b.wait_layout(10, "b attached", |l| !l.panes.is_empty());

    // B opens tab 2: only B switches (AC2-HP); A stays on tab 1 and sees the
    // catalog grow without moving.
    b.cmd(Command::NewTab);
    let lb = b.wait_layout(10, "b on tab 2", |l| {
        l.squads.first().map(|s| (s.tabs.len(), s.active_tab)) == Some((2, 1))
    });
    let pane_b = lb.focus;
    assert_ne!(pane_b, pane_a);
    let la = a.wait_layout(10, "a sees 2 tabs", |l| {
        l.squads.first().map(|s| s.tabs.len()) == Some(2)
    });
    assert_eq!(la.squads[0].active_tab, 0, "a's view did not move");
    assert_eq!(la.focus, pane_a, "a's focus stays on its own tab");

    // Concurrent typing routes each client's input to ITS viewed tab's
    // focused pane (AC2-HP input half).
    a.input(b"echo tab-one-in#\r");
    b.input(b"echo tab-two-in#\r");
    a.wait_pane_text(15, pane_a, |t| t.contains("tab-one-in#"));
    b.wait_pane_text(15, pane_b, |t| t.contains("tab-two-in#"));

    // AC2-FR: a ticker floods B's tab; A (not viewing it) gets ZERO frames
    // for that pane while B streams.
    b.input(b"while true; do echo tick; sleep 0.2; done\r");
    b.wait_pane_text(10, pane_b, |t| t.contains("tick"));
    a.reset_counts();
    a.pump(Duration::from_millis(1200));
    assert_eq!(
        a.frame_counts.get(&pane_b).copied().unwrap_or(0),
        0,
        "frames must reach viewers only"
    );

    // Switching A onto the fed-but-unviewed tab yields an immediate frame.
    let tab2_id = la.squads[0].tabs[1].id;
    a.cmd(Command::SelectTab(tab2_id));
    a.wait(10, "immediate frame on switch", |c| {
        c.frames.contains_key(&pane_b).then_some(())
    });

    // AC2-ERR: B's viewed pane holds negotiated modes (mouse on); A closes
    // the shared tab 2. Both re-anchor to tab 1; B's reliable stream must
    // read ModeSync (mode reset) -> Layout -> frames, in that order, and
    // B's next keystroke lands in the re-anchored tab's focused pane.
    b.input(b"\x03");
    std::thread::sleep(Duration::from_millis(300));
    b.input(b"printf '\\033[?1000h\\033[?1006h'; cat\r");
    b.wait(10, "modes negotiated", |c| {
        c.modesyncs
            .iter()
            .any(|m| String::from_utf8_lossy(m).contains("?1000h"))
            .then_some(())
    });
    let start = {
        b.pump(Duration::from_millis(200));
        b.order.len()
    };
    a.cmd(Command::CloseTab); // a also views tab 2 by now
    let lb = b.wait_layout(10, "b re-anchored", |l| {
        l.squads.first().map(|s| s.tabs.len()) == Some(1) && l.focus == pane_a
    });
    assert_eq!(lb.squads[0].active_tab, 0);
    let tail = &b.order[start..];
    let sync_at = tail.iter().position(|e| *e == Absorbed::ModeSync);
    let layout_at = tail.iter().position(|e| *e == Absorbed::Layout);
    let frame_at = tail.iter().position(|e| matches!(e, Absorbed::Frame(_)));
    let (sync_at, layout_at) = (
        sync_at.expect("re-anchor away from moded pane must ModeSync"),
        layout_at.expect("re-anchor must send Layout"),
    );
    assert!(
        sync_at < layout_at,
        "ModeSync must precede Layout on re-anchor; got {tail:?}"
    );
    if let Some(frame_at) = frame_at {
        assert!(
            layout_at < frame_at,
            "frames follow the Layout; got {tail:?}"
        );
    }
    b.input(b"echo landed#\r");
    b.wait_pane_text(15, pane_a, |t| t.contains("landed#"));
}

// -- AC3: named sessions -----------------------------------------------------

#[test]
fn multiclient_named_session_sets_fno_session_in_panes() {
    // AC3-HP's pane-env half through the real server: a server on work.sock
    // derives its session name and every pane carries FNO_SESSION=work.
    let scratch = Scratch::new("named");
    let _server = spawn_server(&scratch.0.join("work.sock"), &[("SHELL", "/bin/sh")]);
    let cwd = scratch.dir("w");
    let mut c = FakeClient::attach(&scratch.0.join("work.sock"), 24, 80, cwd.to_str().unwrap());
    let pane = c
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    c.input(b"echo S=$FNO_SESSION#\r");
    c.wait_pane_text(15, pane, |t| t.contains("S=work#"));
}

// -- AC4: mux ls / kill-server ------------------------------------------------

/// Leave a stale socket behind: bind a listener, then drop it (the file
/// survives, nothing listens) - byte-identical to a SIGKILLed server.
fn plant_stale_socket(path: &Path) {
    let l = std::os::unix::net::UnixListener::bind(path).unwrap();
    drop(l);
    assert!(path.exists(), "stale socket must remain for the test");
}

#[test]
fn multiclient_mux_ls_reports_live_counts_and_stale_without_unlinking() {
    let scratch = Scratch::new("ls");
    let _server = sh_server(&scratch); // session "main"
    let cwd = scratch.dir("w");
    let mut c = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    c.wait_layout(10, "attached", |l| l.panes.len() == 1);
    plant_stale_socket(&scratch.0.join("dead.sock"));

    let out = fno_cmd(&scratch, &["mux", "ls"]);
    assert!(out.status.success(), "ls exits 0: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("main: 1 clients, 1 squads, 1 panes"),
        "live row with counts; got: {stdout}"
    );
    assert!(stdout.contains("dead: stale"), "stale row; got: {stdout}");
    assert!(
        scratch.0.join("dead.sock").exists(),
        "ls is read-only: it never unlinks (AC4-HP)"
    );
    // The probed live session is unharmed: the client still works.
    let pane = c.focus();
    c.input(b"echo still-alive#\r");
    c.wait_pane_text(15, pane, |t| t.contains("still-alive#"));
}

#[test]
fn multiclient_kill_server_live_stale_and_missing() {
    let scratch = Scratch::new("kill");
    let mut server = sh_server(&scratch); // session "main"
    let cwd = scratch.dir("w");
    let mut c = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = c.wait_layout(10, "attached", |l| l.panes.len() == 1).focus;
    // Learn the pane child's PID so AC4-FR (children do not outlive the
    // kill) is observable from outside. Skip the echoed command line (where
    // `$$` is still literal); take the first occurrence that parses.
    fn extract_pid(t: &str) -> Option<i32> {
        t.split("pid=")
            .skip(1)
            .find_map(|s| s.split('#').next().and_then(|v| v.trim().parse().ok()))
    }
    c.input(b"echo pid=$$#\r");
    let child_pid: i32 = c.wait(15, "pane pid", |c| extract_pid(&c.pane_text(pane)));

    // Live kill: exit 0, the attached client is Byed, the socket vanishes,
    // the server process exits, and the pane child is dead (AC4-UI/FR).
    let out = fno_cmd(&scratch, &["mux", "kill-server", "main"]);
    assert!(out.status.success(), "live kill exits 0: {out:?}");
    c.pump(Duration::from_millis(500));
    assert!(
        c.byes.iter().any(|r| r.contains("killed")),
        "attached client must receive Bye(killed); got {:?}",
        c.byes
    );
    assert!(!scratch.sock().exists(), "socket must be unlinked");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if server.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server must exit after kill-server"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        // kill(pid, 0) errors with ESRCH once the child is fully gone.
        if unsafe { libc::kill(child_pid, 0) } != 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pane child {child_pid} must not outlive kill-server (AC4-FR)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Stale kill: unlinked with a message, exit 0 (AC4-EDGE).
    plant_stale_socket(&scratch.0.join("work.sock"));
    let out = fno_cmd(&scratch, &["mux", "kill-server", "work"]);
    assert!(out.status.success(), "stale kill exits 0: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stale"),
        "stale unlink says so: {out:?}"
    );
    assert!(!scratch.0.join("work.sock").exists());

    // Missing: "no server", exit 1.
    let out = fno_cmd(&scratch, &["mux", "kill-server", "nowhere"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "missing socket exits 1: {out:?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("no server"));
}

// -- AC3-UI: nested guard through the real client-under-PTY -------------------

#[test]
fn multiclient_nested_same_session_attach_refused_pre_raw_mode() {
    // Simulate "bare fno inside a pane of session main": FNO_SESSION=main in
    // the client's environment. The refusal must name the session and both
    // remedies, exit non-zero, and never enter raw mode / the alt screen.
    let scratch = common::Scratch::new("mc-nested");
    let mut h = common::ClientHarness::spawn_with(&scratch, &[("FNO_SESSION", "main")]);
    let status = h.wait_exit(15);
    assert!(
        !status.success(),
        "nested attach must refuse, got {status:?}"
    );
    let out = h.raw_output();
    assert!(out.contains("main"), "refusal names the session: {out}");
    assert!(
        out.contains("--session") && out.contains("unset FNO_SESSION"),
        "refusal names both remedies: {out}"
    );
    assert!(
        !out.contains("\x1b[?1049h"),
        "the terminal must never enter the alternate screen on a refusal"
    );
}
