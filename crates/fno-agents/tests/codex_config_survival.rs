//! AC25: the top-level `model_context_window` rides through the upgrade
//! transaction. Present and unchanged is the pass; a key that disappears
//! across the transaction is a FAILED transaction with `writer=unknown`,
//! never a success; an absent key is never invented.

use fno_agents::codex_fake_daemon::{Behavior, FakeDaemon};
use std::path::PathBuf;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fno-cfg-{}-{}-{}",
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

/// The fake codex CLI. The restart arm optionally strips the sizing key
/// from the private config when FNO_FAKE_STRIP_WINDOW=1: the writer that
/// removes it is unknowable to the transaction, which is exactly the
/// unattributable-removal shape AC25-ERR demands be named, never blessed.
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
         \x20   if os.environ.get(\"FNO_FAKE_STRIP_WINDOW\") == \"1\":\n\
         \x20       path = os.path.join(home, \"config.toml\")\n\
         \x20       lines = [ln for ln in open(path) if not ln.startswith(\"model_context_window\")]\n\
         \x20       open(path, \"w\").writelines(lines)\n\
         \x20   child = subprocess.Popen([\"sleep\", \"30\"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)\n\
         \x20   state = os.path.join(home, \"app-server-daemon\", \"app-server.pid\")\n\
         \x20   with open(state, \"w\") as f:\n\
         \x20       json.dump({{\"pid\": child.pid, \"processStartTime\": \"defer\"}}, f)\n\
         \x20   sys.exit(0)\n\
         sys.exit(3)\n"
    );
    std::fs::write(&script, body).expect("write fake codex");
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod fake codex");
    script
}

/// Kill the sleep child the rewritten state file names.
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

/// A private CODEX_HOME carrying a config, plus the stale fake daemon and
/// fake CLI wired for the full upgrade path. Returns (daemon, home root).
fn stale_daemon_with_config(root: &std::path::Path, config: &str) -> (FakeDaemon, PathBuf) {
    let _script = install_fake_codex(root, "0.153.4", "0.154.0");
    let script = root.join("fakebin").join("codex");
    unsafe { std::env::set_var("FNO_CODEX_BIN", &script) };
    let daemon = FakeDaemon::start(
        Behavior::quick()
            .with_thread_id("thr-cfg")
            .with_loaded_ids(&["thr-cfg"])
            .with_server_version("0.153.4")
            .with_upgrade_marker("upgraded.marker", "0.154.0"),
    );
    let home = daemon.socket_path().parent().expect("home").to_path_buf();
    let home_root = home.parent().expect("home root").to_path_buf();
    std::fs::write(home_root.join("config.toml"), config).expect("write config");
    (daemon, home_root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_window_survives_and_rides_the_receipt() {
    let _guard = env_guard();
    let root = temp_root("present");
    let (daemon, home_root) = stale_daemon_with_config(&root, "model_context_window = 1000000\n");
    let outcome = fno_agents::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    let home = home_root.clone();
    drop(daemon);
    unsafe { std::env::remove_var("FNO_CODEX_BIN") };
    kill_state_pid(&home);
    match outcome {
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::Upgraded {
            model_context_window_before,
            model_context_window_after,
            ..
        } => {
            assert_eq!(model_context_window_before.as_deref(), Some("1000000"));
            assert_eq!(model_context_window_after.as_deref(), Some("1000000"));
        }
        other => panic!("expected upgraded with the window intact, got {other:?}"),
    }
    let _ = root;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unattributable_removal_is_failed_never_success() {
    let _guard = env_guard();
    let root = temp_root("strip");
    unsafe { std::env::set_var("FNO_FAKE_STRIP_WINDOW", "1") };
    let (daemon, home_root) = stale_daemon_with_config(&root, "model_context_window = 1000000\n");
    let outcome = fno_agents::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    let home = home_root.clone();
    drop(daemon);
    unsafe {
        std::env::remove_var("FNO_CODEX_BIN");
        std::env::remove_var("FNO_FAKE_STRIP_WINDOW");
    }
    kill_state_pid(&home);
    match outcome {
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::Failed { reason, .. } => {
            assert!(
                reason.contains("model_context_window") && reason.contains("writer=unknown"),
                "the failure names the key and the unattributable writer: {reason}"
            );
        }
        other => panic!("a stripped window must read FAILED, never success; got {other:?}"),
    }
    let _ = root;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_key_is_recorded_not_invented() {
    let _guard = env_guard();
    let root = temp_root("absent");
    let (daemon, home_root) = stale_daemon_with_config(&root, "# no sizing key here\n");
    let outcome = fno_agents::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    let home = home_root.clone();
    drop(daemon);
    unsafe { std::env::remove_var("FNO_CODEX_BIN") };
    kill_state_pid(&home);
    match outcome {
        fno_agents::codex_daemon_upgrade::UpgradeOutcome::Upgraded {
            model_context_window_before,
            model_context_window_after,
            ..
        } => {
            assert_eq!(
                model_context_window_before, None,
                "absent before, and NOT invented as a default"
            );
            assert_eq!(model_context_window_after, None);
        }
        other => panic!("expected upgraded with the key absent, got {other:?}"),
    }
    let _ = root;
}
