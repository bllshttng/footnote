//! Carrying a session's claims across a writer change.
//!
//! The two handoff strategies stop one process and start another. A claim
//! pinned to the stopped pid reads STALE in that window, and a stale claim
//! is stealable - which is how a converted session loses its node to a
//! passing dispatcher halfway through its own conversion.
//!
//! So the claims move in two hops, and the daemon holds them in between:
//! re-pin to the daemon BEFORE the old writer stops, re-pin to the new
//! writer AFTER it is live. The daemon is alive for both hops, so at no
//! point is a live claim pinned to a dead process.
//!
//! Both hops are a same-holder re-acquire, which the claim store treats as
//! idempotent: the holder string never changes, so nothing is stolen and
//! nothing is released. Only the recorded pid moves.

use serde_json::Value;

/// One claim this session holds, reduced to what a re-pin needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldClaim {
    pub key: String,
    pub holder: String,
    pub pid: Option<u32>,
}

/// The claims to carry: every LIVE claim whose recorded pid is the writer
/// about to stop. A claim pinned to some other pid belongs to another
/// session and is never touched, and a claim with no pid has nothing to
/// re-pin.
///
/// Sound for the FIRST hop only. The pane child pid belongs to exactly one
/// session, so filtering by it selects that session's claims and no others.
/// The daemon pid does not: a codex thread parks its claims there for the
/// life of the session, and a concurrent conversion parks its own there
/// mid-move. So the second hop never calls this. It moves the exact set the
/// first hop returned, which is why [`repin_all`] takes a list rather than
/// a pid to search by.
pub fn claims_to_carry(rows: &Value, writer_pid: u32) -> Vec<HeldClaim> {
    let Some(rows) = rows.get("rows").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pid = u32::try_from(row.get("pid")?.as_u64()?).ok()?;
            if pid != writer_pid {
                return None;
            }
            Some(HeldClaim {
                key: row.get("key")?.as_str()?.to_string(),
                holder: row.get("holder")?.as_str()?.to_string(),
                pid: Some(pid),
            })
        })
        .collect()
}

/// Re-pin every claim to `pid`, under the SAME holder. Returns the keys
/// that could not be moved, in order.
///
/// A failure here is reported, never swallowed, and never retried blind: a
/// claim the conversion could not carry is one the operator must see, and
/// the caller's refusal names it. The re-pins that DID land stay landed -
/// undoing them would pin a live claim back to a process that is stopping.
pub fn repin_all(
    claims: &[HeldClaim],
    pid: u32,
    acquire: &dyn Fn(&str, &str, u32) -> Result<(), String>,
) -> Vec<(String, String)> {
    let mut failures = Vec::new();
    for claim in claims {
        if let Err(error) = acquire(&claim.key, &claim.holder, pid) {
            failures.push((claim.key.clone(), error));
        }
    }
    failures
}

/// The production re-pin: a same-holder acquire with an explicit pid.
pub fn repin(key: &str, holder: &str, pid: u32) -> Result<(), String> {
    match crate::claims::acquire(
        key,
        holder,
        crate::claims::AcquireOpts {
            pid: Some(pid),
            ..Default::default()
        },
    ) {
        crate::claims::AcquireOutcome::Acquired(_) => Ok(()),
        // Another live holder took it while the conversion was mid-move.
        // That is a real loss, not a retryable hiccup: the caller reports
        // it rather than stealing the claim back.
        crate::claims::AcquireOutcome::HeldByOther { holder, .. } => {
            Err(format!("now held by {holder}"))
        }
        crate::claims::AcquireOutcome::Error(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_pid_selects_every_session_on_it_which_is_why_hop_two_never_searches() {
        // Two sessions parked on one daemon pid. Searching by that pid
        // returns BOTH, so a second hop derived this way would pin another
        // session's claim to this session's new writer.
        let rows = serde_json::json!({"rows": [
            {"key": "session:aaa", "holder": "pty:aaa", "pid": 900},
            {"key": "session:bbb", "holder": "pty:bbb", "pid": 900},
            {"key": "session:ccc", "holder": "pty:ccc", "pid": 901},
        ]});
        let carried = claims_to_carry(&rows, 900);
        assert_eq!(carried.len(), 2, "the filter is pid-only: {carried:?}");
        // The caller's defence is to carry the first hop's own list forward,
        // never to re-derive from a pid it shares.
        let mine = vec![carried[0].clone()];
        assert_eq!(mine.len(), 1);
    }

    fn rows() -> Value {
        serde_json::json!({"rows": [
            {"key": "node:x-1", "holder": "target-session:abc", "pid": 4242, "state": "live"},
            {"key": "session:uuid", "holder": "pty:short", "pid": 4242, "state": "live"},
            {"key": "node:x-2", "holder": "someone-else", "pid": 999, "state": "live"},
            {"key": "node:x-3", "holder": "no-pid", "state": "live"},
        ]})
    }

    #[test]
    fn only_the_stopping_writers_claims_are_carried() {
        let carried = claims_to_carry(&rows(), 4242);
        assert_eq!(
            carried.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            vec!["node:x-1", "session:uuid"],
            "another session's claim and a pid-less claim are never touched"
        );
        assert!(carried.iter().all(|claim| claim.pid == Some(4242)));
    }

    #[test]
    fn an_unreadable_listing_carries_nothing_rather_than_guessing() {
        assert!(claims_to_carry(&serde_json::json!({}), 4242).is_empty());
        assert!(claims_to_carry(&serde_json::json!({"rows": "nope"}), 4242).is_empty());
    }

    #[test]
    fn a_repin_keeps_the_holder_and_moves_only_the_pid() {
        let seen = std::cell::RefCell::new(Vec::new());
        let acquire = |key: &str, holder: &str, pid: u32| {
            seen.borrow_mut()
                .push((key.to_string(), holder.to_string(), pid));
            Ok(())
        };
        let failures = repin_all(&claims_to_carry(&rows(), 4242), 777, &acquire);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            seen.into_inner(),
            vec![
                (
                    "node:x-1".to_string(),
                    "target-session:abc".to_string(),
                    777
                ),
                ("session:uuid".to_string(), "pty:short".to_string(), 777),
            ]
        );
    }

    #[test]
    fn a_failed_repin_is_reported_by_key_and_never_hidden() {
        let acquire = |key: &str, _: &str, _: u32| {
            if key == "session:uuid" {
                Err("now held by another".to_string())
            } else {
                Ok(())
            }
        };
        let failures = repin_all(&claims_to_carry(&rows(), 4242), 777, &acquire);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "session:uuid");
        assert!(failures[0].1.contains("held by"));
    }
}
