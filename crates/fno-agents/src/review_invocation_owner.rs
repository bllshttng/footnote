// @vendored-core: this file is vendored byte-identical into
// crates/fno-agents/src/review_invocation_owner.rs; the parity test
// owner_core_is_vendored_byte_identical pins the equality. Edit here,
// then copy - never fork. No `crate::` references: the core must splice
// into either crate.
//
// The one owner of the review-invocation vocabulary: the verb parser, the
// canonical flag table, the id mint, and the per-session pending sidecar.
// Ported from the deleted Python leg (`cli/src/fno/review/invocation.py`)
// and from the two partial Rust copies that predated it (the mux verb parse
// in the fno crate, the codex sidecar in `codex_inject.rs`); every reader
// now answers from this module.

use std::path::{Path, PathBuf};

/// The review flags whose spelling this module is the authority for: the
/// router prose cites this vocabulary rather than restating it, so a
/// spelling accepted in one place is accepted in the other and the two
/// cannot drift. `verify-fixes` is a scoped fix-verification, NOT a round;
/// the cap gate derives its spelling from [`VERIFY_FIXES`].
pub const VERIFY_FIXES: &str = "verify-fixes";

const KNOWN_REVIEW_FLAGS: [&str; 3] = ["comment", "fix", VERIFY_FIXES];
const REVIEW_VERBS: [&str; 4] = ["code-review", "review", "review-changes", "sigma-review"];
const REVIEW_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Return the canonical `--` spelling when `token` is a review flag.
///
/// Three spellings reach the router and this parser for the same flag: the
/// canonical double hyphen, the bare word an operator drops the hyphens
/// from, and the single em dash a phone autocorrect substitutes for the
/// double hyphen. The em-dash alias is deliberately narrow: exactly one
/// character, immediately followed by a KNOWN flag name - no other dash (en
/// dash, minus sign, horizontal bar) is a flag, because no legitimate target
/// begins with an em dash while those characters do occur in prose and
/// paths. Bare words count only for the known names, so a branch literally
/// named `fix` is the one casualty and stays reachable through its
/// double-hyphen spelling or a fuller ref form. Unknown `--` tokens return
/// None here; the parser still records them verbatim.
pub fn canonical_flag(token: &str) -> Option<String> {
    let mut chars = token.chars();
    let name = if token.starts_with("--") {
        &token[2..]
    } else if chars.next() == Some('\u{2014}') {
        &token['\u{2014}'.len_utf8()..]
    } else {
        token
    };
    if KNOWN_REVIEW_FLAGS.contains(&name) {
        Some(format!("--{name}"))
    } else {
        None
    }
}

/// One parsed review invocation, field-for-field the Python dict: the
/// `None` observations stay present as null so the JSON record keeps one
/// shape across every transport.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ParsedInvocation {
    pub verb: String,
    pub args_raw: String,
    pub level: String,
    pub level_source: String,
    pub flags: Vec<String>,
    pub target: Option<String>,
    pub pr_number: Option<i64>,
}

/// Parse a review command without altering its raw argument string.
pub fn parse_review_invocation(raw: &str) -> Option<ParsedInvocation> {
    let candidate = raw.trim();
    if candidate.is_empty() {
        return None;
    }
    let mut parts = candidate.splitn(2, char::is_whitespace);
    let token = parts.next()?.trim_start_matches('/');
    let name = token.rsplit(':').next()?;
    if !REVIEW_VERBS.contains(&name) {
        return None;
    }
    let args_raw = parts.next().map(str::trim_start).unwrap_or("");
    // shlex.split raises on an unbalanced quote in Python; the parser falls
    // back to a whitespace split there, and `shlex::split` answering None is
    // the same shape.
    let args = shlex::split(args_raw).unwrap_or_else(|| {
        args_raw
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let first = args.first().map(String::as_str);
    let (level, level_source) = match first {
        Some("ultra") => ("ultra", "ultra_forced"),
        Some(word) if REVIEW_LEVELS.contains(&word) => (word, "explicit"),
        _ => ("unset", "fallback"),
    };
    // One flag normalizer serves the telemetry and the router prose. A known
    // flag records its canonical spelling whatever it arrived as; an unknown
    // double-hyphen token still records verbatim, so a novel flag is visible
    // in the record rather than silently dropped.
    let flags: Vec<String> = args
        .iter()
        .filter_map(|arg| match canonical_flag(arg) {
            Some(canonical) => Some(canonical),
            None if arg.starts_with("--") => Some(arg.clone()),
            None => None,
        })
        .collect();
    // The router's own target rule (skills/review/SKILL.md): strip the flags,
    // then the first token that is not the leading level token is the review's
    // subject. An all-digit subject, or a GitHub PR URL, is a PR number. The
    // hold side keys on this instead of whatever branch the session stands on,
    // so a review that names its PR holds that PR from any checkout.
    let level_led = match first {
        Some("ultra") => true,
        Some(word) => REVIEW_LEVELS.contains(&word),
        None => false,
    };
    let rest: &[String] = if level_led { &args[1..] } else { &args[..] };
    let target = rest
        .iter()
        .find(|arg| !(canonical_flag(arg).is_some() || arg.starts_with("--")))
        .cloned();
    let pr_number = target.as_deref().and_then(pr_number_of);
    Some(ParsedInvocation {
        verb: format!("/{name}"),
        args_raw: args_raw.to_string(),
        level: level.to_string(),
        level_source: level_source.to_string(),
        flags,
        target,
        pr_number,
    })
}

/// The router's PR rule: an all-digit positive subject, or a GitHub PR URL
/// with an optional trailing slash, is a PR number.
fn pr_number_of(target: &str) -> Option<i64> {
    if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
        return target.parse::<i64>().ok().filter(|n| *n > 0);
    }
    let url = regex::Regex::new(r"^https://github\.com/[^/]+/[^/]+/pull/([1-9][0-9]*)/?$").ok()?;
    url.captures(target)?.get(1)?.as_str().parse::<i64>().ok()
}

/// The invocation id. No reader parses the id, so the shape the two Rust
/// sites already mint stays: monotonic-ish nanos plus the pid, enough to
/// keep concurrent mints apart.
pub fn mint_invocation_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ri-{nanos:x}-{}", std::process::id())
}

/// The sidecar root: an explicit home wins (tests), then `FNO_HOME`, then
/// `FNO_STATE_DIR`, then `HOME/.fno` - the chain the hook spells as
/// `${FNO_HOME:-$HOME/.fno}` and Python's `paths.state_dir` resolves to.
fn invocation_home(home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        return home.to_path_buf();
    }
    for key in ["FNO_HOME", "FNO_STATE_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
}

/// The sidecar path for one target session. The session id must be a
/// non-empty path component: the sidecar is one file per session under one
/// directory, and a separator would escape it.
pub fn pending_invocation_path(
    target_session_id: &str,
    home: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    if target_session_id.is_empty()
        || Path::new(target_session_id)
            .file_name()
            .and_then(|n| n.to_str())
            != Some(target_session_id)
    {
        return Err("target_session_id must be a non-empty path component");
    }
    Ok(invocation_home(home)
        .join("review-invocations")
        .join(format!("{target_session_id}.json")))
}

/// Write one pending id without replacing a concurrent sender's id. A
/// sidecar failure is deliberately best-effort: the sender event remains
/// useful by itself, and the reviewer can mint an unjoined id if adoption is
/// unavailable.
pub fn write_pending_invocation(
    target_session_id: &str,
    invocation_id: &str,
    home: Option<&Path>,
) -> bool {
    let Ok(path) = pending_invocation_path(target_session_id, home) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    // The Python writer created the sidecar 0600; keep the mode so the join
    // id is readable only by the owner, as before.
    #[allow(unused_mut)]
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(&path) else {
        return false;
    };
    let value = serde_json::json!({
        "invocation_id": invocation_id,
        "target_session_id": target_session_id,
    });
    if std::io::Write::write_all(&mut file, value.to_string().as_bytes()).is_err() {
        // A half-written sidecar is worse than none: the adopt side consumes
        // corrupt files, but the create already won the O_EXCL slot, so
        // remove and report the failure.
        let _ = std::fs::remove_file(&path);
        return false;
    }
    true
}

/// Read AND consume a pending id, returning None on any miss. The sidecar is
/// ALWAYS consumed when it exists, even on an unparseable read: one corrupt
/// leftover must not block every later join by making the O_EXCL writer fail
/// forever while senders keep minting unjoinable ids.
pub fn adopt_pending_invocation(target_session_id: &str, home: Option<&Path>) -> Option<String> {
    let path = pending_invocation_path(target_session_id, home).ok()?;
    let mut invocation_id = None;
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            invocation_id = value
                .get("invocation_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
        }
    }
    let _ = std::fs::remove_file(&path);
    invocation_id
}

/// The fno transport stamps its journal rows with this; the fno-agents
/// door lets the emitter stamp, so the vendored copy reads unused there.
#[allow(dead_code)]
pub fn review_invocation_timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3_600,
        (remainder % 3_600) / 60,
        remainder % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure table: the flag spellings, the level words, the target and
    /// PR rules, and the quote fallback. The cross-language contract is
    /// pinned by the goldens; this test keeps the table readable in place.
    #[test]
    fn parse_table_and_canonical_flags() {
        let p = |raw: &str| parse_review_invocation(raw).unwrap();
        assert_eq!(canonical_flag("--comment").as_deref(), Some("--comment"));
        assert_eq!(canonical_flag("fix").as_deref(), Some("--fix"));
        assert_eq!(canonical_flag("\u{2014}fix").as_deref(), Some("--fix"));
        assert_eq!(canonical_flag("--novel"), None);
        assert_eq!(canonical_flag("\u{2013}comment"), None);
        assert_eq!(
            canonical_flag("verify-fixes").as_deref(),
            Some("--verify-fixes")
        );

        let parsed = p("/fno:review high --comment 123");
        assert_eq!(parsed.verb, "/review");
        assert_eq!(parsed.level, "high");
        assert_eq!(parsed.level_source, "explicit");
        assert_eq!(parsed.flags, vec!["--comment".to_string()]);
        assert_eq!(parsed.target.as_deref(), Some("123"));
        assert_eq!(parsed.pr_number, Some(123));

        let ultra = p("/review ultra --fix 42");
        assert_eq!(ultra.level, "ultra");
        assert_eq!(ultra.level_source, "ultra_forced");
        assert_eq!(ultra.pr_number, Some(42));

        let quoted = p("/review high --comment \"two words\"");
        assert_eq!(quoted.target.as_deref(), Some("two words"));

        // An unbalanced quote is the whitespace fallback: the quote survives
        // into the target, and no flag is read.
        let fallback = p("/review high 'unclosed");
        assert_eq!(fallback.target.as_deref(), Some("'unclosed"));
        assert!(fallback.flags.is_empty());

        // A bare `fix` and the one-em-dash spelling are the same flag; an
        // unknown `--` token records verbatim, and the first bare token after
        // the skipped flags is still the target (the Python leg's rule the
        // goldens pin).
        assert_eq!(p("/review fix").flags, vec!["--fix".to_string()]);
        let novel = p("/review high --novel 55");
        assert_eq!(novel.flags, vec!["--novel".to_string()]);
        assert_eq!(novel.target.as_deref(), Some("55"));

        assert_eq!(
            p("/review https://github.com/o/r/pull/77/").pr_number,
            Some(77)
        );
        assert_eq!(p("/review feature/x").target.as_deref(), Some("feature/x"));
        assert_eq!(parse_review_invocation("/compact"), None);
        assert_eq!(parse_review_invocation("   "), None);

        // The JSON record keeps every key present, nulls included.
        let json = serde_json::to_value(&parsed).unwrap();
        assert!(json.get("target").is_some());
        assert_eq!(json["pr_number"], serde_json::json!(123));
    }

    /// The sidecar contract: the component check, O_EXCL semantics, and the
    /// always-consume adopt.
    #[test]
    fn sidecar_write_adopt_and_component_check() {
        let home = std::env::temp_dir().join(format!("fno-ri-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(pending_invocation_path("", Some(&home)).is_err());
        assert!(pending_invocation_path("a/b", Some(&home)).is_err());

        assert!(write_pending_invocation("s1", "ri-1", Some(&home)));
        // O_EXCL: a second writer loses and reports false.
        assert!(!write_pending_invocation("s1", "ri-2", Some(&home)));
        // Adopt consumes: the id comes back once, the file is gone.
        assert_eq!(
            adopt_pending_invocation("s1", Some(&home)).as_deref(),
            Some("ri-1")
        );
        assert_eq!(adopt_pending_invocation("s1", Some(&home)), None);
        // A corrupt sidecar is consumed and answers None, never wedges.
        let corrupt = pending_invocation_path("s2", Some(&home)).unwrap();
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        std::fs::write(&corrupt, "{not json").unwrap();
        assert_eq!(adopt_pending_invocation("s2", Some(&home)), None);
        assert!(!corrupt.exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
