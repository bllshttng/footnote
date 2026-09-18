//! Two rows on one territory, and which kind of two.
//!
//! `double_ruled` is an authority finding: more than one NON-TERMINAL row
//! carries the same territory key. For a single-member scope that is the
//! same condition `crown_settle::resolve` refuses a grant on. Settle
//! compares the raw scope string, so a comma re-spelling or an alias name
//! can pass the grant door yet read as double-ruled here. That gap is
//! named, not fixed.
//!
//! `stale` is a board finding: a TERMINAL row still carrying crown fields.
//! Nothing clears a crown when a row goes terminal, so these accumulate and
//! are what a reader mistakes for two kings. Separate word, separate remedy.

use crate::state::RegistryEntry;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScopeSplit {
    pub scope: String,
    pub holders: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StaleCrown {
    pub row: String,
    pub scope: String,
    pub stored_status: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CrownSplits {
    pub double_ruled: Vec<ScopeSplit>,
    pub stale: Vec<StaleCrown>,
}

/// Group crowned rows by normalized territory key over ALL rows, live and
/// terminal. The court's own conflict scan filters to stored-live rows, so
/// a terminal crowned row is invisible there by construction; this reader
/// reads the registry directly for that reason. Same territory key is
/// deliberately narrower than the court's overlap rule: a live level-1
/// crown over a project and a live level-2 crown over one node never pair,
/// which is the legitimate portfolio-and-court arrangement.
pub(crate) fn read_crown_splits(rows: &[RegistryEntry]) -> CrownSplits {
    let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stale: Vec<StaleCrown> = Vec::new();
    for row in rows {
        let Some(scope) = row
            .crown_scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let key = crate::loop_reign::territory_key(scope);
        if key.is_empty() {
            continue;
        }
        if crate::loop_reign::is_terminal(row) {
            // The stored word, exactly as the registry serializes it, so the
            // anomaly line and a court JSON read compare without a
            // translation table.
            let stored_status = serde_json::to_value(row.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            stale.push(StaleCrown {
                row: row.name.clone(),
                scope: key,
                stored_status,
            });
        } else {
            live.entry(key).or_default().push(row.name.clone());
        }
    }
    // BTreeMap iteration is key-sorted, so the output order is a function of
    // the registry alone and two beats over an unchanged registry render
    // identically.
    let double_ruled = live
        .into_iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(scope, mut holders)| {
            holders.sort();
            ScopeSplit { scope, holders }
        })
        .collect();
    stale.sort_by(|a, b| (&a.scope, &a.row).cmp(&(&b.scope, &b.row)));
    CrownSplits {
        double_ruled,
        stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentStatus;

    fn row(name: &str, scope: Option<&str>, status: AgentStatus) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            crown_scope: scope.map(str::to_string),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn two_live_rows_on_one_scope_are_one_double_rule() {
        let rows = [
            row("king-b", Some("shared"), AgentStatus::Busy),
            row("king-a", Some("shared"), AgentStatus::Live),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.stale.is_empty());
        assert_eq!(out.double_ruled.len(), 1);
        assert_eq!(out.double_ruled[0].scope, "shared");
        assert_eq!(out.double_ruled[0].holders, vec!["king-a", "king-b"]);
    }

    #[test]
    fn a_terminal_row_is_stale_never_double_ruled() {
        let rows = [
            row("king-live", Some("shared"), AgentStatus::Live),
            row("king-dead", Some("shared"), AgentStatus::Orphaned),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.double_ruled.is_empty());
        assert_eq!(out.stale.len(), 1);
        assert_eq!(out.stale[0].row, "king-dead");
        assert_eq!(out.stale[0].scope, "shared");
        assert_eq!(out.stale[0].stored_status, "orphaned");
    }

    #[test]
    fn comma_members_normalize_to_one_territory() {
        let rows = [
            row("one", Some("alpha,beta"), AgentStatus::Busy),
            row("two", Some("beta, alpha"), AgentStatus::Busy),
        ];
        let out = read_crown_splits(&rows);
        assert_eq!(out.double_ruled.len(), 1);
        assert_eq!(out.double_ruled[0].scope, "alpha,beta");
        assert_eq!(out.double_ruled[0].holders, vec!["one", "two"]);
    }

    #[test]
    fn no_crowned_row_reads_empty_on_both_sides() {
        let rows = [
            row("plain", None, AgentStatus::Busy),
            row("blank", Some("  "), AgentStatus::Live),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.double_ruled.is_empty());
        assert!(out.stale.is_empty());
    }

    #[test]
    fn two_terminal_rows_still_are_not_a_double_rule() {
        let rows = [
            row("dead-1", Some("shared"), AgentStatus::Exited),
            row("dead-2", Some("shared"), AgentStatus::PermanentDead),
        ];
        let out = read_crown_splits(&rows);
        assert!(out.double_ruled.is_empty());
        assert_eq!(out.stale.len(), 2);
    }
}
