//! A foreground harness typed into an fno pane proves which pane it runs in,
//! so the session report can bind the registry row there.
//!
//! `FNO_PANE` is inherited by anything a pane process spawns, including a
//! long-lived daemon merely BORN in a pane once. [`from_parts`] tells a real
//! pane-hosted session apart from a stale inherited env: the nearest harness
//! ancestor must not be claude pool machinery
//! ([`crate::spawn_context::session_identity_from_table`] already refuses
//! that), its own parent must be a real process (not a detached shared host,
//! ppid 0 or 1), and no ancestor above that may itself be a harness.

use crate::census::{descendants, ProcRow};
use crate::spawn_context;
use std::collections::BTreeMap;

/// Mirrors `spawn_context::MAX_ANCESTRY_DEPTH` (private there).
const MAX_ANCESTRY_DEPTH: usize = 25;

/// A proven pane claim: the mux session name and pane id a foreground
/// harness is running under right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneClaim {
    pub session: String,
    pub pane_id: u64,
}

/// True when some process strictly above `parent_pid` is itself a harness:
/// this session is nested inside another harness's pane turn (a Bash tool
/// running `claude -p ...` in the same pane), so the OUTER session owns the
/// pane.
fn nested_in_another_harness(table: &BTreeMap<u32, ProcRow>, parent_pid: u32) -> bool {
    let mut pid = parent_pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        let Some(row) = table.get(&pid) else {
            return false;
        };
        if row.ppid == 0 || row.ppid == pid {
            return false;
        }
        let Some(prow) = table.get(&row.ppid) else {
            return false;
        };
        if spawn_context::harness_name_of_command(&prow.command).is_some() {
            return true;
        }
        pid = row.ppid;
    }
    false
}

/// The pure decision, over an injected env getter and census table (cheaply
/// unit-testable): a claim only when `FNO_PANE` trims to digits only;
/// `FNO_SERVER`, else `FNO_SESSION`, is non-empty (same precedence as the
/// pane-host pin, `hook/session_state.rs`); the ancestry walk names a
/// non-pool-machinery harness; collapsing further ancestors that walk also
/// names as the SAME harness (one launch's own wrapper layers), the topmost
/// one's parent is a real process; and no process above THAT parent is
/// itself a harness.
pub fn from_parts<F: Fn(&str) -> Option<String>>(
    get: F,
    table: &BTreeMap<u32, ProcRow>,
    start_pid: u32,
) -> Option<PaneClaim> {
    let pane_raw = get("FNO_PANE")?;
    let pane_raw = pane_raw.trim();
    if pane_raw.is_empty() || !pane_raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let pane_id: u64 = pane_raw.parse().ok()?;

    let session = get("FNO_SERVER")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| get("FNO_SESSION").filter(|s| !s.trim().is_empty()))?;

    let (harness_pid, harness) = spawn_context::session_identity_from_table(table, start_pid)?;

    // Collapse consecutive ancestors the walk also names as this SAME
    // harness: a harness's own wrapper layers are one launch, not nesting.
    let mut collapsed = harness_pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        let Some(row) = table.get(&collapsed) else {
            break;
        };
        if row.ppid == 0 || row.ppid == collapsed {
            break;
        }
        let Some(prow) = table.get(&row.ppid) else {
            break;
        };
        if spawn_context::harness_name_of_command(&prow.command) != Some(harness) {
            break;
        }
        collapsed = row.ppid;
    }

    let parent_pid = table.get(&collapsed)?.ppid;
    if parent_pid == 0 || parent_pid == 1 || nested_in_another_harness(table, parent_pid) {
        return None;
    }

    Some(PaneClaim {
        session: session.trim().to_string(),
        pane_id,
    })
}

/// The ambient wrapper: reads the table only after `FNO_PANE` is present, so
/// a session outside any pane pays no census cost.
pub fn ambient() -> Option<PaneClaim> {
    std::env::var("FNO_PANE")
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    let table = spawn_context::ancestry_table();
    let start_pid = unsafe { libc::getppid() } as u32;
    from_parts(|k| std::env::var(k).ok(), &table, start_pid)
}

/// One keeper-hosted pane's observation: the mux session and pane id a bind
/// would name, the keeper's child pid (the root of that pane's process
/// tree), and the pane's working directory in both recorded forms (the
/// shell's PWD spelling and the resolved one, `/tmp` vs `/private/tmp`;
/// either may be empty).
pub struct PaneRow {
    pub session: String,
    pub pane_id: u64,
    pub child_pid: u32,
    pub cwd: String,
    pub alt_cwd: String,
}

/// True when `root` itself or any strict descendant of it is an opencode TUI
/// client: a process the harness-name reader calls opencode whose command
/// line is not the shared `serve` daemon (someone may run `opencode serve`
/// inside a pane; a server is not a TUI).
fn subtree_has_opencode_tui(table: &[ProcRow], root: u32) -> bool {
    let mut pids = vec![root];
    pids.extend(descendants(table, root));
    pids.iter().any(|pid| {
        table.iter().find(|row| row.pid == *pid).is_some_and(|row| {
            spawn_context::harness_name_of_command(&row.command) == Some("opencode")
                && !row.command.contains(" serve")
        })
    })
}

/// The pane-side decision, over injected keeper rows and a census table.
/// `from_parts` walks UP from the reporter, which can never work for a
/// harness whose plugin reports from a per-machine daemon: opencode's
/// nearest harness ancestor is `opencode.exe serve --service`, parented by
/// pid 1. This walks DOWN instead. The panes whose subtree runs an opencode
/// TUI and whose recorded working directory holds `report_cwd` are
/// candidates; exactly one binds. Zero and two-or-more refuse, because a
/// silent guess would point the row at the wrong pane. A dead `child_pid`
/// (absent from the table) is never a candidate.
pub fn from_pane_side(panes: &[PaneRow], table: &[ProcRow], report_cwd: &str) -> Option<PaneClaim> {
    let report_cwd = report_cwd.trim();
    if report_cwd.is_empty() {
        return None;
    }
    let candidates: Vec<&PaneRow> = panes
        .iter()
        .filter(|pane| {
            (pane.cwd == report_cwd || pane.alt_cwd == report_cwd)
                && subtree_has_opencode_tui(table, pane.child_pid)
        })
        .collect();
    if candidates.len() != 1 {
        return None;
    }
    let pane = candidates[0];
    Some(PaneClaim {
        session: pane.session.clone(),
        pane_id: pane.pane_id,
    })
}

/// Binds `entries[idx]` to `claim`: sets its `mux` and clears `short_id` (the
/// one-live-ref invariant, `state::validate_single_live_ref`) in one write,
/// and clears `mux` on every OTHER `operator`-origin row already on the same
/// claim (a later foreground session takes the pointer from a stale one; a
/// spawned row's mux is never touched). Returns `(old, new)` only when the
/// target row's mux changed, so a repeat report writes nothing new.
pub fn bind(
    entries: &mut [crate::state::RegistryEntry],
    idx: usize,
    claim: &PaneClaim,
) -> Option<(Option<crate::state::MuxRef>, crate::state::MuxRef)> {
    let new_mux = crate::state::MuxRef {
        session: claim.session.clone(),
        pane_id: claim.pane_id,
    };
    if entries[idx].mux.as_ref() == Some(&new_mux) {
        return None;
    }
    let old = entries[idx].mux.clone();
    for (i, entry) in entries.iter_mut().enumerate() {
        if i != idx
            && entry.origin.as_deref() == Some("operator")
            && entry.mux.as_ref() == Some(&new_mux)
        {
            entry.mux = None;
        }
    }
    entries[idx].mux = Some(new_mux.clone());
    entries[idx].short_id.clear();
    Some((old, new_mux))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tbl(rows: &[(u32, u32, &str)]) -> BTreeMap<u32, ProcRow> {
        rows.iter()
            .map(|&(pid, ppid, command)| {
                (
                    pid,
                    ProcRow {
                        pid,
                        ppid,
                        state: 'S',
                        elapsed_s: 0,
                        cpu_pct: 0.0,
                        rss_kb: 0,
                        command: command.to_string(),
                    },
                )
            })
            .collect()
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    const PLAIN: &[(u32, u32, &str)] = &[
        (1, 0, "/sbin/launchd"),
        (10, 1, "/bin/zsh -il"),
        (20, 10, "codex"),
        (30, 20, "bash /some/hook.sh"),
        (40, 30, "fno-agents report --kind session"),
    ];

    /// AC2-HP: FNO_PANE=7, FNO_SERVER=main, zsh -> codex -> bash -> fno-agents
    /// -> session main, pane 7.
    #[test]
    fn happy_path_binds_to_pane() {
        let table = tbl(PLAIN);
        let claim = from_parts(
            env(&[("FNO_PANE", "7"), ("FNO_SERVER", "main")]),
            &table,
            40,
        )
        .expect("must bind");
        assert_eq!(
            claim,
            PaneClaim {
                session: "main".into(),
                pane_id: 7
            }
        );
    }

    /// AC2-ERR: nested under another claude (claude -> zsh -> claude -> bash
    /// -> fno-agents) -> None.
    #[test]
    fn nested_harness_refuses() {
        let table = tbl(&[
            (1, 0, "/sbin/launchd"),
            (5, 1, "/Users/x/.local/bin/claude"),
            (10, 5, "/bin/zsh -il"),
            (20, 10, "/Users/x/.local/bin/claude"),
            (30, 20, "bash /some/hook.sh"),
            (40, 30, "fno-agents report --kind session"),
        ]);
        let get = env(&[("FNO_PANE", "7"), ("FNO_SERVER", "main")]);
        assert_eq!(from_parts(get, &table, 40), None);
    }

    /// AC2-ERR: under `claude bg-pty-host` pool machinery -> None, via
    /// `session_identity_from_table`'s own refusal.
    #[test]
    fn claude_pool_machinery_refuses() {
        let table = tbl(&[
            (1, 0, "/sbin/launchd"),
            (5, 1, "claude bg-pty-host --bg-pty-host /tmp/x.pty.sock 200 50 -- claude --bg-spare /tmp/x.claim.sock"),
            (30, 5, "bash /some/hook.sh"),
            (40, 30, "fno-agents report --kind session"),
        ]);
        let get = env(&[("FNO_PANE", "7"), ("FNO_SERVER", "main")]);
        assert_eq!(from_parts(get, &table, 40), None);
    }

    /// AC2-ERR: FNO_PANE is non-numeric (`%7`, a uuid) -> None, never a parse.
    #[test]
    fn non_numeric_pane_refuses() {
        let table = tbl(PLAIN);
        let get = env(&[("FNO_PANE", "%7"), ("FNO_SERVER", "main")]);
        assert_eq!(from_parts(get, &table, 40), None);
        let get_uuid = env(&[
            ("FNO_PANE", "9f1b1e4e-1111-2222-3333-444444444444"),
            ("FNO_SERVER", "main"),
        ]);
        assert_eq!(from_parts(get_uuid, &table, 40), None);
    }

    /// A detached shared host: the harness's own parent is ppid 1 (a
    /// long-lived daemon started once inside a pane) -> None.
    #[test]
    fn detached_shared_host_refuses() {
        let table = tbl(&[
            (1, 0, "/sbin/launchd"),
            (5, 1, "opencode.exe serve --service"),
            (40, 5, "fno-agents report --kind session"),
        ]);
        let get = env(&[("FNO_PANE", "74"), ("FNO_SERVER", "main")]);
        assert_eq!(from_parts(get, &table, 40), None);
    }

    fn rows(v: &[(u32, u32, &str)]) -> Vec<ProcRow> {
        v.iter()
            .map(|&(pid, ppid, command)| crate::census::test_proc_row(pid, ppid, command))
            .collect()
    }

    fn pane(session: &str, pane_id: u64, child: u32, cwd: &str) -> PaneRow {
        PaneRow {
            session: session.into(),
            pane_id,
            child_pid: child,
            cwd: cwd.into(),
            alt_cwd: String::new(),
        }
    }

    /// AC1-HP: one pane whose subtree holds an opencode TUI at the report's
    /// cwd returns a claim naming that pane.
    #[test]
    fn pane_side_happy_path_binds() {
        let table = rows(&[
            (1, 0, "/sbin/launchd"),
            (100, 1, "fno-agents-worker --pane"),
            (110, 100, "/bin/zsh -il"),
            (200, 110, "opencode.exe"),
        ]);
        let panes = vec![pane("main", 3, 100, "/w")];
        assert_eq!(
            from_pane_side(&panes, &table, "/w"),
            Some(PaneClaim {
                session: "main".into(),
                pane_id: 3
            })
        );
    }

    /// The TUI may be the pane's direct child (`mux pane run -- opencode`).
    #[test]
    fn pane_side_tui_as_direct_child_binds() {
        let table = rows(&[(1, 0, "/sbin/launchd"), (100, 1, "opencode.exe")]);
        let panes = vec![pane("main", 3, 100, "/w")];
        assert!(from_pane_side(&panes, &table, "/w").is_some());
    }

    /// Each candidate-filter half refuses alone: the pane's recorded cwd
    /// must hold the report's cwd, and the subtree must actually run an
    /// opencode TUI (a claude pane in the same directory never wins).
    #[test]
    fn pane_side_filter_halves_refuse() {
        let tui_table = rows(&[
            (1, 0, "/sbin/launchd"),
            (100, 1, "fno-agents-worker --pane"),
            (110, 100, "/bin/zsh -il"),
            (200, 110, "opencode.exe"),
        ]);
        let panes = vec![pane("main", 3, 100, "/w")];
        assert_eq!(from_pane_side(&panes, &tui_table, "/other"), None);
        let claude_table = rows(&[
            (1, 0, "/sbin/launchd"),
            (100, 1, "fno-agents-worker --pane"),
            (110, 100, "/bin/zsh -il"),
            (210, 110, "claude"),
        ]);
        assert_eq!(from_pane_side(&panes, &claude_table, "/w"), None);
    }

    /// Two opencode TUIs in two panes at one cwd are ambiguous: refuse
    /// rather than guess.
    #[test]
    fn pane_side_ambiguous_panes_refuse() {
        let table = rows(&[
            (1, 0, "/sbin/launchd"),
            (100, 1, "fno-agents-worker --pane"),
            (110, 100, "/bin/zsh -il"),
            (200, 110, "opencode.exe"),
            (300, 1, "fno-agents-worker --pane"),
            (310, 300, "/bin/zsh -il"),
            (400, 310, "opencode.exe"),
        ]);
        let panes = vec![pane("main", 3, 100, "/w"), pane("main", 5, 300, "/w")];
        assert_eq!(from_pane_side(&panes, &table, "/w"), None);
    }

    /// Each subtree guard refuses alone: a keeper row naming a pid the
    /// census lacks is a dead tree, and an `opencode serve` inside a pane
    /// is a server, not a TUI.
    #[test]
    fn pane_side_subtree_guards_refuse() {
        let table = rows(&[(1, 0, "/sbin/launchd")]);
        let panes = vec![pane("main", 3, 999, "/w")];
        assert_eq!(from_pane_side(&panes, &table, "/w"), None);
        let serve_table = rows(&[
            (1, 0, "/sbin/launchd"),
            (100, 1, "fno-agents-worker --pane"),
            (110, 100, "opencode.exe serve --service"),
        ]);
        let panes = vec![pane("main", 3, 100, "/w")];
        assert_eq!(from_pane_side(&panes, &serve_table, "/w"), None);
    }
}
