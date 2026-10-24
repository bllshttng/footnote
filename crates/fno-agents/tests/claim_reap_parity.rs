//! parity-stage: characterization
//! parity-oracle: fno.claims.cli.reap (the Python body until the shim
//! swap in this same wave)
//!
//! Characterization harness for the `claim reap` leaf port (claims-port
//! epic). The goldens were captured from the Python leaf
//! (`cli/src/fno/claims/cli.py::reap`) while it still ran its own body: the
//! apply and dry-run sweeps of an expired holder-process claim, the kept
//! buckets (live pid, off-host, pid-unavailable suspect, handover suspect
//! with the pane probe unanswered), a node-keyed sweep, a mixed batch in
//! human and JSON shape, the default-root sweep, root dedup, and the two
//! usage refusals. Captured with `FNO_CAPTURE_GOLDEN=1` while the oracle
//! still exists; in normal mode the frozen bytes are the contract and the
//! Python leg never runs.
//!
//! Both legs run in a pinned state dir. The roster arm is unreachable: the
//! pinned `FNO_STATE_DIR`/`FNO_AGENTS_HOME` carry no roster state, so the
//! suspect verdicts come from the session-parse and pane-probe arms only.
//! `FNO_BIN` is pinned to the host's own `fno` on BOTH legs so the mux pane
//! probe shells the same binary, and `FNO_MUX_DIR` is pinned to an empty
//! dir in the handover case so both legs parse the same empty pane list and
//! answer "not absent" identically (a live shared mux could return a real
//! row and reap the claim on one leg only). Dead-pid fixtures kill AND wait
//! the sleeper: an unreaped zombie still answers kill(pid, 0) and would
//! read as alive.

use common::{assert_golden, Golden};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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

/// The host's own `fno` front, resolved once from the TEST process PATH
/// (no shim is prepended there). Pinned as FNO_BIN on both legs: the Rust
/// leaf shells it for the mux pane probe, the Python oracle's probe and
/// events layer shell it too.
fn host_fno() -> &'static str {
    static FNO: OnceLock<String> = OnceLock::new();
    FNO.get_or_init(|| {
        String::from_utf8_lossy(
            &Command::new("which")
                .arg("fno")
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default(),
        )
        .trim()
        .to_string()
    })
    .as_str()
}

/// The machine's real hostname, cached: the on-host liveness arm only
/// engages when the planted host matches, so the fixtures plant the real
/// name and both legs see the same one.
fn real_hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        String::from_utf8_lossy(
            &Command::new("hostname")
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default(),
        )
        .trim()
        .to_string()
    })
    .as_str()
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
        // Point the global config read at nothing so the legs never dial the
        // fleet's remote claims store; both binaries resolve
        // FNO_GLOBAL_SETTINGS_PATH before $HOME/.fno/config.toml.
        ("FNO_GLOBAL_SETTINGS_PATH", "/dev/null".into()),
    ]
}

fn run_rust(root: &Path, args: &[String], extra: &[(&'static str, String)]) -> Leg {
    let out = Command::new(binary_path())
        .args(["claim", "reap"])
        .args(args)
        .envs(base_env())
        .envs(fno_agents::test_run::self_owner_env())
        .current_dir(repo_root())
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .env("FNO_BIN", host_fno())
        .envs(extra.iter().map(|(k, v)| (*k, v.as_str())))
        .output()
        .expect("run fno-agents claim reap");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout), root),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr), root),
    }
}

fn run_python(root: &Path, args: &[String], extra: &[(&'static str, String)]) -> Leg {
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
        .args(["agents", "claim", "reap"])
        .args(args)
        .envs(base_env())
        .envs(fno_agents::test_run::self_owner_env())
        .current_dir(repo_root())
        .env("FNO_CLAIMS_ROOT", root)
        .env("FNO_STATE_DIR", root)
        .env("FNO_AGENTS_HOME", root.join("agents-home"))
        .env("FNO_EVENTS_PATH", root.join("events.jsonl"))
        .env("FNO_BIN", host_fno())
        .envs(extra.iter().map(|(k, v)| (*k, v.as_str())))
        .env("PATH", format!("{}:{}", shim_dir.display(), path_var))
        .output()
        .expect("run python fno agents claim reap");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout), root),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr), root),
    }
}

const SUBJECT: &str = "claim_reap";

// -------------------------------------------------------------------------
// Fixtures: claims planted as files under <root>/.fno/claims, the claims
// dirname the io layer owns. Keys encode ':' to %3A in the filename.
// -------------------------------------------------------------------------

const ACQUIRED_MS: &str = "1791138000000";
const EXPIRED_MS: &str = "1591138000000";
const FUTURE_MS: &str = "1891138000000";

fn plant_yaml(root: &Path, key: &str, body: String) {
    let dir = root.join(".fno/claims");
    std::fs::create_dir_all(&dir).unwrap();
    let encoded = key.replace(':', "%3A");
    std::fs::write(dir.join(format!("{encoded}.lock")), body).unwrap();
}

/// An expired holder-process claim on this host: provably dead by deadline,
/// reaped (or would-reaped) without any liveness probe.
fn plant_expired(root: &Path, key: &str, pid: u32) {
    plant_yaml(
        root,
        key,
        format!(
            "schema_version: 1\nkey: {key}\nholder: pty:par\n\
             acquired_at: {ACQUIRED_MS}\npid: {pid}\nhost: {}\n\
             expires_at: {EXPIRED_MS}\nharness: claude\nsession_id: sess-file\n\
             pid_provenance: holder-process\n",
            real_hostname()
        ),
    );
}

/// An UNEXPIRED holder-process claim on this host whose pid is alive: the
/// kept-live bucket. (An expired row would be provably dead before the
/// host check runs and would reap instead.)
fn plant_live(root: &Path, key: &str, pid: u32) {
    plant_yaml(
        root,
        key,
        format!(
            "schema_version: 1\nkey: {key}\nholder: pty:par\n\
             acquired_at: {ACQUIRED_MS}\npid: {pid}\nhost: {}\n\
             expires_at: {FUTURE_MS}\nharness: claude\nsession_id: sess-file\n\
             pid_provenance: holder-process\n",
            real_hostname()
        ),
    );
}

/// An unexpired claim minted on another host: the sweep keeps it without
/// probing (the pid belongs to a machine that is not this one).
fn plant_offhost(root: &Path, key: &str) {
    plant_yaml(
        root,
        key,
        format!(
            "schema_version: 1\nkey: {key}\nholder: pty:par\n\
             acquired_at: {ACQUIRED_MS}\npid: 4242\nhost: testhost\n\
             expires_at: {FUTURE_MS}\nharness: claude\nsession_id: sess-file\n\
             pid_provenance: ambient\nmachine_id: MACH\n"
        ),
    );
}

/// A schema-2 pid-unavailable claim: the sweep must prove abandonment by
/// finding the holder, never by failing to find it. With no parseable
/// session id in the holder and no native verdict, the verdict is
/// suspect (probe unanswered: holder-session-unparsed).
fn plant_suspect(root: &Path, key: &str, holder: &str) {
    plant_yaml(
        root,
        key,
        format!(
            "schema_version: 2\nkey: {key}\nholder: {holder}\n\
             acquired_at: {ACQUIRED_MS}\npid_unavailable: true\nhost: {}\n\
             expires_at: {FUTURE_MS}\nharness: claude\n",
            real_hostname()
        ),
    );
}

/// A pid that is REALLY dead: spawn a sleeper, kill it, and WAIT. Waiting
/// reaps the zombie; an unreaped zombie still answers kill(pid, 0) and
/// would read as alive on both legs.
fn dead_pid() -> u32 {
    let mut child = Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep for a dead pid");
    let pid = child.id();
    child.kill().expect("kill sleep");
    child.wait().expect("wait sleep");
    pid
}

// -------------------------------------------------------------------------
// Normalization: volatile fields to placeholders, JSON to a canonical key
// order, so a Python row and a Rust row freeze to the same bytes.
// -------------------------------------------------------------------------

fn normalize(raw: &str, root: &Path) -> String {
    // Every path under the pinned root differs between the two legs'
    // tempdirs; the root itself is the only variable part.
    let root_str = root.display().to_string();
    let raw = raw.replace(&root_str, "<ROOT>");
    // Deadline digits never reach the reap receipt, but the scrub is cheap
    // and keeps the stack identical to the sibling harnesses.
    let raw = scrub_expires_at(&raw);
    // The python events layer's store-client command is skewed against the
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
    // A CLI-framework usage refusal renders differently per leg: clap always
    // prints the usage block, typer prefixes some diagnostics with `Error: `
    // and drops the block entirely for missing-argument errors. The leaf
    // contract is the diagnostic sentence, so a usage block reduces both legs
    // to the sentence after it and a leading `Error: ` comes off. The block
    // names each leg's own argv[0] (fno-py vs fno), which goes with it.
    let raw = if raw.contains("Usage: ") {
        match raw.rfind("\n\n") {
            Some(pos) => raw[pos + 2..].to_string(),
            None => raw,
        }
    } else {
        raw.to_string()
    };
    let raw = raw.strip_prefix("Error: ").unwrap_or(&raw).to_string();
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

/// The args closure runs per leg against that leg's own tempdir, so the
/// `--root` cases can name the root they sweep.
fn case(
    label: &'static str,
    args: impl Fn(&Path) -> Vec<String>,
    extra: &[(&'static str, String)],
    setup: impl Fn(&Path),
) {
    let rust_root = tempfile::tempdir().unwrap();
    setup(rust_root.path());
    let rust = run_rust(rust_root.path(), &args(rust_root.path()), extra);
    let oracle = common::capture_mode().then(|| {
        let py_root = tempfile::tempdir().unwrap();
        setup(py_root.path());
        run_python(py_root.path(), &args(py_root.path()), extra)
    });
    assert_golden(SUBJECT, label, &rust.golden(), oracle.map(|o| o.golden()));
}

fn with_root(root: &Path, flags: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
    args.push("--root".into());
    args.push(root.display().to_string());
    args
}

#[test]
fn the_reap_leaf_matches_the_python_leg_on_every_frozen_case() {
    // 1. --apply reaps an expired holder-process claim whose pid is really
    // dead, and emits the swept event.
    case(
        "apply_reaps_expired_holder_process",
        |root| with_root(root, &["--apply"]),
        &[],
        |root| plant_expired(root, "session:ref-reap", dead_pid()),
    );

    // 2. The same shape dry-run lands in would-reap.
    case(
        "dry_run_reports_expired_holder_process",
        |root| with_root(root, &[]),
        &[],
        |root| plant_expired(root, "session:ref-reap-dry", dead_pid()),
    );

    // 3. An unexpired on-host claim with a LIVE pid is kept. The sleeper
    // stays alive across the case and is collected afterwards.
    let mut live = Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .expect("spawn live holder");
    let live_pid = live.id();
    case(
        "live_claim_kept",
        |root| with_root(root, &[]),
        &[],
        |root| plant_live(root, "session:ref-live", live_pid),
    );
    live.kill().expect("kill live holder");
    live.wait().expect("reap live holder");

    // 4. An unexpired off-host claim is kept without a probe.
    case(
        "offhost_kept",
        |root| with_root(root, &[]),
        &[],
        |root| plant_offhost(root, "session:ref-offhost"),
    );

    // 5. A pid-unavailable suspect with an unparseable holder session stays
    // suspect (probe unanswered: holder-session-unparsed) in a dry run.
    case(
        "suspect_pid_unavailable_kept",
        |root| with_root(root, &[]),
        &[],
        |root| plant_suspect(root, "node:reap-suspect", "pty:par"),
    );

    // 6. A handover suspect answers through the pane probe. FNO_MUX_DIR is
    // pinned to an empty dir OUTSIDE the leg roots, shared by both legs, so
    // both probe the same empty pane list and answer identically. The mux
    // tempdir outlives the case. FNO_BIN pins a stub so the policy consult
    // never boots the real CLI: a loaded runner could push that python-boot
    // subprocess past the probe's 10s budget and flip the token to
    // probe-blocked. The stub answers `worktree policy` fast and prints
    // nothing for `mux ls`, which parses as unanswered - the golden shape.
    let mux_dir = tempfile::tempdir().unwrap();
    let mux = mux_dir.path().display().to_string();
    let stub_dir = tempfile::tempdir().unwrap();
    let stub = stub_dir.path().join("fno");
    std::fs::write(
        &stub,
        "#!/bin/sh\nif [ \"$1\" = mux ]; then exit 0; fi\necho harness-native\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    case(
        "handover_suspect_pane_probe_unanswered",
        |root| with_root(root, &[]),
        &[
            ("FNO_MUX_DIR", mux),
            ("FNO_BIN", stub.display().to_string()),
        ],
        |root| plant_suspect(root, "node:reap-handover", "spawn-handover:worker-1"),
    );

    // 7. A node-keyed expired claim sweeps on the same dry-run shape.
    case(
        "node_key_expired_would_reap",
        |root| with_root(root, &[]),
        &[],
        |root| plant_expired(root, "node:reap-expired", dead_pid()),
    );

    // 8. A mixed batch dry-run: one would-reap, one off-host keep, one
    // suspect keep, all in one receipt.
    case(
        "mixed_batch_dry_run",
        |root| with_root(root, &[]),
        &[],
        |root| {
            plant_expired(root, "session:ref-mix-reap", dead_pid());
            plant_offhost(root, "session:ref-mix-offhost");
            plant_suspect(root, "node:reap-mix-suspect", "pty:par");
        },
    );

    // 9. The same batch under --json: ONLY the summary object prints, and
    // normalization canonicalizes its key order.
    case(
        "json_mixed_batch",
        |root| with_root(root, &["--json"]),
        &[],
        |root| {
            plant_expired(root, "session:ref-json-reap", dead_pid());
            plant_offhost(root, "session:ref-json-offhost");
            plant_suspect(root, "node:reap-json-suspect", "pty:par");
        },
    );

    // 10. No --root: the default root (the pinned FNO_CLAIMS_ROOT) sweeps
    // empty and the receipt still names the root it scanned.
    case("no_root_sweeps_defaults", |_| vec![], &[], |_| {});

    // 11. The same root named twice dedups to one scan; the file is counted
    // once.
    case(
        "repeat_root_dedup",
        |root| {
            let r = root.display().to_string();
            vec!["--root".into(), r.clone(), "--root".into(), r]
        },
        &[],
        |root| plant_expired(root, "session:ref-dedup", dead_pid()),
    );

    // 12-13. The usage refusals: an unknown flag and a valueless --root,
    // both the typer layout (exit 2).
    case(
        "usage_unknown_flag",
        |_| vec!["--bogus".into()],
        &[],
        |_| {},
    );
    case("root_missing_value", |_| vec!["--root".into()], &[], |_| {});
}

/// A schema-2 pid-unavailable opt-out claim past its deadline: the sweep
/// reaps it without any liveness probe, then the leaf must restore the
/// recorded prior value through the opt-out bridge. The metadata carries
/// the whole restore contract (config key, file, prior state), exactly
/// what a real merge-gating opt-out records.
fn plant_optout(root: &Path, key: &str, config_path: &Path) {
    let mut body = format!(
        "schema_version: 2\nkey: {key}\nholder: bg:home\n\
         acquired_at: {ACQUIRED_MS}\npid_unavailable: true\nhost: {}\n\
         expires_at: {EXPIRED_MS}\nharness: claude\n",
        real_hostname()
    );
    body.push_str("metadata:\n");
    body.push_str("  config_key: review.self_review_required\n");
    body.push_str(&format!("  config_path: {}\n", config_path.display()));
    body.push_str("  scope: global\n");
    body.push_str("  prior_present: false\n");
    body.push_str("  prior_value: null\n");
    plant_yaml(root, key, body);
}

fn plant_optout_fixture(root: &Path) {
    let config = root.join("config.toml");
    std::fs::write(&config, "[review]\nself_review_required = false\n").unwrap();
    plant_optout(root, "config-optout:review.self_review_required", &config);
}

/// Bespoke (not `case()`): the bridge's contract lives partly in the file
/// system - the restored config must lose the opt-out key - so this case
/// adds a post-run file assert on the Rust leg that `case()` cannot
/// express. The frozen golden still carries the full contract in normal
/// mode: a failed bridge folds a FAILED stderr line in and flips the exit
/// to 1, which diverges from the frozen bytes.
#[test]
fn case_14_optout_restore_bridge() {
    let rust_root = tempfile::tempdir().unwrap();
    plant_optout_fixture(rust_root.path());
    let config = rust_root.path().join("config.toml");
    let rust = run_rust(
        rust_root.path(),
        &with_root(rust_root.path(), &["--apply"]),
        &[],
    );
    let restored = std::fs::read_to_string(&config).unwrap();
    assert!(
        !restored.contains("self_review_required"),
        "config still carries the opt-out key after reap --apply: {restored}"
    );
    let oracle = common::capture_mode().then(|| {
        let py_root = tempfile::tempdir().unwrap();
        plant_optout_fixture(py_root.path());
        run_python(
            py_root.path(),
            &with_root(py_root.path(), &["--apply"]),
            &[],
        )
    });
    assert_golden(
        SUBJECT,
        "optout_restore_bridge",
        &rust.golden(),
        oracle.map(|o| o.golden()),
    );
}
