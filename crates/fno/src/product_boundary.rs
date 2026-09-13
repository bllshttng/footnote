//! One declaration of what each product surface requires of the others,
//! plus bounded, read-only observations of the optional components.
//!
//! The native workspace (this crate) never links the agent runtime: the
//! pairing is process and protocol, not a library edge. This module is the
//! SINGLE classifier for "is the optional component there, and what is the
//! repair when it is not". Callers project these observations into their own
//! surfaces (mux doctor, refusal messages); none of them re-derive
//! availability from error prose. Resolution itself stays with its existing
//! owners: the paired-binary resolver in `digest_overlay` and the worker
//! resolver in `store_client`.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// The four shipped binaries. The plugin is markdown and ships no binary.
pub const FNO_BIN: &str = "fno";
pub const AGENT_RUNTIME_BIN: &str = "fno-agents";
pub const AGENT_DAEMON_BIN: &str = "fno-agents-daemon";
pub const GRAPH_WORKER_BIN: &str = "fno-agents-worker";
/// The Python porcelain the native mux forwards delivery verbs to.
pub const PYTHON_CLI: &str = "python3 (fno cli)";

/// How an optional component presented itself when observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Found where its existing resolver looks.
    Available,
    /// Not found at observation time.
    Unavailable,
    /// Found but refusing to interoperate (version or protocol mismatch).
    Incompatible,
    /// Not probed: a read-only diagnostic does not execute the component or
    /// spend a round trip on it.
    Unmeasured,
}

impl Availability {
    /// The word doctor prints for this availability.
    pub fn word(self) -> &'static str {
        match self {
            Availability::Available => "available",
            Availability::Unavailable => "unavailable",
            Availability::Incompatible => "incompatible",
            Availability::Unmeasured => "unmeasured",
        }
    }
}

/// Workspace operation classes and what each one needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Shell, split, detach, reattach, terminal diagnostics: native, needs
    /// no optional component.
    PlainWorkspace,
    /// Attach-time overlays (digest, feed, court, needs) and the agent
    /// lifecycle views: shell out to the runtime binary.
    AgentLifecycle,
    /// Keeper-backed graph operations and the verbs the native mux forwards
    /// to the Python porcelain.
    DeliveryBackedView,
}

/// The ONE operation-to-component table. Doctor renders it; refusal paths
/// quote the same names. A plain workspace needs none of the optional
/// components, which is why missing ones stay advisory there.
pub fn required_components(surface: Surface) -> &'static [&'static str] {
    match surface {
        Surface::PlainWorkspace => &[],
        Surface::AgentLifecycle => &[AGENT_RUNTIME_BIN],
        Surface::DeliveryBackedView => &[GRAPH_WORKER_BIN, AGENT_RUNTIME_BIN, PYTHON_CLI],
    }
}

/// One bounded, read-only observation of an optional component. `observed_at`
/// is epoch seconds. `reason` is prose for a human line; nothing re-parses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub component: &'static str,
    /// The surface that breaks without this component.
    pub requirement: &'static str,
    pub availability: Availability,
    pub reason: String,
    pub observed_at: u64,
}

impl Observation {
    /// The supported repair, or none when nothing is broken or unprobed.
    pub fn remedy(&self) -> Option<&'static str> {
        match self.availability {
            Availability::Available | Availability::Unmeasured => None,
            Availability::Unavailable => Some(self.unavailable_repair()),
            Availability::Incompatible => {
                Some("restart the mux server so both sides re-resolve the component")
            }
        }
    }

    fn unavailable_repair(&self) -> &'static str {
        match self.component {
            GRAPH_WORKER_BIN => "set FNO_AGENTS_WORKER or install the runtime",
            AGENT_RUNTIME_BIN => "set FNO_AGENTS_BIN or install the runtime",
            _ => "install the component",
        }
    }
}

/// The single classified refusal for a requested graph operation when the
/// worker binary is absent. store_client returns it verbatim, so the receipt
/// names the component and the repair without a second classifier.
pub fn graph_worker_missing_error() -> String {
    format!("{GRAPH_WORKER_BIN} not found (set FNO_AGENTS_WORKER or install the runtime)")
}

/// The classified refusal when the worker binary exists but cannot spawn.
/// The receipt names the component and the underlying reason either way.
pub fn graph_worker_spawn_error(bin: &Path, err: std::io::Error) -> String {
    format!("cannot spawn {GRAPH_WORKER_BIN} ({}): {err}", bin.display())
}

/// The ONE PATH walk for a paired binary. store_client's worker resolver and
/// the runtime observation both land here.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Resolve the runtime binary the way attach does, then settle a bare PATH
/// fallback against PATH itself so the observation never reads cwd.
fn resolve_runtime_bin() -> PathBuf {
    let resolved = crate::digest_overlay::fno_agents_bin();
    if resolved == Path::new(AGENT_RUNTIME_BIN) {
        find_on_path(AGENT_RUNTIME_BIN).unwrap_or(resolved)
    } else {
        resolved
    }
}

fn runtime_availability(bin: &Path) -> (Availability, String) {
    if bin.exists() {
        (
            Availability::Available,
            format!("resolved {}", bin.display()),
        )
    } else {
        (
            Availability::Unavailable,
            format!("{AGENT_RUNTIME_BIN} not found"),
        )
    }
}

/// Observe the agent runtime. Existence only: no execution, no version probe.
pub fn agent_runtime_observation() -> Observation {
    agent_runtime_observation_at(crate::digest_overlay::now_secs())
}

pub(crate) fn agent_runtime_observation_at(now: u64) -> Observation {
    let bin = resolve_runtime_bin();
    let (availability, reason) = runtime_availability(&bin);
    Observation {
        component: AGENT_RUNTIME_BIN,
        requirement: "attach-time overlays (digest, feed, court, needs) and agent lifecycle views",
        availability,
        reason,
        observed_at: now,
    }
}

/// Observe the graph worker through store_client's own resolver.
pub fn graph_worker_observation() -> Observation {
    graph_worker_observation_at(crate::digest_overlay::now_secs())
}

pub(crate) fn graph_worker_observation_at(now: u64) -> Observation {
    let (availability, reason) = match crate::store_client::worker_binary() {
        Some(p) => (Availability::Available, format!("resolved {}", p.display())),
        None => (Availability::Unavailable, graph_worker_missing_error()),
    };
    Observation {
        component: GRAPH_WORKER_BIN,
        requirement: "native graph operations (board rank, defer) and every keeper-backed read",
        availability,
        reason,
        observed_at: now,
    }
}

/// The Python backend is deliberately UNMEASURED: native workspace verbs
/// never invoke it, and probing an interpreter from a read-only diagnostic
/// would spend exactly the kind of execution this module exists to avoid.
pub fn python_cli_observation() -> Observation {
    python_cli_observation_at(crate::digest_overlay::now_secs())
}

pub(crate) fn python_cli_observation_at(now: u64) -> Observation {
    Observation {
        component: PYTHON_CLI,
        requirement: "verbs the native mux forwards to the porcelain (delivery, backlog writes)",
        availability: Availability::Unmeasured,
        reason: "native workspace verbs never invoke it; not probed".into(),
        observed_at: now,
    }
}

/// The attach digest's distinct states. Disabled-by-config is a choice;
/// backend-unavailable is a broken promise. Doctor must never show the two
/// as the same line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestState {
    DisabledByConfig,
    BackendUnavailable,
    Available,
}

/// One bounded observation of the digest. Kept a separate type from
/// [`Observation`] because "disabled" is none of the four availability
/// verdicts: it is not a defect to repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DigestObservation {
    pub state: DigestState,
    pub reason: String,
    pub observed_at: u64,
}

/// Pure over (enabled, backend_present) so every order is testable without
/// config files. Disabled wins over missing: a knob the operator turned off
/// is not reported as a fault.
pub(crate) fn digest_state_for(
    enabled: bool,
    backend_present: bool,
) -> (DigestState, &'static str) {
    match (enabled, backend_present) {
        (false, _) => (
            DigestState::DisabledByConfig,
            "obsidian.enabled=false: the digest is off by choice",
        ),
        (true, false) => (
            DigestState::BackendUnavailable,
            "the digest is enabled but fno-agents could not be resolved",
        ),
        (true, true) => (DigestState::Available, "the digest backend resolved"),
    }
}

/// Observe the digest from the doctor's cwd, through the config owner
/// (`digest_overlay::ObsidianCfg`) and the same runtime resolver as above.
pub fn digest_observation(cwd: &Path) -> DigestObservation {
    digest_observation_at(
        &crate::digest_overlay::ObsidianCfg::read(cwd),
        crate::digest_overlay::now_secs(),
    )
}

pub(crate) fn digest_observation_at(
    cfg: &crate::digest_overlay::ObsidianCfg,
    now: u64,
) -> DigestObservation {
    let backend_present = resolve_runtime_bin().exists();
    let (state, why) = digest_state_for(cfg.enabled, backend_present);
    DigestObservation {
        state,
        reason: why.to_string(),
        observed_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_serializes_snake_case_with_all_fields() {
        let obs = Observation {
            component: AGENT_RUNTIME_BIN,
            requirement: "test requirement",
            availability: Availability::Unmeasured,
            reason: "why".into(),
            observed_at: 1726000000,
        };
        let v = serde_json::to_value(&obs).unwrap();
        assert_eq!(v["component"], AGENT_RUNTIME_BIN);
        assert_eq!(v["availability"], "unmeasured");
        assert_eq!(v["observed_at"], 1726000000);
        assert_eq!(v["reason"], "why");
        assert_eq!(Availability::Incompatible.word(), "incompatible");
    }

    #[test]
    fn surfaces_declare_their_components_once() {
        assert!(required_components(Surface::PlainWorkspace).is_empty());
        assert_eq!(
            required_components(Surface::AgentLifecycle),
            [AGENT_RUNTIME_BIN]
        );
        let delivery = required_components(Surface::DeliveryBackedView);
        assert!(delivery.contains(&GRAPH_WORKER_BIN));
        assert!(delivery.contains(&AGENT_RUNTIME_BIN));
        assert!(delivery.contains(&PYTHON_CLI));
    }

    #[test]
    fn digest_distinguishes_disabled_from_unavailable() {
        let (state, _) = digest_state_for(false, true);
        assert_eq!(state, DigestState::DisabledByConfig);
        let (state, _) = digest_state_for(false, false);
        assert_eq!(state, DigestState::DisabledByConfig);
        let (state, _) = digest_state_for(true, false);
        assert_eq!(state, DigestState::BackendUnavailable);
        let (state, _) = digest_state_for(true, true);
        assert_eq!(state, DigestState::Available);
    }

    #[test]
    fn runtime_observation_reports_present_and_missing() {
        let bin = std::env::temp_dir().join(format!("fno-pb-test-{}", std::process::id()));
        std::fs::write(&bin, b"").unwrap();
        assert_eq!(
            runtime_availability(&bin),
            (
                Availability::Available,
                format!("resolved {}", bin.display())
            )
        );
        let gone = bin.with_extension("missing");
        let (availability, reason) = runtime_availability(&gone);
        assert_eq!(availability, Availability::Unavailable);
        assert!(reason.contains("not found"), "{reason}");
        std::fs::remove_file(&bin).ok();
    }

    #[test]
    fn graph_worker_missing_error_names_component_and_repair() {
        let e = graph_worker_missing_error();
        assert!(e.contains(GRAPH_WORKER_BIN), "names the component: {e}");
        assert!(e.contains("FNO_AGENTS_WORKER"), "names the repair: {e}");
    }

    #[test]
    fn observations_carry_their_observation_time() {
        assert_eq!(agent_runtime_observation_at(4242).observed_at, 4242);
        assert_eq!(graph_worker_observation_at(4243).observed_at, 4243);
        assert_eq!(python_cli_observation_at(4244).observed_at, 4244);
        let cfg = crate::digest_overlay::ObsidianCfg::default();
        assert_eq!(digest_observation_at(&cfg, 4245).observed_at, 4245);
    }

    #[test]
    fn unavailable_worker_observation_carries_the_classified_error() {
        // When the resolver misses, the observation's reason IS the refusal
        // store_client returns: one classifier, two projections.
        if crate::store_client::worker_binary().is_none() {
            let obs = graph_worker_observation_at(0);
            assert_eq!(obs.availability, Availability::Unavailable);
            assert_eq!(obs.reason, graph_worker_missing_error());
        }
    }
}
