//! The lease heartbeat: with a shared primary set, the daemon renews every
//! dispatch claim this machine holds, in one statement per beat.
//!
//! A peer cannot probe this machine's pids, so on the primary only the lease
//! says a claim is held. Renewal at a worker's stop never comes during one
//! turn that runs for hours, so the daemon renews while the holder's pid is
//! provably alive here. A claim that a peer took after its lease ran out is
//! lost: the beat names the taker in a `claim_lost` event, and the worker
//! reads the same reason at its next stop (`claim_store::lost_to_peer`).
//! With no primary set, a beat returns at once and opens no socket.

use crate::claims::{self, ClaimRecord};
use crate::store_remote::Remote;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_secs(60);

/// How far one beat moves a lease: ten beats. A machine that drops off the
/// network keeps its claims this long, then a peer may take them.
pub const LEASE: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
    /// key -> holder for the claims the last good beat renewed.
    held: Arc<Mutex<BTreeMap<String, String>>>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Beat {
    pub renewed: Vec<String>,
    /// (key, reason) for each claim a peer took since the last good beat.
    pub lost: Vec<(String, String)>,
}

pub fn maybe_tick(arm: &Arm) {
    let remote = match crate::store_remote::configured() {
        Ok(Some(remote)) => remote,
        Ok(None) => return,
        Err(error) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| eprintln!("lease-heartbeat: {error}"));
            return;
        }
    };
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|tick| tick.elapsed() < INTERVAL)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    let held = Arc::clone(&arm.held);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let mut held = held.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = beat(&remote, &mut held, LEASE.as_millis() as i64) {
            eprintln!("lease-heartbeat: {error}");
        }
    });
}

/// One beat. An unreachable primary is an error and leaves `held` as it was,
/// so the first beat after the network returns still sees what was lost.
pub(crate) fn beat(
    remote: &Remote,
    held: &mut BTreeMap<String, String>,
    lease_ms: i64,
) -> Result<Beat, String> {
    let machine = claims::machine_id();
    if machine.is_empty() {
        return Err("this machine has no stable id, so the primary cannot tell its claims".into());
    }
    let alive: Vec<ClaimRecord> = crate::claim_store::leased_on(remote, &machine)?
        .into_iter()
        .filter(claims::is_live)
        .collect();
    let renewed = crate::claim_store::extend_leases(remote, &machine, &alive, lease_ms)?;
    let mut lost = Vec::new();
    for (key, holder) in held.iter() {
        if renewed.contains(key) {
            continue;
        }
        if let Some(reason) = crate::claim_store::lost_on(remote, key, holder)? {
            claims::emit_audit_event(
                None,
                "claim_lost",
                json!({"key": key, "holder": holder, "reason": reason})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            lost.push((key.clone(), reason));
        }
    }
    *held = alive
        .into_iter()
        .filter(|record| renewed.contains(&record.key))
        .map(|record| (record.key, record.holder))
        .collect();
    Ok(Beat { renewed, lost })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{AcquireOpts, AcquireOutcome};

    /// One beat renews only the claims whose pid lives here. A peer's
    /// takeover after the lease ran out is named, and an unreachable primary
    /// leaves what the arm remembers untouched.
    #[test]
    fn lease_heartbeat_renews_live_claims_and_names_a_lost_one() {
        if claims::machine_id().is_empty() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let dir = claims::claims_dir_for(Some(root.path())).unwrap();
        let primary = crate::store_remote::test_primary::start();
        crate::claim_store::route_to_primary(Some((primary.remote.clone(), dir)));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        for (key, pid) in [("node:live", std::process::id()), ("node:dead", dead)] {
            let opts = AcquireOpts {
                root: Some(root.path().to_path_buf()),
                pid: Some(pid),
                ttl_ms: Some(60_000),
                ..Default::default()
            };
            let outcome = claims::acquire(key, "me", opts);
            assert!(matches!(outcome, AcquireOutcome::Acquired(_)), "{outcome:?}");
        }
        crate::claim_store::route_to_primary(None);
        let expiry = |key: &str| -> i64 {
            primary
                .db
                .lock()
                .unwrap()
                .query_row("SELECT expires_at FROM claims WHERE key = ?1", [key], |r| r.get(0))
                .unwrap()
        };
        let dead_lease = expiry("node:dead");
        let mut held = BTreeMap::new();
        let first = beat(&primary.remote, &mut held, 600_000).unwrap();
        assert_eq!(first, Beat { renewed: vec!["node:live".into()], lost: vec![] });
        assert!(expiry("node:live") >= claims::now_ms() + 590_000);
        assert_eq!(expiry("node:dead"), dead_lease, "a dead holder's lease lapses");

        let mut offline = held.clone();
        let dead_primary = crate::store_remote::test_primary::dead();
        let error = beat(&dead_primary, &mut offline, 600_000).unwrap_err();
        assert!(crate::store_remote::is_unreachable(&error), "{error}");
        assert_eq!(offline, held);

        primary
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE claims SET holder = 'peer', host = 'imac', machine_id = 'imac-id', \
                 acquired_at = acquired_at + 1 WHERE key = 'node:live'",
                [],
            )
            .unwrap();
        let second = beat(&primary.remote, &mut held, 600_000).unwrap();
        assert!(second.renewed.is_empty());
        assert_eq!(second.lost.len(), 1, "{second:?}");
        assert!(second.lost[0].1.contains("held by peer on imac"), "{second:?}");
        assert!(held.is_empty());
    }
}
