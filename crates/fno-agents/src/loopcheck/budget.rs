//! Is the budget spent? Wall-clock and cost caps from the manifest and config, checked against the ledger.

use super::*;

#[derive(Debug, PartialEq)]
pub(super) enum BudgetTrip {
    WallClock,
    Cost,
}

/// Resolve an `Option<Result<T, String>>` budget cap for use in check_budget.
/// - None => absent (no cap)
/// - Some(Ok(v)) => valid cap value
/// - Some(Err(raw)) => malformed: fail-closed, treat as cap exceeded immediately
pub(super) enum ResolvedCap<T> {
    Absent,
    Valid(T),
    Malformed(String),
}

pub(super) fn resolve_cap<T: Copy>(cap: &Option<Result<T, String>>) -> ResolvedCap<T> {
    match cap {
        None => ResolvedCap::Absent,
        Some(Ok(v)) => ResolvedCap::Valid(*v),
        Some(Err(raw)) => ResolvedCap::Malformed(raw.clone()),
    }
}

pub(super) fn check_budget(
    manifest: &Manifest,
    settings: &Settings,
    now: &DateTime<Utc>,
    ledger_path: &Path,
) -> Option<BudgetTrip> {
    let attended = manifest.attended;

    // Wall-clock cap: prefer manifest value, then settings
    let wall_cap = match resolve_cap(&manifest.budget_wall_clock_cap_minutes) {
        ResolvedCap::Absent => {
            if attended {
                resolve_cap(&settings.attended_wall_cap_minutes)
            } else {
                resolve_cap(&settings.unattended_wall_cap_minutes)
            }
        }
        other => other,
    };

    match wall_cap {
        ResolvedCap::Malformed(raw) => {
            eprintln!("loop-check: malformed budget cap '{raw}' - failing closed; fix the config");
            return Some(BudgetTrip::WallClock);
        }
        ResolvedCap::Valid(cap) => {
            if let Some(ca_str) = &manifest.created_at {
                if let Ok(created) = ca_str.parse::<DateTime<Utc>>() {
                    // Guard against negative elapsed (clock skew / future created_at)
                    let duration = now.signed_duration_since(created);
                    let elapsed_min = if duration.num_minutes() < 0 {
                        0u64
                    } else {
                        duration.num_minutes() as u64
                    };
                    if elapsed_min >= cap {
                        return Some(BudgetTrip::WallClock);
                    }
                }
            }
        }
        ResolvedCap::Absent => {}
    }

    // Cost cap: prefer manifest value, then nested settings, then flat budget_cap
    let cost_cap = match resolve_cap(&manifest.budget_cost_cap_usd) {
        ResolvedCap::Absent => {
            let nested = if attended {
                resolve_cap(&settings.attended_cost_cap_usd)
            } else {
                resolve_cap(&settings.unattended_cost_cap_usd)
            };
            match nested {
                ResolvedCap::Absent => resolve_cap(&settings.flat_budget_cap),
                other => other,
            }
        }
        other => other,
    };

    match cost_cap {
        ResolvedCap::Malformed(raw) => {
            eprintln!("loop-check: malformed budget cap '{raw}' - failing closed; fix the config");
            Some(BudgetTrip::Cost)
        }
        ResolvedCap::Valid(cap) => {
            if let Some(session_id) = &manifest.session_id {
                let cost = session_cost_from_ledger(ledger_path, session_id);
                if cost >= cap {
                    return Some(BudgetTrip::Cost);
                }
            }
            None
        }
        ResolvedCap::Absent => None,
    }
}
