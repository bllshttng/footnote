//! Pane frame flow: counters, emission, and the x-a600 repaint gesture - moved
//! verbatim out of server.rs (file budget shrink). Parent helpers resolve
//! through the glob.
use super::*;

/// Rig: a Core with one live `/bin/cat` pane fed `bytes` through the real
/// pty-output drain path (our own channel stands in for the pty reader
/// thread). Returns the core and the pane id. The pane is tiny (2x4) on
/// purpose: a serialized Frame scales with cells, and a full-size grid
/// overflows the 4096-byte duplex the emission tests read only AFTER the
/// write returns, deadlocking the writer.
fn counter_core(bytes: &[u8]) -> (Core, u64) {
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(2, 4, "/tmp").expect("pane");
    core.panes.get_mut(&pid).unwrap().node = Some("x-deadbeef".into());
    core.panes.get_mut(&pid).unwrap().name = Some("peer".into());
    core.panes.get_mut(&pid).unwrap().cmd = Some("claude".into());
    let (tx, mut rx) = mpsc::channel::<(u64, Vec<u8>)>(8);
    tx.try_send((pid, bytes.to_vec())).unwrap();
    drop(tx);
    let mut first_out = HashSet::new();
    drain_pty_output(&mut core, &mut rx, None, &mut first_out);
    (core, pid)
}

#[test]
fn pane_counters_count_fed_bytes_bursts_and_cpu() {
    let (core, pid) = counter_core(b"hello");
    let c = &core.panes[&pid].stats;
    assert_eq!(c.bytes_in.load(Ordering::Relaxed), 5);
    assert_eq!(c.grid_updates.load(Ordering::Relaxed), 1);
    assert!(
        c.cpu_ns.load(Ordering::Relaxed) > 0,
        "feeding one burst must attribute nonzero handling time"
    );
}

#[test]
fn watcherless_pane_stamps_last_output_on_every_burst() {
    // (x-d401) `note_pane_output` returns early with no `pane wait`
    // subscriber, which is why `last_activity_age_s` read None for panes
    // running real workloads. The stamp lives on the drain path itself:
    // every fed burst advances it whether or not anyone watches. The
    // marker is the stamp's recency against a pre-feed Instant - only the
    // drain path can produce it.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let pid = core.spawn_pane(2, 4, "/tmp").expect("pane");
    let registered = core.panes[&pid].last_output;
    let (tx, mut rx) = mpsc::channel::<(u64, Vec<u8>)>(8);
    let mut first_out = HashSet::new();
    tx.try_send((pid, b"burst".to_vec())).unwrap();
    drop(tx);
    let before = Instant::now();
    drain_pty_output(&mut core, &mut rx, None, &mut first_out);
    assert!(
        core.panes[&pid].last_output >= before,
        "the drain path must stamp last_output with no watcher attached"
    );
    assert!(
        core.panes[&pid].last_output > registered,
        "a second burst must advance the registration stamp"
    );
}

#[test]
fn hidden_pane_is_fed_but_never_composites() {
    // No client is attached, so broadcast_pane's visible-gate returns
    // before vt.frame(): the counter set must show fed-but-never-
    // composited, the exact reading that separates feeding from display.
    let (core, pid) = counter_core(b"data");
    let c = &core.panes[&pid].stats;
    assert_eq!(c.frames_composited.load(Ordering::Relaxed), 0);
    assert_eq!(c.bytes_in.load(Ordering::Relaxed), 4);
}

#[test]
fn redraw_pane_reseeds_viewers_and_notices() {
    // (x-a600) The repaint gesture: a named pane is nudged and its frame
    // re-seeded to every VIEWING client (one composite per enqueue, the
    // broadcast_pane convention); a non-viewer holds nothing and gets no
    // notice beyond the sender's own.
    let (mut core, pid) = counter_core(b"x");
    let (tx9, mut rx9) = mpsc::channel::<ServerMsg>(8);
    let (tx10, mut rx10) = mpsc::channel::<ServerMsg>(8);
    let dirty9: DirtyMap = Arc::default();
    let dirty10: DirtyMap = Arc::default();
    let notify = Arc::new(Notify::new());
    core.clients.push(Client {
        id: 9,
        reliable_tx: tx9,
        dirty: dirty9.clone(),
        notify: notify.clone(),
        synced_modes: Modes::default(),
        view: (1, 1),
        visible: HashSet::from([pid]),
        dims: (24, 80),
        passive: false,
        last_press: None,
    });
    core.clients.push(Client {
        id: 10,
        reliable_tx: tx10,
        dirty: dirty10.clone(),
        notify: notify.clone(),
        synced_modes: Modes::default(),
        view: (1, 1),
        visible: HashSet::new(),
        dims: (24, 80),
        passive: false,
        last_press: None,
    });
    let base = core.panes[&pid]
        .stats
        .frames_composited
        .load(Ordering::Relaxed);
    core.command(9, Command::RedrawPane { pane: Some(pid) });
    assert!(
        dirty9.lock().unwrap().contains_key(&pid),
        "the viewer's slot holds a fresh frame"
    );
    assert!(
        dirty10.lock().unwrap().is_empty(),
        "a non-viewing client is never seeded"
    );
    assert_eq!(
        core.panes[&pid]
            .stats
            .frames_composited
            .load(Ordering::Relaxed),
        base + 1,
        "one composite per enqueue"
    );
    match rx9.try_recv() {
        Ok(ServerMsg::Notice { text }) => {
            assert!(text.contains("repaint requested"), "notice said {text}")
        }
        other => panic!("expected a repaint notice, got {other:?}"),
    }
    assert!(rx10.try_recv().is_err(), "no notice to the non-viewer");
}

#[test]
fn redraw_pane_on_a_gone_pane_notices_fail_closed() {
    // (x-a600, AC4-ERR) A stale pane id is refused with a notice and
    // nothing is seeded: the handler must not unwrap a missing entry.
    let (mut core, pid) = counter_core(b"x");
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(8);
    let dirty: DirtyMap = Arc::default();
    let notify = Arc::new(Notify::new());
    core.clients.push(Client {
        id: 9,
        reliable_tx: tx,
        dirty: dirty.clone(),
        notify: notify.clone(),
        synced_modes: Modes::default(),
        view: (1, 1),
        visible: HashSet::from([pid]),
        dims: (24, 80),
        passive: false,
        last_press: None,
    });
    core.command(9, Command::RedrawPane { pane: Some(4242) });
    assert!(
        dirty.lock().unwrap().is_empty(),
        "a gone pane seeds nothing"
    );
    match rx.try_recv() {
        Ok(ServerMsg::Notice { text }) => {
            assert!(text.contains("4242"), "refusal named the pane: {text}")
        }
        other => panic!("expected a refusal notice, got {other:?}"),
    }
}

#[test]
fn dropped_dirty_frame_composites_but_never_emits() {
    // Two broadcasts with no writer drain: the newest-wins map keeps one
    // frame, so composited advances twice and emitted once - the gap is
    // the measurement, never "fixed" by counting the dropped frame.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (mut core, pid) = counter_core(b"x");
            core.panes.get_mut(&pid).unwrap().vt.feed(b"$ ");
            let (tx, _rx) = mpsc::channel::<ServerMsg>(8);
            let dirty: DirtyMap = Arc::default();
            let notify = Arc::new(Notify::new());
            core.clients.push(Client {
                id: 9,
                reliable_tx: tx,
                dirty: dirty.clone(),
                notify: notify.clone(),
                synced_modes: Modes::default(),
                view: (1, 1),
                visible: HashSet::from([pid]),
                dims: (24, 80),
                passive: false,
                last_press: None,
            });
            let base = core.panes[&pid]
                .stats
                .frames_composited
                .load(Ordering::Relaxed);
            core.broadcast_pane(pid);
            core.broadcast_pane(pid);
            let c = &core.panes[&pid].stats;
            assert_eq!(c.frames_composited.load(Ordering::Relaxed), base + 2);
            assert_eq!(dirty.lock().unwrap().len(), 1, "newest-wins coalescing");

            // The Bye flush is one wire path: one emitted frame.
            let (mut writer, mut reader) = tokio::io::duplex(4096);
            let stats = core.pane_stats.clone();
            write_reliable(
                &mut writer,
                &ServerMsg::Bye { reason: "d".into() },
                &dirty,
                &stats,
            )
            .await
            .unwrap();
            let _ = read_msg::<_, ServerMsg>(&mut reader).await;
            assert_eq!(c.frames_emitted.load(Ordering::Relaxed), 1);

            // A reliable Frame (the cold-attach snapshot path) emits too.
            write_reliable(
                &mut writer,
                &ServerMsg::Frame {
                    pane_id: pid,
                    frame: core.panes[&pid].vt.frame(),
                },
                &dirty,
                &stats,
            )
            .await
            .unwrap();
            let _ = read_msg::<_, ServerMsg>(&mut reader).await;
            assert_eq!(c.frames_emitted.load(Ordering::Relaxed), 2);
            // A pane with no registry row (reaped mid-drain) skips
            // silently rather than counting a phantom emit.
            count_frame_emitted(&stats, u64::MAX);
            assert_eq!(c.frames_emitted.load(Ordering::Relaxed), 2);
        });
}

#[test]
fn pane_stats_payload_carries_provenance_and_totals() {
    let (core, pid) = counter_core(b"abc");
    let payload = core.pane_stats_payload().expect("live pane -> payload");
    let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(v["session"], "test");
    let row = v["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"].as_u64() == Some(pid))
        .expect("the live pane's row");
    assert_eq!(row["node"], "x-deadbeef");
    assert_eq!(row["name"], "peer");
    assert_eq!(row["cmd"], "claude");
    assert_eq!(row["bytes_in"], 3);
    for key in [
        "grid_updates",
        "frames_composited",
        "frames_emitted",
        "cpu_ns",
    ] {
        assert!(row[key].is_u64(), "{key} must be a number");
    }
    assert!(empty_core().pane_stats_payload().is_none());
}

#[test]
fn reaping_a_pane_drops_its_counter_row() {
    let (mut core, pid) = counter_core(b"z");
    assert!(core.pane_stats.read().unwrap().contains_key(&pid));
    core.reap_pane(pid);
    assert!(!core.pane_stats.read().unwrap().contains_key(&pid));
}
