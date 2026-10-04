//! `fno config paths emit-shell`: the one paths verb answered natively.
//!
//! One verb per PR (law d-450caaeb): `emit-shell` landed first; `shell-stub`,
//! `verify` and `handoff` still forward to the Python paths group until their
//! child nodes port. The route execs the worker's `--paths-exec` lane
//! (worker_binary resolution like the law door), argv and exit code
//! pass through unchanged.

use std::ffi::OsString;
use std::process::Command;

/// The verbs the native lane serves. One per PR (law d-450caaeb):
/// emit-shell landed first; the rest forward to Python.
pub const NATIVE_PATHS_VERBS: &[&str] = &["emit-shell"];

/// Classify `fno config paths <verb> ...` for the front door.
pub fn classify(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() >= 3
        && args[0].to_str() == Some("config")
        && args[1].to_str() == Some("paths")
        && NATIVE_PATHS_VERBS.contains(&args[2].to_str()?)
    {
        Some(args[2..].to_vec())
    } else {
        None
    }
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
    fn classify_leaves_other_verbs_to_python() {
        for verb in ["shell-stub", "verify", "handoff", "bogus"] {
            assert!(classify(&oss(&["config", "paths", verb])).is_none());
        }
        assert!(classify(&oss(&["config", "setup", "auto-wire"])).is_none());
    }
}
