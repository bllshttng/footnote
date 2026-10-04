//! parity-stage: characterization
//! parity-oracle: cli/src/fno/pr_watch/_install.py:liveness_report
//!
//! Characterization for the pr-watch status port: the Rust verb
//! (`fno-agents pr-watch status`, reached through `pr_watch::status`) is
//! pinned by the frozen goldens under tests/golden/pr-watch-status/. While
//! the Python leg lived, this file ran as a LIVE differential under
//! `FNO_CAPTURE_GOLDEN=1`: both implementations answered the same fixture
//! with the same env pins and the run asserted byte-identity of the masked
//! JSON and the masked human readout before the goldens froze. The Python
//! leg (`liveness_report`, `liveness_report_live`, and the `status` renderer
//! in `_install.py`) was deleted in the same change that landed the port, so
//! the goldens stand as the contract and capture mode refuses.
//!
//! Five fixture states, the ones the plan named: no plist (dead), a plist
//! loaded with a fresh tick (healthy), three broken tick ends under a fresh
//! bounce (wedged), a stale last tick (dead), and the watcher disabled.
//! Fixture timestamps are minted at test time relative to now, so every
//! golden is rendered through a mask that replaces RFC3339 stamps with
//! `<TS>` and relative ages (`Ns ago`) with `<AGE>s ago` first - the golden
//! describes the rule tree, not this run's clock.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{assert_golden, capture_mode, slug, Golden};
use regex::Regex;

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

/// RFC3339 stamps and relative ages are the only moving parts: both legs
/// mint them at run time from the same fixture file.
fn masked(text: &str, dir: &Path) -> String {
    let ts = Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(\+00:00|Z)").unwrap();
    let age = Regex::new(r"\b\d{1,6}s ago").unwrap();
    let rooted = text.replace(&dir.display().to_string(), "<CASE>");
    let stamped = ts.replace_all(&rooted, "<TS>").into_owned();
    age.replace_all(&stamped, "<AGE>s ago").into_owned()
}

fn iso(epoch: u64) -> String {
    chrono::DateTime::from_timestamp(epoch as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// One fixture case: a config file pinning `FNO_CONFIG`-style state, an
/// events journal, the watcher state stores, and a LaunchAgents dir holding
/// (or missing) the plist.
struct Case {
    label: &'static str,
    enabled: bool,
    plist: bool,
    /// Backdated plist mtime, seconds before now.
    plist_age: u64,
    events: String,
}

fn fixture_events(now: u64, shape: &str) -> String {
    fn tick(rows: &mut Vec<String>, now: u64, offset: u64, data: &str) {
        rows.push(format!(
            r#"{{"type":"pr_watch_tick","ts":"{}","data":{data}}}"#,
            iso(now - offset)
        ));
    }
    fn attempt(rows: &mut Vec<String>, now: u64, offset: u64) {
        rows.push(format!(
            r#"{{"type":"pr_watch_tick_attempt","ts":"{}","data":{{}}}}"#,
            iso(now - offset)
        ));
    }
    fn end(rows: &mut Vec<String>, now: u64, offset: u64, outcome: &str) {
        rows.push(format!(
            r#"{{"type":"pr_watch_tick_end","ts":"{}","data":{{"outcome":"{outcome}","phase":"sweep","duration_s":12.3}}}}"#,
            iso(now - offset)
        ));
    }
    let mut rows: Vec<String> = Vec::new();
    match shape {
        "fresh" => {
            tick(
                &mut rows,
                now,
                60,
                r#"{"swept_count":1,"swept":{"fixture/other":[7]},"merge_scan":{"completed":true,"scanned":3}}"#,
            );
            attempt(&mut rows, now, 61);
            end(&mut rows, now, 62, "ok");
        }
        "wedged" => {
            tick(
                &mut rows,
                now,
                800,
                r#"{"swept_count":1,"swept":{"fixture/other":[7]}}"#,
            );
            attempt(&mut rows, now, 801);
            end(&mut rows, now, 700, "timeout");
            end(&mut rows, now, 699, "timeout");
            end(&mut rows, now, 698, "timeout");
        }
        "dead" => {
            tick(
                &mut rows,
                now,
                7200,
                r#"{"swept_count":1,"swept":{"fixture/other":[7]}}"#,
            );
            attempt(&mut rows, now, 7201);
            end(&mut rows, now, 7202, "ok");
        }
        _ => {}
    }
    rows.join("\n") + "\n"
}

fn write_case(root: &Path, case: &Case, now: u64) -> PathBuf {
    let dir = root.join(slug(case.label));
    let agents = dir.join("LaunchAgents");
    let state = dir.join("state");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir_all(state.join("logs")).unwrap();

    // state_dir is a TOP-LEVEL key: under a [table] header the config loader
    // reads it as pr_watch.state_dir and warns it is unmodeled.
    let mut config = format!("state_dir = \"{}\"\n\n", state.display());
    config.push_str("[pr_watch]\n");
    config.push_str(&format!("enabled = {}\n", case.enabled));
    config.push_str("interval_seconds = 600\n");
    config.push_str("wedged_after_ticks = 3\n");
    std::fs::write(dir.join("config.toml"), config).unwrap();

    if case.plist {
        let plist = agents.join("sh.fno.pr-watcher.plist");
        std::fs::write(&plist, "# fixture plist\n").unwrap();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&plist)
            .unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(now - case.plist_age)),
        )
        .unwrap();
    }

    std::fs::write(state.join("events.jsonl"), &case.events).unwrap();

    let store = format!(
        r#"{{"fixture/other#123":{{"last_seen_state":"OPEN","slug":"fixture/other","parked":"why","last_polled_at":null}}}}"#
    );
    std::fs::write(state.join("pr-watcher-state.json"), store).unwrap();
    let delivery = format!(
        r#"{{"fixture/other#456":{{"last_seen_state":"MERGED","slug":"fixture/other","parked":"done","last_polled_at":null}}}}"#
    );
    std::fs::write(state.join("pr-watcher-state-delivery.json"), delivery).unwrap();
    dir
}

/// The env pins both legs inherit: one config file, one state root, the
/// built binary for the parked read, and the injected load state. The PATH
/// pin keeps the deployed `fno` out of reach: the Python store reader would
/// otherwise hand the fixture journal to the installed binary, whose store
/// answer depends on the deployed build rather than this tree, and the
/// watermark fold would diverge from the Rust leg's own read. With no native
/// binary reachable, both legs read the same raw bytes.
fn pin_env(dir: &Path) -> Vec<(String, String)> {
    vec![
        (
            "FNO_CONFIG".into(),
            dir.join("config.toml").display().to_string(),
        ),
        (
            "FNO_STATE_DIR".into(),
            dir.join("state").display().to_string(),
        ),
        (
            "FNO_AGENTS_BIN".into(),
            env!("CARGO_BIN_EXE_fno-agents").to_string(),
        ),
        ("FNO_PR_WATCH_TEST_LOADED".into(), "1".into()),
        (
            "FNO_PR_WATCH_TEST_LAUNCH_AGENTS_DIR".into(),
            dir.join("LaunchAgents").display().to_string(),
        ),
        ("PATH".into(), "/usr/bin:/bin:/usr/sbin".into()),
    ]
}

/// The Rust leg through the built binary: the exact surface the Python leaf
/// forwards to.
fn rust_leg(dir: &Path, json: bool) -> Golden {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    if json {
        cmd.args(["pr-watch", "status", "--json"]);
    } else {
        cmd.args(["pr-watch", "status"]);
    }
    for (k, v) in pin_env(dir) {
        cmd.env(k, v);
    }
    cmd.envs(fno_agents::test_run::self_owner_env());
    cmd.current_dir(dir);
    let out = cmd.output().expect("pr-watch status runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let mut streams = vec![stdout];
    if !stderr.is_empty() {
        streams.push(stderr);
    }
    Golden {
        exit: Some(out.status.code().unwrap_or(-1)),
        streams,
    }
}

/// The old leg through its own Python surface, capture mode only.
fn python_oracle(dir: &Path, json: bool) -> Golden {
    let oracle_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr_watch_status/oracle.py");
    let mut cmd = Command::new(python_executable());
    cmd.arg(&oracle_script);
    if json {
        cmd.args(["--mode", "json"]);
    } else {
        cmd.args(["--mode", "text"]);
    }
    cmd.arg("--case").arg(dir);
    for (k, v) in pin_env(dir) {
        cmd.env(k, v);
    }
    cmd.env("PYTHONPATH", pythonpath());
    cmd.current_dir(dir);
    let out = cmd.output().expect("oracle runs");
    // stdout + exit are the pinned contract: interpreter stderr is this
    // machine's noise, and the fixtures keep the STALE path off.
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    Golden {
        exit: Some(out.status.code().unwrap_or(-1)),
        streams: vec![stdout],
    }
}

fn cases(now: u64) -> Vec<Case> {
    vec![
        Case {
            label: "no plist reads dead",
            enabled: true,
            plist: false,
            plist_age: 0,
            events: fixture_events(now, "fresh"),
        },
        Case {
            label: "fresh tick reads healthy",
            enabled: true,
            plist: true,
            plist_age: 120,
            events: fixture_events(now, "fresh"),
        },
        Case {
            label: "bounce over three broken ticks reads wedged",
            enabled: true,
            plist: true,
            plist_age: 0,
            events: fixture_events(now, "wedged"),
        },
        Case {
            label: "stale last tick reads dead",
            enabled: true,
            plist: true,
            plist_age: 7300,
            events: fixture_events(now, "dead"),
        },
        Case {
            label: "disabled watcher reads disabled",
            enabled: false,
            plist: true,
            plist_age: 120,
            events: String::new(),
        },
    ]
}

#[test]
fn pr_watch_status_goldens() {
    let tmp = tempfile::TempDir::new().unwrap();
    let now = now_unix();
    for case in cases(now) {
        let dir = write_case(tmp.path(), &case, now);
        for json in [true, false] {
            let mode = if json { "json" } else { "text" };
            let label = format!("{} {mode}", case.label);
            let rust = rust_leg(&dir, json);
            let rust = Golden {
                exit: rust.exit,
                streams: rust.streams.into_iter().map(|s| masked(&s, &dir)).collect(),
            };
            let oracle = capture_mode().then(|| {
                let golden = python_oracle(&dir, json);
                Golden {
                    exit: golden.exit,
                    streams: golden
                        .streams
                        .into_iter()
                        .map(|s| masked(&s, &dir))
                        .collect(),
                }
            });
            assert_golden("pr-watch-status", &label, &rust, oracle);
        }
    }
}
