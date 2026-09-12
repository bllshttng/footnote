//! Task-context continuity journey (x-59b0).
//!
//! Deterministic fixtures around the REAL binary verbs: prepare, submit,
//! observed/unavailable honesty, compaction-stage revalidation, explicit
//! handoff (two-proof shape), and refusal recovery - every failure stage
//! named. Closes by emitting TASK_CONTEXT_CONTINUITY_PASS with the attempt
//! and the retained constraint, so a pass is a positive marker naming its
//! subject, never a bare zero-hit.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const VERB_BIN: &str = env!("CARGO_BIN_EXE_fno-agents");
const ATTEMPT: &str = "20260912T070000Z-journey-abc123";
const CONSTRAINT: &str = "Do not widen scope beyond the plan";

/// Drive one real binary verb: JSON request on stdin, JSON answer parsed.
fn run_verb(verb: &str, payload: &Value) -> (i32, Value) {
    let mut child = Command::new(VERB_BIN)
        .arg(verb)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fno-agents");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write request");
    let out = child.wait_with_output().expect("wait");
    let answer: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{verb}: {e}"));
    (out.status.code().unwrap_or(-1), answer)
}

/// A deterministic attempt fixture: a worktree with a single-file plan.
fn fixture(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("PLAN.md"), "plan bytes\n").expect("plan");
    root
}

fn binding_request(root: &Path) -> Value {
    let plan = std::fs::read_to_string(root.join("PLAN.md")).expect("plan");
    json!({
        "version": 1,
        "node": "x-59b0",
        "attempt": ATTEMPT,
        "harness": "claude",
        "session": "sess-parent",
        "worktree": root.to_string_lossy(),
        "plan_path": "PLAN.md",
        "plan_digest": sha256_hex(plan.as_bytes()),
        "required_constraints": [CONSTRAINT],
        "required_sources": [{
            "path": "PLAN.md",
            "content_revision": "b48ba4b8c",
            "content_digest": sha256_hex(plan.as_bytes()),
            "byte_size": plan.len(),
        }],
        "source_bytes": plan.len(),
        "payload_bytes": 512,
        "stage": "prepared",
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[test]
fn task_context_survives_the_execution_compaction_handoff_journey() {
    let dir = tempfile::tempdir().expect("tmp");
    let root = fixture(dir.path(), "attempt");

    // 1. PREPARE: the binding names the exact plan revision, identity, and
    //    separate byte measures, and its digest is stable.
    let request = binding_request(&root);
    let (code, prepared) = run_verb("task-context-prepare", &json!({"binding": request}));
    assert_eq!(code, 0, "prepare refused: {prepared}");
    assert_eq!(prepared["ok"], json!(true));
    let digest = prepared["binding_digest"]
        .as_str()
        .expect("digest")
        .to_string();
    let mut bound = request.clone();
    bound["binding_digest"] = json!(digest);
    assert_ne!(
        prepared["binding"]["payload_bytes"], prepared["binding"]["source_bytes"],
        "source and payload byte measures must stay separate"
    );

    // 2. SUBMIT (pane and non-pane carry the SAME bound binding): the stage
    //    advances forward only, re-stamping the digest. Prepared is not
    //    Submitted: the submitted binding is a different digest, and the
    //    carry never claims a read.
    let (code, submitted) = run_verb(
        "task-context-stage",
        &json!({"binding": bound, "to": "submitted"}),
    );
    assert_eq!(code, 0, "submit refused: {submitted}");
    assert_eq!(submitted["stage"], json!("submitted"));
    let submitted_digest = submitted["binding_digest"].as_str().expect("d").to_string();
    assert_ne!(submitted_digest, digest, "stage advance must re-stamp");
    let mut bound_submitted = submitted["binding"].clone();
    bound_submitted["binding_digest"] = json!(submitted_digest);

    // Backward is refused at the same stage door (the verb never re-mints a
    // prepared binding; the unit tests pin advance_stage's regression table).
    let (_, backward) = run_verb(
        "task-context-stage",
        &json!({"binding": bound_submitted, "to": "prepared"}),
    );
    assert_eq!(backward["ok"], json!(false), "{backward}");

    // 3. ACCEPTED-TURN OBSERVATION (x-175a's evidence is consumed, not
    //    re-detected): the observed stage is reachable only from submitted,
    //    and unavailable is the honesty ceiling that can never claim observed.
    let (_, observed) = run_verb(
        "task-context-stage",
        &json!({"binding": bound_submitted, "to": "observed"}),
    );
    assert_eq!(observed["stage"], json!("observed"), "{observed}");
    let (_, unavailable) = run_verb(
        "task-context-stage",
        &json!({"binding": bound_submitted, "to": "unavailable"}),
    );
    assert_eq!(unavailable["stage"], json!("unavailable"), "{unavailable}");
    let unavailable_bound = unavailable["binding"].clone();
    let mut unavailable_bound = unavailable_bound;
    unavailable_bound["binding_digest"] = json!(unavailable["binding_digest"]);
    let (_, upgrade) = run_verb(
        "task-context-stage",
        &json!({"binding": unavailable_bound, "to": "observed"}),
    );
    assert!(
        upgrade["reason"]
            .as_str()
            .unwrap()
            .starts_with("stage_regression"),
        "unavailable cannot claim observed: {upgrade}"
    );

    // 4. COMPACTION: the stage advance for compaction revalidates against the
    //    worktree - the binding SURVIVES the boundary unchanged (the pointer
    //    is re-emitted; the session is not replaced).
    let (_, compact_ok) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0"},
            "root": root.to_string_lossy(),
        }),
    );
    assert_eq!(compact_ok["ok"], json!(true), "{compact_ok}");

    // 5. EXPLICIT HANDOFF, two-proof shape: the parent prepared under its own
    //    authority; a FOREIGN successor refuses by name before any delegation;
    //    the AUTHORIZED successor (the named session) revalidates clean.
    let (_, foreign) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0", "attempt": ATTEMPT, "session": "sess-intruder"},
            "root": root.to_string_lossy(),
        }),
    );
    assert_eq!(foreign["ok"], json!(false));
    assert!(
        foreign["reason"]
            .as_str()
            .unwrap()
            .starts_with("foreign_session"),
        "wrong-session refused by name: {foreign}"
    );
    let (_, wrong_attempt) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0", "attempt": "OTHER-attempt", "session": "sess-parent"},
            "root": root.to_string_lossy(),
        }),
    );
    assert!(
        wrong_attempt["reason"]
            .as_str()
            .unwrap()
            .starts_with("wrong_attempt"),
        "wrong-attempt refused by name: {wrong_attempt}"
    );
    let (_, authorized) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0", "attempt": ATTEMPT, "session": "sess-parent"},
            "root": root.to_string_lossy(),
        }),
    );
    assert_eq!(authorized["ok"], json!(true), "{authorized}");

    // 6. FAILED CHILD / missing worktree: a dead or missing root refuses by
    //    name instead of passing vacuously.
    let (_, dead_child) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0"},
            "root": root.join("no-such-child").to_string_lossy(),
        }),
    );
    assert_eq!(dead_child["ok"], json!(false));
    assert!(
        dead_child["reason"]
            .as_str()
            .unwrap()
            .starts_with("missing_source"),
        "failed-child root refused by name: {dead_child}"
    );

    // 7. REFUSAL RECOVERY: a required source changing under the binding
    //    refuses; an authorized task revision (re-prepare) produces a NEW
    //    immutable binding, and restoring the source recovers the original.
    std::fs::write(root.join("PLAN.md"), "REVISED bytes\n").expect("revise");
    let (_, stale) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0"},
            "root": root.to_string_lossy(),
        }),
    );
    assert!(
        stale["reason"]
            .as_str()
            .unwrap()
            .starts_with("stale_source"),
        "changed required source refused by name: {stale}"
    );
    let (_, revised) = run_verb(
        "task-context-prepare",
        &json!({"binding": {
            "version": 1,
            "node": "x-59b0",
            "attempt": ATTEMPT,
            "harness": "claude",
            "session": "sess-parent",
            "worktree": root.to_string_lossy(),
            "plan_path": "PLAN.md",
            "plan_digest": sha256_hex(b"REVISED bytes\n"),
            "required_constraints": [CONSTRAINT],
            "required_sources": [{
                "path": "PLAN.md",
                "content_revision": "revised-1",
                "content_digest": sha256_hex(b"REVISED bytes\n"),
                "byte_size": "REVISED bytes\n".len(),
            }],
            "source_bytes": "REVISED bytes\n".len(),
            "payload_bytes": 512,
            "stage": "prepared",
        }}),
    );
    assert_eq!(
        revised["ok"],
        json!(true),
        "authorized revision re-prepares: {revised}"
    );
    assert_ne!(
        revised["binding_digest"],
        json!(submitted_digest),
        "an authorized revision is a NEW immutable binding"
    );
    std::fs::write(root.join("PLAN.md"), "plan bytes\n").expect("restore");
    let (_, recovered) = run_verb(
        "task-context-revalidate",
        &json!({
            "binding": bound_submitted,
            "expect": {"node": "x-59b0"},
            "root": root.to_string_lossy(),
        }),
    );
    assert_eq!(
        recovered["ok"],
        json!(true),
        "restored source recovers: {recovered}"
    );

    println!("TASK_CONTEXT_CONTINUITY_PASS attempt={ATTEMPT} constraint={CONSTRAINT:?}");
}
