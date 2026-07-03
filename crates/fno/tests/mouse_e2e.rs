//! Mouse e2e over the real server loop (carveout from the Phase 5 G1
//! scroll/select/copy work): every mouse layer is unit-tested, but this is
//! the wire-level regression net - a FakeClient against the real headless
//! server, no PTY on the test side, following the multiclient_e2e.rs seam.
//!
//! Out of scope by plan: pass-through to a mouse-mode child (needs a real-PTY
//! harness to observe honestly) and any new product code.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{spawn_server, FakeClient};
use fno::proto::{MouseButton, MouseEvent, MouseKind};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-mouse-{}-{name}", std::process::id()));
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

fn wheel_up() -> MouseEvent {
    MouseEvent {
        row: 0,
        col: 0,
        kind: MouseKind::WheelUp,
    }
}

/// Fill the pane with enough output that the wheel has history to scroll
/// into, and wait until the shell is provably done producing it.
fn fill_history(c: &mut FakeClient, pane: u64) {
    c.input(b"i=0; while [ $i -lt 200 ]; do echo hist-$i; i=$((i+1)); done; echo filled#\r");
    c.wait_pane_text(15, pane, |t| t.contains("filled#"));
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
    assert!(
        copied.contains("copy-me"),
        "copy carries the dragged text; got {copied:?}"
    );

    // The copy is a reply to the gesture, not a broadcast: the co-viewer's
    // clipboard is untouched.
    b.pump(Duration::from_millis(500));
    assert!(
        b.copies.is_empty(),
        "copy must reach the initiator only; b got {:?}",
        b.copies
    );
}
