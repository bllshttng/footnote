//! One write path for every backlog board (the mux TUI and the web bridge):
//! the argv builders and the one bounded shell-out. The verbs themselves are
//! Python-served `fno backlog` commands; a board builds argv here, runs it
//! through [`run_verb`], and shows the verb's own last line back as the
//! notice. No board builds a write argv by hand.

use std::time::Duration;

/// One `fno backlog ...` write verb's budget (the board read's).
const VERB_BUDGET: Duration = Duration::from_secs(10);

/// Where a rank move puts the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Top,
    Bottom,
    Before,
    After,
}

/// The argv for one card move: `fno backlog rank <id> <place> ...`, always
/// ending in `--operator`. Before and After need their anchor.
pub fn rank_argv(id: &str, place: Place, anchor: Option<&str>) -> Result<Vec<String>, String> {
    let (word, flag) = match place {
        Place::Top => ("top", None),
        Place::Bottom => ("bottom", None),
        Place::Before => ("before", Some("--before")),
        Place::After => ("after", Some("--after")),
    };
    if flag.is_some() && anchor.is_none() {
        return Err(format!("rank {word}: no anchor card named"));
    }
    let mut args: Vec<String> = vec!["backlog".into(), "rank".into(), id.into(), word.into()];
    if let (Some(flag), Some(anchor)) = (flag, anchor) {
        args.push(flag.into());
        args.push(anchor.into());
    }
    args.push("--operator".into());
    Ok(args)
}

/// The one bounded shell-out for every board write: argv as an array, off
/// the UI loop, `kill_on_drop`, a 10 s budget. The notice is the last
/// stderr line on a non-zero exit (the verb's refusal, verbatim), else the
/// last stdout line, else the updated fallback; the bool is the exit fact
/// so the web bridge can answer `{"ok": false, ...}` without re-reading the
/// text. Sibling of the foreground restart runner
/// ([`crate::client::update_menu::run_restart_foreground`]), which instead
/// inherits stdio so the operator watches the receipts.
pub(crate) async fn run_verb(args: &[String], stdin: Option<String>) -> (bool, String) {
    use std::process::Stdio;
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    let fut = async {
        let mut child = command.spawn().ok()?;
        if let Some(text) = stdin {
            let mut w = child.stdin.take()?;
            use tokio::io::AsyncWriteExt;
            w.write_all(text.as_bytes()).await.ok()?;
            w.shutdown().await.ok()?;
            drop(w);
        }
        child.wait_with_output().await.ok()
    };
    let output = match tokio::time::timeout(VERB_BUDGET, fut).await {
        Ok(Some(o)) => o,
        Ok(None) => return (false, "the verb could not start".into()),
        Err(_) => {
            return (
                false,
                format!(
                    "fno {} timed out after 10s; re-read to see if it landed",
                    args.first().map(String::as_str).unwrap_or("verb")
                ),
            )
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return (
            false,
            last_line(&stderr).unwrap_or_else(|| "the verb refused".into()),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    (true, last_line(&stdout).unwrap_or_else(|| "updated".into()))
}

/// The last non-blank line of a verb's output.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rank_argv_always_ends_operator_and_needs_an_anchor() {
        let args = rank_argv("x-1", Place::Top, None).expect("top needs no anchor");
        assert_eq!(args.last().map(String::as_str), Some("--operator"));
        assert!(rank_argv("x-1", Place::Before, None).is_err());
        let args = rank_argv("x-1", Place::After, Some("x-2")).expect("anchored after");
        assert_eq!(
            args,
            vec![
                "backlog",
                "rank",
                "x-1",
                "after",
                "--after",
                "x-2",
                "--operator"
            ]
        );
    }

    // AC4/AC8 halves: the shell-out reports the exit fact and the verb's own
    // last line. FNO_BIN is process-global, so the existing guard serializes
    // the tests that repoint it.
    #[tokio::test]
    async fn run_verb_reports_the_last_stdout_line_on_success() {
        let _guard = crate::pane_send_audit::FNO_BIN_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-backlog-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("argv");
        let stub = dir.join("stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\necho board wrote the card\n",
                record.display()
            ),
        )
        .unwrap();
        let _ = std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755));
        let previous = std::env::var_os("FNO_BIN");
        std::env::set_var("FNO_BIN", &stub);
        let (ok, notice) = run_verb(
            &rank_argv("x-1", Place::Top, None).expect("top needs no anchor"),
            None,
        )
        .await;
        std::env::remove_var("FNO_BIN");
        if let Some(prev) = previous {
            std::env::set_var("FNO_BIN", prev);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        assert_eq!(notice, "board wrote the card");
    }

    #[tokio::test]
    async fn run_verb_reports_the_verb_refusal_on_failure() {
        let _guard = crate::pane_send_audit::FNO_BIN_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir =
            std::env::temp_dir().join(format!("fno-backlog-write-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("stub.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho 'rank: anchor x-2 is unranked' >&2\nexit 1\n",
        )
        .unwrap();
        let _ = std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755));
        let previous = std::env::var_os("FNO_BIN");
        std::env::set_var("FNO_BIN", &stub);
        let (ok, notice) =
            run_verb(&rank_argv("x-1", Place::Top, None).expect("passes"), None).await;
        std::env::remove_var("FNO_BIN");
        if let Some(prev) = previous {
            std::env::set_var("FNO_BIN", prev);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!ok);
        assert_eq!(notice, "rank: anchor x-2 is unranked");
    }
}
