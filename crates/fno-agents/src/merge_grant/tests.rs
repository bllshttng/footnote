//! Pure tests over JSON entries, a claim closure and a `LiveConfig` literal.
//! Ported from `cli/tests/unit/test_pr_merge_grant.py` (the resolver suite)
//! under each test's name, without the `test_` prefix; the queue and op
//! tests are new here.

use super::*;
use serde_json::json;
use std::path::PathBuf;

const NODE: &str = "ab-grantunit1";
const PR: i64 = 42;

fn receipt(approved: bool, source: &str, at: &str) -> Value {
    json!({"approved": approved, "source": source, "recorded_by": "spawner", "recorded_at": at})
}

fn do_row(grant: Option<Value>, session: &str) -> Value {
    let mut row = json!({
        "phase": "execute",
        "harness": "claude",
        "session_id": session,
        "started_at": "2026-08-24T11:00:00Z",
    });
    if let Some(g) = grant {
        row["merge_grant"] = g;
    }
    row
}

fn node_with(id: &str, pr: i64, sessions: Vec<Value>) -> Value {
    json!({"id": id, "title": "t", "pr_number": pr, "sessions": sessions})
}

fn live() -> LiveConfig {
    LiveConfig {
        enabled: true,
        grant_dispatch: true,
        floor_block: None,
    }
}

fn cfg_off() -> LiveConfig {
    LiveConfig {
        enabled: false,
        grant_dispatch: true,
        floor_block: None,
    }
}

fn grant_not_dispatch() -> LiveConfig {
    LiveConfig {
        enabled: true,
        grant_dispatch: false,
        floor_block: None,
    }
}

fn below_floor() -> LiveConfig {
    LiveConfig {
        enabled: true,
        grant_dispatch: true,
        floor_block: Some(
            "auto-merge refused: review.posture resolves to no_review (rank 1), \
below the merge floor self_review (rank 3)."
                .to_string(),
        ),
    }
}

/// Everything else reads Stale: the Python fixture's default claim state.
fn claims_of(pairs: Vec<(&str, ClaimState)>) -> impl Fn(&str) -> ClaimState + use<'_> {
    move |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, s)| *s)
            .unwrap_or(Stale)
    }
}

fn stale_claims() -> impl Fn(&str) -> ClaimState {
    claims_of(vec![])
}

fn verdict(
    entries: &[Value],
    claim: &dyn Fn(&str) -> ClaimState,
    cfg: &dyn Fn() -> LiveConfig,
) -> Verdict {
    verdict_for_pr(entries, PR, None, claim, cfg)
}

// --- AC10-HP: the granted path -------------------------------------------

#[test]
fn verdict_rows() {
    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    let v = verdict(&entries, &stale_claims(), &live);
    assert_eq!(v.state, GRANTED);
    assert_eq!(v.node_id.as_deref(), Some(NODE));
    assert_eq!(v.claim_state.as_deref(), Some("stale"));
    assert_eq!(
        v.grant.expect("grant rides a granted verdict")["approved"],
        json!(true)
    );

    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    assert_eq!(verdict(&entries, &stale_claims(), &live).state, GRANTED);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    let v = verdict(&entries, &stale_claims(), &below_floor);
    assert_eq!(v.state, HELD);
    assert!(v.reason.contains("below the merge floor"), "{}", v.reason);
    assert!(v.reason.contains("no_review"), "{}", v.reason);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-09-02T10:00:00+00:00")),
            "w1",
        )],
    )];
    let v = verdict(&entries, &stale_claims(), &live);
    assert_eq!(v.state, UNKNOWN);
    assert!(v.reason.contains("canonical"), "{}", v.reason);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![
            do_row(Some(receipt(true, "config", "2026-08-24T10:00:00Z")), "w0"),
            do_row(
                Some(receipt(false, "no-merge-flag", "2026-08-24T12:00:00Z")),
                "w1",
            ),
        ],
    )];
    let v = verdict(&entries, &stale_claims(), &live);
    assert_eq!(v.state, REFUSED);
    assert!(v.reason.contains("no-merge-flag"), "{}", v.reason);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![
            do_row(
                Some(receipt(false, "no-merge-flag", "2026-08-24T10:00:00Z")),
                "w0",
            ),
            do_row(Some(receipt(true, "config", "2026-08-24T12:00:00Z")), "w1"),
        ],
    )];
    assert_eq!(verdict(&entries, &stale_claims(), &live).state, GRANTED);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![
            do_row(
                Some(receipt(false, "no-merge-flag", "2026-08-24T12:00:00Z")),
                "w1",
            ),
            do_row(Some(receipt(true, "config", "2026-08-24T10:00:00Z")), "w0"),
        ],
    )];
    assert_eq!(verdict(&entries, &stale_claims(), &live).state, REFUSED);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![
            do_row(Some(receipt(true, "config", "2026-08-24T12:00:00Z")), "w0"),
            do_row(Some(receipt(false, "config", "2026-08-24T12:00:00Z")), "w1"),
        ],
    )];
    assert_eq!(verdict(&entries, &stale_claims(), &live).state, UNKNOWN);
}

// --- AC9-EDGE: newest explicit receipt wins, by recorded_at not row order -

// --- AC10-CON: liveness and standing config hold the merge ----------------

#[test]
fn claim_rows() {
    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    for state in [Live, Suspect] {
        let v = verdict(&entries, &claims_of(vec![(NODE, state)]), &live);
        assert_eq!(v.state, HELD, "{state:?} must hold");
        assert!(
            v.reason
                .contains(&format!("node claim is {}", state.as_str())),
            "{}",
            v.reason
        );
    }

    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    assert_eq!(
        verdict(&entries, &claims_of(vec![(NODE, Corrupted)]), &live).state,
        UNKNOWN
    );

    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    assert_eq!(verdict(&entries, &stale_claims(), &cfg_off).state, HELD);

    let entries = vec![node_with(
        NODE,
        PR,
        vec![do_row(
            Some(receipt(true, "config", "2026-08-24T12:00:00Z")),
            "w1",
        )],
    )];
    assert_eq!(
        verdict(&entries, &stale_claims(), &grant_not_dispatch).state,
        HELD
    );
}

// --- Absence and ambiguity never grant ------------------------------------

#[test]
fn node_rows() {
    assert_eq!(verdict(&[], &stale_claims(), &live).state, ABSENT);

    let entries = vec![node_with(NODE, PR, vec![do_row(None, "w1")])];
    assert_eq!(verdict(&entries, &stale_claims(), &live).state, ABSENT);

    let entries = vec![
        json!({"id": "ab-grantunit1", "title": "a", "pr_number": PR}),
        json!({"id": "ab-grantunit2", "title": "b", "pr_number": PR}),
    ];
    assert_eq!(verdict(&entries, &stale_claims(), &live).state, UNKNOWN);

    let bads = [
        json!({"approved": true, "source": "config", "recorded_by": "s"}),
        json!({"approved": true, "source": "config", "recorded_by": "s",
               "recorded_at": "2026-08-24T12:00:00Z", "extra": 1}),
        json!({"approved": "yes", "source": "config", "recorded_by": "s",
               "recorded_at": "2026-08-24T12:00:00Z"}),
        json!({"approved": true, "source": "config", "recorded_by": "s",
               "recorded_at": "yesterday"}),
    ];
    for bad in &bads {
        let entries = vec![node_with(NODE, PR, vec![do_row(Some(bad.clone()), "w1")])];
        let v = verdict(&entries, &stale_claims(), &live);
        assert_eq!(v.state, UNKNOWN, "{bad}");
    }
}

// --- AC12-ERR: malformed receipts are loud unknowns ------------------------

// --- The queue --------------------------------------------------------------

fn queue_node(id: &str, pr: i64, extra: Value) -> Value {
    let mut n = json!({
        "id": id,
        "title": "t",
        "pr_number": pr,
        "pr_url": format!("https://github.com/owner/repo/pull/{pr}"),
        "cwd": "/checkouts/one",
        "sessions": [do_row(Some(receipt(true, "config", "2026-08-24T12:00:00Z")), "w1")],
    });
    for (k, v) in extra.as_object().expect("extra is an object") {
        n[k.as_str()] = v.clone();
    }
    n
}

fn root_from_cwd(entry: &Value) -> Option<PathBuf> {
    entry.get("cwd").and_then(Value::as_str).map(PathBuf::from)
}

fn drain(entries: &[Value], claims: &dyn Fn(&str) -> ClaimState) -> Value {
    queue_from_entries(entries, claims, &root_from_cwd, &|_p| live(), 0)
}

#[test]
fn queue_rows() {
    let entries = vec![
        queue_node("ab-grantone", 1, json!({})),
        queue_node("ab-grantlive", 2, json!({})),
        queue_node("ab-grantsup", 3, json!({"status": "superseded"})),
        queue_node("ab-grantmerged", 4, json!({"merge_status": "merged"})),
        json!({"id": "ab-grantless", "title": "t", "pr_number": 5,
               "pr_url": "https://github.com/owner/repo/pull/5",
               "cwd": "/checkouts/one", "sessions": [do_row(None, "w1")]}),
    ];
    let claims = claims_of(vec![("ab-grantone", Free), ("ab-grantlive", Live)]);
    let out = drain(&entries, &claims);
    assert_eq!(out["candidates"], json!(2), "{out}");
    assert_eq!(out["verdicts"]["granted"], json!(1), "{out}");
    assert_eq!(out["verdicts"]["held"], json!(1), "{out}");
    let queue = out["queue"].as_array().expect("queue is an array");
    assert_eq!(queue.len(), 1, "{out}");
    let row = &queue[0];
    assert_eq!(row["node_id"], json!("ab-grantone"));
    assert_eq!(row["pr"], json!(1));
    assert_eq!(row["repo_slug"], json!("owner/repo"));
    assert_eq!(row["cwd"], json!("/checkouts/one"));
    let grant = row["grant"].as_object().expect("grant is an object");
    let mut keys: Vec<String> = grant.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "recorded_at".to_string(),
            "recorded_by".to_string(),
            "source".to_string()
        ],
        "grant carries exactly the three writer keys"
    );

    let entries = vec![
        queue_node("ab-sup", 1, json!({"status": "superseded"})),
        queue_node("ab-merged", 2, json!({"merge_status": "merged"})),
        queue_node("ab-closed", 3, json!({"merge_status": "closed"})),
        json!({"id": "ab-none", "title": "t", "pr_number": 4,
               "pr_url": "https://github.com/owner/repo/pull/4",
               "cwd": "/checkouts/one", "sessions": [do_row(None, "w1")]}),
    ];
    let out = drain(&entries, &stale_claims());
    assert_eq!(out["candidates"], json!(0), "{out}");
    assert_eq!(out["queue"], json!([]), "{out}");

    let entries = vec![
        json!({"id": "ab-noslug", "title": "t", "pr_number": 1, "cwd": "/checkouts/one",
               "sessions": [do_row(Some(receipt(true, "config", "2026-08-24T12:00:00Z")), "w1")]}),
        json!({"id": "ab-nocwd", "title": "t", "pr_number": 2,
               "pr_url": "https://github.com/owner/repo/pull/2",
               "sessions": [do_row(Some(receipt(true, "config", "2026-08-24T12:00:00Z")), "w1")]}),
    ];
    let out = drain(&entries, &stale_claims());
    assert_eq!(out["candidates"], json!(2), "{out}");
    assert_eq!(out["verdicts"]["unknown"], json!(2), "{out}");
    assert_eq!(out["queue"], json!([]), "{out}");

    let entries = vec![
        queue_node("ab-done", 1, json!({"status": "done"})),
        queue_node("ab-open", 2, json!({"status": "in_review"})),
    ];
    let out = drain(&entries, &stale_claims());
    assert_eq!(out["candidates"], json!(1), "{out}");
    let queue = out["queue"].as_array().expect("queue is an array");
    assert_eq!(queue.len(), 1, "{out}");
    assert_eq!(queue[0]["pr"], json!(2), "{out}");

    let entries = vec![
        queue_node("ab-one", 1, json!({})),
        queue_node("ab-two", 2, json!({})),
        queue_node("ab-three", 3, json!({})),
    ];
    let roots = std::cell::Cell::new(0usize);
    let configs = std::cell::Cell::new(0usize);
    let claims = claims_of(vec![("ab-one", Free), ("ab-two", Free), ("ab-three", Free)]);
    let out = queue_from_entries(
        &entries,
        &claims,
        &|e| {
            roots.set(roots.get() + 1);
            root_from_cwd(e)
        },
        &|_p| {
            configs.set(configs.get() + 1);
            live()
        },
        0,
    );
    assert_eq!(out["queue"].as_array().expect("queue").len(), 3, "{out}");
    assert_eq!(roots.get(), 1, "root_of runs once per checkout");
    assert_eq!(configs.get(), 1, "cfg_of runs once per checkout");

    let entries = vec![
        queue_node("ab-rot1", 1, json!({})),
        queue_node("ab-rot2", 2, json!({})),
        queue_node("ab-rot3", 3, json!({})),
    ];
    let claims = claims_of(vec![
        ("ab-rot1", Free),
        ("ab-rot2", Free),
        ("ab-rot3", Free),
    ]);
    let order = |rotate: u64| -> Vec<i64> {
        let out = queue_from_entries(&entries, &claims, &root_from_cwd, &|_p| live(), rotate);
        out["queue"]
            .as_array()
            .expect("queue is an array")
            .iter()
            .map(|row| row["pr"].as_i64().expect("pr is an integer"))
            .collect()
    };
    assert_eq!(order(0), vec![1, 2, 3]);
    assert_eq!(order(1), vec![2, 3, 1]);
    assert_eq!(order(5), vec![3, 1, 2]);

    assert_eq!(
        repo_slug_from_pr_url("https://github.com/owner/repo/pull/7").as_deref(),
        Some("owner/repo")
    );
    assert_eq!(
        repo_slug_from_pr_url("https://github.com/owner/repo/pull/7#issuecomment-1").as_deref(),
        Some("owner/repo")
    );
    assert_eq!(
        repo_slug_from_pr_url("https://gitlab.com/o/r/-/merge_requests/1"),
        None
    );
    assert_eq!(repo_slug_from_pr_url("nope"), None);

    let verdict = verdict_op(
        Err("bad json".to_string()),
        &json!({"pr": PR, "cwd": "/tmp"}),
    );
    let v: Value = serde_json::from_str(&verdict).expect("verdict is json");
    assert_eq!(v["state"], json!(UNKNOWN));
    assert!(
        v["reason"]
            .as_str()
            .unwrap_or("")
            .starts_with("graph unreadable"),
        "{}",
        v["reason"]
    );
    let q = queue_op(Err("bad json".to_string()), 0, std::time::Instant::now());
    assert_eq!(q["error"], json!("graph unreadable: bad json"));
    assert!(q["elapsed_ms"].is_u64(), "{q}");

    let q = queue_op(Ok(vec![]), 0, std::time::Instant::now());
    assert_eq!(q["candidates"], json!(0), "{q}");
    assert!(q["elapsed_ms"].is_u64(), "{q}");

    let out = run_op("grant-nope", &json!({"cwd": "/tmp"}));
    let o: Value = serde_json::from_str(&out).expect("receipt is json");
    assert_eq!(o["error"], json!("unknown op grant-nope"));

    // The bound-manifest fold, over a real linked worktree: the live
    // manifest binds the node and refuses; an unbound node keeps the legacy
    // posture; a legacy manifest without the posture key permits; an
    // ambiguous binding fails closed; an unreadable manifest never reads as
    // permission; the branch-keyed read serves the authority walk.
    let _lock = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (base, repo, wt) = bound_fixture();
    let _home = HomeGuard::at(base.path());
    write_manifest(
        &wt,
        "---\nauto_merge_approved: false\nauto_merge_source: flag-no-merge\nharness_session_id: w-bound\n---\ngraph_node_id: ab-bound1\n",
    );
    match bound_node_posture(&repo, "ab-bound1") {
        Ok(Some(bound)) => {
            assert_eq!(bound.approved, Some(false));
            assert_eq!(bound.source.as_deref(), Some("flag-no-merge"));
            assert_eq!(bound.session.as_deref(), Some("w-bound"));
            assert!(bound.live, "the live manifest outranks archives");
        }
        other => panic!("expected the live bound manifest, got {other:?}"),
    }
    assert!(matches!(
        bound_fold(&repo, "ab-bound1"),
        ManifestFold::Refuse(_)
    ));
    assert!(matches!(
        bound_fold(&repo, "ab-nobody"),
        ManifestFold::Unbound
    ));
    assert!(matches!(
        branch_bound_manifest(&repo, "feature/x"),
        BoundRead::Read(_)
    ));
    assert!(matches!(branch_bound_manifest(&repo, ""), BoundRead::None));
    // A legacy manifest without the posture key permits and stays silent.
    let wt2 = base.path().join("wt2");
    git_worktree(&repo, &wt2, "feature/y");
    write_manifest(
        &wt2,
        "---\nharness_session_id: w-old\n---\ngraph_node_id: ab-legacy\n",
    );
    match bound_fold(&repo, "ab-legacy") {
        ManifestFold::Permit(b) => assert_eq!(b.approved, None),
        other => panic!("legacy manifest permits, got {other:?}"),
    }
    // Two binds of one node never grant.
    let wt3 = base.path().join("wt3");
    git_worktree(&repo, &wt3, "feature/z");
    write_manifest(
        &wt3,
        "---\nauto_merge_approved: true\n---\ngraph_node_id: ab-legacy\n",
    );
    assert!(bound_node_posture(&repo, "ab-legacy").is_err());
    // A manifest path that is not a file is unreadable, never permission.
    let manifest = crate::state_path::resolve("target-state", wt).expect("target-state resolves");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir_all(&manifest).unwrap();
    assert!(matches!(read_bound_manifest(&wt), BoundRead::Unreadable(_)));
    drop(base);

    // Queue parity: with config enabled in the scratch repo, a granted row
    // whose node's bound manifest refuses drops, moving the count, and the
    // queue never widens the merge verb's gates.
    let (base, repo, wt) = bound_fixture();
    let _home2 = HomeGuard::at(base.path());
    write_manifest(
        &wt,
        "---\nauto_merge_approved: false\n---\ngraph_node_id: ab-qbound\n",
    );
    std::fs::create_dir_all(repo.join(".fno")).unwrap();
    std::fs::write(
        repo.join(".fno").join("config.toml"),
        "[auto_merge]\nenabled = true\n",
    )
    .unwrap();
    let entries = vec![json!({
        "id": "ab-qbound", "title": "t", "slug": "ab-qbound", "type": "feature",
        "status": "ready", "priority": "p2",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "cwd": repo.to_string_lossy(),
        "sessions": [do_row(Some(receipt(true, "config", "2026-10-01T00:00:00Z")), "w1")],
    })];
    let q = queue_op(Ok(entries), 0, std::time::Instant::now());
    assert_eq!(q["verdicts"]["granted"], json!(0), "{q}");
    assert_eq!(q["verdicts"]["refused"], json!(1), "{q}");
    assert_eq!(q["queue"].as_array().map(Vec::len), Some(0), "{q}");
    drop(base);
}

// --- The ops ----------------------------------------------------------------

// --- narrowed store reads (AC1-AC4) ----------------------------------------

fn granted_node(id: &str, pr: i64, status: &str) -> Value {
    json!({
        "id": id, "title": id, "slug": id, "type": "feature",
        "status": status, "priority": "p2",
        "pr_number": pr,
        "pr_url": format!("https://github.com/owner/repo/pull/{pr}"),
        "cwd": "/tmp/grant-fixture",
        "sessions": [do_row(Some(receipt(true, "config", "2026-09-21T00:00:00Z")), "w1")],
    })
}

fn plain_node(id: &str) -> Value {
    json!({
        "id": id, "title": id, "slug": id, "type": "feature",
        "status": "ready", "priority": "p2",
    })
}

fn grant_sqlite_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let entries = json!([
        granted_node("ab-ac1open", 11, "ready"),
        granted_node("ab-ac1done", 12, "done"),
        json!({
            "id": "ab-ac1merged", "title": "m", "slug": "m", "type": "feature",
            "status": "ready", "priority": "p2",
            "pr_number": 13, "merge_status": "merged",
            "pr_url": "https://github.com/owner/repo/pull/13",
            "cwd": "/tmp/grant-fixture",
        }),
        json!({
            "id": "ab-ac1carry", "title": "c", "slug": "c", "type": "feature",
            "status": "ready", "priority": "p2",
            "cwd": "/tmp/grant-fixture",
            "additional_prs": [
                {"number": 11, "url": "https://github.com/owner/repo/pull/11"}
            ],
        }),
        json!({
            "id": "ab-ac1urlonly", "title": "u", "slug": "u", "type": "feature",
            "status": "ready", "priority": "p2",
            "cwd": "/tmp/grant-fixture",
            "additional_prs": [
                {"url": "https://github.com/external/repo/pull/11"}
            ],
        }),
        plain_node("ab-ac1plain1"),
        plain_node("ab-ac1plain2"),
    ]);
    crate::graph_store::seed_rows(&graph, entries.as_array().unwrap()).unwrap();
    (dir, graph)
}

fn git(args: &[&str], cwd: &std::path::Path) {
    let out = std::process::Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A scratch repo with one linked worktree, each carrying a `.fno` state
/// dir. Returned TempDir keeps every path alive until the test ends.
fn bound_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let repo_s = repo.to_str().expect("repo path is utf8");
    git(&["init", "-q", repo_s], base.path());
    std::fs::write(repo.join("README.md"), "x").unwrap();
    git(&["add", "."], &repo);
    git(
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
        &repo,
    );
    let wt = base.path().join("wt");
    git_worktree(&repo, &wt, "feature/x");
    (base, repo, wt)
}

fn git_worktree(repo: &std::path::Path, wt: &std::path::Path, branch: &str) {
    let wt_s = wt.to_str().expect("worktree path is utf8");
    git(&["worktree", "add", "-q", "-b", branch, wt_s], repo);
    std::fs::create_dir_all(wt.join(".fno")).unwrap();
}

fn write_manifest(wt: &std::path::Path, text: &str) {
    let path = crate::state_path::resolve("target-state", wt).expect("target-state resolves");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// Point the state roots at the fixture base so the canonical resolver
/// answers inside the tempdir instead of the machine's own spaces. Held
/// under the test env lock, restored on drop.
struct HomeGuard {
    agents_home: Option<std::ffi::OsString>,
    spaces: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn at(base: &std::path::Path) -> Self {
        let agents_home = std::env::var_os("FNO_AGENTS_HOME");
        let spaces = std::env::var_os("FNO_SPACES_DIR");
        std::env::set_var("FNO_AGENTS_HOME", base.join("home"));
        std::env::set_var("FNO_SPACES_DIR", base.join("spaces"));
        Self {
            agents_home,
            spaces,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.agents_home.take() {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        match self.spaces.take() {
            Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
            None => std::env::remove_var("FNO_SPACES_DIR"),
        }
    }
}

/// AC1-HP: the narrowed read keeps the open grant node and the grantless
/// PR-11 carrier, in ordinal order, each equal to its full-read row on the
/// keys the grant ops read.
#[test]
fn narrow_rows() {
    let (_dir, graph) = grant_sqlite_fixture();
    let narrowed = crate::graph_store::read_pr_rows(&graph, None).unwrap();
    let ids: Vec<&str> = narrowed
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, vec!["ab-ac1open", "ab-ac1carry", "ab-ac1urlonly"]);
    let full = crate::graph_store::read_rows(&graph).unwrap();
    for key in [
        "id",
        "status",
        "pr_number",
        "pr_url",
        "additional_prs",
        "merge_status",
        "cwd",
        "sessions",
    ] {
        for row in &narrowed {
            let id = row.get("id").and_then(Value::as_str).unwrap();
            let want = full
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(id));
            assert_eq!(row.get(key), want.and_then(|e| e.get(key)), "{key} of {id}");
        }
    }

    let (_dir, graph) = grant_sqlite_fixture();
    let full = Ok(crate::graph_store::read_rows(&graph).unwrap());
    let narrowed = Ok(crate::graph_store::read_pr_rows(&graph, None)
        .map(|rows| crate::backlog::api::rows_in(&rows))
        .unwrap());
    let a = queue_op(full, 0, std::time::Instant::now());
    let b = queue_op(narrowed, 0, std::time::Instant::now());
    let strip = |mut receipt: Value| {
        receipt.as_object_mut().unwrap().remove("elapsed_ms");
        receipt
    };
    assert_eq!(strip(a), strip(b));

    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let entries = json!([
        granted_node("ab-ac4one", 11, "ready"),
        granted_node("ab-ac4two", 11, "ready"),
    ]);
    crate::graph_store::seed_rows(&graph, entries.as_array().unwrap()).unwrap();
    let narrowed = crate::graph_store::read_pr_rows(&graph, Some(11)).unwrap();
    let full = crate::graph_store::read_rows(&graph).unwrap();
    for rows in [&narrowed, &full] {
        let v = verdict_for_pr(rows, 11, None, &stale_claims(), &live);
        assert_eq!(v.state, UNKNOWN);
        assert!(v.reason.contains("ab-ac4one"), "{}", v.reason);
        assert!(v.reason.contains("ab-ac4two"), "{}", v.reason);
    }
}

/// AC3-HP: the queue receipt is identical over the narrowed and full reads.

/// AC4-ERR: two open grant nodes carrying the same PR stay ambiguous over
/// the narrowed read, naming both ids, as the full read does.

// ── the head-scoped operator merge grant ──────────────────────────────────

fn law_row(authority: &str, decision: Option<&str>) -> Value {
    let mut row = json!({"authority_source": authority});
    if let Some(d) = decision {
        row["decision"] = json!(d);
    }
    row
}

fn decisions_payload(rows: Vec<Value>) -> Vec<u8> {
    json!({ "decisions": rows }).to_string().into_bytes()
}

#[test]
fn op_rows() {
    let sha = "a29b38c37b18e737eaf850e8765920498287cabf";
    assert_eq!(
        parse_head_grant_subject(&format!("merge-grant:o/r#42@{sha}")),
        Some(("o/r".to_string(), 42, sha.to_string()))
    );
    // A bare repo slug is a real subject shape: the minter holds whatever
    // the remote normalizes to, with or without an owner.
    assert_eq!(
        parse_head_grant_subject(&format!("merge-grant:footnote#2739@{sha}")),
        Some(("footnote".to_string(), 2739, sha.to_string()))
    );
    for bad in [
        "merge-grant:o/r#42@short",
        "merge-grant:#42@a29b38c37b18e737eaf850e8765920498287cabf",
        "merge-grant:42@a29b38c37b18e737eaf850e8765920498287cabf",
        "other:o/r#42@a29b38c37b18e737eaf850e8765920498287cabf",
        "merge-grant:o/r#zero@a29b38c37b18e737eaf850e8765920498287cabf",
        "merge-grant:o/r#0@a29b38c37b18e737eaf850e8765920498287cabf",
    ] {
        assert_eq!(
            parse_head_grant_subject(bad),
            None,
            "{bad} must not parse as a merge-grant subject"
        );
    }

    let cmd = attended_grant_command("o/r", 42, "abc");
    assert!(cmd.starts_with("fno backlog decide '"), "{cmd}");
    assert!(cmd.contains("'merge-grant:o/r#42@abc'"), "{cmd}");
    assert!(cmd.contains("'merge authorized for this head'"), "{cmd}");
    assert!(cmd.ends_with("--authority operator"), "{cmd}");

    let payload = decisions_payload(vec![law_row("operator", Some(MERGE_GRANT_DECISION))]);
    assert_eq!(head_grant_status(Some(&payload)), HeadGrant::Granted);

    let payload = decisions_payload(vec![
        law_row("operator", Some(MERGE_GRANT_DECISION)),
        law_row("operator", Some(MERGE_GRANT_DECISION)),
    ]);
    assert_eq!(head_grant_status(Some(&payload)), HeadGrant::Granted);

    let payload = decisions_payload(vec![]);
    assert_eq!(head_grant_status(Some(&payload)), HeadGrant::Absent);

    // AC2-ERR: only a person at a terminal carries the grant. Rows recorded
    // through the law door by a harness session (`chat_attested`) or without
    // an authority read as no operator row at all: Absent, never granted.
    let payload = decisions_payload(vec![
        law_row("chat_attested", Some(MERGE_GRANT_DECISION)),
        law_row("unknown", Some(MERGE_GRANT_DECISION)),
    ]);
    assert_eq!(head_grant_status(Some(&payload)), HeadGrant::Absent);

    let payload = decisions_payload(vec![
        law_row("operator", Some(MERGE_GRANT_DECISION)),
        law_row("operator", Some("not this head")),
    ]);
    assert_eq!(head_grant_status(Some(&payload)), HeadGrant::Conflict);

    let payload = decisions_payload(vec![law_row("operator", None)]);
    assert_eq!(head_grant_status(Some(&payload)), HeadGrant::Conflict);

    // AC2-ERR fail-closed polarity: a dead probe is not "no grant", and a
    // payload without the decisions array is a shape the CLI never emits.
    assert_eq!(
        head_grant_status(None),
        HeadGrant::Unreadable("the decisions read did not answer".to_string())
    );
    let payload = br#"{"error": "damaged index"}"#.to_vec();
    assert_eq!(
        head_grant_status(Some(&payload)),
        HeadGrant::Unreadable("malformed decisions payload".to_string())
    );
}
