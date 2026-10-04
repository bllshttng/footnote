//! `fno config paths emit-shell`, `shell-stub` and `handoff`: the paths
//! verbs answered natively.
//!
//! One verb per PR (law d-450caaeb): `emit-shell` landed first, `shell-stub`
//! and `handoff` followed; only `verify` still forwards to the Python paths
//! group until its child node ports. The route execs the worker's
//! `--paths-exec` lane (worker_binary resolution like the law door), argv
//! and exit code

use std::ffi::OsString;
use std::process::Command;

/// The verbs the native lane serves. One per PR (law d-450caaeb):
/// emit-shell landed first, shell-stub and handoff followed; verify is the
/// last Python leg.
pub const NATIVE_PATHS_VERBS: &[&str] = &["emit-shell", "shell-stub", "handoff"];

/// Classify the paths verb for the front door: `fno config paths <v> ...`
/// and its deprecated top-level spelling `fno paths <v> ...` both route
/// here when the verb is served natively.
pub fn classify(args: &[OsString]) -> Option<Vec<OsString>> {
    let w0 = args.first().and_then(|a| a.to_str());
    let start = match w0 {
        Some("config") if args.get(1).and_then(|a| a.to_str()) == Some("paths") => 2,
        Some("paths") => 1,
        _ => return None,
    };
    let verb = args.get(start).and_then(|a| a.to_str())?;
    if !NATIVE_PATHS_VERBS.contains(&verb) {
        return None;
    }
    Some(args[start..].to_vec())
}

/// Exec the worker's `--paths-exec` lane; stdout, stderr and the exit code
/// pass through. A missing worker is a refusal (exit 3), never an empty
/// answer, matching the law door's posture.
pub fn run(args: &[OsString]) -> i32 {
    let Some(binary) = crate::store_client::worker_binary() else {
        eprintln!("fno config paths: refused: the fno-agents-worker binary is unavailable (set FNO_AGENTS_WORKER, or install the worker beside fno).");
        return 3;
    };
    match Command::new(&binary)
        .arg("--paths-exec")
        .args(args)
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fno config paths: refused: cannot spawn the paths worker: {e}");
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oss(pieces: &[&str]) -> Vec<OsString> {
        pieces.iter().map(OsString::from).collect()
    }

    #[test]
    fn classify_claims_emit_shell() {
        let rest = classify(&oss(&[
            "config",
            "paths",
            "emit-shell",
            "--output",
            "/tmp/p.sh",
        ]))
        .unwrap();
        assert_eq!(rest.len(), 3);
        assert_eq!(rest[0], OsString::from("emit-shell"));
    }

    #[test]
    fn classify_claims_handoff() {
        let rest = classify(&oss(&[
            "config",
            "paths",
            "handoff",
            "--scope",
            "fno-x-aaaa",
        ]))
        .unwrap();
        assert_eq!(rest.len(), 3);
        assert_eq!(rest[0], OsString::from("handoff"));
        let rest = classify(&oss(&["paths", "handoff", "--scope", "s"])).unwrap();
        assert_eq!(rest[0], OsString::from("handoff"));
    }

    #[test]
    fn classify_leaves_other_verbs_to_python() {
        for verb in ["verify", "bogus"] {
            assert!(classify(&oss(&["config", "paths", verb])).is_none());
            assert!(classify(&oss(&["paths", verb])).is_none());
        }
        assert!(classify(&oss(&["config", "setup", "auto-wire"])).is_none());
        assert!(classify(&oss(&["paths"])).is_none());
        assert!(classify(&oss(&["config", "paths"])).is_none());
    }

    #[test]
    fn classify_claims_shell_stub() {
        let rest = classify(&oss(&["config", "paths", "shell-stub"])).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0], OsString::from("shell-stub"));
    }

    #[test]
    fn classify_claims_the_deprecated_top_level_spelling() {
        let rest = classify(&oss(&["paths", "emit-shell", "--output", "/tmp/p.sh"])).unwrap();
        assert_eq!(rest.len(), 3);
        assert_eq!(rest[0], OsString::from("emit-shell"));
    }
}
