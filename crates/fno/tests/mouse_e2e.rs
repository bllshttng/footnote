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

/// Fill the pane with enough output that the wheel has history to scroll
/// into, and wait until the shell is provably done producing it.
fn fill_history(c: &mut FakeClient, pane: u64) {
    c.wait_prompt(pane);
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

// -- x-a2d0: a plain click on a URL resolves it and ships it to the clicker ---

/// Press and release on the same cell: no drag, so the selection is empty and
/// the release lands in the plain-click arm.
fn click(c: &mut FakeClient, pane: u64, row: u16, col: u16) {
    for kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Release(MouseButton::Left),
    ] {
        c.mouse(pane, MouseEvent { row, col, kind });
    }
}

/// Echo a line and return the viewport row its OUTPUT landed on. The echoed
/// command line above it is prompt-prefixed, so `starts_with` picks the real
/// output, exactly as the copy test disambiguates the two.
fn echo_line(c: &mut FakeClient, pane: u64, line: &str) -> u16 {
    c.input(format!("echo {line}\r").as_bytes());
    let text = c.wait_pane_text(15, pane, |t| t.lines().any(|l| l.starts_with(line)));
    text.lines().position(|l| l.starts_with(line)).unwrap() as u16
}

#[test]
fn mouse_click_on_a_url_opens_it_for_the_clicking_client_only() {
    let scratch = Scratch::new("link");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    b.wait_layout(10, "b attached", |l| !l.panes.is_empty());

    a.wait_prompt(pane);
    // "link " is 5 chars, so the URL occupies cols 5..=35 and "end" follows it.
    let row = echo_line(&mut a, pane, "link https://example.com/pr/700 end");

    click(&mut a, pane, row, 10);
    let opened = a.wait(10, "opened link", |c| c.opened_links.first().cloned());
    assert_eq!(
        opened, "https://example.com/pr/700",
        "the whole URL, not the clicked word"
    );

    // Same causal barrier the copy test uses: OpenLink and Frame share b's one
    // ordered socket, so a (buggy) broadcast would have arrived before this
    // marker frame. Opening a browser on every co-viewer's desk would be the
    // worst possible way to get this wrong.
    a.input(b"echo no-stray-open#\r");
    b.wait_pane_text(15, pane, |t| t.contains("no-stray-open#"));
    assert!(
        b.opened_links.is_empty(),
        "the link must reach the clicker only; b got {:?}",
        b.opened_links
    );
}

#[test]
fn mouse_release_without_a_matching_press_never_opens_a_link() {
    // codex P2 on PR 702. The client hit-tests every mouse report independently
    // (client.rs `hit_test`) and forwards each to whatever pane the pointer is
    // over, so a drag begun in pane A and released over pane B delivers a BARE
    // RELEASE to pane B. Pane B has no selection of its own, so that release
    // used to read as a plain click there and launch a browser in the middle of
    // an ordinary cross-pane selection.
    //
    // A lone release is exactly the state pane B sees, which is what makes this
    // a faithful reproduction without a second pane. The second case covers the
    // within-pane sibling: a drag that happened to select nothing.
    let scratch = Scratch::new("link-press");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    a.wait_prompt(pane);
    let row = echo_line(&mut a, pane, "link https://example.com/pr/700 end");

    // 1. Release on the URL with no press at all (what pane B receives).
    a.mouse(
        pane,
        MouseEvent {
            row,
            col: 10,
            kind: MouseKind::Release(MouseButton::Left),
        },
    );
    // 2. Press on the URL, release on a DIFFERENT cell of it: a drag, not a click.
    a.mouse(
        pane,
        MouseEvent {
            row,
            col: 8,
            kind: MouseKind::Press(MouseButton::Left),
        },
    );
    a.mouse(
        pane,
        MouseEvent {
            row,
            col: 20,
            kind: MouseKind::Release(MouseButton::Left),
        },
    );

    // Causal barrier rather than a sleep: OpenLink and Frame share one ordered
    // socket, so any open would have landed before this marker frame.
    a.input(b"echo no-stray-open-2#\r");
    a.wait_pane_text(15, pane, |t| t.contains("no-stray-open-2#"));
    assert!(
        a.opened_links.is_empty(),
        "a release with no matching same-cell press must not open: {:?}",
        a.opened_links
    );

    // The real click still works, so the guard did not just disable the feature.
    click(&mut a, pane, row, 10);
    let opened = a.wait(10, "opened link", |c| c.opened_links.first().cloned());
    assert_eq!(opened, "https://example.com/pr/700");
}

#[test]
fn mouse_click_off_a_url_opens_nothing_and_a_drag_still_copies() {
    let scratch = Scratch::new("link-neg");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    a.wait_prompt(pane);
    let row = echo_line(&mut a, pane, "link https://example.com/pr/700 end");

    // Col 1 is inside the leading "link" word, col 33 inside the trailing "end".
    click(&mut a, pane, row, 1);
    click(&mut a, pane, row, 33);

    // A DRAG across the URL must still copy, not open: the two gestures share
    // the release and would collide if opening were wired to the wrong one.
    for (col, kind) in [
        (5, MouseKind::Press(MouseButton::Left)),
        (30, MouseKind::Drag(MouseButton::Left)),
        (30, MouseKind::Release(MouseButton::Left)),
    ] {
        a.mouse(pane, MouseEvent { row, col, kind });
    }
    let copied = a.wait(10, "copy payload", |c| c.copies.first().cloned());
    assert_eq!(copied, "https://example.com/pr/700", "drag copies the URL");
    assert!(
        a.opened_links.is_empty(),
        "neither an off-URL click nor a drag may open anything; got {:?}",
        a.opened_links
    );
}

// -- v56, hover affordance: the lookup reaches the requester only -------------

#[test]
fn hover_link_lookup_returns_the_span_to_the_requester_only() {
    let scratch = Scratch::new("hover-link");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    b.wait_layout(10, "b attached", |l| !l.panes.is_empty());

    a.wait_prompt(pane);
    // "link " is 5 chars, so the URL occupies cols 5..=35.
    let row = echo_line(&mut a, pane, "link https://example.com/pr/700 end");

    // Positive: a probe over the URL answers its whole visible span.
    a.link_hover(pane, row, 10, 1);
    let (_, _, cells) = a.wait(10, "hover span", |c| c.link_hovers.first().cloned());
    assert_eq!(
        cells.first(),
        Some(&(row, 5)),
        "the span starts at the URL's first cell"
    );
    assert_eq!(cells.len(), 26, "https://example.com/pr/700 is 26 cells");
    assert!(cells.contains(&(row, 30)) && !cells.contains(&(row, 4)));

    // Negative with a positive control in the same fixture: the "link" word
    // answers EMPTY cells (a miss clears the underline, it never wedges).
    a.link_hover(pane, row, 2, 2);
    let (_, _, miss) = a.wait(10, "hover miss", |c| {
        c.link_hovers.iter().find(|(_, s, _)| *s == 2).cloned()
    });
    assert!(
        miss.is_empty(),
        "a non-link cell answers no cells: {miss:?}"
    );

    // Same causal barrier the click test uses: a (buggy) broadcast would have
    // arrived before this marker frame on b's ordered socket.
    a.input(b"echo no-stray-hover#\r");
    b.wait_pane_text(15, pane, |t| t.contains("no-stray-hover#"));
    assert!(
        b.link_hovers.is_empty(),
        "the hover span must reach the requester only; b got {:?}",
        b.link_hovers
    );
}

#[test]
fn hover_link_refuses_a_pane_whose_app_owns_the_mouse() {
    // The click-ownership rule, applied to the affordance: once the pane's
    // app negotiated mouse reporting, its grid interaction belongs to the
    // app and the mux offers no hover affordance over it. Positive control
    // FIRST (same fixture, same cell): before the app takes the mouse, the
    // probe answers a span - so the refusal is the ownership verdict, not a
    // dead lookup.
    let scratch = Scratch::new("hover-mouse");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    a.wait_prompt(pane);
    let row = echo_line(&mut a, pane, "link https://example.com/pr/700 end");

    a.link_hover(pane, row, 10, 1);
    let (_, _, before) = a.wait(10, "span before", |c| c.link_hovers.first().cloned());
    assert!(!before.is_empty(), "control: the same cell yields a span");

    // Take the mouse in-app (?1000 clicks + ?1006 SGR), then re-probe. Key on
    // the printf's OUTPUT line (echo_line's trick), not a prompt: the screen
    // already ends in a prompt, so a prompt wait returns on the stale
    // pre-printf frame. The output marker only exists after the escapes were
    // parsed.
    a.input(b"printf '\\033[?1000h\\033[?1006h'; echo mouse-on#\r");
    a.wait_pane_text(15, pane, |t| t.lines().any(|l| l.starts_with("mouse-on#")));
    a.link_hover(pane, row, 10, 2);
    let (_, _, after) = a.wait(10, "span after", |c| {
        c.link_hovers.iter().find(|(_, s, _)| *s == 2).cloned()
    });
    assert!(after.is_empty(), "an app-owned grid answers no hover cells");
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

    a.wait_prompt(pane);
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
