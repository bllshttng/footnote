//! parity-stage: characterization
//! parity-oracle: fno.review.invocation
//!
//! Golden parity for the `review_invocation` port (the verb parser, the
//! sidecar join, the reserve, the emit, and the settle sweep), frozen
//! against the live Python leg before its deletion. The case table lives in
//! `golden/review_invocation/cases.json`, written by
//! `review_invocation_golden_driver.py` alongside the per-case goldens; this
//! file is its lockstep consumer. The driver and this file share the
//! normalization both sides apply: every volatile head becomes `<head9>`
//! (rows, receipt) or `<head>` (appended data), and a minted id is pinned by
//! its `ri-` shape, never frozen.

mod common;

use common::golden_dir;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every prefix that can leak machine identity into the config walk or the
/// sidecar root. The capture driver scrubbed the same set.
const SCRUB_PREFIXES: [&str; 10] = [
    "FNO_",
    "TARGET_",
    "CLAUDE_",
    "CODEX_",
    "GEMINI_",
    "OPENCODE_",
    "ANTHROPIC_",
    "OPENAI_",
    "ZAI_",
    "GLM_",
];

fn scrubbed() -> Command {
    let mut cmd = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_fno-agents")));
    for (k, _) in std::env::vars_os() {
        let text = k.to_string_lossy().to_string();
        if SCRUB_PREFIXES.iter().any(|p| text.starts_with(p)) {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for (k, _) in std::env::vars_os() {
        let text = k.to_string_lossy().to_string();
        if SCRUB_PREFIXES.iter().any(|p| text.starts_with(p)) {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git(repo: &Path, args: &[&str]) {
    let out = git_command()
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git spawns");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo_head9(repo: &Path) -> String {
    let out = git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("git spawns")
        .stdout;
    String::from_utf8_lossy(&out)
        .trim()
        .chars()
        .take(9)
        .collect()
}

fn rfc3339_ms(ms: i64) -> String {
    // Same civil-from-days algorithm the fno crate's timestamp uses, from a
    // given instant so the fixture ages relative to the sweep's own clock.
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let remainder = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        remainder / 3_600,
        (remainder % 3_600) / 60,
        remainder % 60
    )
}

struct Runtime {
    repo: PathBuf,
    journal: PathBuf,
    home: PathBuf,
}

fn build_runtime(base: &Path, key: &str, detached: bool) -> Runtime {
    let dir = base.join(key);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "feature/x"]);
    git(&repo, &["config", "user.email", "t@example.test"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("f.txt"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    if detached {
        git(&repo, &["checkout", "-q", "--detach"]);
    }
    let fno_dir = dir.join(".fno");
    std::fs::create_dir_all(&fno_dir).unwrap();
    Runtime {
        home: dir.join("home"),
        repo,
        journal: fno_dir.join("events.jsonl"),
    }
}

fn fixture_envelope(row: &Value, now_ms: i64) -> String {
    let age_ms = row["age_minutes"].as_i64().unwrap_or(0) * 60_000;
    let ts = rfc3339_ms(now_ms - age_ms);
    let kind = row["kind"].as_str().unwrap();
    let id = row["id"].as_str().unwrap();
    let verb = row["verb"].as_str().unwrap_or("/code-review");
    match kind {
        "sent" => serde_json::json!({
            "ts": ts,
            "type": "review_invocation",
            "source": "daemon",
            "data": {
                "invocation_id": id,
                "stage": "sent",
                "verb": verb,
                "transport": "mail_send_raw",
                "receipt": "delivered (hosted)",
            },
        }),
        "refused" => serde_json::json!({
            "ts": ts,
            "type": "review_invocation",
            "source": "daemon",
            "data": {
                "invocation_id": id,
                "stage": "refused",
                "verb": verb,
                "reason": "empty_diff",
            },
        }),
        "attested" => serde_json::json!({
            "ts": ts,
            "type": "review_attestation",
            "source": "hook",
            "data": {
                "reviewer": "code-review",
                "head_sha": "f".repeat(40),
                "verdict": "pass",
                "session_id": "fixture",
                "invocation_id": id,
            },
        }),
        other => panic!("unknown fixture kind: {other}"),
    }
    .to_string()
}

fn compare_parse(mut bin_cmd: Command, case: &Value, problems: &mut Vec<String>) {
    let label = case["label"].as_str().unwrap();
    let key = common::slug(label);
    let mut bad = |msg: String| problems.push(format!("[{label}] {msg}"));
    let out = bin_cmd
        .args([
            "review-level",
            "invocations",
            "parse",
            "--raw",
            case["raw"].as_str().unwrap(),
        ])
        .env("HOME", std::env::temp_dir())
        .output()
        .expect("fno-agents binary spawns");
    if out.status.code() != Some(0) {
        bad(format!(
            "exit differs: golden 0, rust {:?}",
            out.status.code()
        ));
        return;
    }
    let got: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let golden_text = std::fs::read_to_string(
        golden_dir("review_invocation")
            .join("parse")
            .join(format!("{key}.json")),
    )
    .unwrap_or_else(|e| panic!("missing golden parse/{key}.json: {e}"));
    let want: Value = serde_json::from_str(&golden_text).expect("golden parses");
    // Parsed-JSON equality, never bytes: Python's dumps spacing differs from
    // serde's, exactly as the plan states.
    if got != want {
        bad(format!("parse differs\ngolden={want}\nrust  ={got}"));
    }
}

fn sidecar_root(runtime: &Runtime, home_mode: &str) -> PathBuf {
    if home_mode == "set" {
        runtime.home.clone()
    } else {
        runtime.home.join(".fno")
    }
}

fn compare_sidecar(runtime_base: &Path, case: &Value, problems: &mut Vec<String>) {
    let label = case["label"].as_str().unwrap();
    let key = common::slug(label);
    let mut bad = |msg: String| problems.push(format!("[{label}] {msg}"));
    let runtime = build_runtime(runtime_base, &key, false);
    std::fs::create_dir_all(&runtime.home).unwrap();
    let home_mode = case["home"].as_str().unwrap();
    let root = sidecar_root(&runtime, home_mode);
    let session = case["session"].as_str().unwrap();
    let preset = case["preset"].as_str();
    if let Some(preset) = preset {
        let directory = root.join("review-invocations");
        std::fs::create_dir_all(&directory).unwrap();
        let sidecar = directory.join(format!("{session}.json"));
        if preset == "valid" {
            std::fs::write(
                &sidecar,
                serde_json::json!({"invocation_id": "ri-golden-valid", "target_session_id": session})
                    .to_string(),
            )
            .unwrap();
        } else {
            std::fs::write(&sidecar, "{not json").unwrap();
        }
    }
    let mut cmd = scrubbed();
    cmd.env("HOME", &runtime.home);
    if home_mode == "set" {
        cmd.env("FNO_HOME", &runtime.home);
    }
    let out = cmd
        .args(["review-level", "invocations", "join", "--session", session])
        .current_dir(&runtime.repo)
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("fno-agents binary spawns");
    if out.status.code() != Some(0) {
        bad(format!(
            "exit differs: golden 0, rust {:?}",
            out.status.code()
        ));
        return;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let sidecar = root
        .join("review-invocations")
        .join(format!("{session}.json"));
    match case["expect"]["id"].as_str() {
        Some(want) => {
            if id != want {
                bad(format!("join id differs: golden {want:?}, rust {id:?}"));
            }
        }
        None => {
            if !id.starts_with("ri-") {
                bad(format!("join id lacks the mint shape: {id:?}"));
            }
        }
    }
    // A non-empty session always consumes or holds exactly one sidecar.
    if !session.is_empty() {
        if preset == Some("valid") {
            if sidecar.exists() {
                bad("a valid preset sidecar must be consumed by the join".to_string());
            }
        } else {
            match std::fs::read_to_string(&sidecar) {
                Ok(text) => {
                    let value: Value = serde_json::from_str(&text).expect("sidecar json");
                    let held = value["invocation_id"].as_str().unwrap_or("");
                    if held != id {
                        bad(format!(
                            "the sidecar must hold the join's id: {held:?} vs {id:?}"
                        ));
                    }
                }
                Err(e) => bad(format!("the join must write the sidecar: {e}")),
            }
        }
    }
    // The golden drives the outcome table; this case's id is pinned above.
    let _ = key;
}

fn compare_settle(runtime_base: &Path, case: &Value, problems: &mut Vec<String>) {
    let label = case["label"].as_str().unwrap();
    let key = common::slug(label);
    let mut bad = |msg: String| problems.push(format!("[{label}] {msg}"));
    let golden = golden_dir("review_invocation").join("settle");
    // Run one: the sweep settles the lost rows.
    let runtime = build_runtime(
        runtime_base,
        &format!("{key}-run1"),
        case["detached"].as_bool().unwrap_or(false),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let rows_spec = case["rows"].as_array().cloned().unwrap_or_default();
    let envelopes: Vec<String> = rows_spec
        .iter()
        .map(|row| fixture_envelope(row, now))
        .collect();
    for envelope in &envelopes {
        fno::event_store::append_envelope(&runtime.journal, envelope, None)
            .expect("the fixture envelope commits");
    }
    let run = |runtime: &Runtime| {
        scrubbed()
            .args(["review-level", "invocations", "settle", "--json"])
            .env("HOME", &runtime.home)
            .env("FNO_EVENTS_PATH", &runtime.journal)
            .current_dir(&runtime.repo)
            .envs(fno_agents::test_run::self_owner_env())
            .output()
            .expect("fno-agents binary spawns")
    };
    let out = run(&runtime);
    if out.status.code() != Some(0) {
        bad(format!(
            "exit differs: golden 0, rust {:?}",
            out.status.code()
        ));
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut got: Value = serde_json::from_str(&stdout).expect("settle prints a JSON array");
    // The volatile head leaves wholesale: a substring replace inside the full
    // sha would leave a placeholder-prefixed tail the golden never had.
    if let Some(rows) = got.as_array_mut() {
        for row in rows {
            if row.get("head").is_some() {
                row["head"] = Value::String("<head9>".to_string());
            }
        }
    }
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(golden.join(format!("{key}.rows.json")))
            .unwrap_or_else(|e| panic!("missing golden settle/{key}.rows.json: {e}")),
    )
    .expect("golden rows parse");
    if got != want {
        bad(format!("settle rows differ\ngolden={want}\nrust  ={got}"));
    }
    // The appended rows: one lost review_attestation per lost sent row, the
    // same normalization the driver froze.
    let appended = settled_rows(&runtime.journal);
    let appended_golden: Value = serde_json::from_str(
        &std::fs::read_to_string(golden.join(format!("{key}.appended.json")))
            .unwrap_or_else(|e| panic!("missing golden settle/{key}.appended.json: {e}")),
    )
    .expect("golden appended parse");
    if appended != appended_golden {
        bad(format!(
            "appended attestation data differs\ngolden={appended_golden}\nrust  ={appended}"
        ));
    }
    // Run two: idempotent. A second sweep appends nothing.
    let out2 = run(&runtime);
    let rows2: Value = serde_json::from_slice(&out2.stdout).expect("second settle prints JSON");
    // Idempotent in rows, not in printout: a sweep that SETTLED everything
    // answers an empty list the second time, but a sweep that REFUSED (no
    // readable head) still owns those lost rows and re-reports the refusal.
    let settled_all = got.as_array().is_some_and(|rows| {
        rows.iter()
            .all(|r| r.get("settled") == Some(&serde_json::json!(true)))
    });
    if settled_all {
        if rows2.as_array().map(Vec::is_empty) != Some(true) {
            bad(format!("the second sweep must add nothing, got {rows2}"));
        }
    } else if rows2 != got {
        bad(format!(
            "the second sweep must repeat the refusal unchanged\ngolden={got}\nrust  ={rows2}"
        ));
    }
    let appended2 = settled_rows(&runtime.journal);
    if appended2 != appended {
        bad("the second sweep must not append rows".to_string());
    }
    // Run three: the bare receipt on a fresh fixture, byte for byte except
    // the head placeholder.
    let runtime3 = build_runtime(
        runtime_base,
        &format!("{key}-run3"),
        case["detached"].as_bool().unwrap_or(false),
    );
    for envelope in &envelopes {
        fno::event_store::append_envelope(&runtime3.journal, envelope, None)
            .expect("the fixture envelope commits");
    }
    let out3 = scrubbed()
        .args(["review-level", "invocations", "settle"])
        .env("HOME", &runtime3.home)
        .env("FNO_EVENTS_PATH", &runtime3.journal)
        .current_dir(&runtime3.repo)
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("fno-agents binary spawns");
    let stdout3 =
        String::from_utf8_lossy(&out3.stdout).replace(&repo_head9(&runtime3.repo), "<head9>");
    let want3 = std::fs::read_to_string(golden.join(format!("{key}.receipt.txt")))
        .unwrap_or_else(|e| panic!("missing golden settle/{key}.receipt.txt: {e}"));
    if stdout3 != want3 {
        bad(format!(
            "receipt differs\ngolden={want3:?}\nrust  ={stdout3:?}"
        ));
    }
}

/// The appended settle rows, read the way the capture driver read them:
/// committed `review_attestation` rows whose `session_id` is `settle`, data
/// only, sorted by invocation id, head normalized.
fn settled_rows(journal: &Path) -> Value {
    let rows = fno::event_store::query_events(
        journal,
        &fno::event_store::EventQuery::of_types(&["review_attestation"]),
    )
    .unwrap_or_default();
    let mut out: Vec<Value> = rows
        .iter()
        .filter_map(|row| serde_json::from_str::<Value>(&row.line).ok())
        .filter(|event| event["type"] == "review_attestation")
        .filter(|event| event["data"]["session_id"] == "settle")
        .map(|event| event["data"].clone())
        .map(|mut data| {
            // Wholesale placeholder: the golden froze `<head>`, never a
            // truncated sha.
            if data.get("head_sha").is_some() {
                data["head_sha"] = Value::String("<head>".to_string());
            }
            data
        })
        .collect();
    out.sort_by_key(|data| data["invocation_id"].as_str().unwrap_or("").to_string());
    Value::Array(out)
}

fn cases() -> Vec<Value> {
    let path = golden_dir("review_invocation").join("cases.json");
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cases.json unreadable: {e}")),
    )
    .expect("cases.json parses")
}

/// The vendored core is the fno crate's file, byte for byte: the pure owner
/// (parse, flags, mint, sidecar) has one canonical copy, and the copy this
/// crate splices in drifts only by a failed test.
#[test]
fn owner_core_is_vendored_byte_identical() {
    let canonical = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../fno/src/review_invocation_core.rs"
    ))
    .expect("the fno crate's core file sits beside this checkout");
    let vendored = include_bytes!("../src/review_invocation_owner.rs");
    assert_eq!(
        vendored, canonical.as_slice(),
        "review_invocation_owner.rs drifted from crates/fno/src/review_invocation_core.rs; copy the file, never fork it"
    );
}

#[test]
fn review_invocation_parse_golden_parity() {
    let cases = cases();
    let mut problems: Vec<String> = Vec::new();
    let mut ran = 0;
    for case in &cases {
        if case["family"].as_str() == Some("parse") {
            compare_parse(scrubbed(), case, &mut problems);
            ran += 1;
        }
    }
    assert!(ran > 0, "the parse table ran no case: cases.json lost it");
    assert!(
        problems.is_empty(),
        "{} parse case(s) differ from the goldens:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn review_invocation_sidecar_golden_parity() {
    let cases = cases();
    let base =
        std::env::temp_dir().join(format!("review-invocation-sidecar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let mut problems: Vec<String> = Vec::new();
    let mut ran = 0;
    for case in &cases {
        if case["family"].as_str() == Some("sidecar") {
            compare_sidecar(&base, case, &mut problems);
            ran += 1;
        }
    }
    assert!(ran > 0, "the sidecar table ran no case: cases.json lost it");
    assert!(
        problems.is_empty(),
        "{} sidecar case(s) differ from the goldens:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn review_invocation_settle_golden_parity() {
    let cases = cases();
    let base =
        std::env::temp_dir().join(format!("review-invocation-settle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let mut problems: Vec<String> = Vec::new();
    let mut ran = 0;
    for case in &cases {
        if case["family"].as_str() == Some("settle") {
            compare_settle(&base, case, &mut problems);
            ran += 1;
        }
    }
    assert!(ran > 0, "the settle table ran no case: cases.json lost it");
    assert!(
        problems.is_empty(),
        "{} settle case(s) differ from the goldens:\n{}",
        problems.len(),
        problems.join("\n")
    );
}
