//! The reviewer lane's second GitHub identity (x-93ea): post a review verdict
//! to GitHub as `config.review.bot_identity` so a clean pass can carry APPROVE
//! and `reviewDecision` a real value. GitHub refuses an approving review from
//! the PR's author - the API accepts the call and silently records COMMENTED -
//! and one account authors every PR here, so the producer is the identity, not
//! the review pipeline.
//!
//! This is the port of the Python producer that previously lived at
//! `cli/src/fno/pr/_publish_review.py` (the file-budget gate asked for the
//! port; Python keeps the two doors and nothing else). One JSON payload on
//! stdin, one JSON answer on stdout, same contract as `spawn-axes`. The emit
//! chokepoint (`fno event emit -t review_attestation`) and the hidden
//! `fno pr publish-review` verb are the two doors; the payload is the same
//! shape for both, with the verb door passing empty `verdict` to have the
//! default resolved from the newest head-pinned attestation for HEAD.
//!
//! Fail-closed by construction, in this order: config missing -> skipped;
//! token missing -> skipped; identity collision with the PR author -> refused;
//! stale head pin -> refused; unmappable verdict -> refused. Config is read
//! BEFORE any gh call, so an unconfigured install makes zero network calls per
//! attestation emit. Only after the gate chain does the POST fire, and the
//! answer carries the `reviewDecision` GitHub reports back rather than
//! trusting the POST receipt - a receipt saying "posted APPROVE" while GitHub
//! recorded COMMENTED is the exact lie this module exists to remove.
//!
//! Each gh call is unbounded here; the Python transport (`verb_call`) bounds
//! the whole verb, so a hung network stack costs one timeout and a
//! `skipped (mirror error ...)` receipt, never a wedged emit.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One `gh` invocation. `token` set means GH_TOKEN=token in the child env
/// ONLY: the caller's own gh auth must survive the call unchanged, and the
/// token must not leak into any other subprocess this process later spawns.
pub trait Gh {
    fn run(&self, args: &[String], token: Option<&str>, cwd: &Path) -> GhOut;
}

pub struct GhOut {
    pub ok: bool,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct GhReal;

impl Gh for GhReal {
    fn run(&self, args: &[String], token: Option<&str>, cwd: &Path) -> GhOut {
        let mut cmd = Command::new("gh");
        cmd.args(args).current_dir(cwd);
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t);
        }
        match cmd.output() {
            Ok(out) => GhOut {
                ok: out.status.success(),
                code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => GhOut {
                ok: false,
                code: -1,
                stdout: String::new(),
                stderr: format!("gh spawn failed: {e}"),
            },
        }
    }
}

/// The full answer for one publish attempt. `status`:
/// - `posted`  - the POST succeeded and `review_decision` carries what GitHub
///               read back (never assumed from the POST alone)
/// - `skipped` - not configured / no token / no open PR / dry-run; posting was
///               not the right action, nothing is wrong
/// - `refused` - posting would manufacture a false verdict (identity
///               collision, stale head pin, unmappable verdict)
/// - `failed`  - gh errored or is missing; `stderr` carries the detail
pub struct Answer {
    pub status: &'static str,
    pub reason: String,
    pub event: Option<&'static str>,
    pub review_decision: Option<String>,
    pub stderr: Option<String>,
    pub receipt: String,
}

fn receipt(status: &str, reason: &str) -> String {
    // The posted line carries the event, identity, and readback inline (the
    // plan's documented shape); every other status wraps its reason.
    if status == "posted" {
        format!("bot-review: {reason}")
    } else {
        format!("bot-review: {status} ({reason})")
    }
}

impl Answer {
    fn done(status: &'static str, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Answer {
            status,
            receipt: receipt(status, &reason),
            reason,
            event: None,
            review_decision: None,
            stderr: None,
        }
    }

    /// The verb door's process exit: 0 posted or dry-run-skipped, 2 the
    /// no-attestation default miss, 1 everything else.
    pub fn exit(&self) -> i32 {
        match self.status {
            "posted" => 0,
            "skipped" if self.reason.starts_with("no head-pinned attestation") => 2,
            "skipped" if self.reason.starts_with("dry-run:") => 0,
            _ => 1,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "reason": self.reason,
            "event": self.event,
            "review_decision": self.review_decision,
            "stderr": self.stderr,
            "receipt": self.receipt,
            "exit": self.exit(),
        })
    }
}

fn answer_with_event(mut a: Answer, event: &'static str) -> Answer {
    a.event = Some(event);
    a
}

/// Drop a trailing `[bot]` for comparison (GitHub appends it to app logins).
/// `str::get` keeps a multibyte login from panicking on a non-boundary slice.
fn strip_bot(login: &str) -> &str {
    match login.get(login.len().saturating_sub(5)..) {
        Some(tail) if tail.eq_ignore_ascii_case("[bot]") => &login[..login.len() - 5],
        _ => login,
    }
}

/// owner/repo parsed from a PR url (https://github.com/owner/repo/pull/N).
/// Anchored on the `pull` segment rather than fixed indices so a host with a
/// subpath still resolves; a non-PR url reads as "" (skip) not garbage.
fn slug_from_url(url: &str) -> String {
    let segs: Vec<&str> = url.split('/').filter(|s| !s.is_empty()).collect();
    if let Some(i) = segs.iter().position(|s| *s == "pull") {
        if i >= 2 && segs.len() > i {
            return format!("{}/{}", segs[i - 2], segs[i - 1]);
        }
    }
    String::new()
}

fn verdict_event(verdict: &str) -> Option<&'static str> {
    match verdict {
        "pass" => Some("APPROVE"),
        "fail" => Some("REQUEST_CHANGES"),
        _ => None,
    }
}

/// The two `[review]` keys, parsed from one config.toml's text. Absent keys
/// read None so the caller can layer project over global per key.
fn review_keys_from(text: &str) -> (Option<String>, Option<String>) {
    let parsed: Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let review = parsed.get("review");
    let read = |key: &str| {
        review
            .and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    (read("bot_identity"), read("bot_token_env"))
}

fn home_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))?;
    Some(Path::new(&home).join(".fno").join("config.toml"))
}

/// `bot_identity` and `bot_token_env`, project-local config first, global
/// fallback per key. `load_settings_for_repo` reads `<root>/.fno/` with no
/// upward walk, so a publish invoked from a subdirectory must resolve the git
/// toplevel first or it would silently see only the global layer and report
/// the lane unconfigured.
fn review_config(cwd: &Path) -> (Option<String>, Option<String>) {
    let toplevel = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output();
    let root = match toplevel {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
        }
        _ => cwd.to_path_buf(),
    };
    let mut merged = (None, None);
    for path in [Some(root.join(".fno").join("config.toml")), home_config()] {
        let Some(path) = path else { continue };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let (identity, token_env) = review_keys_from(&text);
        if merged.0.is_none() {
            merged.0 = identity;
        }
        if merged.1.is_none() {
            merged.1 = token_env;
        }
    }
    merged
}

/// The newest `review_attestation` whose head_sha equals `head`, last-wins on
/// ts (a same-second re-emit is the newer verdict). Pure over the journal
/// paths: missing or malformed rows read as absent, never an error.
fn newest_head_attestation(journals: &[PathBuf], head: &str) -> Option<Value> {
    let mut newest: Option<(String, Value)> = None;
    for path in journals {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.get("type").and_then(Value::as_str) != Some("review_attestation") {
                continue;
            }
            let Some(data) = row.get("data").filter(|d| d.is_object()) else {
                continue;
            };
            if data.get("head_sha").and_then(Value::as_str) != Some(head) {
                continue;
            }
            let ts = row
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let take = match &newest {
                Some((best_ts, _)) => ts >= *best_ts,
                None => true,
            };
            if take {
                newest = Some((ts, data.clone()));
            }
        }
    }
    newest.map(|(_, data)| data)
}

fn git_head(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !s.is_empty() {
        Some(s)
    } else {
        None
    }
}

/// The gate chain + POST, shared by both doors. Never panics on payload
/// shape: a malformed field reads as its empty default and lands in the same
/// refusals a real value would.
pub fn publish(payload: &Value, gh: &dyn Gh, env: &dyn Fn(&str) -> Option<String>) -> Answer {
    let cwd = Path::new(payload.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let dry_run = payload
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pr_number = payload.get("pr_number").and_then(Value::as_u64);

    // Default resolution (the verb door): verdict/head/reviewer come from the
    // newest head-pinned attestation for HEAD, never from memory.
    let mut verdict = payload
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut head_sha = payload
        .get("head_sha")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut reviewer = payload
        .get("reviewer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if verdict.is_empty() {
        let head = if head_sha.is_empty() {
            match git_head(cwd) {
                Some(h) => h,
                None => {
                    return Answer::done("skipped", "could not read git HEAD");
                }
            }
        } else {
            head_sha.clone()
        };
        let toplevel_journals = vec![
            crate::paths::events_path(cwd),
            crate::paths::worktree_repo_root(cwd)
                .join(".fno")
                .join("events.jsonl"),
        ];
        match newest_head_attestation(&toplevel_journals, &head) {
            Some(att) => {
                verdict = att
                    .get("verdict")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                head_sha = att
                    .get("head_sha")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                reviewer = att
                    .get("reviewer")
                    .and_then(Value::as_str)
                    .unwrap_or("manual")
                    .to_string();
            }
            None => {
                return Answer::done(
                    "skipped",
                    "no head-pinned attestation for HEAD; pass --verdict explicitly",
                );
            }
        }
    }
    if reviewer.is_empty() {
        reviewer = "manual".to_string();
    }

    // 1. Config + token. Missing either is a skip, not an error: an
    //    unconfigured lane must behave identically to today apart from the
    //    receipt line, and the config check sits BEFORE any gh call.
    // An unreadable or absent config layer reads as unconfigured (skipped),
    // never an error: a garbage config.toml must not fail the emit.
    let (identity, token_env) = review_config(cwd);
    let Some(identity) = identity else {
        return Answer::done("skipped", "review.bot_identity unset");
    };
    let Some(token_env) = token_env else {
        return Answer::done("skipped", "review.bot_token_env unset");
    };
    // A fine-grained PAT in an env var is deliberately the whole token story:
    // a GitHub App installation token (JWT signing + hourly refresh) is the
    // upgrade path when more than this one repo needs the reviewer identity.
    // The env lookup is injected so the tests stay hermetic: process env is
    // shared by every parallel test thread in the binary, and a set/remove
    // race there reads as a flaky skip.
    let token = env(&token_env).unwrap_or_default();
    if token.is_empty() {
        return Answer::done("skipped", format!("${token_env} unset or empty"));
    }

    // 2-4. One combined gh read answers the PR to post on, its author, its
    //    head, and the repo slug (parsed from the PR url).
    let mut view: Vec<String> = vec!["pr".to_string(), "view".to_string()];
    if let Some(n) = pr_number {
        view.push(n.to_string());
    }
    view.extend(
        ["--json", "number,author,headRefOid,url"]
            .iter()
            .map(|s| s.to_string()),
    );
    let out = gh.run(&view, None, cwd);
    let fields: Value = if out.ok {
        serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    if fields.is_null() {
        let reason = match pr_number {
            Some(n) => format!("could not read #{n}"),
            None => "no open PR for HEAD".to_string(),
        };
        return Answer::done("skipped", reason);
    }
    let resolved_number = fields.get("number").and_then(Value::as_u64);
    let number = resolved_number.or(pr_number);
    let Some(number) = number else {
        return Answer::done("skipped", "could not read PR number");
    };
    let slug = slug_from_url(fields.get("url").and_then(Value::as_str).unwrap_or(""));

    // Identity collision. This is the whole defect the node exists to fix:
    // posting as the PR author would have GitHub accept the call and record
    // COMMENTED - manufacturing the exact empty-reviewDecision lie.
    let author = fields
        .get("author")
        .and_then(|a| a.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if author.is_empty() {
        return Answer::done("skipped", format!("could not read author of #{number}"));
    }
    if strip_bot(author) == strip_bot(&identity) {
        return Answer::done(
            "refused",
            format!("bot identity {identity} is the PR author"),
        );
    }

    // Head pin. The attestation is evidence about ONE commit; approving a PR
    // whose head has moved past it approves commits nobody reviewed. A stale
    // approval is worse than no approval.
    let pr_head = fields
        .get("headRefOid")
        .and_then(Value::as_str)
        .unwrap_or("");
    if pr_head.is_empty() {
        return Answer::done("skipped", format!("could not read head sha of #{number}"));
    }
    if pr_head != head_sha {
        let short_a: String = head_sha.chars().take(8).collect();
        let short_h: String = pr_head.chars().take(8).collect();
        return Answer::done(
            "refused",
            format!("stale pin: attested {short_a} but PR head is {short_h}"),
        );
    }

    // 5. Verdict map. Anything the attestation schema does not enumerate is a
    //    verdict no GitHub event exists for; posting it as a comment would be
    //    the empty-reviewDecision shape again.
    let Some(event) = verdict_event(&verdict) else {
        return Answer::done("refused", format!("unmappable verdict {verdict:?}"));
    };

    if dry_run {
        return answer_with_event(
            Answer::done(
                "skipped",
                format!("dry-run: would post {event} as {identity}"),
            ),
            event,
        );
    }

    // 6. The POST, authenticated as the bot in the subprocess env ONLY.
    if slug.is_empty() {
        return Answer::done("skipped", "could not resolve repo slug");
    }
    let body = format!("fno review mirror: reviewer={reviewer} verdict={verdict} head={head_sha}");
    let post: Vec<String> = [
        "api",
        "-X",
        "POST",
        &format!("/repos/{slug}/pulls/{number}/reviews"),
        "-f",
        &format!("event={event}"),
        "-f",
        &format!("commit_id={head_sha}"),
        "-f",
        &format!("body={body}"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = gh.run(&post, Some(&token), cwd);
    if !out.ok {
        let mut a = Answer::done(
            "failed",
            format!("gh api POST reviews failed (exit {})", out.code),
        );
        a.stderr = Some(out.stderr.trim().to_string());
        return answer_with_event(a, event);
    }

    // 7. Read back what GitHub actually recorded. Load-bearing, not
    //    decoration: the readback is the only honest answer to "did it land
    //    as APPROVED?".
    let readback: Vec<String> = [
        "pr",
        "view",
        &number.to_string(),
        "--json",
        "reviewDecision",
        "--jq",
        ".reviewDecision",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = gh.run(&readback, None, cwd);
    let decision = out.stdout.trim();
    let decision = if out.ok && !decision.is_empty() && decision != "null" {
        Some(decision.to_string())
    } else {
        None
    };
    let mut a = Answer {
        status: "posted",
        review_decision: decision.clone(),
        event: Some(event),
        stderr: None,
        receipt: String::new(),
        reason: format!(
            "posted {event} as {identity} on #{number} (reviewDecision={})",
            decision.unwrap_or_default()
        ),
    };
    a.receipt = receipt("posted", &a.reason);
    a
}

/// The verb entry: one JSON payload on stdin, one JSON answer on stdout,
/// always exit 0 (the answer carries the caller-facing exit under "exit";
/// only a transport failure - unreadable or malformed stdin - exits 2).
pub fn run_publish_review(_args: &[String]) -> i32 {
    use std::io::Read;

    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        eprint!("publish-review: cannot read payload\n");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("publish-review: bad payload: {e}\n");
            return 2;
        }
    };
    println!(
        "{}",
        publish(&parsed, &GhReal, &|name: &str| std::env::var(name).ok()).to_json()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted gh: pops one GhOut per call and records the argv so tests can
    /// assert both answers and the exact commands (no POST may fire on a
    /// refusal path - the call log proves the absence).
    struct GhFake {
        answers: std::sync::Mutex<Vec<GhOut>>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl GhFake {
        fn new(answers: Vec<GhOut>) -> Self {
            GhFake {
                answers: std::sync::Mutex::new(answers),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        fn gh_env(token: &str) -> String {
            format!("gh-token:{token}")
        }
    }

    impl Gh for GhFake {
        fn run(&self, args: &[String], token: Option<&str>, _cwd: &Path) -> GhOut {
            self.calls.lock().unwrap().push(args.to_vec());
            // Fail closed on an exhausted script: an unexpected call reads as
            // a failing gh, never an index panic.
            let mut answers = self.answers.lock().unwrap();
            let mut out = if answers.is_empty() {
                GhOut {
                    ok: false,
                    code: 1,
                    stdout: String::new(),
                    stderr: "gh-fake: unexpected call".to_string(),
                }
            } else {
                answers.remove(0)
            };
            drop(answers);
            if let Some(t) = token {
                // Prove the token rode the child env, not the caller's.
                out.stdout = format!("{}\n{}", GhFake::gh_env(t), out.stdout);
            }
            out
        }
    }

    fn view_answer() -> GhOut {
        GhOut {
            ok: true,
            code: 0,
            stdout: r#"{"number": 931, "author": {"login": "bllshttng"}, "headRefOid": "abc123", "url": "https://github.com/bllshttng/footnote/pull/931"}"#.to_string(),
            stderr: String::new(),
        }
    }

    fn post_ok() -> GhOut {
        GhOut {
            ok: true,
            code: 0,
            stdout: r#"{"id": 1}"#.to_string(),
            stderr: String::new(),
        }
    }

    fn readback(decision: &str) -> GhOut {
        GhOut {
            ok: true,
            code: 0,
            stdout: format!("{decision}\n"),
            stderr: String::new(),
        }
    }

    fn write_config(dir: &Path, identity: &str, token_env: &str) {
        let dir = dir.join(".fno");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!("[review]\nbot_identity = \"{identity}\"\nbot_token_env = \"{token_env}\"\n"),
        )
        .unwrap();
    }

    /// Config paths derive from `cwd`; each test gets its own temp dir so the
    /// suite runs in parallel. HOME is not consulted: the project layer
    /// carries both keys.
    fn temp_repo(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-publish-review-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn verdict_maps_pass_and_fail_and_refuses_the_rest() {
        assert_eq!(verdict_event("pass"), Some("APPROVE"));
        assert_eq!(verdict_event("fail"), Some("REQUEST_CHANGES"));
        assert_eq!(verdict_event("comment"), None);
        assert_eq!(verdict_event(""), None);
    }

    #[test]
    fn strip_bot_compares_app_suffixes_away() {
        assert_eq!(strip_bot("fno-review-bot[bot]"), "fno-review-bot");
        assert_eq!(strip_bot("fno-review-bot"), "fno-review-bot");
        assert_eq!(strip_bot("FNO-BOT[BOT]"), "FNO-BOT");
        // A multibyte login must not panic on a non-char-boundary slice.
        assert_eq!(strip_bot("日本語ボット"), "日本語ボット");
        assert_eq!(strip_bot("[bot]"), "");
    }

    #[test]
    fn slug_parses_from_the_pull_segment() {
        assert_eq!(
            slug_from_url("https://github.com/owner/repo/pull/931"),
            "owner/repo"
        );
        assert_eq!(
            slug_from_url("https://ghe.example.com/git/owner/repo/pull/7"),
            "owner/repo"
        );
        assert_eq!(slug_from_url("https://github.com/owner/repo"), "");
        assert_eq!(slug_from_url(""), "");
    }

    #[test]
    fn config_reads_both_keys_and_tolerates_garbage() {
        let (identity, token_env) = review_keys_from(
            "[review]\nbot_identity = \"fno-review-bot\"\nbot_token_env = \"GH_REVIEW_BOT_TOKEN\"\n",
        );
        assert_eq!(identity.as_deref(), Some("fno-review-bot"));
        assert_eq!(token_env.as_deref(), Some("GH_REVIEW_BOT_TOKEN"));
        // Absent, empty, and malformed all read None - a config that cannot
        // be parsed is an unconfigured lane (skipped), never a crash.
        assert_eq!(review_keys_from("[review]\n"), (None, None));
        assert_eq!(
            review_keys_from("[review]\nbot_identity = \"\"\n"),
            (None, None)
        );
        assert_eq!(review_keys_from("not toml at all {"), (None, None));
    }

    #[test]
    fn attestation_scan_takes_the_newest_head_pinned_row() {
        let dir = temp_repo("scan");
        let journal = dir.join("events.jsonl");
        std::fs::write(
            &journal,
            concat!(
                "{\"type\":\"review_attestation\",\"ts\":\"2026-09-11T10:00:00Z\",\"data\":{\"head_sha\":\"abc\",\"verdict\":\"fail\",\"reviewer\":\"r1\"}}\n",
                "{\"type\":\"other\",\"ts\":\"2026-09-11T11:00:00Z\",\"data\":{\"head_sha\":\"abc\"}}\n",
                "not json\n",
                "{\"type\":\"review_attestation\",\"ts\":\"2026-09-11T12:00:00Z\",\"data\":{\"head_sha\":\"other\",\"verdict\":\"pass\"}}\n",
                "{\"type\":\"review_attestation\",\"ts\":\"2026-09-11T13:00:00Z\",\"data\":{\"head_sha\":\"abc\",\"verdict\":\"pass\",\"reviewer\":\"r2\"}}\n",
            ),
        )
        .unwrap();
        let hit = newest_head_attestation(&[journal.clone()], "abc").unwrap();
        assert_eq!(hit.get("verdict").and_then(Value::as_str), Some("pass"));
        assert_eq!(hit.get("reviewer").and_then(Value::as_str), Some("r2"));
        assert!(newest_head_attestation(&[journal], "nope").is_none());
        assert!(newest_head_attestation(&[dir.join("absent.jsonl")], "abc").is_none());
    }

    #[test]
    fn unconfigured_lane_skips_without_any_gh_call() {
        // A temp-dir cwd carries no project config; the test also assumes the
        // running environment's global config layer names no bot keys (true
        // on CI and on any machine that has not configured the lane).
        let dir = temp_repo("unconfigured");
        let fake = GhFake::new(vec![]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "pass",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| None,
        );
        assert_eq!(answer.status, "skipped");
        assert!(answer.reason.contains("bot_identity"));
        // The config check sits BEFORE any gh call: the fake never ran.
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn missing_token_env_var_skips_without_any_gh_call() {
        let dir = temp_repo("no-token");
        write_config(&dir, "fno-review-bot", "FNO_PUBLISH_REVIEW_TEST_UNSET");
        let fake = GhFake::new(vec![]);
        // Hermetic: the token lookup is injected, so an unset name is a None
        // without touching the process env.
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "pass",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| None,
        );
        assert_eq!(answer.status, "skipped");
        assert!(answer.reason.contains("FNO_PUBLISH_REVIEW_TEST_UNSET"));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn clean_pass_posts_approve_and_reads_the_decision_back() {
        let dir = temp_repo("posted");
        write_config(&dir, "fno-review-bot", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![view_answer(), post_ok(), readback("APPROVED")]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "pass",
                    "reviewer": "local_attestation", "cwd": dir.to_string_lossy(),
                    "dry_run": false}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "posted");
        assert_eq!(answer.review_decision.as_deref(), Some("APPROVED"));
        assert_eq!(
            answer.receipt,
            "bot-review: posted APPROVE as fno-review-bot on #931 (reviewDecision=APPROVED)"
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].iter().any(|a| a.contains("event=APPROVE")));
        assert!(calls[1].iter().any(|a| a.contains("commit_id=abc123")));
        assert!(calls[2].contains(&".reviewDecision".to_string()));
    }

    #[test]
    fn fail_verdict_posts_request_changes() {
        let dir = temp_repo("fail");
        write_config(&dir, "fno-review-bot", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![view_answer(), post_ok(), readback("")]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "fail",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "posted");
        assert_eq!(answer.event, Some("REQUEST_CHANGES"));
        assert_eq!(answer.review_decision, None);
        assert!(answer.receipt.ends_with("(reviewDecision=)"));
    }

    #[test]
    fn identity_collision_refuses_before_any_post() {
        let dir = temp_repo("collision");
        write_config(&dir, "bllshttng", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![view_answer()]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "pass",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "refused");
        assert!(answer.reason.contains("is the PR author"));
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn stale_head_pin_refuses_before_any_post() {
        let dir = temp_repo("stale");
        write_config(&dir, "fno-review-bot", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![view_answer()]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "deadbeef", "verdict": "pass",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "refused");
        assert!(answer.reason.contains("stale pin"));
        assert!(answer.reason.contains("attested deadbeef"));
        assert!(answer.reason.contains("PR head is abc123"));
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn unmappable_verdict_refuses_before_any_post() {
        let dir = temp_repo("bad-verdict");
        write_config(&dir, "fno-review-bot", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![view_answer()]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "comment",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "refused");
        assert!(answer.reason.contains("unmappable verdict"));
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn dry_run_resolves_and_refuse_checks_but_never_posts() {
        let dir = temp_repo("dry");
        write_config(&dir, "fno-review-bot", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![view_answer()]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "pass",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": true}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "skipped");
        assert_eq!(answer.event, Some("APPROVE"));
        assert!(answer.reason.starts_with("dry-run: would post APPROVE"));
        assert_eq!(answer.exit(), 0);
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn failed_post_carries_stderr_and_never_panics() {
        let dir = temp_repo("post-fail");
        write_config(&dir, "fno-review-bot", "GH_REVIEW_BOT_TOKEN");
        let fake = GhFake::new(vec![
            view_answer(),
            GhOut {
                ok: false,
                code: 1,
                stdout: String::new(),
                stderr: "HTTP 403: Not Allowed".to_string(),
            },
        ]);
        let answer = publish(
            &json!({"pr_number": 931, "head_sha": "abc123", "verdict": "pass",
                    "reviewer": "r", "cwd": dir.to_string_lossy(), "dry_run": false}),
            &fake,
            &|_| Some("tok-123".to_string()),
        );
        assert_eq!(answer.status, "failed");
        assert_eq!(answer.stderr.as_deref(), Some("HTTP 403: Not Allowed"));
        assert_eq!(answer.exit(), 1);
    }

    #[test]
    fn exit_codes_carry_the_verb_door_contract() {
        let posted = Answer::done(
            "posted",
            "posted APPROVE as b on #1 (reviewDecision=APPROVED)",
        );
        assert_eq!(posted.exit(), 0);
        let dry = Answer::done("skipped", "dry-run: would post APPROVE as b");
        assert_eq!(dry.exit(), 0);
        let no_att = Answer::done(
            "skipped",
            "no head-pinned attestation for HEAD; pass --verdict explicitly",
        );
        assert_eq!(no_att.exit(), 2);
        let refused = Answer::done("refused", "bot identity b is the PR author");
        assert_eq!(refused.exit(), 1);
    }
}
