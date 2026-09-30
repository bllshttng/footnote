//! The ready-for-review draft guard: one config key, two enforcement points.
//!
//! Ported from the deleted `cli/src/fno/pr/_draft_ready.py`: `cli/src/fno`
//! bars new Python (it is the compatibility shell), so the decision lives
//! here and Python forwards through the `graph-get` stdin door, the tracker
//! seam's shape.
//!
//! `config.pr.open_ready` (default true) is the user's rule - pull requests
//! open ready for review, never draft, unless the user asks in the moment -
//! as a knob instead of rule text. Two consumers:
//!
//! - The gh proxy's delegate path refuses draft-intent argv (`gh pr create
//!   --draft`, `gh pr ready <n> --draft`) from any Footnote-launched process;
//!   `worker_environment` puts the shim on every spawned worker's PATH, so
//!   this is the one door all six harnesses cross.
//! - The pr-watch sweep flips an open fno-bound draft PR back to ready and
//!   journals the flip as `pr_watch_draft_flip` (schema.yaml).
//!
//! The one escape is an operator law row at a `pr-draft:` subject whose
//! decision equals [`DRAFT_DECISION`], and only an `authority_source ==
//! "operator"` row can carry it - a draft exception is the user's call, the
//! same strictness the review-coverage waiver gate records.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The one decision value that counts as an affirmative draft ruling. The
/// `fno inbox law set` door mints exactly this string; row existence carries
/// no polarity (a note at the subject is not a waiver).
pub const DRAFT_DECISION: &str = "draft permitted for this pull request";

const DRAFT_TOKENS: [&str; 2] = ["--draft", "--draft=true"];

/// Which argv shape carried the draft intent; the subject keys on the branch
/// for `create` (no PR exists yet) and on the repo+number for `ready`.
#[derive(Debug, PartialEq, Eq)]
enum Intent {
    Create,
    Ready(Option<u32>),
}

/// Drop gh-wide options so equivalent command spellings share one policy -
/// the port of `_quota.command_args`: leading `-R/--repo/--hostname`
/// (separate or `=`-joined) fall off, then again after the `pr` word.
fn command_args(args: &[String]) -> Vec<String> {
    fn strip(tokens: &[String]) -> Vec<String> {
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            if token == "-R" || token == "--repo" || token == "--hostname" {
                if i + 1 >= tokens.len() {
                    return Vec::new();
                }
                i += 2;
            } else if (token.starts_with("-R") && token != "-R")
                || token.starts_with("--repo=")
                || token.starts_with("--hostname=")
            {
                i += 1;
            } else {
                break;
            }
        }
        tokens[i..].to_vec()
    }
    let stripped = strip(args);
    if stripped.first().map(String::as_str) == Some("pr") {
        let mut out = vec!["pr".to_string()];
        out.extend(strip(&stripped[1..]));
        return out;
    }
    stripped
}

/// Draft intent in `command_args`-normalized gh argv. `--draft=false` creates
/// ready in gh and is not intent; a numberless `pr ready --draft` is gh DWIM
/// against the current branch's PR and still is.
fn draft_intent(command: &[String]) -> Option<Intent> {
    if command.len() < 2 || command[0] != "pr" {
        return None;
    }
    let (sub, rest) = (&command[1], &command[2..]);
    if !rest.iter().any(|t| DRAFT_TOKENS.contains(&t.as_str())) {
        return None;
    }
    let numbered = |t: &String| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    match sub.as_str() {
        "create" => Some(Intent::Create),
        "ready" => Some(Intent::Ready(
            rest.iter()
                .find(|t| numbered(t))
                .and_then(|t| t.parse().ok()),
        )),
        _ => None,
    }
}

/// `pr.open_ready` for the repo at cwd; default true on any load failure -
/// the rule's default state is on, and a broken config file never disables a
/// guard. A quoted "true"/"false" coerces (the stored-type trap); anything
/// else reads as the default.
fn open_ready(cwd: Option<&Path>) -> bool {
    let Some(cwd) = cwd else {
        return true;
    };
    crate::agents_config::config_lookup(cwd, &["pr", "open_ready"])
        .and_then(|value| match value {
            toml::Value::Boolean(b) => Some(b),
            toml::Value::String(s) if s == "true" => Some(true),
            toml::Value::String(s) if s == "false" => Some(false),
            _ => None,
        })
        .unwrap_or(true)
}

/// The project scope rows are read under: `Some(slug)` filters, `None` fails
/// open and hides nothing (`retain_in_scope`'s own rule).
fn scope_project(cwd: Option<&Path>) -> Option<String> {
    crate::law_match::resolve_project(cwd, &crate::law_match::settings_sources()).ok()
}

/// Three-state law resolution for one `pr-draft:` subject: `(status, probe)`
/// with status `single`/`none`/`unknown`, the shape of the Python coverage
/// gate's `law_authority`. Only an operator-source row whose decision equals
/// [`DRAFT_DECISION`] counts; a dead probe, damaged rows, or conflicting rows
/// are unknown, never none (fail toward refusing the draft).
fn law_authority(subject: &str, cwd: Option<&Path>) -> (&'static str, String) {
    let mut index = match crate::decision_index::default_store_live() {
        Ok(index) => index,
        Err(e) => {
            return (
                "unknown",
                format!("decision probe failed for {subject}: {e}"),
            )
        }
    };
    crate::decision_index::retain_in_scope(&mut index, scope_project(cwd).as_deref());
    let rows: Vec<&Value> = index
        .rows
        .iter()
        .filter(|row| crate::decision_index::is_law(row))
        .filter(|row| {
            row.get("subject")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case(subject))
        })
        .filter(|row| row.get("authority_source").and_then(Value::as_str) == Some("operator"))
        .collect();
    if index.damaged > 0 {
        let noun = if index.damaged == 1 { "row" } else { "rows" };
        return (
            "unknown",
            format!(
                "decision probe: {} damaged {noun} for {subject}",
                index.damaged
            ),
        );
    }
    match rows.as_slice() {
        [] => ("none", String::new()),
        [row] => match row.get("decision").and_then(Value::as_str) {
            None => (
                "unknown",
                format!("decision probe: single law row carries no decision for {subject}"),
            ),
            Some(d) if d == DRAFT_DECISION => ("single", String::new()),
            Some(_) => ("none", String::new()),
        },
        _ => (
            "unknown",
            format!("decision probe: conflicting law rows for {subject}"),
        ),
    }
}

/// One bounded subprocess: `(exit, stdout, stderr)`, killed at `secs`. A hung
/// `git`/`gh` must not hang the proxy door or the sweep tick.
fn run_bounded(argv: &[String], cwd: Option<&Path>, secs: u64) -> (i32, String, String) {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return (127, String::new(), String::new()),
    };
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut out);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut err);
                }
                return (status.code().unwrap_or(127), out, err);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (124, String::new(), "timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return (127, String::new(), String::new()),
        }
    }
}

fn current_branch(cwd: Option<&Path>) -> Option<String> {
    let argv = vec![
        "git".to_string(),
        "branch".to_string(),
        "--show-current".to_string(),
    ];
    let (code, out, _) = run_bounded(&argv, cwd, 10);
    if code == 0 {
        let branch = out.trim().to_string();
        if !branch.is_empty() {
            return Some(branch);
        }
    }
    None
}

fn refusal_text(subject: &str) -> String {
    format!(
        "gh proxy: --draft refused (config.pr.open_ready): pull requests open \
         ready for review, never draft. To grant this one, record the operator \
         ruling from your own terminal: fno inbox law set {subject} \
         \"draft permitted for this pull request\" --rationale \"<why>\". \
         To turn the rule off for this repo: config pr.open_ready = false."
    )
}

/// The proxy guard: draft-intent argv is refused unless the repo stood the
/// rule down or an operator ruling spares this subject. Everything else is
/// admitted without a config or law read, so the ordinary gh call pays
/// nothing.
fn check(args: &[String], cwd: Option<&str>) -> Value {
    let command = command_args(args);
    let Some(intent) = draft_intent(&command) else {
        return json!({ "admitted": true });
    };
    let cwd_path = cwd.map(PathBuf::from);
    if !open_ready(cwd_path.as_deref()) {
        return json!({ "admitted": true });
    }
    let subject = match intent {
        Intent::Ready(Some(n)) => {
            let slug = crate::backlog::pr_link::resolve_current_repo_slug(cwd)
                .unwrap_or_else(|| "unknown-repo".to_string());
            format!("pr-draft:{slug}#{n}")
        }
        Intent::Ready(None) | Intent::Create => {
            let branch =
                current_branch(cwd_path.as_deref()).unwrap_or_else(|| "unknown-branch".to_string());
            format!("pr-draft:{branch}")
        }
    };
    let (status, _probe) = law_authority(&subject, cwd_path.as_deref());
    if status == "single" {
        return json!({ "admitted": true });
    }
    json!({ "admitted": false, "refusal": refusal_text(&subject) })
}

/// The sweep flip: one observed draft PR goes back to ready unless the rule
/// stood down or an operator ruling spares it. The flip is pinned to the
/// candidate's repo (`--repo slug`): gh resolves a bare PR number against the
/// process cwd's repo, and a candidate with no repo_dir would otherwise flip
/// a same-numbered PR in whatever repo the daemon sits in. Never refuses the
/// tick: any failure is one `error` outcome row, never a raise.
fn flip(spec: &Value, gh: &dyn Fn(&[String], Option<&Path>) -> (i32, String, String)) -> Value {
    let pr = spec.get("pr").and_then(Value::as_i64).unwrap_or(0);
    let repo = spec
        .get("repo")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let node = spec.get("node").and_then(Value::as_str).unwrap_or("");
    let cwd = spec
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let journal = spec
        .get("journal")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::decision_index::default_state_path("events.jsonl"));

    let subject = format!("pr-draft:{}#{pr}", repo.unwrap_or("unknown-repo"));
    let (status, _probe) = law_authority(&subject, cwd.as_deref());
    if status == "single" {
        return json!({
            "outcome": "spared",
            "receipt": format!("spared by operator ruling at {subject}"),
        });
    }
    if !open_ready(cwd.as_deref()) {
        return json!({ "outcome": "stand_down", "receipt": "open_ready=false; no flip" });
    }
    let mut cmd = vec![
        "gh".to_string(),
        "pr".to_string(),
        "ready".to_string(),
        pr.to_string(),
    ];
    if let Some(slug) = repo {
        cmd.push("--repo".to_string());
        cmd.push(slug.to_string());
    }
    let (code, stdout, stderr) = gh(&cmd, cwd.as_deref());
    let outcome = if code == 0 { "flipped" } else { "error" };
    let mut data = json!({
        "pr": pr,
        "repo": repo.unwrap_or("unknown-repo"),
        "node": node,
        "outcome": outcome,
    });
    let receipt = if code == 0 {
        "flipped".to_string()
    } else {
        let text = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        let err: String = text.chars().take(200).collect();
        data["error"] = json!(err);
        format!("flip refused: {err}")
    };
    let emitted = crate::events::EventEmitter::new(&journal, "daemon")
        .emit("pr_watch_draft_flip", &data)
        .is_ok();
    json!({ "outcome": outcome, "receipt": receipt, "journaled": emitted })
}

/// The `graph-get` stdin door arm (`{"pr_draft_ready": ...}`), the tracker
/// door's shape: one JSON answer, exit 0, refusals ride the payload.
pub fn run_door(payload: &Value) -> Value {
    let op = payload.get("pr_draft_ready").unwrap_or(&Value::Null);
    if let Some(args) = op.get("check").and_then(Value::as_array) {
        let args: Vec<String> = args
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let cwd = op
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        return check(&args, cwd);
    }
    if let Some(spec) = op.get("flip") {
        return flip(spec, &|argv, cwd| run_bounded(argv, cwd, 30));
    }
    json!({ "error": "pr_draft_ready needs a check or flip op" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::claims::test_env_lock().lock().unwrap()
    }

    /// A hermetic state root: FNO_HOME and FNO_STATE_DIR at the fixture dir,
    /// FNO_CONFIG at a path the test controls (absent by default, so every
    /// config getter takes its default and no machine file leaks in).
    fn hermetic(dir: &Path) {
        std::env::set_var("FNO_HOME", dir);
        std::env::set_var("FNO_STATE_DIR", dir);
        std::env::set_var("FNO_CONFIG", dir.join("config.toml"));
    }

    fn law_envelope(id: &str, subject: &str, decision: &str, authority: &str) -> String {
        format!(
            "{{\"type\":\"operator_decision\",\"ts\":\"2026-09-29T00:00:00Z\",\"data\":{{\
             \"decision_id\":\"{id}\",\"subject\":\"{subject}\",\"decision\":\"{decision}\",\
             \"authority_source\":\"{authority}\"}}}}"
        )
    }

    fn seed_laws(dir: &Path, lines: &[String]) {
        fs::write(dir.join("decisions.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn command_args_drops_gh_wide_options() {
        let s = |xs: &[&str]| -> Vec<String> { xs.iter().map(|x| x.to_string()).collect() };
        assert_eq!(
            command_args(&s(&["-R", "o/r", "pr", "create", "--draft"])),
            s(&["pr", "create", "--draft"])
        );
        assert_eq!(
            command_args(&s(&["pr", "--hostname=x", "ready", "7"])),
            s(&["pr", "ready", "7"])
        );
        assert_eq!(command_args(&s(&["-R"])), Vec::<String>::new());
    }

    #[test]
    fn draft_intent_table() {
        let s = |xs: &[&str]| -> Vec<String> { xs.iter().map(|x| x.to_string()).collect() };
        assert_eq!(
            draft_intent(&command_args(&s(&["pr", "create", "--draft"]))),
            Some(Intent::Create)
        );
        assert_eq!(
            draft_intent(&command_args(&s(&["pr", "create", "--draft=true"]))),
            Some(Intent::Create)
        );
        assert_eq!(
            draft_intent(&command_args(&s(&["pr", "create", "--draft=false"]))),
            None
        );
        assert_eq!(draft_intent(&command_args(&s(&["pr", "view", "9"]))), None);
        assert_eq!(draft_intent(&command_args(&s(&["auth", "status"]))), None);
        assert_eq!(
            draft_intent(&command_args(&s(&["pr", "ready", "7", "--draft"]))),
            Some(Intent::Ready(Some(7)))
        );
        assert_eq!(
            draft_intent(&command_args(&s(&["pr", "ready", "--draft"]))),
            Some(Intent::Ready(None))
        );
        assert_eq!(
            draft_intent(&command_args(&s(&["pr", "ready", "--draft=false"]))),
            None
        );
    }

    #[test]
    fn open_ready_coerces_and_defaults_true() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-open-ready-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        hermetic(&dir);

        fs::remove_file(&config).ok();
        assert!(open_ready(Some(&dir)), "no file reads as the default on");

        fs::write(&config, "[pr]\nopen_ready = false\n").unwrap();
        assert!(!open_ready(Some(&dir)));

        fs::write(&config, "[pr]\nopen_ready = \"false\"\n").unwrap();
        assert!(!open_ready(Some(&dir)), "a quoted false coerces");

        fs::write(&config, "[pr]\nopen_ready = 3\n").unwrap();
        assert!(
            open_ready(Some(&dir)),
            "a malformed value reads as the default on"
        );

        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn law_authority_three_states() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-law-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);
        let subject = "pr-draft:owner/repo#7";

        assert_eq!(
            law_authority(subject, None).0,
            "none",
            "an empty store is none"
        );

        seed_laws(
            &dir,
            vec![law_envelope("d-1", subject, DRAFT_DECISION, "operator")],
        );
        assert_eq!(law_authority(subject, None).0, "single");

        seed_laws(
            &dir,
            vec![law_envelope(
                "d-2",
                subject,
                DRAFT_DECISION,
                "chat_attested",
            )],
        );
        assert_eq!(law_authority(subject, None).0, "none", "chat cannot grant");

        seed_laws(
            &dir,
            vec![law_envelope("d-3", subject, "a note", "operator")],
        );
        assert_eq!(
            law_authority(subject, None).0,
            "none",
            "a note is no waiver"
        );

        seed_laws(
            &dir,
            vec![
                law_envelope("d-4", subject, DRAFT_DECISION, "operator"),
                law_envelope("d-5", subject, DRAFT_DECISION, "operator"),
            ],
        );
        assert_eq!(law_authority(subject, None).0, "unknown", "conflicting");

        seed_laws(
            &dir,
            vec![
                "this line is not json".to_string(),
                law_envelope("d-6", subject, DRAFT_DECISION, "operator"),
            ],
        );
        let (status, probe) = law_authority(subject, None);
        assert_eq!(status, "unknown", "a damaged probe is never none");
        assert!(probe.contains("damaged 1 rows") || probe.contains("damaged 1 row"));

        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn check_refuses_draft_and_names_the_door() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-check-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);

        let reply = check(
            &[
                "pr".to_string(),
                "create".to_string(),
                "--draft".to_string(),
            ],
            None,
        );
        assert_eq!(reply["admitted"], json!(false));
        let refusal = reply["refusal"].as_str().unwrap();
        assert!(refusal.contains("pr-draft:unknown-branch"), "{refusal}");
        assert!(refusal.contains(DRAFT_DECISION), "{refusal}");
        assert!(refusal.contains("fno inbox law set"), "{refusal}");

        let ruling = law_envelope("d-7", "pr-draft:unknown-branch", DRAFT_DECISION, "operator");
        seed_laws(&dir, vec![ruling]);
        let reply = check(
            &[
                "pr".to_string(),
                "create".to_string(),
                "--draft".to_string(),
            ],
            None,
        );
        assert_eq!(reply["admitted"], json!(true), "a ruling spares the draft");

        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn check_admits_ordinary_argv_and_standdown() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-admit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);

        let reply = check(&["auth".to_string(), "status".to_string()], None);
        assert_eq!(reply["admitted"], json!(true));

        fs::write(dir.join("config.toml"), "[pr]\nopen_ready = false\n").unwrap();
        let reply = check(
            &[
                "pr".to_string(),
                "create".to_string(),
                "--draft".to_string(),
            ],
            None,
        );
        assert_eq!(reply["admitted"], json!(true), "the rule stood down");

        let reply = check(
            &[
                "pr".to_string(),
                "ready".to_string(),
                "7".to_string(),
                "--draft".to_string(),
            ],
            Some("/nonexistent-x-3159"),
        );
        assert_eq!(reply["admitted"], json!(true));
        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn check_keys_the_ready_subject_on_the_repo() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-ready-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);
        let reply = check(
            &[
                "pr".to_string(),
                "ready".to_string(),
                "7".to_string(),
                "--draft".to_string(),
            ],
            Some("/nonexistent-x-3159"),
        );
        let refusal = reply["refusal"].as_str().unwrap();
        assert!(refusal.contains("pr-draft:unknown-repo#7"), "{refusal}");
        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    fn flip_spec(journal: &Path) -> Value {
        json!({
            "pr": 7,
            "repo": "owner/repo",
            "node": "x-abc12345",
            "cwd": null,
            "journal": journal.to_string_lossy(),
        })
    }

    #[test]
    fn flip_spared_or_stood_down_never_calls_gh() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-flip-a-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);
        let journal = dir.join("events.jsonl");
        let no_gh = |_argv: &[String], _cwd: Option<&Path>| -> (i32, String, String) {
            panic!("gh must not run when the flip is spared or stood down");
        };

        seed_laws(
            &dir,
            vec![law_envelope(
                "d-8",
                "pr-draft:owner/repo#7",
                DRAFT_DECISION,
                "operator",
            )],
        );
        let reply = flip(&flip_spec(&journal), &no_gh);
        assert_eq!(reply["outcome"], json!("spared"));
        assert!(reply["receipt"]
            .as_str()
            .unwrap()
            .contains("operator ruling"));

        fs::write(dir.join("config.toml"), "[pr]\nopen_ready = false\n").unwrap();
        let reply = flip(&flip_spec(&journal), &no_gh);
        assert_eq!(reply["outcome"], json!("stand_down"));
        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn flip_pins_the_repo_and_journals() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-flip-b-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);
        let journal = dir.join("events.jsonl");
        let calls = std::cell::RefCell::new(Vec::new());
        let gh = |argv: &[String], _cwd: Option<&Path>| -> (i32, String, String) {
            calls.borrow_mut().push(argv.to_vec());
            (0, String::new(), String::new())
        };
        let reply = flip(&flip_spec(&journal), &gh);
        assert_eq!(reply["outcome"], json!("flipped"));
        assert_eq!(reply["journaled"], json!(true));
        assert_eq!(
            calls.into_inner(),
            vec![vec![
                "gh".to_string(),
                "pr".to_string(),
                "ready".to_string(),
                "7".to_string(),
                "--repo".to_string(),
                "owner/repo".to_string(),
            ]]
        );
        let text = crate::event_store::journal_text(&journal, &["pr_watch_draft_flip".to_string()])
            .unwrap();
        let row: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(row["type"], json!("pr_watch_draft_flip"));
        assert_eq!(row["source"], json!("daemon"));
        assert_eq!(row["data"]["outcome"], json!("flipped"));
        assert_eq!(row["data"]["pr"], json!(7));
        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn flip_error_journals_the_refusal() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join(format!("pdr-flip-c-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        hermetic(&dir);
        let journal = dir.join("events.jsonl");
        let gh = |_argv: &[String], _cwd: Option<&Path>| -> (i32, String, String) {
            (1, String::new(), "not a draft".to_string())
        };
        let reply = flip(&flip_spec(&journal), &gh);
        assert_eq!(reply["outcome"], json!("error"));
        assert!(reply["receipt"].as_str().unwrap().contains("not a draft"));
        let text = crate::event_store::journal_text(&journal, &["pr_watch_draft_flip".to_string()])
            .unwrap();
        let row: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(row["data"]["outcome"], json!("error"));
        assert_eq!(row["data"]["error"], json!("not a draft"));
        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_STATE_DIR");
    }

    #[test]
    fn door_needs_a_known_op() {
        let reply = run_door(&json!({ "pr_draft_ready": {} }));
        assert!(reply["error"].as_str().unwrap().contains("check or flip"));
        let reply = run_door(&json!({}));
        assert!(reply["error"].as_str().unwrap().contains("check or flip"));
    }
}
