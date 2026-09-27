//! The spawn transaction: one owner of admission, launch,
//! birth persistence and the receipt.
//!
//! Sequence: validate -> acquire admission reservations -> allocate spawn_id
//! -> append the durable `agent_spawn_accepted` journal record -> launch one
//! backend -> persist the birth -> emit the birth event/receipt. The registry
//! write lock is never held while the backend starts a process; races are
//! rejected at insertion through the existing locked write path. Every call
//! allocates a fresh spawn_id: duplicate detection belongs to the producer
//! layer, which reads the durable accepted records it owns.

use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::spawn_contract::{validate, SpawnError, SpawnProvenance, SpawnRequest, ValidatedSpawn};
use crate::state::{update_registry, RegistryEntry};
use serde::{Deserialize, Serialize};

/// A spawn-attempt id: `sp-` plus 16 hex chars from the OS CSPRNG.
pub fn allocate_spawn_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sp-{hex}")
}

/// Distinguishable receipt facts: accepted, launched, registered, ready,
/// failed and recovery-required never collapse into one "live" word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnReceiptState {
    /// The request validated, reserved and journaled; no backend ran yet.
    Accepted,
    /// The backend reports the process/harness started.
    Launched,
    /// The birth row persisted durably.
    Registered,
    /// The backend observed readiness.
    Ready,
    /// A named failure, with the stage it failed at.
    Failed,
    /// The backend launched but persistence failed without provable cleanup:
    /// the attempt stays and the receipt names the recovery handles.
    RecoveryRequired,
}

/// Whether the child's full harness session id is known yet. A pending UUID
/// never delays or erases birth provenance (AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStatus {
    Resolved,
    Pending,
    NotApplicable,
}

/// What a backend observed when it started a worker. Backends return facts;
/// they cannot choose provenance, mint a birth row, or emit a birth event.
#[derive(Debug, Clone, Default)]
pub struct LaunchFacts {
    pub pid: Option<u32>,
    pub pid_start_time: Option<u64>,
    pub child_session_id: Option<String>,
    pub log_path: Option<String>,
    pub transport: Option<String>,
}

impl LaunchFacts {
    /// A launch that produced no handle an observer could resolve is a
    /// FAILED launch, not a registered one.
    pub fn has_handle(&self) -> bool {
        (self.pid.is_some() && self.pid_start_time.is_some())
            || self.log_path.as_deref().is_some_and(|p| !p.is_empty())
            || self
                .child_session_id
                .as_deref()
                .is_some_and(|s| !s.is_empty())
    }
}

/// The error a backend returns: `Failure` means nothing started (or the exact
/// child was proven cleaned up); `RecoveryRequired` means a child may exist
/// and the receipt names the handles needed to settle it.
#[derive(Debug, Clone)]
pub enum SpawnLaunchError {
    Failure(String),
    RecoveryRequired {
        facts: LaunchFacts,
        detail: String,
        pid: Option<u32>,
    },
}

/// The backend seam: one operation. Real lanes implement it;
/// the journey tests inject a spy.
pub trait SpawnBackend {
    fn launch(
        &self,
        launch: &ValidatedSpawn,
        spawn_id: &str,
    ) -> Result<LaunchFacts, SpawnLaunchError>;
}

/// Admission reservations (name, capacity, session) scoped through one trait
/// so the transaction stays backend-agnostic and fault injection can drop
/// them. The real impl wraps the existing locked name/capacity checks
/// (`daemon/codex_thread_lane.rs:141`, `daemon.rs:3534`).
pub trait SpawnClaims {
    /// Reserve the worker name; `Err` names the conflict. The guard releases
    /// the reservation on Drop (before-launch failures leave no residue).
    fn reserve_name(&self, name: &str) -> Result<NameReservation, String>;
}

pub struct NameReservation {
    name: String,
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl NameReservation {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The caller-supplied release closure runs on Drop unless `keep` runs
    /// first (on success the row itself becomes the record).
    pub fn new(name: &str, release: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            name: name.to_string(),
            release: Some(release),
        }
    }

    /// Explicitly keep the reservation (on success the row itself becomes the
    /// record); otherwise Drop releases it.
    pub fn keep(mut self) {
        self.release = None;
    }
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// The service handles the transaction needs. It carries existing service
/// handles and I/O dependencies; it is never a second source of parent,
/// owner, or default values - those arrive only on the request.
pub struct SpawnRuntime<'a> {
    pub home: &'a AgentsHome,
    pub emitter: &'a EventEmitter,
    pub backend: &'a dyn SpawnBackend,
    pub claims: &'a dyn SpawnClaims,
}

/// The receipt: a correlated statement of what is durably true about one
/// spawn attempt, naming spawn_id, effective source/owner, the known full
/// child session id, and whether identity is resolved, pending, or
/// not applicable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SpawnReceipt {
    pub spawn_id: String,
    pub state: SpawnReceiptState,
    pub name: String,
    pub provenance: SpawnProvenance,
    pub child_session_id: Option<String>,
    pub identity_status: IdentityStatus,
    pub detail: Option<String>,
}

/// The one spawn entry: validate, reserve, journal, launch, persist,
/// emit. `Err` names the defect and the stage it was caught at; no worker,
/// harness session, live row, or held admission reservation survives a
/// pre-launch refusal (AC2/AC5).
pub async fn spawn(
    request: &SpawnRequest,
    runtime: &SpawnRuntime<'_>,
) -> Result<SpawnReceipt, SpawnError> {
    // 1. Validation precedes every effect (AC2).
    let validated: ValidatedSpawn = validate(request)?;
    let name = validated.request().work.name.clone();
    let provenance = validated.provenance();

    // 2. Every attempt mints a fresh id; the durable accepted record is what
    //    a producer (or reconciliation) correlates retries against.
    let spawn_id = allocate_spawn_id();

    // 3. Admission: reserve the name; Drop releases on any early return.
    let _reservation = runtime
        .claims
        .reserve_name(&name)
        .map_err(|e| SpawnError::Contradiction(format!("admission refused: {e}")))?;

    // 4. The durable accepted record lands BEFORE any backend runs; it is not
    //    a birth. A crash between here and launch leaves a accepted record
    //    without a worker, which reconciliation reads as an unresolved
    //    attempt (never a live worker).
    let accepted_payload = serde_json::json!({
        "spawn_id": spawn_id,
        "name": name,
        "origin": serde_json::to_value(&provenance.origin)
            .map_err(|e| SpawnError::Malformed(format!("origin: {e}")))?,
        "owner": serde_json::to_value(&provenance.owner)
            .map_err(|e| SpawnError::Malformed(format!("owner: {e}")))?,
        "substrate": validated.request().how.substrate,
        "harness": validated.request().how.harness,
    });
    runtime
        .emitter
        .emit("agent_spawn_accepted", &accepted_payload)
        .map_err(|e| SpawnError::Malformed(format!("journal write failed: {e}")))?;

    // 5. Launch OUTSIDE any registry lock; the registry insertion later
    //    rejects races at the locked write.
    let facts = match runtime.backend.launch(&validated, &spawn_id) {
        Ok(facts) => facts,
        Err(SpawnLaunchError::Failure(detail)) => {
            let err = format!("spawn {spawn_id} failed at launch (nothing started): {detail}");
            let _ = runtime.emitter.emit(
                "agent_spawn_failed",
                &serde_json::json!({"spawn_id": spawn_id, "name": name, "reason": "launch_failed", "detail": err}),
            );
            return Err(SpawnError::Contradiction(err));
        }
        Err(SpawnLaunchError::RecoveryRequired { facts, detail, pid }) => {
            let err = format!(
                "spawn {spawn_id} launched then failed persistence (child may exist, pid={pid:?}): {detail}"
            );
            let _ = runtime.emitter.emit(
                "agent_spawn_failed",
                &serde_json::json!({"spawn_id": spawn_id, "name": name, "reason": "recovery_required", "detail": err}),
            );
            return Ok(receipt(
                spawn_id,
                SpawnReceiptState::RecoveryRequired,
                &name,
                provenance,
                facts.child_session_id.clone(),
                Some(err),
            ));
        }
    };

    // 6. A launch with no observable handle is not a birth.
    if !facts.has_handle() {
        let err = format!("spawn {spawn_id} launched but produced no resolvable handle");
        let _ = runtime.emitter.emit(
            "agent_spawn_failed",
            &serde_json::json!({"spawn_id": spawn_id, "name": name, "reason": "no_handle", "detail": err}),
        );
        return Err(SpawnError::Contradiction(err));
    }

    // 7. Persist the birth through the locked registry write, which rejects
    //    concurrent same-name insertions.
    let mut entry = RegistryEntry::new_spawn(&spawn_id, &provenance);
    entry.name = name.clone();
    entry.cwd = validated.request().work.cwd.clone();
    entry.harness = Some(validated.request().how.harness.clone());
    entry.substrate = Some(validated.request().how.substrate.clone());
    entry.status = crate::AgentStatus::Spawning;
    entry.node = validated.request().work.node.clone();
    apply_facts(&mut entry, &facts);
    let registry_path = runtime.home.registry_json();
    update_registry(
        &registry_path,
        |reg: &mut crate::state::Registry| -> Result<(), String> {
            if reg.entries.iter().any(|r| r.name == name) {
                return Err(format!(
                    "row '{name}' already exists; a concurrent spawn won the name"
                ));
            }
            reg.entries.push(entry.clone());
            Ok(())
        },
    )
    .map_err(|e: crate::state::StateError| {
        SpawnError::Contradiction(format!("birth persistence failed: {e}"))
    })?
    .map_err(|e: String| SpawnError::Contradiction(format!("birth persistence failed: {e}")))?;

    // 8. The durable birth event, correlated by spawn_id. The lineage is the
    //    same triple RegistryEntry::new_spawn derived from the provenance.
    let lineage = match &provenance.origin {
        crate::spawn_contract::SpawnOrigin::Session { parent, .. } => {
            crate::state::Lineage::captured((
                Some(parent.session_id.clone()),
                Some(parent.harness.clone()),
                Some(parent.cwd.clone()),
            ))
        }
        crate::spawn_contract::SpawnOrigin::NonSession { .. } => {
            crate::state::Lineage::unproven("spawn transaction: origin names no session parent")
        }
    };
    let _ = runtime.emitter.emit(
        "agent_spawned",
        &crate::spawn_edge::birth_event(
            &name,
            &lineage,
            serde_json::json!({
                "spawn_id": spawn_id,
                "pid": facts.pid,
                "harness_session_id": facts.child_session_id,
                "cwd": validated.request().work.cwd,
                "substrate": validated.request().how.substrate,
            }),
        ),
    );

    Ok(receipt(
        spawn_id,
        SpawnReceiptState::Registered,
        &name,
        provenance,
        facts.child_session_id,
        None,
    ))
}

/// Fold backend-observed launch facts into the row (never provenance).
fn apply_facts(entry: &mut RegistryEntry, facts: &LaunchFacts) {
    entry.pid = facts.pid;
    entry.pid_start_time = facts.pid_start_time;
    entry.harness_session_id = facts.child_session_id.clone();
    entry.log_path = facts.log_path.clone();
}

fn receipt(
    spawn_id: String,
    state: SpawnReceiptState,
    name: &str,
    provenance: SpawnProvenance,
    child_session_id: Option<String>,
    detail: Option<String>,
) -> SpawnReceipt {
    let identity_status = if child_session_id.is_some() {
        IdentityStatus::Resolved
    } else {
        IdentityStatus::Pending
    };
    SpawnReceipt {
        spawn_id,
        state,
        name: name.to_string(),
        provenance,
        child_session_id: child_session_id,
        identity_status,
        detail,
    }
}
