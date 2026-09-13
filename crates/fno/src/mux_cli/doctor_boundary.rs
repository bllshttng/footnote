//! The optional-component rows of `fno mux doctor`: the four observations
//! product_boundary classifies, projected as doctor checks (text and exit
//! path) and as typed JSON. Advisory by construction: no row ever fails, so
//! a plain workspace with no optional component installed never exits
//! non-zero from doctor.

use super::*;
use crate::product_boundary;
use serde_json::json;
use std::path::Path;

/// One dependency row: the doctor check (name/verdict/detail/remedy, the
/// text and exit path) plus the typed JSON row (availability, reason,
/// observed_at, remedy) doctor --json carries beside the checks.
pub(super) struct DependencyRow {
    pub(super) check: Check,
    pub(super) json: serde_json::Value,
}

/// The doctor verdict for an availability. Missing or incompatible is
/// degraded-but-usable (Warn), never a Fail: the plain workspace is not
/// broken. Unmeasured is clean.
fn verdict_for(a: product_boundary::Availability) -> Verdict {
    match a {
        product_boundary::Availability::Available => Verdict::Ok,
        product_boundary::Availability::Unavailable => Verdict::Warn,
        product_boundary::Availability::Incompatible => Verdict::Warn,
        product_boundary::Availability::Unmeasured => Verdict::Na,
    }
}

/// The digest's distinct states, projected the same advisory way.
fn verdict_for_digest(s: product_boundary::DigestState) -> Verdict {
    match s {
        product_boundary::DigestState::Available => Verdict::Ok,
        product_boundary::DigestState::DisabledByConfig => Verdict::Na,
        product_boundary::DigestState::BackendUnavailable => Verdict::Warn,
    }
}

pub(super) fn dependency_rows(cwd: &Path) -> Vec<DependencyRow> {
    let mut rows = vec![
        row(&product_boundary::agent_runtime_observation()),
        row(&product_boundary::graph_worker_observation()),
        row(&product_boundary::python_cli_observation()),
    ];
    rows.push(digest_row(cwd));
    rows
}

fn row(obs: &product_boundary::Observation) -> DependencyRow {
    let verdict = verdict_for(obs.availability);
    DependencyRow {
        check: Check {
            name: format!("optional component {}", obs.component),
            verdict,
            detail: obs.reason.clone(),
            remedy: obs.remedy().map(str::to_string),
        },
        json: json!({
            "component": obs.component,
            "requirement": obs.requirement,
            "availability": obs.availability,
            "reason": obs.reason,
            "observed_at": obs.observed_at,
            "remedy": obs.remedy(),
        }),
    }
}

fn digest_row(cwd: &Path) -> DependencyRow {
    let obs = product_boundary::digest_observation(cwd);
    let verdict = verdict_for_digest(obs.state);
    DependencyRow {
        check: Check {
            name: "digest backend (fno-agents)".into(),
            verdict,
            detail: obs.reason.clone(),
            remedy: match obs.state {
                product_boundary::DigestState::BackendUnavailable => {
                    Some("set FNO_AGENTS_BIN or install the runtime".to_string())
                }
                _ => None,
            },
        },
        json: json!({
            "component": "digest",
            "requirement": "the attach-time digest overlay",
            "state": obs.state,
            "reason": obs.reason,
            "observed_at": obs.observed_at,
            "remedy": match obs.state {
                product_boundary::DigestState::BackendUnavailable => {
                    json!("set FNO_AGENTS_BIN or install the runtime")
                }
                _ => serde_json::Value::Null,
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_rows_cover_all_four_observations() {
        let rows = dependency_rows(Path::new("."));
        assert!(rows
            .iter()
            .any(|r| r.check.name.contains("fno-agents-worker")));
        assert!(rows.iter().any(|r| {
            let n = r.check.name.as_str();
            n.contains("fno-agents") && !n.contains("worker") && !n.starts_with("digest")
        }));
        assert!(rows.iter().any(|r| r.json["component"]
            .as_str()
            .is_some_and(|c| c.contains("python"))));
        assert!(rows.iter().any(|r| r.check.name.starts_with("digest")));
    }

    #[test]
    fn dependency_rows_never_fail() {
        let rows = dependency_rows(Path::new("."));
        for r in &rows {
            assert_ne!(r.check.verdict, Verdict::Fail);
        }
    }

    #[test]
    fn json_rows_carry_availability_reason_observed_at_remedy() {
        let rows = dependency_rows(Path::new("."));
        for r in &rows {
            assert!(r.json["component"].is_string());
            assert!(
                r.json["availability"].is_string() || r.json["state"].is_string(),
                "each row carries a typed verdict"
            );
            assert!(r.json["reason"].is_string());
            assert!(r.json["observed_at"].is_u64());
            assert!(r.json.get("remedy").is_some());
        }
    }
}
