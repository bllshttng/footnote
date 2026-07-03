//! In-scrollback search e2e (x-e780, proto v12): the full leader+/ loop scripted
//! through the socket seam - two fake wire clients co-viewing one pane against
//! the real headless server. Asserts the shared jump + highlight reaches the
//! co-viewer, the counter reaches the initiator ONLY, n/N walk, no-match is a
//! visible zero, and clear drops the highlight everywhere.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{spawn_server, FakeClient};
use fno::proto::{cell_flags, BlockDir, Frame};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-search-{}-{name}", std::process::id()));
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

/// Cells carrying the v7 `SELECTED` highlight in a frame.
fn selected_cells(frame: &Frame) -> usize {
    frame
        .cells
        .iter()
        .filter(|c| c.flags & cell_flags::SELECTED != 0)
        .count()
}

#[test]
fn search_loop_shared_jump_initiator_counter_walk_and_clear() {
    let scratch = Scratch::new("loop");
    let _server = sh_server(&scratch);
    let cwd = scratch.dir("w");

    // A and B co-view the one pane of the one squad/tab.
    let mut a = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    let pane = a
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    let mut b = FakeClient::attach(&scratch.sock(), 24, 80, cwd.to_str().unwrap());
    b.wait_layout(10, "b attached", |l| !l.panes.is_empty());

    // Let the shell reach its first prompt before seeding input (a keystroke sent
    // before the child is reading races its startup); draining A here also primes
    // its frame state so the pane-text waits below settle promptly.
    a.pump(Duration::from_millis(1000));

    // Seed two output lines carrying "DEADLOCK". The `''` splits the token in the
    // ECHOED command line (`echo one DEAD''LOCK`) but the shell prints it whole
    // (`one DEADLOCK`), so a search for "deadlock" matches the two output lines
    // only - a deterministic total of 2, not inflated by the command echoes.
    a.input(b"echo one DEAD''LOCK\r");
    a.input(b"echo two DEAD''LOCK\r");
    a.wait_pane_text(15, pane, |t| t.matches("DEADLOCK").count() >= 2);
    b.wait_pane_text(15, pane, |t| t.matches("DEADLOCK").count() >= 2);

    // AC1-HP + AC3-EDGE: a case-insensitive search jumps + highlights. A alone
    // gets the counter; B (co-viewer) gets the shared highlight via a Frame.
    b.reset_counts();
    a.search_open(pane, "deadlock");
    let (rp, total, current) = a.wait(10, "A's SearchResult", |c| c.search_results.last().copied());
    assert_eq!(rp, pane, "result names the searched pane");
    assert_eq!(
        total, 2,
        "two output-line matches (command echoes excluded)"
    );
    assert!(
        (1..=total).contains(&current),
        "current in range: {current}"
    );

    // AC4: the co-viewer sees the highlight (a broadcast frame with SELECTED
    // cells) but never the counter (initiator-only).
    b.wait(10, "B sees the shared highlight", |c| {
        c.frames
            .get(&pane)
            .filter(|f| selected_cells(f) > 0)
            .map(|_| ())
    });
    assert!(
        b.search_results.is_empty(),
        "co-viewer must NOT receive the SearchResult counter"
    );

    // AC2-HP: n/N walk the matches; each step returns a fresh valid counter.
    let before = a.search_results.len();
    a.search_step(pane, BlockDir::Prev); // older
    let (_, t2, c2) = a.wait(10, "step Prev result", |c| {
        (c.search_results.len() > before).then(|| *c.search_results.last().unwrap())
    });
    assert_eq!(t2, 2);
    assert!((1..=t2).contains(&c2));
    let before = a.search_results.len();
    a.search_step(pane, BlockDir::Next); // newer
    a.wait(10, "step Next result", |c| {
        (c.search_results.len() > before).then_some(())
    });

    // AC1-ERR: a query with no occurrence is a visible zero, not silence.
    let before = a.search_results.len();
    a.search_open(pane, "zznope-no-such-token");
    let (_, t0, c0) = a.wait(10, "no-match result", |c| {
        (c.search_results.len() > before).then(|| *c.search_results.last().unwrap())
    });
    assert_eq!((t0, c0), (0, 0), "no matches -> total 0");

    // Clear drops the highlight for every co-viewer (panics if B never sees a
    // frame with the selection gone).
    b.reset_counts();
    a.search_clear(pane);
    b.wait(10, "B's highlight cleared", |c| {
        c.frames
            .get(&pane)
            .filter(|f| selected_cells(f) == 0)
            .map(|_| ())
    });
}
