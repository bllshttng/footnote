use super::{parse_pr_facts, PrFacts};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

const READ_BOUND: Duration = Duration::from_secs(15);

pub(super) fn read(cwd: &Path, pr: Option<u64>) -> Result<PrFacts, String> {
    read_with(OsStr::new(&crate::scrape::fno_bin()), cwd, pr, READ_BOUND)
}

fn read_with(bin: &OsStr, cwd: &Path, pr: Option<u64>, bound: Duration) -> Result<PrFacts, String> {
    let number = pr.map(|n| n.to_string());
    let mut args = vec!["do", "pr", "info"];
    if let Some(number) = number.as_deref() {
        args.push(number);
    }
    let out = crate::loopcheck::bounded_read(bin, &args, cwd, "PR head", bound)
        .map_err(|e| crate::loopcheck::bounded_read_diagnostic("PR head", &e))?;
    if !out.status.success() {
        return Err(format!(
            "fno do pr info failed: {}",
            String::from_utf8_lossy(&out.stderr_tail).trim()
        ));
    }
    let payload = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("fno do pr info returned unreadable JSON: {e}"))?;
    parse_pr_facts(&payload)
}

pub(super) fn receipt(payload: &Value) -> Value {
    let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
        return json!({"head": null, "error": "payload needs cwd"});
    };
    let Some(pr) = payload.get("pr").and_then(Value::as_u64) else {
        return json!({"head": null, "error": "payload needs numeric pr"});
    };
    match read(Path::new(cwd), Some(pr)) {
        Ok(facts) => json!({"head": facts.head_sha}),
        Err(error) => json!({"head": null, "error": error}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn stalled_head_read_is_bounded_and_malformed_heads_are_refused() {
        assert!(parse_pr_facts(&json!({"pr": 7})).is_err());
        assert!(parse_pr_facts(&json!({"error": "gh api failed"})).is_err());
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fno");
        std::fs::write(&bin, "#!/bin/sh\n[ \"$*\" = 'do pr info 7' ] || exit 1\nprintf '%s' '{\"pr\":7,\"head_sha\":\"old-head\"}'\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read_with(bin.as_os_str(), dir.path(), Some(7), Duration::from_secs(2))
                .unwrap()
                .head_sha,
            "old-head"
        );
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();
        let error = read_with(
            bin.as_os_str(),
            dir.path(),
            Some(7),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.contains("outcome=timeout"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
