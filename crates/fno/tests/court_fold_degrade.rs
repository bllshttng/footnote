//! (x-3cb3) The deliberate break: a `fno` binary that cannot answer.
//!
//! This test sets `FNO_BIN`, which is PROCESS-GLOBAL. libtest runs the tests
//! in one integration binary on parallel threads, so an env-mutating test
//! sharing a binary with any other test is a race: the sibling inherits
//! `/bin/false` and fails, or wins the scheduling and makes this one read a
//! real payload. Hence its own file, which is its own binary, which is the
//! only structure that makes the mutation safe without a global lock or a
//! `--test-threads=1` invocation nobody remembers to type.

use fno::court_overlay::fold_now;

#[tokio::test]
async fn a_binary_that_cannot_answer_degrades_rather_than_hangs() {
    // SAFETY: this binary holds exactly one test, so no other thread can be
    // reading the environment while it is written. See the module note.
    unsafe { std::env::set_var("FNO_BIN", "/bin/false") };
    let start = std::time::Instant::now();

    let court = fold_now().await;

    assert!(court.is_none(), "a silent binary is a failed fold");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "the fold is bounded, it does not hang"
    );
}
