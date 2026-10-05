//! parity-stage: characterization
//! parity-oracle: fno.claims.cli.release (the Python body until the shim
//! swap in this same wave)
//!
//! Characterization harness for the `claim release` leaf port (wave 2 of the
//! claims-port epic). The goldens were captured from the Python leaf
//! (`cli/src/fno/claims/cli.py::release`) while it still ran its own body:
//! the released/no-op receipts (human + JSON), the wrong-holder no-op, the
//! strict holder mismatch (exit 4), every mode-validation refusal, the force
//! modes (archived and nothing-released, human + JSON), the lane release,
//! and the do-row close/rollback skips. Captured with `FNO_CAPTURE_GOLDEN=1`
//! while the oracle still exists; in normal mode the frozen bytes are the
//! contract and the Python leg never runs.
//!
//! Both legs run in a pinned state dir (`FNO_STATE_DIR`) with
//! `FNO_AGENTS_HOME` empty, so every node-keyed case lands the same named
//! do-row skip (the node is not in the pin) and the registry effort read is
//! empty on both sides. Volatile fields - pid, host, and every path under
//! the pinned root - are normalized to placeholders, so a Python row and a
//! Rust row freeze to the same bytes when they match.

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
        .args(["claim", "release"])
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
        .env("CLAUDECODE", "")
        .output()
        .expect("run fno-agents claim release");
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
    let out = Command::new("fno")
        .args(["agents", "claim", "release"])
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
        .env("CLAUDECODE", "")
        .output()
        .expect("run python fno agents claim release");
    Leg {
        exit: out.status.code().unwrap_or(-1),
        stdout: normalize(&String::from_utf8_lossy(&out.stdout), root),
        stderr: normalize(&String::from_utf8_lossy(&out.stderr), root),
    }
}

const SUBJECT: &str = "claim_release";

/// Plant state through the native binary - identical for both legs, so the
/// setup never carries a leg's own behavior.
fn run_raw(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(binary_path())
        .args(["claim"])
        .args(args)
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
        .env("CLAUDECODE", "")
        .output()
        .expect("run setup claim");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn plant(root: &Path, args: &[&str]) {
    let (code, _, stderr) = run_raw(root, args);
    assert_eq!(code, 0, "setup acquire failed: {stderr}");
}

/// Plant a node-keyed claim file directly, WITH its identity fields. The two
/// runtimes' ambient identity resolvers disagree under a hermetic env (the
/// python prover refuses an unprovable stamp; the rust resolver accepts the
/// pin), and the do-row identity must come from the CLAIM for the legs to
/// agree - so the stamp cases plant the file bytes instead of running an
/// acquire whose record depends on a resolver.
fn plant_node_claim(root: &Path, key: &str, holder: &str) {
    let dir = root.join(".fno/claims");
    std::fs::create_dir_all(&dir).unwrap();
    let encoded = key.replace(':', "%3A");
    std::fs::write(
        dir.join(format!("{encoded}.lock")),
        format!(
            "schema_version: 1\nkey: {key}\nholder: {holder}\n\
             acquired_at: 1791138000000\npid: 4242\nhost: testhost\n\
             expires_at: 1891138000000\nharness: claude\nsession_id: sess-file\n\
             pid_provenance: ambient\nmachine_id: MACH\n"
        ),
    )
    .unwrap();
}

// -------------------------------------------------------------------------
// Normalization: volatile fields to placeholders, JSON to a canonical key
// order, so a Python row and a Rust row freeze to the same bytes.
// -------------------------------------------------------------------------

fn normalize(raw: &str, root: &Path) -> String {
    // Every path under the pinned root (claims file receipts, force
    // archives, the resolved nothing-released path) differs between the two
    // legs' tempdirs; the root itself is the only variable part.
    let root_str = root.display().to_string();
    let raw = raw.replace(&root_str, "<ROOT>");
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
fn the_release_leaf_matches_the_python_leg_on_every_frozen_case() {
    // 1-2. Own claim released: human and JSON receipts.
    case(
        "released_human",
        &["session:rel-a", "--holder", "pty:par"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-a",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );
    case(
        "released_json",
        &["--json", "session:rel-a", "--holder", "pty:par"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-a",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 3-4. Nothing was ever held: a no-op release, human and JSON.
    case(
        "noop_missing",
        &["session:rel-none", "--holder", "pty:par"],
        |_| {},
    );
    case(
        "noop_missing_json",
        &["--json", "session:rel-none", "--holder", "pty:par"],
        |_| {},
    );

    // 5. A foreign holder is a no-op without --strict.
    case(
        "wrong_holder_noop",
        &["session:rel-foreign", "--holder", "pty:other"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-foreign",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 6-7. --strict names a foreign holder (exit 4) and releases an own one.
    // The TTL plant keeps the prior state non-free host-stably: pid 4242 is
    // dead everywhere, so the claim reads suspect on both legs.
    case(
        "strict_mismatch",
        &["session:rel-strict", "--holder", "pty:other", "--strict"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-strict",
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
        "strict_own_released",
        &["session:rel-strict2", "--holder", "pty:par", "--strict"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-strict2",
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

    // 8-15. The mode-validation refusals.
    case("missing_holder", &["session:rel-x"], |_| {});
    case("missing_key", &["--holder", "pty:par"], |_| {});
    case(
        "reason_without_force",
        &["session:rel-x", "--holder", "pty:par", "--reason", "why"],
        |_| {},
    );
    case("force_needs_reason", &["session:rel-x", "--force"], |_| {});
    case(
        "force_with_holder",
        &[
            "session:rel-x",
            "--force",
            "--reason",
            "why",
            "--holder",
            "pty:par",
        ],
        |_| {},
    );
    case(
        "force_with_strict",
        &["session:rel-x", "--force", "--reason", "why", "--strict"],
        |_| {},
    );
    case(
        "force_with_stamp_do",
        &["session:rel-x", "--force", "--reason", "why", "--stamp-do"],
        |_| {},
    );
    case(
        "stamp_and_rollback",
        &[
            "session:rel-x",
            "--holder",
            "pty:par",
            "--stamp-do",
            "--rollback-do",
        ],
        |_| {},
    );

    // 16-17. Force drops an owned claim to .expired/, naming the archive.
    case(
        "force_released",
        &["session:rel-force", "--force", "--reason", "admin drop"],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-force",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );
    case(
        "force_released_json",
        &[
            "--json",
            "session:rel-force2",
            "--force",
            "--reason",
            "admin drop",
        ],
        |root| {
            plant(
                root,
                &[
                    "acquire",
                    "session:rel-force2",
                    "--holder",
                    "pty:par",
                    "--pid",
                    "4242",
                ],
            )
        },
    );

    // 18-19. Force with nothing at the resolved path REFUSES (exit 1).
    case(
        "force_nothing",
        &["session:rel-void", "--force", "--reason", "admin drop"],
        |_| {},
    );
    case(
        "force_nothing_json",
        &[
            "--json",
            "session:rel-void2",
            "--force",
            "--reason",
            "admin drop",
        ],
        |_| {},
    );

    // 20-21. Lane slots release through the same command.
    case("lane_release", &["--lane", "rel-lane"], |root| {
        plant(
            root,
            &["lane-acquire", "--lane", "rel-lane", "--max-lanes", "2"],
        )
    });
    case(
        "lane_release_json",
        &["--json", "--lane", "rel-lane2"],
        |root| {
            plant(
                root,
                &["lane-acquire", "--lane", "rel-lane2", "--max-lanes", "2"],
            )
        },
    );

    // 22-23. The do-row close and rollback fire on a released node claim;
    // the pinned store holds no node, so the named skip lands on both legs.
    // The plant writes the claim file WITH its identity fields: the two
    // runtimes' ambient identity resolvers disagree under a hermetic env, so
    // the row identity must come from the CLAIM, never a resolver.
    case(
        "stamp_do_skip",
        &[
            "node:rel-stamp",
            "--holder",
            "target-session:sess",
            "--stamp-do",
        ],
        |root| plant_node_claim(root, "node:rel-stamp", "target-session:sess"),
    );
    case(
        "rollback_do_skip",
        &[
            "node:rel-rollback",
            "--holder",
            "target-session:sess",
            "--rollback-do",
        ],
        |root| plant_node_claim(root, "node:rel-rollback", "target-session:sess"),
    );

    // 24-25. A no-op release names the skipped do row instead of silence.
    case(
        "stamp_do_noop_release",
        &[
            "node:rel-noop",
            "--holder",
            "target-session:sess",
            "--stamp-do",
        ],
        |_| {},
    );
    case(
        "rollback_do_noop_release",
        &[
            "node:rel-noop2",
            "--holder",
            "target-session:sess",
            "--rollback-do",
        ],
        |_| {},
    );
}
