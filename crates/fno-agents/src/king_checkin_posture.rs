//! The lead-posture reading: the permission mode and sandbox the lead was
//! crowned with, the posture its reports now observe, and whether the two
//! disagree. The codex Kestrel's sandbox flipped mid-reign and nothing
//! showed it (lead comparison 2026-10-03), so the check-in now prints both
//! on every beat and names a drift as what it is: a change only the user
//! may make. The crowned record itself is stamped at spawn (`RegistryEntry`
//! v19/v35 posture columns); this reading never writes one.

use serde_json::{json, Value};

use crate::king_checkin::Reading;

pub(super) fn reading() -> Result<Value, String> {
    let (session_id, _) = crate::claims::resolve_identity();
    let session_id = session_id
        .ok_or_else(|| "no session id resolved from the ambient environment".to_string())?;
    let registry =
        crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
            .map_err(|e| format!("registry unreadable: {e}"))?;
    let mine: Vec<&crate::state::RegistryEntry> = registry
        .entries
        .iter()
        .filter(|row| row.harness_session_id.as_deref() == Some(session_id.as_str()))
        .collect();
    if mine.is_empty() {
        return Err(format!("no registry row for session {session_id}"));
    }
    let crowned: Vec<&&crate::state::RegistryEntry> = mine
        .iter()
        .filter(|row| row.crown_level.is_some())
        .collect();
    match crowned.len() {
        0 => Err("this session's registry row holds no crown".into()),
        1 => fold(crowned[0]),
        _ => Err("multiple crowned rows carry this session id".into()),
    }
}

/// The posture facts off one crowned row, pure so tests can drive it.
/// `drifted` is provable only when BOTH the crowned sandbox and an observed
/// sandbox exist and differ: a missing baseline or a missing observation
/// reads unproven, never drifted.
fn fold(row: &crate::state::RegistryEntry) -> Result<Value, String> {
    let observed = row
        .inside_leg
        .as_ref()
        .and_then(|report| report.posture.as_ref());
    let observed_sandbox = observed.map(|p| p.sandbox.as_str());
    let drifted = matches! {
        (row.sandbox_posture.as_deref(), observed_sandbox),
        (Some(crowned), Some(seen)) if crowned != seen
    };
    Ok(json!({
        "mode": row.requested_permission_mode,
        "sandbox": row.sandbox_posture,
        "resolved_sandbox": row.resolved_sandbox,
        "observed": observed.map(|p| format!("{}:{}", p.sandbox, p.approval)),
        "observed_sandbox": observed_sandbox,
        "drifted": drifted,
    }))
}

pub(super) fn lines(readings: &[Reading]) -> Vec<String> {
    let reading = readings.iter().find(|r| r.name == "posture");
    let Some(reading) = reading else {
        return Vec::new();
    };
    if !reading.ok {
        return vec![format!("READER FAILED posture: {}", reading.error)];
    }
    let v = &reading.value;
    let part = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("-")
    };
    if v.get("drifted").and_then(Value::as_bool) == Some(true) {
        return vec![format!(
            "POSTURE DRIFT: crowned sandbox {}, observed {} - a lead's posture \
             is fixed for its reign; only the user may re-pin or restore it",
            part("sandbox"),
            part("observed_sandbox"),
        )];
    }
    vec![format!(
        "posture: mode {}, sandbox {}, observed {}",
        part("mode"),
        part("sandbox"),
        part("observed"),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{InsideLegReport, ObservedPosture, RegistryEntry};

    fn crowned_row(sandbox: Option<&str>, mode: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            crown_level: Some(1),
            sandbox_posture: sandbox.map(str::to_string),
            requested_permission_mode: mode.map(str::to_string),
            ..Default::default()
        }
    }

    fn observed(row: &mut RegistryEntry, sandbox: &str, approval: &str) {
        let mut report = InsideLegReport::default();
        report.posture = Some(ObservedPosture::observed(sandbox, approval));
        row.inside_leg = Some(report);
    }

    #[test]
    fn fold_judges_drift_only_from_both_halves_and_lines_print_both() {
        // Drift is provable only when the crowned sandbox and an observed
        // sandbox both exist and differ; either half missing reads unproven.
        let mut row = crowned_row(Some("danger-full-access"), Some("yolo"));
        assert_eq!(fold(&row).unwrap()["drifted"], false);
        observed(&mut row, "workspace-write", "never");
        assert_eq!(fold(&row).unwrap()["drifted"], true);
        let mut no_baseline = crowned_row(None, None);
        observed(&mut no_baseline, "workspace-write", "never");
        assert_eq!(fold(&no_baseline).unwrap()["drifted"], false);
        let mut same = crowned_row(Some("workspace-write"), None);
        observed(&mut same, "workspace-write", "on-request");
        let out = fold(&same).unwrap();
        assert_eq!(out["drifted"], false);
        assert_eq!(out["observed"], "workspace-write:on-request");

        let readings = |ok: bool, value: Value| {
            vec![if ok {
                Reading::took("posture", value)
            } else {
                Reading::failed("posture", "registry unreadable".into())
            }]
        };
        let drifted = lines(&readings(true, fold(&row).unwrap()));
        assert!(drifted[0].starts_with(
            "POSTURE DRIFT: crowned sandbox danger-full-access, observed workspace-write"
        ));
        let calm = lines(&readings(true, fold(&same).unwrap()));
        assert_eq!(
            calm[0],
            "posture: mode -, sandbox workspace-write, observed workspace-write:on-request"
        );
        let failed = lines(&readings(false, Value::Null));
        assert_eq!(failed[0], "READER FAILED posture: registry unreadable");
    }
}
