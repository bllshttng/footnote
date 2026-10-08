//! parity-stage: characterization
//! parity-oracle: cli/src/fno/pr_watch/_install.py:refresh_watcher
//!
//! Characterization for the pr-watch refresh port: the Rust verb
//! (`fno-agents pr-watch refresh`, reached through `pr_watch::refresh`) is
//! pinned by the frozen goldens under tests/golden/pr-watch-refresh/. Under
//! `FNO_CAPTURE_GOLDEN=1` this file runs as a LIVE differential: the Python
//! leg (`refresh_watcher` + `heal_status_line` through the fixture oracle)
//! answers the same fixture with the same env pins and the run asserts
//! byte-identity before the goldens freeze. The Python function survives
//! only for callers a later wave moves (`groom.py`, the `heal` leaf), so
//! capture keeps working until wave 6.
//!
//! Four fixture states: the watcher disabled, a tick in flight (bounce
//! deferred), a plain bounce (bootout/bootstrap/kickstep scripted), and an
//! unchanged plist (no bounce). The bounce cases never touch the real
//! launchctl: `FNO_TEST_PR_WATCH_LAUNCHCTL` scripts the steps on both legs,
//! the way the load-state pin does for status.

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{assert_golden, capture_mode, slug, Golden};

mod common;

/// Repo `cli/src` so Python can import the real `fno` package.
fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn python_executable() -> PathBuf {
    // Worktree venv, then the canonical checkout's venv (a fresh worktree has
    // no .venv of its own; its .git is a gitdir FILE pointing at the canonical
    // repo, so the walk follows it), then a bare python3.
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

/// Case directories are the only moving paths; timestamps ride no stdout.
fn masked(text: &str, dir: &Path) -> String {
    text.replace(&dir.display().to_string(), "<CASE>")
}

/// One fixture case: a config file, a state root with the events journal,
/// and a LaunchAgents dir holding (or not) the plist.
struct Case {
    label: &'static str,
    enabled: bool,
    /// Pre-render the plist with the Rust renderer and write it, so the
    /// run reads "unchanged". The Python oracle renders with its own
    /// renderer over the same inputs: a byte difference between the two
    /// renderers fails the capture instead of hiding.
    prerender: bool,
    tick_pid: Option<u32>,
    /// Scripted launchctl steps as `[rc, timed]` pairs, in invocation order.
    launchctl: Vec<(i32, bool)>,
    force_bounce: bool,
}

fn write_case(root: &Path, case: &Case) -> PathBuf {
    let dir = root.join(slug(case.label));
    let agents = dir.join("LaunchAgents");
    let state = dir.join("state");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    // state_dir is a TOP-LEVEL key: under a [table] header the config loader
    // reads it as pr_watch.state_dir and warns it is unmodeled.
    let mut config = format!("state_dir = \"{}\"\n\n", state.display());
    config.push_str("[pr_watch]\n");
    config.push_str(&format!("enabled = {}\n", case.enabled));
    config.push_str("interval_seconds = 600\n");
    std::fs::write(dir.join("config.toml"), config).unwrap();

    if case.prerender {
        let plist_text =
            fno_agents::pr_watch::render_plist_for_test(&agents, "/usr/local/bin/fno", None, 600);
        std::fs::write(agents.join("sh.fno.pr-watcher.plist"), plist_text).unwrap();
    } else {
        std::fs::write(agents.join("sh.fno.pr-watcher.plist"), "# fixture plist\n").unwrap();
    }

    std::fs::write(state.join("events.jsonl"), "").unwrap();
    dir
}

/// The env pins both legs inherit: one config file, one state root, the
/// injected tick-in-flight answer, and the scripted launchctl steps.
fn pin_env(dir: &Path, case: &Case) -> Vec<(String, String)> {
    let mut pins = vec![
        (
            "FNO_CONFIG".into(),
            dir.join("config.toml").display().to_string(),
        ),
        (
            "FNO_STATE_DIR".into(),
            dir.join("state").display().to_string(),
        ),
        (
            "FNO_TEST_PR_WATCH_LAUNCH_AGENTS_DIR".into(),
            dir.join("LaunchAgents").display().to_string(),
        ),
    ];
    if let Some(pid) = case.tick_pid {
        pins.push(("FNO_TEST_PR_WATCH_TICK_PID".into(), pid.to_string()));
    }
    if !case.launchctl.is_empty() {
        let script: Vec<String> = case
            .launchctl
            .iter()
            .map(|(rc, timed)| format!("[{rc},{timed}]"))
            .collect();
        pins.push((
            "FNO_TEST_PR_WATCH_LAUNCHCTL".into(),
            format!("[{}]", script.join(",")),
        ));
    }
    pins
}

/// The Rust leg through the built binary: the exact surface the Python leaf
/// forwards to.
fn rust_leg(dir: &Path, case: &Case) -> Golden {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.args(["pr-watch", "refresh", "--fno-binary", "/usr/local/bin/fno"]);
    if case.force_bounce {
        cmd.arg("--force-bounce");
    }
    for (k, v) in pin_env(dir, case) {
        cmd.env(k, v);
    }
    cmd.envs(fno_agents::test_run::self_owner_env());
    cmd.current_dir(dir);
    let out = cmd.output().expect("pr-watch refresh runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    Golden {
        exit: Some(out.status.code().unwrap_or(-1)),
        streams: vec![stdout],
    }
}

/// The old leg through its own Python surface, capture mode only.
fn python_oracle(dir: &Path, case: &Case) -> Golden {
    let oracle_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr_watch_refresh/oracle.py");
    let mut cmd = Command::new(python_executable());
    cmd.arg(&oracle_script);
    cmd.arg("--case").arg(dir);
    if case.force_bounce {
        cmd.arg("--force-bounce");
    }
    for (k, v) in pin_env(dir, case) {
        cmd.env(k, v);
    }
    cmd.env("PYTHONPATH", pythonpath());
    cmd.current_dir(dir);
    let out = cmd.output().expect("oracle runs");
    // stdout + exit are the pinned contract: interpreter stderr is this
    // machine's noise.
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    Golden {
        exit: Some(out.status.code().unwrap_or(-1)),
        streams: vec![stdout],
    }
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            label: "disabled watcher skips the refresh",
            enabled: false,
            prerender: false,
            tick_pid: None,
            launchctl: vec![],
            force_bounce: false,
        },
        Case {
            label: "tick in flight defers the bounce",
            enabled: true,
            prerender: false,
            tick_pid: Some(4242),
            launchctl: vec![],
            force_bounce: false,
        },
        Case {
            label: "plain bounce runs the three launchctl steps",
            enabled: true,
            prerender: false,
            tick_pid: None,
            launchctl: vec![(0, false), (0, false), (0, false)],
            force_bounce: false,
        },
        Case {
            label: "unchanged plist skips the bounce",
            enabled: true,
            prerender: true,
            tick_pid: None,
            launchctl: vec![],
            force_bounce: false,
        },
    ]
}

#[test]
fn pr_watch_refresh_goldens() {
    let tmp = tempfile::TempDir::new().unwrap();
    for case in cases() {
        let dir = write_case(tmp.path(), &case);
        let rust = rust_leg(&dir, &case);
        let rust = Golden {
            exit: rust.exit,
            streams: rust.streams.into_iter().map(|s| masked(&s, &dir)).collect(),
        };
        let oracle = capture_mode().then(|| {
            let golden = python_oracle(&dir, &case);
            Golden {
                exit: golden.exit,
                streams: golden
                    .streams
                    .into_iter()
                    .map(|s| masked(&s, &dir))
                    .collect(),
            }
        });
        assert_golden("pr-watch-refresh", case.label, &rust, oracle);
    }
}
