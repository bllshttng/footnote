use fno::process_admission::ADMISSION_ACCEPTED;
use fno::process_admission::{
    configured_max_processes, decide_panes, decide_processes, Census, MaxPanes, MaxProcesses,
    PaneCount, Scope,
};
use std::process::Stdio;
use std::sync::{Arc, Barrier, Mutex, OnceLock};

static ADMISSION_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// These tests exercise the agent path. A human at a terminal is always
/// admitted, so a worker identity keeps a developer's tty run on the gate.
fn isolate_admission_state() {
    std::env::set_var("FNO_AGENT_SELF", "admission-e2e");
    std::env::set_var("FNO_E2E", "1");
    std::env::set_var(
        "FNO_MUX_ADMISSION_NAMESPACE",
        format!("process-admission-{}", std::process::id()),
    );
}

#[test]
fn ac1_hp_sync_output_preserves_implicit_capture() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "512");
    let mut command = fno::process_admission::std_command("printf");
    command.arg("sync-capture");
    let output = fno::process_admission::std_output(&mut command).unwrap();
    restore_max_processes(previous);
    assert_eq!(output.stdout, b"sync-capture");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn ac1_hp_async_output_preserves_implicit_capture() {
    isolate_admission_state();
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    let output = {
        let _env_lock = ADMISSION_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "512");
        let mut command = fno::process_admission::tokio_command("printf");
        command.arg("async-capture");
        fno::process_admission::tokio_output(&mut command)
            .await
            .unwrap()
    };
    restore_max_processes(previous);
    assert_eq!(output.stdout, b"async-capture");
}

#[test]
fn ac4_neg_refuses_incomplete_snapshot_without_substituting_zero() {
    let decision = decide_processes(
        &Census::unavailable("worker root discovery unavailable"),
        MaxProcesses::new(2),
    );

    assert_eq!(
        decision.refusal(),
        Some(
            "process admission refused: count=unknown ceiling=2 scope=fleet reason=measurement-unavailable"
                .to_string(),
        )
    );
}

#[test]
fn ac9_edge_applies_tab_ceiling_as_a_separate_scope() {
    let decision = decide_panes(PaneCount::new(4), MaxPanes::new(4));

    assert_eq!(decision.scope(), Some(Scope::Tab));
    let refusal = decision.refusal().expect("over the tab cap refuses");
    // No refusal carries a bypass hint: the gate has no operator-facing
    // recovery switch to advertise (the machine arm self-expires).
    assert!(!refusal.contains("FNO_PROCESS_ADMISSION"), "{refusal}");
}

#[test]
fn ac2_err_creation_path_emits_positive_refusal_marker() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    let previous_brake = std::env::var_os("FNO_MACHINE_BRAKE");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "2");
    // A live machine brake would outrank the census, so point the brake at
    // an absent file and let the ceiling arm speak.
    std::env::set_var(
        "FNO_MACHINE_BRAKE",
        std::env::temp_dir().join(format!("brake-absent-{}.json", std::process::id())),
    );

    // The default door never measures, so the ceiling is forced here the
    // only way left: children recorded by the admitted default spawns fill
    // the census the agent-spawn door reads.
    let mut children = Vec::new();
    for attempt in 0..2 {
        println!("spawn attempted index={attempt}");
        let mut command = fno::process_admission::std_command("sleep");
        command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        children.push(fno::process_admission::std_spawn(&mut command).expect("the default admits"));
    }

    let outcome = fno::process_admission::admit_agent_spawn();

    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
    restore_max_processes(previous);
    restore_env("FNO_MACHINE_BRAKE", previous_brake);

    let error = outcome
        .err()
        .expect("a full fleet holds the agent-spawn door");
    let text = error.to_string();
    assert!(text.contains("over-limit"), "{text}");
    assert!(text.contains("ceiling=2"), "{text}");
}

#[test]
fn ac3_edge_concurrent_launchers_remeasure_after_the_first_spawn() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    let previous_brake = std::env::var_os("FNO_MACHINE_BRAKE");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "1");
    std::env::set_var(
        "FNO_MACHINE_BRAKE",
        std::env::temp_dir().join(format!("brake-absent-{}.json", std::process::id())),
    );

    // One live child puts the census at the ceiling. The two doors then
    // serialize through the machine lock: each reads the true count and
    // holds, while the default door in the same world keeps admitting.
    let mut command = fno::process_admission::std_command("sleep");
    command
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = fno::process_admission::std_spawn(&mut command)
        .expect("the default door admits and records the child");

    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|attempt| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                println!("door attempted index={attempt}");
                barrier.wait();
                fno::process_admission::admit_agent_spawn()
            })
        })
        .collect::<Vec<_>>();

    let mut refusals = Vec::new();
    for handle in handles {
        if let Err(error) = handle.join().expect("door thread must finish") {
            refusals.push(error.to_string());
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    restore_max_processes(previous);
    restore_env("FNO_MACHINE_BRAKE", previous_brake);

    assert_eq!(refusals.len(), 2, "{refusals:?}");
    for text in refusals {
        assert!(text.contains("over-limit"), "{text}");
        assert!(text.contains("ceiling=1"), "{text}");
    }
}

#[test]
fn process_ceiling_uses_its_own_wire_and_process_default() {
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");

    std::env::remove_var("FNO_PROCESS_ADMISSION_MAX");
    assert_eq!(configured_max_processes().unwrap().get(), 400);

    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "650");
    assert_eq!(configured_max_processes().unwrap().get(), 650);

    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "not-processes");
    assert!(configured_max_processes().is_err());
    restore_max_processes(previous);
}

#[test]
fn ac5_hp_off_switch_bypasses_cap_before_config_and_lock() {
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous_mode = std::env::var_os("FNO_PROCESS_ADMISSION");
    let previous_max = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");

    std::env::set_var("FNO_PROCESS_ADMISSION", "off");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "not-a-number");

    assert!(fno::process_admission::admit_fleet().is_ok());
    assert!(fno::process_admission::admit_tab(99, Some(1)).is_ok());

    restore_env("FNO_PROCESS_ADMISSION", previous_mode);
    restore_env("FNO_PROCESS_ADMISSION_MAX", previous_max);

    // An armed runaway brake holds the agent-spawn door and nothing else.
    isolate_admission_state();
    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    let brake = std::env::temp_dir().join(format!("brake-e2e-{}.json", std::process::id()));
    std::fs::write(
        &brake,
        format!(r#"{{"until_epoch":{until},"reason":"machine runaway: hot for 3600s"}}"#),
    )
    .unwrap();
    let previous_brake = std::env::var_os("FNO_MACHINE_BRAKE");
    std::env::set_var("FNO_MACHINE_BRAKE", &brake);

    let default_door = fno::process_admission::admit_fleet();
    let agent = fno::process_admission::admit_agent_spawn();

    restore_env("FNO_MACHINE_BRAKE", previous_brake);
    let _ = std::fs::remove_file(&brake);

    let default_err = default_door.as_ref().err().map(|e| e.to_string());
    assert!(
        default_door.is_ok(),
        "the default door is never held by the brake: {}",
        default_err.unwrap_or_default(),
    );
    let error = agent.err().expect("the agent-spawn door still refuses");
    assert!(error.to_string().contains("machine-runaway"), "{error}");
}

#[test]
fn ac5_err_invalid_off_switch_fails_closed_with_accepted_values() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous_mode = std::env::var_os("FNO_PROCESS_ADMISSION");

    std::env::set_var("FNO_PROCESS_ADMISSION", "maybe");
    let outcome = fno::process_admission::admit_agent_spawn().map(|_| ());
    // Restore before asserting: an assertion that panics with the switch
    // still set leaks "maybe" into every later test in this binary, and the
    // cascade reads as unrelated failures.
    restore_env("FNO_PROCESS_ADMISSION", previous_mode);

    let error = match outcome {
        Ok(()) => panic!("invalid switch must refuse"),
        Err(error) => error,
    };
    assert!(error.detail().contains("FNO_PROCESS_ADMISSION"));
    assert!(
        error.detail().contains(ADMISSION_ACCEPTED),
        "{}",
        error.detail()
    );
}

fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

fn restore_max_processes(previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => std::env::set_var("FNO_PROCESS_ADMISSION_MAX", value),
        None => std::env::remove_var("FNO_PROCESS_ADMISSION_MAX"),
    }
}

/// The inversion, end to end: under an armed brake and an over-limit
/// ceiling, fno's own helper spawns admit and the agent-spawn door alone is
/// held, only when its caller carries a worker identity.
#[test]
fn ac_scope_only_the_agent_spawn_door_is_held() {
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("FNO_E2E", "1");
    std::env::set_var(
        "FNO_MUX_ADMISSION_NAMESPACE",
        format!("scope-{}", std::process::id()),
    );
    std::env::remove_var("FNO_AGENT_SELF");
    let previous_max = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    let previous_brake = std::env::var_os("FNO_MACHINE_BRAKE");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "1");
    let brake_dir = tempfile::tempdir().unwrap();
    let brake_path = brake_dir.path().join("brake.json");
    std::fs::write(
        &brake_path,
        serde_json::json!({
            "until_epoch": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 600,
            "reason": "machine runaway: test",
            "group": {"name": "g i t", "count": 8000, "ppid": 1},
        })
        .to_string(),
    )
    .unwrap();
    std::env::set_var("FNO_MACHINE_BRAKE", &brake_path);

    let output = fno::process_admission::std_output(
        fno::process_admission::std_command("printf").arg("admitted"),
    )
    .expect("fno's own spawn admits over the ceiling and under an armed brake");
    assert_eq!(output.stdout, b"admitted");

    // The agent-spawn door in the same world holds only a worker identity.
    let human_door = fno::process_admission::admit_agent_spawn();
    let human_err = human_door.as_ref().err().map(|e| e.to_string());
    assert!(
        human_door.is_ok(),
        "no worker identity, no hold: {}",
        human_err.unwrap_or_default(),
    );

    std::env::set_var("FNO_AGENT_SELF", "scope-worker");
    let refusal = fno::process_admission::admit_agent_spawn();
    restore_env("FNO_AGENT_SELF", None);
    restore_max_processes(previous_max);
    restore_env("FNO_MACHINE_BRAKE", previous_brake);
    let error = refusal.err().expect("a worker identity is held");
    assert!(error.to_string().contains("machine-runaway"), "{error}");
}
