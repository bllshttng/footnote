//! Replay the committed backlog write goldens (`backlog_update`,
//! `backlog_rank`, `backlog_add`, `backlog_idea`). Each golden line was
//! captured from the surface that owned the verb before its port (Python for
//! update at the port's base sha; the native binary for rank; Python's
//! `_create_node_impl` for add/idea) and carries argv, exit code, stdout,
//! stderr and the post-case row projection. The replay seeds a fresh store,
//! runs the built binary and byte-compares the masked text.
//!
//! Masking contract (shared with `capture_update_goldens.py` and
//! `capture_add_goldens.py`, which wrote the goldens - see each golden dir's
//! capture.json): the fixture dir reads `<FIXTURE>`, any other absolute
//! `/Users`|`/home` path reads `<PATH>`, ISO timestamps read `<TS>`, and the
//! case's minted node id reads `<MINTED>` (add/idea mint one node per case).

use fno_agents::event_store::{query_events, EventQuery};
use fno_agents::graph_store::seed_rows_with_slugs;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tempfile::TempDir;

const SESSION_ID: &str = "golden-fixed-session-0001";
const GIT_ORIGIN: &str = "https://github.com/capture-owner/capture-repo.git";

/// The harness markers this test process must not carry: identity cases
/// prove the golden session through a process-tree walk whose parent is this
/// process, so any harness marker in the live environ contradicts the child's
/// pair. Idempotent; runs once before the first identity case spawns.
fn scrub_test_process_markers() {
    static SCRUB: OnceLock<()> = OnceLock::new();
    SCRUB.get_or_init(|| {
        for key in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_ENTRYPOINT",
            "FNO_HARNESS_NAME",
            "FNO_HARNESS_SESSION_ID",
            "CODEX_THREAD_ID",
            "CODEX_SESSION_ID",
            "FNO_AGENT_SELF",
            "FNO_AGENT_HARNESS",
            "FNO_AGENT_SUBSTRATE",
        ] {
            std::env::remove_var(key);
        }
    });
}

struct Case {
    name: String,
    group: String,
    argv: Vec<String>,
    flavor: String,
    settings: Option<String>,
    identity: bool,
    birth: Option<String>,
    backend: Option<String>,
    fno_node: Option<String>,
    prerun: Vec<Vec<String>>,
    read_nodes: Vec<String>,
    code: i32,
    stdout: String,
    stderr: String,
    rows: BTreeMap<String, Value>,
    events: Vec<Value>,
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
                identity: v["identity"].as_bool().unwrap_or(false),
                birth: v["birth"].as_str().map(str::to_string),
                backend: v["backend"].as_str().map(str::to_string),
                fno_node: v["fno_node"].as_str().map(str::to_string),
                prerun: v["prerun"]
                    .as_array()
                    .map(|runs| {
                        runs.iter()
                            .map(|run| {
                                run.as_array()
                                    .expect("prerun argv")
                                    .iter()
                                    .map(|a| a.as_str().expect("argv str").into())
                                    .collect()
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
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
                events: v["events"]
                    .as_array()
                    .map(|a| a.clone())
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

/// The seed both the capture harnesses and this replay commit to. Keep
/// byte-equal with SEED in capture_update_goldens.py and
/// capture_add_goldens.py: the goldens only hold what the case did to these
/// rows.
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
               "model_tier": "team", "created_at": created}),
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
        // add/idea rows: rollup epics, dedup twins, wave target, fold candidate.
        json!({"id": "x-e0aa0001", "title": "Deployment pipeline hardening", "status": "ready",
               "priority": "p1", "type": "epic", "domain": "code", "project": "fno",
               "created_at": created}),
        json!({"id": "x-e0bb0002", "title": "Docs portal refresh", "status": "ready",
               "priority": "p2", "type": "epic", "domain": "docs", "project": "fno",
               "created_at": created}),
        json!({"id": "x-e0cc0003", "title": "Retired epic", "status": "done",
               "priority": "p3", "type": "epic", "domain": "code", "project": "fno",
               "completed_at": "2026-09-04T00:00:00+00:00", "created_at": created}),
        json!({"id": "x-dedp0001", "title": "Fix deployment pipeline flake", "status": "ready",
               "priority": "p2", "type": "bug", "domain": "code", "project": "fno",
               "details": "the deployment pipeline flakes on reruns", "created_at": created}),
        json!({"id": "x-done0001", "title": "Shipped deployment tool", "status": "done",
               "priority": "p3", "type": "feature", "domain": "code", "project": "fno",
               "completed_at": "2026-09-05T00:00:00+00:00", "created_at": created}),
        json!({"id": "x-arch0001", "title": "Deployment pipeline cleanup", "status": "done",
               "priority": "p3", "type": "feature", "domain": "code", "project": "fno",
               "completed_at": "2026-09-06T00:00:00+00:00",
               "archived_at": "2026-09-10T00:00:00+00:00", "created_at": created}),
        json!({"id": "x-wave0001", "title": "Live wave node", "status": "in_progress",
               "priority": "p2", "type": "feature", "domain": "code", "project": "fno",
               "created_at": created}),
        json!({"id": "x-docs0001", "title": "Docs portal refresh phase two", "status": "in_progress",
               "priority": "p2", "type": "feature", "domain": "code", "project": "fno",
               "plan_path": "plans/surface/00-INDEX.md", "created_at": created}),
    ]
}

const PLAN_OWNED: &str = "---\nclaims: [x-cafe1111]\nsize: M\nstatus: ready\n---\n\n# Owned plan\n";
const PLAN_ALPHA: &str = "---\nsize: S\nstatus: ready\n---\n\n# Alpha plan\n";
const PLAN_SURFACE: &str = "---\nclaims: [x-docs0001]\nsize: M\nstatus: in_progress\n---\n\n# Surface plan\n\n## Files to Modify\n\n| File | Action |\n|------|--------|\n| `cli/src/fno/graph/cli.py` | Modify |\n";

const UPDATE_FIELDS: &[&str] = &[
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
// The create-projection adds every field the builder writes plus the
// row-level stores the birth/wave paths land in.
const ADD_FIELDS: &[&str] = &[
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
    "slug",
    "details",
    "roadmap_id",
    "vision_path",
    "source",
    "source_kind",
    "source_session_id",
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "request_origin",
    "origin_evidence",
    "merge_status",
    "cost_usd",
    "cost_sessions",
    "encounters",
    "progress_notes",
    "archived_at",
];

struct Fixture {
    _dir: TempDir,
    fixture: PathBuf,
    env: Vec<(String, String)>,
    argv: Vec<String>,
}

fn materialize(case: &Case) -> Fixture {
    scrub_test_process_markers();
    let dir = TempDir::new().expect("tempdir");
    let fixture = dir.path().join("fx");
    for sub in ["plans/owned", "plans/alpha", "plans/surface", "proj"] {
        std::fs::create_dir_all(fixture.join(sub)).expect("fixture dirs");
    }
    std::fs::write(
        fixture.join("config.toml"),
        format!("state_dir = \"{}\"\n", fixture.display()),
    )
    .expect("config.toml");
    std::fs::write(fixture.join("plans/owned/00-INDEX.md"), PLAN_OWNED).expect("owned plan");
    std::fs::write(fixture.join("plans/alpha/00-INDEX.md"), PLAN_ALPHA).expect("alpha plan");
    std::fs::write(fixture.join("plans/surface/00-INDEX.md"), PLAN_SURFACE).expect("surface plan");
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
    seed_rows_with_slugs(&fixture.join("graph.json"), &seed_entries()).expect("seed");
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
    // The canonical stamp rides the rank cases (their operator fence needs a
    // resolvable identity). Identity cases (--evidence votes, the armed birth
    // hook) carry the claude ambient pair: the process-tree walk proves the
    // marker self-minted against THIS test process, so the scrub below keeps
    // the parent marker-free. Identity-free cases keep provenance
    // degradation deterministic (see the golden dirs' capture.json).
    if case.group == "rank" {
        env.push(("FNO_HARNESS_NAME".into(), "claude".into()));
        env.push(("FNO_HARNESS_SESSION_ID".into(), SESSION_ID.into()));
    }
    if case.identity {
        env.push(("CLAUDECODE".into(), "1".into()));
        env.push(("CLAUDE_CODE_SESSION_ID".into(), SESSION_ID.into()));
    }
    if case.birth.as_deref() == Some("armed") {
        env.push(("FNO_THINK_SPAWN".into(), "1".into()));
        env.push(("FNO_THINK_SPAWN_PRESENCE".into(), "attended".into()));
        env.push((
            "FNO_EVENTS_PATH".into(),
            fixture
                .join("events")
                .join("events.jsonl")
                .to_string_lossy()
                .to_string(),
        ));
    }
    if let Some(backend) = &case.backend {
        env.push(("FNO_TRACKER_BACKEND".into(), backend.clone()));
    }
    if let Some(node) = &case.fno_node {
        env.push(("FNO_NODE".into(), node.clone()));
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
    // The canonicalized cwd carries macOS's /private prefix in the capture;
    // a Linux replay produces the bare temp path, so fold both spellings.
    // (Path::join would discard the /private against an absolute fixture.)
    let private_prefix = format!("/private{}", fixture.to_string_lossy());
    let s = s.replace(private_prefix.as_str(), "<FIXTURE>");
    let s = s.replace(fixture.to_string_lossy().as_ref(), "<FIXTURE>");
    let s = path_re().replace_all(&s, "<PATH>").into_owned();
    ts_re().replace_all(&s, "<TS>").into_owned()
}

/// Mirror of the capture's `norm_value`: normalize every string (fixture dir,
/// other absolute paths, timestamps), then mask known row-key timestamps.
fn mask_value(v: &Value, fixture: &Path) -> Value {
    match v {
        Value::String(s) => Value::String(norm_text(s, fixture)),
        Value::Array(items) => Value::Array(items.iter().map(|i| mask_value(i, fixture)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| {
                    if k == "ts" || k == "started_at" || k == "created_at" {
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

fn fields_for(group: &str) -> &'static [&'static str] {
    if group == "add" || group == "idea" {
        ADD_FIELDS
    } else {
        UPDATE_FIELDS
    }
}

/// The projected post-case rows, read from the store the same way every
/// native reader reads it. A node named `<MINTED>` resolves to the case's
/// minted id, and the minted id is masked inside the projected values.
fn read_projected_rows(
    graph: &Path,
    nodes: &[String],
    fixture: &Path,
    minted: Option<&str>,
    group: &str,
) -> BTreeMap<String, Value> {
    let fields = fields_for(group);
    let rows = fno_agents::graph_store::read_rows(graph).expect("read rows");
    let resolve = |node_id: &str| -> String {
        match (node_id, minted) {
            ("<MINTED>", Some(id)) => id.to_string(),
            _ => node_id.to_string(),
        }
    };
    let mut out = BTreeMap::new();
    for node_id in nodes {
        let real = resolve(node_id);
        let row = rows
            .iter()
            .find(|r| r["id"].as_str() == Some(real.as_str()))
            .unwrap_or(&Value::Null);
        let mut projected = serde_json::Map::new();
        for f in fields {
            let value = match row {
                Value::Object(map) => map.get(*f).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            let mut value = mask_value(&value, fixture);
            if let Some(id) = minted {
                value = replace_id(&value, id);
            }
            projected.insert((*f).to_string(), value);
        }
        out.insert(node_id.clone(), Value::Object(projected));
    }
    out
}

fn replace_id(v: &Value, minted: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace(minted, "<MINTED>")),
        Value::Array(items) => Value::Array(items.iter().map(|i| replace_id(i, minted)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| (k.clone(), replace_id(val, minted)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The ids the seed holds: anything else in the store after a run was minted
/// by the case (or its preruns).
fn seed_ids() -> std::collections::BTreeSet<String> {
    seed_entries()
        .iter()
        .filter_map(|e| e["id"].as_str().map(str::to_string))
        .collect()
}

fn spawn_backlog(
    env: &[(String, String)],
    group: &str,
    argv: &[String],
    cwd: &Path,
) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.arg("backlog").arg(group).args(argv);
    cmd.current_dir(cwd);
    cmd.env_clear();
    for (k, v) in fno_agents::test_run::self_owner_env() {
        cmd.env(k, v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The projected events beside FNO_EVENTS_PATH, ordered by seq: the same
/// `type` + masked `data` projection the Python capture wrote.
fn read_projected_events(journal: &Path, fixture: &Path, minted: Option<&str>) -> Vec<Value> {
    let rows = query_events(journal, &EventQuery::default()).expect("read events");
    rows.iter()
        .map(|row| {
            let line: Value =
                serde_json::from_str(&row.line).expect("event line is the envelope json");
            let mut data = line["data"].clone();
            if let Some(id) = minted {
                data = replace_id(&data, id);
            }
            json!({
                "type": line["type"].clone(),
                "data": mask_value(&data, fixture),
            })
        })
        .collect()
}

fn run_case(case: &Case) {
    let fx = materialize(case);
    // Preruns mint their own nodes; their ids join the "not new" set and
    // their output is discarded.
    for prerun in &case.prerun {
        let (code, _, stderr) = spawn_backlog(&fx.env, &case.group, prerun, &fx.fixture);
        assert_eq!(code, 0, "[{}] prerun failed: {stderr}", case.name);
    }
    let (code, raw_stdout, raw_stderr) = spawn_backlog(&fx.env, &case.group, &fx.argv, &fx.fixture);
    // The case's minted id: one node beyond the seed and the preruns.
    let rows_now =
        fno_agents::graph_store::read_rows(&fx.fixture.join("graph.json")).expect("read rows");
    let known = seed_ids();
    let mut extra_ids: Vec<String> = rows_now
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .filter(|id| !known.contains(id))
        .collect();
    extra_ids.sort();
    // A prerun case holds two non-seed ids (the warm node plus the captured
    // mint); the captured run wrote second, so the latest created_at is the
    // mint.
    let minted: Option<String> = match extra_ids.len() {
        0 => None,
        1 => Some(extra_ids[0].clone()),
        2 if !case.prerun.is_empty() => {
            let created = |id: &str| -> String {
                rows_now
                    .iter()
                    .find(|r| r["id"].as_str() == Some(id))
                    .and_then(|r| r["created_at"].as_str().map(str::to_string))
                    .unwrap_or_default()
            };
            extra_ids.sort_by_key(|id| created(id));
            extra_ids.last().cloned()
        }
        n => panic!(
            "[{}] expected at most one mint (+prerun), got {n}",
            case.name
        ),
    };
    let mask = |s: &str| -> String {
        let s = match &minted {
            Some(id) => s.replace(id.as_str(), "<MINTED>"),
            None => s.to_string(),
        };
        norm_text(&s, &fx.fixture)
    };
    let stdout = mask(&raw_stdout);
    let stderr = mask(&raw_stderr);
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
            minted.as_deref(),
            &case.group,
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
    if !case.events.is_empty() || case.birth.as_deref() == Some("armed") {
        let journal = fx.fixture.join("events").join("events.jsonl");
        let got = read_projected_events(&journal, &fx.fixture, minted.as_deref());
        assert_eq!(
            got, case.events,
            "[{}] events\n--- golden:\n{:?}\n--- got:\n{:?}",
            case.name, case.events, got
        );
    }
    if !case.ranks.is_empty() {
        for (node, golden) in &case.ranks {
            let mut get = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
            get.args(["backlog", "get", node, "--field", "rank"]);
            get.env_clear();
            for (k, v) in fno_agents::test_run::self_owner_env() {
                get.env(k, v);
            }
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

#[test]
fn replay_backlog_add_goldens() {
    let cases = load_cases("tests/golden/backlog_add/cases.jsonl");
    assert!(!cases.is_empty(), "no add goldens");
    for case in &cases {
        run_case(case);
    }
}

#[test]
fn replay_backlog_idea_goldens() {
    let cases = load_cases("tests/golden/backlog_idea/cases.jsonl");
    assert!(!cases.is_empty(), "no idea goldens");
    for case in &cases {
        run_case(case);
    }
}
