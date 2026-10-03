//! The shared-registry write guard: the two refusals every Rust registry
//! write routes through at [`crate::state::update_registry`], and Python's
//! `write_registry` mirrors in `cli/src/fno/agents/registry.py`.
//!
//! 2026-09-27 a worker's probe called Python's `write_registry` with no
//! pinned state dir and replaced the live `~/.fno/agents/registry.json` with
//! a 1-row fixture; every team stamp since the last backup was lost. Two
//! independent arms, either of which stops that class:
//!
//! 1. A process carrying a test or probe marker (`cargo test`,
//!    `PYTEST_CURRENT_TEST`, `FNO_TEST_HERMETIC=1`) never writes the real
//!    shared registry. The remedy is pinning the process's own state root
//!    (`FNO_AGENTS_HOME`), not an override - the same move-the-target
//!    philosophy as the source-ahead fence.
//! 2. A write that drops most live rows is a fixture or a bug, not a roster
//!    update. Refused unless `FNO_REGISTRY_ALLOW_ROW_LOSS=1` names the drop
//!    deliberate.
//!
//! The guard stands down unless the target IS the ambient shared registry
//! and the home is not a tempdir sandbox: a pinned `FNO_AGENTS_HOME` or a
//! hermetic `HOME` is the caller's own store, which keeps every existing
//! test's writes legal.

use std::ffi::OsStr;
use std::path::Path;

use crate::state::RegistryEntry;
use crate::AgentStatus;

/// The row-loss override, shared verbatim with the Python mirror.
pub const ROW_LOSS_OVERRIDE_ENV: &str = "FNO_REGISTRY_ALLOW_ROW_LOSS";

/// The guard's inputs, all explicit so tests construct the case instead of
/// hoping the ambient environment reproduces it (same shape as
/// `source_ahead_root`).
pub struct GuardInputs<'a> {
    /// The file the write targets.
    pub target: &'a Path,
    /// The ambient `$HOME` the process resolved its root from.
    pub home: Option<&'a OsStr>,
    pub cfg_test: bool,
    pub pytest_current_test: bool,
    pub hermetic_claim: bool,
    pub row_loss_override: bool,
    pub before_live: usize,
    pub after_live: usize,
}

/// Decide the write. `Ok(())` proceeds; the `Err` text names the remedy.
pub fn check(inp: GuardInputs) -> Result<(), String> {
    let Some(home) = inp.home else {
        return Ok(());
    };
    let shared = crate::paths::AgentsHome::ambient_from(home).registry_json();
    if crate::paths::under_temp_dir(&shared) || !same_path(inp.target, &shared) {
        return Ok(());
    }
    if inp.cfg_test || inp.pytest_current_test || inp.hermetic_claim {
        return Err(format!(
            "refusing a write of the shared registry at {}: this process runs \
             under a test or probe marker, and fleet state is not a fixture. \
             Pin the process's own state root (FNO_AGENTS_HOME) instead.",
            shared.display()
        ));
    }
    if inp.row_loss_override {
        return Ok(());
    }
    if inp.before_live >= 2 && inp.after_live * 2 < inp.before_live {
        return Err(format!(
            "refusing a write of the shared registry at {} that drops live \
             rows from {} to {}: a write this destructive is a fixture or a \
             bug, not a roster update. Set {ROW_LOSS_OVERRIDE_ENV}=1 when the \
             drop is deliberate.",
            shared.display(),
            inp.before_live,
            inp.after_live
        ));
    }
    Ok(())
}

/// The env-reading entry [`crate::state::update_registry`] calls.
pub fn check_env(target: &Path, before_live: usize, after_live: usize) -> Result<(), String> {
    check(GuardInputs {
        target,
        home: std::env::var_os("HOME").as_deref(),
        cfg_test: cfg!(test),
        pytest_current_test: std::env::var_os("PYTEST_CURRENT_TEST").is_some(),
        hermetic_claim: std::env::var_os("FNO_TEST_HERMETIC").as_deref() == Some(OsStr::new("1")),
        row_loss_override: std::env::var_os(ROW_LOSS_OVERRIDE_ENV).as_deref()
            == Some(OsStr::new("1")),
        before_live,
        after_live,
    })
}

/// Live rows by the `OWNERSHIP_LIVE_STATUSES` vocabulary, the same six
/// statuses Python's mirror counts. A row that moved to a terminal status is
/// not a loss; a row removed while still owning a session is.
pub fn count_live(entries: &[RegistryEntry]) -> usize {
    entries
        .iter()
        .filter(|e| {
            matches!(
                e.status,
                AgentStatus::Spawning
                    | AgentStatus::Ready
                    | AgentStatus::Idle
                    | AgentStatus::Busy
                    | AgentStatus::Live
                    | AgentStatus::Restarting
            )
        })
        .count()
}

/// Canonicalized-equal, tolerating an absent file (a fresh install) by
/// falling back to the raw comparison - the same resolve-or-identity the
/// schema-bump guard uses.
fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn inputs(target: &Path, before: usize, after: usize) -> GuardInputs<'_> {
        GuardInputs {
            target,
            home: Some(OsStr::new("/home/operator")),
            cfg_test: false,
            pytest_current_test: false,
            hermetic_claim: false,
            row_loss_override: false,
            before_live: before,
            after_live: after,
        }
    }

    fn shared_target() -> PathBuf {
        Path::new("/home/operator")
            .join(".fno")
            .join("agents")
            .join("registry.json")
    }

    #[test]
    fn a_cargo_test_process_refuses_the_shared_root() {
        let target = shared_target();
        let err = check(GuardInputs {
            cfg_test: true,
            ..inputs(&target, 33, 1)
        })
        .unwrap_err();
        assert!(err.contains("test or probe marker"), "{err}");
        assert!(err.contains("FNO_AGENTS_HOME"), "{err}");
    }

    #[test]
    fn a_pytest_probe_marker_refuses_too() {
        let target = shared_target();
        assert!(check(GuardInputs {
            pytest_current_test: true,
            ..inputs(&target, 33, 1)
        })
        .is_err());
    }

    #[test]
    fn a_mass_live_row_drop_refuses_without_the_override() {
        let target = shared_target();
        let err = check(inputs(&target, 33, 1)).unwrap_err();
        assert!(err.contains("drops live rows from 33 to 1"), "{err}");
        assert!(err.contains(ROW_LOSS_OVERRIDE_ENV), "{err}");
    }

    #[test]
    fn the_override_admits_a_deliberate_drop() {
        let target = shared_target();
        assert!(check(GuardInputs {
            row_loss_override: true,
            ..inputs(&target, 33, 1)
        })
        .is_ok());
    }

    #[test]
    fn a_write_that_keeps_most_live_rows_proceeds() {
        let target = shared_target();
        assert!(check(inputs(&target, 6, 4)).is_ok());
        assert!(check(inputs(&target, 2, 1)).is_ok());
        assert!(check(inputs(&target, 1, 0)).is_ok());
    }

    #[test]
    fn a_sandboxed_home_stands_down() {
        let home = std::env::temp_dir().join("guard-sandbox-home");
        let target = home.join(".fno").join("agents").join("registry.json");
        assert!(check(GuardInputs {
            cfg_test: true,
            home: Some(home.as_os_str()),
            ..inputs(&target, 33, 1)
        })
        .is_ok());
    }

    #[test]
    fn a_pinned_or_explicit_other_target_is_the_callers_own_store() {
        let pinned = Path::new("/tmp/scratch/registry.json");
        assert!(check(GuardInputs {
            cfg_test: true,
            ..inputs(pinned, 33, 1)
        })
        .is_ok());
        assert!(check(GuardInputs {
            home: None,
            cfg_test: true,
            ..inputs(&shared_target(), 33, 1)
        })
        .is_ok());
    }

    #[test]
    fn count_live_matches_the_six_status_vocabulary() {
        let rows = [
            AgentStatus::Spawning,
            AgentStatus::Ready,
            AgentStatus::Idle,
            AgentStatus::Busy,
            AgentStatus::Live,
            AgentStatus::Restarting,
            AgentStatus::Exited,
            AgentStatus::Orphaned,
            AgentStatus::Failed,
            AgentStatus::PermanentDead,
        ]
        .map(|status| RegistryEntry {
            status,
            ..Default::default()
        });
        assert_eq!(count_live(&rows), 6);
    }
}
