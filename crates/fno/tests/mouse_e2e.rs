//! Mouse e2e over the real server loop (carveout from the Phase 5 G1
//! scroll/select/copy work): every mouse layer is unit-tested, but this is
//! the wire-level regression net - a FakeClient against the real headless
//! server, no PTY on the test side, following the multiclient_e2e.rs seam.
//!
//! Out of scope by plan: pass-through to a mouse-mode child (needs a real-PTY
//! harness to observe honestly) and any new product code.

mod common;

use std::path::PathBuf;

use common::{spawn_server, FakeClient};
use fno::proto::{MouseButton, MouseEvent, MouseKind};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-mouse-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fno::proto::ensure_private_dir(&dir).unwrap();
        Scratch(dir)
    }
    fn sock(&self) -> PathBuf {
        self.0.join("main.sock")
    }
    fn dir(&self, name: &str) -> PathBuf {
        let d = self.0.join(name);
        fno::proto::ensure_private_dir(&d).unwrap();
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

fn wheel_up() -> MouseEvent {
    MouseEvent {
        row: 0,
        col: 0,
        kind: MouseKind::WheelUp,
    }
}

/// Wait until the pane's shell has printed its first prompt. Writing to the
/// PTY before `sh` is ready to read gets the bytes echoed by the tty line
/// discipline but never executed (the submitting `\r` is lost during startup),
/// so every test gates its FIRST input on this. The pinned prompt ends with
/// `$` (`sh-3.2$ ` on macOS bash-as-sh).
fn wait_prompt(c: &mut FakeClient, pane: u64) {
    c.wait_pane_text(15, pane, |t| t.trim_end().ends_with('$'));
}

/// Fill the pane with enough output that the wheel has history to scroll
/// into, and wait until the shell is provably done producing it.
fn fill_history(c: &mut FakeClient, pane: u64) {
    wait_prompt(c, pane);
    // 60 lines: comfortably past the 24-row viewport so a wheel-up has real
    // history to scroll into, without the load of a 200-iteration sh loop
    // (which under a saturated box crawls and blows the timeout).
    c.input(b"i=0; while [ $i -lt 60 ]; do echo hist-$i; i=$((i+1)); done; echo filled#\r");
    // Wait on the loop's OWN output, not "filled#" alone: the echoed command
    // line already carries "filled#" the instant it is typed, so keying on it
    // would return before the loop produced any scrollback. "hist-59" only
    // exists once the loop's last iteration ran.
    c.wait_pane_text(15, pane, |t| t.contains("hist-59") && t.contains("filled#"));
}

// -- AC1-HP + AC-EDGE: wheel scroll is shared state every co-viewer sees ------

#[test]
fn mouse_wheel_scrolls_history_and_coviewer_sees_same_offset() {
    let scratch = Scratch::new("wheel");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    fill_history(&mut a, pane);

    // B co-views the same pane (same cwd -> same squad) at the same size.
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    b.wait_pane_text(10, pane, |t| t.contains("filled#"));

    // Live bottom before the wheel: offset 0 on the frames both hold.
    assert_eq!(a.frames.get(&pane).unwrap().scroll_offset, 0);

    // A wheels up: the next broadcast Frame carries scroll_offset > 0 (US1)
    // and the co-viewer receives the SAME scrolled frame (shared viewport).
    a.mouse(pane, wheel_up());
    let off_a = a.wait(10, "a scrolled frame", |c| {
        c.frames
            .get(&pane)
            .map(|f| f.scroll_offset)
            .filter(|o| *o > 0)
    });
    let off_b = b.wait(10, "b scrolled frame", |c| {
        c.frames
            .get(&pane)
            .map(|f| f.scroll_offset)
            .filter(|o| *o > 0)
    });
    assert_eq!(off_a, off_b, "co-viewers share one scroll offset");

    // While scrolled, the live cursor is hidden (tmux copy-mode behavior).
    assert!(!a.frames.get(&pane).unwrap().cursor_visible);

    // A keystroke snaps back to the live bottom (Invariant: input always
    // lands on the visible line) - for every viewer.
    a.input(b"echo back#\r");
    a.wait_pane_text(15, pane, |t| t.contains("back#"));
    assert_eq!(a.frames.get(&pane).unwrap().scroll_offset, 0);
    b.wait(10, "b back to live", |c| {
        c.frames
            .get(&pane)
            .map(|f| f.scroll_offset)
            .filter(|o| *o == 0)
    });
}

// -- AC2-HP: drag + release auto-copies to the initiating client only ---------

#[test]
fn mouse_drag_release_copies_selection_to_initiator_only() {
    let scratch = Scratch::new("copy");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    b.wait_layout(10, "b attached", |l| !l.panes.is_empty());

    wait_prompt(&mut a, pane);
    a.input(b"echo copy-me-payload#\r");
    let text = a.wait_pane_text(15, pane, |t| {
        t.lines().any(|l| l.starts_with("copy-me-payload#"))
    });
    // The OUTPUT line starts at col 0; the echoed command line above it is
    // prompt-prefixed, so starts_with disambiguates the two.
    let row = text
        .lines()
        .position(|l| l.starts_with("copy-me-payload#"))
        .unwrap() as u16;

    // Left press, drag across the payload, release: the server extracts the
    // selection and ships ServerMsg::Copy to the initiating client (US2,
    // Warp release-to-copy behavior).
    a.mouse(
        pane,
        MouseEvent {
            row,
            col: 0,
            kind: MouseKind::Press(MouseButton::Left),
        },
    );
    a.mouse(
        pane,
        MouseEvent {
            row,
            col: 14,
            kind: MouseKind::Drag(MouseButton::Left),
        },
    );
    a.mouse(
        pane,
        MouseEvent {
            row,
            col: 14,
            kind: MouseKind::Release(MouseButton::Left),
        },
    );
    let copied = a.wait(10, "copy payload", |c| c.copies.first().cloned());
    // Exact, not a substring: cols 0..=14 on the output line is precisely
    // "copy-me-payload" (the `#` sits at col 15). A substring check would pass
    // even if the selection had bled into the prompt-prefixed command line
    // above or over-run its right edge.
    assert_eq!(
        copied, "copy-me-payload",
        "copy carries exactly the dragged cells"
    );

    // The copy is a reply to the gesture, not a broadcast: the co-viewer's
    // clipboard is untouched. Prove it with a causal barrier, not a timer: a
    // emits a marker and we absorb b's stream until b renders it. Copy and
    // Frame share b's single ordered socket, so any (buggy) Copy broadcast to b
    // would have been read before this marker frame - if b.copies is still
    // empty here, none was ever sent.
    a.input(b"echo no-stray-copy#\r");
    b.wait_pane_text(15, pane, |t| t.contains("no-stray-copy#"));
    assert!(
        b.copies.is_empty(),
        "copy must reach the initiator only; b got {:?}",
        b.copies
    );
}
