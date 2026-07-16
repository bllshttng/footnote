//! Layout e2e (task 2.6): the brief's 10 verification items scripted through
//! the Phase-1 socket seam - fake wire clients against the real headless
//! server binary. No TTY anywhere; winsize truths come from `stty size`
//! typed into the panes themselves.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{connect_with_retry, spawn_server, FakeClient};
use fno::proto::{
    read_msg_sync, write_msg_sync, ClientMsg, Command, ControlVerb, PanePlacement, PaneTarget,
    ServerMsg, BUILD_VERSION, PROTO_VERSION,
};
use fno::tree::Dir;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-layout-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn sock(&self) -> PathBuf {
        self.0.join("s.sock")
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

/// Attach + settle on the first single-pane Layout. Returns (client, pane id).
fn attach_settled(scratch: &Scratch, cwd: &Path) -> (FakeClient, u64) {
    let mut c = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let layout = c.wait_layout(10, "first layout", |l| l.panes.len() == 1);
    (c, layout.focus)
}

fn run_pane(scratch: &Scratch, cwd: &Path, placement: PanePlacement) -> Result<u64, String> {
    let mut stream = connect_with_retry(&scratch.sock());
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write_msg_sync(
        &mut stream,
        &ClientMsg::Control {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.into(),
            verb: ControlVerb::PaneRun {
                cwd: cwd.to_string_lossy().into_owned(),
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                cols: None,
                rows: None,
                claim: false,
                placement,
            },
        },
    )
    .unwrap();
    match read_msg_sync(&mut stream).unwrap() {
        ServerMsg::PaneSpawned { pane_id } => Ok(pane_id),
        ServerMsg::Err { msg, .. } => Err(msg),
        other => panic!("unexpected pane-run reply: {other:?}"),
    }
}

/// `stty size` in the FOCUSED pane must report `rows cols`. Wire-level proof
/// that the kernel winsize followed the layout rect (items 1 and 3).
fn assert_focused_winsize(c: &mut FakeClient, pane: u64, rows: u16, cols: u16) {
    c.input(format!("echo sz=$(stty size)#\r").as_bytes());
    let want = format!("sz={rows} {cols}#");
    c.wait_pane_text(15, pane, |t| t.contains(&want));
}

#[test]
fn layout_e2e_pane_run_places_left_and_refuses_too_small_split() {
    let scratch = Scratch::new("placed-run");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");
    let (mut c, original) = attach_settled(&scratch, &cwd);

    let placed = run_pane(
        &scratch,
        &cwd,
        PanePlacement {
            target: PaneTarget::CurrentRoute,
            split: Some(Dir::Left),
            here: false,
        },
    )
    .unwrap();
    let layout = c.wait_layout(10, "directional pane-run", |l| {
        l.panes.len() == 2 && l.focus == placed
    });
    let placed_rect = layout.panes.iter().find(|(id, _)| *id == placed).unwrap().1;
    let original_rect = layout
        .panes
        .iter()
        .find(|(id, _)| *id == original)
        .unwrap()
        .1;
    assert!(placed_rect.x < original_rect.x);

    c.resize(24, 16);
    c.wait_layout(10, "narrow layout", |l| l.area == (24, 16));
    let before = c.layout.clone().unwrap();
    let squad_tabs_before = before
        .squads
        .iter()
        .find(|s| s.id == before.active_squad)
        .unwrap()
        .tabs
        .len();
    // x-9f75: a split refused at min-size no longer errors - it falls back to a
    // NEW TAB in the same squad (best-effort placement, never a dead-end). The
    // crowded tab keeps its panes; the squad gains a tab.
    run_pane(
        &scratch,
        &cwd,
        PanePlacement {
            target: PaneTarget::CurrentRoute,
            split: Some(Dir::Right),
            here: false,
        },
    )
    .unwrap();
    c.wait_layout(10, "split fallback adds a tab", |l| {
        l.squads
            .iter()
            .find(|s| s.id == l.active_squad)
            .map(|s| s.tabs.len())
            == Some(squad_tabs_before + 1)
    });
    // The viewed (crowded) tab's panes are untouched by the fallback.
    assert_eq!(c.layout.as_ref().unwrap().panes, before.panes);
}

// -- item 1: splits create live shells sized to their rects ---------------

#[test]
fn layout_e2e_split_h_and_v_yield_three_live_sized_shells() {
    let scratch = Scratch::new("splits");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));
    // The single pane owns the whole 24x80 content area.
    assert_focused_winsize(&mut c, pane_a, 24, 80);

    // Split H: 79 usable cols (1 divider), floor half = 39, last child 40.
    c.cmd(Command::SplitH);
    let l = c.wait_layout(10, "2-pane layout", |l| l.panes.len() == 2);
    let pane_b = l.focus;
    assert_ne!(pane_b, pane_a, "the new pane takes focus (AC1-HP)");
    assert_focused_winsize(&mut c, pane_b, 24, 40);

    // Split V on the focused right pane: 23 usable rows, 11 top / 12 bottom.
    c.cmd(Command::SplitV);
    let l = c.wait_layout(10, "3-pane layout", |l| l.panes.len() == 3);
    let pane_c = l.focus;
    assert_focused_winsize(&mut c, pane_c, 12, 40);

    // All three shells are independently interactive: the first pane still
    // answers after focus returns to it.
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "focus back on A", |l| l.focus == pane_a);
    assert_focused_winsize(&mut c, pane_a, 24, 39);
}

// -- item 2: geometric navigation on a 2x2 grid ----------------------------

#[test]
fn layout_e2e_2x2_grid_navigates_geometrically() {
    let scratch = Scratch::new("nav");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));

    c.cmd(Command::SplitH); // A | B
    let pane_b = c.wait_layout(10, "A|B", |l| l.panes.len() == 2).focus;
    c.cmd(Command::SplitV); // B stacks -> C below
    let pane_c = c.wait_layout(10, "B/C", |l| l.panes.len() == 3).focus;
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "focus A", |l| l.focus == pane_a);
    c.cmd(Command::SplitV); // A stacks -> D below
    let pane_d = c.wait_layout(10, "A/D", |l| l.panes.len() == 4).focus;

    // Grid: A top-left, D bottom-left, B top-right, C bottom-right.
    // AC2-HP: bottom-left -> right lands bottom-right, not tree order.
    c.cmd(Command::FocusDir(Dir::Right));
    c.wait_layout(10, "D -> right = C", |l| l.focus == pane_c);
    c.cmd(Command::FocusDir(Dir::Up));
    c.wait_layout(10, "C -> up = B", |l| l.focus == pane_b);
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "B -> left = A", |l| l.focus == pane_a);
    c.cmd(Command::FocusDir(Dir::Down));
    c.wait_layout(10, "A -> down = D", |l| l.focus == pane_d);

    // AC2-ERR: nothing below the bottom row - focus unchanged + a notice.
    let notices_before = c.notices.len();
    c.cmd(Command::FocusDir(Dir::Down));
    c.wait(10, "edge-nav notice", |c| {
        (c.notices.len() > notices_before).then_some(())
    });
    assert_eq!(c.focus(), pane_d, "focus must not move off the edge");
}

// -- item 3: resize moves the divider, propagates winsize, coalesces -------

#[test]
fn layout_e2e_resize_propagates_winsize_and_burst_settles_exactly() {
    let scratch = Scratch::new("resize");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));
    c.cmd(Command::SplitH);
    c.wait_layout(10, "2 panes", |l| l.panes.len() == 2);
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "focus A", |l| l.focus == pane_a);

    // One step right: A 0.5 -> 0.55 of 79 usable = 43 cols.
    c.cmd(Command::ResizeDir(Dir::Right));
    let l = c.wait_layout(10, "divider moved", |l| {
        l.panes.iter().any(|(id, r)| *id == pane_a && r.cols == 43)
    });
    // Exact tiling survives: widths + divider == 80 (Boundaries).
    let total: u16 = l.panes.iter().map(|(_, r)| r.cols).sum();
    assert_eq!(total + 1, 80, "rects + divider must tile exactly");
    assert_focused_winsize(&mut c, pane_a, 24, 43);

    // AC3-FR: a resize-key burst settles on the exact final ratios.
    for _ in 0..4 {
        c.cmd(Command::ResizeDir(Dir::Right));
    }
    // 0.55 + 4*0.05 = 0.75 -> floor(79*0.75) = 59.
    let l = c.wait_layout(10, "burst settled", |l| {
        l.panes.iter().any(|(id, r)| *id == pane_a && r.cols == 59)
    });
    let total: u16 = l.panes.iter().map(|(_, r)| r.cols).sum();
    assert_eq!(total + 1, 80);
    assert_focused_winsize(&mut c, pane_a, 24, 59);
}

// -- item 4: close redistributes proportionally + geometric re-anchor ------

#[test]
fn layout_e2e_close_middle_redistributes_and_focus_survives() {
    let scratch = Scratch::new("close");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));
    c.cmd(Command::SplitH); // [A .5, B .5]
    let pane_b = c.wait_layout(10, "2 panes", |l| l.panes.len() == 2).focus;
    c.cmd(Command::SplitH); // same-axis insert: [A .5, B .25, C .25]
    let pane_c = c.wait_layout(10, "3 panes", |l| l.panes.len() == 3).focus;

    c.cmd(Command::FocusDir(Dir::Left)); // the middle pane (B)
    c.wait_layout(10, "focus middle", |l| l.focus == pane_b);
    c.cmd(Command::ClosePane);
    // AC4-HP: B's quarter redistributes proportionally -> A 2/3, C 1/3 of
    // 79 usable: floor(79*2/3)=52, C takes the remainder 27.
    let l = c.wait_layout(10, "middle closed", |l| l.panes.len() == 2);
    let a = l.panes.iter().find(|(id, _)| *id == pane_a).unwrap().1;
    let cc = l.panes.iter().find(|(id, _)| *id == pane_c).unwrap().1;
    assert_eq!((a.cols, cc.cols), (52, 27), "proportional redistribution");
    assert!(
        l.focus == pane_a || l.focus == pane_c,
        "focus re-anchors to a geometric survivor"
    );
    // AC4-ERR half: input still lands (no dangling focus after the close).
    assert_focused_winsize(&mut c, l.focus, 24, if l.focus == pane_a { 52 } else { 27 });
}

// -- item 5: tabs - inactive tab is wire-silent, grid stays live -----------

#[test]
fn layout_e2e_inactive_tab_sends_no_frames_but_grid_updates() {
    let scratch = Scratch::new("tabs");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));

    // A ticker in tab 1 keeps producing output forever.
    c.input(b"while true; do echo tick; sleep 0.2; done\r");
    c.wait_pane_text(10, pane_a, |t| t.contains("tick"));

    // New tab: fresh full-window shell, tab bar catalog grows (AC5-HP).
    c.cmd(Command::NewTab);
    let l = c.wait_layout(10, "tab 2 active", |l| {
        l.squads.first().map(|s| (s.tabs.len(), s.active_tab)) == Some((2, 1)) && l.panes.len() == 1
    });
    let pane_b = l.focus;
    assert_ne!(pane_b, pane_a);
    assert_focused_winsize(&mut c, pane_b, 24, 80);

    // AC5-EDGE: while tab 2 is active, tab 1's flooding pane crosses the
    // wire ZERO times...
    c.reset_counts();
    c.pump(Duration::from_millis(1500));
    assert_eq!(
        c.frame_counts.get(&pane_a).copied().unwrap_or(0),
        0,
        "inactive tab's pane must send no frames"
    );

    // ...but its grid kept updating server-side: switching back shows a
    // fresh full frame with the still-running ticker (AC5-HP round-trip).
    // v3: SelectTab names the stable TabId from the catalog, not an index.
    let tab1_id = c.layout.as_ref().unwrap().squads[0].tabs[0].id;
    c.cmd(Command::SelectTab(tab1_id));
    c.wait_layout(10, "tab 1 active again", |l| {
        l.squads.first().map(|s| s.active_tab) == Some(0) && l.focus == pane_a
    });
    c.wait_pane_text(10, pane_a, |t| t.contains("tick"));
}

// -- item 6: flood isolation across sibling panes ---------------------------

#[test]
fn layout_e2e_flooded_pane_never_starves_its_sibling() {
    let scratch = Scratch::new("flood");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));
    c.cmd(Command::SplitH);
    let pane_b = c.wait_layout(10, "2 panes", |l| l.panes.len() == 2).focus;

    // Flood the focused right pane hard.
    c.input(b"yes | head -500000; echo FLOOD-DONE\r");
    c.wait_pane_text(15, pane_b, |t| t.contains('y') || t.contains("FLOOD-DONE"));

    // AC2-EDGE (US1): typing in the sibling mid-flood stays live - per-pane
    // newest-wins means B coalesces without starving A.
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "focus A", |l| l.focus == pane_a);
    c.input(b"echo sibling-ok\r");
    c.wait_pane_text(15, pane_a, |t| t.contains("sibling-ok"));
}

// -- item 7: multi-pane reattach restores layout + content ------------------

#[test]
fn layout_e2e_multi_pane_multi_tab_reattach_restores_all_state() {
    let scratch = Scratch::new("reattach");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");
    let (mut c, pane_a) = attach_settled(&scratch, &cwd);
    // Tab 1: a split with distinct markers in both panes.
    c.cmd(Command::SplitH);
    let pane_b = c.wait_layout(10, "2 panes", |l| l.panes.len() == 2).focus;
    c.input(b"echo marker-right\r");
    c.wait_pane_text(10, pane_b, |t| t.contains("marker-right"));
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "focus A", |l| l.focus == pane_a);
    c.input(b"echo marker-left\r");
    c.wait_pane_text(10, pane_a, |t| t.contains("marker-left"));
    // Tab 2 (active at detach), its pane holding non-default terminal modes
    // - the focused-pane mode-restoration half of verification item 7.
    c.cmd(Command::NewTab);
    let l = c.wait_layout(10, "tab 2", |l| {
        l.squads.first().map(|s| s.tabs.len()) == Some(2) && l.panes.len() == 1
    });
    let pane_c = l.focus;
    // Bracketed paste is the mode probe: mouse-reporting modes are no longer
    // synced (Phase 5 - the client owns mouse capture, the server routes), so
    // this exercises the focused-pane mode replay with a mode that DOES sync.
    c.input(b"printf '\\033[?2004h'; cat\r");
    c.wait(10, "modes negotiated", |c| {
        c.modesyncs
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("?2004h"))
            .then_some(())
    });
    let before = c.layout.clone().unwrap();
    c.detach();
    drop(c);

    // Reattach from the same cwd: the SAME squad, ACTIVE TAB (tab 2), panes,
    // rects, and focus (AC5-FR); tab 1's content is intact when we return to
    // it (AC3-HP generalized); and the fresh client's terminal is synced to
    // the focused pane's negotiated modes BEFORE it draws (item 7's modes
    // half - a fresh terminal starts raw, so the sync must replay ?2004h).
    let mut c2 = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let after = c2.wait_layout(10, "reattach layout", |l| !l.panes.is_empty());
    assert_eq!(after, before, "layout must survive detach exactly");
    assert_eq!(after.focus, pane_c, "focus lands on tab 2's pane");
    assert_eq!(
        after.squads[0].active_tab, 1,
        "the active tab persists across detach (AC5-FR)"
    );
    assert!(
        c2.modesyncs
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("?2004h")),
        "reattach must sync the focused pane's modes to the fresh terminal; got {:?}",
        c2.modesyncs
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
    );
    // Back to tab 1: both panes' content survived server-side.
    let tab1_id = after.squads[0].tabs[0].id;
    c2.cmd(Command::SelectTab(tab1_id));
    c2.wait_layout(10, "tab 1 again", |l| l.panes.len() == 2);
    c2.wait_pane_text(10, pane_a, |t| t.contains("marker-left"));
    c2.wait_pane_text(10, pane_b, |t| t.contains("marker-right"));
}

// -- item 8: ModeSync follows focus between mode-divergent panes ------------

#[test]
fn layout_e2e_modesync_flips_with_focus_between_divergent_panes() {
    let scratch = Scratch::new("modes");
    let _server = sh_server(&scratch);
    let (mut c, pane_a) = attach_settled(&scratch, &scratch.dir("w"));
    c.cmd(Command::SplitH);
    let pane_b = c.wait_layout(10, "2 panes", |l| l.panes.len() == 2).focus;

    // Focused pane B negotiates BOTH bracketed paste (?2004, synced) AND mouse
    // reporting (?1000/?1006). Phase 5 (brief Locked 2): mouse modes are NOT
    // synced to the client - it owns capture and the server routes - so only
    // the paste mode reaches the client terminal.
    c.input(b"printf '\\033[?2004h\\033[?1000h\\033[?1006h'; cat\r");
    c.wait(10, "paste-on ModeSync", |c| {
        c.modesyncs
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("?2004h"))
            .then_some(())
    });
    // The mouse-mode enable must NEVER cross the wire, even though B negotiated it.
    assert!(
        !c.modesyncs
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("?1000h")),
        "mouse reporting must not sync (Phase 5); got {:?}",
        c.modesyncs
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
    );

    // Focus to the plain shell: the client terminal must RESET B's paste mode.
    c.modesyncs.clear();
    c.cmd(Command::FocusDir(Dir::Left));
    c.wait_layout(10, "focus A", |l| l.focus == pane_a);
    c.wait(10, "paste-off ModeSync", |c| {
        c.modesyncs
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("?2004l"))
            .then_some(())
    });

    // And back: B's paste mode re-applies.
    c.modesyncs.clear();
    c.cmd(Command::FocusDir(Dir::Right));
    c.wait_layout(10, "focus B", |l| l.focus == pane_b);
    c.wait(10, "paste-on again", |c| {
        c.modesyncs
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("?2004h"))
            .then_some(())
    });
}

// -- item 9: worktree rollup + hanging-git fallback --------------------------

#[test]
fn layout_e2e_worktree_attach_rolls_up_to_one_squad() {
    let scratch = Scratch::new("rollup");
    let _server = sh_server(&scratch);

    // A real repo + a worktree OUTSIDE it (the conductor layout), plus an
    // unrelated plain directory.
    let repo = scratch.dir("footnote");
    let git = |args: &[&str], cwd: &Path| {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap()
                .status
                .success(),
            "git {args:?} failed"
        );
    };
    git(&["init", "-q"], &repo);
    git(&["config", "user.email", "t@t"], &repo);
    git(&["config", "user.name", "t"], &repo);
    git(&["commit", "-q", "--allow-empty", "-m", "init"], &repo);
    let wt = scratch.0.join("workspaces").join("athens");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git(
        &["worktree", "add", "-q", wt.to_str().unwrap(), "HEAD"],
        &repo,
    );

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, repo.to_str().unwrap());
    a.wait_layout(10, "repo squad", |l| l.squads.len() == 1);
    // AC6-HP: the worktree attach lands in the EXISTING squad - catalog
    // still lists exactly one.
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, wt.to_str().unwrap());
    let l = b.wait_layout(10, "worktree layout", |l| !l.panes.is_empty());
    assert_eq!(l.squads.len(), 1, "worktree must roll up, not duplicate");
    assert_eq!(l.squads[0].name, "footnote");

    // A non-git cwd creates a SECOND, literal-path squad (AC6-ERR half) -
    // and its shell starts IN that directory, not wherever the long-lived
    // server process happens to live (codex P2: later squads must not
    // inherit the first client's cwd).
    let plain = scratch.dir("plain");
    let mut p = FakeClient::attach(&scratch.sock(), 24, 80, plain.to_str().unwrap());
    let l = p.wait_layout(10, "plain squad", |l| l.squads.len() == 2);
    assert_eq!(l.squads[1].name, "plain");
    let pane = l.focus;
    p.input(b"echo d=$(pwd)#\r");
    p.wait_pane_text(15, pane, |t| t.contains("/plain#"));
}

#[test]
fn layout_e2e_hanging_git_falls_back_within_the_timeout() {
    // AC6-ERR: `git` on PATH hangs forever; the attach must still land (a
    // literal-cwd squad) after the bounded 2s resolution timeout.
    let scratch = Scratch::new("githang");
    let stub_dir = scratch.dir("stub");
    let stub = stub_dir.join("git");
    std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The server's PATH sees only the hanging stub (plus nothing else that
    // matters - /bin/sh is spawned by absolute path).
    let _server = spawn_server(
        &scratch.sock(),
        &[("SHELL", "/bin/sh"), ("PATH", stub_dir.to_str().unwrap())],
    );

    let started = Instant::now();
    let mut c = FakeClient::attach(&scratch.sock(), 24, 80, scratch.0.to_str().unwrap());
    c.wait_layout(15, "fallback layout", |l| l.panes.len() == 1);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "attach took {:?}; the 2s git timeout must bound it",
        started.elapsed()
    );
}

// -- item 10: per-client SelectSquad + stale-id fail-closed ------------------

#[test]
fn layout_e2e_select_squad_is_per_client_and_stale_id_refused() {
    let scratch = Scratch::new("squadsel");
    let _server = sh_server(&scratch);
    let dir1 = scratch.dir("one");
    let dir2 = scratch.dir("two");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, dir1.to_str().unwrap());
    let sid1 = a
        .wait_layout(10, "squad one", |l| l.squads.len() == 1)
        .squads[0]
        .id;
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, dir2.to_str().unwrap());
    let l = b.wait_layout(10, "squad two active for b", |l| l.squads.len() == 2);
    let sid2 = l.active_squad;
    assert_ne!(sid1, sid2);

    // Phase 3 (AC2-HP squad half): b's attach grew the catalog but did NOT
    // move a's view - a's next Layout still shows squad one active.
    let l = a.wait_layout(10, "a sees the catalog grow", |l| l.squads.len() == 2);
    assert_eq!(l.active_squad, sid1, "a's view survives b's attach");

    // a switches itself to squad two; b's view must not move.
    a.cmd(Command::SelectSquad(sid2));
    a.wait_layout(10, "a on squad two", |l| l.active_squad == sid2);
    b.pump(Duration::from_millis(500));
    assert_eq!(
        b.layout.as_ref().unwrap().active_squad,
        sid2,
        "b's view is untouched by a's SelectSquad"
    );

    // AC6-FR: a dead/unknown id is refused fail-closed - notice + BEL, the
    // sender's view unchanged, no crash.
    let notices_before = a.notices.len();
    a.cmd(Command::SelectSquad(999_999));
    a.wait(10, "stale-id notice", |c| {
        (c.notices.len() > notices_before).then_some(())
    });
    assert_eq!(a.layout.as_ref().unwrap().active_squad, sid2);

    // Same fail-closed shape for a dead TabId (AC2-EDGE).
    let notices_before = a.notices.len();
    a.cmd(Command::SelectTab(999_999));
    a.wait(10, "stale-tab notice", |c| {
        (c.notices.len() > notices_before).then_some(())
    });
}
