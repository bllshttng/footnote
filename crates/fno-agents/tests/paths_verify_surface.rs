//! The verify verb's user-visible surface: exit codes and the exact message
//! text the retired Python verify_cmd printed, pinned against the built
//! worker binary so the strings cannot drift silently.

use std::path::PathBuf;
use std::process::Command;

fn repo_paths_sh() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/lib/paths.sh")
        .canonicalize()
        .expect("scripts/lib/paths.sh exists in the checkout")
}

fn run_verify(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents-worker"))
        .arg("--paths-exec")
        .arg("verify")
        .args(args)
        .output()
        .expect("worker binary runs");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn verify_surface_messages() {
    // Sync: stdout names the schema hash and exits 0.
    let path = repo_paths_sh();
    let (code, stdout, stderr) = run_verify(&[path.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.starts_with("paths.sh is in sync with schema (hash: ") && stdout.ends_with("...)\n"),
        "unexpected sync line: {stdout:?}"
    );

    // Mismatch: the exact Python-shaped block on stderr, exit 1.
    let mutated =
        std::env::temp_dir().join(format!("fno-verify-surface-{}.sh", std::process::id()));
    std::fs::copy(&path, &mutated).unwrap();
    let mut content = std::fs::read_to_string(&mutated).unwrap();
    content.push_str("\n# INJECTED_MUTATION\n");
    std::fs::write(&mutated, &content).unwrap();
    let (code, stdout, stderr) = run_verify(&[mutated.to_str().unwrap()]);
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(stdout.is_empty());
    assert!(stderr.starts_with("--- expected (from schema)\n+++ checked-in\nschema hash:  "));
    assert!(stderr.contains("\nfile hash:    "));
    assert!(
        stderr.ends_with("\n\nHashes differ. Regenerate with:\n  fno config paths emit-shell\n")
    );

    // Missing file: the regen hint on stderr, exit 1.
    let missing = std::env::temp_dir().join("fno-verify-surface-does-not-exist.sh");
    let (code, _, stderr) = run_verify(&[missing.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        stderr.ends_with(" does not exist. Generate it with: fno config paths emit-shell\n"),
        "unexpected missing-file text: {stderr:?}"
    );
}
