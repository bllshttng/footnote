//! (x-3cb3) The court fold against a real `fno` on PATH.
//!
//! The parse tests pin the payload shape; this pins the one thing only a real
//! subprocess can prove, which is that the fold reaches a live verb at all.
//! The deliberate-break half lives in `court_fold_degrade.rs`, in its own
//! binary, because it mutates process-global `FNO_BIN`.

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
