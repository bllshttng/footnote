//! How a resume wake delivers, and when its receipt may say live.
//!
//! Split out of `client_verbs.rs` (x-6ac3): that file is over the 5,000-line
//! budget and shrink-only, and the wake-delivery + respawn-confirm question
//! outgrew living beside the argv parsers. Callers keep thin call sites in
//! `client_verbs.rs`; the delivery and confirmation logic lives here.

use crate::claude_ask::{read_state_json, ClaudeHome};
use crate::client_verbs::{append_agents_event, trace_events_path};
use crate::paths::AgentsHome;
use crate::truth_probe::family1_truth_state;
use serde_json::Value;

/// Default injected wake text when the caller passes no `--message`. Matches
/// the Python wake lane's `_DEFAULT_WAKE_MESSAGE` so the two runtimes stay in
/// parity.
const RESUME_WAKE_MESSAGE: &str = "continue";

/// Deliver a resume wake to a codex thread row over the app-server daemon
/// (x-6ac3). Exit 0 is a positive receipt: the daemon accepted the turn. An
/// `Err` maps to exit 16 carrying the reason token (`no-daemon`, `io-error`),
/// never an exec that may have done nothing under a captured stdin.
pub(crate) fn run_codex_thread_delivery(
    name: &str,
    session_id: &str,
    message: Option<&str>,
    cwd: &str,
    home: &AgentsHome,
) -> i32 {
    let text = message.unwrap_or(RESUME_WAKE_MESSAGE).to_string();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno agents resume: codex daemon delivery failed: {e}");
            return 16;
        }
    };
    let text = text.clone();
    let session_id = session_id.to_string();
    let result = runtime.block_on(async {
        crate::codex_inject::deliver_via_codex_daemon(&session_id, &text).await
    });
    match result {
        Ok(()) => {
            append_agents_event(
                &trace_events_path(home),
                "agent_resumed",
                &[
                    ("name", Value::String(name.to_string())),
                    ("provider", Value::String("codex".to_string())),
                    ("session_id", Value::String(session_id)),
                    ("cwd", Value::String(cwd.to_string())),
                ],
            );
            eprintln!("delivered to {name} over the codex daemon");
            0
        }
        Err(reason) => {
            eprintln!("fno agents resume: not delivered: {reason}");
            16
        }
    }
}

/// Run a respawn plan as a child and confirm it actually revived the row.
///
/// `claude respawn` exits as soon as the job is relaunched, so this verb must
/// NOT exec it (the exec convention the other arms use): the operator's shell
/// would come back with nothing to show. And exit 0 is not proof - the
/// receipt-can-lie shape `fno agents rm` already shipped once. The
/// confirmation is the positive marker: `jobs/<short>/state.json` re-read
/// after the respawn with an `updated_at` newer than the pre-respawn read.
/// The state WORD is not evidence - the wake lane's confirm primitive exists
/// because `working -> working` read the same for a landed and an unlanded
/// message.
pub(crate) fn run_and_confirm_respawn(
    plan: &crate::reentry::ReentryPlan,
    name: &str,
    verb: &str,
    event_kind: &str,
    home: &AgentsHome,
) -> i32 {
    run_and_confirm_respawn_with_truth(
        plan,
        name,
        verb,
        event_kind,
        home,
        ClaudeHome::from_env(),
        family1_truth_state,
        std::thread::sleep,
    )
}

pub(crate) fn run_and_confirm_respawn_with_truth<F, S>(
    plan: &crate::reentry::ReentryPlan,
    name: &str,
    verb: &str,
    event_kind: &str,
    home: &AgentsHome,
    claude_home: ClaudeHome,
    truth_fn: F,
    sleep_fn: S,
) -> i32
where
    F: Fn(&str) -> Option<String>,
    S: Fn(std::time::Duration),
{
    let jobs_dir = claude_home.jobs_dir_for(&plan.short_id);
    let before_updated_at = read_state_json(&jobs_dir).ok().and_then(|s| s.updated_at);

    let mut command = std::process::Command::new(&plan.argv[0]);
    command.args(&plan.argv[1..]).current_dir(&plan.cwd);
    for (key, value) in &plan.env {
        command.env(key, value);
    }
    let status = match command.status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno agents {verb}: failed to run {}: {e}", plan.argv[0]);
            return 1;
        }
    };
    if !status.success() {
        eprintln!(
            "fno agents {verb}: {} for {name} exited {}",
            plan.argv.join(" "),
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
        return 1;
    }

    let confirmed = match (before_updated_at, read_state_json(&jobs_dir)) {
        (Some(before), Ok(s)) => s.updated_at.as_deref().is_some_and(|a| a > before.as_str()),
        // No readable BEFORE stamp (the file the resolver just proved exists
        // did not parse): an AFTER read carrying any stamp is the evidence
        // left, and it is still content, never an exit code.
        (None, Ok(s)) => s.updated_at.is_some(),
        (_, Err(_)) => false,
    };
    if !confirmed {
        eprintln!(
            "fno agents {verb}: respawn for {name} reported success but {} did not \
             advance; the row is NOT confirmed back in agent view. Check \
             `claude agents` before retrying.",
            jobs_dir.join("state.json").display()
        );
        return 16;
    }

    // x-6ac3: `updated_at` advancing proves the job relaunched, never that
    // the worker is answering - the same receipt-can-lie shape the node
    // recorded (`is live again` printed, then truth read stalled). The
    // receipt may say live only when the truth probe agrees, within a
    // bounded window: a relaunched worker takes seconds to reach its first
    // live state.
    const TRUTH_POLLS: u32 = 10;
    const TRUTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    let is_live = |state: &str| matches!(state, "working" | "watching" | "your-move");
    let mut last_state = "unknown".to_string();
    let mut live = false;
    for attempt in 0..TRUTH_POLLS {
        if attempt > 0 {
            sleep_fn(TRUTH_INTERVAL);
        }
        if let Some(state) = truth_fn(&plan.session_id) {
            if is_live(&state) {
                live = true;
                last_state = state;
                break;
            }
            last_state = state;
        }
    }
    if !live {
        eprintln!(
            "fno agents {verb}: respawned {name}, but truth reads \
             {last_state}; not confirmed live."
        );
        return 16;
    }

    append_agents_event(
        &trace_events_path(home),
        event_kind,
        &[
            ("name", Value::String(name.to_string())),
            ("provider", Value::String("claude".to_string())),
            ("session_id", Value::String(plan.session_id.clone())),
            ("cwd", Value::String(plan.cwd.clone())),
        ],
    );
    eprintln!(
        "{name} is live again under {} (same session id).",
        plan.session_id
    );
    eprintln!("`fno agents attach {name}` to drop in.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_thread_resume_delivers_over_the_daemon_and_exits_0() {
        let _guard = crate::path_test_guard();
        // FakeDaemon::start tokio::spawns its server, so it needs a reactor.
        // Multi-thread keeps the serve task running while the delivery
        // helper block_on's its own current-thread runtime below.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let daemon = rt.block_on(async {
            crate::codex_fake_daemon::FakeDaemon::start(crate::codex_fake_daemon::Behavior::quick())
        });
        let dir = std::env::temp_dir().join(format!("fno-cv-codex-deliver-{}", std::process::id()));
        let home = AgentsHome::at(dir.clone());
        let code = run_codex_thread_delivery("w1", "thread-abc", Some("continue"), "/tmp/x", &home);
        assert_eq!(code, 0);
        let params = daemon
            .first_params("turn/start")
            .expect("turn/start must have run");
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["input"][0]["text"], "continue");
        drop(daemon);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_thread_resume_without_a_daemon_refuses_16() {
        // No exec arm exists in the delivery helper: it never constructs a
        // Command, so no `codex resume` process can start from this lane.
        let _guard = crate::path_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let saved = std::env::var_os("CODEX_HOME");
        std::env::set_var("CODEX_HOME", temp.path());
        let home = AgentsHome::at(temp.path().join("agents-home"));
        let code = run_codex_thread_delivery("w1", "thread-abc", None, "/tmp/x", &home);
        match &saved {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        assert_eq!(code, 16);
    }

    #[test]
    fn respawn_receipt_reads_live_only_when_truth_reads_live() {
        // ClaudeHome is injected (not read off HOME) so the test is hermetic
        // against concurrent tests mutating HOME.
        let temp = tempfile::tempdir().unwrap();
        let jobs = temp.path().join(".claude").join("jobs").join("abcd1234");
        std::fs::create_dir_all(&jobs).unwrap();
        let state = jobs.join("state.json");
        std::fs::write(
            &state,
            r#"{"state":"idle","updatedAt":"2026-09-13T00:00:00Z"}"#,
        )
        .unwrap();
        let plan = crate::reentry::ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "respawn".into(),
            name: "w1".into(),
            fno_id: None,
            node: None,
            session_id: "sess-uuid".into(),
            short_id: "abcd1234".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: temp.path().display().to_string(),
            substrate: "bg".into(),
            mux: None,
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "printf '%s' '{{\"state\":\"working\",\"updatedAt\":\"2026-09-13T00:01:00Z\"}}' > \"{}\"",
                    state.display()
                ),
            ],
            env: Default::default(),
        };
        let home = AgentsHome::at(temp.path().join("agents-home"));
        let code = run_and_confirm_respawn_with_truth(
            &plan,
            "w1",
            "resume",
            "agent_resumed",
            &home,
            crate::claude_ask::ClaudeHome::at(temp.path()),
            |handle| {
                assert_eq!(handle, "sess-uuid");
                Some("working".to_string())
            },
            |_| {},
        );
        assert_eq!(code, 0);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn respawn_receipt_refuses_when_truth_never_reads_live() {
        let temp = tempfile::tempdir().unwrap();
        let jobs = temp.path().join(".claude").join("jobs").join("abcd1234");
        std::fs::create_dir_all(&jobs).unwrap();
        let state = jobs.join("state.json");
        std::fs::write(
            &state,
            r#"{"state":"idle","updatedAt":"2026-09-13T00:00:00Z"}"#,
        )
        .unwrap();
        let plan = crate::reentry::ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "respawn".into(),
            name: "w1".into(),
            fno_id: None,
            node: None,
            session_id: "sess-uuid".into(),
            short_id: "abcd1234".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: temp.path().display().to_string(),
            substrate: "bg".into(),
            mux: None,
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "printf '%s' '{{\"state\":\"working\",\"updatedAt\":\"2026-09-13T00:01:00Z\"}}' > \"{}\"",
                    state.display()
                ),
            ],
            env: Default::default(),
        };
        let home = AgentsHome::at(temp.path().join("agents-home"));
        let code = run_and_confirm_respawn_with_truth(
            &plan,
            "w1",
            "resume",
            "agent_resumed",
            &home,
            crate::claude_ask::ClaudeHome::at(temp.path()),
            |_| Some("stalled".to_string()),
            |_| {}, // no-op sleep: the window must not cost wall clock in tests
        );
        assert_eq!(code, 16);
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
