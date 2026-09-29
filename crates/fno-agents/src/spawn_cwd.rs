//! Spawn cwd resolution for the client dispatch sites: the `--cwd` /
//! `--here` / default-canonical precedence, the redirect note, and the
//! node-named spawn guard's refusal-or-project answer. One module so the
//! client binary's own file budget stops carrying the whole cluster.

use serde_json::Value;

/// Canonicalize a `--cwd` string to an absolute path, matching Python's
/// `Path(cwd).resolve()`: prefer `std::fs::canonicalize`, falling back to a
/// join against the caller cwd for a relative path that does not exist yet.
/// Extracted from the previously-duplicated claude-ask / spawn cwd blocks.
pub fn canonicalize_cwd(c: &str) -> std::path::PathBuf {
    std::fs::canonicalize(c).unwrap_or_else(|_| {
        let p = std::path::PathBuf::from(c);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
        }
    })
}

/// Read the `fresh` / `here` booleans a caller set via `--fresh` /
/// `--here`(`--in-place`). Both default to false: `--fresh` is an opt-in
/// mechanism, never on by default at the client layer (the policy layer decides
/// when to pass it -- AC3 keeps non-target verbs on caller cwd unless asked).
pub fn fresh_here_flags(params: &Value) -> (bool, bool) {
    let fresh = params
        .get("fresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let here = params
        .get("here")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (fresh, here)
}

/// One-line stderr note when the default (or `--fresh` alias) actually moves the
/// worker cwd off the caller's dir, so the redirect is never silent on any path,
/// default included (Locked Decision 5; Failure Modes > Errors).
pub fn note_fresh_redirect(caller: &std::path::Path, chosen: &std::path::Path) {
    if chosen != caller {
        eprintln!(
            "fno-agents: dispatching from canonical main (default) ({}); pass --here to stay in this worktree",
            chosen.display()
        );
    }
}

/// Pure cwd precedence for a spawn/ask dispatch: explicit `--cwd` > `--here`
/// (caller) > default canonical. inverted the default: with no explicit
/// cwd source the worker lands on the canonical root, so the identical command
/// behaves the same regardless of where the launcher stands; `--here` is the
/// explicit opt-in to keep the caller's cwd. `--fresh` is an accepted no-op
/// alias (the default already resolves canonical). An unresolved canonical
/// (None) falls back to the caller cwd, the safe side. No git / env / IO, so the
/// precedence is unit-testable (Failure Modes > Invariants: `--cwd` is the
/// highest-priority cwd source and wins over everything).
pub fn effective_worker_cwd(
    explicit_cwd: Option<std::path::PathBuf>,
    _fresh: bool,
    here: bool,
    canonical: Option<std::path::PathBuf>,
    caller: std::path::PathBuf,
) -> std::path::PathBuf {
    if let Some(c) = explicit_cwd {
        return c; // explicit --cwd always wins
    }
    if here {
        return caller; // --here: explicit opt-in to the caller's cwd
    }
    canonical.unwrap_or(caller) // default: canonical; caller on resolution failure
}

/// Resolve the worker cwd for a client-side (claude/codex) spawn/ask dispatch,
/// honoring `--cwd` > `--here` (caller) > default canonical. Shells to git only
/// on the default path (no `--cwd`, no `--here`); emits the redirect note on an
/// actual move. Returns `(cwd, moved)` where `moved` is exactly the note
/// condition, so a caller surfacing `cwd` in a receipt couples to the note with
/// no second, divergent comparison (; gemini review). Single source of cwd
/// truth for the two client-side dispatch blocks (claude `ask`, claude `spawn`).
pub fn resolve_dispatch_cwd(params: &Value) -> (std::path::PathBuf, bool) {
    let caller = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let explicit = params
        .get("cwd")
        .and_then(|v| v.as_str())
        // An empty --cwd is absent, never the empty-string path (Failure Modes >
        // Boundaries; Python's `if cwd:` twin). Without this, canonicalize_cwd("")
        // resolves to the caller dir and suppresses the canonical default -- the
        // exact worktree leak this change prevents (review).
        .filter(|s| !s.is_empty())
        .map(canonicalize_cwd);
    let (fresh, here) = fresh_here_flags(params);
    // Default path (no explicit --cwd, no --here) resolves canonical; --fresh is
    // now a no-op alias since canonical IS the default.
    let default_path = explicit.is_none() && !here;
    let canonical = if default_path {
        crate::paths::canonical_repo_root(&caller)
    } else {
        None
    };
    let chosen = effective_worker_cwd(explicit.clone(), fresh, here, canonical, caller.clone());
    let moved = default_path && chosen != caller;
    if moved {
        note_fresh_redirect(&caller, &chosen);
    }
    (chosen, moved)
}

/// The node-named spawn's cwd guard for both client dispatch sites: a
/// foreign-repo caller is refused (`Err` names both paths), a node-named
/// spawn dispatches from its project (the redirect note fires here), and an
/// unnamed spawn answers `Ok(None)` for the site's own default.
pub fn node_cwd_or_refuse(
    params: &Value,
    caller: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, String> {
    match crate::node_seed::spawn_node_cwd(params, caller) {
        crate::node_seed::SpawnNodeCwd::Foreign(msg) => Err(msg),
        crate::node_seed::SpawnNodeCwd::Project(project) => {
            note_fresh_redirect(caller, &project);
            Ok(Some(project))
        }
        crate::node_seed::SpawnNodeCwd::Unnamed => Ok(None),
    }
}
