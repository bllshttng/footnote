//! parity-stage: characterization
//! parity-oracle: fno.pr._coverage_gate.coverage_verdict
//!
//! The goldens under tests/golden/coverage_gate/ were captured from the
//! Python legs by capture_driver.py: its default mode runs each entry family
//! (check, verdict, publish, waive, chain) on its hermetic fixture and
//! freezes the masked streams plus a folded stub-call journal. This test is
//! the Rust half of that protocol. It rebuilds the identical fixture tree
//! with `--build-only` (import-free, no oracle run), drives the operand
//! through the production argv door (`fno-agents review-coverage
//! check|publish|waive|op`), and compares masked streams against the frozen
//! goldens. Capture mode is a no-op here: the driver is the capture path,
//! and a golden can only be re-frozen by running the driver against the
//! Python legs, never from this binary.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use common::{assert_golden, Golden};
use serde_json::{json, Value};

mod common;

/// The cases whose goldens carry a sentence the native verdict answers
/// differently BY DESIGN (the driver's own *.divergence notes say why: the
/// native verdict counts one shared head set per round, names the stop
/// gate's existing sentence for an unreadable decision store, and answers a
/// missing row with the unknown sentinel instead of the searched-paths
/// sentence). Parity for them compares state and exit only, exactly as the
/// notes promise; the check-shaped member also pins the stderr refusal word.
const DIVERGENCE_CASES: [&str; 4] = [
    "verdict_rounds_two_axes",
    "verdict_waiver_authority_unknown",
    "verdict_no_row",
    "check_no_row",
];

/// Session identity stripped before the operand runs, mirroring STRIP_VARS
/// in capture_driver.py: a cargo-test parent carries live-session pins that
/// would otherwise reach into the hermetic fixture. FNO_SPACES_DIR is NOT
/// mirrored: the driver resolves the case state off FNO_STATE_DIR and the
/// native off the agents-home parent, so the test pins the spaces root to
/// the case dir explicitly and a parent's live pin cannot leak either way.
const STRIP_VARS: [&str; 21] = [
    "CLAUDE_CODE_SESSION_ID",
    "TARGET_SESSION_ID",
    "SESSION_ID",
    "FNO_HARNESS_NAME",
    "FNO_HARNESS_SESSION_ID",
    "CLAUDE_PLUGIN_ROOT",
    "TARGET_MISSION_ID",
    "FNO_AGENTS_BIN",
    "FNO_AGENTS_FRONT",
    "FNO_AGENTS_WORKER",
    "FNO_REPO_ROOT",
    "FNO_CAPTURE_GOLDEN",
    "FNO_TRACKER_BACKEND",
    "FNO_EVENTS_PATH",
    "FNO_STATE_DIR",
    "FNO_CLAIMS_ROOT",
    "FNO_CONFIG",
    "FNO_AGENTS_HOME",
    "FNO_COVERAGE_CASE_DIR",
    "GIT_DIR",
    "GIT_WORK_TREE",
];

#[test]
fn coverage_gate_matches_python_goldens() {
    if common::capture_mode() {
        // See the module docs: the driver is the capture path.
        return;
    }
    let root = tempfile::tempdir().expect("fixture tempdir");
    let driver = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/coverage_gate/capture_driver.py");
    let built = Command::new("python3")
        .arg(&driver)
        .arg("--build-only")
        .arg(root.path())
        .output()
        .expect("capture_driver --build-only runs");
    assert!(
        built.status.success(),
        "--build-only failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let start = built
        .stdout
        .iter()
        .position(|&b| b == b'{')
        .expect("--build-only prints a JSON map");
    let map: Value = serde_json::from_slice(&built.stdout[start..]).expect("build map parses");
    let stubbin = PathBuf::from(map["stubbin"].as_str().expect("stubbin path"));
    let cases = map["cases"].as_array().expect("cases list");

    // cases.json order, not a hash-map order: a failure names the case the
    // fixture set put at that position on every machine.
    for entry in cases {
        let case_dir = PathBuf::from(entry["dir"].as_str().expect("case dir"));
        run_case(&case_dir, &stubbin);
    }
}

/// Run one case's operand child on the fixture and compare streams + folds.
fn run_case(case_dir: &Path, stubbin: &Path) {
    let case: Value = serde_json::from_str(
        &fs::read_to_string(case_dir.join("case.json")).expect("case.json exists"),
    )
    .expect("case.json parses");
    let label = case["label"].as_str().expect("case label");
    let entry = case["entry"].as_str().expect("case entry");
    let pr = case["pr"].as_u64().expect("numeric pr");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    // The keeper-reap guard requires every triad-spawning test to declare
    // its owner, so the spawned client is attributed to this test process.
    cmd.envs(fno_agents::test_run::self_owner_env());
    cmd.arg("review-coverage");
    let mut stdin_payload: Option<String> = None;
    match entry {
        "check" => {
            cmd.arg("check").arg(pr.to_string());
            if case["recompute"].as_bool().unwrap_or(false) {
                cmd.arg("--recompute");
            }
        }
        "publish" => {
            cmd.arg("publish").arg(pr.to_string());
            if let Some(head) = case["head"].as_str().filter(|h| !h.is_empty()) {
                cmd.arg("--head").arg(head);
            }
        }
        "waive" => {
            cmd.arg("waive").arg(pr.to_string());
            let reason = case["reason"].as_str().unwrap_or("");
            cmd.arg("--reason").arg(reason);
        }
        "verdict" | "chain" => {
            // The op door: one JSON payload on stdin, one JSON line back.
            cmd.arg("op");
            let mut payload = match case["cwd"].as_str() {
                Some(cwd) => json!({"op": entry, "cwd": cwd}),
                None => json!({"op": entry, "cwd": case_dir.to_string_lossy()}),
            };
            if entry == "verdict" {
                payload["pr"] = json!(pr);
                payload["recompute"] = json!(case["recompute"].as_bool().unwrap_or(false));
            } else {
                payload["branch"] = json!(case["branch"].as_str().unwrap_or(""));
                payload["head"] = json!(case["head"].as_str().unwrap_or(""));
            }
            stdin_payload = Some(payload.to_string());
        }
        other => panic!("unknown case entry {other}"),
    }

    // The environment cmd_case pins: session identity stripped, the fixture's
    // config/state/home/claims world installed, stub binaries on the knobs,
    // git de-promped and de-configured, then the case's own env map applied
    // last. FNO_LOOPCHECK_FNO_BIN is the native leg's front-binary seam: the
    // Python child patched resolve_front_binary to the stub, and the native
    // law reads and the waive decide door resolve through this pin instead.
    for var in STRIP_VARS {
        cmd.env_remove(var);
    }
    cmd.env("FNO_CONFIG", case_dir.join("config.toml"));
    cmd.env("FNO_STATE_DIR", case_dir);
    // The builder plants the events journal under <case>/spaces (the
    // FNO_STATE_DIR convention); the native resolves the spaces root off the
    // agents-home parent when no explicit pin exists. The pin is the one
    // declared knob that points the native at the planted file.
    cmd.env("FNO_SPACES_DIR", case_dir.join("spaces"));
    cmd.env("FNO_AGENTS_HOME", case_dir.join("home").join("agents-home"));
    cmd.env("HOME", case_dir.join("home"));
    cmd.env("FNO_CLAIMS_ROOT", case_dir.join("claims"));
    cmd.env("FNO_COVERAGE_CASE_DIR", case_dir);
    cmd.env("FNO_AGENTS_BIN", stubbin.join("fno-agents"));
    cmd.env("FNO_LOOPCHECK_FNO_BIN", stubbin.join("fno"));
    // The head probe resolves its front binary through FNO_BIN (scrape), not
    // through the loopcheck seam above: unpinned it finds the operator's
    // deployed fno and the case world never sees the read.
    cmd.env("FNO_BIN", stubbin.join("fno"));
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    match case["path"].as_str().unwrap_or_default() {
        "empty" => {
            cmd.env("PATH", "");
        }
        "gitbin" => {
            cmd.env("PATH", case_dir.join("gitbin"));
        }
        _ => {
            cmd.env("PATH", format!("{}:/usr/bin:/bin", stubbin.display()));
        }
    }
    if let Some(env) = case["env"].as_object() {
        for (key, value) in env {
            cmd.env(key, value.as_str().expect("case env values are strings"));
        }
    }
    cmd.current_dir(case_dir);

    let output = match stdin_payload {
        Some(payload) => {
            cmd.stdin(Stdio::piped());
            let mut child = cmd.spawn().expect("op-door child spawns");
            child
                .stdin
                .take()
                .expect("stdin piped")
                .write_all(payload.as_bytes())
                .expect("op payload written");
            child.wait_with_output().expect("op-door child runs")
        }
        None => {
            cmd.stdin(Stdio::null());
            cmd.output().expect("operand child runs")
        }
    };

    let masked_out = mask_text(&String::from_utf8_lossy(&output.stdout), case_dir);
    let masked_err = mask_text(&String::from_utf8_lossy(&output.stderr), case_dir);
    let key = common::slug(label);

    if DIVERGENCE_CASES.contains(&key.as_str()) {
        // The .divergence notes name the sentence difference; the answer's
        // state and the exit stay the contract.
        let golden_out =
            fs::read_to_string(common::golden_dir("coverage_gate").join(format!("{key}.out")))
                .unwrap_or_else(|e| panic!("[{label}] missing golden {key}.out: {e}"));
        let golden_exit =
            fs::read_to_string(common::golden_dir("coverage_gate").join(format!("{key}.exit")))
                .expect("golden exit");
        let rust_exit = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_default();
        assert_eq!(
            golden_exit.trim(),
            rust_exit,
            "[{label}] divergent-case exit differs"
        );
        if golden_out.trim().is_empty() {
            // The check-shaped divergence (check_no_row): the refusal word
            // rides on stderr, stdout stays empty, and the searched-paths
            // suffix is the state-root fingerprint the note absorbs.
            assert!(
                masked_out.trim().is_empty(),
                "[{label}] divergent-case stdout carries bytes where the golden is empty"
            );
            assert!(
                masked_err.starts_with("coverage unknown"),
                "[{label}] divergent-case stderr does not carry the sentinel refusal word: {masked_err:?}"
            );
        } else {
            let golden: Value =
                serde_json::from_str(golden_out.trim()).expect("golden verdict parses");
            let rust: Value = serde_json::from_str(masked_out.trim()).expect("rust verdict parses");
            assert_eq!(
                golden["state"], rust["state"],
                "[{label}] divergent-case state differs"
            );
        }
        return;
    }

    // The driver freezes .out always and .err only when the masked stderr is
    // nonempty; assert_golden reads the same shape back. The op-door answers
    // re-render through the Python dumps spelling first: the goldens were
    // frozen with json.dumps(sort_keys=True), whose separators are part of
    // the frozen bytes and not part of the door's contract.
    let out_line = if entry == "verdict" || entry == "chain" {
        let parsed: Value = serde_json::from_str(masked_out.trim()).expect("op-door answer parses");
        py_dumps(&parsed)
    } else {
        masked_out
    };
    let mut streams = vec![out_line];
    if !masked_err.is_empty() {
        streams.push(masked_err);
    }
    let rust = Golden {
        exit: output.status.code(),
        streams,
    };
    assert_golden("coverage_gate", label, &rust, None);
    compare_calls(label, case_dir);
}

/// Compare the folded stub-call journals against the golden `.calls`. Only
/// the doors that survive the port compare: the `agents` and `gh` folds are
/// the topology the port legitimately absorbed (the Python gate shelled the
/// producer verb and the REST pre-read; the native verdict reads the row and
/// the PR facts in process), so their rows differ by design, while the law
/// doors (`front`, `law_subjects`) are behavior the native leg must
/// reproduce in the same order.
fn compare_calls(label: &str, case_dir: &Path) {
    let key = common::slug(label);
    let golden_text =
        fs::read_to_string(common::golden_dir("coverage_gate").join(format!("{key}.calls")))
            .unwrap_or_else(|e| panic!("[{label}] missing golden {key}.calls: {e}"));
    let golden: Value = serde_json::from_str(golden_text.trim())
        .unwrap_or_else(|e| panic!("[{label}] golden {key}.calls does not parse: {e}"));
    let rust_text = mask_text(&fold_calls(case_dir).to_string(), case_dir);
    let rust: Value = serde_json::from_str(&rust_text)
        .unwrap_or_else(|e| panic!("[{label}] folded calls do not parse: {e}"));
    for field in ["front", "law_subjects", "events"] {
        assert_eq!(
            golden.get(field),
            rust.get(field),
            "[{label}] stub-call fold '{field}' differs from the golden"
        );
    }
}

/// Mirror of fold_calls in capture_driver.py: read the journal keys in the
/// driver's order and collapse each to first-appearance runs.
fn fold_calls(case_dir: &Path) -> Value {
    let journals = case_dir.join("journals");
    let front = read_rows(&journals, "front.jsonl");
    json!({
        "agents": collapse(read_rows(&journals, "fno-agents.jsonl").iter().map(|row| field_of(row, "op")).collect()),
        "gh": collapse(read_rows(&journals, "gh.jsonl").iter().map(|row| field_of(row, "op")).collect()),
        "front": collapse(front.iter().map(|row| field_of(row, "op")).collect()),
        "law_subjects": collapse(front.iter().map(|row| field_of(row, "subject")).collect()),
        "events": collapse(read_rows(&journals, "events.jsonl").iter().map(|row| field_of(row, "type")).collect()),
    })
}

/// Missing file folds to no rows; an unparsable line folds to an op sentinel,
/// exactly as the driver does.
fn read_rows(journals: &Path, name: &str) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(journals.join(name)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or(json!({"op": "<unparsed>"})))
        .collect()
}

fn field_of(row: &Value, key: &str) -> Value {
    row.get(key).cloned().unwrap_or(Value::Null)
}

/// Mirror of collapse in capture_driver.py: first-appearance order; a
/// consecutive run longer than one folds to `<value>*`, a run of exactly one
/// keeps the bare value.
fn collapse(values: Vec<Value>) -> Vec<Value> {
    let mut runs: Vec<(Value, usize)> = Vec::new();
    for value in values {
        match runs.last_mut() {
            Some(entry) if entry.0 == value => entry.1 += 1,
            _ => runs.push((value, 1)),
        }
    }
    runs.into_iter()
        .map(|(value, n)| {
            if n == 1 {
                value
            } else {
                Value::String(format!("{}*", py_str(&value)))
            }
        })
        .collect()
}

/// Python `%s` spelling of a JSON value, for the run-star suffix.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

/// Mirror of mask_text in capture_driver.py: strip host-specific spellings so
/// the frozen bytes stay portable. The resolved spelling first (a sentence
/// carrying the realpath contains the unresolved one as a substring), then
/// the dashed spellings of paths.space_slug, then the pid span.
fn mask_text(text: &str, case_dir: &Path) -> String {
    let mut out = text.to_string();
    // The resolved spelling first: it contains the unresolved one as a
    // substring, and replacing the shorter one first would strand the
    // /private prefix (macOS tempdirs) as machine-specific golden bytes.
    let real = fs::canonicalize(case_dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    for base in [real, case_dir.to_string_lossy().to_string()] {
        if base.is_empty() {
            continue;
        }
        out = out.replace(&base, "<CASE>");
        let dashed = base.replace('/', "-");
        if !dashed.is_empty() {
            out = out.replace(&dashed, "<CASE>");
        }
    }
    // The sized self-review hint: the port renders the harness-native
    // spelling (`/fno:review <level> --comment`) where the Python leg froze
    // the CLI spelling (`fno do review --level <level> --reviewer
    // code-review`). Both name the same advice, so the segment masks to one
    // placeholder on both sides of the compare; the goldens carry the
    // masked form.
    let out = replace_span(&out, " - `", '`', true, " - `<hint>`");
    replace_span(&out, "(pid=", ')', true, "(pid=<ERR>)")
}

/// A regex-free `needle[^terminator]*` substitution; the pid pattern ends
/// with `[^)]*` inside parens, so the `)` is consumed.
fn replace_span(
    text: &str,
    needle: &str,
    terminator: char,
    consume: bool,
    replacement: &str,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(needle) {
        let after = &rest[start + needle.len()..];
        match after.find(terminator) {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push_str(replacement);
                if !consume {
                    out.push(terminator);
                }
                rest = &after[end + terminator.len_utf8()..];
            }
            None => {
                out.push_str(&rest[..start]);
                out.push_str(replacement);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Python `json.dumps(payload, sort_keys=True)`: the frozen goldens carry its
/// separators (", " and ": ") and its ensure_ascii escaping, so the op-door
/// answers render through the same shape before the byte compare.
fn py_dumps(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(text) => py_string(text),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_dumps).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            // sort_keys=True: serde_json's preserve_order keeps insertion
            // order, so the keys sort here explicitly.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}: {}", py_string(k), py_dumps(&map[k])))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// The `ensure_ascii=True` string spelling: quotes and backslashes escaped,
/// control characters and anything outside printable ASCII as `\uXXXX`
/// (surrogate pairs above the BMP).
fn py_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) >= 0x7f => {
                let code = c as u32;
                if code < 0x10000 {
                    out.push_str(&format!("\\u{code:04x}"));
                } else {
                    let v = code - 0x10000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
