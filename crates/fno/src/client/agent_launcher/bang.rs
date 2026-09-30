//! The composer's bang mode run path: one `!` line becomes one shell run
//! in a new mux pane, plus its agents-journal rows (`composer_shell_ran`
//! when a pane was born, `composer_shell_refused` when the run never
//! started). The line rides as its own argv element and is never spliced
//! into a script, so quotes, pipes and redirects pass through intact.

use std::path::Path;
use std::time::Duration;

use super::{close, open, write_msg, ClientMsg, Phase, View};
use crate::mux_cli::{control_roundtrip_with_timeouts, ControlError};
use crate::pane_send_audit::{append_agents_event, pane_send_audit_events_path};
use crate::proto::{socket_path, Command, ControlVerb, PanePlacement, ServerMsg};

/// The journal line is data, not a transcript: past this the row truncates.
const MAX_LINE_CHARS: usize = 512;

/// The control deadlines: a wedged server holds the composer no longer.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// `/bin/sh` runs `<shell> -lc <line>`, prints `[exit N]`, then hands the
/// pane to an interactive login shell. Positional args keep the line out of
/// the script text; the pane never hangs because the command exited.
fn shell_argv(shell: &str, line: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("\"$0\" -lc \"$1\"; s=$?; printf '\\n[exit %s]\\n' \"$s\"; exec \"$0\" -l"),
        shell.to_string(),
        line.to_string(),
    ]
}

/// The user's login shell, resolved where the mux's own shell panes
/// resolve theirs: `$SHELL`, else `/bin/sh`.
fn user_shell() -> String {
    crate::pty::shell_candidates(std::env::var_os("SHELL").as_deref())
        .first()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn cut(line: &str) -> String {
    line.chars().take(MAX_LINE_CHARS).collect()
}

fn ran_row(mux_session: &str, cwd: &str, shell: &str, line: &str, pane: u64) -> serde_json::Value {
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "composer_shell_ran",
        "source": "cli",
        "data": {
            "mux_session": mux_session,
            "cwd": cwd,
            "shell": shell,
            "line": cut(line),
            "pane": pane,
        }
    })
}

fn refused_row(
    mux_session: &str,
    cwd: &str,
    line: &str,
    reason: &str,
    outcome: &str,
) -> serde_json::Value {
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "composer_shell_refused",
        "source": "cli",
        "data": {
            "mux_session": mux_session,
            "cwd": cwd,
            "line": cut(line),
            "reason": reason,
            "outcome": outcome,
        }
    })
}

/// Best-effort, like the pane-close emit: a failed append prints one line
/// and never blocks the run.
fn emit(events: &Path, row: &serde_json::Value) {
    if append_agents_event(events, row).is_err() {
        eprintln!("fno mux: composer_shell emit failed");
    }
}

/// Run the drafted line in a new pane through the session's control
/// socket, journaling to the standard events path.
pub(super) async fn run(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let events = pane_send_audit_events_path();
    let session = view.session.clone();
    let sock = socket_path(&session)?;
    run_with(view, sock_w, &events, move |verb| {
        control_roundtrip_with_timeouts(&sock, &session, verb, CONTROL_TIMEOUT, CONTROL_TIMEOUT)
    })
    .await
}

/// The run core. `events` and the control round-trip are parameters so
/// tests inject both; the real path passes the standard journal path and
/// the one-shot socket round-trip.
pub(super) async fn run_with(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
    events: &Path,
    roundtrip: impl FnOnce(ControlVerb) -> Result<ServerMsg, ControlError> + Send + 'static,
) -> Result<(), String> {
    let Some(l) = view.launcher.as_mut() else {
        return Ok(());
    };
    if !l.shell {
        return Ok(());
    }
    // An unresolved attempt blocks a second send until dismissed: a lost
    // reply never runs the line twice (AC7).
    if matches!(l.phase, Phase::Unknown { .. } | Phase::Submitting { .. }) || l.armed.is_some() {
        return Ok(());
    }
    // An empty `!` line asked for nothing: nothing is sent, nothing is
    // written (AC8).
    let line = l.draft.message.trim().to_string();
    if line.is_empty() {
        return Ok(());
    }
    let session = view.session.clone();
    let cwd = l.draft.cwd();
    let request_id = l.next_request_id;
    l.next_request_id += 1;
    if cwd.is_empty() {
        let reason = "no project chosen; pick one on the Project chip".to_string();
        emit(
            events,
            &refused_row(&session, &cwd, &line, &reason, "refused"),
        );
        l.phase = Phase::Refused { request_id, reason };
        return Ok(());
    }
    let shell = user_shell();
    let argv = shell_argv(&shell, &line);
    let verb = ControlVerb::PaneRun {
        cwd: cwd.clone(),
        argv,
        cols: None,
        rows: None,
        claim: false,
        placement: PanePlacement::default(),
        worker: None,
    };
    let reply = tokio::task::spawn_blocking(move || roundtrip(verb))
        .await
        .map_err(|e| format!("control round-trip task failed: {e}"))?;
    let Some(l) = view.launcher.as_mut() else {
        return Ok(());
    };
    match reply {
        Ok(ServerMsg::PaneSpawned { pane_id, .. }) => {
            emit(events, &ran_row(&session, &cwd, &shell, &line, pane_id));
            l.draft.message.clear();
            l.draft.bump();
            l.shell = false;
            close(view);
            view.note_command_sent(&Command::FocusPane(pane_id));
            write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pane_id)))
                .await
                .map_err(|e| format!("focus send failed: {e}"))?;
        }
        Err(ControlError::Unanswered(reason)) => {
            emit(
                events,
                &refused_row(&session, &cwd, &line, &reason, "unanswered"),
            );
            l.phase = Phase::Unknown { request_id, reason };
        }
        _ => {
            let reason = match &reply {
                Ok(other) => format!("unexpected control reply: {other:?}"),
                Err(e) => e.to_string(),
            };
            emit(
                events,
                &refused_row(&session, &cwd, &line, &reason, "refused"),
            );
            l.phase = Phase::Refused { request_id, reason };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bang-{tag}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A composer in shell mode on an empty input, pointed at one project
    /// so `draft.cwd()` has an answer.
    fn shell_view(session: &str, line: &str, cwd: Option<&str>) -> View {
        let mut v = crate::client::tests::two_pane_view();
        v.session = session.to_string();
        open(&mut v);
        let l = v.launcher.as_mut().unwrap();
        l.shell = true;
        l.draft.message = line.to_string();
        if let Some(cwd) = cwd {
            l.draft.projects = vec![cwd.to_string()];
            l.draft.project_idx = 0;
        }
        v
    }

    /// The AC5 trampoline: the line runs under the login shell, the exit
    /// is printed, and the pane hands off to an interactive shell instead
    /// of closing.
    #[test]
    fn trampoline_runs_the_line_and_reports_the_exit() {
        let dir = tmp_dir("argv");
        let argv = shell_argv("/bin/sh", "printf hi; pwd; exit 3");
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .current_dir(&dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("hi\n"), "line ran: {stdout:?}");
        assert!(
            stdout.contains(dir.to_str().unwrap()),
            "pwd names the cwd: {stdout:?}"
        );
        assert!(stdout.contains("[exit 3]"), "exit printed: {stdout:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC4: the exact `PaneRun` verb, one `composer_shell_ran` row, the
    /// composer closed with its message cleared and shell mode off, and
    /// the focus frame on the socket.
    #[test]
    fn run_sends_pane_run_focuses_the_pane_and_writes_one_ran_row() {
        let dir = tmp_dir("ran");
        let events = dir.join("events.jsonl");
        let seen: std::sync::Arc<std::sync::Mutex<Option<ControlVerb>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen2 = seen.clone();
        let roundtrip = move |verb: ControlVerb| {
            *seen2.lock().unwrap() = Some(verb);
            Ok(ServerMsg::PaneSpawned {
                pane_id: 9,
                placement: None,
            })
        };
        let mut v = shell_view("bang-ran", "printf hi; pwd", Some("/tmp/proj-a"));
        let mut sock = Vec::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            run_with(&mut v, &mut sock, &events, roundtrip)
                .await
                .unwrap();
        });
        let verb = seen.lock().unwrap().take().unwrap();
        let ControlVerb::PaneRun {
            cwd, argv, claim, ..
        } = verb
        else {
            panic!("expected PaneRun, got another verb");
        };
        assert_eq!(cwd, "/tmp/proj-a");
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[4], "printf hi; pwd");
        assert!(!claim);
        // One journal row: the ran row naming the cwd, the shell, the line
        // and the pane id.
        let journal = std::fs::read_to_string(&events).unwrap();
        let rows: Vec<&str> = journal.lines().collect();
        assert_eq!(rows.len(), 1, "exactly one row: {journal:?}");
        let row: serde_json::Value = serde_json::from_str(rows[0]).unwrap();
        assert_eq!(row["type"], "composer_shell_ran");
        assert_eq!(row["data"]["cwd"], "/tmp/proj-a");
        assert_eq!(row["data"]["line"], "printf hi; pwd");
        assert_eq!(row["data"]["pane"], 9);
        // The composer closed, shell mode reset with the message cleared.
        assert!(v.launcher.is_none());
        let closed = v.launcher_closed.as_ref().unwrap();
        assert!(!closed.shell);
        assert!(closed.draft.message.is_empty());
        // The focus frame went out on the socket.
        assert!(!sock.is_empty(), "the focus frame was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC6 + AC8: a fatal round-trip refuses with the line and shell mode
    /// kept and one refused row; an empty project refuses with its own
    /// reason; an empty line sends and writes nothing.
    #[test]
    fn run_refuses_keeps_the_draft_and_writes_rows() {
        let dir = tmp_dir("refused");
        let events = dir.join("events.jsonl");
        let mut v = shell_view("bang-refused", "git status", Some("/tmp/proj-b"));
        let mut sock = Vec::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            run_with(&mut v, &mut sock, &events, |_| {
                Err(ControlError::Fatal("cannot reach session".into()))
            })
            .await
            .unwrap();
        });
        {
            let l = v.launcher.as_ref().unwrap();
            assert!(matches!(l.phase, Phase::Refused { .. }), "fatal refuses");
            assert!(l.shell, "shell mode kept");
            assert_eq!(l.draft.message, "git status", "the line is kept");
        }
        let journal = std::fs::read_to_string(&events).unwrap();
        let row: serde_json::Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
        assert_eq!(row["type"], "composer_shell_refused");
        assert_eq!(row["data"]["outcome"], "refused");

        // The empty-project refusal carries its own reason.
        let mut v = shell_view("bang-refused", "git status", None);
        rt.block_on(async {
            run_with(&mut v, &mut sock, &events, |_| {
                panic!("no verb may be sent for an empty project")
            })
            .await
            .unwrap();
        });
        {
            let l = v.launcher.as_ref().unwrap();
            assert!(matches!(l.phase, Phase::Refused { .. }));
        }
        let journal = std::fs::read_to_string(&events).unwrap();
        let row: serde_json::Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
        assert_eq!(row["data"]["cwd"], "");

        // An empty line sends nothing and writes nothing (AC8).
        let mut v = shell_view("bang-refused", "   ", Some("/tmp/proj-c"));
        rt.block_on(async {
            run_with(&mut v, &mut sock, &events, |_| {
                panic!("an empty line sends nothing")
            })
            .await
            .unwrap();
        });
        let journal = std::fs::read_to_string(&events).unwrap();
        assert_eq!(journal.lines().count(), 3, "no row for the empty line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC7: an unanswered round-trip blocks a second send until dismissed.
    #[test]
    fn run_unanswered_blocks_a_second_send() {
        let dir = tmp_dir("unanswered");
        let events = dir.join("events.jsonl");
        let mut v = shell_view("bang-unans", "sleep 1", Some("/tmp/proj-d"));
        let mut sock = Vec::new();
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(async {
            run_with(&mut v, &mut sock, &events, |_| {
                Err(ControlError::Unanswered("the reply never arrived".into()))
            })
            .await
            .unwrap();
        });
        {
            let l = v.launcher.as_ref().unwrap();
            assert!(matches!(l.phase, Phase::Unknown { .. }), "unknown phase");
        }
        let journal = std::fs::read_to_string(&events).unwrap();
        let row: serde_json::Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
        assert_eq!(row["data"]["outcome"], "unanswered");

        // The second Enter sends nothing and writes nothing until the
        // operator dismisses (Esc) the Unknown.
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called2 = called.clone();
        rt.block_on(async {
            run_with(&mut v, &mut sock, &events, move |_| {
                called2.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(ServerMsg::Ok)
            })
            .await
            .unwrap();
        });
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "a second send is blocked"
        );
        let journal = std::fs::read_to_string(&events).unwrap();
        assert_eq!(journal.lines().count(), 1, "no second row");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
