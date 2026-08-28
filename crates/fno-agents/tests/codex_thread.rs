use fno_agents::codex_fake_daemon::{Behavior, FakeDaemon, Interrupt, Steer};
use fno_agents::codex_thread::{
    parse_thread_start_response, thread_start_request_json, CodexThread, CodexThreadActor,
    InterruptOutcome, ThreadStartError,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn thread_start_reads_nested_thread_id_and_rejects_top_level_alias() {
    let request: serde_json::Value =
        serde_json::from_str(&thread_start_request_json("/tmp/worktree", "never"))
            .expect("valid request JSON");
    assert_eq!(request["method"], "thread/start");
    assert_eq!(request["params"]["cwd"], "/tmp/worktree");
    assert_eq!(request["params"]["approvalPolicy"], "never");

    assert_eq!(
        parse_thread_start_response(
            r#"{"id":1,"result":{"thread":{"id":"thread-1","path":"/tmp/rollout"}}}"#
        ),
        Ok(("thread-1".to_string(), "/tmp/rollout".to_string()))
    );
    assert_eq!(
        parse_thread_start_response(r#"{"id":1,"result":{"threadId":"thread-1"}}"#),
        Err(ThreadStartError::NotConfirmed)
    );
}

/// Serializes every `CODEX_HOME` mutation in this binary: two fake daemons
/// must never overlap, because the variable that points the driver at one is
/// process-global.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `body` against a fake SHARED app-server daemon.
///
/// The fakes these tests used to install were `codex app-server` children on
/// PATH speaking newline-delimited JSON on stdio. That is the transport this
/// lane stopped using, so they stopped covering the driver and started
/// reaching the operator's real daemon instead.
async fn with_fake_daemon(behavior: Behavior, body: impl std::future::Future<Output = ()>) {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _daemon = FakeDaemon::start(behavior);
    body.await;
}

/// A turn that streams MORE frames than the old frame ceiling and goes quiet
/// LONGER than the old per-frame timeout before completing. The journey
/// test's live seed answers in a handful of frames and well under 15s, so it
/// cleared both old bounds by being small; this is the scale the real lane
/// runs at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_survives_more_frames_and_a_longer_gap_than_the_old_bounds() {
    let behavior = Behavior::quick()
        .with_thread_id("thread-bounds")
        .with_event_frames(300)
        .with_turn_duration(Duration::from_secs(16));
    with_fake_daemon(behavior, async {
        let worktree = tempfile::tempdir().unwrap();
        let mut thread = CodexThread::start(worktree.path(), None, false, None)
            .await
            .expect("thread starts against the fake daemon");
        assert_eq!(thread.thread_id(), "thread-bounds");
        let turn = thread
            .drive_turn("run the wide turn")
            .await
            .expect("turn completes under the whole-turn budget");
        assert_eq!(turn.turn_id, "turn-1");
        assert_eq!(turn.text, "REPLY-1");
    })
    .await;
}

// ---------------------------------------------------------------------------
// Actor tests (single-owner rewrite, x-de10). Each names the scenario it needs
// through the shared fake's knobs rather than embedding its own app-server.
// ---------------------------------------------------------------------------

async fn start_actor() -> (CodexThreadActor, tempfile::TempDir) {
    let worktree = tempfile::tempdir().unwrap();
    let driver = CodexThread::start(worktree.path(), None, false, None)
        .await
        .expect("thread starts against the fake daemon");
    (driver.into_actor(Arc::new(|_| {})), worktree)
}

/// AC1: two back-to-back submits against an idle thread make ONE turn/start,
/// ONE turn/steer, ONE turn/completed, and BOTH waiters resolve from that
/// shared completion. On the old mutex handle the second ask queued behind the
/// whole first turn and drove a SECOND turn/start (zero steers) - the daemon
/// test `codex_thread_ask_while_driving_steers_instead_of_queueing` is the
/// probe that fails on the old code; this pins the actor's half.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_while_driving_steers_into_the_shared_turn() {
    with_fake_daemon(Behavior::quick(), async {
        let (actor, _keep) = start_actor().await;
        let reply_a = actor.submit("first question".into()).await.unwrap();
        // Give the actor time to receive the turn/start ack (turn 1 driving).
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(actor.current_turn_id().as_deref(), Some("turn-1"));
        let reply_b = actor.submit("follow-up".into()).await.unwrap();
        let a = tokio::time::timeout(std::time::Duration::from_secs(10), reply_a)
            .await
            .expect("first submit resolves")
            .unwrap()
            .expect("turn 1 completed");
        let b = tokio::time::timeout(std::time::Duration::from_secs(10), reply_b)
            .await
            .expect("steered submit resolves")
            .unwrap()
            .expect("shared turn completed");
        assert_eq!(a.turn_id, "turn-1");
        assert_eq!(b.turn_id, "turn-1", "both waiters ride the shared turn");
        assert_eq!(a.text, "REPLY-1");
        assert_eq!(b.text, "REPLY-1");
        assert_eq!(
            actor.current_turn_id(),
            None,
            "cleared on routed completion"
        );
        actor.shutdown().await.unwrap();
    })
    .await;
}

/// AC2: a steer that fails its expectedTurnId precondition (the turn
/// completed in the race window) drains the old completion so ITS waiters
/// resolve, then retries once as a fresh turn/start; the submitter still gets
/// a reply from the new turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_precondition_failure_drains_then_retries_fresh_start() {
    with_fake_daemon(
        Behavior::quick().with_steer(Steer::FailPreconditionOnce),
        async {
            let (actor, _keep) = start_actor().await;
            let reply_a = actor.submit("first".into()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // Turn 1 is driving server-side but completes 200ms in; submit B so the
            // steer lands while the actor still holds turn-1 as driving, forcing
            // the precondition error + drain path.
            let reply_b = actor.submit("second".into()).await.unwrap();
            let a = tokio::time::timeout(std::time::Duration::from_secs(15), reply_a)
                .await
                .expect("first submit resolves from the drained completion")
                .unwrap()
                .expect("turn 1 receipt");
            let b = tokio::time::timeout(std::time::Duration::from_secs(15), reply_b)
                .await
                .expect("retried submit resolves")
                .unwrap()
                .expect("fresh turn receipt");
            assert_eq!(a.turn_id, "turn-1");
            assert_eq!(a.text, "REPLY-1");
            assert_eq!(
                b.turn_id, "turn-2",
                "the retry is a fresh turn/start, not a second steer"
            );
            assert_eq!(b.text, "REPLY-2");
            actor.shutdown().await.unwrap();
        },
    )
    .await;
}

/// AC5 + AC17: interrupt mid-turn returns the terminal `interrupted` receipt,
/// and the driving turn id survives until that completion actually routes -
/// it is the interrupt handle after any caller-side timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_mid_turn_resolves_interrupted_and_survives_as_handle() {
    with_fake_daemon(Behavior::long(), async {
        let (actor, _keep) = start_actor().await;
        let reply = actor.submit("long turn".into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(actor.current_turn_id().as_deref(), Some("turn-1"));
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), actor.interrupt())
            .await
            .expect("interrupt settles")
            .unwrap();
        match outcome {
            InterruptOutcome::Interrupted(receipt) => {
                assert_eq!(receipt.turn_id, "turn-1");
                assert_eq!(receipt.status, "interrupted");
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
        let turn = tokio::time::timeout(std::time::Duration::from_secs(5), reply)
            .await
            .expect("waiter resolves after interrupt")
            .unwrap()
            .expect("interrupted receipt");
        assert_eq!(turn.status, "interrupted");
        assert_eq!(actor.current_turn_id(), None);
        actor.shutdown().await.unwrap();
    })
    .await;
}

/// The RPC ack wait and the settle wait split ONE interrupt budget. A child
/// that acks slowly must not leave a settle wait that outlives the daemon's
/// outer stop bound: stacked full bounds (60s ack + 65s settle) once stalled
/// `shutdown()` on the interrupt tail until past the client's 120s deadline.
/// The env bound and the fake's 1s ack delay make the stacked behavior fail
/// here in seconds instead of 66.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_settle_shares_one_deadline_with_the_ack_wait() {
    with_fake_daemon(
        Behavior::long().with_interrupt(Interrupt::AckOnly(Duration::from_secs(1))),
        async {
            std::env::set_var("FNO_CODEX_INTERRUPT_BOUND_MS", "1500");
            let (actor, _keep) = start_actor().await;
            let _reply = actor.submit("long turn".into()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert_eq!(actor.current_turn_id().as_deref(), Some("turn-1"));
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(5), actor.interrupt())
                    .await
                    .expect("interrupt answers inside the shared budget, not the stacked 66s")
                    .unwrap();
            assert!(
                matches!(outcome, InterruptOutcome::Timeout),
                "the settle wait consumed the remaining budget: {outcome:?}"
            );
            std::env::remove_var("FNO_CODEX_INTERRUPT_BOUND_MS");
            actor.shutdown().await.unwrap();
        },
    )
    .await;
}

/// A completion for a turn nobody drives (the review lane's own turn id, or a
/// stale completion racing the steer-precondition retry) must stay parked
/// telemetry: the driving turn's waiters resolve from the REAL completion and
/// its shared id clears only there. The old take()-then-filter dropped both
/// on the first foreign completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_completion_leaves_the_driving_turn_untouched() {
    with_fake_daemon(
        Behavior::quick().with_stray_completion_after(Duration::from_millis(600)),
        async {
            let (actor, _keep) = start_actor().await;
            let reply = actor.submit("drive me".into()).await.unwrap();
            let turn = tokio::time::timeout(std::time::Duration::from_secs(10), reply)
                .await
                .expect("the driving turn's waiter survives the foreign completion")
                .unwrap()
                .expect("turn 1 receipt");
            assert_eq!(turn.turn_id, "turn-1");
            assert_eq!(turn.text, "REPLY-1");
            assert_eq!(
                actor.current_turn_id(),
                None,
                "cleared by the real completion, not the stray"
            );
            actor.shutdown().await.unwrap();
        },
    )
    .await;
}

/// AC3 + AC17 (actor half): a submit whose bounded wait expires leaves the
/// turn RUNNING, keeps the turn id as the interrupt handle, and still
/// resolves the receipt when the turn completes - the daemon layers its 90s
/// `in_flight` receipt on exactly this behavior.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_submit_wait_leaves_turn_running_and_receipt_arrives_later() {
    let done: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&done);
    with_fake_daemon(
        Behavior::quick().with_turn_duration(Duration::from_millis(1500)),
        async move {
            let worktree = tempfile::tempdir().unwrap();
            let driver = CodexThread::start(worktree.path(), None, false, None)
                .await
                .expect("thread starts");
            let actor = driver.into_actor(Arc::new(move |receipt| {
                seen.lock().unwrap().push(receipt.turn_id.clone());
            }));
            let reply = actor.submit("slow question".into()).await.unwrap();
            let expired = tokio::time::timeout(std::time::Duration::from_millis(250), reply).await;
            assert!(expired.is_err(), "the bounded wait expires first");
            assert_eq!(
                actor.current_turn_id().as_deref(),
                Some("turn-1"),
                "the id survives the expired wait as the interrupt handle"
            );
            // The caller drops its receiver (the in_flight ask already answered);
            // the turn keeps running and the done callback still fires.
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            assert_eq!(
                done.lock().unwrap().as_slice(),
                ["turn-1"],
                "agent_ask_done hook fired exactly once at completion"
            );
            assert_eq!(actor.current_turn_id(), None);
            actor.shutdown().await.unwrap();
        },
    )
    .await;
}

/// AC5 (ownership half): shutdown closes THIS driver's connection and leaves
/// the app-server running.
///
/// The assertion this replaces was the opposite one: the recorded pid had to
/// be GONE within five seconds, because the driver owned a private
/// app-server child and `kill_on_drop` ended it. Against the shared daemon
/// that same assertion would demand a worker's stop kill every other codex
/// session on the machine.
///
/// The positive marker is a FRESH connection succeeding after the shutdown
/// ack: the daemon is still serving, which no absence check could establish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_the_connection_and_leaves_the_daemon_serving() {
    with_fake_daemon(Behavior::long(), async {
        let socket = fno_agents::codex_inject::codex_app_server_socket_path();
        let (actor, _keep) = start_actor().await;
        let pid = actor.pid().expect("the serving app-server pid");
        assert_eq!(
            pid,
            std::process::id(),
            "the recorded pid is the daemon's, not a child's"
        );
        let _reply = actor.submit("long turn".into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        actor.shutdown().await.unwrap();

        let _reconnected = fno_agents::codex_inject::connect_app_server(&socket)
            .await
            .expect("the shared daemon still serves after one worker stopped");
        // Further commands are refused: the actor is gone even though the
        // daemon is not.
        assert!(
            actor.submit("after shutdown".into()).await.is_err(),
            "a shut-down actor must refuse work"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// The state-root grant on the wire (x-f22f).
//
// These read the frames the fake RECEIVED. They deliberately do not ask the
// fake whether a sandbox was applied: it models no sandbox, so an assertion
// about enforcement would measure the double instead of the target. Whether
// the real app-server HONORS the field was settled by a live probe against the
// real daemon and is recorded in docs/architecture/codex-thread-driver.md.
// What is worth pinning in CI is the other half - that fno's hops put the
// roots on the wire at all, on the carrier that probe identified.
// ---------------------------------------------------------------------------

fn grant_roots() -> Vec<String> {
    vec!["/Users/x/.fno".to_string()]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn granted_thread_puts_the_roots_on_every_turn_start() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let daemon = FakeDaemon::start(Behavior::quick().with_thread_id("thread-grant"));
    let worktree = tempfile::tempdir().unwrap();
    let mut thread =
        CodexThread::start_with_state_dirs(worktree.path(), None, false, None, &grant_roots())
            .await
            .expect("thread starts");
    thread.drive_turn("first").await.expect("turn 1");
    thread.drive_turn("second").await.expect("turn 2");

    // thread/start keeps the scalar posture: the object form is ignored there.
    let start = daemon.first_params("thread/start").expect("a thread/start");
    assert_eq!(start["sandbox"], "workspace-write");
    assert!(start.get("sandboxPolicy").is_none());

    // Every turn carries the grant, not just the first. A turn-level override
    // becomes the thread default, so once would hold for a thread that is
    // never resumed - and a resumed thread re-resolves its posture.
    let turns: Vec<_> = daemon
        .received()
        .into_iter()
        .filter(|f| f.get("method").and_then(|m| m.as_str()) == Some("turn/start"))
        .collect();
    assert_eq!(turns.len(), 2, "two turns were driven");
    for turn in turns {
        let policy = &turn["params"]["sandboxPolicy"];
        assert_eq!(policy["type"], "workspaceWrite");
        assert_eq!(policy["writableRoots"][0], "/Users/x/.fno");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ungranted_thread_emits_todays_frames_unchanged() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let daemon = FakeDaemon::start(Behavior::quick().with_thread_id("thread-plain"));
    let worktree = tempfile::tempdir().unwrap();
    let mut thread = CodexThread::start_with_state_dirs(worktree.path(), None, false, None, &[])
        .await
        .expect("thread starts");
    thread.drive_turn("go").await.expect("turn");

    let start = daemon.first_params("thread/start").expect("a thread/start");
    assert_eq!(start["sandbox"], "workspace-write");
    let turn = daemon.first_params("turn/start").expect("a turn/start");
    assert!(
        turn.get("sandboxPolicy").is_none(),
        "no roots resolved, so the frame must be today's: {turn}"
    );
}

/// A yolo thread is already `danger-full-access`. A workspaceWrite policy
/// would NARROW it, so the roots are dropped rather than sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_thread_is_never_narrowed_by_the_grant() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let daemon = FakeDaemon::start(Behavior::quick().with_thread_id("thread-yolo"));
    let worktree = tempfile::tempdir().unwrap();
    let mut thread =
        CodexThread::start_with_state_dirs(worktree.path(), None, true, None, &grant_roots())
            .await
            .expect("thread starts");
    thread.drive_turn("go").await.expect("turn");

    let start = daemon.first_params("thread/start").expect("a thread/start");
    assert_eq!(start["sandbox"], "danger-full-access");
    let turn = daemon.first_params("turn/start").expect("a turn/start");
    assert!(
        turn.get("sandboxPolicy").is_none(),
        "a full-access thread must not be handed a narrower policy: {turn}"
    );
}
