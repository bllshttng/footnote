//! `fno agents grid` - client-side TUI compositor (ab-3c063856).
//!
//! Tiles N PTY-managed agents (codex / gemini) side by side as live watcher
//! panes; one focused pane is promotable to a driver session on Enter and
//! released on Esc. The interaction model is hybrid **watch + take-over**:
//! every pane is read-only by default, the focused pane becomes a full driver
//! when Enter is pressed, and Esc demotes it back to a watcher. This is the
//! "single pane of glass" for a fleet of running agents.
//!
//! ## Composition strategy
//!
//! v1 composes **client-side only**: the client opens one watcher WebSocket
//! per agent (existing `agent.drive` RPC with `mode: "watch"`) and tiles the
//! streams locally. The daemon is untouched (Locked Decision #2). A daemon-
//! side composite is a tracked follow-up justified by thin-client / remote /
//! headless needs.
//!
//! ## Render substrate
//!
//! Each pane parses its PTY byte stream with `alacritty_terminal` (the
//! crate the plan's Locked Decision #3 named). An earlier revision used
//! `vt100` on a "dependency weight" rationale that turned out to be wrong
//! for `alacritty_terminal` 0.26 - its transitive tree is `vte` (the same
//! parser `vt100` wraps) plus a handful of small crates, NOT `winit` or a
//! Windows GUI stack. The richer cell model (`Flags`, `NamedColor`/`Rgb`,
//! `Dimensions`) is exactly what per-cell compositing needs, so the whole
//! crate standardized on it (ab-3c063856 review). Cell extraction walks
//! `Term::grid()` per [`grid::pane`] into the compositor's render buffer.
//!
//! ## At-most-one driver invariant
//!
//! The compositor holds **at most one driver claim** at any time. The global
//! [`Mode`] is single-valued (WATCH or DRIVE), so there is never N-way driver
//! contention to coordinate - take-over is serialized through the single
//! focused pane. Esc, WS-drop, agent-exit, and `q` all release the claim
//! before any subsequent action.
//!
//! ## claude is out of scope for v1
//!
//! Locked Decision #6: claude is self-supervised (`claude --bg` owns its own
//! bg daemon + rendezvous socket) and is therefore not abi-PTY-driveable.
//! A claude-in-a-panel feature rides `ab-cc926b4e` and the claude-native
//! agent-view surface - not this compositor.

use crate::paths::AgentsHome;

pub mod group;
pub mod layout;
pub mod palette;
pub mod pane;
pub mod repo;
pub mod run;
pub mod state;

/// Default soft cap on the number of panes a grid will tile (Locked Decision
/// 5 / fu-grid-pagination). The grid keeps every agent's watcher WS open for
/// the whole session (eager; Locked Decision 3), so the connection count
/// scales with the fleet. Above this cap the grid renders the first N and
/// warns explicitly rather than opening unbounded connections. 32 is a
/// starting point tuned against realistic megatron-wave fleet sizes
/// (single-digit to low-double-digit agents); see Claude's Discretion 6.
pub const DEFAULT_MAX_PANES: usize = 32;

/// Resolve the soft pane cap. `FNO_GRID_MAX_PANES` overrides the default
/// (the crate has no settings-file reader yet; the plan's `config.grid.max_panes`
/// maps to this env override for now). A value of 0 or an unparseable value
/// falls back to [`DEFAULT_MAX_PANES`].
pub fn max_panes() -> usize {
    std::env::var("FNO_GRID_MAX_PANES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_PANES)
}

/// Apply the soft fleet cap to the resolved agent list (Locked Decision 5;
/// Open Question 1 = warn-and-truncate so the grid still works). Returns the
/// (possibly truncated) names and `Some(warning)` when truncation happened.
/// Pure over its inputs so it is unit-testable.
pub fn apply_soft_cap(mut names: Vec<String>, max: usize) -> (Vec<String>, Option<String>) {
    let total = names.len();
    let max = max.max(1);
    if total <= max {
        return (names, None);
    }
    names.truncate(max);
    let warning = format!(
        "fleet too large for grid ({total} > {max}); showing first {max} (set FNO_GRID_MAX_PANES to raise the cap)"
    );
    (names, Some(warning))
}

/// Errors surfaced by `fno agents grid` argument parsing. Kept separate from
/// runtime errors so the CLI dispatch layer can map them to exit code 2
/// ("usage error") without chasing through a generic `Box<dyn Error>`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GridArgError {
    #[error("fno-agents grid: --all takes no positional names (got: {0})")]
    AllWithNames(String),

    #[error("fno-agents grid: unknown flag: {0}")]
    UnknownFlag(String),
}

/// Parsed grid CLI arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridArgs {
    /// Explicit agent names to tile, in operator order. Empty when [`all`] is
    /// set.
    pub names: Vec<String>,
    /// `--all` selects every PTY-managed running agent from the registry.
    /// Resolution happens at runtime against [`AgentsHome::registry_path`].
    pub all: bool,
    /// Left navigation rail listing "sidelines" - one repo's agents each
    /// (ab-1fab1fdf Phase 1; "sideline" is the footnote term, not herdr's "space").
    /// E5c Locked Decision 2 made this the default: a bare `grid` / `grid --all`
    /// drops into the rail (grouped by cwd), and explicit names or `--no-rail`
    /// fall back to the railless tiled grid.
    pub rail: bool,
    /// Initial group-by key for the rail (`--group-by cwd|session|provider|status`).
    /// Only meaningful when `rail` is true. Defaults to `cwd` when absent.
    pub group_by: Option<String>,
}

impl GridArgs {
    /// Parse the raw verb-stripped argv tail (the slice after `agents grid`).
    ///
    /// The grammar is intentionally tiny:
    ///
    /// ```text
    /// grid                                 # bare → live fleet sidelines (rail); E5b front door if the fleet is empty
    /// grid --all                           # every PTY-managed agent, rail on
    /// grid <name>...                       # explicit names, railless (escape hatch)
    /// grid --no-rail                       # fleet, railless tiled grid
    /// grid [--all|<name>...] --rail [--group-by <key>]
    /// ```
    ///
    /// Mixing positional names with `--all` is rejected so the operator never
    /// types `grid --all extra` and silently sees `extra` ignored.
    pub fn parse(argv: &[String]) -> Result<Self, GridArgError> {
        let mut names = Vec::new();
        let mut all = false;
        // `None` = no explicit rail flag; the default is resolved below from
        // whether explicit names were given (E5c Locked Decision 2).
        let mut rail_override: Option<bool> = None;
        let mut group_by: Option<String> = None;
        let mut iter = argv.iter().peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--all" => all = true,
                // Last rail flag wins; --rail / --no-rail force the choice.
                "--rail" => rail_override = Some(true),
                "--no-rail" => rail_override = Some(false),
                "--group-by" => {
                    // Consume the next token as the key value, but only if it is
                    // NOT another flag - `--group-by --no-rail` must not eat the
                    // flag as the key (codex peer P3). A missing/flag value is
                    // silently ignored (group_by defaults to cwd downstream).
                    if let Some(key) = iter.peek() {
                        if !key.starts_with("--") {
                            group_by = Some((*key).clone());
                            iter.next();
                        }
                    }
                }
                s if s.starts_with("--") => return Err(GridArgError::UnknownFlag(s.to_string())),
                s => names.push(s.to_string()),
            }
        }
        if all && !names.is_empty() {
            return Err(GridArgError::AllWithNames(names.join(" ")));
        }
        // E5c Locked Decision 2: a bare invocation (no names, no `--all`)
        // defaults to the live fleet, so `fno agents grid` shows the sidelines
        // (one per repo) over running agents. When that fleet resolves EMPTY at runtime,
        // the run loop falls through to E5b's zero-config goal-launcher front
        // door (the operator types a goal and the grid spawns a `/target`
        // worker) instead of an empty grid.
        if names.is_empty() && !all {
            all = true;
        }
        // Rail defaults ON for the fleet/sidelines view (no explicit names) and
        // OFF when explicit names are given (the railless escape hatch).
        // `--rail` / `--no-rail` override the default either way.
        let rail = rail_override.unwrap_or(names.is_empty());
        Ok(GridArgs {
            names,
            all,
            rail,
            group_by,
        })
    }

    /// Resolve the initial `GroupKey` from `--group-by`. Unknown values default
    /// to `Cwd` (defensive; the caller never panics on a bad flag value).
    pub fn initial_group_key(&self) -> group::GroupKey {
        match self.group_by.as_deref() {
            Some("session") => group::GroupKey::Session,
            Some("provider") => group::GroupKey::Provider,
            Some("status") => group::GroupKey::Status,
            _ => group::GroupKey::Cwd,
        }
    }
}

/// `fno agents grid <name...> [--all]` entrypoint.
///
/// Parses arguments, then hands off to the live run loop in [`run`], which
/// opens one watcher WebSocket per agent, tiles them with the [`layout`]
/// manager, renders via [`pane`]'s snapshots, and drives the
/// [`state::Compositor`] / [`state::ConnState`] FSMs from `crossterm` key
/// events. Enter on a focused pane opens a take-over (interactive) drive
/// connection; Esc releases it.
pub async fn run_grid(args: &[String], home: &AgentsHome) -> i32 {
    let parsed = match GridArgs::parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    run::run(parsed, home).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_one_name() {
        let a = GridArgs::parse(&argv(&["wkA"])).unwrap();
        assert_eq!(a.names, vec!["wkA".to_string()]);
        assert!(!a.all);
    }

    #[test]
    fn parses_multiple_names() {
        let a = GridArgs::parse(&argv(&["wkA", "wkB", "wkC"])).unwrap();
        assert_eq!(a.names, vec!["wkA", "wkB", "wkC"]);
        assert!(!a.all);
    }

    #[test]
    fn parses_all_flag() {
        let a = GridArgs::parse(&argv(&["--all"])).unwrap();
        assert!(a.names.is_empty());
        assert!(a.all);
    }

    // ── Rail-default tests (E5c AC-E5c-1) ────────────────────────────────
    // Locked Decision 2: bare `grid` drops into the rail-grouped-by-cwd view
    // over all live agents; explicit names / `--no-rail` are the escape hatch.

    #[test]
    fn bare_defaults_to_all_rail_cwd() {
        // `fno agents grid` with no args → fleet view, rail on, grouped by cwd.
        // (When the resolved fleet is empty, the run loop shows E5b's front-door
        // launcher instead - that is a runtime behavior, not a parse contract.)
        let a = GridArgs::parse(&[]).unwrap();
        assert!(a.names.is_empty());
        assert!(a.all, "bare invocation defaults to --all (the live fleet)");
        assert!(a.rail, "bare invocation defaults to the rail (sidelines)");
        assert_eq!(a.initial_group_key(), group::GroupKey::Cwd);
    }

    #[test]
    fn all_flag_defaults_rail_on() {
        let a = GridArgs::parse(&argv(&["--all"])).unwrap();
        assert!(a.all);
        assert!(
            a.rail,
            "--all with no names defaults rail on (fleet = sidelines)"
        );
    }

    #[test]
    fn explicit_names_disable_rail() {
        // Explicit names are the escape hatch: railless tiled grid by default.
        let a = GridArgs::parse(&argv(&["wkA", "wkB"])).unwrap();
        assert!(!a.all);
        assert!(!a.rail, "explicit names default to the railless tiled grid");
    }

    #[test]
    fn no_rail_flag_forces_off_over_fleet() {
        // `--no-rail` alone → fleet (all) but railless tiled.
        let a = GridArgs::parse(&argv(&["--no-rail"])).unwrap();
        assert!(a.all, "no names + no --all still defaults to the fleet");
        assert!(!a.rail, "--no-rail forces the railless grid");
    }

    #[test]
    fn no_rail_flag_overrides_default_with_names() {
        let a = GridArgs::parse(&argv(&["wkA", "--no-rail"])).unwrap();
        assert_eq!(a.names, vec!["wkA".to_string()]);
        assert!(!a.rail);
    }

    #[test]
    fn rail_flag_forces_on_with_names() {
        // `--rail` overrides the explicit-names default of rail-off.
        let a = GridArgs::parse(&argv(&["wkA", "--rail"])).unwrap();
        assert!(a.rail, "--rail forces the rail on even with explicit names");
    }

    #[test]
    fn conflicting_rail_flags_last_wins() {
        let on_last = GridArgs::parse(&argv(&["--no-rail", "--rail"])).unwrap();
        assert!(on_last.rail, "last rail flag wins (--rail last → on)");
        let off_last = GridArgs::parse(&argv(&["--rail", "--no-rail"])).unwrap();
        assert!(!off_last.rail, "last rail flag wins (--no-rail last → off)");
    }

    #[test]
    fn group_by_does_not_consume_a_following_flag() {
        // `--group-by --no-rail`: the flag must not be eaten as the key value
        // (codex peer P3). group_by stays default; --no-rail still applies.
        let a = GridArgs::parse(&argv(&["--all", "--group-by", "--no-rail"])).unwrap();
        assert_eq!(a.group_by, None, "a flag is not a valid --group-by value");
        assert!(!a.rail, "--no-rail still takes effect");
    }

    #[test]
    fn rejects_all_with_names() {
        let err = GridArgs::parse(&argv(&["--all", "wkA"])).unwrap_err();
        assert!(matches!(err, GridArgError::AllWithNames(_)));
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = GridArgs::parse(&argv(&["--whoops", "wkA"])).unwrap_err();
        assert!(matches!(err, GridArgError::UnknownFlag(s) if s == "--whoops"));
    }

    // ── Soft cap tests (fu-grid-pagination, task 4.2 / Locked Decision 5) ─

    #[test]
    fn soft_cap_passes_through_under_limit() {
        let names = argv(&["a", "b", "c"]);
        let (out, warn) = apply_soft_cap(names.clone(), 32);
        assert_eq!(out, names);
        assert!(warn.is_none());
    }

    #[test]
    fn soft_cap_passes_through_at_limit() {
        let names = argv(&["a", "b", "c", "d"]);
        let (out, warn) = apply_soft_cap(names.clone(), 4);
        assert_eq!(out, names);
        assert!(warn.is_none(), "exactly at the cap must not warn");
    }

    #[test]
    fn soft_cap_truncates_and_warns_over_limit() {
        let names = argv(&["a", "b", "c", "d", "e"]);
        let (out, warn) = apply_soft_cap(names, 3);
        assert_eq!(out, argv(&["a", "b", "c"]), "keeps first N in order");
        let w = warn.expect("over-cap must warn");
        assert!(w.contains("5 > 3"), "warning names the counts, got: {w}");
        assert!(w.contains("showing first 3"));
    }

    #[test]
    fn soft_cap_clamps_zero_max_to_one() {
        let names = argv(&["a", "b"]);
        let (out, warn) = apply_soft_cap(names, 0);
        assert_eq!(out.len(), 1, "max 0 clamps to 1");
        assert!(warn.is_some());
    }

    // ── Rail flag tests (ab-1fab1fdf, Phase 1) ────────────────────────────

    #[test]
    fn parses_rail_flag() {
        let a = GridArgs::parse(&argv(&["wkA", "--rail"])).unwrap();
        assert!(a.rail, "--rail flag must be captured");
        assert!(!a.all);
    }

    #[test]
    fn rail_false_by_default() {
        let a = GridArgs::parse(&argv(&["wkA"])).unwrap();
        assert!(
            !a.rail,
            "explicit names default to railless (the escape hatch)"
        );
    }

    #[test]
    fn parses_rail_with_group_by_session() {
        let a = GridArgs::parse(&argv(&["--all", "--rail", "--group-by", "session"])).unwrap();
        assert!(a.rail);
        assert_eq!(a.group_by.as_deref(), Some("session"));
    }

    #[test]
    fn parses_rail_with_group_by_provider() {
        let a = GridArgs::parse(&argv(&["wkA", "--rail", "--group-by", "provider"])).unwrap();
        assert!(a.rail);
        assert_eq!(a.initial_group_key(), group::GroupKey::Provider);
    }

    #[test]
    fn initial_group_key_defaults_to_cwd() {
        let a = GridArgs::parse(&argv(&["wkA", "--rail"])).unwrap();
        assert_eq!(a.initial_group_key(), group::GroupKey::Cwd);
    }

    #[test]
    fn initial_group_key_unknown_value_defaults_to_cwd() {
        let a = GridArgs::parse(&argv(&["wkA", "--rail", "--group-by", "foobar"])).unwrap();
        assert_eq!(
            a.initial_group_key(),
            group::GroupKey::Cwd,
            "unknown --group-by defaults to cwd"
        );
    }

    #[test]
    fn rail_flag_with_all_flag() {
        let a = GridArgs::parse(&argv(&["--all", "--rail"])).unwrap();
        assert!(a.all);
        assert!(a.rail);
    }
}
