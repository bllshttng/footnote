//! (x-3cb3) The org fold against a real `fno` on PATH.
//!
//! The parse tests pin the payload shape; this pins the one thing only a real
//! subprocess can prove, which is that the fold reaches a live verb at all.
//! The deliberate-break half lives in `org_fold_degrade.rs`, in its own
//! binary, because it mutates process-global `FNO_BIN`.

use fno::org_overlay::fold_now;

/// `#[ignore]` because it shells out to whatever `FNO_BIN` names and takes a
/// `ps` snapshot plus a `macmon` sample. Run it deliberately:
/// `cargo test -p fno --test org_fold_live -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn the_fold_reads_a_live_machine() {
    let org = fold_now().await.expect("the fold answered");
    eprintln!("lane_count      {:?}", org.lane_count);
    eprintln!("refused_reason  {:?}", org.refused_reason);
    eprintln!("census          {:?}", org.census);
    // A live read must carry the machine arms, whatever their state.
    assert!(org.arm("spawn load").is_some(), "the load arm is present");
    if let (Some(leads), Some(workers), Some(rows)) =
        (org.census.leads, org.census.workers, org.census.roster_rows)
    {
        assert_eq!(leads + workers, rows, "the census counts add up");
    }
}
