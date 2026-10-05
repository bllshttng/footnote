//! parity-stage: characterization
//! parity-oracle: fno.claims.cli.refresh (the Python body until the shim
//! swap in this same wave)
//!
//! Characterization harness for the `claim refresh` leaf port (wave 3 of the
//! claims-port epic). The goldens were captured from the Python leaf
//! (`cli/src/fno/claims/cli.py::refresh`) while it still ran its own body:
//! the refreshed receipts (human + JSON, ttl-given and default-window), the
//! missing/wrong-holder/stale refusals in BOTH core shapes (global-id keys
//! ran the legacy body, repo-local keys the native pre-read), the
//! PID-liveness no-ops, and the ttl refusals (zero, sub-range, non-numeric,
//! absent --holder). Captured with `FNO_CAPTURE_GOLDEN=1` while the oracle
//! still exists; in normal mode the frozen bytes are the contract and the
//! Python leg never runs.
//!
//! Both legs run in a pinned state dir (`FNO_STATE_DIR`) with
//! `FNO_AGENTS_HOME` empty-of-registry, so the registry reads (re-anchor)
//! come up empty on both sides. Volatile fields - pid, host, every path
//! under the pinned root, and the two runs' `expires_at` (each leg extends
//! from its own now_ms) - are normalized to placeholders, so a Python row
//! and a Rust row freeze to the same bytes when they match.

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

fn base_env() -> Vec<(&'static str, String)> {
    vec![
        ("PYTHONPATH", pythonpath().display().to_string()),
        ("FNO_AGENTS_BIN", binary_path().to_string()),
        ("FNO_HARNESS_NAME", "claude".into()),
        (
            "FNO_HARNESS_SESSION_ID",
            "a1b2c3d4-1111-2222-3333-444455556666".into(),
        ),
        ("CODEX_THREAD_ID", "".into()),
        ("CLAUDE_CODE_SESSION_ID", "".into()),
        ("CODEX_SESSION_ID", "".into()),
        ("GEMINI_SESSION_ID", "".into()),
        ("OPENCODE_SESSION_ID", "".into()),
        ("CLAUDE_SESSION_ID", "".into()),
        ("CLAUDECODE", "".into()),
    ]
}

fn run_rust(root: &Path, args: &[&str]) -> Leg {
    let out = Command::new(binary_path())
        .args(["claim", "refresh"])
        .args(args)
        .envs(base_env())
        .envs(fno_agents::test_run::self_owner_env())
        .current_dir(repo_root())
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .output()
        .expect("run fno-agents claim refresh");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout), root),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr), root),
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
    // The events validator shells `fno doctor event emit-envelope` and
    // resolves `fno` through PATH - where the shim ABOVE shadows the real
    // Rust front (the shim is the Python app, which has no such command).
    // Pin FNO_BIN to the host's own `fno` resolved BEFORE the shim prepend,
    // so the oracle's emit validates against the production substrate.
    let host_fno = String::from_utf8_lossy(
        &Command::new("which")
            .arg("fno")
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default(),
    )
    .trim()
    .to_string();
    let out = Command::new("fno")
        .args(["agents", "claim", "refresh"])
        .args(args)
        .envs(base_env())
        .envs(fno_agents::test_run::self_owner_env())
        .current_dir(repo_root())
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .env("PATH", format!("{}:{}", shim_dir.display(), path_var))
        .env("FNO_BIN", host_fno)
        .output()
        .expect("run python fno agents claim refresh");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout), root),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr), root),
    }
}

const SUBJECT: &str = "claim_refresh";

/// Plant state through the native binary - identical for both legs, so the
/// setup never carries a leg's own behavior.
fn run_raw(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(binary_path())
        .args(["claim"])
        .args(args)
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_HARNESS_NAME", "claude")
        .env(
            "FNO_HARNESS_SESSION_ID",
            "a1b2c3d4-1111-2222-3333-444455556666",
        )
        .env("CLAUDECODE", "")
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .output()
        .expect("run setup claim");
    (
        exit_code(out.status.code()),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn exit_code(code: Option<i32>) -> i32 {
    code.unwrap_or(-1)
}

fn plant(root: &Path, args: &[&str]) {
    let (code, _, stderr) = run_raw(root, args);
    assert_eq!(code, 0, "setup acquire failed: {stderr}");
}

/// Plant a claim FILE directly, for states the verbs cannot mint on demand
/// (a stale lease with a past expires_at) and for receipts whose bytes the
/// two runtimes would otherwise diverge on: the engine backfills a missing
/// session_id on renew and the legacy body does not, so a JSON receipt must
/// come from a record that already carries one.
fn plant_file(root: &Path, key: &str, expires_at: &str) {
    let dir = root.join(".fno/claims");
    std::fs::create_dir_all(&dir).unwrap();
    let encoded = key.replace(':', "%3A");
    std::fs::write(
        dir.join(format!("{encoded}.lock")),
        format!(
            "schema_version: 1\nkey: {key}\nholder: pty:par\n\
             acquired_at: 1791138000000\npid: 4242\nhost: testhost\n\
             expires_at: {expires_at}\nharness: claude\nsession_id: sess-file\n\
             pid_provenance: ambient\nmachine_id: MACH\n"
        ),
    )
    .unwrap();
}

/// The same file-plant with a FUTURE expires_at: the extendable shape a
/// JSON receipt case needs.
fn plant_live_file(root: &Path, key: &str) {
    plant_file(root, key, "1891138000000");
}

// -------------------------------------------------------------------------
// Normalization: volatile fields to placeholders, JSON to a canonical key
// order, so a Python row and a Rust row freeze to the same bytes.
// -------------------------------------------------------------------------

fn normalize(raw: &str, root: &Path) -> String {
    // Every path under the pinned root (gone-away receipts name the claim
    // file) differs between the two legs' tempdirs; the root itself is the
    // only variable part.
    let root_str = root.display().to_string();
    let raw = raw.replace(&root_str, "<ROOT>");
    // Each leg extends from its own now_ms, so the renewed deadline differs
    // by the gap between the two runs; the FACT of an extension is the
    // contract, the wall-clock digit is not.
    let raw = scrub_expires_at(&raw);
    // The python events layer's store-client verb is skewed against the
    // rust front (a pre-existing break, filed separately); its best-effort
    // refusal banner is oracle-infra noise, not leaf contract, so the whole
    // embedded block is dropped before comparison.
    let raw = drop_event_store_noise(&raw);
    // The python CLI bootstraps a fresh state root with a one-time path
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
    let trimmed = raw.trim_end_matches('\n');
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return canonical_json(&v);
        }
    }
    trimmed.to_string()
}

/// `expires_at=<digits>` (the human receipt) and `"expires_at": <digits>`
/// (inside the claim JSON) become placeholders before anything else reads
/// them. The JSON form keeps quotes around the placeholder so the blob stays
/// parseable and the canonicalization pass still reorders its keys.
fn scrub_expires_at(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(pos) = rest.find("expires_at") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "expires_at".len()..];
        if let Some(v) = after.strip_prefix('=') {
            let digits = digits_prefix(v);
            if !digits.is_empty() {
                out.push_str("expires_at=<EXPIRES>");
                rest = &v[digits.len()..];
                continue;
            }
        }
        if let Some(v) = after.strip_prefix("\":") {
            let trimmed = v.trim_start();
            let digits = digits_prefix(trimmed);
            if !digits.is_empty() {
                out.push_str("expires_at\": \"<EXPIRES>\"");
                rest = &v[v.len() - trimmed.len() + digits.len()..];
                continue;
            }
        }
        out.push_str("expires_at");
        rest = after;
    }
    out.push_str(rest);
    out
}

fn digits_prefix(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..end]
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

fn canonical_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap()
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
fn the_refresh_leaf_matches_the_python_leg_on_every_frozen_case() {
    // 1-3. Own TTL claim refreshed: human and JSON receipts, plus the
    // default-window refresh (empty --ttl extends by MIN_TTL_MS).
    case(
        "refreshed_human",
        &["session:ref-a", "--holder", "pty:par", "--ttl", "1h"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:ref-a",
                    "--holder",
                    "pty:par",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );
    case(
        "refreshed_json",
        &[
            "--json",
            "session:ref-b",
            "--holder",
            "pty:par",
            "--ttl",
            "1h",
        ],
        |root| plant_live_file(root, "session:ref-b"),
    );
    case(
        "refreshed_default_ttl",
        &["session:ref-c", "--holder", "pty:par"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:ref-c",
                    "--holder",
                    "pty:par",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 4. Nothing was ever held: a named gone-away (exit 3).
    case(
        "missing_claim",
        &["session:ref-none", "--holder", "pty:par", "--ttl", "1h"],
        |_| {},
    );

    // 5. A foreign holder is the named exit-4 refusal on the legacy shape.
    case(
        "wrong_holder",
        &[
            "session:ref-foreign",
            "--holder",
            "pty:other",
            "--ttl",
            "30m",
        ],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:ref-foreign",
                    "--holder",
                    "pty:par",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 6-7. A PID-liveness claim (expires_at absent) is a no-op in both
    // surfaces.
    case(
        "pid_liveness_human",
        &["session:ref-pid", "--holder", "pty:par", "--ttl", "30m"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:ref-pid",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );
    case(
        "pid_liveness_json",
        &[
            "--json",
            "session:ref-pid2",
            "--holder",
            "pty:par",
            "--ttl",
            "30m",
        ],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:ref-pid2",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 8. A stale lease refuses with the legacy message (exit 2).
    case(
        "stale_refused",
        &["session:ref-stale", "--holder", "pty:par", "--ttl", "30m"],
        |root| plant_file(root, "session:ref-stale", "1591138000000"),
    );

    // 9-12. The ttl refusals: zero, sub-range, non-numeric, and the absent
    // --holder flag (the typer usage layout).
    case(
        "ttl_zero",
        &["session:ref-zero", "--holder", "pty:par", "--ttl", "0"],
        |_| {},
    );
    case(
        "ttl_small",
        &["session:ref-small", "--holder", "pty:par", "--ttl", "30s"],
        |_| {},
    );
    case(
        "ttl_not_numeric",
        &["session:ref-nan", "--holder", "pty:par", "--ttl", "abc"],
        |_| {},
    );
    case("missing_holder_flag", &["session:ref-noh"], |_| {});

    // 13-14. The repo-local key class runs the NATIVE shape (status
    // pre-read, engine renew, no-op on a benign refusal): refreshed receipts.
    case(
        "refreshed_human_local",
        &["work:ref-l1", "--holder", "pty:par", "--ttl", "1h"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "work:ref-l1",
                    "--holder",
                    "pty:par",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );
    case(
        "refreshed_json_local",
        &[
            "--json",
            "work:ref-l2",
            "--holder",
            "pty:par",
            "--ttl",
            "1h",
        ],
        |root| plant_live_file(root, "work:ref-l2"),
    );

    // 15. Never-held repo-local key with no --ttl: the status pre-read sees
    // free and names the gone-away (exit 3).
    case(
        "missing_local",
        &["work:ref-none", "--holder", "pty:par"],
        |_| {},
    );

    // 16. A foreign holder on the NATIVE shape collapses into the no-op
    // (renew returns a benign false; the Python core returned None). This
    // is the legacy/native divergence the port freezes deliberately.
    case(
        "wrong_holder_local_noop",
        &["work:ref-foreign", "--holder", "pty:other", "--ttl", "30m"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "work:ref-foreign",
                    "--holder",
                    "pty:par",
                    "--ttl",
                    "30m",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 17. A stale repo-local lease refuses with the native message (exit 2).
    case(
        "stale_refused_local",
        &["work:ref-stale", "--holder", "pty:par", "--ttl", "30m"],
        |root| plant_file(root, "work:ref-stale", "1591138000000"),
    );
}
