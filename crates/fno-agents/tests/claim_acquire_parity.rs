//! parity-stage: characterization
//! parity-oracle: fno.claims.cli.acquire (the deleted Python body, goldens
//! captured before the shim swap)
//!
//! Characterization harness for the `claim acquire` leaf port (wave 1 of the
//! claims-port epic). The goldens were captured from the Python leaf
//! (`cli/src/fno/claims/cli.py::acquire`) while it still ran its own body:
//! free key (human + JSON), idempotent re-acquire, held-by-other, the
//! validation refusals, the TTL and metadata parse errors, both lane modes,
//! pid-unavailable, and the handover (declined on a holder mismatch, and the
//! takeover that stamps the do row). Captured with `FNO_CAPTURE_GOLDEN=1`
//! while the oracle still exists; in normal mode the frozen bytes are the
//! contract and the Python leg never runs.
//!
//! Both legs run in a pinned state dir (`FNO_STATE_DIR`) with
//! `FNO_AGENTS_HOME` empty, so every node-keyed case lands the same named
//! do-row skip (the node is not in the pin) and the registry effort read is
//! empty on both sides. Volatile fields - pid, host, acquired_at,
//! machine_id, the ambient harness tag - are normalized to placeholders in
//! a fixed key order, so a Python row and a Rust row freeze to the same
//! bytes when they match.

use common::{assert_golden, Golden};
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn python_executable() -> PathBuf {
    let venv = pythonpath().join("../.venv/bin/python");
    if venv.is_file() {
        venv
    } else {
        PathBuf::from("python3")
    }
}

fn binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_fno-agents")
}

/// One leg's observable outcome, already normalized.
struct Leg {
    exit: i32,
    stdout: String,
    stderr: String,
}

impl Leg {
    fn golden(&self) -> Golden {
        Golden {
            exit: Some(self.exit),
            streams: vec![self.stdout.clone(), self.stderr.clone()],
        }
    }
}

fn run_rust(root: &Path, args: &[&str]) -> Leg {
    let out = Command::new(binary_path())
        .args(["claim", "acquire"])
        .args(args)
        .env("FNO_AGENTS_BIN", binary_path())
        .env("PYTHONPATH", pythonpath())
        .current_dir(repo_root())
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_HARNESS_NAME", "claude")
        .env(
            "FNO_HARNESS_SESSION_ID",
            "a1b2c3d4-1111-2222-3333-444455556666",
        )
        .env("CODEX_THREAD_ID", "")
        .env("CLAUDE_CODE_SESSION_ID", "")
        .env("CODEX_SESSION_ID", "")
        .env("GEMINI_SESSION_ID", "")
        .env("OPENCODE_SESSION_ID", "")
        .env("CLAUDE_SESSION_ID", "")
        .output()
        .expect("run fno-agents claim acquire");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout)),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr)),
    }
}

fn run_python(root: &Path, args: &[&str]) -> Leg {
    // The oracle runs behind an `fno` shim so the Usage line names the
    // production prog, not `python -m fno.cli`.
    let shim_dir = root.join("bin");
    let _ = std::fs::create_dir_all(&shim_dir);
    let shim = shim_dir.join("fno");
    // A shebang script, not `python -m`: typer reconstructs the prog name
    // from argv[0], so the oracle's Usage line must come from a real `fno`
    // basename to match production.
    let script = format!(
        "#!{}\nimport sys\nsys.path.insert(0, {:?})\nfrom fno.cli import app\napp()\n",
        python_executable().display(),
        pythonpath().display()
    );
    std::fs::write(&shim, script).expect("write fno shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fno shim");
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    let out = Command::new("fno")
        .args(["agents", "claim", "acquire"])
        .args(args)
        .env("PYTHONPATH", pythonpath())
        .env("FNO_AGENTS_BIN", binary_path())
        .current_dir(repo_root())
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_HARNESS_NAME", "claude")
        .env(
            "FNO_HARNESS_SESSION_ID",
            "a1b2c3d4-1111-2222-3333-444455556666",
        )
        .env("CODEX_THREAD_ID", "")
        .env("CLAUDE_CODE_SESSION_ID", "")
        .env("CODEX_SESSION_ID", "")
        .env("GEMINI_SESSION_ID", "")
        .env("OPENCODE_SESSION_ID", "")
        .env("CLAUDE_SESSION_ID", "")
        .env("PATH", format!("{}:{}", shim_dir.display(), path_var))
        .output()
        .expect("run python fno agents claim acquire");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout)),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr)),
    }
}

const SUBJECT: &str = "claim_acquire";

fn run_raw(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(binary_path())
        .args(["claim", "acquire"])
        .args(args)
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("run setup acquire");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Plant state through the native binary - identical for both legs, so the
/// setup never carries a leg's own behavior.
fn plant(root: &Path, args: &[&str]) {
    let (code, _, stderr) = run_raw(root, args);
    assert_eq!(code, 0, "setup acquire failed: {stderr}");
}

// -------------------------------------------------------------------------
// Normalization: volatile fields to placeholders, JSON to a canonical key
// order, so a Python row and a Rust row freeze to the same bytes.
// -------------------------------------------------------------------------

fn normalize(raw: &str) -> String {
    // The Python CLI bootstraps a fresh state root with a one-time path
    // migration banner on stderr; that is oracle-side setup noise, not leaf
    // behavior, so it is dropped before comparison.
    let raw = raw
        .lines()
        .filter(|line| {
            !line.starts_with("[setup] path migration complete")
                && !line.starts_with("  detected install:")
                && !line.starts_with("  settings written to:")
                && !line.starts_with("  obsidian enabled:")
                && !line.starts_with("  sentinel:")
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The python events layer's store-client verb is skewed against the
    // rust front (a pre-existing break, filed separately); its best-effort
    // refusal banner is oracle-infra noise, not leaf contract, so the whole
    // embedded block is dropped before comparison.
    let raw = drop_event_store_noise(&raw);
    // The two JSON parsers name a malformed payload differently
    // (Python's `Expecting value: ...` against serde's `expected value ...`);
    // the frozen contract keeps the prefix and normalizes the detail.
    let raw = raw.replace(
        "--metadata is not valid JSON: ",
        "--metadata is not valid JSON: PARSER_DETAIL ",
    );
    let trimmed = raw.trim_end_matches('\n');
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return canonical_json(&v);
        }
    }
    trimmed
        .lines()
        .map(scrub_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn drop_event_store_noise(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in raw.lines() {
        if line.starts_with("claims: failed to emit '") {
            skipping = true;
            continue;
        }
        if skipping {
            // The banner embeds the refused subprocess's usage text; it ends
            // at the first line starting with the store's own Error prefix.
            if line.starts_with("Error: ") {
                skipping = false;
            }
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

fn scrub_line(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(pos) = rest.find("pid=").or_else(|| rest.find("host=")) {
        let (kind, placeholder) = if rest[pos..].starts_with("pid=") {
            ("pid=", "pid=PID")
        } else {
            ("host=", "host=HOST")
        };
        out.push_str(&rest[..pos]);
        out.push_str(placeholder);
        rest = &rest[pos + kind.len()..];
        let stop = rest
            .find(|c: char| c.is_whitespace() || c == ')' || c == ',')
            .unwrap_or(rest.len());
        rest = &rest[stop..];
    }
    out.push_str(rest);
    out
}

fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    let obj = v.as_object().expect("checked object upstream");
    if !(obj.contains_key("schema_version") && obj.contains_key("holder")) {
        // Not a claim record (lane receipts, engine payloads): serialize
        // as-is; both legs print the same native bytes there.
        return serde_json::to_string(v).unwrap();
    }
    // harness and session_id are deliberately NOT in the frozen form: they
    // are the additive identity fields on which the legacy (retiring) engine
    // and the native engine are known to diverge, the same fields the
    // cross-impl matrix's STATUS_PARITY_FIELDS excludes.
    let field = |name: &str| -> String {
        match obj.get(name) {
            Some(Value::String(s)) => format!("\"{s}\""),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Object(m)) => serde_json::to_string(m).unwrap(),
            _ => "null".into(),
        }
    };
    let pid_field = match obj.get("pid") {
        Some(p) if !p.is_null() => "\"PID\"".to_string(),
        _ => "null".to_string(),
    };
    let ttl_field = obj
        .get("expires_at")
        .map(|e| (!e.is_null()).to_string())
        .unwrap_or_else(|| "false".into());
    let machine_field = obj
        .get("machine_id")
        .map(|m| (!m.is_null()).to_string())
        .unwrap_or_else(|| "false".into());
    format!(
        "{{\"schema_version\": {}, \"key\": {}, \"holder\": {}, \
         \"pid\": {}, \"has_ttl\": {}, \"machine_id\": {}, \"reason\": {}, \
         \"pid_provenance\": {}, \
         \"metadata\": {}}}",
        field("schema_version"),
        field("key"),
        field("holder"),
        pid_field,
        ttl_field,
        machine_field,
        field("reason"),
        field("pid_provenance"),
        field("metadata"),
    )
}

fn case(label: &'static str, args: &[&str], setup: impl Fn(&Path)) {
    let rust_root = tempfile::tempdir().unwrap();
    setup(rust_root.path());
    let rust = run_rust(rust_root.path(), args);
    let oracle = common::capture_mode().then(|| {
        let py_root = tempfile::tempdir().unwrap();
        setup(py_root.path());
        run_python(py_root.path(), args)
    });
    assert_golden(SUBJECT, label, &rust.golden(), oracle.map(|o| o.golden()));
}

#[test]
fn the_acquire_leaf_matches_the_python_leg_on_every_frozen_case() {
    // 1-2. Free key, human and JSON surfaces.
    case(
        "free_key",
        &["session:par-free", "--holder", "pty:par", "--pid", "4242"],
        |_| {},
    );
    case(
        "free_key_json",
        &[
            "--json",
            "session:par-free",
            "--holder",
            "pty:par",
            "--pid",
            "4242",
        ],
        |_| {},
    );

    // 3. Idempotent re-acquire under the same holder.
    case(
        "idempotent_reacquire",
        &["session:par-again", "--holder", "pty:par", "--pid", "4242"],
        |root| {
            plant(
                root,
                &["session:par-again", "--holder", "pty:par", "--pid", "4242"],
            )
        },
    );

    // 4. Held by a foreign holder: exit 1, named stderr.
    case(
        "held_by_other",
        &["session:par-held", "--holder", "pty:other", "--pid", "4242"],
        |root| {
            plant(
                root,
                &["session:par-held", "--holder", "pty:par", "--pid", "4242"],
            )
        },
    );

    // 5-6. The plain validation refusals.
    case(
        "missing_holder",
        &["session:par-x", "--pid", "4242"],
        |_| {},
    );
    case("missing_key", &["--holder", "pty:par"], |_| {});

    // 7-9. Parse and range refusals on --ttl; the metadata refusal.
    case(
        "ttl_bad_format",
        &["session:par-x", "--holder", "pty:par", "--ttl", "abc"],
        |_| {},
    );
    case(
        "ttl_out_of_range",
        &["session:par-x", "--holder", "pty:par", "--ttl", "1s"],
        |_| {},
    );
    case(
        "metadata_not_object",
        &["session:par-x", "--holder", "pty:par", "--metadata", "[1]"],
        |_| {},
    );

    // 10. pid-unavailable requires --ttl.
    case(
        "pid_unavailable_requires_ttl",
        &["session:par-x", "--holder", "pty:par", "--pid-unavailable"],
        |_| {},
    );

    // 11. pid-unavailable with a TTL: the schema 2 record.
    case(
        "pid_unavailable_json",
        &[
            "--json",
            "session:par-nopid",
            "--holder",
            "pty:par",
            "--pid-unavailable",
            "--ttl",
            "30m",
        ],
        |_| {},
    );

    // 12-13. Lane modes: a free slot, then the cap full.
    case(
        "lane_slot",
        &["--lane", "par-lane", "--max-lanes", "2"],
        |_| {},
    );
    case(
        "lane_cap_full",
        &["--lane", "par-full", "--max-lanes", "1"],
        |root| plant(root, &["--lane", "par-full", "--max-lanes", "1"]),
    );

    // 14. Handover declined on a holder mismatch: the named decline, then
    //     the ordinary acquire's held-by-other (two stderr lines).
    case(
        "handover_declined_mismatch",
        &[
            "node:par-ho",
            "--holder",
            "target-session:sess",
            "--handover-from",
            "spawn-handover:worker",
            "--pid",
            "4242",
        ],
        |root| {
            plant(
                root,
                &[
                    "node:par-ho",
                    "--holder",
                    "target-session:other",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 15. The takeover: a launch-window holder is replaced and the do row is
    //     attempted (the pinned store has no node, so the named skip fires).
    case(
        "handover_takeover",
        &[
            "node:par-take",
            "--holder",
            "target-session:sess",
            "--handover-from",
            "spawn-handover:worker",
            "--ttl",
            "30m",
            "--pid",
            "4242",
        ],
        |root| {
            plant(
                root,
                &[
                    "node:par-take",
                    "--holder",
                    "spawn-handover:worker",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 16. --max-lanes without --lane.
    case("max_lanes_needs_lane", &["--max-lanes", "2"], |_| {});
}
