//! Harness-aware ownership posture for the autonomous dispatch guard (x-3e70).
//!
//! The abi-loop / megawalk dispatchers default every worker to `claude`. When a
//! foreign harness (codex / gemini) is already working a node - or is about to,
//! inside its own worktree, before its claim lands - a default-claude dispatch
//! stampedes it (observed 2026-07-09: a claude worker was dispatched onto a node
//! a codex thread already owned, because the claim was harness-blind and the
//! provider defaulted to claude). This module resolves a node's ownership
//! posture from the legible signals so the shared node-acquire chokepoint can
//! DEFER to a foreign owner instead of stampeding it.

use std::path::Path;

/// Ownership posture of a candidate node relative to the dispatcher's own harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipPosture {
    /// No foreign-harness signal: dispatch as today (native default preserved).
    Native,
    /// A different harness owns / is working this node; the dispatcher must
    /// defer to it rather than default-spawn its own worker.
    Foreign { harness: String },
}

/// A harness tag counts as foreign when it names a real, different harness.
/// `unknown`/empty (a pre-change claim, or a markerless daemon claim) is NOT
/// foreign on its own - it falls through to the branch/cwd signals, then native.
fn is_foreign(harness: &str, own: &str) -> bool {
    !harness.is_empty() && harness != "unknown" && harness != own
}

/// The foreign-harness owner of a branch, e.g. `codex/x-efc7` -> `codex`.
/// Requires the `<harness>/` prefix exactly (a `codexfoo/...` branch is not a
/// match), mirroring the plan's `codex/*` / `gemini/*` signal.
fn branch_harness(branch: &str) -> Option<&'static str> {
    for h in ["codex", "gemini"] {
        if branch
            .strip_prefix(h)
            .and_then(|r| r.strip_prefix('/'))
            .is_some()
        {
            return Some(h);
        }
    }
    None
}

/// The foreign-harness owner of a worktree path, e.g. a cwd under
/// `~/.codex/worktrees` -> `codex`.
fn cwd_harness(cwd: &str) -> Option<&'static str> {
    // Component match (not a substring) so `~/.codex` with no trailing slash
    // still hits and an unrelated `.codexbar` dir never does.
    let path = Path::new(cwd);
    if path.components().any(|c| c.as_os_str() == ".codex") {
        Some("codex")
    } else if path.components().any(|c| c.as_os_str() == ".gemini") {
        Some("gemini")
    } else {
        None
    }
}

/// Resolve a node's ownership posture from three signals (first hit wins):
///  1. an existing claim tagged with a harness != `own_harness` (the caller
///     passes the harness ONLY from a Live OR Suspect claim, so a *suspect*
///     foreign claim is treated as owned - AC3);
///  2. the node's branch carries a foreign-harness prefix (`codex/`, `gemini/`);
///  3. the node's worktree cwd sits under a foreign-harness worktree root.
///
/// A node with no foreign signal is `Native` (AC5). A claim harness of
/// `unknown`/absent never forces foreign on its own (AC6).
pub fn resolve_ownership_posture(
    claim_harness: Option<&str>,
    branch: Option<&str>,
    cwd: Option<&str>,
    own_harness: &str,
) -> OwnershipPosture {
    if let Some(h) = claim_harness {
        if is_foreign(h, own_harness) {
            return OwnershipPosture::Foreign {
                harness: h.to_string(),
            };
        }
    }
    if let Some(h) = branch.and_then(branch_harness) {
        if h != own_harness {
            return OwnershipPosture::Foreign {
                harness: h.to_string(),
            };
        }
    }
    if let Some(h) = cwd.and_then(cwd_harness) {
        if h != own_harness {
            return OwnershipPosture::Foreign {
                harness: h.to_string(),
            };
        }
    }
    OwnershipPosture::Native
}

/// Best-effort: the foreign harness that owns / is working `node`, or `None`
/// when the dispatcher's own `own_harness` may proceed. Gathers the claim tag
/// (Live or Suspect) plus a bounded `git worktree list` probe for a foreign
/// branch referencing the node, then classifies via [`resolve_ownership_posture`].
///
/// Degrades to `None` (dispatch as today) on any read error - the guard never
/// blocks dispatch on a probe failure, and never fakes a foreign owner.
pub fn foreign_owner_of(node: &str, own_harness: &str, repo_cwd: &Path) -> Option<String> {
    foreign_owner_of_in(node, own_harness, repo_cwd, None)
}

/// Testable core of [`foreign_owner_of`]: `claims_root` pins the claims dir so
/// the claim-read signal is exercised without touching the global root.
fn foreign_owner_of_in(
    node: &str,
    own_harness: &str,
    repo_cwd: &Path,
    claims_root: Option<&Path>,
) -> Option<String> {
    let claim_harness = claim_harness_for(node, claims_root);
    let (branch, cwd) = foreign_worktree_signal(node, repo_cwd);
    match resolve_ownership_posture(
        claim_harness.as_deref(),
        branch.as_deref(),
        cwd.as_deref(),
        own_harness,
    ) {
        OwnershipPosture::Foreign { harness } => Some(harness),
        OwnershipPosture::Native => None,
    }
}

/// Emit the `dispatch_deferred` audit event naming the foreign owner, into the
/// project's `events.jsonl` under `repo_cwd`. Best-effort (never fatal) and
/// un-schema'd, matching the sibling `active_backlog_*` operational events.
pub fn emit_dispatch_deferred(
    node: &str,
    owner_harness: &str,
    dispatcher_harness: &str,
    repo_cwd: &Path,
) {
    let emitter =
        crate::events::EventEmitter::new(repo_cwd.join(".fno").join("events.jsonl"), "megawalk");
    let mut f = serde_json::Map::new();
    f.insert("node_id".into(), node.into());
    f.insert("owner_harness".into(), owner_harness.into());
    f.insert("dispatcher_harness".into(), dispatcher_harness.into());
    let _ = emitter.emit_fields("dispatch_deferred", f);
}

/// The harness tag on a node's claim, but ONLY when the claim is Live or Suspect
/// (an owned slot; a suspect claim is never stolen, so it counts as owned - AC3).
/// Stale/free/corrupted -> `None`.
fn claim_harness_for(node: &str, claims_root: Option<&Path>) -> Option<String> {
    use crate::claims::{status, ClaimState};
    let (state, rec) = status(&format!("node:{node}"), claims_root);
    match state {
        ClaimState::Live | ClaimState::Suspect => rec.and_then(|r| r.harness),
        _ => None,
    }
}

/// Scan `git worktree list --porcelain` (run in `repo_cwd`) for a linked
/// worktree whose branch carries a foreign-harness prefix (or whose path sits
/// under a foreign worktree root) AND references `node`. Returns `(branch, cwd)`
/// for the first hit so the pure resolver classifies it; `(None, None)` on no
/// hit or any error. A foreign run in a *separate clone* (not a linked worktree)
/// is invisible here and falls back to the claim signal - best-effort by design.
fn foreign_worktree_signal(node: &str, repo_cwd: &Path) -> (Option<String>, Option<String>) {
    let out = std::process::Command::new("git")
        .current_dir(repo_cwd)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(out) = out else {
        return (None, None);
    };
    if !out.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Porcelain: per-worktree blocks; each has a `worktree <path>` line and
    // (for a branch checkout) a `branch refs/heads/<b>` line.
    let mut cur_path: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            cur_path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            let branch = b.strip_prefix("refs/heads/").unwrap_or(b);
            let path = cur_path.clone().unwrap_or_default();
            let foreign = branch_harness(branch).is_some() || cwd_harness(&path).is_some();
            // Node linkage rides the branch (a codex worktree's branch encodes
            // the node, e.g. `codex/x-efc7`); the path rarely carries the id.
            if foreign && branch.contains(node) {
                return (Some(branch.to_string()), Some(path));
            }
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC1-HP: a foreign-harness claim (the caller passes it from a Live OR
    // Suspect claim - AC3 rides this same path) is respected.
    #[test]
    fn ac1_foreign_claim_is_foreign() {
        assert_eq!(
            resolve_ownership_posture(Some("codex"), None, None, "claude"),
            OwnershipPosture::Foreign {
                harness: "codex".into()
            }
        );
    }

    // AC2-HP: no claim, but a codex branch in a codex worktree -> foreign.
    #[test]
    fn ac2_foreign_branch_and_cwd_is_foreign() {
        let cwd = "/Users/x/.codex/worktrees/0290/footnote";
        assert_eq!(
            resolve_ownership_posture(None, Some("codex/x-efc7"), Some(cwd), "claude"),
            OwnershipPosture::Foreign {
                harness: "codex".into()
            }
        );
        // cwd signal alone (branch not foreign-prefixed) still fires.
        assert_eq!(
            resolve_ownership_posture(None, Some("main"), Some(cwd), "claude"),
            OwnershipPosture::Foreign {
                harness: "codex".into()
            }
        );
    }

    // AC5-FR: a native node with no foreign signal dispatches as today.
    #[test]
    fn ac5_native_node_is_native() {
        assert_eq!(
            resolve_ownership_posture(None, Some("main"), Some("/Users/x/repo"), "claude"),
            OwnershipPosture::Native
        );
        // A claim by our OWN harness is not foreign.
        assert_eq!(
            resolve_ownership_posture(Some("claude"), None, None, "claude"),
            OwnershipPosture::Native
        );
    }

    // AC6-FR: an unknown/absent claim harness never forces foreign on its own.
    #[test]
    fn ac6_unknown_claim_is_not_foreign() {
        assert_eq!(
            resolve_ownership_posture(Some("unknown"), None, None, "claude"),
            OwnershipPosture::Native
        );
        assert_eq!(
            resolve_ownership_posture(Some(""), None, None, "claude"),
            OwnershipPosture::Native
        );
        assert_eq!(
            resolve_ownership_posture(None, None, None, "claude"),
            OwnershipPosture::Native
        );
    }

    // A gemini dispatcher over a codex node still defers; over its OWN gemini
    // branch it does not (own harness is never foreign to itself).
    #[test]
    fn own_harness_is_relative_to_dispatcher() {
        assert_eq!(
            resolve_ownership_posture(None, Some("codex/x-1"), None, "gemini"),
            OwnershipPosture::Foreign {
                harness: "codex".into()
            }
        );
        assert_eq!(
            resolve_ownership_posture(None, Some("gemini/x-1"), None, "gemini"),
            OwnershipPosture::Native
        );
    }

    // AC3-EDGE: the claim-read signal treats a SUSPECT foreign claim as owned
    // (TTL unexpired, pid unproven), and a STALE (expired) one as free again.
    #[test]
    fn claim_read_treats_suspect_foreign_as_owned_stale_as_free() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let key = "node:x-suspect";
        let path = crate::claims::claim_path(key, Some(root)).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // Suspect: TTL far in the future + a host that is never live locally.
        std::fs::write(
            &path,
            format!(
                "schema_version: 1\nkey: {key}\nholder: target-session:cx\nacquired_at: {now}\npid: 424242\nhost: no-such-host-xyz\nexpires_at: {}\nharness: codex\n",
                now + 3_600_000
            ),
        )
        .unwrap();
        // A non-repo cwd makes the git probe a no-op, isolating the claim signal.
        assert_eq!(
            foreign_owner_of_in("x-suspect", "claude", root, Some(root)),
            Some("codex".to_string())
        );
        // Expire the claim -> Stale -> no longer an owner (dispatch may proceed).
        std::fs::write(
            &path,
            format!(
                "schema_version: 1\nkey: {key}\nholder: target-session:cx\nacquired_at: {}\npid: 424242\nhost: no-such-host-xyz\nexpires_at: {}\nharness: codex\n",
                now - 7_200_000,
                now - 3_600_000
            ),
        )
        .unwrap();
        assert_eq!(
            foreign_owner_of_in("x-suspect", "claude", root, Some(root)),
            None
        );
    }

    // Prefix must be exact: `codexfoo/...` is NOT a codex-owned branch.
    #[test]
    fn branch_prefix_requires_slash_boundary() {
        assert_eq!(branch_harness("codex/x"), Some("codex"));
        assert_eq!(branch_harness("gemini/x-9"), Some("gemini"));
        assert_eq!(branch_harness("codexfoo/x"), None);
        assert_eq!(branch_harness("feature/codex/x"), None);
    }
}
