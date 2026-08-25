use fno::process_admission::{
    configured_max_processes, decide_panes, decide_processes, AdmissionDecision, Census, MaxPanes,
    MaxProcesses, PaneCount, Scope,
};
use std::process::Stdio;
use std::sync::{Arc, Barrier, Mutex, OnceLock};

static ADMISSION_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn census(count: usize) -> Census {
    Census::complete(count)
}

fn isolate_admission_state() {
    std::env::set_var("FNO_E2E", "1");
    std::env::set_var(
        "FNO_MUX_ADMISSION_NAMESPACE",
        format!("process-admission-{}", std::process::id()),
    );
}

#[test]
fn ac1_hp_allows_complete_snapshot_below_fleet_ceiling() {
    let decision = decide_processes(&census(1), MaxProcesses::new(2));

    assert_eq!(decision, AdmissionDecision::Admit);
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
async fn ac1_hp_async_output_preserves_implicit_capture() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "512");
    let mut command = fno::process_admission::tokio_command("printf");
    command.arg("async-capture");
    let output = fno::process_admission::tokio_output(&mut command)
        .await
        .unwrap();
    restore_max_processes(previous);
    assert_eq!(output.stdout, b"async-capture");
}

#[test]
fn ac2_err_refuses_at_fleet_ceiling_with_positive_marker() {
    let decision = decide_processes(&census(2), MaxProcesses::new(2));

    assert_eq!(
        decision.refusal(),
        Some("process admission refused: count=2 ceiling=2 scope=fleet reason=over-limit".into())
    );
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
                .into(),
        )
    );
}

#[test]
fn ac9_edge_applies_tab_ceiling_as_a_separate_scope() {
    let decision = decide_panes(PaneCount::new(4), MaxPanes::new(4));

    assert_eq!(decision.scope(), Some(Scope::Tab));
    assert!(decision.refusal().is_some());
}

#[test]
fn ac2_err_creation_path_emits_positive_refusal_marker() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "2");

    let mut children = Vec::new();
    let mut refusal = None;
    for attempt in 0..=2 {
        println!("spawn attempted index={attempt}");
        let mut command = fno::process_admission::std_command("sleep");
        command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match fno::process_admission::std_spawn(&mut command) {
            Ok(child) => children.push(child),
            Err(error) => {
                refusal = Some(error.to_string());
                break;
            }
        }
    }

    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
    restore_max_processes(previous);

    assert_eq!(
        refusal,
        Some("process admission refused: count=2 ceiling=2 scope=fleet reason=over-limit".into())
    );
}

#[test]
fn ac3_edge_concurrent_launchers_remeasure_after_the_first_spawn() {
    isolate_admission_state();
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("FNO_PROCESS_ADMISSION_MAX");
    std::env::set_var("FNO_PROCESS_ADMISSION_MAX", "1");

    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|attempt| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                println!("spawn attempted index={attempt}");
                barrier.wait();
                let mut command = fno::process_admission::std_command("sleep");
                command
                    .arg("60")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                fno::process_admission::std_spawn(&mut command)
            })
        })
        .collect::<Vec<_>>();

    let mut children = Vec::new();
    let mut refusals = Vec::new();
    for handle in handles {
        match handle.join().expect("launcher thread must finish") {
            Ok(child) => children.push(child),
            Err(error) => refusals.push(error.to_string()),
        }
    }
    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
    restore_max_processes(previous);

    assert_eq!(
        refusals,
        vec![
            "process admission refused: count=1 ceiling=1 scope=fleet reason=over-limit"
                .to_string(),
        ]
    );
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

fn restore_max_processes(previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => std::env::set_var("FNO_PROCESS_ADMISSION_MAX", value),
        None => std::env::remove_var("FNO_PROCESS_ADMISSION_MAX"),
    }
}
