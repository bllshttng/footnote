//! The off-loop board reader: the server's work-queue card source.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::backlog_view;
use crate::server::CoreMsg;
use crate::store_client;

/// The off-loop work-queue reader: the same 1s change-gated shape as the
/// registry reader, over the graph store. The gate is the keeper's mutation
/// counter (`store_client::version`), which bumps on every backlog mutation
/// (claim/close) and keeps moving after the SQLite flip, where a file mtime
/// would freeze (Risk 5). The 4M document read is skipped whenever the stamp
/// is unchanged.
///
/// External tracker backend: the graph store is not the authoritative
/// backend there, so there is no counter to gate on. The reader instead
/// executes the backend-neutral snapshot verb (`fno backlog status
/// --snapshot`) on a bounded refresh clock and caches its payload; both
/// modes feed the SAME ReaderState, so the last-good retention, the
/// stale-after-3-failures marker, and the pure derivations (cards, lanes,
/// prs, missions) are unchanged. A snapshot exec failure is a read failure
/// like any other - never a fallback to the graph store (which would
/// resurrect stale graph-only rows the external backend no longer owns).
pub(crate) fn spawn(
    core_tx: mpsc::Sender<CoreMsg>,
    client_count_rx: tokio::sync::watch::Receiver<usize>,
) {
    let path = backlog_view::graph_path();
    let external = backlog_view::external_backend_selected();
    let mut count_rx = client_count_rx.clone();
    tokio::spawn(async move {
        // The board's project scope, latched ONCE (x-20f1). The server
        // resolves nothing: the CLIENT read the config at spawn, from the
        // checkout the operator launched in, and passed the answer in the
        // env. Latched rather than re-read per tick, because a scope that
        // changed under a live board makes the card set unexplainable.
        // Changing it means `fno mux kill-server`, and `fno mux doctor`
        // reports what a fresh spawn latches.
        let (scope, why) = backlog_view::board_scope_from_spawn_env();
        eprintln!("fno mux: backlog board scope: {why}");
        let mut state = backlog_view::ReaderState::with_scope(scope);
        // The last-good claim sweep (x-54fa): `None` until the first
        // success (render un-overlaid), then only ever replaced by a
        // fresher success — a sweep failure keeps this tick's overlay.
        let mut last_live: Option<HashMap<String, String>> = None;
        let mut sweep_gate = backlog_view::SweepLogGate::default();
        // Due immediately: the first gated tick lands a fresh overlay.
        let mut sweep_next = tokio::time::Instant::now();
        // Snapshot-mode pacing: the current minted stamp and when the next
        // refresh is due. A fresh stamp is minted only when the window
        // opens, and the exec is attempted at most once per window (a
        // failed read never commits its stamp to ReaderState, so without
        // the attempted flag every 1s tick would re-exec during an
        // outage - the hot loop SNAPSHOT_REFRESH_SECS exists to bound).
        let mut snapshot_seq: i64 = 0;
        let mut snapshot_stamp: Option<(i64, u64)> = None;
        let mut next_refresh = tokio::time::Instant::now();
        let mut snapshot_attempted = false;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // Gate the per-tick claim-sweep SUBPROCESS + store read on
            // an attached client (x-4e30). This is the idle-CPU root fix:
            // an orphaned server with no viewer stops fork/exec'ing a whole
            // `fno-agents claim sweep` process every second. The
            // `changed()` arm is the 0->1 kick so the first attach's
            // overlay is not up to 1s stale (AC3-FR).
            tokio::select! {
                _ = tick.tick() => {}
                res = count_rx.changed() => {
                    if res.is_err() {
                        return; // Core dropped; server shutting down
                    }
                }
            }
            if *count_rx.borrow() == 0 {
                continue; // no viewer -> no sweep subprocess, no read
            }
            // The sweep is not mtime-gated (claims move without a graph
            // write) but it is paced: one fno-agents process per
            // SWEEP_EVERY, due at once on the first attach. Log lines
            // are flap-gated in SweepLogGate.
            if sweep_next <= tokio::time::Instant::now() {
                sweep_next = tokio::time::Instant::now() + backlog_view::SWEEP_EVERY;
                match backlog_view::run_claim_sweep().await {
                    Some(live) => {
                        if sweep_gate.success() {
                            eprintln!("fno mux: claim sweep recovered");
                        }
                        last_live = Some(live);
                    }
                    None => {
                        if sweep_gate.failure() {
                            eprintln!("fno mux: claim sweep failed; keeping last-good overlay");
                        }
                    }
                }
            }
            // Mode split. Graph mode: the store's mutation counter, read
            // through the keeper (`store_client::version`) instead of the
            // file's mtime+len - after the flip graph.json freezes, so
            // the counter is the signal that still moves (Risk 5). The
            // board document read rides the same keeper. Snapshot mode:
            // mint a fresh stamp on the refresh clock, exec the snapshot
            // verb when the stamp differs from the cached one.
            // `stamp != cached` is the single changed-signal both modes
            // share.
            let (stamp, raw) = if external {
                let now = tokio::time::Instant::now();
                if now >= next_refresh {
                    next_refresh = now + Duration::from_secs(backlog_view::SNAPSHOT_REFRESH_SECS);
                    // A monotonically minted window counter: each
                    // increment differs from every previously minted
                    // (and cached) stamp, so each refresh window reads
                    // as changed exactly once.
                    snapshot_seq += 1;
                    snapshot_stamp = Some((snapshot_seq, 0));
                    snapshot_attempted = false;
                }
                let stamp = snapshot_stamp;
                let changed = stamp != state.cached_stamp();
                let raw = if changed && !snapshot_attempted {
                    snapshot_attempted = true;
                    tokio::task::spawn_blocking(backlog_view::read_snapshot)
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                (stamp, raw)
            } else {
                let version_path = path.clone();
                let stamp = tokio::task::spawn_blocking(move || {
                    store_client::version(&version_path).ok().map(|v| (v, 0))
                })
                .await
                .ok()
                .flatten();
                let read_path = path.clone();
                let changed = stamp != state.cached_stamp();
                let raw = if changed {
                    tokio::task::spawn_blocking(move || {
                        store_client::nodes(
                            &read_path,
                            serde_json::json!({}),
                            None,
                            None,
                            true,
                            None,
                        )
                        .ok()
                        .map(|conn| {
                            serde_json::to_string(&serde_json::json!({
                                "entries": conn.nodes
                            }))
                            .ok()
                        })
                        .flatten()
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    None
                };
                (stamp, raw)
            };
            if let Some((queue, prs, missions)) = state.tick(stamp, move || raw, last_live.as_ref())
            {
                let holders = last_live.clone().unwrap_or_default();
                if core_tx
                    .send(CoreMsg::BacklogCards {
                        cards: queue.cards,
                        lanes: queue.lanes,
                        stale: queue.stale,
                        holders,
                        prs,
                        missions,
                    })
                    .await
                    .is_err()
                {
                    return; // core loop gone; the server is shutting down
                }
            }
        }
    });
}
