//! Wave G4: gemini ask cross-language parity harness (ab-73da4ac2).
//!
//! Drives the SAME fake `gemini` binary through BOTH Python
//! (`providers/gemini.py` `create`/`resume`) and the Rust `gemini_ask` path,
//! asserting identical reply text + exit code. Mirrors `codex_ask_parity.rs`.
//!
//! Skips (not fails) when `python3` or the `abilities` package is unavailable.
//!
//! Cases:
//! - create / resume happy path (single JSON blob -> reply)
//! - null `response` -> empty reply (model declined)
//! - schema drift (missing `stats`) -> exit 11 both sides
//! - non-zero exit with parseable JSON -> exit propagated
//! - stderr noise (Ripgrep/MCP warnings) does NOT corrupt the stdout parse
//!   (the gemini-specific separate-stderr-drain behavior)
//! - inject_from_name parity
//! - cross-language registry round-trip for gemini_session_id + cwd (cv-6c04ef29)

use fno_agents::state::{load_registry, update_registry, RegistryEntry};
use fno_agents::AgentStatus;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Module-level PATH mutex: every test that mutates the process-global PATH or
/// FAKE_GEMINI_* env must hold this for the mutation's duration (cargo runs
/// tests in parallel; subprocess env inheritance ignores Rust-level locks
/// otherwise — same race the codex harness guards).
static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn python_available() -> bool {
    let probe = Command::new("python3")
        .arg("-c")
        .arg("import fno.agents.providers.gemini")
        .env("PYTHONPATH", pythonpath())
        .output();
    matches!(probe, Ok(o) if o.status.success())
}

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-gemini-parity-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Fake `gemini`: emits optional stderr noise, then ONE JSON object to stdout.
/// `FAKE_GEMINI_BLOB` injects a literal blob (drift / null-response cases);
/// otherwise a well-formed blob is built from session_id + reply.
fn install_fake_gemini(bin_dir: &Path) {
    let script = r#"#!/bin/sh
if [ -n "$FAKE_GEMINI_STDERR" ]; then
  printf '%s\n' "$FAKE_GEMINI_STDERR" >&2
fi
if [ -n "$FAKE_GEMINI_BLOB" ]; then
  printf '%s' "$FAKE_GEMINI_BLOB"
else
  printf '{"session_id":"%s","response":"%s","stats":{}}' "${FAKE_GEMINI_SESSION_ID:-}" "${FAKE_GEMINI_REPLY:-}"
fi
exit "${FAKE_GEMINI_EXIT:-0}"
"#;
    let path = bin_dir.join("gemini");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Drive Python `providers.gemini.create`/`.resume`, mapping the provider
/// exceptions to the dispatch.py exit codes so both sides compare the
/// user-facing exit. MUST hold `PATH_MUTEX` for the subprocess lifetime.
fn py_gemini(
    mode: &str,
    session_id: Option<&str>,
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout_sec: u64,
    bin_dir: &Path,
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    let sess = session_id.unwrap_or("");
    let code = format!(
        r#"
import os, sys
sys.path.insert(0, os.environ["PYTHONPATH"])
from pathlib import Path
from fno.agents.providers import gemini as g

prompt = os.environ.get("PROMPT","")
from_name = os.environ.get("FROM_NAME","abilities")
output_path = Path(os.environ["OUTPUT_PATH"])
cwd = Path(os.environ["CWD"])
session_id = os.environ.get("SESSION_ID","")
timeout = float(os.environ.get("TIMEOUT","10"))
yolo = os.environ.get("YOLO","0") == "1"

try:
    if "{mode}" == "create":
        r = g.create(cwd=cwd, prompt=prompt, from_name=from_name, yolo=yolo, output_path=output_path, timeout=timeout)
    else:
        r = g.resume(session_id=session_id, cwd=cwd, prompt=prompt, from_name=from_name, yolo=yolo, output_path=output_path, timeout=timeout)
    sys.stdout.write(r.last_msg or "")
    sys.exit(r.exit_code)
except g.GeminiTimeoutError:
    sys.exit(15)
except g.GeminiParseError:
    sys.exit(11)
except g.GeminiInvocationError as e:
    sys.exit(e.exit_code if e.exit_code != 0 else 1)
"#,
        mode = mode
    );

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(&code);
    for k in [
        "FAKE_GEMINI_SESSION_ID",
        "FAKE_GEMINI_REPLY",
        "FAKE_GEMINI_BLOB",
        "FAKE_GEMINI_EXIT",
        "FAKE_GEMINI_STDERR",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("PYTHONPATH", pythonpath());
    cmd.env("PROMPT", prompt);
    cmd.env("FROM_NAME", from_name);
    cmd.env("OUTPUT_PATH", output_path);
    cmd.env("CWD", cwd);
    cmd.env("SESSION_ID", sess);
    cmd.env("TIMEOUT", timeout_sec.to_string());
    cmd.env("YOLO", if yolo { "1" } else { "0" });
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    cmd.env("PATH", &new_path);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let out = cmd.output().expect("run python3");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

/// Drive Rust gemini_create/gemini_resume with the fake gemini on PATH.
fn rust_gemini(
    mode: &str,
    session_id: Option<&str>,
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout_sec: u64,
    bin_dir: &Path,
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    use fno_agents::gemini_ask::{gemini_create, gemini_resume};

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    for (k, v) in extra_env {
        unsafe { std::env::set_var(k, v) };
    }

    let timeout = Some(std::time::Duration::from_secs(timeout_sec));
    let result = if mode == "create" {
        gemini_create(cwd, prompt, from_name, yolo, output_path, timeout, None)
    } else {
        gemini_resume(
            session_id.unwrap_or(""),
            cwd,
            prompt,
            from_name,
            yolo,
            output_path,
            timeout,
        )
    };

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    for (k, _) in extra_env {
        unsafe { std::env::remove_var(k) };
    }

    match result {
        Ok(r) => (r.exit_code, r.last_msg),
        Err(e) => (e.exit_code(), String::new()),
    }
}

fn assert_parity(case: &str, py: (i32, String), rs: (i32, String)) {
    assert_eq!(py.0, rs.0, "{case}: exit mismatch py={} rs={}", py.0, rs.0);
    assert_eq!(
        py.1, rs.1,
        "{case}: reply mismatch py={:?} rs={:?}",
        py.1, rs.1
    );
}

// ---------------------------------------------------------------------------
// Differential cases
// ---------------------------------------------------------------------------

#[test]
fn parity_create_happy_path() {
    if !python_available() {
        eprintln!("SKIP: python3 with fno.agents.providers.gemini not available");
        return;
    }
    let bin_dir = tmpdir("c1-bin");
    let cwd = tmpdir("c1-cwd");
    install_fake_gemini(&bin_dir);
    let extra = [
        (
            "FAKE_GEMINI_SESSION_ID",
            "g1111111-2222-3333-4444-555566667777",
        ),
        ("FAKE_GEMINI_REPLY", "gemini create reply"),
    ];
    let py = py_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("c1-pyo").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    let rs = rust_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("c1-rso").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    assert_parity("create-happy", py.clone(), rs);
    assert_eq!(py.0, 0);
    assert_eq!(py.1, "gemini create reply");
}

#[test]
fn parity_resume_happy_path() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let bin_dir = tmpdir("r1-bin");
    let cwd = tmpdir("r1-cwd");
    install_fake_gemini(&bin_dir);
    // resume does not re-capture session_id; the blob may still carry one.
    let extra = [
        (
            "FAKE_GEMINI_SESSION_ID",
            "g2222222-2222-3333-4444-555566667777",
        ),
        ("FAKE_GEMINI_REPLY", "gemini resume reply"),
    ];
    let sid = "g2222222-2222-3333-4444-555566667777";
    let py = py_gemini(
        "resume",
        Some(sid),
        &cwd,
        "again",
        "abilities",
        false,
        &tmpdir("r1-pyo").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    let rs = rust_gemini(
        "resume",
        Some(sid),
        &cwd,
        "again",
        "abilities",
        false,
        &tmpdir("r1-rso").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    assert_parity("resume-happy", py.clone(), rs);
    assert_eq!(py.1, "gemini resume reply");
}

#[test]
fn parity_null_response_empty_reply() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let bin_dir = tmpdir("nr-bin");
    let cwd = tmpdir("nr-cwd");
    install_fake_gemini(&bin_dir);
    let extra = [(
        "FAKE_GEMINI_BLOB",
        r#"{"session_id":"g3-2222-3333-4444-555566667777","response":null,"stats":{}}"#,
    )];
    let py = py_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("nr-pyo").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    let rs = rust_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("nr-rso").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    assert_parity("null-response", py.clone(), rs);
    assert_eq!(py.0, 0);
    assert_eq!(py.1, "", "null response must yield empty reply");
}

#[test]
fn parity_schema_drift_missing_stats_exit_11() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let bin_dir = tmpdir("sd-bin");
    let cwd = tmpdir("sd-cwd");
    install_fake_gemini(&bin_dir);
    // Missing `stats` key -> schema drift -> GeminiParseError -> exit 11.
    let extra = [(
        "FAKE_GEMINI_BLOB",
        r#"{"session_id":"g4-2222-3333-4444-555566667777","response":"hi"}"#,
    )];
    let py = py_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("sd-pyo").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    let rs = rust_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("sd-rso").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    assert_parity("drift-missing-stats", py.clone(), rs);
    assert_eq!(py.0, 11, "missing stats must be parse-error exit 11");
}

#[test]
fn parity_nonzero_exit_propagates() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let bin_dir = tmpdir("nz-bin");
    let cwd = tmpdir("nz-cwd");
    install_fake_gemini(&bin_dir);
    // Parseable JSON but non-zero exit -> Invocation -> exit propagated (3).
    let extra = [
        (
            "FAKE_GEMINI_BLOB",
            r#"{"session_id":"g5-2222-3333-4444-555566667777","response":"partial","stats":{}}"#,
        ),
        ("FAKE_GEMINI_EXIT", "3"),
    ];
    let py = py_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("nz-pyo").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    let rs = rust_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("nz-rso").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    assert_parity("nonzero-exit", py.clone(), rs);
    assert_eq!(
        py.0, 3,
        "non-zero exit with parseable JSON propagates the code"
    );
}

#[test]
fn parity_stderr_noise_does_not_corrupt_parse() {
    // The gemini-specific behavior: structural warnings on stderr (Ripgrep,
    // MCP, skill conflicts) must NOT bleed into the stdout JSON parse.
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let bin_dir = tmpdir("se-bin");
    let cwd = tmpdir("se-cwd");
    install_fake_gemini(&bin_dir);
    let extra = [
        (
            "FAKE_GEMINI_SESSION_ID",
            "g6666666-2222-3333-4444-555566667777",
        ),
        ("FAKE_GEMINI_REPLY", "clean reply despite noise"),
        (
            "FAKE_GEMINI_STDERR",
            "Ripgrep is not available; falling back to slower search",
        ),
    ];
    let py = py_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("se-pyo").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    let rs = rust_gemini(
        "create",
        None,
        &cwd,
        "hi",
        "abilities",
        false,
        &tmpdir("se-rso").join("o.jsonl"),
        10,
        &bin_dir,
        &extra,
    );
    assert_parity("stderr-noise", py.clone(), rs);
    assert_eq!(py.0, 0);
    assert_eq!(py.1, "clean reply despite noise");
}

#[test]
fn parity_inject_from_name() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let code = r#"
import os, sys
sys.path.insert(0, os.environ["PYTHONPATH"])
from fno.agents.providers.gemini import inject_from_name
sys.stdout.write(inject_from_name(os.environ.get("PROMPT",""), os.environ.get("FROM_NAME","abilities")))
"#;
    for (prompt, from_name) in [
        ("hello world", "alice"),
        ("", "x"),
        ("multi\nline", "agent-a"),
        ("a&b<c>", "x\"y"),
    ] {
        let py_out = Command::new("python3")
            .arg("-c")
            .arg(code)
            .env("PYTHONPATH", pythonpath())
            .env("PROMPT", prompt)
            .env("FROM_NAME", from_name)
            .output()
            .expect("python inject_from_name");
        assert!(py_out.status.success());
        let py_result = String::from_utf8_lossy(&py_out.stdout).to_string();
        let rs_result = fno_agents::gemini_ask::inject_from_name(prompt, from_name);
        assert_eq!(
            py_result, rs_result,
            "inject mismatch for ({prompt:?},{from_name:?})"
        );
    }
}

// ---------------------------------------------------------------------------
// cv-6c04ef29: cross-language registry round-trip for gemini_session_id + cwd.
// Defends against the PR #364 schema-drift bug class (Rust-only fields must
// not break Python's AgentEntry(**row), and Python rows must read in Rust).
// ---------------------------------------------------------------------------

fn gemini_registry_entry(name: &str, cwd: &str, sid: &str, log_path: &str) -> RegistryEntry {
    RegistryEntry {
        name: name.to_string(),
        short_id: String::new(),
        provider: "gemini".to_string(),
        cwd: cwd.to_string(),
        project_root: String::new(),
        session_id: None,
        claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: Some(sid.to_string()),
        mcp_channel_id: None,
        host_mode: None,
        cc_session_id: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: "2026-05-29T00:00:00Z".to_string(),
        pid: None,
        pid_start_time: None,
        log_path: Some(log_path.to_string()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
    }
}

#[test]
fn registry_roundtrip_rust_to_python() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let dir = tmpdir("rt-r2p");
    let reg_path = dir.join("registry.json");
    let cwd = "/tmp/gemini-proj";
    let sid = "g7777777-2222-3333-4444-555566667777";

    // Rust writes a gemini row.
    update_registry(&reg_path, |reg| {
        reg.entries.push(gemini_registry_entry(
            "rt-agent",
            cwd,
            sid,
            "/tmp/out.jsonl",
        ));
        true
    })
    .expect("rust registry write");

    // Python load_registry must read it back without TypeError (Rust-only
    // fields are skip_serializing'd when empty) and recover the fields.
    let code = r#"
import os, sys
sys.path.insert(0, os.environ["PYTHONPATH"])
from pathlib import Path
from fno.agents.registry import load_registry
rows = load_registry(Path(os.environ["REG"]))
e = next(r for r in rows if r.name == "rt-agent")
sys.stdout.write(f"{e.provider}|{e.cwd}|{e.gemini_session_id}")
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .env("REG", &reg_path)
        .output()
        .expect("python load_registry");
    assert!(
        out.status.success(),
        "python load_registry failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("gemini|{}|{}", cwd, sid)
    );
}

#[test]
fn registry_roundtrip_python_to_rust() {
    if !python_available() {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let dir = tmpdir("rt-p2r");
    let reg_path = dir.join("registry.json");
    let cwd = "/tmp/gemini-proj2";
    let sid = "g8888888-2222-3333-4444-555566667777";

    // Python writes a gemini row.
    let code = r#"
import os, sys
sys.path.insert(0, os.environ["PYTHONPATH"])
from pathlib import Path
from fno.agents.registry import write_registry, AgentEntry
e = AgentEntry(name="rt-agent2", provider="gemini", cwd=os.environ["CWD"],
               log_path="/tmp/out2.jsonl", gemini_session_id=os.environ["SID"])
write_registry([e], Path(os.environ["REG"]))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .env("REG", &reg_path)
        .env("CWD", cwd)
        .env("SID", sid)
        .output()
        .expect("python write_registry");
    assert!(
        out.status.success(),
        "python write_registry failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Rust load_registry must read the Python-authored row and recover fields.
    let reg = load_registry(&reg_path).expect("rust load_registry");
    let e = reg.find("rt-agent2").expect("row present");
    assert_eq!(e.provider, "gemini");
    assert_eq!(e.cwd, cwd);
    assert_eq!(e.gemini_session_id.as_deref(), Some(sid));
}
