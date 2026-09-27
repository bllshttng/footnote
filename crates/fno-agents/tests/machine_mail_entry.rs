use std::process::Command;

#[test]
fn machine_mail_send_rejects_unknown_arm_before_delivery() {
    let output = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .envs(fno_agents::test_run::self_owner_env())
        .args([
            "machine-mail-send",
            "--arm",
            "unknown",
            "--to",
            "worker",
            "--",
            "body",
        ])
        .output()
        .expect("run the fno-agents client");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("expected events-push or note-pointer"),
        "unexpected refusal: {stderr}"
    );
}
