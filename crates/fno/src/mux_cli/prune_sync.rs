//! The prune aftermath (v69): after an applied store pass, tell every live
//! server to re-read `squads.json` (`ControlVerb::SquadReload`), then report
//! the run. Extracted from mux_cli.rs under the file-budget gate - the code
//! the change touched moves with it.

use super::tab_prune::TabPruneOutcome;
use super::*;

/// What the reload wave did: which sessions re-read the store, which refused.
/// `ran` is false on a dry-run or tabs-only pass, which sends nothing.
pub(super) struct ReloadOutcome {
    pub(super) ran: bool,
    pub(super) refreshed: Vec<(String, usize)>,
    pub(super) failed: Vec<String>,
}

/// Send `SquadReload` to every answering session after a real store pass. The
/// in-memory member list is authoritative and the next `persist_squad` would
/// otherwise write the reaped members back over the pruned file. A refused
/// reload is a warning, not a failure: the file pass was real, and the next
/// prune or the server's own SweepDead heals the skew.
pub(super) fn reload_live_sessions(ran: bool, answered: &[String]) -> ReloadOutcome {
    let mut out = ReloadOutcome {
        ran,
        refreshed: Vec::new(),
        failed: Vec::new(),
    };
    if !ran {
        return out;
    }
    for name in answered {
        let verdict = proto::socket_path(name)
            .map_err(|e| e.to_string())
            .and_then(|sock| {
                control_roundtrip(&sock, name, ControlVerb::SquadReload).map_err(|e| e.to_string())
            });
        match verdict {
            Ok(ServerMsg::SquadReloaded { members, .. }) => {
                out.refreshed.push((name.clone(), members));
            }
            Ok(_) | Err(_) => out.failed.push(name.clone()),
        }
    }
    if !out.failed.is_empty() {
        eprintln!(
            "fno mux workspace prune: reload refused: {}",
            out.failed.join(", ")
        );
    }
    out
}

/// A pruned squad's identity line: its name if named, else its durable key.
pub(super) fn prune_identity(sq: &crate::squad_store::PrunedSquad) -> String {
    if sq.name.is_empty() {
        format!("<key:{}>", sq.key)
    } else {
        sq.name.clone()
    }
}

/// The one-line summary after a (dry-)run: count pruned plus why the rest
/// stayed. The tabs line splits `kept` by reason (x-cf97) so "kept 22" is
/// never again one opaque number, and a dry-run NAMES each tab it would
/// close - a count alone stopped being a decision somewhere around tab six.
#[allow(clippy::too_many_arguments)]
pub(super) fn print_prune_summary(
    verb: &str,
    n: usize,
    kept_unknown: usize,
    skipped_named: usize,
    kept_protected: usize,
    members_reaped: usize,
    members_kept_live: usize,
    members_kept_unknown: usize,
    include_named: bool,
    tabs: &TabPruneOutcome,
    answered: usize,
    probed: usize,
    unreachable: &[String],
    reload: &ReloadOutcome,
) {
    let mut parts = vec![format!("{verb} {n} squad(s)")];
    // The acted-on count carries its mood in the verb: an apply run says
    // `closed`, a dry-run says `would close`. Never both numbers at once - the
    // apply reading `tabs 1 (would close 0, ...)` put the closed count beside
    // a zero and read as a no-op.
    let (tabs_word, tabs_n) = if verb == "pruned" {
        ("closed", tabs.closed)
    } else {
        ("would close", tabs.would_close)
    };
    parts.push(format!(
        "tabs {tabs_word} {tabs_n} (skipped named {}, kept {} = last-in-squad {}, not-pristine {}, zero-pane {}, unreachable {}{}, server reachable {answered}/{probed})",
        tabs.skipped_named,
        tabs.kept,
        tabs.kept_last_in_squad,
        tabs.kept_not_pristine,
        tabs.kept_zero_panes,
        tabs.kept_unreachable,
        if tabs.kept_not_probed > 0 {
            format!(", not-probed {}", tabs.kept_not_probed)
        } else {
            String::new()
        },
    ));
    for label in &tabs.closed_named {
        let name_verb = if verb == "pruned" {
            "closed"
        } else {
            "would close"
        };
        println!("{name_verb}: {label}");
    }
    if tabs.used_shells > 0 && tabs.would_close_used == 0 && tabs.closed_used == 0 {
        // The opt-in population the pass saw but (flag off) did not act on.
        parts.push(format!(
            "used shells {} (pass --include-used-shells)",
            tabs.used_shells
        ));
    }
    // (v69) The orphan category names itself only when it fired: zero lines
    // read as "nothing matched", the same rule as every other part.
    let orphan_total = tabs.closed_orphaned + tabs.would_close_orphaned;
    if orphan_total > 0 {
        let overb = if verb == "pruned" {
            "closed"
        } else {
            "would close"
        };
        parts.push(format!("orphaned worker tabs {overb} {orphan_total}"));
    }
    if !unreachable.is_empty() {
        parts.push(format!(
            "skipped unreachable session(s): {}",
            unreachable.join(", ")
        ));
    }
    if kept_protected > 0 {
        parts.push(format!("kept {kept_protected} (live/origin)"));
    }
    if kept_unknown > 0 {
        parts.push(format!("kept {kept_unknown} (liveness unknown)"));
    }
    if skipped_named > 0 && !include_named {
        parts.push(format!(
            "skipped {skipped_named} named (pass --include-named)"
        ));
    }
    if members_reaped > 0 {
        let mverb = if verb == "pruned" {
            "reaped"
        } else {
            "would reap"
        };
        parts.push(format!(
            "{mverb} {members_reaped} dead member(s) from surviving squads"
        ));
    }
    if members_kept_live > 0 {
        parts.push(format!("kept {members_kept_live} live member(s)"));
    }
    if members_kept_unknown > 0 {
        parts.push(format!("kept {members_kept_unknown} unknown member(s)"));
    }
    if reload.ran {
        parts.push(format!("refreshed {} session(s)", reload.refreshed.len()));
    }
    if !reload.failed.is_empty() {
        parts.push(format!("reload refused: {}", reload.failed.join(", ")));
    }
    println!("{}", parts.join("; "));
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_prune_json(
    removed: &[crate::squad_store::PrunedSquad],
    dry_run: bool,
    kept_unknown: usize,
    skipped_named: usize,
    kept_protected: usize,
    members_reaped: usize,
    members_kept_live: usize,
    members_kept_unknown: usize,
    tabs: &TabPruneOutcome,
    probed: usize,
    unreachable: &[String],
    notice: Option<&str>,
    reload: &ReloadOutcome,
) {
    let pruned: Vec<_> = removed
        .iter()
        .map(|sq| {
            serde_json::json!({
                "name": sq.name,
                "key": sq.key,
                "origins": sq.origins,
                "members": sq.members,
                "reason": crate::squad_store::prune_reason(sq),
            })
        })
        .collect();
    let mut payload = serde_json::json!({
        "pruned": pruned,
        "pruned_count": pruned.len(),
        "dry_run": dry_run,
        "kept_protected": kept_protected,
        "kept_unknown": kept_unknown,
        "skipped_named": skipped_named,
        "members_reaped": members_reaped,
        "members_kept_live": members_kept_live,
        "members_kept_unknown": members_kept_unknown,
        "tabs_closed": tabs.closed,
        "tabs_would_close": tabs.would_close,
        "tabs_skipped_named": tabs.skipped_named,
        "tabs_kept": tabs.kept,
        // (x-cf97) The kept split and the opt-in used-shell population:
        // the four reasons sum to `tabs_kept` whenever the fold ran, and
        // `kept_not_probed` covers the run where it did not.
        "tabs_kept_last_in_squad": tabs.kept_last_in_squad,
        "tabs_kept_not_pristine": tabs.kept_not_pristine,
        "tabs_kept_zero_panes": tabs.kept_zero_panes,
        "tabs_kept_unreachable": tabs.kept_unreachable,
        "tabs_kept_not_probed": tabs.kept_not_probed,
        "tabs_used_shells": tabs.used_shells,
        "tabs_closed_used_shells": tabs.closed_used,
        "tabs_would_close_used_shells": tabs.would_close_used,
        "tabs_closed_orphaned": tabs.closed_orphaned,
        "tabs_would_close_orphaned": tabs.would_close_orphaned,
        // (review) One key covers both runs on purpose: on a dry-run these
        // are the candidates, on an apply they are what actually closed.
        "tabs_close_named": tabs.closed_named,
        "server_reachable": unreachable.is_empty(),
        "sessions_probed": probed,
        "sessions_unreachable": unreachable,
        "notice": notice,
    });
    // (v69) The reload keys exist only on a run that sent SquadReload - a
    // dry-run or tabs-only pass sends nothing, so its receipt carries no key
    // rather than an empty list that reads as "zero sessions refreshed".
    if reload.ran {
        payload["refreshed"] = serde_json::json!(reload
            .refreshed
            .iter()
            .map(|(session, members)| serde_json::json!({
                "session": session,
                "members": members,
            }))
            .collect::<Vec<_>>());
        payload["reload_failed"] = serde_json::json!(reload.failed);
    }
    println!("{payload}");
}
