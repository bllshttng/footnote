//! Journey tests for the spawn transaction.
//!
//! A spy backend records every launch; the runtime runs against a temp
//! agents home with a real EventEmitter and the locked registry write, so
//! the tests prove durable effects (journal lines, registry rows) rather
//! than internal state.

use fno_agents::events::EventEmitter;
use fno_agents::paths::AgentsHome;
use fno_agents::spawn_contract::{
    InvocationRef, NonSessionSource, SessionRef, SpawnHow, SpawnOrigin, SpawnOwner, SpawnRequest,
    SpawnWork,
};
use fno_agents::spawn_transaction::{
    spawn, IdentityStatus, LaunchFacts, NameReservation, SpawnBackend, SpawnClaims,
    SpawnLaunchError, SpawnReceiptState,
};

/// Admission claims: refuses the reserved name token; guards release into
/// nothing (the registry row is the real release record).
struct TestClaims;

/// A const item so `&CLAIMS` has a 'static lifetime for the runtime.
const CLAIMS: TestClaims = TestClaims;

impl SpawnClaims for TestClaims {
    fn reserve_name(&self, name: &str) -> Result<NameReservation, String> {
        if name == "occupied" {
            return Err("name 'occupied' is taken".into());
        }
        Ok(NameReservation::new(name, Box::new(|| {})))
    }
}

/// The spy: counts launches and can be told to fail at launch or to report a
/// launch that then lost persistence (recovery-required shape).
struct SpyBackend {
    launches: std::sync::atomic::AtomicUsize,
    mode: Mode,
}

enum Mode {
    Happy,
    FailAtLaunch,
    LoseAfterLaunch,
}

impl SpawnBackend for SpyBackend {
    fn launch(
        &self,
        _launch: &fno_agents::spawn_contract::ValidatedSpawn,
        spawn_id: &str,
    ) -> Result<LaunchFacts, SpawnLaunchError> {
        self.launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match &self.mode {
            Mode::Happy => Ok(LaunchFacts {
                pid: Some(4242),
                pid_start_time: Some(77),
                child_session_id: Some(format!("child-{spawn_id}")),
                log_path: None,
                transport: Some("test".into()),
            }),
            Mode::FailAtLaunch => Err(SpawnLaunchError::Failure("fake binary not found".into())),
            Mode::LoseAfterLaunch => Err(SpawnLaunchError::RecoveryRequired {
                facts: LaunchFacts {
                    pid: Some(9999),
                    pid_start_time: None,
                    child_session_id: None,
                    log_path: None,
                    transport: Some("test".into()),
                },
                detail: "registry write lost after launch".into(),
                pid: Some(9999),
            }),
        }
    }
}

/// The harness each test drives: temp home + journal + spy + claims.
struct Journey {
    root: tempfile::TempDir,
    emitter: EventEmitter,
    backend: std::sync::Arc<SpyBackend>,
}

impl Journey {
    fn new(mode: Mode) -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        let emitter = EventEmitter::new(&root.path().join("events.jsonl"), "daemon");
        let backend = std::sync::Arc::new(SpyBackend {
            launches: std::sync::atomic::AtomicUsize::new(0),
            mode,
        });
        Self {
            root,
            emitter,
            backend,
        }
    }

    fn home(&self) -> AgentsHome {
        AgentsHome::at(self.root.path().join("agents"))
    }

    fn runtime<'a>(
        &'a self,
        home: &'a AgentsHome,
    ) -> fno_agents::spawn_transaction::SpawnRuntime<'a> {
        fno_agents::spawn_transaction::SpawnRuntime {
            home,
            emitter: &self.emitter,
            backend: self.backend.as_ref(),
            claims: &CLAIMS,
        }
    }

    fn journal(&self) -> Vec<serde_json::Value> {
        let text =
            std::fs::read_to_string(self.root.path().join("events.jsonl")).unwrap_or_default();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn launches(&self) -> usize {
        self.backend
            .launches
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The request every test uses: a proven session caller with a session owner.
fn test_request(name: &str, cwd: &str) -> SpawnRequest {
    SpawnRequest::new(
        SpawnOrigin::Session {
            parent: SessionRef {
                harness: "claude".into(),
                session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                cwd: cwd.to_string(),
            },
            invocation: Some(InvocationRef {
                kind: "test_script".into(),
                reference: "crates/fno-agents/tests/spawn_contract_journey.rs".into(),
            }),
        },
        SpawnOwner::Session(SessionRef {
            harness: "claude".into(),
            session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
            cwd: cwd.to_string(),
        }),
        SpawnHow::new("claude", "headless"),
        SpawnWork::new(name, "seed turn", cwd),
    )
}

#[tokio::test]
async fn accepted_birth_and_receipt_share_one_spawn_id() {
    let journey = Journey::new(Mode::Happy);
    let home = journey.home();
    let rt = journey.runtime(&home);
    let req = test_request("worker-a", home.root().to_str().unwrap());
    let receipt = spawn(&req, &rt).await.expect("spawn succeeds");
    assert_eq!(receipt.state, SpawnReceiptState::Registered);
    assert_eq!(receipt.identity_status, IdentityStatus::Resolved);

    let reg = fno_agents::state::load_registry(&home.registry_json()).unwrap();
    let row = &reg.entries[0];
    assert_eq!(row.origin.as_deref(), Some("spawn"), "door birth marker");
    assert!(row.spawn_id.is_some());
    assert!(row.spawn_provenance.is_some());
    assert_eq!(
        row.provenance_status(),
        fno_agents::state::ProvenanceStatus::Ok
    );
    assert_eq!(
        row.spawned_by_session.as_deref(),
        Some("0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9")
    );

    let lines = journey.journal();
    let accepted_idx = lines
        .iter()
        .position(|l| l["type"] == "agent_spawn_accepted")
        .expect("accepted line");
    let birth_idx = lines
        .iter()
        .position(|l| l["type"] == "agent_spawned")
        .expect("birth line");
    assert!(accepted_idx < birth_idx);
    assert_eq!(
        lines[accepted_idx]["data"]["spawn_id"],
        lines[birth_idx]["data"]["spawn_id"]
    );
    assert_eq!(journey.launches(), 1);
}

#[tokio::test]
async fn refusal_performs_zero_worker_effects() {
    let journey = Journey::new(Mode::Happy);
    let home = journey.home();
    let rt = journey.runtime(&home);
    // Daemon origin with a session owner: refused before ANY effect.
    let mut req = test_request("worker-b", home.root().to_str().unwrap());
    req.origin = SpawnOrigin::NonSession {
        source: NonSessionSource::Daemon {
            exe: "fno-agents-daemon".into(),
            arm: "active-backlog".into(),
            cause: "ab".into(),
        },
    };
    let err = spawn(&req, &rt).await.expect_err("must refuse");
    assert!(err.message().contains("mission or crown owner"), "{err}");
    assert_eq!(journey.launches(), 0, "zero worker effects");
    assert!(
        journey
            .journal()
            .iter()
            .all(|l| l["type"] != "agent_spawn_accepted"),
        "nothing journaled"
    );
    assert!(fno_agents::state::load_registry(&home.registry_json())
        .map(|r| r.entries.is_empty())
        .unwrap_or(true));
}

#[tokio::test]
async fn failure_before_launch_releases_everything() {
    let journey = Journey::new(Mode::FailAtLaunch);
    let home = journey.home();
    let rt = journey.runtime(&home);
    let req = test_request("worker-c", home.root().to_str().unwrap());
    let err = spawn(&req, &rt).await.expect_err("launch failure");
    assert!(err.message().contains("nothing started"), "{err}");
    assert_eq!(journey.launches(), 1, "backend ran once");
    assert!(
        fno_agents::state::load_registry(&home.registry_json())
            .map(|r| r.entries.is_empty())
            .unwrap_or(true),
        "no birth row survives"
    );
    let lines = journey.journal();
    assert!(lines.iter().any(|l| l["type"] == "agent_spawn_failed"));
    assert!(lines.iter().all(|l| l["type"] != "agent_spawned"));
}

#[tokio::test]
async fn recovery_required_keeps_the_attempt_named() {
    let journey = Journey::new(Mode::LoseAfterLaunch);
    let home = journey.home();
    let rt = journey.runtime(&home);
    let req = test_request("worker-d", home.root().to_str().unwrap());
    let receipt = spawn(&req, &rt).await.expect("recovery receipt");
    assert_eq!(receipt.state, SpawnReceiptState::RecoveryRequired);
    assert_eq!(receipt.identity_status, IdentityStatus::Pending);
    assert!(receipt
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("9999")));
    assert!(
        fno_agents::state::load_registry(&home.registry_json())
            .map(|r| r.entries.is_empty())
            .unwrap_or(true),
        "no row: the attempt is retained, not birthed"
    );
    assert_eq!(journey.launches(), 1);
}

#[tokio::test]
async fn admission_refusal_beats_the_backend() {
    let journey = Journey::new(Mode::Happy);
    let home = journey.home();
    let rt = journey.runtime(&home);
    let req = test_request("occupied", home.root().to_str().unwrap());
    let err = spawn(&req, &rt).await.expect_err("name refused");
    assert!(err.message().contains("admission refused"), "{err}");
    assert_eq!(journey.launches(), 0, "backend never ran");
}

#[tokio::test]
async fn stateless_headless_receipt_has_not_applicable_identity() {
    let journey = Journey::new(Mode::Happy);
    let home = journey.home();
    let rt = journey.runtime(&home);
    let mut req = test_request("worker-e", home.root().to_str().unwrap());
    req.how.substrate = "headless".into();
    let receipt = spawn(&req, &rt).await.expect("spawn succeeds");
    // Identity status rides the LAUNCH OBSERVATION: the spy resolves an id,
    // so the receipt is Resolved even on a headless substrate. The
    // not_applicable arm is a backend decision, never a fabricated id.
    assert_eq!(receipt.identity_status, IdentityStatus::Resolved);
    assert_eq!(journey.launches(), 1);
}
