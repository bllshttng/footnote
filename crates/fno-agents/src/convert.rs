//! Pane-to-thread conversion: the pure classifier and its host read.
//!
//! A live pane session becomes a persistent thread under its OWN session id.
//! Which mechanism does that is declared per harness in the capability
//! contract (`[harness.<name>.conversion]`), never derived here and never
//! keyed on a harness name: `thread_lane` answers `attach` for opencode, so
//! a derivation would send it down claude's path and fork a session the
//! operator asked to keep.
//!
//! Everything in this module is a PURE function of three inputs - the
//! registry row, the contract row, and the host read - so a test stages a
//! keeper-hosted pane, a bare TUI pane and a dead pane without a mux server.
//! The strategies that ACT on a plan live in `convert::keeper_rebind`,
//! `convert::server_resume` and `convert::client_resume`.

pub mod claim_repin;
pub mod client_resume;
pub mod keeper_rebind;
pub mod server_resume;

use crate::harness_capabilities::ResolvedConversion;
use crate::state::RegistryEntry;

/// How the session is hosted right now, read from the live pane listing and
/// the keeper sockets. This is the fact the strategy acts on: a keeper
/// behind the pane means the child never has to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertHost {
    /// A pane whose PTY lives in a keeper process. The keeper survives the
    /// hand-off, so the child pid and session id are unchanged by it.
    KeeperPane {
        session: String,
        pane_id: u64,
        keeper_socket: String,
        keeper_pid: u32,
        child_pid: u32,
    },
    /// A pane the mux server hosts directly, with no keeper behind it. The
    /// child must STOP before any resume, because it owns its session
    /// in-process.
    BarePane {
        session: String,
        pane_id: u64,
        child_pid: u32,
    },
}

impl ConvertHost {
    pub fn child_pid(&self) -> u32 {
        match self {
            ConvertHost::KeeperPane { child_pid, .. } | ConvertHost::BarePane { child_pid, .. } => {
                *child_pid
            }
        }
    }

    pub fn pane(&self) -> (&str, u64) {
        match self {
            ConvertHost::KeeperPane {
                session, pane_id, ..
            }
            | ConvertHost::BarePane {
                session, pane_id, ..
            } => (session.as_str(), *pane_id),
        }
    }

    fn describe(&self) -> String {
        match self {
            ConvertHost::KeeperPane {
                session,
                pane_id,
                keeper_socket,
                keeper_pid,
                child_pid,
            } => format!(
                "keeper-hosted pane {session}:{pane_id} (keeper {keeper_pid} at {keeper_socket}, child {child_pid})"
            ),
            ConvertHost::BarePane {
                session,
                pane_id,
                child_pid,
            } => format!("server-hosted pane {session}:{pane_id} (child {child_pid})"),
        }
    }
}

/// What a conversion will do, printed verbatim by `--dry-run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertPlan {
    pub name: String,
    pub harness: String,
    pub strategy: String,
    pub preserves_id: bool,
    pub session_id: String,
    pub host: ConvertHost,
    /// The steps in order, each one a sentence an operator can check the
    /// run against. The receipt is the plan, so a dry run and a real run
    /// cannot describe two different operations.
    pub steps: Vec<String>,
}

impl ConvertPlan {
    /// The dry-run receipt. Named facts only: the harness, the strategy the
    /// contract declared, the host as read, whether the id survives, and
    /// every step.
    pub fn receipt(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("convert {} ({})\n", self.name, self.harness));
        out.push_str(&format!("  strategy:     {}\n", self.strategy));
        out.push_str(&format!("  host:         {}\n", self.host.describe()));
        out.push_str(&format!("  session id:   {}\n", self.session_id));
        out.push_str(&format!(
            "  preserves id: {}\n",
            if self.preserves_id { "yes" } else { "no" }
        ));
        for (n, step) in self.steps.iter().enumerate() {
            out.push_str(&format!("  step {}: {step}\n", n + 1));
        }
        out
    }
}

/// Why a conversion cannot run, with the remedy where one exists. Every
/// variant is raised BEFORE any mutation, so a refusal never leaves a
/// half-converted row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertRefusal {
    /// Already a thread with the same id. Exit 0: the caller asked for a
    /// state the world is already in.
    AlreadyAThread { name: String, session_id: String },
    /// Exit 2, with the reason and (where one exists) the remedy.
    Refused(String),
}

impl ConvertRefusal {
    pub fn exit_code(&self) -> i32 {
        match self {
            ConvertRefusal::AlreadyAThread { .. } => 0,
            ConvertRefusal::Refused(_) => 2,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ConvertRefusal::AlreadyAThread { name, session_id } => {
                format!("{name} is already a thread under session {session_id}; nothing to do")
            }
            ConvertRefusal::Refused(reason) => reason.clone(),
        }
    }
}

/// One keeper socket as `fno mux pane keeper list --json` reports it. The
/// session id is NOT a field of that row: the keeper reads it off the child
/// argv, and so does this, through the keeper's own rule
/// ([`crate::pane_keeper::session_id_from_argv`]), so the two can never
/// disagree about what a given argv names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeeperSighting {
    pub socket: String,
    pub keeper_pid: u32,
    pub child_pid: u32,
    pub cwd: String,
    pub argv: Vec<String>,
    /// The listing's own reason this keeper is not usable (no listener, a
    /// dead child). Present means the keeper answers for nothing.
    pub stale: Option<String>,
}

impl KeeperSighting {
    pub fn session_id(&self) -> Option<String> {
        crate::pane_keeper::session_id_from_argv(&self.argv)
    }

    /// Parse the rows `fno mux pane keeper list --json` prints. An
    /// unparseable row is DROPPED rather than guessed at: a keeper this
    /// cannot read is a keeper the classifier must not claim to have found.
    pub fn from_json(rows: &serde_json::Value) -> Vec<Self> {
        let Some(rows) = rows.as_array() else {
            return Vec::new();
        };
        rows.iter()
            .filter_map(|row| {
                Some(KeeperSighting {
                    socket: row.get("socket")?.as_str()?.to_string(),
                    keeper_pid: u32::try_from(row.get("keeper_pid")?.as_u64()?).ok()?,
                    child_pid: u32::try_from(row.get("child_pid")?.as_u64()?).ok()?,
                    cwd: row
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    argv: row
                        .get("argv")
                        .and_then(serde_json::Value::as_array)
                        .map(|argv| {
                            argv.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    stale: row
                        .get("stale")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect()
    }
}

/// One live pane, as the pane listing reports it. Mirrors
/// `pane_stop::PaneSighting`, which is crate-private to that module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneRead {
    pub session: String,
    pub pane_id: u64,
    pub child_pid: Option<u32>,
}

/// The rule that decides which strategies need a keeper behind the pane.
/// One place, so a new keeper-lane harness needs no second edit.
fn requires_keeper(strategy: &str) -> bool {
    strategy == "keeper-rebind"
}

/// Classify a conversion request. Pure: every world fact arrives as an
/// argument, so the caller re-runs this under the agent lock against a
/// fresh read and gets the same answer or a fresh refusal.
pub fn classify(
    entry: &RegistryEntry,
    contract: &ResolvedConversion,
    panes: &[PaneRead],
    keepers: &[KeeperSighting],
) -> Result<ConvertPlan, ConvertRefusal> {
    let name = entry.name.clone();
    let harness = entry.harness_name().to_string();
    let session_id = entry
        .harness_session_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ConvertRefusal::Refused(format!(
                "{name} records no harness session id; there is no identity to carry onto the \
                 thread lane. Re-spawn the worker."
            ))
        })?;

    // Idempotence first: a row already on the thread lane under the same id
    // is the state the caller asked for. Answering this before the pane read
    // keeps a second request cheap and keeps a concurrent caller, which
    // arrives here after the first released the lock, from refusing on a
    // pane that is correctly gone.
    if entry.substrate.as_deref() == Some("thread") && entry.mux.is_none() {
        return Err(ConvertRefusal::AlreadyAThread { name, session_id });
    }

    if contract.strategy == "unsupported" {
        return Err(ConvertRefusal::Refused(format!(
            "{name} runs on {harness}, which declares no pane-to-thread conversion: {}",
            contract.refusal
        )));
    }

    if entry.substrate.as_deref() != Some("pane") {
        return Err(ConvertRefusal::Refused(format!(
            "{name} is not a pane row (substrate {}); conversion moves a live pane onto the \
             thread lane and has nothing to move here",
            entry.substrate.as_deref().unwrap_or("unrecorded")
        )));
    }

    // The pane is found by CHILD PID, never by the row's stored pane id: the
    // server re-mints pane ids when it re-adopts a keeper, and a stored id
    // that no longer resolves would read as "no pane" on a live session.
    let child_pid = entry.pid.ok_or_else(|| {
        ConvertRefusal::Refused(format!(
            "{name} carries no verified pid, so no live pane can be matched to it; \
             run `fno agents reconcile` and retry"
        ))
    })?;
    let pane = panes
        .iter()
        .find(|pane| pane.child_pid == Some(child_pid))
        .ok_or_else(|| {
            ConvertRefusal::Refused(format!(
                "no live pane hosts {name}'s child pid {child_pid}; there is no running session \
                 to convert. Resume it first: fno agents resume {name}"
            ))
        })?;

    let keeper = keepers
        .iter()
        .find(|keeper| keeper.child_pid == child_pid && keeper.stale.is_none());

    if requires_keeper(&contract.strategy) {
        let keeper = keeper.ok_or_else(|| {
            ConvertRefusal::Refused(format!(
                "{name} runs the {} strategy, which moves the keeper that holds the pane, but no \
                 live keeper holds child pid {child_pid}. Relaunch it keeper-hosted first: \
                 fno agents stop {name}, then fno agents resume {name}, then convert.",
                contract.strategy
            ))
        })?;
        // The keeper's own word on WHICH session it holds, and where. A
        // keeper that answers a different id than the row is not this row's
        // keeper, and rebinding it would hand the row someone else's session.
        let keeper_session = keeper.session_id().ok_or_else(|| {
            ConvertRefusal::Refused(format!(
                "the keeper holding {name} answers no session id (its child argv names none), so \
                 the rebound row could not be proven to carry session {session_id}"
            ))
        })?;
        if keeper_session != session_id {
            return Err(ConvertRefusal::Refused(format!(
                "the keeper holding {name}'s child pid {child_pid} answers session \
                 {keeper_session}, but the row records {session_id}; refusing rather than \
                 rebinding the row onto another session"
            )));
        }
        if !keeper.cwd.is_empty() && keeper.cwd != entry.cwd {
            return Err(ConvertRefusal::Refused(format!(
                "the keeper holding {name} answers cwd {}, but the row records {}; refusing \
                 rather than rebinding a row onto a session in another directory",
                keeper.cwd, entry.cwd
            )));
        }
        let host = ConvertHost::KeeperPane {
            session: pane.session.clone(),
            pane_id: pane.pane_id,
            keeper_socket: keeper.socket.clone(),
            keeper_pid: keeper.keeper_pid,
            child_pid,
        };
        let steps = vec![
            format!(
                "hand the keeper socket off to the thread dir, leaving child {child_pid} running"
            ),
            "probe the moved socket and assert the same session id and child pid".to_string(),
            format!("flip {name} to substrate=thread with the keeper as its host"),
        ];
        return Ok(ConvertPlan {
            name,
            harness,
            strategy: contract.strategy.clone(),
            preserves_id: contract.preserves_id,
            session_id,
            host,
            steps,
        });
    }

    // The handoff strategies. A keeper may or may not sit behind the pane;
    // either way the CHILD owns its session in-process and must be gone
    // before a resume, so the host read records what is there and the stop
    // is confirmed by ESRCH rather than assumed.
    let host = match keeper {
        Some(keeper) => ConvertHost::KeeperPane {
            session: pane.session.clone(),
            pane_id: pane.pane_id,
            keeper_socket: keeper.socket.clone(),
            keeper_pid: keeper.keeper_pid,
            child_pid,
        },
        None => ConvertHost::BarePane {
            session: pane.session.clone(),
            pane_id: pane.pane_id,
            child_pid,
        },
    };
    let resume_step = match contract.strategy.as_str() {
        "server-resume" => format!(
            "resume session {session_id} through the shared app-server and host it on the \
             thread lane"
        ),
        _ => format!(
            "relaunch session {session_id} detached and read the resumed id back before \
             accepting it"
        ),
    };
    let steps = vec![
        "re-pin every claim this writer holds to the daemon, so none goes stale mid-move"
            .to_string(),
        format!("stop pane child {child_pid} and confirm it is gone"),
        resume_step,
        "re-pin the claims to the new writer".to_string(),
    ];
    Ok(ConvertPlan {
        name,
        harness,
        strategy: contract.strategy.clone(),
        preserves_id: contract.preserves_id,
        session_id,
        host,
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MuxRef;
    use crate::AgentStatus;

    fn contract(strategy: &str, preserves_id: bool, refusal: &str) -> ResolvedConversion {
        ResolvedConversion {
            strategy: strategy.to_string(),
            preserves_id,
            refusal: refusal.to_string(),
        }
    }

    fn pane_row(harness: &str, session_id: &str, pid: u32) -> RegistryEntry {
        let mut entry = RegistryEntry {
            name: "king-delivery".to_string(),
            cwd: "/repo".to_string(),
            status: AgentStatus::Live,
            created_at: "2026-09-20T00:00:00Z".to_string(),
            ..Default::default()
        };
        entry.harness = Some(harness.to_string());
        entry.harness_session_id = Some(session_id.to_string());
        entry.substrate = Some("pane".to_string());
        entry.pid = Some(pid);
        entry.mux = Some(MuxRef {
            session: "fno".to_string(),
            pane_id: 2313,
        });
        entry
    }

    fn pane(pid: u32) -> PaneRead {
        PaneRead {
            session: "fno".to_string(),
            pane_id: 2313,
            child_pid: Some(pid),
        }
    }

    fn keeper(pid: u32, session_id: &str, cwd: &str) -> KeeperSighting {
        KeeperSighting {
            socket: "/state/mux/panes/fno-2313.sock".to_string(),
            keeper_pid: 900,
            child_pid: pid,
            cwd: cwd.to_string(),
            argv: vec![
                "pi".to_string(),
                "--session-id".to_string(),
                session_id.to_string(),
            ],
            stale: None,
        }
    }

    fn refusal_text(result: Result<ConvertPlan, ConvertRefusal>) -> String {
        match result {
            Ok(plan) => panic!("expected a refusal, got a plan: {plan:?}"),
            Err(refusal) => refusal.message(),
        }
    }

    #[test]
    fn a_keeper_hosted_pane_plans_a_rebind_that_keeps_the_child() {
        let entry = pane_row("pi", "11111111-2222-3333-4444-555555555555", 4242);
        let plan = classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(4242)],
            &[keeper(
                4242,
                "11111111-2222-3333-4444-555555555555",
                "/repo",
            )],
        )
        .expect("a keeper-hosted pane converts");
        assert_eq!(plan.strategy, "keeper-rebind");
        assert!(plan.preserves_id);
        assert_eq!(plan.host.child_pid(), 4242);
        assert!(matches!(plan.host, ConvertHost::KeeperPane { .. }));
        // The rebind never stops anything: no step may claim a stop.
        assert!(
            !plan.steps.iter().any(|step| step.contains("stop")),
            "rebind steps must not stop the child: {:?}",
            plan.steps
        );
        assert!(plan.receipt().contains("keeper-rebind"));
    }

    #[test]
    fn a_handoff_strategy_stops_the_child_before_it_resumes() {
        for (harness, strategy) in [("codex", "server-resume"), ("claude", "client-resume")] {
            let entry = pane_row(harness, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", 77);
            let plan = classify(&entry, &contract(strategy, true, ""), &[pane(77)], &[])
                .expect("a bare pane converts on a handoff strategy");
            assert_eq!(plan.strategy, strategy);
            assert!(matches!(plan.host, ConvertHost::BarePane { .. }));
            let stop = plan
                .steps
                .iter()
                .position(|step| step.contains("stop pane child"))
                .expect("a handoff stops the child");
            let resume = plan
                .steps
                .iter()
                .position(|step| step.contains("resume") || step.contains("relaunch"))
                .expect("a handoff resumes the session");
            assert!(
                stop < resume,
                "the stop must precede the resume: {:?}",
                plan.steps
            );
        }
    }

    #[test]
    fn an_unsupported_harness_refuses_with_its_declared_reason() {
        let entry = pane_row("opencode", "id-1", 5);
        let text = refusal_text(classify(
            &entry,
            &contract(
                "unsupported",
                false,
                "its thread-lane honesty is an open question",
            ),
            &[pane(5)],
            &[],
        ));
        assert!(text.contains("opencode"), "{text}");
        assert!(text.contains("open question"), "{text}");
    }

    #[test]
    fn a_keeper_lane_pane_with_no_keeper_refuses_and_names_the_remedy() {
        let entry = pane_row("pi", "id-1", 4242);
        let text = refusal_text(classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(4242)],
            &[],
        ));
        assert!(text.contains("no live keeper"), "{text}");
        assert!(text.contains("fno agents resume"), "{text}");
    }

    #[test]
    fn a_stale_keeper_is_not_a_keeper() {
        let entry = pane_row("pi", "id-1", 4242);
        let mut dead = keeper(4242, "id-1", "/repo");
        dead.stale = Some("child pid 4242 is gone".to_string());
        let text = refusal_text(classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(4242)],
            &[dead],
        ));
        assert!(text.contains("no live keeper"), "{text}");
    }

    #[test]
    fn a_keeper_answering_another_session_or_cwd_refuses() {
        let entry = pane_row("pi", "row-session", 4242);
        let text = refusal_text(classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(4242)],
            &[keeper(4242, "other-session", "/repo")],
        ));
        assert!(text.contains("other-session"), "{text}");
        assert!(text.contains("row-session"), "{text}");

        let text = refusal_text(classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(4242)],
            &[keeper(4242, "row-session", "/elsewhere")],
        ));
        assert!(text.contains("/elsewhere"), "{text}");
    }

    #[test]
    fn a_keeper_whose_argv_names_no_session_refuses() {
        // The agy pane lane today: a keeper that mints no conversation id
        // cannot be proven to hold the row's session, so it is not rebound.
        let entry = pane_row("agy", "row-session", 4242);
        let mut anonymous = keeper(4242, "row-session", "/repo");
        anonymous.argv = vec!["agy".to_string()];
        let text = refusal_text(classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(4242)],
            &[anonymous],
        ));
        assert!(text.contains("no session id"), "{text}");
    }

    #[test]
    fn a_row_with_no_live_pane_refuses_before_anything_else() {
        let entry = pane_row("pi", "id-1", 4242);
        let text = refusal_text(classify(
            &entry,
            &contract("keeper-rebind", true, ""),
            &[pane(999)],
            &[keeper(4242, "id-1", "/repo")],
        ));
        assert!(text.contains("no live pane"), "{text}");
    }

    #[test]
    fn a_thread_row_under_the_same_id_is_idempotent_and_exits_zero() {
        let mut entry = pane_row("codex", "same-id", 42);
        entry.substrate = Some("thread".to_string());
        entry.mux = None;
        let refusal = classify(&entry, &contract("server-resume", true, ""), &[], &[])
            .expect_err("a thread row answers already-a-thread");
        assert_eq!(refusal.exit_code(), 0);
        assert!(refusal.message().contains("already a thread"));
    }

    #[test]
    fn a_non_pane_non_thread_row_refuses() {
        let mut entry = pane_row("codex", "id-1", 42);
        entry.substrate = Some("headless".to_string());
        let text = refusal_text(classify(
            &entry,
            &contract("server-resume", true, ""),
            &[pane(42)],
            &[],
        ));
        assert!(text.contains("not a pane row"), "{text}");
    }

    #[test]
    fn a_row_with_no_session_id_refuses() {
        let mut entry = pane_row("codex", "id-1", 42);
        entry.harness_session_id = None;
        let text = refusal_text(classify(
            &entry,
            &contract("server-resume", true, ""),
            &[pane(42)],
            &[],
        ));
        assert!(text.contains("no harness session id"), "{text}");
    }

    #[test]
    fn keeper_rows_parse_from_the_listing_json_and_drop_what_they_cannot_read() {
        let rows = serde_json::json!([
            {
                "socket": "/s/mux/panes/fno-1.sock",
                "keeper_pid": 10,
                "child_pid": 11,
                "cwd": "/repo",
                "argv": ["pi", "--session-id", "abc"],
            },
            {
                "socket": "/s/mux/panes/fno-2.sock",
                "keeper_pid": 20,
                "child_pid": 21,
                "stale": "no listener: ENOENT",
            },
            {"socket": "/s/mux/panes/fno-3.sock"},
        ]);
        let parsed = KeeperSighting::from_json(&rows);
        assert_eq!(
            parsed.len(),
            2,
            "the row with no pids is dropped: {parsed:?}"
        );
        assert_eq!(parsed[0].session_id().as_deref(), Some("abc"));
        assert!(parsed[0].stale.is_none());
        assert_eq!(parsed[1].stale.as_deref(), Some("no listener: ENOENT"));
        // A keeper with no argv answers no session id rather than guessing.
        assert_eq!(parsed[1].session_id(), None);
    }
}
