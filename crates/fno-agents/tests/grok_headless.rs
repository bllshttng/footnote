use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "fno-grok-headless-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn client_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_fno-agents") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/debug/fno-agents")
}

#[test]
fn grok_headless_fails_closed_until_live_create_and_resume_are_measured() {
    let base = temp_dir("unmeasured");
    let bin_dir = base.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let marker = base.join("grok-ran");
    let grok = bin_dir.join("grok");
    fs::write(&grok, format!("#!/bin/sh\ntouch {:?}\n", marker)).unwrap();
    fs::set_permissions(&grok, fs::Permissions::from_mode(0o755)).unwrap();

    let cwd = temp_dir("cwd");
    let home = temp_dir("home");
    let bin = client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping: fno-agents binary unavailable at {}",
            bin.display()
        );
        return;
    }
    let output = Command::new(bin)
        .envs(fno_agents::test_run::self_owner_env())
        .args([
            "spawn",
            "grok-worker",
            "hello",
            "--harness",
            "grok",
            "--substrate",
            "headless",
        ])
        .arg("--cwd")
        .arg(cwd)
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1")
        .env("FNO_AGENTS_HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
        .output()
        .expect("run fno-agents spawn");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("headless_create"), "{stderr}");
    assert!(stderr.contains("unsupported"), "{stderr}");
    assert!(
        !marker.exists(),
        "an unmeasured Grok lane must not start the CLI"
    );
}
