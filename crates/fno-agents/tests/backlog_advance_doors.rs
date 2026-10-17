//! Runtime contracts migrated from the retired Python dispatch owners.
use fno_agents::backlog::advance_join::{plan_graph, width, write_policies};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct World {
    dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
}
impl World {
    fn new(rows: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.toml");
        std::fs::write(&config,format!("state_dir = {:?}\n[agents]\nmax_live = 4\n[agents.defaults]\nprovider = \"claude\"\nmodel = \"glm-5.3-flash[1m]\"\n",root.to_string_lossy())).unwrap();
        fno_agents::graph_store::seed_rows(&root.join("graph.json"), rows.as_array().unwrap())
            .unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        Self { dir, root, config }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
        c.current_dir(&self.root)
            .env("FNO_CONFIG", &self.config)
            .env("FNO_STATE_DIR", &self.root)
            .env("FNO_TRACKER_BACKEND", "graph")
            .env("FNO_CONFIG_SEARCH_ROOT", self.dir.path())
            .envs(fno_agents::test_run::self_owner_env())
            .env("FNO_EVENTS_PATH", self.root.join("events.jsonl"))
            .env("FNO_AGENTS_HOME", self.dir.path().join("agents"))
            .env("FNO_CLAIMS_ROOT", self.dir.path().join("claims"))
            .env("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
            .env("FNO_NO_CANONICAL_CONFIG", "1")
            .env("FNO_SPAWN_GATE", "0")
            .env("FNO_AUTO_CONTINUE", "0")
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("claude"));
        for key in [
            "FNO_AGENT_SELF",
            "FNO_HARNESS_NAME",
            "FNO_HARNESS_SESSION_ID",
            "FNO_SESSION_ID",
            "CODEX_THREAD_ID",
            "CLAUDECODE_SESSION_ID",
            "CLAUDE_SESSION_ID",
            "FNO_BG",
            "TARGET_BRIEF",
        ] {
            c.env_remove(key);
        }
        c
    }
    fn run(&self, args: &[&str], request: Option<&Value>) -> Output {
        let mut c = self.command();
        c.args(args);
        self.output(c, request)
    }
    fn output(&self, mut c: Command, request: Option<&Value>) -> Output {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(request) = request {
            let mut child = c.stdin(Stdio::piped()).spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(request.to_string().as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        } else {
            c.output().unwrap()
        }
    }
    fn claim(&self, id: &str, worktree: &Path) {
        let key = format!("node:{id}");
        let metadata = json!({"worktree":worktree}).to_string();
        let out = self.run(
            &[
                "claim",
                "acquire",
                &key,
                "--holder",
                "fixture-holder",
                "--pid",
                &std::process::id().to_string(),
                "--ttl-ms",
                "180000",
                "--metadata",
                &metadata,
            ],
            None,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn plan(&self, text: &str) -> PathBuf {
        let p = self.dir.path().join("plan.md");
        std::fs::write(&p, text).unwrap();
        p
    }
    #[cfg(unix)]
    fn endpoint(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = self.dir.path().join("fno-endpoint");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
    fn events(&self) -> Vec<Value> {
        let roots = [self.root.clone(), self.dir.path().join("agents")];
        let mut out = Vec::new();
        for root in roots {
            let journal = root.join("events.jsonl");
            if fno_agents::event_store::store_path(&journal).is_file() {
                for row in
                    fno_agents::event_store::query_events(&journal, &Default::default()).unwrap()
                {
                    out.push(serde_json::from_str::<Value>(&row.line).unwrap());
                }
            }
        }
        out
    }
}
fn plan(tasks: &str, waves: &str) -> String {
    format!("---\ndifficulty: high\nstatus: ready\n---\n## Execution Strategy\n```yaml\ntasks:\n{tasks}\nwaves:\n{waves}\n```\n")
}
fn row(id: &str, extra: Value) -> Value {
    let mut r = json!({"id":id,"title":id,"slug":id,"type":"task","project":"fixture","priority":"p2","status":"ready","dispatch_verb":"/target","difficulty":"high","created_at":chrono::Utc::now().to_rfc3339()});
    r.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    r
}
fn decode(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "status={} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn join_width_tracks_dependency_frontiers_and_collision_serialization() {
    let world = World::new(json!([]));
    for (surfaces, mode, expected) in [
        ("a.rs", "parallel", 2),
        ("a.rs", "sequential", 2),
        ("shared.rs", "parallel", 1),
        ("shared.rs", "sequential", 1),
    ] {
        let first = if surfaces == "shared.rs" {
            surfaces
        } else {
            "b.rs"
        };
        let text = plan(
            &format!("- id: a\n  surface: [{first}]\n- id: b\n  surface: [{surfaces}]"),
            &format!("- wave: 1\n  mode: {mode}\n  tasks: [a, b]"),
        );
        let p = world.plan(&text);
        let out = world.run(&["backlog", "join", "width", p.to_str().unwrap()], None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected.to_string()
        );
    }
    for edges in ["- id: a\n  surface: [a.rs]\n  blocked_by: [missing]","- id: a\n  surface: [a.rs]\n  blocked_by: [b]\n- id: b\n  surface: [b.rs]\n  blocked_by: [a]"] {
        let wave=if edges.contains("- id: b") {"- wave: 1\n  tasks: [a, b]"} else {"- wave: 1\n  tasks: [a]"};let p=world.plan(&plan(edges,wave));let out=world.run(&["backlog","join","width",p.to_str().unwrap()],None);assert!(!out.status.success());assert!(out.stdout.is_empty());
    }
    let p=world.plan(&plan("- id: a\n  surface: [a.rs]\n- id: b\n  surface: [b.rs]\n  blocked_by: []\n- id: c\n  surface: [c.rs]","- wave: 1\n  tasks: [a]\n- wave: 2\n  tasks: [b, c]"));
    assert_eq!(width(&plan_graph(&p).unwrap()).unwrap(), 2);
}
#[test]
fn join_write_policy_never_narrows_overlapping_or_unevaluated_bands() {
    let w = World::new(json!([]));
    for (right, expected) in [
        ("[b.rs]", "enforced"),
        ("[a.rs]", "overlapping"),
        ("[]", "unevaluated"),
    ] {
        let p=w.plan(&plan(&format!("- id: a\n  surface: [a.rs]\n- id: b\n  surface: {right}"),"- wave: 1\n  difficulty: high\n  tasks: [a]\n- wave: 2\n  difficulty: low\n  tasks: [b]"));
        let graph = plan_graph(&p).unwrap();
        let policy = write_policies(&graph, &["high".into(), "low".into()]);
        assert_eq!(policy["low"]["verdict"], expected);
        if expected == "enforced" {
            assert_eq!(policy["low"]["deny_edit"], json!(["a.rs"]));
            assert!(policy["low"]["allow_write"]
                .as_array()
                .unwrap()
                .contains(&json!(".git/")));
        } else {
            assert!(policy["low"]["allow_write"].is_null());
            assert!(policy["low"]["deny_edit"].is_null());
        }
    }
}
#[test]
fn public_advance_preserves_disabled_receipt_and_validates_flag_combinations() {
    let w = World::new(json!([]));
    let out = w.run(&["backlog", "advance"], None);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "skipped reason=disabled\n"
    );
    let events = w.events();
    assert_eq!(
        events
            .iter()
            .filter(|r| r["type"] == "advance_skipped" || r["kind"] == "advance_skipped")
            .count(),
        1
    );
    for args in [
        vec!["--epic", "x-aaaa", "--closed", "x-bbbb"],
        vec!["--stop"],
        vec!["--continuation"],
        vec!["--loose"],
        vec!["--source", "bogus"],
        vec!["--model", ""],
    ] {
        let mut argv = vec!["backlog", "advance"];
        argv.extend(args);
        assert_eq!(w.run(&argv, None).status.code(), Some(2));
    }
    let out = w.run(&["backlog", "advance", "--explain"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("SELECTION  0 candidates -> 0 eligible"));
    assert!(text.contains("GATES\n"));
    assert!(text.contains("(no chain: nothing to route)"));
    assert!(text.contains("would dispatch: nothing (no eligible node)"));
    assert_eq!(
        w.events().len(),
        events.len(),
        "explain must not emit decisions or take claims"
    );
}
#[test]
fn native_armed_probe_keeps_master_env_and_config_precedence() {
    let w = World::new(json!([]));
    for (master, configured, env, armed, rank) in [
        (false, true, "1", false, "autonomy"),
        (true, false, "yes", true, "env"),
        (true, true, "0", false, "env"),
    ] {
        std::fs::write(&w.config,format!("state_dir = {:?}\n[autonomy]\nenabled = {master}\n[auto_continue]\nenabled = {configured}\n",w.root.to_string_lossy())).unwrap();
        let mut c = w.command();
        c.args(["backlog", "advance", "--stdin"])
            .env("FNO_AUTO_CONTINUE", env);
        let answer = decode(&w.output(c, Some(&json!({"op":"armed"}))));
        assert_eq!(answer["armed"], armed);
        assert_eq!(answer["rank"], rank);
    }
}
#[test]
fn join_refusal_codes_have_one_outcome_event_and_do_not_launch() {
    let w = World::new(json!([row("x-aaaa", json!({}))]));
    let out = w.run(&["backlog", "join", "x-aaaa"], None);
    assert_eq!(out.status.code(), Some(4));
    let path = w.plan(&plan(
        "- id: a\n  surface: [a.rs]",
        "- wave: 1\n  tasks: [a]",
    ));
    let w = World::new(json!([row("x-aaaa", json!({"plan_path":path}))]));
    assert_eq!(
        w.run(&["backlog", "join", "x-aaaa"], None).status.code(),
        Some(2)
    );
    let pid = std::process::id().to_string();
    let acquired = w.run(
        &[
            "claim",
            "acquire",
            "node:x-aaaa",
            "--holder",
            "fixture-holder",
            "--pid",
            &pid,
            "--ttl-ms",
            "180000",
        ],
        None,
    );
    assert!(acquired.status.success());
    let missing = w.run(&["backlog", "join", "x-aaaa"], None);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("names no holder worktree"));
    let released = w.run(
        &[
            "claim",
            "release",
            "node:x-aaaa",
            "--holder",
            "fixture-holder",
        ],
        None,
    );
    assert!(released.status.success());
    w.claim("x-aaaa", &w.root);
    let out = w.run(&["backlog", "join", "x-aaaa"], None);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = w.events();
    let refusals = events
        .iter()
        .filter(|r| r["type"] == "join_refused" || r["kind"] == "join_refused")
        .collect::<Vec<_>>();
    assert_eq!(refusals.len(), 3);
    assert!(events
        .iter()
        .all(|r| r["type"] != "join_dispatched" && r["kind"] != "join_dispatched"));
}
#[cfg(unix)]
#[test]
fn join_persists_brief_and_enforced_policies_before_each_launch() {
    let seed = World::new(json!([]));
    let p=seed.plan(&plan("- id: a\n  surface: [a.rs]\n- id: b\n  surface: [b.rs]\n- id: c\n  surface: [c.rs]\n  blocked_by: []","- wave: 1\n  difficulty: high\n  tasks: [a, b]\n- wave: 2\n  difficulty: low\n  tasks: [c]"));
    for sandbox in [false, true] {
        let w = World::new(json!([row("x-aaaa", json!({"plan_path":p}))]));
        let prior = std::fs::read_to_string(&w.config).unwrap();
        std::fs::write(&w.config, format!("{prior}\n[join]\nsandbox = {sandbox}\n")).unwrap();
        w.claim("x-aaaa", &w.root);
        let endpoint=w.endpoint("printf '%s\\n' \"$*\" >> \"$LAUNCH_LOG\"\npolicy=\nfor arg in \"$@\"; do\n  if [ \"$policy\" = next ]; then [ -s \"$arg\" ] || exit 9; policy=seen; fi\n  if [ \"$arg\" = --sandbox-write-policy ]; then policy=next; fi\ndone\n[ -s \"$JOIN_BRIEF\" ] || exit 10\nprintf '%s\\n' '{\"short_id\":\"abcd1234\"}'");
        let log = w.dir.path().join("launches");
        let mut c = w.command();
        c.args([
            "backlog",
            "join",
            "x-aaaa",
            "--workers",
            "4",
            "--model",
            "glm-5.3-flash[1m]",
        ])
        .env("FNO_BIN", endpoint)
        .env("LAUNCH_LOG", &log)
        .env("JOIN_BRIEF", w.root.join(".fno/join-briefs/x-aaaa.md"));
        let receipt = decode(&w.output(c, None));
        assert_eq!(receipt["width"], 3);
        assert_eq!(receipt["spawned"].as_array().unwrap().len(), 2);
        let launches = std::fs::read_to_string(log).unwrap();
        assert_eq!(launches.lines().count(), 2);
        assert_eq!(
            launches.matches("--sandbox-write-policy").count(),
            if sandbox { 2 } else { 0 }
        );
        assert!(launches.contains("--model glm-5.3-flash[1m]"));
        let brief = std::fs::read_to_string(w.root.join(".fno/join-briefs/x-aaaa.md")).unwrap();
        assert!(brief.contains(receipt["lead"].as_str().unwrap()));
        assert!(brief.contains("claim ONE ready task"));
        assert_eq!(
            w.events()
                .iter()
                .filter(|r| r["type"] == "join_dispatched" || r["kind"] == "join_dispatched")
                .count(),
            1
        );
    }
}
#[test]
fn epic_continuation_does_not_reactivate_and_stop_clears_persisted_flag() {
    let w = World::new(json!([
        row("x-aaaa", json!({"type":"epic","mission_active":false})),
        row("x-bbbb", json!({"parent":"x-aaaa"}))
    ]));
    let mut c = w.command();
    c.args([
        "backlog",
        "advance",
        "--epic",
        "x-aaaa",
        "--continuation",
        "--json",
    ])
    .env("FNO_AUTO_CONTINUE", "1");
    let receipt = decode(&w.output(c, None));
    assert_eq!(receipt["deactivated"], true);
    assert_eq!(receipt["dispatched"], json!([]));
    let status = decode(&w.run(&["backlog", "get", "x-aaaa"], None));
    assert_ne!(status["mission_active"], true);
    let out = w.run(
        &["backlog", "advance", "--stdin"],
        Some(&json!({"op":"mission","node_id":"x-aaaa","active":true})),
    );
    assert_eq!(decode(&out)["changed"], true);
    let out = w.run(
        &["backlog", "advance", "--epic", "x-aaaa", "--stop", "--json"],
        None,
    );
    assert_eq!(decode(&out)["deactivated"], true);
    let status = decode(&w.run(&["backlog", "get", "x-aaaa"], None));
    assert!(status
        .get("mission_active")
        .is_none_or(|v| v != &json!(true)));
}
#[cfg(unix)]
#[test]
fn launch_requires_identity_and_preserves_seam_notes_without_a_python_owner() {
    let w = World::new(json!([]));
    let p = w.plan("---\nstatus: ready\ndifficulty: high\n---\n");
    let node = row(
        "x-aaaa",
        json!({"plan_path":p,"model":"glm-5.3-flash[1m]","cwd":w.root}),
    );
    for (body,success) in [("printf '%s\\n' noise",false),("printf '%s\\n' '{\"short_id\":\"abcd1234\"}'\nprintf '%s\\n' 'fno agents spawn: effort suppressed' >&2",true),("printf '%s\\n' 'spawn-gate: RAM refused' >&2\nexit 77",false)] {
        let endpoint=w.endpoint(body);let mut c=w.command();c.args(["backlog","advance","--stdin"]).env("FNO_BIN",endpoint);let out=w.output(c,Some(&json!({"op":"spawn","node_id":"x-aaaa","node":node,"cwd":w.root,"harness":"claude","model":"glm-5.3-flash[1m]"})));assert_eq!(out.status.success(),success,"{}",String::from_utf8_lossy(&out.stderr));if success{let value=decode(&out);assert_eq!(value["short_id"],"abcd1234");assert_eq!(value["receipt"]["notes"],json!(["fno agents spawn: effort suppressed"]));}else{assert!(!out.stderr.is_empty());}
    }
}

#[cfg(unix)]
#[test]
fn isolated_lanes_use_the_nodes_repo_and_return_failed_slots() {
    let seed = World::new(json!([]));
    let p = seed.plan("---\nstatus: ready\ndifficulty: high\n---\n");
    let w = World::new(json!([row(
        "x-aaaa",
        json!({"plan_path":p,"cwd":seed.root,"model":"glm-5.3-flash[1m]"})
    )]));
    let script_dir = seed.root.join("scripts/setup");
    std::fs::create_dir_all(&script_dir).unwrap();
    std::fs::write(script_dir.join("setup-worktree.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let lane = w.dir.path().join("lane");
    std::fs::create_dir_all(&lane).unwrap();
    let endpoint=w.endpoint("case \"$*\" in\n  'agents workspace worktree ensure '*) printf '%s\\n' \"$LANE_ROOT\"; printf '%s\\n' \"$*\" >> \"$LAUNCH_LOG\" ;;\n  'agents spawn '*) printf '%s\\n' \"$*\" >> \"$LAUNCH_LOG\"; [ -s \"$LANE_ROOT/.fno/config.local.toml\" ] || exit 9; printf '%s\\n' 'launch transport failed' >&2; exit 1 ;;\n  *) printf '%s\\n' '{}' ;;\nesac");
    let log = w.dir.path().join("launches");
    let mut c = w.command();
    c.args([
        "backlog",
        "dispatch-lanes",
        "--max",
        "1",
        "--project",
        "fixture",
        "--harness",
        "claude",
        "--model",
        "glm-5.3-flash[1m]",
    ])
    .env("FNO_BIN", &endpoint)
    .env("LANE_ROOT", &lane)
    .env("LAUNCH_LOG", &log);
    let out = w.output(c, None);
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(value["fill"]["selected"], 1, "{value}");
    assert_eq!(value["fill"]["skipped"], 1);
    assert_eq!(value["fill"]["dispatched"], 0);
    assert_eq!(out.status.code(), Some(1));
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(calls.contains(&format!("--repo {}", seed.root.display())));
    assert!(calls.contains(&format!("--cwd {}", lane.display())));
    let state = decode(&w.run(&["claim", "status", "dispatch:x-aaaa"], None));
    assert_eq!(state["state"], "free");
    let count = decode(&w.run(&["claim", "lane-count", "--json"], None));
    assert_eq!(count["active_lanes"], 0);
}

#[test]
fn closed_advance_is_single_flight_and_epic_scopes_are_independent() {
    let w = World::new(json!([]));
    let pid = std::process::id().to_string();
    let out = w.run(
        &[
            "claim",
            "flight-acquire",
            "flight:backlog-advance",
            "--holder",
            "fixture",
            "--scope",
            "advance",
            "--pid",
            &pid,
            "--ttl-ms",
            "180000",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = decode(&w.run(
        &["backlog", "advance", "--closed", "x-aaaa", "--json"],
        None,
    ));
    assert_eq!(value["decision"], "held");
    assert!(value["held"] == true);
    assert!(w.events().iter().all(|e| e["type"] != "advance_dispatched"));
    let out = w.run(&["backlog", "advance", "--epic", "x-aaaa", "--json"], None);
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"], "no-such-node");
}

#[cfg(unix)]
#[test]
fn pending_contract_reconcile_keeps_its_sentinel_until_a_worker_owns_the_pass() {
    let w = World::new(json!([row(
        "x-aaaa",
        json!({"dep":"contract","model":"glm-5.3-flash[1m]"})
    )]));
    let pid = std::process::id().to_string();
    let out = w.run(
        &[
            "claim",
            "acquire",
            "reconcile:x-aaaa",
            "--holder",
            "reconcile-pending:x-aaaa",
            "--pid",
            &pid,
            "--ttl-ms",
            "21600000",
        ],
        None,
    );
    assert!(out.status.success());
    std::fs::create_dir_all(w.root.join(".fno")).unwrap();
    std::fs::write(
        w.root.join(".fno/stub-manifest-x-aaaa.json"),
        "{\"node\":\"x-aaaa\",\"reconciled\":false,\"stubs\":[]}",
    )
    .unwrap();
    let endpoint=w.endpoint("case \"$*\" in\n  'agents spawn '*) printf '%s\\n' \"$*\" >> \"$LAUNCH_LOG\"; printf '%s\\n' '{\"short_id\":\"abcd1234\"}' ;;\n  *) printf '%s\\n' '{}' ;;\nesac");
    let log = w.dir.path().join("launches");
    let mut c = w.command();
    c.args(["backlog", "advance", "--stdin"])
        .env("FNO_BIN", endpoint)
        .env("LAUNCH_LOG", &log);
    let receipt = decode(&w.output(
        c,
        Some(&json!({"op":"fire-pending-now","node_id":"x-aaaa","root":w.root})),
    ));
    assert_eq!(receipt["decision"], "dispatched", "{receipt}");
    assert!(std::fs::read_to_string(log)
        .unwrap()
        .contains("--reconcile"));
    let status = decode(&w.run(&["claim", "status", "reconcile:x-aaaa"], None));
    assert_eq!(status["state"], "free");
}

#[cfg(unix)]
#[test]
fn advance_reads_real_selection_and_preserves_launch_pins() {
    let seed = World::new(json!([]));
    let p = seed.plan("---\nstatus: ready\ndifficulty: high\n---\n");
    let w = World::new(json!([row(
        "x-aaaa",
        json!({"plan_path":p,"cwd":seed.root,"model":"glm-5.3-flash[1m]","provider":"claude","effort":"high"})
    )]));
    let endpoint=w.endpoint("case \"$*\" in\n  'agents spawn '*) printf '%s\\n' \"$*\" >> \"$LAUNCH_LOG\"; printf '%s\\n' '{\"short_id\":\"abcd1234\"}' ;;\n  'agents truth '*) printf '%s\\n' '{}' ;;\n  *) exec \"$NATIVE_DOOR\" \"$@\" ;;\nesac");
    let log = w.dir.path().join("launches");
    let invoke = || {
        let mut c = w.command();
        c.args([
            "backlog",
            "advance",
            "--project",
            "fixture",
            "--model",
            "glm-5.3-flash[1m]",
            "--provider",
            "claude",
            "--json",
        ])
        .env("FNO_AUTO_CONTINUE", "1")
        .env("FNO_BIN", &endpoint)
        .env("NATIVE_DOOR", env!("CARGO_BIN_EXE_fno-agents"))
        .env("LAUNCH_LOG", &log);
        w.output(c, None)
    };
    let receipt = decode(&invoke());
    assert_eq!(receipt["decision"], "dispatched", "{receipt}");
    assert_eq!(receipt["node_id"], "x-aaaa");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.contains("--effort high"), "{calls}");
    assert!(calls.contains("--model glm-5.3-flash[1m]"));
    w.claim("x-aaaa", &seed.root);
    let _ = decode(&invoke());
    assert_eq!(
        std::fs::read_to_string(log).unwrap().lines().count(),
        1,
        "a live node must not launch twice"
    );
}

#[cfg(unix)]
#[test]
fn closed_project_drives_dependents_even_when_next_selection_has_another_scope() {
    let seed = World::new(json!([]));
    let p = seed.plan("---\nstatus: ready\ndifficulty: high\n---\n");
    let w = World::new(json!([
        row(
            "x-aaaa",
            json!({"completed_at":chrono::Utc::now().to_rfc3339()})
        ),
        row(
            "x-bbbb",
            json!({"blocked_by":["x-aaaa"],"plan_path":p,"cwd":seed.root,"model":"glm-5.3-flash[1m]","provider":"claude"})
        )
    ]));
    let endpoint=w.endpoint("case \"$*\" in\n  'agents spawn '*) printf '%s\\n' \"$*\" >> \"$LAUNCH_LOG\"; printf '%s\\n' '{\"short_id\":\"abcd1234\"}' ;;\n  'agents truth '*) printf '%s\\n' '{}' ;;\n  *) exec \"$NATIVE_DOOR\" \"$@\" ;;\nesac");
    let log = w.dir.path().join("launches");
    let mut c = w.command();
    c.args([
        "backlog",
        "advance",
        "--closed",
        "x-aaaa",
        "--project",
        "other",
        "--model",
        "glm-5.3-flash[1m]",
        "--provider",
        "claude",
        "--json",
    ])
    .env("FNO_AUTO_CONTINUE", "1")
    .env("FNO_BIN", endpoint)
    .env("NATIVE_DOOR", env!("CARGO_BIN_EXE_fno-agents"))
    .env("LAUNCH_LOG", &log);
    let _ = decode(&w.output(c, None));
    let calls = std::fs::read_to_string(log).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.contains("--node x-bbbb"));
    assert!(w
        .events()
        .iter()
        .any(|r| r["type"] == "advance_dispatched" && r["data"]["node_id"] == "x-bbbb"));
}

#[test]
fn an_auto_continue_tick_commits_to_the_store_and_creates_no_root_file() {
    let w = World::new(json!([]));
    let out = w.run(&["backlog", "advance"], None);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "skipped reason=disabled\n"
    );
    let ticks: Vec<&Value> = w
        .events()
        .iter()
        .filter(|r| r["type"] == "control_plane_tick")
        .collect();
    assert_eq!(ticks.len(), 1, "ticks: {ticks:?}");
    assert_eq!(ticks[0]["data"]["arm"], "auto_continue");
    assert!(
        !w.root.join("events.jsonl").exists(),
        "the tick must not create the root journal file"
    );
}

#[test]
fn a_live_claim_with_a_recent_loop_check_reads_working() {
    let w = World::new(json!([row("x-aaaa", json!({}))]));
    let pid = std::process::id().to_string();
    let acquired = w.run(
        &[
            "claim",
            "acquire",
            "node:x-aaaa",
            "--holder",
            "target-session:tok-1",
            "--pid",
            &pid,
            "--ttl-ms",
            "180000",
        ],
        None,
    );
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    let claim_op = || {
        decode(&w.run(
            &["backlog", "advance", "--stdin"],
            Some(&json!({"op":"claim","node_id":"x-aaaa","emit_events":false})),
        ))
    };
    // With no loop_check row the live claim reads waiting.
    let none = claim_op();
    assert_eq!(none["verdict"], "ours");
    assert_eq!(none["truth_status"], "waiting");
    // One committed loop_check row for the session flips it to working.
    let envelope = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "type": "loop_check",
        "source": "agents",
        "data": {"session_id": "tok-1"},
    });
    fno_agents::event_store::append_envelope(
        &w.root.join("events.jsonl"),
        &envelope.to_string(),
        None,
    )
    .expect("the store beside the journal opened");
    let working = claim_op();
    assert_eq!(working["truth_status"], "working");
}
