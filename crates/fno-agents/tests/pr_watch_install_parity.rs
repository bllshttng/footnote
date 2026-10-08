//! parity-stage: characterization
//! parity-oracle: cli/src/fno/pr_watch/_install.py:install
//!
//! Characterization for the pr-watch install port: the Rust verb
//! (`fno-agents pr-watch install`, plus the `--ensure` coupling mode) is
//! pinned by the frozen goldens under tests/golden/pr-watch-install/. While
//! the Python leg lived, this file ran as a LIVE differential under
//! `FNO_CAPTURE_GOLDEN=1`: both implementations answered the same fixture
//! with the same env pins and the run asserted byte-identity of the stdout
//! before the goldens froze. The Python `install` / `ensure_activated` /
//! `retire_legacy_postmerge_agents` functions were deleted in the same
//! change that landed the port, so the goldens stand as the contract and
//! capture mode refuses.
//!
//! Ten fixture states, the ones the plan named: the dry-run render, the
//! confirmed write + bounce + legacy retirement, the declined gate, the
//! no-activate escape, a loud activation failure, and the four
//! `ensure_activated` outcome words, plus the already-running short circuit
//! and a legacy retirement through the coupling path. Every volatile byte
//! (the case dir, the launchd uid) goes through a mask first - the golden
//! describes the rule tree, not this machine.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{assert_golden, capture_mode, make_script, slug, Golden};

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

/// The only moving parts: the fixture root and the launchd uid in bounce
/// messages. Both legs mint them from the same pins.
fn masked(text: &str, dir: &Path) -> String {
    let uid = regex::Regex::new(r"gui/\d+").unwrap();
    let rooted = text.replace(&dir.display().to_string(), "<CASE>");
    uid.replace_all(&rooted, "gui/<UID>").into_owned()
}

/// What the launchctl stub answers, exported as `FNO_STUB_*` env so both
/// legs' subprocesses see the same fake.
struct LaunchCtl {
    list: Option<&'static str>,
    load_rc: i32,
    bootstrap_fails: u32,
    bootout_rc: i32,
    kickstart_rc: i32,
}

impl Default for LaunchCtl {
    fn default() -> Self {
        LaunchCtl {
            list: None,
            load_rc: 0,
            bootstrap_fails: 0,
            bootout_rc: 0,
            kickstart_rc: 0,
        }
    }
}

struct Case {
    label: &'static str,
    mode: Mode,
    interval: i64,
    model: bool,
    stdin: Option<&'static str>,
    legacy: bool,
    /// Replace the LaunchAgents dir with a file: the write-failed shape.
    agents_dir_is_file: bool,
    launchctl: LaunchCtl,
}

enum Mode {
    Install { dry_run: bool, no_activate: bool },
    Ensure,
}

fn write_case(root: &Path, case: &Case) -> PathBuf {
    let dir = root.join(slug(case.label));
    let home = dir.join("home");
    let agents = home.join("Library").join("LaunchAgents");
    let state = dir.join("state");
    let stub = dir.join("stub");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&stub).unwrap();

    let mut config = format!("state_dir = \"{}\"\n\n", state.display());
    config.push_str("[pr_watch]\n");
    config.push_str("enabled = true\n");
    config.push_str("interval_seconds = 600\n");
    std::fs::write(dir.join("config.toml"), config).unwrap();

    if case.agents_dir_is_file {
        std::fs::write(&agents, "not a directory").unwrap();
    } else {
        std::fs::create_dir_all(&agents).unwrap();
    }

    if case.legacy {
        std::fs::write(
            agents.join("com.fno.postmerge-old.plist"),
            "<plist><dict/></plist>",
        )
        .unwrap();
    }

    // The launchctl stub both legs resolve through PATH. The bootstrap-fails
    // counter lives in the case dir so retries never leak between cases.
    make_script(
        &stub,
        "launchctl",
        r#"
case "$1" in
  list) [ -n "$FNO_STUB_LIST" ] && echo "$FNO_STUB_LIST" ; exit 0 ;;
  bootout) exit ${FNO_STUB_BOOTOUT_RC:-0} ;;
  bootstrap)
    if [ -n "$FNO_STUB_BOOTSTRAP_FAILS" ]; then
      c=$(cat "$FNO_STUB_STATE/bootstrap_count" 2>/dev/null || echo 0)
      c=$((c+1))
      echo "$c" > "$FNO_STUB_STATE/bootstrap_count"
      if [ "$c" -le "$FNO_STUB_BOOTSTRAP_FAILS" ]; then exit 5; fi
    fi
    exit ${FNO_STUB_BOOTSTRAP_RC:-0} ;;
  kickstart) exit ${FNO_STUB_KICKSTART_RC:-0} ;;
  load) exit ${FNO_STUB_LOAD_RC:-0} ;;
  *) exit 0 ;;
esac
"#,
    );
    // The fno binary path the plist carries: a stub, so the rendered
    // ProgramArguments stay inside the masked case dir.
    make_script(&stub, "fno-py", "exit 0");

    dir
}

/// The env pins both legs inherit: one home, one state root, one config, the
/// stub PATH, and the launchctl answers.
fn pin_env(dir: &Path, case: &Case) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".into(), dir.join("home").display().to_string()),
        (
            "FNO_CONFIG".into(),
            dir.join("config.toml").display().to_string(),
        ),
        (
            "FNO_STATE_DIR".into(),
            dir.join("state").display().to_string(),
        ),
        ("CARGO_HOME".into(), dir.join("cargo").display().to_string()),
        (
            "FNO_TEST_PR_WATCH_LAUNCH_AGENTS_DIR".into(),
            dir.join("home")
                .join("Library")
                .join("LaunchAgents")
                .display()
                .to_string(),
        ),
        (
            "FNO_TEST_FNO_BINARY".into(),
            dir.join("stub").join("fno-py").display().to_string(),
        ),
        // Keep the deployed fno and any real launchctl out of reach; the stub
        // dir answers both.
        (
            "PATH".into(),
            format!("{}:/usr/bin:/bin:/usr/sbin", dir.join("stub").display()),
        ),
        ("FNO_STUB_STATE".into(), dir.display().to_string()),
        (
            "FNO_STUB_LIST".into(),
            case.launchctl.list.unwrap_or("").to_string(),
        ),
        (
            "FNO_STUB_LOAD_RC".into(),
            case.launchctl.load_rc.to_string(),
        ),
        (
            "FNO_STUB_BOOTSTRAP_FAILS".into(),
            case.launchctl.bootstrap_fails.to_string(),
        ),
        (
            "FNO_STUB_BOOTOUT_RC".into(),
            case.launchctl.bootout_rc.to_string(),
        ),
        (
            "FNO_STUB_KICKSTART_RC".into(),
            case.launchctl.kickstart_rc.to_string(),
        ),
        // The Python store client resolves the binary through this pin, so
        // the bounce event commits into the fixture's own store.
        (
            "FNO_AGENTS_BIN".into(),
            env!("CARGO_BIN_EXE_fno-agents").to_string(),
        ),
    ];
    if case.launchctl.bootstrap_fails > 0 {
        env.push(("FNO_STUB_BOOTSTRAP_RC".into(), "5".into()));
    }
    env
}

fn case_args(case: &Case) -> Vec<String> {
    let Mode::Install {
        dry_run,
        no_activate,
    } = &case.mode
    else {
        return vec!["--ensure".to_string()];
    };
    let mut argv: Vec<String> = Vec::new();
    if *dry_run {
        argv.push("--dry-run".to_string());
    }
    if case.interval > 0 {
        argv.push("--interval".to_string());
        argv.push(case.interval.to_string());
    }
    if case.model {
        argv.push("--model".to_string());
        argv.push("zz".to_string());
    }
    if *no_activate {
        argv.push("--no-activate".to_string());
    }
    argv
}

/// The Rust leg through the built binary: the exact surface the Python leaf
/// forwards to.
fn rust_leg(dir: &Path, case: &Case) -> Golden {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.args(["pr-watch", "install"]);
    for arg in case_args(case) {
        cmd.arg(arg);
    }
    for (k, v) in pin_env(dir, case) {
        cmd.env(k, v);
    }
    cmd.envs(fno_agents::test_run::self_owner_env());
    cmd.current_dir(dir);
    // All three handles piped: wait_with_output captures only piped handles,
    // and an inherited stdout would land the verb's bytes on the test console
    // with an empty Output.stdout.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("pr-watch install runs");
    if let Some(answer) = case.stdin {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(answer.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("pr-watch install finishes");
    Golden {
        exit: Some(out.status.code().unwrap_or(-1)),
        streams: vec![String::from_utf8_lossy(&out.stdout).to_string()],
    }
}

/// The old leg through its own Python surface, capture mode only.
fn python_oracle(dir: &Path, case: &Case) -> Golden {
    let oracle_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr_watch_install/oracle.py");
    let mut cmd = Command::new(python_executable());
    cmd.arg(&oracle_script);
    cmd.arg("--case").arg(dir);
    match &case.mode {
        Mode::Install {
            dry_run,
            no_activate,
        } => {
            cmd.args(["--mode", "install"]);
            if *dry_run {
                cmd.arg("--dry-run");
            }
            if *no_activate {
                cmd.arg("--no-activate");
            }
        }
        Mode::Ensure => {
            cmd.args(["--mode", "ensure"]);
        }
    }
    if case.interval > 0 {
        cmd.args(["--interval", &case.interval.to_string()]);
    }
    for (k, v) in pin_env(dir, case) {
        cmd.env(k, v);
    }
    cmd.env("PYTHONPATH", pythonpath());
    cmd.current_dir(dir);
    // Same three-pipe rule as the Rust leg: unpiped handles read as empty.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("oracle runs");
    if let Some(answer) = case.stdin {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(answer.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("oracle finishes");
    // stdout + exit are the pinned contract: interpreter stderr is this
    // machine's noise.
    Golden {
        exit: Some(out.status.code().unwrap_or(-1)),
        streams: vec![String::from_utf8_lossy(&out.stdout).to_string()],
    }
}

fn cases() -> Vec<Case> {
    vec![
        // Dry run: the full render with a non-default interval, and a --model
        // the verb must tolerate.
        Case {
            label: "dry run renders the plist",
            mode: Mode::Install {
                dry_run: true,
                no_activate: false,
            },
            interval: 1234,
            model: true,
            stdin: None,
            legacy: false,
            agents_dir_is_file: false,
            launchctl: LaunchCtl::default(),
        },
        // The confirmed path: write, bounce, legacy retirement, heal line.
        Case {
            label: "confirmed install bounces and retires legacy",
            mode: Mode::Install {
                dry_run: false,
                no_activate: false,
            },
            interval: 0,
            model: false,
            stdin: Some("y\n"),
            legacy: true,
            agents_dir_is_file: false,
            launchctl: LaunchCtl::default(),
        },
        Case {
            label: "declined install writes nothing",
            mode: Mode::Install {
                dry_run: false,
                no_activate: false,
            },
            interval: 0,
            model: false,
            stdin: Some("n\n"),
            legacy: false,
            agents_dir_is_file: false,
            launchctl: LaunchCtl::default(),
        },
        Case {
            label: "no activate install defers to launchctl bootstrap",
            mode: Mode::Install {
                dry_run: false,
                no_activate: true,
            },
            interval: 0,
            model: false,
            stdin: Some("y\n"),
            legacy: false,
            agents_dir_is_file: false,
            launchctl: LaunchCtl::default(),
        },
        // The stub bootstrap refuses four times, so the retry ladder exhausts
        // and the WARNING line prints with the manual remedy.
        Case {
            label: "activation failure is loud",
            mode: Mode::Install {
                dry_run: false,
                no_activate: false,
            },
            interval: 0,
            model: false,
            stdin: Some("y\n"),
            legacy: false,
            agents_dir_is_file: false,
            launchctl: LaunchCtl {
                bootstrap_fails: 4,
                ..LaunchCtl::default()
            },
        },
        Case {
            label: "ensure already running",
            mode: Mode::Ensure,
            interval: 0,
            model: false,
            stdin: None,
            legacy: false,
            agents_dir_is_file: false,
            launchctl: LaunchCtl {
                list: Some("sh.fno.pr-watcher"),
                ..LaunchCtl::default()
            },
        },
        Case {
            label: "ensure activates and retires legacy",
            mode: Mode::Ensure,
            interval: 0,
            model: false,
            stdin: None,
            legacy: true,
            agents_dir_is_file: false,
            launchctl: LaunchCtl::default(),
        },
        Case {
            label: "ensure reports load failure",
            mode: Mode::Ensure,
            interval: 0,
            model: false,
            stdin: None,
            legacy: false,
            agents_dir_is_file: false,
            launchctl: LaunchCtl {
                load_rc: 9,
                ..LaunchCtl::default()
            },
        },
        Case {
            label: "ensure reports write failure",
            mode: Mode::Ensure,
            interval: 0,
            model: false,
            stdin: None,
            legacy: false,
            agents_dir_is_file: true,
            launchctl: LaunchCtl::default(),
        },
    ]
}

#[test]
fn pr_watch_install_goldens() {
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
        assert_golden("pr-watch-install", case.label, &rust, oracle);
    }
}

/// The ensure coupling path retires legacy agents from disk even though its
/// only output is the outcome word: the receipt-less cleanup the Python leg
/// performed is preserved.
#[test]
fn ensure_retires_the_legacy_plist_from_disk() {
    let tmp = tempfile::TempDir::new().unwrap();
    let case = Case {
        label: "ensure legacy disk check",
        mode: Mode::Ensure,
        interval: 0,
        model: false,
        stdin: None,
        legacy: true,
        agents_dir_is_file: false,
        launchctl: LaunchCtl::default(),
    };
    let dir = write_case(tmp.path(), &case);
    let legacy = dir
        .join("home")
        .join("Library")
        .join("LaunchAgents")
        .join("com.fno.postmerge-old.plist");
    assert!(legacy.is_file());
    let golden = rust_leg(&dir, &case);
    assert_eq!(golden.exit, Some(0));
    assert_eq!(golden.streams[0].trim(), "activated");
    assert!(!legacy.exists(), "legacy plist must be retired");
}
