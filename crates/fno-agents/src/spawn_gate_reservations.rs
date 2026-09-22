//! Lane reservations: a provider lane slot held for a worker that has not
//! spawned yet, minted by `fno-agents spawn-gate reserve`, redeemed by the
//! named spawn at admission, and freed by TTL whichever happens first.
//!
//! The claim shape already existed: a `worker:<name>` claim tagged
//! `model_provider` spends a lane slot with no registry row behind it. What
//! was missing was a guarded way to mint one, the redemption path so the
//! reserved spawn is not refused by its own reservation, and the expiry
//! ceiling that keeps a dead reservation from starving the lane.

use std::path::Path;

use serde_json::Value;

use crate::claims;
use crate::spawn_gate::NOTE;

/// The reservation rule, in the same words everywhere it appears: the
/// provider-cap refusal, the reserve mode's usage text, and
/// docs/architecture/coordination.md. The refusal must state what actually
/// governs, so an agent stops inferring a rule from a number.
pub(crate) const RESERVATION_RULE: &str = "A reservation is held for a NAME: spawn with \
     --name <that name> to redeem it. A reservation expires within 15 minutes, whatever \
     happened to the session that made it. Slot order is otherwise first-come; read \
     `fno agents gate-status` for the lane.";

/// The provider-cap refusal's reservation clause: the held names and their
/// expiries, capped at five with an ellipsis (the held_rows_suffix shape).
/// Empty reservations produce an empty string.
pub(crate) fn reserved_note(reserved: &[(String, i64)]) -> String {
    if reserved.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = reserved
        .iter()
        .take(5)
        .map(|(name, exp)| format!("{name} expires {}", hhmm_z(*exp)))
        .collect();
    let more = if reserved.len() > 5 {
        format!(", {} more...", reserved.len() - 5)
    } else {
        String::new()
    };
    format!(
        " ({} reserved: {}{})",
        reserved.len(),
        shown.join(", "),
        more
    )
}

/// The refusal receipt's `reserved` field: one object per held reservation.
pub(crate) fn reserved_receipt(reserved: &[(String, i64)]) -> Value {
    Value::Array(
        reserved
            .iter()
            .map(|(name, exp)| serde_json::json!({"name": name, "expires_at": exp}))
            .collect(),
    )
}

/// Epoch ms to a UTC `HH:MMZ` clock time; 0 (no expiry recorded) reads `?`.
pub(crate) fn hhmm_z(expires_ms: i64) -> String {
    if expires_ms <= 0 {
        return "?".to_string();
    }
    let secs = (expires_ms / 1000) % 86_400;
    format!("{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60)
}

/// Release the redeemed reservation just before the admitted spawn takes its
/// own slot claim: a claim whose name is this spawn's AND which carries the
/// `reserved_by` key is the reservation minted for it. A live worker's plain
/// slot claim never carries `reserved_by`, so borrowing a live worker's name
/// releases nothing (AC2-ERR). A gate that dies before redeeming leaves the
/// reservation holding its lane only until its TTL; the safe direction.
pub(crate) fn release_redeemed_reservation(name: &str, root: Option<&Path>) {
    let key = format!("worker:{name}");
    let (state, record) = claims::status(&key, root);
    let Some(rec) = record else {
        return;
    };
    if !matches!(
        state,
        claims::ClaimState::Live | claims::ClaimState::Suspect
    ) {
        return;
    }
    let Some(by) = rec
        .metadata
        .get("reserved_by")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let why = rec
        .metadata
        .get("reserved_reason")
        .and_then(Value::as_str)
        .unwrap_or("no reason recorded");
    match crate::claim_store::force_release(&key, &format!("redeemed by spawn {name}"), root) {
        Ok(_) => {
            eprintln!("spawn-gate: redeemed reservation {key} (reserved by {by}: {why})");
        }
        Err(e) => {
            eprintln!("{NOTE} reservation {key} could not be released before acquire: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims;
    use crate::spawn_gate::{run_gate, GateFlags, GateInput, EXIT_PROVIDER_CAP};
    use std::path::PathBuf;

    /// Fixture: config with zai capped at 2, one live zai registry row, one
    /// zai-tagged reservation claim held live.
    fn reservation_fixture(dir: &Path) -> (PathBuf, PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
        let root = dir.join("claims-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n\n\
             [agents.provider_limits.zai]\nlanes = 2\n",
        )
        .unwrap();
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let reg = agents.join("registry.json");
        let me = std::process::id();
        let start = crate::daemon::process_start_time(me).unwrap_or(0);
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[
                    {{"name":"w1","provider":"zai","cwd":"/tmp","status":"live","pid":{me},"pid_start_time":{start},"created_at":"2026-01-01T00:00:00Z"}}]}}"#
            ),
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        let lock = claims_dir.join(format!(
            "{}.lock",
            claims::encode_key("worker:t-reserved-x-4444")
        ));
        std::fs::write(
            &lock,
            format!(
                "schema_version: {}\nkey: worker:t-reserved-x-4444\nholder: king-1\nacquired_at: {now}\nexpires_at: {}\npid: {}\nhost: {}\nmetadata:\n  model_provider: zai\n  reserved_by: king-1\n  reserved_reason: four parked PRs\n",
                claims::SCHEMA_VERSION,
                now + 600_000,
                me,
                claims::hostname()
            ),
        )
        .unwrap();
        (root, reg)
    }

    /// AC2-EDGE: at cap, a spawn that is NOT the reserved name refuses exit 78
    /// and the receipt names the reservation with its expiry beside the
    /// first-come rule.
    #[test]
    fn provider_cap_refusal_names_its_reservations_and_the_rule() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-resv-ref-{}", std::process::id()));
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        let (_root, reg) = reservation_fixture(&dir);

        let got = run_gate(
            &dir,
            &reg,
            GateInput {
                name: "w-other".into(),
                substrate: "headless".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                route_provider: Some("zai".into()),
                ..GateInput::default()
            },
        );

        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        let refusal = got.err().expect("lane 2/2 must refuse a stranger");
        assert_eq!(refusal.exit_code, EXIT_PROVIDER_CAP);
        let receipt = refusal
            .receipt
            .expect("provider_cap refusal carries a receipt");
        assert_eq!(receipt["reason"], "provider_cap");
        assert_eq!(receipt["reserved"][0]["name"], "t-reserved-x-4444");
        assert!(
            receipt["reserved"][0]["expires_at"].as_i64().unwrap_or(0) > 0,
            "{receipt}"
        );
        let note = reserved_note(&[("t-reserved-x-4444".into(), 1)]);
        assert!(note.contains("1 reserved: t-reserved-x-4444"), "{note}");
        assert!(RESERVATION_RULE.contains("first-come"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC2-HP: at cap, the reserved name is admitted and its reservation is
    /// released before its own slot claim is taken.
    #[test]
    fn a_reserved_name_admits_at_cap_and_releases_the_reservation() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-resv-adm-{}", std::process::id()));
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );
        let (root, reg) = reservation_fixture(&dir);

        let got = run_gate(
            &dir,
            &reg,
            GateInput {
                name: "t-reserved-x-4444".into(),
                substrate: "headless".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                route_provider: Some("zai".into()),
                ..GateInput::default()
            },
        );

        let guard = match got {
            Ok(guard) => guard,
            Err(r) => {
                std::env::remove_var("FNO_CLAIMS_ROOT");
                match prior_payload {
                    Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
                    None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
                }
                match prior_spawn_gate {
                    Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
                    None => std::env::remove_var("FNO_SPAWN_GATE"),
                }
                panic!("the reserved name must admit at cap: {:?}", r.receipt);
            }
        };
        // The reservation is gone; the spawn's own slot claim stands.
        let (state, rec) = claims::status("worker:t-reserved-x-4444", Some(&root));
        assert!(!matches!(
            rec.as_ref(),
            Some(r) if r.metadata.get("reserved_by").is_some()
        ));
        assert!(
            matches!(state, claims::ClaimState::Live),
            "the admitted spawn's own slot claim, not the reservation: {state:?}"
        );
        drop(guard);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC2-ERR: a live worker's own slot claim carries no `reserved_by`, so a
    /// spawn borrowing its name releases nothing.
    #[test]
    fn release_redeemed_reservation_spares_a_live_workers_own_claim() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-resv-err-{}", std::process::id()));
        let root = dir.join("claims-root");
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let host = claims::hostname();
        let lock = claims_dir.join(format!(
            "{}.lock",
            claims::encode_key("worker:t-live-worker")
        ));
        std::fs::write(
            &lock,
            format!(
                "schema_version: {}\nkey: worker:t-live-worker\nholder: h\nacquired_at: {now}\nexpires_at: {}\npid: {}\nhost: {host}\nmetadata:\n  model_provider: zai\n",
                claims::SCHEMA_VERSION,
                now + 600_000,
                std::process::id()
            ),
        )
        .unwrap();
        release_redeemed_reservation("t-live-worker", Some(&root));
        let (state, _) = claims::status("worker:t-live-worker", Some(&root));
        assert!(
            matches!(state, claims::ClaimState::Live),
            "a live worker's claim survives a name-borrowing spawn: {state:?}"
        );
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
