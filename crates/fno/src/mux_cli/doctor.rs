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
            name: "workspace store (squads.json)".into(),
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

/// `fno mux doctor`'s canonical-venv check: a worktree install once
/// rewrote the CANONICAL checkout's `cli/.venv` console scripts with the
/// worktree's interpreter, and every deployed script died with the pruned
/// worktree. Every script shebang in the canonical venv's bin must name an
/// interpreter inside the canonical checkout. Read-only; Na on a machine
/// with no canonical cli venv.
pub(super) fn canonical_venv_check() -> Check {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let repo_root = crate::digest_overlay::repo_root_from(&cwd);
    let root = crate::digest_overlay::canonical_root_with(
        &repo_root,
        crate::digest_overlay::canonical_suppressed_by_env(),
    )
    .unwrap_or(repo_root);
    let bin = root.join("cli/.venv/bin");
    if !bin.is_dir() {
        return Check {
            name: "canonical cli venv".into(),
            verdict: Verdict::Na,
            detail: format!("no cli/.venv under {}", root.display()),
            remedy: None,
        };
    }
    let offenders = venv_shebang_offenders(&bin, &root);
    if offenders.is_empty() {
        Check {
            name: "canonical cli venv".into(),
            verdict: Verdict::Ok,
            detail: format!(
                "every script shebang in {} sits inside {}",
                bin.display(),
                root.display()
            ),
            remedy: None,
        }
    } else {
        let mut listed = String::new();
        for (i, (script, interp)) in offenders.iter().enumerate() {
            if i == 4 {
                listed.push_str(&format!(" and {} more", offenders.len() - 4));
                break;
            }
            if i > 0 {
                listed.push_str("; ");
            }
            listed.push_str(&format!("{script} -> {interp}"));
        }
        Check {
            name: "canonical cli venv".into(),
            verdict: Verdict::Fail,
            detail: format!(
                "venv script(s) name an interpreter outside {}: {}",
                root.display(),
                listed
            ),
            remedy: Some(format!("cd {}/cli && uv sync", root.display())),
        }
    }
}

/// Script name + interpreter for every regular file in `bin` whose `#!` names
/// an interpreter outside `root`. Symlinks skip (the venv's own
/// `python3 -> python` chain is venv-internal, whatever it resolves to);
/// `env`-form and relative shebangs skip (they name no absolute tree, so they
/// cannot name the pruned worktree). The comparison is lexical against
/// `root` plus the canonicalized root when it resolves, so a `/tmp` vs
/// `/private/tmp` alias never reads as an offender and a shebang spelled
/// through a symlink into `root` never reads as clean.
pub(super) fn venv_shebang_offenders(bin: &Path, root: &Path) -> Vec<(String, String)> {
    use std::io::Read;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(bin) else {
        return out;
    };
    let alt_root = std::fs::canonicalize(root).ok();
    for entry in entries.filter_map(Result::ok) {
        let Ok(meta) = entry.file_type() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mut file = match std::fs::File::open(entry.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut head = [0u8; 512];
        let n = std::io::Read::read(&mut file, &mut head).unwrap_or(0);
        if !head[..n].starts_with(b"#!") {
            continue;
        }
        let line_end = head[..n].iter().position(|&b| b == b'\n').unwrap_or(n);
        let interp = std::str::from_utf8(&head[2..line_end])
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        if !interp.starts_with('/') || interp.rsplit('/').next() == Some("env") {
            continue;
        }
        let interp_path = std::path::Path::new(interp);
        let inside = interp_path.starts_with(root)
            || alt_root
                .as_deref()
                .is_some_and(|r| interp_path.starts_with(r));
        if !inside {
            out.push((
                entry.file_name().to_string_lossy().into_owned(),
                interp.to_string(),
            ));
        }
    }
    out
}

/// What the backlog board would be scoped to.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_text_lines_are_single_line_with_verdict() {
        // AC6-UI: every finding is one line carrying its verdict word.
        let c = Check {
            name: "socket-dir".into(),
            verdict: Verdict::Warn,
            detail: "mode 755".into(),
            remedy: Some("chmod 700".into()),
        };
        // Render captures stdout only in an integration harness; here assert the
        // verdict vocabulary the line is built from stays stable.
        assert_eq!(c.verdict.word(), "warn");
        assert_eq!(Verdict::Ok.word(), "ok");
        assert_eq!(Verdict::Fail.word(), "fail");
        assert_eq!(Verdict::Na.word(), "n/a");

        // The canonical-venv scan: a shebang inside the root passes,
        // any absolute shebang outside is named (even a system shim, which
        // factually points outside the checkout), env-form skips.
        let td = tempfile::tempdir().unwrap();
        let bin = td.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let root = td.path();
        std::fs::write(
            bin.join("good"),
            format!("#!{}/venv/python3\n", root.display()),
        )
        .unwrap();
        std::fs::write(bin.join("envform"), "#!/usr/bin/env python3\n").unwrap();
        std::fs::write(bin.join("shim"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(bin.join("rogue"), "#!/wt/pruned/bin/python3\n").unwrap();
        let offenders = venv_shebang_offenders(&bin, root);
        assert_eq!(offenders.len(), 2);
        assert!(offenders
            .iter()
            .any(|(s, i)| s == "rogue" && i == "/wt/pruned/bin/python3"));
        assert!(offenders.iter().any(|(s, _)| s == "shim"));
    }
}
