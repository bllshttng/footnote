//! What work is stuck: a verb process running far past its own deadline, and
//! a single-flight lock whose holder pid is gone. One module answers "is any
//! work stuck on this machine" for the three readers that page or print it
//! (`fno agents status`, arm_watch, the king check-in), so a hung `backlog
//! advance` can never again run 3h29m with the only signal a banner nobody
//! sees.
//!
//! The read is a snapshot: ps once, the claims dirs once. Findings are
//! self-describing lines so every reader prints the same text without a
//! second renderer.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The age past which a verb with no declared timeout reads hung: 3 x the
/// 10-minute-class default, the wall clock the drain's own lease carries.
// ponytail: fixed 1800 s floor and 300 s holder grace, move to [notify] keys if a lane needs a different clock
const DEFAULT_HUNG_FLOOR_S: u64 = 1800;

/// A dead-pid `flight:` hold must sit this long before the read names it: the
/// gate reclaims a dead hold on the next acquire, so a fresh corpse is not
/// news.
// ponytail: fixed 1800 s floor and 300 s holder grace, move to [notify] keys
// if a lane needs a different clock
const DEAD_HOLDER_GRACE_S: i64 = 300;

/// How much of a finding's argv a line carries. The notice names the argv so
/// a missing exclusion shape is fixable from the page alone.
const ARGV_CAP: usize = 120;

/// The argv tokens that mark a long-lived shape: not hung, by design.
const LONG_LIVED_TOKENS: [&str; 3] = ["daemon", "attach", "mux"];

/// One stuck thing, self-describing. `key` is the dedupe token part the
/// arm_watch tick folds it into; `line` is the only text a reader prints.
pub struct Finding {
    pub kind: &'static str,
    pub key: String,
    pub line: String,
}

/// A verb is hung when its age passes three times its declared `--timeout`
/// (else 1800 s). The verb rule and the excluded shapes are the whole
/// classifier; every shape here is pinned by a test.
pub(crate) fn hung_verbs(rows: &[(u32, u64, String)], now_unix: u64) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (pid, age_s, args) in rows {
        let Some(finding) = hung_verb(*pid, *age_s, args, now_unix) else {
            continue;
        };
        findings.push(finding);
    }
    findings
}

fn hung_verb(pid: u32, age_s: u64, args: &str, now_unix: u64) -> Option<Finding> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let argv0 = tokens.first()?;
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let is_verb = matches!(base, "fno" | "fno-py" | "fno-agents")
        || (base.starts_with("python") && tokens.get(1).is_some_and(|a| a.ends_with("/fno-py")));
    if !is_verb {
        return None;
    }
    // Long-lived shapes: the daemon and the mux clients run for days by
    // design; a bare `fno` is a shell, not a verb.
    if tokens.len() == 1
        || tokens.contains(&"--server")
        || tokens.iter().any(|a| LONG_LIVED_TOKENS.contains(a))
        || tokens.windows(2).any(|w| w == ["loop", "run"])
    {
        return None;
    }
    let timeout = declared_timeout(&tokens);
    let floor = 3 * timeout
        .as_ref()
        .map(|(_, s)| *s)
        .unwrap_or(DEFAULT_HUNG_FLOOR_S);
    if age_s <= floor {
        return None;
    }
    let basis = match &timeout {
        Some((raw, _)) => format!("over 3x --timeout {raw}"),
        None => format!("over {DEFAULT_HUNG_FLOOR_S}s"),
    };
    Some(Finding {
        kind: "hung_verb",
        key: format!("hung:{pid}@{}", now_unix.saturating_sub(age_s)),
        line: format!(
            "hung verb pid {pid} {} {} ({basis})",
            fmt_age(age_s),
            cut(args, ARGV_CAP)
        ),
    })
}

/// The raw `--timeout <dur>` token, as written (`30`, `30s`, `30m`, `30h`).
fn declared_timeout(tokens: &[&str]) -> Option<(String, u64)> {
    let pos = tokens.iter().position(|a| *a == "--timeout")?;
    let raw = *tokens.get(pos + 1)?;
    let digits = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    let secs = match raw.chars().last() {
        Some('s') | Some('0'..='9') => n,
        Some('m') => n * 60,
        Some('h') => n * 3600,
        _ => return None,
    };
    Some((raw.to_string(), secs))
}

/// One `ps -axo pid=,etime=,args=` pass, parsed to (pid, age_s, args). A
/// non-zero exit or unreadable output is an Err, never an empty list: a
/// blind read must not page as "nothing is stuck".
fn ps_rows() -> Result<Vec<(u32, u64, String)>, String> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,etime=,args="])
        .output()
        .map_err(|e| format!("ps failed to run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ps exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(120)
                .collect::<String>()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let (etime, args) = match rest.trim_start().split_once(' ') {
            Some((etime, args)) => (etime, args.trim()),
            None => (rest.trim_start(), ""),
        };
        let Some(age) = crate::gc::parse_etime(etime) else {
            continue;
        };
        rows.push((pid, age, args.to_string()));
    }
    Ok(rows)
}

/// Dead holders among the `flight:` claims: rows `long_holds` already aged
/// past the grace whose pid probe read `absent` on this host. Only `flight:`
/// holds count - they are pid-scoped by design; a session claim outlives its
/// ambient pid on purpose.
pub(crate) fn dead_holders(dirs: &[PathBuf]) -> Result<Vec<Finding>, String> {
    let payload = crate::claims::long_holds(dirs, DEAD_HOLDER_GRACE_S)?;
    let now_unix = crate::claims::now_ms() / 1000;
    let rows: Vec<Value> = payload
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut findings = Vec::new();
    for row in &rows {
        if row.get("pid_observed").and_then(Value::as_str) != Some("absent") {
            continue;
        }
        let Some(key) = row.get("key").and_then(Value::as_str) else {
            continue;
        };
        let holder = row.get("holder").and_then(Value::as_str).unwrap_or("?");
        let pid = row
            .get("pid")
            .map(|p| p.to_string().trim_matches('"').to_string());
        let held_s = row.get("held_s").and_then(Value::as_i64).unwrap_or(0);
        let anchor = (now_unix - held_s.max(0)).max(0) as u64;
        findings.push(Finding {
            kind: "dead_holder",
            key: format!("holder:{key}@{anchor}"),
            line: format!(
                "dead holder {key} holder {holder} pid {} absent held {}",
                pid.as_deref().unwrap_or("None"),
                fmt_age(held_s.max(0) as u64)
            ),
        });
    }
    Ok(findings)
}

/// The claims directories the dead-holder read scans: the global dir, then
/// the project dir (`$FNO_CLAIMS_ROOT/.fno/claims` when set, else the space
/// dir's claims), the order the Python claims io resolves.
pub fn claims_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(global) = crate::claims::global_claims_dir() {
        dirs.push(global);
    }
    let project = match std::env::var_os("FNO_CLAIMS_ROOT").filter(|v| !v.is_empty()) {
        Some(root) => PathBuf::from(root).join(".fno").join("claims"),
        None => crate::paths::space_dir(cwd).join("claims"),
    };
    if !dirs.contains(&project) {
        dirs.push(project);
    }
    dirs
}

/// The full read: hung verbs first, then dead holders. An error from either
/// leg fails the whole read - a partially blind answer reads as a failed
/// read, never as a clean empty.
pub fn collect(cwd: &Path) -> Result<Vec<Finding>, String> {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut findings = hung_verbs(&ps_rows()?, now_unix);
    findings.extend(dead_holders(&claims_dirs(cwd))?);
    Ok(findings)
}

/// The `fno agents status` JSON shape: the two finding lists, or one error.
pub fn status_value(cwd: &Path) -> Value {
    match collect(cwd) {
        Ok(findings) => {
            let hung: Vec<&str> = findings
                .iter()
                .filter(|f| f.kind == "hung_verb")
                .map(|f| f.line.as_str())
                .collect();
            let holders: Vec<&str> = findings
                .iter()
                .filter(|f| f.kind == "dead_holder")
                .map(|f| f.line.as_str())
                .collect();
            json!({"hung_verbs": hung, "dead_holders": holders})
        }
        Err(e) => json!({"error": e}),
    }
}

/// The `stuck work:` block the human status prints: nothing when the read is
/// clean, one line per finding, the reason when the read failed.
pub fn render_lines(value: &Value) -> Vec<String> {
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return vec![format!("stuck work: unreadable ({err})")];
    }
    let hung = value
        .get("hung_verbs")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let holders = value
        .get("dead_holders")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    if hung + holders == 0 {
        return Vec::new();
    }
    let mut lines = vec!["stuck work:".to_string()];
    for item in value
        .get("hung_verbs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            value
                .get("dead_holders")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
    {
        if let Some(text) = item.as_str() {
            lines.push(format!("  {text}"));
        }
    }
    lines
}

/// Compact floored age, `45s` / `12m` / `3h29m`.
fn fmt_age(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    if h > 0 {
        if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{seconds}s")
    }
}

fn cut(text: &str, cap: usize) -> &str {
    match text.char_indices().nth(cap) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{encode_key, hostname, machine_id, now_ms, SCHEMA_VERSION};
    use tempfile::TempDir;

    fn dead_pid() -> u32 {
        let mut candidate = 999_999u32;
        while unsafe { libc::kill(candidate as i32, 0) } == 0 {
            candidate += 1;
        }
        candidate
    }

    fn flight_rec(
        key: &str,
        holder: &str,
        pid: Option<i32>,
        acquired_s_ago: i64,
    ) -> crate::claims::ClaimRecord {
        crate::claims::ClaimRecord {
            schema_version: SCHEMA_VERSION,
            key: key.into(),
            holder: holder.into(),
            acquired_at: now_ms() - acquired_s_ago * 1000,
            pid,
            host: hostname(),
            pid_unavailable: false,
            expires_at: None,
            reason: None,
            harness: None,
            session_id: None,
            pid_provenance: None,
            machine_id: Some(machine_id()),
            metadata: Default::default(),
        }
    }

    fn write_rec(dir: &Path, rec: &crate::claims::ClaimRecord) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.lock", encode_key(&rec.key))),
            serde_json::to_string(rec).unwrap(),
        )
        .unwrap();
    }

    // AC2-HP: the measured hung advance is a finding.
    #[test]
    fn hung_advance_is_a_finding() {
        let now = 1_800_000_000u64;
        let rows = vec![(
            66_853u32,
            12_540u64,
            "/x/python3 /x/fno-py backlog advance --loose --project fno --source ab --json"
                .to_string(),
        )];
        let findings = hung_verbs(&rows, now);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.kind, "hung_verb");
        assert_eq!(f.key, format!("hung:66853@{}", now - 12_540));
        assert!(
            f.line
                .starts_with("hung verb pid 66853 3h29m /x/python3 /x/fno-py backlog advance"),
            "{}",
            f.line
        );
        assert!(f.line.ends_with("(over 1800s)"), "{}", f.line);
    }

    // AC2-ERR: the declared timeout triples the floor.
    #[test]
    fn declared_timeout_triples_the_floor() {
        let now = 1_800_000_000u64;
        let args = "fno-py do pr wait 2033 --timeout 30m".to_string();
        let findings = hung_verbs(&vec![(1u32, 5_000u64, args.clone())], now);
        assert!(findings.is_empty(), "5000s is inside 3x30m");
        let findings = hung_verbs(&vec![(1u32, 5_500u64, args)], now);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].line.contains("(over 3x --timeout 30m)"),
            "{}",
            findings[0].line
        );
    }

    // AC2-EDGE: the long-lived shapes stay quiet; a failed ps is an Err.
    #[test]
    fn long_lived_shapes_never_page() {
        let now = 1_800_000_000u64 + 3600 * 35 + 11 * 3600;
        let rows = vec![
            (1u32, 126_600u64, "fno --server /x/main.sock".to_string()),
            (2u32, 126_600u64, "fno".to_string()),
            (3u32, 3600u64, "fno-agents-daemon --home /x".to_string()),
            (4u32, 9u64, "fno-py backlog render-views".to_string()),
            (5u32, 900u64, "fno mux".to_string()),
            (
                6u32,
                900u64,
                "fno-agents loop run --driver target".to_string(),
            ),
        ];
        assert!(hung_verbs(&rows, now).is_empty());
    }

    #[test]
    fn bad_ps_is_an_error_not_an_empty_list() {
        // A ps whose exit is non-zero is Err; this stub never runs, so the
        // error kind is the spawn failure itself.
        let prev = std::env::var_os("PATH");
        std::env::set_var("PATH", "/nonexistent-for-stuck-work");
        let result = ps_rows();
        match prev {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        assert!(result.is_err(), "a failed ps is Err");
    }

    // AC3-HP: a dead-pid flight hold names its holder.
    #[test]
    fn dead_flight_holder_is_a_finding() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        write_rec(
            &claims_dir,
            &flight_rec(
                "flight:backlog-advance",
                "single-flight:ab",
                Some(dead_pid() as i32),
                600,
            ),
        );
        let findings = dead_holders(&[claims_dir]).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.kind, "dead_holder");
        assert!(
            f.key.starts_with("holder:flight:backlog-advance@"),
            "{}",
            f.key
        );
        assert!(
            f.line
                .starts_with("dead holder flight:backlog-advance holder single-flight:ab"),
            "{}",
            f.line
        );
        assert!(
            f.line.contains("pid ") && f.line.contains("absent held 10m"),
            "{}",
            f.line
        );
    }

    // AC3-ERR: a live holder, no pid, or a non-flight claim is not a finding.
    #[test]
    fn live_null_and_non_flight_never_page() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        write_rec(
            &claims_dir,
            &flight_rec(
                "flight:live",
                "single-flight:me",
                Some(std::process::id() as i32),
                600,
            ),
        );
        write_rec(
            &claims_dir,
            &flight_rec("flight:nul", "single-flight:x", None, 600),
        );
        write_rec(
            &claims_dir,
            &flight_rec("node:z", "target-session:w", Some(dead_pid() as i32), 600),
        );
        assert!(dead_holders(&[claims_dir]).unwrap().is_empty());
    }

    // AC3-EDGE: a fresh corpse is not news; a claims dir that is a file errs.
    #[test]
    fn fresh_corpse_and_file_dir() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        write_rec(
            &claims_dir,
            &flight_rec(
                "flight:fresh",
                "single-flight:y",
                Some(dead_pid() as i32),
                200,
            ),
        );
        assert!(dead_holders(&[claims_dir]).unwrap().is_empty());

        let file_dir = td.path().join("not-a-dir");
        std::fs::write(&file_dir, "x").unwrap();
        assert!(dead_holders(&[file_dir]).is_err());
    }

    #[test]
    fn status_value_and_render_round_trip() {
        let value = json!({
            "hung_verbs": ["hung verb pid 7 1h fno backlog advance (over 1800s)"],
            "dead_holders": []
        });
        let lines = render_lines(&value);
        assert_eq!(lines[0], "stuck work:");
        assert_eq!(lines.len(), 2);
        // Clean and failed reads.
        assert!(render_lines(&json!({"hung_verbs": [], "dead_holders": []})).is_empty());
        let err = render_lines(&json!({"error": "ps exited 1"}));
        assert_eq!(err[0], "stuck work: unreadable (ps exited 1)");
    }
}
