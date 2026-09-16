//! The spawn contract: one typed request every worker birth must
//! carry, with required origin and a separate owner.
//!
//! `SpawnRequest` is the door's input: `origin` (who caused this), `owner`
//! (who answers for it), `how` (harness/substrate/route/posture) and `work`
//! (name/seed/cwd/node). Nothing derives `Default`, and every field is
//! required, so a caller cannot construct a request that "says nothing": a
//! missing origin or owner is a compile-time shape and a runtime refusal,
//! never a blank row. Validation turns a request into a `ValidatedSpawn`
//! before any worker effect; backend code accepts only the validated form.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;

/// Cause codes a spawn row may carry. Reuses `naming::dispatch_sources`
/// (the naming-codes.yaml `sources` table) minus `sob`: retires
/// spawn-on-blueprint, and a retired producer must not re-enter through the
/// door that replaced it.
fn allowed_causes() -> &'static HashSet<String> {
    static ALLOWED: OnceLock<HashSet<String>> = OnceLock::new();
    ALLOWED.get_or_init(|| {
        crate::naming::dispatch_sources()
            .iter()
            .filter(|c| c.as_str() != "sob")
            .cloned()
            .collect()
    })
}

/// A proven session caller: full session id, harness, and the cwd the caller
/// ran in. Built only from a PROVED owned identity (`spawn_context`), never
/// parsed from a worker-name prefix or an ambient marker under test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRef {
    pub harness: String,
    pub session_id: String,
    pub cwd: String,
}

/// How a session-origin request was invoked, when the invocation itself has a
/// name (a script the session ran). Provenance only: the parent stays the
/// live session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationRef {
    pub kind: String,
    pub reference: String,
}

/// A typed, nonblank non-session actor. No variant may be constructed from
/// "no markers found": every field names the actual source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NonSessionSource {
    /// The daemon executable running one of its arms (`fno-agents-daemon`
    /// + `active-backlog`, `merge-close`, ...). `cause` is the declared
    /// naming code (`ab`, `ac`, ...).
    Daemon {
        exe: String,
        arm: String,
        cause: String,
    },
    /// A LaunchAgent label with its declared cause.
    LaunchAgent { label: String, cause: String },
    /// An interactive shell with a TTY: operator-typed, with the process
    /// identity that proves the shell existed.
    Shell {
        exe: String,
        tty: String,
        pid: u32,
        start_token: Option<u64>,
    },
    /// A standalone test script with its run identity.
    TestScript { path: String, run_id: String },
}

/// WHO caused the birth. A session reference carries the proven caller;
/// `NonSession` names the actual non-session source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnOrigin {
    Session {
        parent: SessionRef,
        invocation: Option<InvocationRef>,
    },
    NonSession {
        source: NonSessionSource,
    },
}

/// WHO answers for the worker. Separate from origin by design: daemon work
/// names its mission or crown without inventing a session parent, and a
/// kingless scope is still a valid owner (the durable reference is required,
/// never a live king).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnOwner {
    Session(SessionRef),
    Mission { project: String, mission: String },
    Crown { project: String, scope: String },
    Operator { tty: String },
    TestRun { script: String, run_id: String },
}

/// Requested harness coordinates and posture. Every request-sensitive value
/// the backend needs rides here; account secrets never do (references only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SpawnHow {
    pub harness: String,
    pub substrate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Protected account REFERENCE (record id), never credential material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
}

impl SpawnHow {
    pub fn new(harness: impl Into<String>, substrate: impl Into<String>) -> Self {
        Self {
            harness: harness.into(),
            substrate: substrate.into(),
            provider: None,
            model: None,
            effort: None,
            account_ref: None,
            permission_mode: None,
            writable_roots: Vec::new(),
        }
    }
}

/// The requested work: label, seed, working directory, and the assigned
/// backlog node when one exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SpawnWork {
    pub name: String,
    pub seed: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

impl SpawnWork {
    pub fn new(name: impl Into<String>, seed: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            seed: seed.into(),
            cwd: cwd.into(),
            node: None,
        }
    }
}

/// The door's input. `origin` and `owner` are required by construction (no
/// `Default`, no nullable escape); `how` and `work` carry the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SpawnRequest {
    pub origin: SpawnOrigin,
    pub owner: SpawnOwner,
    pub how: SpawnHow,
    pub work: SpawnWork,
}

impl SpawnRequest {
    pub fn new(origin: SpawnOrigin, owner: SpawnOwner, how: SpawnHow, work: SpawnWork) -> Self {
        Self {
            origin,
            owner,
            how,
            work,
        }
    }
}

/// The storage projection persisted on the row (schema v33): structured
/// origin plus owner, serialized through both languages. `spawned_by_*` on a
/// new birth are COMPATIBILITY projections derived from this value, never
/// independently writable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnProvenance {
    pub origin: SpawnOrigin,
    pub owner: SpawnOwner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// A required field is blank or the shape is wrong.
    Malformed(String),
    /// Origin and owner disagree with the rules (daemon without mission/crown,
    /// a cause outside the vocabulary, a known-shape id attributed to the
    /// wrong harness).
    Contradiction(String),
    /// An autonomous source with no resolvable owner.
    MissingOwner(String),
}

impl SpawnError {
    pub fn message(&self) -> &str {
        match self {
            SpawnError::Malformed(m)
            | SpawnError::Contradiction(m)
            | SpawnError::MissingOwner(m) => m,
        }
    }
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::Malformed(m) => write!(f, "spawn request malformed: {m}"),
            SpawnError::Contradiction(m) => write!(f, "spawn request contradicts itself: {m}"),
            SpawnError::MissingOwner(m) => write!(f, "spawn request has no owner: {m}"),
        }
    }
}

impl std::error::Error for SpawnError {}

fn require_nonblank(field: &str, value: &str) -> Result<(), SpawnError> {
    if value.trim().is_empty() {
        return Err(SpawnError::Malformed(format!("{field} must be nonblank")));
    }
    Ok(())
}

/// Full-id shapes that name exactly one harness (mirror of Python
/// `harness_of_session_id`): claude is a UUIDv4, codex a UUIDv7, opencode
/// `ses_`-prefixed. Legacy and fno-minted ids name no harness and are never
/// refused on shape.
fn shape_harness_of(session_id: &str) -> Option<&'static str> {
    let sid = session_id.trim();
    if let Some(rest) = sid.strip_prefix("ses_") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Some("opencode");
        }
        return None;
    }
    let parts: Vec<&str> = sid.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let lengths = [8, 4, 4, 4, 12];
    for (part, want) in parts.iter().zip(lengths) {
        if part.len() != want || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    match parts[2].chars().next()?.to_ascii_lowercase() {
        '4' => Some("claude"),
        '7' => Some("codex"),
        _ => None,
    }
}

fn validate_session_ref(ref_name: &str, r: &SessionRef) -> Result<(), SpawnError> {
    require_nonblank(&format!("{ref_name}.harness"), &r.harness)?;
    require_nonblank(&format!("{ref_name}.session_id"), &r.session_id)?;
    require_nonblank(&format!("{ref_name}.cwd"), &r.cwd)?;
    if let Some(shape) = shape_harness_of(&r.session_id) {
        // A definitively-shaped id attributed to a DIFFERENT known harness is
        // the forgery shape: the id proves the caller lied about the family.
        let known = matches!(r.harness.as_str(), "claude" | "codex" | "opencode");
        if known && shape != r.harness {
            return Err(SpawnError::Contradiction(format!(
                "{}.session_id shape names '{shape}' but harness says '{}'",
                ref_name, r.harness
            )));
        }
    }
    Ok(())
}

fn validate_source(source: &NonSessionSource) -> Result<Option<&str>, SpawnError> {
    match source {
        NonSessionSource::Daemon { exe, arm, cause } => {
            require_nonblank("daemon.exe", exe)?;
            require_nonblank("daemon.arm", arm)?;
            require_nonblank("daemon.cause", cause)?;
            Ok(Some(cause.as_str()))
        }
        NonSessionSource::LaunchAgent { label, cause } => {
            require_nonblank("launch_agent.label", label)?;
            require_nonblank("launch_agent.cause", cause)?;
            Ok(Some(cause.as_str()))
        }
        NonSessionSource::Shell {
            exe,
            tty,
            pid,
            start_token,
        } => {
            require_nonblank("shell.exe", exe)?;
            require_nonblank("shell.tty", tty)?;
            if *pid == 0 {
                return Err(SpawnError::Malformed("shell.pid must be nonzero".into()));
            }
            // A start token, when carried, must match the pid's liveness rule
            // of being nonzero (0 is the "unreadable" sentinel, not a value).
            if *start_token == Some(0) {
                return Err(SpawnError::Malformed(
                    "shell.start_token 0 means unreadable; omit it instead".into(),
                ));
            }
            Ok(None)
        }
        NonSessionSource::TestScript { path, run_id } => {
            require_nonblank("test_script.path", path)?;
            require_nonblank("test_script.run_id", run_id)?;
            Ok(None)
        }
    }
}

fn validate_owner(owner: &SpawnOwner) -> Result<(), SpawnError> {
    match owner {
        SpawnOwner::Session(r) => validate_session_ref("owner.session", r),
        SpawnOwner::Mission { project, mission } => {
            require_nonblank("owner.mission.project", project)?;
            require_nonblank("owner.mission.mission", mission)
        }
        SpawnOwner::Crown { project, scope } => {
            require_nonblank("owner.crown.project", project)?;
            require_nonblank("owner.crown.scope", scope)
        }
        SpawnOwner::Operator { tty } => require_nonblank("owner.operator.tty", tty),
        SpawnOwner::TestRun { script, run_id } => {
            require_nonblank("owner.test_run.script", script)?;
            require_nonblank("owner.test_run.run_id", run_id)
        }
    }
}

/// The `spawned_by_*` compatibility triple a session origin projects, shared
/// by every lane that stamps a row from request-carried lineage. A
/// non-session origin leaves all three `None`: a daemon parent is null BY
/// CONSTRUCTION, and null is correct there.
pub fn compatibility_parent(
    origin: &SpawnOrigin,
) -> (Option<String>, Option<String>, Option<String>) {
    match origin {
        SpawnOrigin::Session { parent, .. } => (
            Some(parent.session_id.clone()),
            Some(parent.harness.clone()),
            Some(parent.cwd.clone()),
        ),
        SpawnOrigin::NonSession { .. } => (None, None, None),
    }
}

/// Parse the `origin`/`owner` pair off an `agent.spawn` request into a
/// validated `SpawnProvenance`. `Ok(None)` when the caller sent neither (the
/// legacy ambient path, enforced away by the door rollout); a malformed or
/// half-sent pair names the defect and refuses.
pub fn parse_request_provenance(
    params: &serde_json::Value,
) -> Result<Option<SpawnProvenance>, String> {
    let origin = params.get("origin");
    let owner = params.get("owner");
    match (origin, owner) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            Err("origin and owner must arrive together on the spawn request".to_string())
        }
        (Some(o), Some(w)) => {
            let provenance: SpawnProvenance = serde_json::from_value(serde_json::json!({
                "origin": o,
                "owner": w,
            }))
            .map_err(|e| format!("malformed spawn provenance: {e}"))?;
            // Validate through the door's rules before any effect (AC2).
            validate(&SpawnRequest {
                origin: provenance.origin.clone(),
                owner: provenance.owner.clone(),
                how: SpawnHow::new("codex", "thread"),
                work: SpawnWork::new("validation", "", "."),
            })
            .map_err(|e| e.to_string())?;
            Ok(Some(provenance))
        }
    }
}

/// The one validation gate: request -> `ValidatedSpawn` or a named refusal.
/// Runs before ANY worker effect (no backend is reachable from here), so a
/// defective request never creates a worker, harness session, live row, or
/// held admission reservation (AC2).
pub fn validate(request: &SpawnRequest) -> Result<ValidatedSpawn, SpawnError> {
    let origin_cause = match &request.origin {
        SpawnOrigin::Session { parent, invocation } => {
            validate_session_ref("origin.session.parent", parent)?;
            if let Some(inv) = invocation {
                require_nonblank("origin.session.invocation.kind", &inv.kind)?;
                require_nonblank("origin.session.invocation.reference", &inv.reference)?;
            }
            None
        }
        SpawnOrigin::NonSession { source } => validate_source(source)?,
    };
    validate_owner(&request.owner)?;

    // An autonomous daemon/LaunchAgent dispatch REQUIRES a mission or crown
    // owner (AC2/AC3): the source names the arm, the owner names the durable
    // responsibility. Shell/test origins carry their own operator/test owner.
    if let SpawnOrigin::NonSession { source } = &request.origin {
        let autonomous = matches!(
            source,
            NonSessionSource::Daemon { .. } | NonSessionSource::LaunchAgent { .. }
        );
        if autonomous
            && !matches!(
                request.owner,
                SpawnOwner::Mission { .. } | SpawnOwner::Crown { .. }
            )
        {
            return Err(SpawnError::MissingOwner(
                "daemon/launch-agent origin requires a mission or crown owner; the daemon \
                 starter is never substituted"
                    .into(),
            ));
        }
        // A cause code, when the source carries one, must speak the shared
        // vocabulary; `sob` is retired and refuses here.
        if let Some(cause) = origin_cause {
            if !allowed_causes().contains(cause) {
                return Err(SpawnError::Contradiction(format!(
                    "cause '{cause}' is not in the naming-codes vocabulary (sob is retired)"
                )));
            }
        }
    }

    require_nonblank("how.harness", &request.how.harness)?;
    require_nonblank("how.substrate", &request.how.substrate)?;
    require_nonblank("work.name", &request.work.name)?;
    require_nonblank("work.cwd", &request.work.cwd)?;

    Ok(ValidatedSpawn {
        request: request.clone(),
    })
}

/// A request that passed validation. Backend code accepts only this form; it
/// carries no authority of its own - origin is descriptive provenance, and
/// existing crown/git/merge permission checks still authorize effects.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedSpawn {
    request: SpawnRequest,
}

impl ValidatedSpawn {
    pub fn request(&self) -> &SpawnRequest {
        &self.request
    }

    /// The durable storage projection for the row (schema v33).
    pub fn provenance(&self) -> SpawnProvenance {
        SpawnProvenance {
            origin: self.request.origin.clone(),
            owner: self.request.owner.clone(),
        }
    }

    /// The `spawned_by_*` compatibility triple a new birth derives from the
    /// origin: a session parent fills the parent edge; a non-session source
    /// leaves them None (a daemon parent is null BY CONSTRUCTION, and null
    /// is correct there). Never independently writable on a new birth.
    pub fn compatibility_parent(&self) -> (Option<String>, Option<String>, Option<String>) {
        match &self.request.origin {
            SpawnOrigin::Session { parent, .. } => (
                Some(parent.session_id.clone()),
                Some(parent.harness.clone()),
                Some(parent.cwd.clone()),
            ),
            SpawnOrigin::NonSession { .. } => (None, None, None),
        }
    }

    pub fn spawn_id(&self) -> String {
        // Overwritten by the transaction with the allocated id;
        // validation alone never mints one.
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_request() -> SpawnRequest {
        SpawnRequest::new(
            SpawnOrigin::Session {
                parent: SessionRef {
                    harness: "claude".into(),
                    session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                    cwd: "/repo".into(),
                },
                invocation: None,
            },
            SpawnOwner::Session(SessionRef {
                harness: "claude".into(),
                session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                cwd: "/repo".into(),
            }),
            SpawnHow::new("claude", "bg"),
            SpawnWork::new("w", "seed", "/repo"),
        )
    }

    fn daemon_request(owner: SpawnOwner) -> SpawnRequest {
        SpawnRequest::new(
            SpawnOrigin::NonSession {
                source: NonSessionSource::Daemon {
                    exe: "fno-agents-daemon".into(),
                    arm: "active-backlog".into(),
                    cause: "ab".into(),
                },
            },
            owner,
            SpawnHow::new("claude", "headless"),
            SpawnWork::new("ab-w", "seed", "/repo"),
        )
    }

    #[test]
    fn valid_session_request_round_trips() {
        let v = validate(&session_request()).expect("valid");
        let (sid, harness, cwd) = v.compatibility_parent();
        assert_eq!(sid.as_deref(), Some("0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"));
        assert_eq!(harness.as_deref(), Some("claude"));
        assert_eq!(cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn daemon_with_mission_owner_is_valid() {
        validate(&daemon_request(SpawnOwner::Mission {
            project: "fno".into(),
            mission: "epic-37af".into(),
        }))
        .expect("daemon + mission is valid");
    }

    #[test]
    fn kingless_crown_scope_remains_valid_ownership() {
        validate(&daemon_request(SpawnOwner::Crown {
            project: "fno".into(),
            scope: "epic-x".into(),
        }))
        .expect("a durable scope reference is ownership; no live king required");
    }

    #[test]
    fn daemon_origin_without_mission_or_crown_refuses() {
        for owner in [
            SpawnOwner::Operator {
                tty: "/dev/ttys001".into(),
            },
            SpawnOwner::TestRun {
                script: "t.sh".into(),
                run_id: "r1".into(),
            },
        ] {
            let err = validate(&daemon_request(owner)).expect_err("must refuse");
            assert!(matches!(err, SpawnError::MissingOwner(_)), "{err}");
            assert!(err.message().contains("mission or crown owner"));
        }
    }

    #[test]
    fn blank_session_id_is_malformed_not_missing() {
        let mut req = session_request();
        if let SpawnOrigin::Session { parent, .. } = &mut req.origin {
            parent.session_id = "  ".into();
        }
        let err = validate(&req).expect_err("must refuse");
        assert!(matches!(err, SpawnError::Malformed(_)));
    }

    #[test]
    fn id_shape_attributed_to_wrong_harness_refuses() {
        let mut req = session_request();
        if let SpawnOrigin::Session { parent, .. } = &mut req.origin {
            // A codex UUIDv7 named as claude: the forgery shape.
            parent.session_id = "0198a7b8-86b8-7a9e-8d99-6bd94b0ea9c9".into();
        }
        let err = validate(&req).expect_err("must refuse");
        assert!(matches!(err, SpawnError::Contradiction(_)), "{err}");
    }

    #[test]
    fn retired_sob_cause_refuses() {
        let req = SpawnRequest::new(
            SpawnOrigin::NonSession {
                source: NonSessionSource::Daemon {
                    exe: "fno-agents-daemon".into(),
                    arm: "blueprint".into(),
                    cause: "sob".into(),
                },
            },
            SpawnOwner::Crown {
                project: "fno".into(),
                scope: "epic".into(),
            },
            SpawnHow::new("claude", "headless"),
            SpawnWork::new("sob-w", "seed", "/repo"),
        );
        let err = validate(&req).expect_err("sob is retired");
        assert!(err.message().contains("sob is retired"), "{err}");
    }

    #[test]
    fn shell_origin_takes_operator_owner_without_a_crown() {
        let req = SpawnRequest::new(
            SpawnOrigin::NonSession {
                source: NonSessionSource::Shell {
                    exe: "/bin/zsh".into(),
                    tty: "/dev/ttys003".into(),
                    pid: 4242,
                    start_token: Some(777),
                },
            },
            SpawnOwner::Operator {
                tty: "/dev/ttys003".into(),
            },
            SpawnHow::new("claude", "pane"),
            SpawnWork::new("shell-w", "seed", "/repo"),
        );
        validate(&req).expect("shell + operator owner is valid");
    }

    #[test]
    fn session_launched_test_keeps_session_parent_with_invocation() {
        let mut req = session_request();
        req.origin = SpawnOrigin::Session {
            parent: SessionRef {
                harness: "claude".into(),
                session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                cwd: "/repo".into(),
            },
            invocation: Some(InvocationRef {
                kind: "test_script".into(),
                reference: "tests/mux-portal-restore-server-kill.sh".into(),
            }),
        };
        let v = validate(&req).expect("valid");
        let (sid, _, _) = v.compatibility_parent();
        assert!(sid.is_some(), "the session parent survives the script");
    }

    #[test]
    fn provenance_serializes_python_readably() {
        let v = validate(&daemon_request(SpawnOwner::Mission {
            project: "fno".into(),
            mission: "epic-37af".into(),
        }))
        .expect("valid");
        let json = serde_json::to_value(v.provenance()).expect("serializes");
        let origin = json.get("origin").expect("origin key");
        assert_eq!(
            origin.get("kind").and_then(|k| k.as_str()),
            Some("non_session")
        );
        let owner = json.get("owner").expect("owner key");
        assert_eq!(owner.get("kind").and_then(|k| k.as_str()), Some("mission"));
    }

    #[test]
    fn python_shaped_provenance_round_trips() {
        // The exact dict shape build_spawn_provenance() in
        // cli/src/fno/agents/spawn_lineage.py emits must deserialize into the
        // door's SpawnProvenance, so a Python-stamped row is door-shaped.
        let python_json = serde_json::json!({
            "origin": {
                "kind": "session",
                "parent": {
                    "harness": "claude",
                    "session_id": "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9",
                    "cwd": "/repo",
                },
                "invocation": Option::<serde_json::Value>::None,
            },
            "owner": {
                "kind": "session",
                "harness": "claude",
                "session_id": "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9",
                "cwd": "/repo",
            },
        });
        let provenance: SpawnProvenance =
            serde_json::from_value(python_json).expect("python shape round-trips");
        let request = SpawnRequest::new(
            provenance.origin,
            provenance.owner,
            SpawnHow::new("claude", "headless"),
            SpawnWork::new("w", "seed", "/repo"),
        );
        assert!(validate(&request).is_ok());
    }

    #[test]
    fn python_daemon_provenance_shape_round_trips() {
        // The daemon-producer shape (producers emit this).
        let python_json = serde_json::json!({
            "origin": {
                "kind": "non_session",
                "source": {
                    "kind": "daemon",
                    "exe": "fno-agents-daemon",
                    "arm": "active-backlog",
                    "cause": "ab",
                },
            },
            "owner": {
                "kind": "crown",
                "project": "fno",
                "scope": "epic-x",
            },
        });
        let provenance: SpawnProvenance =
            serde_json::from_value(python_json).expect("daemon shape round-trips");
        assert_eq!(
            provenance.origin,
            SpawnOrigin::NonSession {
                source: NonSessionSource::Daemon {
                    exe: "fno-agents-daemon".into(),
                    arm: "active-backlog".into(),
                    cause: "ab".into(),
                }
            }
        );
    }

    #[test]
    fn spawn_request_has_no_default_impl() {
        // Documentation test, not a compile-time pin: stable Rust cannot
        // assert the ABSENCE of a Default impl, so this records the intent -
        // SpawnRequest derives no Default and every field is required, so a
        // caller cannot construct a request that says nothing. Do not add a
        // Default impl; a derivation here is a regression.
        let request = session_request();
        assert!(validate(&request).is_ok());
    }
}
