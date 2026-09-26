//! Replay the committed backlog write goldens (`backlog_update`,
//! `backlog_rank`). Each golden line was captured from the surface that owned
//! the verb before its port (Python for update at the port's base sha; the
//! native binary for rank) and carries argv, exit code, stdout, stderr and
//! the post-case row projection. The replay seeds a fresh store, runs the
//! built binary and byte-compares the masked text.
//!
//! Masking contract (shared with `capture_update_goldens.py`, which wrote the
//! goldens - see each golden dir's capture.json): the fixture dir reads
//! `<FIXTURE>`, any other absolute `/Users`|`/home` path reads `<PATH>`, and
//! ISO timestamps read `<TS>`.

use fno_agents::graph_store::seed_rows;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tempfile::TempDir;

const SESSION_ID: &str = "golden-fixed-session-0001";
const GIT_ORIGIN: &str = "https://github.com/capture-owner/capture-repo.git";

struct Case {
    name: String,
    group: String,
    argv: Vec<String>,
    flavor: String,
    settings: Option<String>,
    read_nodes: Vec<String>,
    code: i32,
    stdout: String,
    stderr: String,
    rows: BTreeMap<String, Value>,
    ranks: BTreeMap<String, String>,
}

fn load_cases(rel: &str) -> Vec<Case> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing golden {path:?}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("unparseable golden line in {rel}: {e}"));
            Case {
                name: v["name"].as_str().expect("name").into(),
                group: v["group"].as_str().expect("group").into(),
                argv: v["argv"]
                    .as_array()
                    .expect("argv")
                    .iter()
                    .map(|a| a.as_str().expect("argv str").into())
                    .collect(),
                flavor: v["flavor"].as_str().expect("flavor").into(),
                settings: v["settings"].as_str().map(str::to_string),
                read_nodes: v["read_nodes"]
                    .as_array()
                    .expect("read_nodes")
                    .iter()
                    .map(|a| a.as_str().expect("node str").into())
                    .collect(),
                code: v["code"].as_i64().expect("code") as i32,
                stdout: v["stdout"].as_str().expect("stdout").into(),
                stderr: v["stderr"].as_str().expect("stderr").into(),
                rows: v["rows"]
                    .as_object()
                    .map(|m| m.iter().map(|(k, val)| (k.clone(), val.clone())).collect())
                    .unwrap_or_default(),
                ranks: v["ranks"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .map(|(k, val)| (k.clone(), val.as_str().expect("rank str").into()))
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// The seed both the capture harness and this replay commit to. Keep
/// byte-equal with SEED in capture_update_goldens.py: the goldens only hold
/// what the case did to these rows.
fn seed_entries() -> Vec<Value> {
    let created = "2026-09-01T00:00:00+00:00";
    vec![
        json!({"id": "x-aaaa1111", "title": "Alpha", "status": "ready", "priority": "p1",
               "type": "feature", "domain": "code", "project": "fno", "created_at": created}),
        json!({"id": "x-bbbb2222", "slug": "beta", "title": "Beta", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "rank": 10.0,
               "related": ["x-cccc3333"], "created_at": created}),
        json!({"id": "x-cccc3333", "title": "Gamma", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "other", "created_at": created}),
        json!({"id": "x-dddd4444", "title": "Delta", "status": "done", "priority": "p3",
               "type": "feature", "domain": "code", "project": "fno",
               "completed_at": "2026-09-05T00:00:00+00:00", "created_at": created,
               "blocked_by": ["x-aaaa1111"]}),
        json!({"id": "x-eeee5555", "title": "Epic", "status": "ready", "priority": "p1",
               "type": "epic", "domain": "code", "project": "fno", "created_at": created,
               "pr_number": 42, "pr_url": "https://github.com/own/er/pull/42"}),
        json!({"id": "x-ffff6666", "title": "Child one", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "parent": "x-eeee5555",
               "rank": 5.0, "created_at": created}),
        json!({"id": "x-77778888", "title": "Child two", "status": "ready", "priority": "p3",
               "type": "feature", "domain": "code", "project": "fno", "parent": "x-eeee5555",
               "created_at": created}),
        json!({"id": "x-9999aaaa", "title": "Nested epic", "status": "ready", "priority": "p2",
               "type": "epic", "domain": "code", "project": "fno", "parent": "x-eeee5555",
               "created_at": created}),
        json!({"id": "x-dead0002", "title": "Sibling epic", "status": "ready", "priority": "p2",
               "type": "epic", "domain": "code", "project": "fno", "parent": "x-eeee5555",
               "created_at": created}),
        json!({"id": "x-abcd1234", "title": "Leaf", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "parent": "x-9999aaaa",
               "created_at": created}),
        json!({"id": "ab-12345678", "title": "Legacy id node", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "created_at": created}),
        json!({"id": "x-deadbeef", "title": "Archived one", "status": "done", "priority": "p3",
               "type": "feature", "domain": "code", "project": "fno", "created_at": created,
               "archived_at": "2026-09-10T00:00:00+00:00"}),
        json!({"id": "x-cafe2222", "title": "Contained one", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "contained_in": "x-eeee5555",
               "parent": "x-eeee5555", "pr_number": 42,
               "pr_url": "https://github.com/own/er/pull/42", "created_at": created}),
        json!({"id": "x-0ddba11", "title": "Zero", "status": "ready", "priority": "p0",
               "type": "feature", "domain": "code", "project": "fno", "created_at": created}),
        json!({"id": "x-beef0001", "title": "Banded", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "difficulty": "medium",
               "difficulty_history": [{"value": "medium", "source": "claim",
                                       "ts": "2026-09-10T00:00:00+00:00"}],
               "model_tier": "crown", "created_at": created}),
        json!({"id": "x-cafe1111", "title": "Plan bound", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno",
               "plan_path": "plans/owned/00-INDEX.md", "created_at": created}),
        json!({"id": "x-feed0002", "title": "Tagged", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "tags": ["existing-tag"],
               "created_at": created}),
        json!({"id": "x-bade9999", "title": "Noted", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno",
               "completion_note": "first note", "created_at": created}),
        json!({"id": "x-f00d0003", "slug": "causal", "title": "Causal", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno", "created_at": created}),
        json!({"id": "x-c0de0004", "title": "Recorded", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno",
               "pr_url": "https://github.com/other/repo/pull/7", "pr_number": 7, "fixes_pr": 5,
               "created_at": created}),
        json!({"id": "x-d00d0005", "title": "Addpr", "status": "ready", "priority": "p2",
               "type": "feature", "domain": "code", "project": "fno",
               "additional_prs": [{"number": 11, "url": "https://github.com/own/er/pull/11",
                                   "note": "first"}],
               "created_at": created}),
    ]
}

const PLAN_OWNED: &str = "---\nclaims: [x-cafe1111]\nsize: M\nstatus: ready\n---\n\n# Owned plan\n";
const PLAN_ALPHA: &str = "---\nsize: S\nstatus: ready\n---\n\n# Alpha plan\n";

struct Fixture {
    _dir: TempDir,
    fixture: PathBuf,
    env: Vec<(String, String)>,
    argv: Vec<String>,
}

fn materialize(case: &Case) -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let fixture = dir.path().join("fx");
    for sub in ["plans/owned", "plans/alpha", "proj"] {
        std::fs::create_dir_all(fixture.join(sub)).expect("fixture dirs");
    }
    std::fs::write(
        fixture.join("config.toml"),
        format!("state_dir = \"{}\"\n", fixture.display()),
    )
    .expect("config.toml");
    std::fs::write(fixture.join("plans/owned/00-INDEX.md"), PLAN_OWNED).expect("owned plan");
    std::fs::write(fixture.join("plans/alpha/00-INDEX.md"), PLAN_ALPHA).expect("alpha plan");
    std::fs::write(fixture.join("details.md"), "body from file\n").expect("details.md");
    std::fs::write(
        fixture.join(".gitconfig"),
        "[user]\n\tname = Capture\n\temail = capture@example.com\n[init]\n\tdefaultBranch = main\n",
    )
    .expect(".gitconfig");
    if case.flavor == "git" {
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .env("PATH", "/usr/bin:/bin")
                .status()
                .expect("git available on PATH")
                .success()
        };
        assert!(
            git(&["init", "-q", fixture.to_str().expect("fixture utf8")]),
            "git init"
        );
        assert!(
            git(&[
                "-C",
                fixture.to_str().expect("fixture utf8"),
                "remote",
                "add",
                "origin",
                GIT_ORIGIN
            ]),
            "git remote"
        );
    }
    if case.settings.as_deref() == Some("workmap") {
        std::fs::write(
            dir.path().join("settings.yaml"),
            format!(
                "work:\n  workspaces:\n    ws:\n      projects:\n        - name: myproj\n          path: {}/proj\n",
                fixture.display()
            ),
        )
        .expect("settings.yaml");
    }
    seed_rows(&fixture.join("graph.json"), &seed_entries()).expect("seed");
    let fixture_str = fixture.to_string_lossy().to_string();
    let mut env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), fixture_str.clone()),
        (
            "FNO_CONFIG".into(),
            fixture.join("config.toml").to_string_lossy().to_string(),
        ),
        ("FNO_TRACKER_BACKEND".into(), "graph".into()),
        ("FNO_GLOBAL_SETTINGS_PATH".into(), "/dev/null".into()),
        (
            "FNO_AGENTS_BIN".into(),
            env!("CARGO_BIN_EXE_fno-agents").to_string(),
        ),
        ("FNO_SKIP_MIGRATION".into(), "1".into()),
    ];
    // The canonical stamp only rides the rank cases: their operator fence
    // needs a resolvable identity. Update cases stay identity-free so the
    // best-effort ship stamp prints its deterministic skip warning (see the
    // golden dirs' capture.json).
    if case.group == "rank" {
        env.push(("FNO_HARNESS_NAME".into(), "claude".into()));
        env.push(("FNO_HARNESS_SESSION_ID".into(), SESSION_ID.into()));
    }
    if case.settings.as_deref() == Some("workmap") {
        env.push((
            "FNO_GLOBAL_SETTINGS_PATH".into(),
            dir.path()
                .join("settings.yaml")
                .to_string_lossy()
                .to_string(),
        ));
    }
    // The port era replays forwarded shapes through the wheel's fno-py; after
    // the cut-over (the forward deleted) nothing execs it and the golden run
    // stays Python-free.
    let fno_py = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/.venv/bin/fno-py");
    if fno_py.exists() {
        env.push(("FNO_PY".into(), fno_py.to_string_lossy().to_string()));
    }
    let argv: Vec<String> = case
        .argv
        .iter()
        .map(|a| a.replace("<FIXTURE>", &fixture_str))
        .collect();
    Fixture {
        _dir: dir,
        fixture,
        env,
        argv,
    }
}

fn path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?:/Users|/home)/[^\s'")\\,]*"#).expect("path regex"))
}

fn ts_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})")
            .expect("ts regex")
    })
}

fn norm_text(s: &str, fixture: &Path) -> String {
    let s = s.replace(fixture.to_string_lossy().as_ref(), "<FIXTURE>");
    let s = path_re().replace_all(&s, "<PATH>").into_owned();
    ts_re().replace_all(&s, "<TS>").into_owned()
}

/// Mirror of the capture's `clean`: normalize every string (fixture dir,
/// other absolute paths, timestamps), then mask known row-key timestamps.
fn mask_value(v: &Value, fixture: &Path) -> Value {
    match v {
        Value::String(s) => Value::String(norm_text(s, fixture)),
        Value::Array(items) => Value::Array(items.iter().map(|i| mask_value(i, fixture)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| {
                    if k == "ts" || k == "started_at" {
                        (k.clone(), Value::String("<TS>".into()))
                    } else {
                        (k.clone(), mask_value(val, fixture))
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The projected post-case rows, read from the store the same way every
/// native reader reads it.
fn read_projected_rows(graph: &Path, nodes: &[String], fixture: &Path) -> BTreeMap<String, Value> {
    let fields: &[&str] = &[
        "id",
        "title",
        "status",
        "priority",
        "blocks_everything",
        "type",
        "domain",
        "project",
        "cwd",
        "size",
        "difficulty",
        "difficulty_history",
        "model",
        "model_tier",
        "batch",
        "orphan_ok",
        "public",
        "has_brief",
        "plan_path",
        "locked_by",
        "locked_at",
        "locked_by_harness",
        "locked_by_harness_session",
        "tags",
        "parent",
        "contained_in",
        "released_from",
        "blocked_by",
        "related",
        "source_node_id",
        "caused_by",
        "fixes_pr",
        "reverted",
        "pr_number",
        "pr_url",
        "additional_prs",
        "collisions_acknowledged",
        "completion_note",
        "dispatch_verb",
        "dispatch_brief",
        "rank",
        "created_at",
        "completed_at",
        "touched_at",
    ];
    // The raw store read: the same `read_rows` the api's rows op serves, so
    // the goldens pin store truth (no pydantic presentation defaults).
    let rows = fno_agents::graph_store::read_rows(graph).expect("read rows");
    let mut out = BTreeMap::new();
    for node_id in nodes {
        let row = rows
            .iter()
            .find(|r| r["id"].as_str() == Some(node_id.as_str()))
            .unwrap_or(&Value::Null);
        let mut projected = serde_json::Map::new();
        for f in fields {
            let value = match row {
                Value::Object(map) => map.get(*f).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            projected.insert((*f).to_string(), mask_value(&value, fixture));
        }
        let _ = fixture;
        out.insert(node_id.clone(), Value::Object(projected));
    }
    out
}

fn run_case(case: &Case) {
    let fx = materialize(case);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.arg("backlog").arg(&case.group).args(&fx.argv);
    // The capture ran every case with cwd = the fixture (a git repo), which
    // repo_root()-relative reads like `--plan-path plans/...` resolve against.
    cmd.current_dir(&fx.fixture);
    cmd.env_clear();
    for (k, v) in &fx.env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("binary runs");
    let code = out.status.code().unwrap_or(-1);
    let stdout = norm_text(&String::from_utf8_lossy(&out.stdout), &fx.fixture);
    let stderr = norm_text(&String::from_utf8_lossy(&out.stderr), &fx.fixture);
    assert_eq!(code, case.code, "[{}] exit code", case.name);
    assert_eq!(
        stdout, case.stdout,
        "[{}] stdout\n--- golden:\n{}\n--- got:\n{}",
        case.name, case.stdout, stdout
    );
    assert_eq!(
        stderr, case.stderr,
        "[{}] stderr\n--- golden:\n{}\n--- got:\n{}",
        case.name, case.stderr, stderr
    );
    if !case.read_nodes.is_empty() {
        let got = read_projected_rows(
            &fx.fixture.join("graph.json"),
            &case.read_nodes,
            &fx.fixture,
        );
        for node in &case.read_nodes {
            let golden = case
                .rows
                .get(node)
                .unwrap_or_else(|| panic!("[{}] golden row for {node}", case.name));
            assert_eq!(
                &got[node], golden,
                "[{}] row {node}\n--- golden:\n{golden}\n--- got:\n{}",
                case.name, got[node]
            );
        }
    }
    if !case.ranks.is_empty() {
        for (node, golden) in &case.ranks {
            let mut get = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
            get.args(["backlog", "get", node, "--field", "rank"]);
            get.env_clear();
            for (k, v) in &fx.env {
                get.env(k, v);
            }
            let out = get.output().expect("get runs");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                golden.as_str(),
                "[{}] rank of {node}",
                case.name
            );
        }
    }
    println!("PASS {}", case.name);
}

#[test]
fn replay_backlog_update_goldens() {
    let cases = load_cases("tests/golden/backlog_update/cases.jsonl");
    assert!(!cases.is_empty(), "no update goldens");
    for case in &cases {
        run_case(case);
    }
}

#[test]
fn replay_backlog_rank_goldens() {
    let cases = load_cases("tests/golden/backlog_rank/cases.jsonl");
    assert!(!cases.is_empty(), "no rank goldens");
    for case in &cases {
        run_case(case);
    }
}
