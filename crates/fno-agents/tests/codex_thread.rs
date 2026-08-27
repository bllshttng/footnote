use fno_agents::codex_thread::{
    parse_thread_start_response, thread_start_request_json, CodexThread, CodexThreadActor,
    InterruptOutcome, ThreadStartError,
};
use std::sync::{Arc, Mutex};

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

/// A stub app-server on PATH: enough protocol to start a thread, then a turn
/// that streams MORE frames than the old frame ceiling and goes quiet LONGER
/// than the old per-frame timeout before completing. The journey test's live
/// seed answers in a handful of frames and well under 15s, so it cleared both
/// old bounds by being small; this fixture is the scale the real lane runs at.
const FAKE_APP_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method = msg.get("method")
    if method == "initialize":
        send({"id": msg.get("id"), "result": {}})
    elif method == "thread/start":
        send({"id": msg.get("id"), "result": {"thread": {"id": "thread-bounds", "path": "/tmp/fake-rollout.jsonl"}}})
    elif method == "turn/start":
        send({"id": msg.get("id"), "result": {"turn": {"id": "turn-bounds"}}})
        for i in range(300):
            send({"method": "turn/event", "params": {"seq": i}})
        time.sleep(16)
        send({"method": "turn/completed", "params": {"turn": {"id": "turn-bounds", "status": "completed", "items": [{"type": "agentMessage", "text": "BOUNDS_EXCEEDED_TOKEN"}]}}})
"#;

#[tokio::test]
async fn turn_survives_more_frames_and_a_longer_gap_than_the_old_bounds() {
    with_fake_codex(FAKE_APP_SERVER, async {
        let worktree = tempfile::tempdir().unwrap();
        let mut thread = CodexThread::start(worktree.path(), None, false, None)
            .await
            .expect("thread starts against the stub app-server");
        let result = thread.drive_turn("run the wide turn").await;
        let turn = result.expect("turn completes under the whole-turn budget");
        assert_eq!(turn.turn_id, "turn-bounds");
        assert_eq!(turn.text, "BOUNDS_EXCEEDED_TOKEN");
    })
    .await;
}

// ---------------------------------------------------------------------------
// Actor tests (single-owner rewrite, x-de10). Every script is a self-contained
// fake app-server with the scenario's behavior hardcoded (no env knobs: tests
// in one binary share the process environment, so per-test env would race).
// ---------------------------------------------------------------------------

/// Serializes every PATH mutation in this binary: `set_var`/`remove_var` race
/// when two fake-codex tests run concurrently and one restores PATH under the
/// other's feet, resolving `codex` to the REAL binary mid-test.
static PATH_LOCK: Mutex<()> = Mutex::new(());

async fn with_fake_codex(script: &str, body: impl std::future::Future<Output = ()>) {
    let _guard = PATH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake = bin_dir.path().join("codex");
    std::fs::write(&fake, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let saved_path = std::env::var_os("PATH");
    let mut prefixed = std::ffi::OsString::from(bin_dir.path().as_os_str());
    prefixed.push(":");
    if let Some(rest) = saved_path.as_ref() {
        prefixed.push(rest);
    }
    std::env::set_var("PATH", &prefixed);
    body.await;
    if let Some(path) = saved_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
}

/// Shared fake skeleton: a reader thread queues stdin lines so a turn's sleep
/// can answer steer/interrupt MID-TURN (a sequential `for line in sys.stdin`
/// fake cannot - it sleeps inside the turn/start branch and leaves the
/// interrupt unread for 30s, which is not how the real app-server behaves).
/// Each scenario embeds it with its own turn-wait inner loop.
fn queue_fake(turn_wait_body: &str) -> String {
    format!(
        r#"#!/usr/bin/env python3
import json, sys, time, threading, queue

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

q = queue.Queue()
def _reader():
    for line in sys.stdin:
        q.put(line)
threading.Thread(target=_reader, daemon=True).start()

turn_n = 0
while True:
    try:
        msg = json.loads(q.get())
    except Exception:
        break
    method = msg.get("method")
    if method == "initialize":
        send({{"id": msg.get("id"), "result": {{}}}})
    elif method == "thread/start":
        send({{"id": msg.get("id"), "result": {{"thread": {{"id": "thread-actor", "path": "/tmp/fake-actor-rollout.jsonl"}}}}}})
    elif method == "turn/start":
        turn_n += 1
        send({{"id": msg.get("id"), "result": {{"turn": {{"id": f"turn-{{turn_n}}"}}}}}})
{turn_wait_body}
        send({{"method": "turn/completed", "params": {{"turn": {{"id": f"turn-{{turn_n}}", "status": "completed", "items": [{{"type": "agentMessage", "text": f"REPLY-{{turn_n}}"}}]}}}}}})
"#
    )
}

/// AC1: a turn that completes 1.2s in; a steer arriving mid-turn acks into the
/// SAME turn immediately.
fn fake_steer() -> String {
    queue_fake(
        r#"        end = time.time() + 1.2
        steered = False
        while time.time() < end:
            try:
                m2 = json.loads(q.get(timeout=0.1))
            except queue.Empty:
                continue
            if m2.get("method") == "turn/steer" and not steered:
                send({"id": m2.get("id"), "result": {"turn": {"id": f"turn-{turn_n}"}}})
                steered = True"#,
    )
}

/// AC2: a steer arriving mid-turn fails its expectedTurnId precondition
/// immediately (the turn completed in the race window); the completion still
/// arrives at the 1.2s mark, and the next turn/start drives a fresh turn.
fn fake_precondition() -> String {
    queue_fake(
        r#"        end = time.time() + 1.2
        failed = False
        while time.time() < end:
            try:
                m2 = json.loads(q.get(timeout=0.1))
            except queue.Empty:
                continue
            if m2.get("method") == "turn/steer" and not failed:
                send({"id": m2.get("id"), "error": {"message": f"turn-{turn_n} is not active"}})
                failed = True"#,
    )
}

/// AC5/AC17: a turn that would run 30s unless interrupted; interrupt acks and
/// completes the turn as `interrupted` immediately.
fn fake_interrupt() -> String {
    queue_fake(
        r#"        end = time.time() + 30
        interrupted = False
        while time.time() < end:
            try:
                m2 = json.loads(q.get(timeout=0.1))
            except queue.Empty:
                continue
            if m2.get("method") == "turn/interrupt" and not interrupted:
                send({"id": m2.get("id"), "result": {}})
                send({"method": "turn/completed", "params": {"turn": {"id": f"turn-{turn_n}", "status": "interrupted", "items": []}}})
                interrupted = True
                sys.exit(0)"#,
    )
}

/// AC3: a turn that completes 1.5s in - slower than the caller's bounded wait,
/// fast enough to observe the late receipt. No mid-turn input, so no queue
/// needed.
const FAKE_LATE: &str = r#"#!/usr/bin/env python3
import json, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

turn_n = 0

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method = msg.get("method")
    if method == "initialize":
        send({"id": msg.get("id"), "result": {}})
    elif method == "thread/start":
        send({"id": msg.get("id"), "result": {"thread": {"id": "thread-actor", "path": "/tmp/fake-actor-rollout.jsonl"}}})
    elif method == "turn/start":
        turn_n += 1
        send({"id": msg.get("id"), "result": {"turn": {"id": f"turn-{turn_n}"}}})
        time.sleep(1.5)
        send({"method": "turn/completed", "params": {"turn": {"id": f"turn-{turn_n}", "status": "completed", "items": [{"type": "agentMessage", "text": "LATE_REPLY"}]}}})
"#;

async fn start_actor() -> (CodexThreadActor, tempfile::TempDir) {
    let worktree = tempfile::tempdir().unwrap();
    let driver = CodexThread::start(worktree.path(), None, false, None)
        .await
        .expect("thread starts against the fake app-server");
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
    with_fake_codex(&fake_steer(), async {
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
    with_fake_codex(&fake_precondition(), async {
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
    })
    .await;
}

/// AC5 + AC17: interrupt mid-turn returns the terminal `interrupted` receipt,
/// and the driving turn id survives until that completion actually routes -
/// it is the interrupt handle after any caller-side timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_mid_turn_resolves_interrupted_and_survives_as_handle() {
    with_fake_codex(&fake_interrupt(), async {
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

/// AC3 + AC17 (actor half): a submit whose bounded wait expires leaves the
/// turn RUNNING, keeps the turn id as the interrupt handle, and still
/// resolves the receipt when the turn completes - the daemon layers its 90s
/// `in_flight` receipt on exactly this behavior.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_submit_wait_leaves_turn_running_and_receipt_arrives_later() {
    let done: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&done);
    with_fake_codex(FAKE_LATE, async move {
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
    })
    .await;
}

/// AC5 (pid half): shutdown acks only AFTER the driver dropped, so
/// `kill_on_drop` has already signaled the app-server child - a caller that
/// waits for the ack never reads a live pid as stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_the_child_before_acking() {
    with_fake_codex(&fake_interrupt(), async {
        let (actor, _keep) = start_actor().await;
        let pid = actor.pid().expect("fake app-server pid");
        let _reply = actor.submit("long turn".into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        actor.shutdown().await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // `kill -0` probes existence without signaling; success = alive.
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "app-server child {pid} outlived the shutdown ack"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
}
