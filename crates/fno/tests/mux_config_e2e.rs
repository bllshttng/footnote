//! Isolation e2e (x-f02b): the mux socket dir must follow the config chain,
//! so `FNO_CONFIG` pointing at an isolated demo config never reaches the real
//! fleet's sockets under `$HOME/.fno/mux`.
//!
//! The positive marker matters more than the absence: each case plants a
//! session socket INSIDE the demo state root and requires `ls` to NAME it.
//! A listing that reads the wrong dir and finds nothing would pass a bare
//! "does not mention main" assertion; naming the planted session proves the
//! instrument ran against the demo dir. The fake fleet `main.sock` planted
//! under the scratch HOME is what the pre-fix binary printed (it resolved
//! `$HOME/.fno/mux` unconditionally), which is the leak this test pins shut.

mod common;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use common::Scratch;

/// A socket file nobody answers: bind, then drop the listener without
/// unlinking. `ls` probes it as REFUSED and reports the session as `stale`.
fn plant_socket(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    drop(UnixListener::bind(path).unwrap());
}

/// The fake "real fleet" session socket, under the scratch HOME exactly where
/// the pre-fix mux_dir resolved.
fn home_fleet_sock(scratch: &Scratch) -> PathBuf {
    scratch
        .0
        .join("home")
        .join(".fno")
        .join("mux")
        .join("main.sock")
}

struct Case {
    /// Output of `fno mux ls --json`.
    stdout: String,
    stderr: String,
}

/// Run `fno mux ls --json` with FNO_MUX_DIR stripped (so the config chain
/// decides), HOME pointed at the fake fleet home, and FNO_CONFIG at `config`.
fn ls_under_config(scratch: &Scratch, config: &Path) -> Case {
    let mut cmd = scratch.command();
    cmd.env_remove("FNO_MUX_DIR");
    cmd.env("FNO_CONFIG", config);
    cmd.current_dir(scratch.0.join("demo"));
    let out = cmd.args(["mux", "ls", "--json"]).output().unwrap();
    Case {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The session names `ls --json` reported. Parsed per row rather than
/// substring-matched, so a future field carrying the string "main" (a path,
/// a version stamp) cannot trip the leak assertions.
fn session_names(stdout: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v.as_array().map(|rows| rows.to_vec()))
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get("session").and_then(|s| s.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn mux_ls_under_demo_config_names_the_demo_session_not_the_home_fleet() {
    let scratch = Scratch::new("muxcfg");
    // The fake "real fleet": a session socket under $HOME, exactly where the
    // pre-fix mux_dir resolved unconditionally.
    plant_socket(&home_fleet_sock(&scratch));
    // The isolated demo state root, named by the config chain.
    let demo = scratch.0.join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let config = demo.join("config.toml");
    std::fs::write(
        &config,
        format!("schema_version = 1\nstate_dir = {:?}\n", demo),
    )
    .unwrap();
    plant_socket(&demo.join("mux").join("isolated-session.sock"));

    let case = ls_under_config(&scratch, &config);
    let names = session_names(&case.stdout);
    assert!(
        names.iter().any(|n| n == "isolated-session"),
        "ls must name the demo session; got stdout: {} stderr: {}",
        case.stdout,
        case.stderr
    );
    assert!(
        !names.iter().any(|n| n == "main"),
        "the HOME fleet session leaked through FNO_CONFIG isolation: {}",
        case.stdout
    );
}

#[test]
fn mux_ls_fail_closed_when_fno_config_cannot_be_parsed() {
    // A yaml-pinned FNO_CONFIG is legal for Python (parsed by suffix) but
    // unreadable for this crate's TOML reader. Isolation was still REQUESTED,
    // so the mux dir must land beside the pinned file, never back on
    // $HOME/.fno/mux.
    let scratch = Scratch::new("muxcfgy");
    plant_socket(&home_fleet_sock(&scratch));
    let demo = scratch.0.join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let config = demo.join("config.yaml");
    std::fs::write(&config, "state_dir: /nowhere-relevant\n").unwrap();
    plant_socket(&demo.join("mux").join("yaml-session.sock"));

    let case = ls_under_config(&scratch, &config);
    let names = session_names(&case.stdout);
    assert!(
        names.iter().any(|n| n == "yaml-session"),
        "ls must name the session beside the pinned config; got stdout: {} stderr: {}",
        case.stdout,
        case.stderr
    );
    assert!(
        !names.iter().any(|n| n == "main"),
        "an unparseable FNO_CONFIG fell back to the HOME fleet: {}",
        case.stdout
    );
}

#[test]
fn squad_store_reads_the_legacy_file_under_a_global_state_dir() {
    // The upgrading-user path: state_dir comes from the GLOBAL config tier
    // (the FNO_GLOBAL_SETTINGS_PATH sibling, no FNO_CONFIG pin), the resolved
    // root has no squads.json yet, and the operator's old store sits at the
    // HOME default. The READ must fall back and COUNT the squads (a positive
    // marker: a broken fallback reads as "no squads persisted"). The WRITE
    // seeding shares this same legacy_read, and cannot run from a build-tree
    // binary by the store's own guard, so the write path is covered by that
    // sharing rather than an exec.
    let scratch = Scratch::new("muxsqd");
    let demo = scratch.0.join("demo");
    std::fs::create_dir_all(demo.join("mux")).unwrap();
    // The global tier: the config.toml SIBLING of a settings.yaml pin (the
    // exact basename Python's _prefer_toml substitutes for, so Scratch's
    // default settings.json pin is overridden here).
    let global_cfg = scratch.0.join("iso-cfg");
    std::fs::create_dir_all(&global_cfg).unwrap();
    std::fs::write(
        global_cfg.join("config.toml"),
        format!("state_dir = {:?}\n", demo),
    )
    .unwrap();
    // The operator's old store at the HOME default (<home>/.fno/squads.json,
    // a SIBLING of the mux dir): a named squad plus an attach-born one whose
    // origin is gone.
    let legacy = scratch.0.join("home").join(".fno").join("squads.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(
        &legacy,
        r#"{"version":1,"squads":[
            {"name":"upgrading-user-squad"},
            {"name":"","key":"dead","origins":["/gone-origin"],"members":[]}
        ]}"#,
    )
    .unwrap();

    // Both explicit overrides must come OFF: FNO_MUX_DIR would relocate the
    // sockets, FNO_AGENTS_HOME would point the store at its own root, and
    // either one legitimately disables the ambient fallback under test.
    let mut cmd = scratch.command();
    cmd.env_remove("FNO_MUX_DIR");
    cmd.env_remove("FNO_AGENTS_HOME");
    cmd.env(
        "FNO_GLOBAL_SETTINGS_PATH",
        scratch.0.join("iso-cfg").join("settings.yaml"),
    );
    cmd.current_dir(&demo);
    let out = cmd.args(["mux", "doctor"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("1 orphaned squad(s)"),
        "both legacy squads must load through the fallback; got: {stdout}"
    );
}

#[test]
fn a_global_yaml_state_dir_warns_only_when_it_diverges() {
    // The yaml hint compares VALUES, not substrings: a yaml naming the
    // default root is agreement (the explicit pre-migration spelling), and
    // warning there would fire on every verb forever. A yaml naming a
    // different root IS the divergence the warning exists for - the
    // positive marker that keeps this test from passing vacuously.
    let agree = Scratch::new("muxymla");
    std::fs::create_dir_all(agree.0.join("home").join(".fno")).unwrap();
    std::fs::write(
        agree.0.join("home").join(".fno").join("settings.yaml"),
        format!(
            "state_dir: {}\n",
            agree.0.join("home").join(".fno").display()
        ),
    )
    .unwrap();
    let mut cmd = agree.command();
    cmd.env_remove("FNO_MUX_DIR");
    cmd.env_remove("FNO_GLOBAL_SETTINGS_PATH");
    let out = cmd.args(["mux", "ls"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("cannot read"),
        "a yaml agreeing with the default root must not warn: {stderr}"
    );

    let diverge = Scratch::new("muxymld");
    std::fs::create_dir_all(diverge.0.join("home").join(".fno")).unwrap();
    std::fs::write(
        diverge.0.join("home").join(".fno").join("settings.yaml"),
        format!("state_dir: {}\n", diverge.0.join("elsewhere").display()),
    )
    .unwrap();
    let mut cmd = diverge.command();
    cmd.env_remove("FNO_MUX_DIR");
    cmd.env_remove("FNO_GLOBAL_SETTINGS_PATH");
    let out = cmd.args(["mux", "ls"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("state_dir this mux cannot read"),
        "a diverging yaml must warn: {stderr}"
    );
}
