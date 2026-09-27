//! parity-stage: differential
//! parity-oracle: fno.paths.plan_doc_filename
//!
//! Differential parity for the plans-path port: the Rust chain
//! (`plans_path`, reached through the `fno-agents state plan-path` client
//! verb) and the Python leg (`fno.paths.plan_doc_filename` /
//! `plan_doc_path`) answer the same fixture with the same env pins, and the
//! two answers must match byte for byte. Under `FNO_CAPTURE_GOLDEN=1` the
//! Python leg still runs; the helper asserts Rust==Python and freezes the
//! goldens. The Python leg is deleted in this same change, which converts
//! this file to characterization: the goldens then stand as the contract,
//! capture mode refuses, and the named oracle no longer resolves.
//!
//! Fixture roots move every run, so every golden output is rendered with
//! the fixture root masked first. The pinned instant (2026-09-27 11:30 UTC)
//! renders `20260927` on any UTC machine; date-code goldens depend on that.

use common::{assert_golden as assert_golden_common, capture_mode, Golden};
use std::path::PathBuf;
use std::process::Command;

mod common;

/// Serialize FNO_CONFIG/FNO_STATE_DIR/FNO_SPACES_DIR mutation across the
/// parallel test threads.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Repo `cli/src` so Python can import the real `fno` package.
fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn python_executable() -> PathBuf {
    // Worktree venv, then the canonical checkout's venv (a fresh worktree has
    // no .venv of its own), then a bare python3.
    let venv = pythonpath().join("../.venv/bin/python");
    if venv.is_file() {
        return venv;
    }
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
    {
        if out.status.success() {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let canonical_venv = PathBuf::from(&dir)
                .parent()
                .map(|p| p.join("cli/.venv/bin/python"));
            if let Some(p) = canonical_venv {
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    PathBuf::from("python3")
}

/// The fixture: one git repo `proj` plus the pinned config file, both under
/// a tempdir. `proj` is a git repo so the canonical-root slug both legs hash
/// is the same.
struct Fixture {
    tmp: tempfile::TempDir,
}

/// 2026-09-11 13:30:00 UTC; renders `20260911` under any TZ that keeps the
/// local date on 2026-09-11. UTC CI and the capture machine agree.
const NOW: &str = "1789138200";

/// The env pins BOTH legs inherit: the config walk collapses to the fixture
/// file, and the state roots stay inside it.
fn pin_env(fx: &Fixture) -> Vec<(String, String)> {
    vec![
        (
            "FNO_CONFIG".to_string(),
            fx.tmp.path().join("config.toml").display().to_string(),
        ),
        (
            "FNO_STATE_DIR".to_string(),
            fx.tmp.path().join("state").display().to_string(),
        ),
        (
            "FNO_SPACES_DIR".to_string(),
            fx.tmp.path().join("spaces").display().to_string(),
        ),
    ]
}

/// Mask the moving fixture root (both spellings macOS offers) and the
/// space slug it mints, so the golden describes the chain, not this run's
/// tempdir name.
fn masked(text: String, fx: &Fixture) -> String {
    let root = fx.tmp.path();
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let slug = fno_agents::paths::space_slug(&canonical);
    text.replace(&canonical.to_string_lossy().to_string(), "<fixture>")
        .replace(&root.to_string_lossy().to_string(), "<fixture>")
        .replace(&slug, "<fixture-slug>")
}

/// Run the Rust leg through the built binary: the exact surface the Python
/// verb forwards to.
fn rust_plan_path(fx: &Fixture, pins: &[(String, String)]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["state", "plan-path", "--slug", "s", "--node", "x-aaeb"])
        .arg("--now")
        .arg(NOW)
        .arg(fx.tmp.path().join("proj"))
        .envs(pins.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("run fno-agents state plan-path");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The Python oracle: the old `plan_doc_path` leg on the same fixture and
/// pins. Only runs in capture mode; it dies with the port.
fn py_plan_path(fx: &Fixture, pins: &[(String, String)]) -> (i32, String, String) {
    let code = r#"
import sys
from datetime import datetime
from pathlib import Path
from fno.paths import plan_doc_path
root = Path(sys.argv[1])
now = datetime.fromtimestamp(int(sys.argv[2]))
print(plan_doc_path("s", "x-aaeb", project_root=root, now=now))
"#;
    let out = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(fx.tmp.path().join("proj"))
        .arg(NOW)
        .env("PYTHONPATH", pythonpath())
        .envs(pins.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("run python plan_doc_path");
    (
        out.status.code().unwrap_or(-1),
        masked(String::from_utf8_lossy(&out.stdout).into_owned(), fx),
        masked(String::from_utf8_lossy(&out.stderr).into_owned(), fx),
    )
}

/// One differential case: run the Rust leg, and in capture mode run the
/// Python leg on the same fixture, assert the two agree, and freeze the
/// oracle's answer. In normal mode the frozen golden is the contract.
fn assert_case(label: &str, fx: &Fixture, pins: &[(String, String)]) {
    let _env = env_lock();
    let (code, out, err) = rust_plan_path(fx, pins);
    assert_eq!(code, 0, "rust leg failed: {err:?}");
    let out = masked(out, fx);
    let err = masked(err, fx);
    let golden = Golden {
        exit: Some(code),
        streams: vec![out, err],
    };
    let oracle = capture_mode().then(|| {
        let (pcode, pout, perr) = py_plan_path(fx, pins);
        assert_eq!(
            pcode, code,
            "capture: exit differs\npy_out={pout:?}\npy_err={perr:?}"
        );
        assert_eq!(
            pout, golden.streams[0],
            "capture: the Rust port disagrees with the Python leg"
        );
        assert_eq!(perr, golden.streams[1], "capture: stderr differs");
        Golden {
            exit: Some(pcode),
            streams: vec![pout, perr],
        }
    });
    assert_golden_common("plans-path", label, &golden, oracle);
}

fn build(config: &str, settings: Option<&str>) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    std::fs::create_dir_all(proj.join(".fno")).unwrap();
    let run = |args: &[&str]| {
        Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["-C", proj.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@t"]);
    run(&["config", "user.name", "t"]);
    run(&["commit", "-q", "--allow-empty", "-m", "init"]);
    std::fs::write(tmp.path().join("config.toml"), config).unwrap();
    if let Some(settings) = settings {
        std::fs::write(proj.join(".claude/settings.local.json"), settings).unwrap();
    }
    Fixture { tmp }
}

#[test]
fn local_absolute() {
    let fx = build("", None);
    let dir = fx.tmp.path().join("local-plans");
    std::fs::write(
        fx.tmp.path().join("proj/.claude/settings.local.json"),
        format!(r#"{{"plansDirectory": "{}"}}"#, dir.display()),
    )
    .unwrap();
    assert_case("local_absolute", &fx, &pin_env(&fx));
}

#[test]
fn local_relative() {
    let fx = build("", Some(r#"{"plansDirectory": "docs/plans"}"#));
    assert_case("local_relative", &fx, &pin_env(&fx));
}

#[test]
fn settings_json_relative() {
    let fx = build("", Some(r#"{"plansDirectory": "docs/plans"}"#));
    // Tier 1 must stay silent so tier 2 answers.
    std::fs::remove_file(fx.tmp.path().join("proj/.claude/settings.local.json")).unwrap();
    assert_case("settings_json_relative", &fx, &pin_env(&fx));
}

#[test]
fn config_plain_relative() {
    let fx = build("plans_dir = \"notes/plans\"\n", None);
    assert_case("config_plain_relative", &fx, &pin_env(&fx));
}

#[test]
fn config_sentinel_space() {
    let fx = build("plans_dir = \".fno/plans/\"\n", None);
    assert_case("config_sentinel_space", &fx, &pin_env(&fx));
}

#[test]
fn config_vault_template() {
    let fx = build("plans_dir = \"plans\"\n", None);
    let vault_dir = fx.tmp.path().join("vault");
    std::fs::write(
        fx.tmp.path().join("config.toml"),
        format!(
            "plans_dir = \"{{vault}}/plans\"\n\n[obsidian]\nenabled = true\nvault = \"{}\"\n",
            vault_dir.display()
        ),
    )
    .unwrap();
    assert_case("config_vault_template", &fx, &pin_env(&fx));
}

#[test]
fn config_env_template() {
    let fx = build("plans_dir = \"plans\"\n", None);
    std::fs::write(
        fx.tmp.path().join("config.toml"),
        "plans_dir = \"${PLANS_BASE}/plans\"\n",
    )
    .unwrap();
    let mut pins = pin_env(&fx);
    pins.push((
        "PLANS_BASE".to_string(),
        fx.tmp.path().join("vardir").display().to_string(),
    ));
    assert_case("config_env_template", &fx, &pins);
}

#[test]
fn filename_template_custom() {
    // Date-free template: the golden never depends on the runner's clock.
    let fx = build("plans_filename = \"plans-{slug}-{node}.md\"\n", None);
    assert_case("filename_template_custom", &fx, &pin_env(&fx));
}

#[test]
fn filename_template_dated() {
    let fx = build("", None);
    assert_case("filename_template_dated", &fx, &pin_env(&fx));
}
