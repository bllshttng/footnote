use fno::process_admission::{decide, AdmissionDecision, AdmissionLimits, Census, Scope};
use std::process::Stdio;
use std::sync::{Arc, Barrier, Mutex, OnceLock};

static ADMISSION_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn census(count: usize) -> Census {
    Census::complete(count)
}

#[test]
fn ac1_hp_allows_complete_snapshot_below_fleet_ceiling() {
    let decision = decide(&census(1), AdmissionLimits::fleet(2));

    assert_eq!(decision, AdmissionDecision::Admit);
}

#[test]
fn ac2_err_refuses_at_fleet_ceiling_with_positive_marker() {
    let decision = decide(&census(2), AdmissionLimits::fleet(2));

    assert_eq!(
        decision.refusal(),
        Some("process admission refused: count=2 ceiling=2 scope=fleet reason=over-limit".into())
    );
}

#[test]
fn ac4_neg_refuses_incomplete_snapshot_without_substituting_zero() {
    let decision = decide(
        &Census::unavailable("worker root discovery unavailable"),
        AdmissionLimits::fleet(2),
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
    let decision = decide(&census(4), AdmissionLimits::tab(4));

    assert_eq!(decision.scope(), Some(Scope::Tab));
    assert!(decision.refusal().is_some());
}

#[test]
fn ac2_err_creation_path_emits_positive_refusal_marker() {
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let previous = std::env::var_os("FNO_MUX_MAX_LIVE");
    std::env::set_var("FNO_MUX_MAX_LIVE", "2");

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
    restore_max_live(previous);

    assert_eq!(
        refusal,
        Some("process admission refused: count=2 ceiling=2 scope=fleet reason=over-limit".into())
    );
}

#[test]
fn ac3_edge_concurrent_launchers_remeasure_after_the_first_spawn() {
    let _env_lock = ADMISSION_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let previous = std::env::var_os("FNO_MUX_MAX_LIVE");
    std::env::set_var("FNO_MUX_MAX_LIVE", "1");

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
    restore_max_live(previous);

    assert_eq!(
        refusals,
        vec![
            "process admission refused: count=1 ceiling=1 scope=fleet reason=over-limit"
                .to_string(),
        ]
    );
}

fn restore_max_live(previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => std::env::set_var("FNO_MUX_MAX_LIVE", value),
        None => std::env::remove_var("FNO_MUX_MAX_LIVE"),
    }
}
