//! `off_executor`, `directory_bytes_within`, and `resolve_reclaimed_bytes`
//! (x-4775). Extracted from daemon.rs's tests mod: daemon.rs is over file
//! budget and may only shrink.

use super::*;

#[tokio::test]
async fn off_executor_returns_the_value_on_a_current_thread_runtime() {
    // AC1-EDGE. `block_in_place` PANICS on a current-thread runtime, and
    // every other `#[tokio::test]` in this file is one. Assert the
    // sentinel the closure produces, not merely that nothing panicked: an
    // absence has three explanations and only one of them is this one.
    assert_eq!(
        off_executor(|| "ran-on-current-thread"),
        "ran-on-current-thread"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn off_executor_returns_the_value_on_a_multi_thread_runtime() {
    // The other half of the same pair: the branch the daemon actually
    // takes (`bin/daemon.rs` builds a multi-thread runtime) must also
    // produce the value, not just avoid panicking.
    assert_eq!(
        off_executor(|| "ran-on-multi-thread"),
        "ran-on-multi-thread"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rm_blocking_subprocess_work_does_not_stall_the_runtime() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // AC1-HP, and the regression this node exists for: `handle_rm_with`
    // shells out for up to ~435s, most of it after the registry row is
    // already gone. Run inline it owns a worker thread for that whole
    // span and every other verb queues behind it.
    //
    // `worker_threads = 1` is what makes this discriminating ONLY when
    // the call under test is itself a SPAWNED task, matching how the real
    // daemon dispatches every connection (`serve_connection` is spawned
    // in the accept loop, `crates/fno-agents/src/daemon.rs`). Calling
    // `handle_rm_with(...).await` directly in a test function body does
    // NOT prove this: `#[tokio::test]`'s body runs via `block_on` on the
    // thread that started the runtime, outside the worker pool, so it
    // never contends with the ticker task for that pool regardless of
    // `block_in_place` - measured directly against raw tokio (2026-09-07,
    // job tmp/tokio_probe): un-spawned, a plain `std::thread::sleep` in
    // the test body left the ticker unaffected (26 ticks over 300ms)
    // EVEN WITH EVERY `off_executor` WRAP REVERTED. Spawning the same
    // call showed the real effect: 0 ticks without `block_in_place`, 25
    // with it. So the call under test is spawned here, and the response
    // is read back through `.await` on that `JoinHandle`.
    let home = short_home("rmnostall");
    let mut row = claude_rm_row(
        "pane-worker",
        "aaadd033",
        "aaadd033-1111-2222-3333-444444444444",
    );
    row.short_id.clear();
    row.mux = Some(state::MuxRef {
        session: "work".into(),
        pane_id: 41,
    });
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let ctx = std::sync::Arc::new(test_ctx(home.clone(), PathBuf::from("fno-agents-worker")));
    let request = Request::new(1, "agent.rm", json!({"name": "pane-worker"}));
    let snapshots = claude_row_then_absent("aaadd033", "stopped");

    let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ticker = {
        let ticks = std::sync::Arc::clone(&ticks);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    // Let the ticker reach its first await before the blocking work starts.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let before = ticks.load(std::sync::atomic::Ordering::SeqCst);

    let rm_task = tokio::spawn(async move {
        handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &|_| Ok(()),
            // Stands in for the real chain (`fno ... reapable`, four git
            // calls, `git worktree remove`), compressed to a bound a test
            // can wait on.
            &|_, _| {
                std::thread::sleep(Duration::from_millis(300));
                Ok(true)
            },
            &|_, _| PaneProbe::Unknown,
        )
        .await
    });
    let response = rm_task.await.unwrap();

    let during = ticks.load(std::sync::atomic::Ordering::SeqCst) - before;
    ticker.abort();

    assert_eq!(response.result().unwrap()["removed"], true);
    // A non-blocking 300ms span ticks ~30 times at the 10ms interval.
    // `during > 0` alone is not discriminating: it is also satisfied by
    // ticks accrued in the brief setup BEFORE the blocking closure runs.
    // Require most of the expected count so the assertion can actually
    // fail on a regression.
    assert!(
        during >= 15,
        "expected ~30 ticks over a non-blocking 300ms span, got {during}; \
         the runtime served too little other work while rm blocked on a \
         single worker thread, so the blocking chain is back on the executor"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn resolve_reclaimed_bytes_reports_null_not_zero_for_a_removed_unmeasured_tree() {
    // AC2-EDGE at the result-payload level: a removed worktree whose
    // walk hit its budget must not report the same `0` a kept tree
    // reports for a real reason.
    assert_eq!(resolve_reclaimed_bytes(None, true, None), None);
}

#[test]
fn resolve_reclaimed_bytes_reports_the_measured_size_for_a_removed_tree() {
    // AC2-HP: a removed, successfully-measured tree reports the real size.
    assert_eq!(resolve_reclaimed_bytes(None, true, Some(4096)), Some(4096));
}

#[test]
fn resolve_reclaimed_bytes_reports_zero_for_a_kept_tree_regardless_of_measurement() {
    // A tree that was NOT removed keeps reporting `0`: that zero is true
    // (nothing was reclaimed) whether or not the walk happened to run.
    assert_eq!(resolve_reclaimed_bytes(None, false, None), Some(0));
    assert_eq!(resolve_reclaimed_bytes(None, false, Some(999)), Some(0));
}

#[test]
fn resolve_reclaimed_bytes_the_audit_override_wins_unconditionally() {
    assert_eq!(resolve_reclaimed_bytes(Some(7), true, None), Some(7));
    assert_eq!(resolve_reclaimed_bytes(Some(7), false, Some(99)), Some(7));
}

#[test]
fn directory_bytes_within_measures_a_small_tree_inside_its_budget() {
    // AC2-HP: a walk that completes inside the budget reports the real
    // measured size, unchanged from the unbounded version's behavior.
    let dir = std::env::temp_dir().join(format!("fno-rm-dirbytes-small-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("a.txt"), b"12345").unwrap();
    std::fs::write(dir.join("sub").join("b.txt"), b"1234567890").unwrap();

    let measured = directory_bytes_within(&dir, Duration::from_secs(10));

    assert_eq!(measured, Some(15));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn directory_bytes_within_returns_none_within_its_budget_on_a_stalled_walk() {
    // AC2-EDGE: a walk that cannot finish inside the budget returns
    // `None` (unmeasured), and does so WITHIN the budget - asserted on
    // elapsed wall-clock time, the positive marker, never on the mere
    // absence of a value.
    //
    // The deadline is checked once per DIRECTORY entered, not per file
    // (matching the real cost: `read_dir` + one `symlink_metadata` per
    // entry is what is actually expensive, not the addition). A flat
    // directory full of files gives the walk exactly one checkpoint, so
    // this needs breadth: many subdirectories, each recursion re-checking
    // the deadline.
    let dir = std::env::temp_dir().join(format!("fno-rm-dirbytes-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..5000 {
        std::fs::create_dir(dir.join(format!("d{i}"))).unwrap();
    }

    let start = std::time::Instant::now();
    let budget = Duration::from_millis(1);
    let measured = directory_bytes_within(&dir, budget);
    let elapsed = start.elapsed();

    assert_eq!(measured, None);
    assert!(
        elapsed < Duration::from_secs(2),
        "directory_bytes_within did not return within its budget: took {elapsed:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
