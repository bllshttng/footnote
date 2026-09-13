//! Doctor checks extracted from mux_cli.rs (shrink-only extraction): the
//! legacy-root divergence, the squad store orphans, and the board scope.
//! The verdict logic each calls is pure and unit-tested in mux_cli.rs.

use super::*;

/// `fno mux doctor`'s squad-store check: count how many persisted squads the
/// prune predicate would reap. Read-only (load + registry/roster reads, no pane
/// probe, no mutation). An unreadable registry means the count is unknown, so
/// the check warns and points at the prune verb (which is fail-safe regardless).
pub(super) fn squad_store_check() -> Check {
    let loaded = crate::squad_store::load();
    let total = loaded.squads.len();
    if total == 0 {
        return squad_store_verdict(0, 0);
    }
    let Some(live) = live_set_or_unknown() else {
        return Check {
            name: "squad store".into(),
            verdict: Verdict::Warn,
            detail: "agent registry unreadable; orphan count unknown".into(),
            remedy: Some(PRUNE_REMEDY.into()),
        };
    };
    let origin_exists = |p: &str| std::path::Path::new(p).exists();
    // ONE argument set with the prune that actually runs. This counter used to
    // pass an empty `live_cwds` and no clock, so the number the operator read
    // could disagree with what a bare `prune` would do. `include_named: false`
    // stays hardcoded on purpose: it is what a bare `prune` uses.
    let (_, live_cwds, ..) = live_tabs();
    let now = crate::squad_store::now_epoch_secs();
    let orphan = loaded
        .squads
        .iter()
        .filter(|sq| {
            matches!(
                crate::squad_store::prune_decision_at(
                    sq,
                    false,
                    Some(&live),
                    &live_cwds,
                    &origin_exists,
                    now,
                ),
                crate::squad_store::PruneDecision::Prune
            )
        })
        .count();
    squad_store_verdict(total, orphan)
}

/// Sessions stranded at the pre-config-chain root `~/.fno/mux` when this
/// process resolves its socket dir elsewhere through the ambient chain
/// (`config.state_dir`). A daemon bound before an upgrade - or started from a
/// directory with no state_dir override while clients run inside one that has
/// it - keeps serving a dir no current client lands on, and every reaper
/// (`ls`-based) resolves the new root, so nothing finds it to reap. Visible
/// ONLY here, which is why it is a `warn` with the one command that reaches
/// the old root.
#[cfg(not(test))]
pub(super) fn legacy_mux_root_check() -> Check {
    // An explicit FNO_MUX_DIR relocates the sockets ON PURPOSE (the documented
    // test seam, scratch dirs, operator partitions), and a pinned FNO_CONFIG
    // is the same deliberate relocation from the other side: an isolated demo
    // env running doctor. Sessions at the global root are then live for every
    // normal client, and "restart them under the resolved root" would direct
    // an operator to kill a healthy fleet - into a tempdir, or out of the
    // real machine's mux. The check is about UPGRADE divergence only.
    if std::env::var_os("FNO_MUX_DIR").is_some_and(|v| !v.is_empty())
        || std::env::var_os("FNO_CONFIG").is_some_and(|v| !v.is_empty())
    {
        return Check {
            name: "legacy mux root".into(),
            verdict: Verdict::Na,
            detail: "FNO_MUX_DIR or a pinned FNO_CONFIG relocates the sockets on purpose".into(),
            remedy: None,
        };
    }
    // The same helper the resolver's own fallback uses, so this comparison
    // can never drift from what mux_dir actually falls back to. Canonical
    // forms catch a state_dir that aliases the legacy root through a
    // symlink or a `..` segment - lexically distinct, physically the same
    // dir, and warning there would direct an operator to kill a fleet that
    // never moved.
    let legacy = proto::legacy_mux_root();
    let resolved = proto::mux_dir();
    let same_root = match (
        std::fs::canonicalize(&resolved),
        std::fs::canonicalize(&legacy),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => resolved == legacy,
    };
    if same_root {
        return Check {
            name: "legacy mux root".into(),
            verdict: Verdict::Na,
            detail: "resolved dir is the legacy root".into(),
            remedy: None,
        };
    }
    let socks = std::fs::read_dir(&legacy)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
                .count()
        })
        .unwrap_or(0);
    if socks == 0 {
        return Check {
            name: "legacy mux root".into(),
            verdict: Verdict::Na,
            detail: "no sessions at the legacy root".into(),
            remedy: None,
        };
    }
    Check {
        name: "legacy mux root".into(),
        verdict: Verdict::Warn,
        detail: format!(
            "{socks} session socket(s) sit at the pre-config-chain root {} this \
             process no longer resolves",
            legacy.display()
        ),
        remedy: Some(format!(
            "FNO_MUX_DIR='{}' fno mux ls; kill-server or restart them under the \
             resolved root",
            legacy.display()
        )),
    }
}

/// What the backlog board would be scoped to (x-20f1).
///
/// Resolves LIVE, the same way a client does at spawn, so this answers "what
/// will a server started from here show". A server already running latched its
/// scope when it was spawned; changing the config takes a `mux kill-server`,
/// and this line is where that is visible.
///
/// Read-only and advisory: a refusal is a `warn`, not a `fail`, because nothing
/// is broken - the board just falls back to every project. But it must be
/// VISIBLE here, because that fallback is silent everywhere else: a board
/// correctly scoped to one project and a board that could not resolve one and
/// widened look nothing alike, and only this line says which you got.
pub(super) fn board_scope_check() -> Check {
    let (scope, why) = crate::backlog_view::resolve_board_scope(crate::server::config_get);
    let refused = matches!(
        &scope,
        crate::backlog_view::BoardScope::Projects(s) if s.is_empty()
    );
    Check {
        name: "backlog board scope".into(),
        verdict: if refused { Verdict::Warn } else { Verdict::Ok },
        detail: why,
        remedy: refused
            .then(|| "set config.project.id in this repo, or config.mux.board_scope=all".into()),
    }
}
