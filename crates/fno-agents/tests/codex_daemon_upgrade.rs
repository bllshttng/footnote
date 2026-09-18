//! The codex daemon upgrade transaction, against the fake daemon and a fake
//! `codex` CLI. The fake CLI models the vendor verbs (`--version`,
//! `app-server daemon version`, `app-server daemon restart`): the restart
//! arm writes the upgrade marker (which flips the fake daemon's reported
//! version) and rewrites the daemon state file to name a fresh real pid,
//! exactly the observable surface a real vendor swap leaves behind. No live
//! daemon is ever touched.

use fno_agents::codex_fake_daemon::{Behavior, FakeDaemon};
use std::path::PathBuf;
use std::sync::Mutex;

/// Serializes every test that reads or writes `CODEX_HOME` / `FNO_CODEX_BIN`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Install the fake CLI at `$TMP/fakebin/codex` and point `FNO_CODEX_BIN` at
/// it. OLD is the version it reports before the marker exists (the live
/// daemon's version), NEW after (the installed CLI's version).
fn install_fake_codex(dir: &std::path::Path, old: &str, new: &str) -> PathBuf {
    let bin_dir = dir.join("fakebin");
    std::fs::create_dir_all(&bin_dir).expect("fakebin dir");
    let script = bin_dir.join("codex");
    let body = format!(
        "#!/usr/bin/env python3\nimport json, os, subprocess, sys\n\
         args = sys.argv[1:]\n\
         home = os.environ[\"CODEX_HOME\"]\n\
         marker = os.path.join(home, \"upgraded.marker\")\n\
         if args and args[0] == \"--version\":\n\
         \x20   print(\"codex-cli {new}\")\n\
         \x20   sys.exit(0)\n\
         if len(args) >= 3 and args[1] == \"daemon\" and args[2] == \"version\":\n\
         \x20   print(\"{new}\" if os.path.exists(marker) else \"{old}\")\n\
         \x20   sys.exit(0)\n\
         if len(args) >= 3 and args[1] == \"daemon\" and args[2] == \"restart\":\n\
         \x20   open(marker, \"w\").close()\n\
         \x20   child = subprocess.Popen([\"sleep\", \"30\"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)\n\
         \x20   state = os.path.join(home, \"app-server-daemon\", \"app-server.pid\")\n\
         \x20   with open(state, \"w\") as f:\n\
         \x20       json.dump({{\"pid\": child.pid, \"processStartTime\": \"defer\"}}, f)\n\
         \x20   print(child.pid)\n\
         \x20   sys.exit(0)\n\
         sys.exit(3)\n"
    );
    std::fs::write(&script, body).expect("write fake codex");
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod fake codex");
    script
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fno-upg-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos(),
    ));
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

/// The state file pid after the fake restart: kill the sleep child the
/// state names, or the test leaks a process per run.
fn kill_state_pid(home: &std::path::Path) {
    let path = home.join("app-server-daemon").join("app-server.pid");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(pid) = value.get("pid").and_then(serde_json::Value::as_u64) {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_daemon_is_reused_without_a_swap() {
    let _guard = env_guard();
    let root = temp_root("current");
    let _script = install_fake_codex(&root, "0.154.0", "0.154.0");
    unsafe { std::env::set_var("FNO_CODEX_BIN", &_script) };
    let daemon = FakeDaemon::start(
        Behavior::quick()
            .with_thread_id("thr-cur")
            .with_loaded_ids(&["thr-cur"])
            .with_server_version("0.154.0"),
    );
    let outcome = fno_agents::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    drop(daemon);
    unsafe { std::env::remove_var("FNO_CODEX_BIN") };
    match outcome {
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::ReusedCurrent { installed, live } => {
            assert_eq!(installed.as_deref(), Some("0.154.0"));
            assert_eq!(live.as_deref(), Some("0.154.0"));
        }
        other => panic!("expected reused-current, got {other:?}"),
    }
    let _ = root;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_active_thread_refuses_before_mutation() {
    let _guard = env_guard();
    let root = temp_root("refused");
    let _script = install_fake_codex(&root, "0.153.4", "0.154.0");
    unsafe { std::env::set_var("FNO_CODEX_BIN", &_script) };
    let daemon = FakeDaemon::start(
        Behavior::quick()
            .with_thread_id("thr-busy")
            .with_loaded_ids(&["thr-busy"])
            .with_server_version("0.153.4")
            .with_thread_status("active"),
    );
    let outcome = fno_agents::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    drop(daemon);
    unsafe { std::env::remove_var("FNO_CODEX_BIN") };
    match outcome {
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::Refused { reason, threads } => {
            assert!(
                reason.contains("active turn"),
                "refusal must name the active turn, got: {reason}"
            );
            assert_eq!(threads.len(), 1, "the receipt carries the snapshot");
            assert_eq!(threads[0].id, "thr-busy");
            assert_eq!(threads[0].status.as_deref(), Some("active"));
        }
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::Held {
            reason,
            installed,
            live,
            ..
        } => {
            panic!(
                "held is wrong here, refusal expected; got: {reason} (installed={installed:?}, live={live:?})"
            )
        }
        other => panic!("expected refusal, got {other:?}"),
    }
    let _ = root;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_safe_daemon_upgrades_and_preserves_every_thread() {
    let _guard = env_guard();
    let root = temp_root("upgrade");
    let _script = install_fake_codex(&root, "0.153.4", "0.154.0");
    unsafe { std::env::set_var("FNO_CODEX_BIN", &_script) };
    let daemon = FakeDaemon::start(
        Behavior::quick()
            .with_thread_id("thr-up")
            .with_loaded_ids(&["thr-up"])
            .with_server_version("0.153.4")
            .with_upgrade_marker("upgraded.marker", "0.154.0"),
    );
    let outcome = fno_agents::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    let home = daemon.socket_path().parent().expect("home").to_path_buf();
    drop(daemon);
    unsafe { std::env::remove_var("FNO_CODEX_BIN") };
    kill_state_pid(&home);
    match outcome {
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::Upgraded {
            before,
            after,
            threads,
            config_unchanged,
        } => {
            assert_eq!(
                before.get("live").and_then(|v| v.as_str()),
                Some("0.153.4"),
                "before carries the live (old) version"
            );
            assert_eq!(
                after.get("live").and_then(|v| v.as_str()),
                Some("0.154.0"),
                "after carries the new live version, equal to installed"
            );
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0].id, "thr-up");
            assert_eq!(threads[0].status.as_deref(), Some("idle"));
            assert!(
                config_unchanged,
                "no config file changed and none was written"
            );
        }
        other => panic!("expected upgraded, got {other:?}"),
    }
    let _ = root;
}
