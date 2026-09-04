//! (x-3cb3) The court fold against a real `fno` on PATH, and against a
//! binary that cannot answer.
//!
//! The parse tests pin the payload shape; these pin the two things only a
//! real subprocess can prove: that the fold reaches a live verb at all, and
//! that a binary which refuses degrades to a named failure instead of
//! hanging or panicking.

use fno::court_overlay::fold_now;

/// `#[ignore]` because it shells out to whatever `FNO_BIN` names and takes a
/// `ps` snapshot plus a `macmon` sample. Run it deliberately:
/// `cargo test -p fno --test court_fold_live -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn the_fold_reads_a_live_machine() {
    let court = fold_now().await.expect("the fold answered");
    eprintln!("lane_count      {:?}", court.lane_count);
    eprintln!("refused_reason  {:?}", court.refused_reason);
    eprintln!("census          {:?}", court.census);
    // A live read must carry the machine arms, whatever their state.
    assert!(court.arm("spawn load").is_some(), "the load arm is present");
    if let (Some(kings), Some(workers), Some(rows)) = (
        court.census.kings,
        court.census.workers,
        court.census.roster_rows,
    ) {
        assert_eq!(kings + workers, rows, "the census counts add up");
    }
}

/// The deliberate break the panel must survive: a binary that exits nonzero
/// with no output. The fold degrades to `None`, which the overlay renders as
/// a named failure line rather than a blank panel.
#[tokio::test]
async fn a_binary_that_cannot_answer_degrades_rather_than_hangs() {
    // SAFETY: single-threaded test process; no other thread reads the env.
    unsafe { std::env::set_var("FNO_BIN", "/bin/false") };
    let start = std::time::Instant::now();

    let court = fold_now().await;

    assert!(court.is_none(), "a silent binary is a failed fold");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "the fold is bounded, it does not hang"
    );
}
