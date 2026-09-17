//! The mux keystroke's launch machinery (change 3): board selection
//! inputs, the door argv, the bounded shell-out, and the notice mapping.
//! Lifted out of `server.rs` as its own module named by the question it
//! answers - "how does a mux dispatch launch a node?" - so the shrink-only
//! ratchet on `server.rs` is not fed by new code. The sideline launcher
//! extends the same boundaries: a free-form argv builder over the ONE door,
//! a stdin-capturing shell-out, and one typed outcome decoder shared by
//! node dispatch and the composer.

use std::time::Duration;

use crate::proto::agent_launch::AgentLaunchRequest;

/// Bounded + fail-open (the digest_overlay idiom): read the board, launch the
/// door, turn the outcome into the client notice. An empty return says nothing
/// (the launched pane speaks for itself); every error path yields a visible
/// notice rather than a silent no-op.
pub(crate) fn dispatch_timeout() -> Duration {
    Duration::from_secs(75)
}

/// The launch argv for a dispatch (change 3, step 3): pure so a unit
/// test can pin it. The door is the ONE launcher and now the mux's
/// direct target too - no `--harness`, `--model`, `--route` and no message
/// ride, so the grid picks the lane while the axes are free and the door
/// renders the seed; the door takes the family-2 guard, the spawn gate, and
/// the placement lease.
pub(crate) fn dispatch_spawn_argv(
    fno: &str,
    node_id: &str,
    session: &str,
    account: Option<&str>,
    parent: Option<&str>,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        fno.to_string(),
        "agents".to_string(),
        "spawn".to_string(),
        "--node".to_string(),
        node_id.to_string(),
        "--substrate".to_string(),
        "pane".to_string(),
        "--mux-session".to_string(),
        session.to_string(),
        "--no-wait".to_string(),
    ]
    .to_vec();
    // The client's session-local active account rides the same flag
    // the old porcelain pinned; the mux only forwards the id.
    if let Some(a) = account {
        argv.push("--account".to_string());
        argv.push(a.to_string());
    }
    // A child node opens under its epic's tab, the same placement the card
    // click visualized.
    if let Some(p) = parent {
        argv.push("--tab".to_string());
        argv.push(p.to_string());
    }
    argv
}

/// (id, slug, parent) from a node JSON read (`fno backlog get` / `fno backlog
/// next`): the LAST parseable JSON object on stdout carrying an id (a hook or
/// a note may print first). `None` on `null`, empty output, or a record
/// without an id - for the board leg, every `None` reads as an empty bench.
pub(crate) fn node_identity(stdout: &str) -> Option<(String, Option<String>, Option<String>)> {
    let mut found = None;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                found = Some((
                    id.to_string(),
                    v.get("slug").and_then(|x| x.as_str()).map(str::to_string),
                    v.get("parent").and_then(|x| x.as_str()).map(str::to_string),
                ));
            }
        }
    }
    found
}

/// One bounded `fno` shell-out capturing both streams (change 3): the
/// board reads need stdout; the spawn leg needs the door's stderr for the
/// refusal-reason mapping. `None` on a timeout or a failed spawn - the caller
/// renders its own notice for that.
pub(crate) async fn run_fno_captured(
    argv: &[&str],
    timeout: Duration,
    deadline: tokio::time::Instant,
) -> Option<(bool, String, String)> {
    let mut command = crate::process_admission::tokio_command(argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    // Two bounds: the per-subprocess budget AND the whole dispatch's
    // deadline, so two sequential legs can never exceed the one budget the
    // retired porcelain spent on the entire dispatch.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    match tokio::time::timeout(timeout.min(remaining), fut).await {
        Err(_) => None,
        Ok(Err(_)) => None,
        Ok(Ok(o)) => Some((
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )),
    }
}

/// The launcher's spawn argv: pure and unit-pinned like
/// [`dispatch_spawn_argv`]. The ONE launch executable stays the configured
/// `fno` front door with `agents spawn`; cwd, harness and advanced values
/// ride as separate argv elements, and the message NEVER rides argv - it
/// arrives through `--prompt-file -` stdin at the shell-out below. No
/// `--force`, no `--yolo`: normal gates decide, and a refusal is the
/// product.
pub(crate) fn launch_spawn_argv(fno: &str, req: &AgentLaunchRequest, session: &str) -> Vec<String> {
    let mut argv: Vec<String> = [
        fno.to_string(),
        "agents".to_string(),
        "spawn".to_string(),
        "--harness".to_string(),
        req.harness.clone(),
        "--cwd".to_string(),
        req.cwd.clone(),
        "--substrate".to_string(),
        req.substrate.clone(),
        "--mux-session".to_string(),
        session.to_string(),
        // Fail immediately on a full spawn gate rather than queueing: a
        // popup launch that silently waits reads as a hung button.
        "--no-wait".to_string(),
    ]
    .to_vec();
    if let Some(m) = &req.model {
        argv.extend(["--model".to_string(), m.clone()]);
    }
    if let Some(e) = &req.effort {
        argv.extend(["--effort".to_string(), e.clone()]);
    }
    if let Some(p) = &req.permission_mode {
        argv.extend(["--permission-mode".to_string(), p.clone()]);
    }
    if let Some(t) = &req.placement {
        argv.extend(["--tab".to_string(), t.clone()]);
    }
    // The seed rides stdin even when empty: an empty stdin is the honest
    // "no seed requested", never a fabricated task.
    argv.push("--prompt-file".to_string());
    argv.push("-".to_string());
    argv
}

/// One bounded `fno` shell-out that also feeds `stdin_bytes` to the child:
/// the launcher's message reaches `--prompt-file -` exactly,
/// bytes-for-bytes, with no shell interpolation. `kill_on_drop` + the two
/// bounds (per-subprocess budget AND the whole attempt's deadline) carry
/// over from [`run_fno_captured`].
pub(crate) async fn run_fno_captured_with_stdin(
    argv: &[&str],
    stdin_bytes: &[u8],
    timeout: Duration,
    deadline: tokio::time::Instant,
) -> Option<(bool, String, String)> {
    let mut command = crate::process_admission::tokio_command(argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let fut = async move {
        let mut child = crate::process_admission::tokio_spawn(&mut command).ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            if stdin.write_all(stdin_bytes).await.is_err() {
                return None;
            }
            // Drop the handle so the child sees EOF and `--prompt-file -`
            // terminates; without this the door blocks on its own read.
            drop(stdin);
        }
        child.wait_with_output().await.ok().map(|o| {
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).to_string(),
                String::from_utf8_lossy(&o.stderr).to_string(),
            )
        })
    };
    match tokio::time::timeout(timeout.min(remaining), fut).await {
        Err(_) => None,
        Ok(None) => None,
        Ok(Some(triple)) => Some(triple),
    }
}

/// What one spawn attempt actually produced . The variants state
/// BIRTH facts, not acknowledgments: `Launched` requires a decoded receipt,
/// `Refused` requires the door's own no-birth answer, and everything
/// uncertain - timeout, malformed success, recovery-required receipt, lost
/// reply - is `Unknown`, never flattened into either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaunchOutcome {
    Launched {
        name: String,
        pane: Option<u64>,
        /// Seed fact off the receipt, kept SEPARATE from birth: an
        /// intentionally empty seed ("unattempted") is not a failed delivery.
        seed_delivered: Option<bool>,
    },
    /// The door refused before any effect. Deliberate retry is safe.
    Refused(String),
    /// Whether a worker was born is unresolved.
    Unknown(String),
}

/// The LAST parseable JSON object on stdout (a notice line may print first).
/// Both receipt shapes live here: a pane receipt carries `pane_id`, a bg
/// thread receipt carries `name` + `short_id`.
fn spawn_receipt(stdout: &str) -> Option<serde_json::Value> {
    let mut found = None;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let pane = v.get("pane_id").is_some();
            let thread = v.get("name").is_some() && v.get("short_id").is_some();
            if pane || thread {
                found = Some(v);
            }
        }
    }
    found
}

/// The door's own error line: first non-empty stderr line, else stdout, cut
/// at 160 chars. Shared by the notice mapping and the launcher decoder.
pub(crate) fn refusal_detail(stderr: &str, stdout: &str) -> String {
    let mut detail = crate::server::first_line_or(stderr, "");
    if detail.is_empty() {
        detail = crate::server::first_line_or(stdout, "");
    }
    if detail.chars().count() > 160 {
        detail.chars().take(160).collect()
    } else {
        detail
    }
}

/// Decode one launcher attempt . A recovery-required receipt is the
/// load-bearing ambiguity: the transaction's own contract says the child MAY
/// exist when persistence failed, so it decodes `Unknown`, never `Refused`.
pub(crate) fn decode_launch_outcome(exit_ok: bool, stdout: &str, stderr: &str) -> LaunchOutcome {
    if !exit_ok {
        return match refusal_detail(stderr, stdout) {
            d if d.is_empty() => LaunchOutcome::Refused("spawn failed".to_string()),
            d => LaunchOutcome::Refused(d),
        };
    }
    match spawn_receipt(stdout) {
        Some(v) => {
            let recovery = v
                .get("status")
                .and_then(|s| s.as_str())
                .is_some_and(|s| s == "recovery_required")
                || v.get("recovered").and_then(|r| r.as_bool()) == Some(true);
            if recovery {
                return LaunchOutcome::Unknown(
                    "spawn reported recovery_required: the child may exist".to_string(),
                );
            }
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let pane = v.get("pane_id").and_then(|p| p.as_u64());
            // seed: "submitted" proves delivery; "unattempted" with an empty
            // request message is the intentional interactive case. Absent or
            // other -> unproven.
            let seed_delivered = v
                .get("seed")
                .and_then(|s| s.as_str())
                .map(|s| s == "submitted");
            LaunchOutcome::Launched {
                name,
                pane,
                seed_delivered,
            }
        }
        None => LaunchOutcome::Unknown("spawn exited 0 with no readable receipt".to_string()),
    }
}

/// Map a dispatch launch to the one-line client notice (change 3,
/// step 4). Exit 0 with a pane receipt (a JSON line carrying `pane_id`)
/// renders `dispatched <slug or id>`; the seed / pane_observation doubt text
/// carries over verbatim. The door's family-2 refusal naming an
/// already-being-worked reason renders `already dispatching <slug or id>`.
/// Every other failure renders `grab work failed:` on the first stderr line
/// (cut at 160 chars), never silent on an error.
pub(crate) fn dispatch_notice(
    exit_ok: bool,
    stdout: &str,
    stderr: &str,
    node_id: &str,
    slug: &str,
) -> String {
    let label = if slug.is_empty() { node_id } else { slug };
    if exit_ok {
        // The door prints the pane receipt (one JSON line on stdout) on
        // success; the seed doubt reaches this caller exactly as before:
        // `seed == submitted && observation != unreadable` is the verified
        // launch, and every other answer renders the doubt. Two different
        // things fail that check and the operator acts on them differently, so
        // they are NOT rendered with one phrase. An unreadable pane means the
        // seed WAS delivered (it rode in the harness argv) and nobody could see
        // whether a pane is left to run it: re-probe or reap, never re-seed.
        // "seed unverified" there would send a reader to re-submit a payload
        // the pane may already be running, which is the duplicate this whole
        // split exists to stop. BOTH fields, never the pane one alone: an
        // unreadable pane says nothing about whether a seed was sent, so keying
        // on it by itself tells an agy operator "seed delivered" for a
        // pane-send spawn where nothing was ever typed - and then tells them
        // not to re-seed the one pane that needs it. `submitted` is what makes
        // "delivered" true.
        let receipt = spawn_receipt(stdout);
        return match receipt {
            Some(v) => {
                let seed = v.get("seed").and_then(|s| s.as_str());
                let pane = v.get("pane_observation").and_then(|p| p.as_str());
                match (seed, pane) {
                    (Some("submitted"), Some("unreadable")) if !label.is_empty() => {
                        format!("dispatched {label}, seed delivered, pane unreadable")
                    }
                    (Some("submitted"), Some("unreadable")) => {
                        "dispatched, seed delivered, pane unreadable".to_string()
                    }
                    (Some("submitted"), _) => {
                        if label.is_empty() {
                            "dispatched".to_string()
                        } else {
                            format!("dispatched {label}")
                        }
                    }
                    _ => {
                        if label.is_empty() {
                            "dispatched, seed unverified".to_string()
                        } else {
                            format!("dispatched {label}, seed unverified")
                        }
                    }
                }
            }
            // Exit 0 with no pane receipt: the launch cannot be confirmed.
            None => "grab work failed: fno agents spawn exited 0 with no pane receipt".to_string(),
        };
    }
    // The door's family-2 guard refused. The already-being-worked reasons are
    // a benign no-op (same-node race loser or an in-flight node), not a
    // failure; every other refusal or failure renders its first stderr line.
    if let Some(line) = stderr
        .lines()
        .find(|l| l.contains("node dispatch refused:"))
    {
        let reason = line
            .rsplit("reason=")
            .next()
            .unwrap_or("")
            .split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string();
        if matches!(
            reason.as_str(),
            "already-claimed" | "reservation-held" | "live-claim" | "unproven-claim" | "worker-row"
        ) {
            return if label.is_empty() {
                "already dispatching".to_string()
            } else {
                format!("already dispatching {label}")
            };
        }
    }
    let detail = refusal_detail(stderr, stdout);
    if detail.is_empty() {
        "grab work: dispatch failed".to_string()
    } else {
        format!("grab work failed: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_notice_maps_the_launch_outcomes() {
        // Exit 0 with a pane receipt: the friendly slug shows; the pane itself
        // is the real feedback.
        assert_eq!(
            dispatch_notice(
                true,
                r#"{"outcome":"launched","node":"x-1","slug":"feat","pane_id":7,"seed":"submitted"}"#,
                "",
                "x-1",
                "feat"
            ),
            "dispatched feat"
        );
        // The seed doubt is read straight off the door's receipt; the old
        // porcelain computed seed_verified from these same two fields, so an
        // absent seed reads unverified, exactly as it did before.
        assert_eq!(
            dispatch_notice(
                true,
                r#"{"outcome":"launched","pane_id":7,"seed":"unattempted"}"#,
                "",
                "x-1",
                "feat"
            ),
            "dispatched feat, seed unverified"
        );
        assert_eq!(
            dispatch_notice(true, r#"{"pane_id":7}"#, "", "x-1", "feat"),
            "dispatched feat, seed unverified"
        );
        assert_eq!(
            dispatch_notice(true, r#"{"pane_id":7,"seed":"unattempted"}"#, "", "", ""),
            "dispatched, seed unverified"
        );
        // An unreadable pane is the OTHER way to fail that check, and it earns
        // its own phrase: the seed rode in the argv, so it was delivered, and
        // telling an operator it is unverified sends them to re-seed a pane that
        // may already be running it.
        assert_eq!(
            dispatch_notice(
                true,
                r#"{"outcome":"launched","pane_id":7,"seed":"submitted","pane_observation":"unreadable"}"#,
                "",
                "x-1",
                "feat"
            ),
            "dispatched feat, seed delivered, pane unreadable"
        );
        // A pane that WAS observed keeps the original phrase: the doubt there is
        // about the seed, which is what the words say.
        assert_eq!(
            dispatch_notice(
                true,
                r#"{"outcome":"launched","pane_id":7,"seed":"unattempted","pane_observation":"blank"}"#,
                "",
                "x-1",
                "feat"
            ),
            "dispatched feat, seed unverified"
        );
        // No slug -> fall back to the node id.
        assert_eq!(
            dispatch_notice(true, r#"{"pane_id":7,"seed":"submitted"}"#, "", "x-1", ""),
            "dispatched x-1"
        );
        // The door refused with an already-being-worked reason: a benign
        // no-op, not a failure (AC4-ERR).
        assert_eq!(
            dispatch_notice(
                false,
                "",
                "node dispatch refused: node=x-1 verdict=already-running reason=live-claim",
                "x-1",
                "feat"
            ),
            "already dispatching feat"
        );
        for reason in [
            "already-claimed",
            "reservation-held",
            "unproven-claim",
            "worker-row",
        ] {
            let stderr = format!("node dispatch refused: node=x-1 verdict=refused reason={reason}");
            assert_eq!(
                dispatch_notice(false, "", &stderr, "x-1", ""),
                "already dispatching x-1"
            );
        }
        // A refusal with an UNMAPPED reason fails loudly, never silently
        // reads as a benign no-op.
        assert_eq!(
            dispatch_notice(
                false,
                "",
                "node dispatch refused: node=x-1 verdict=error reason=corrupted",
                "x-1",
                "feat"
            ),
            "grab work failed: node dispatch refused: node=x-1 verdict=error reason=corrupted"
        );
        // Any other failure renders the first stderr line, cut at 160 chars.
        let long = format!("e{}", "x".repeat(200));
        let notice = dispatch_notice(false, "", &long, "x-1", "feat");
        assert_eq!(notice.chars().count(), 18 + 160);
        assert_eq!(
            dispatch_notice(false, "", "boom", "x-1", "feat"),
            "grab work failed: boom"
        );
        // No stderr and no stdout: a visible generic, never silence.
        assert_eq!(
            dispatch_notice(false, "", "", "x-1", "feat"),
            "grab work: dispatch failed"
        );
        // Empty output on the spawn leg at exit 0 reads as a failed launch.
        assert_eq!(
            dispatch_notice(true, "", "", "x-1", "feat"),
            "grab work failed: fno agents spawn exited 0 with no pane receipt"
        );
    }

    #[test]
    fn dispatch_spawn_argv_is_pinned() {
        // AC4-HP: node + pane + session + no-wait, and nothing else - no
        // --harness, --model, --route and no message.
        assert_eq!(
            dispatch_spawn_argv("fno", "x-1", "work", None, None),
            vec![
                "fno",
                "agents",
                "spawn",
                "--node",
                "x-1",
                "--substrate",
                "pane",
                "--mux-session",
                "work",
                "--no-wait",
            ]
        );
        // The account and the epic tab ride only when present.
        assert_eq!(
            dispatch_spawn_argv("fno", "x-1", "work", Some("acc"), Some("3")),
            vec![
                "fno",
                "agents",
                "spawn",
                "--node",
                "x-1",
                "--substrate",
                "pane",
                "--mux-session",
                "work",
                "--no-wait",
                "--account",
                "acc",
                "--tab",
                "3",
            ]
        );
    }

    #[test]
    fn node_identity_reads_the_last_json_object() {
        // `fno backlog next` on an empty board prints null: no node.
        assert!(node_identity("null\n").is_none());
        assert!(spawn_receipt("").is_none());
        // The board read carries id/slug/parent.
        assert_eq!(
            node_identity(r#"{"id":"x-1","slug":"feat","parent":null}"#),
            Some(("x-1".to_string(), Some("feat".to_string()), None))
        );
        // A hook note printed before the node must not win.
        assert_eq!(
            node_identity("note\n{\"id\":\"x-2\",\"slug\":\"s2\",\"parent\":\"e1\"}")
                .map(|(id, _slug, parent)| (id, parent)),
            Some(("x-2".to_string(), Some("e1".to_string())))
        );
    }

    #[test]
    fn launch_spawn_argv_is_pinned() {
        // Full-featured request: every optional pin rides as its own argv
        // element; the message NEVER does (it rides stdin at the shell-out).
        let req = AgentLaunchRequest {
            request_id: 1,
            revision: 1,
            cwd: "/tmp/proj".into(),
            harness: "codex".into(),
            substrate: "pane".into(),
            model: Some("gpt-5.6-luna".into()),
            effort: Some("high".into()),
            permission_mode: Some("workspace-write:on-request".into()),
            placement: Some("name:work".into()),
            message: "line one\nline \"two\" $ ` \u{1f600}".into(),
        };
        assert_eq!(
            launch_spawn_argv("fno", &req, "work"),
            vec![
                "fno",
                "agents",
                "spawn",
                "--harness",
                "codex",
                "--cwd",
                "/tmp/proj",
                "--substrate",
                "pane",
                "--mux-session",
                "work",
                "--no-wait",
                "--model",
                "gpt-5.6-luna",
                "--effort",
                "high",
                "--permission-mode",
                "workspace-write:on-request",
                "--tab",
                "name:work",
                "--prompt-file",
                "-",
            ]
        );
        // Minimal request: only the required axes + the stdin seed door.
        let bare = AgentLaunchRequest {
            request_id: 2,
            revision: 1,
            cwd: "/tmp/p2".into(),
            harness: "claude".into(),
            substrate: "thread".into(),
            model: None,
            effort: None,
            permission_mode: None,
            placement: None,
            message: String::new(),
        };
        assert_eq!(
            launch_spawn_argv("fno", &bare, "s"),
            vec![
                "fno",
                "agents",
                "spawn",
                "--harness",
                "claude",
                "--cwd",
                "/tmp/p2",
                "--substrate",
                "thread",
                "--mux-session",
                "s",
                "--no-wait",
                "--prompt-file",
                "-",
            ]
        );
    }

    #[test]
    fn decode_launch_outcome_separates_birth_from_acknowledgment() {
        // Pane receipt with a delivered seed: a verified birth.
        assert_eq!(
            decode_launch_outcome(
                true,
                r#"{"outcome":"launched","name":"w","pane_id":7,"seed":"submitted","pane_observation":"painted"}"#,
                ""
            ),
            LaunchOutcome::Launched {
                name: "w".into(),
                pane: Some(7),
                seed_delivered: Some(true)
            }
        );
        // A bg thread receipt (name + short_id) is a birth with no pane.
        assert_eq!(
            decode_launch_outcome(
                true,
                r#"{"name":"w2","short_id":"a1b2","harness":"claude","status":"spawning"}"#,
                ""
            ),
            LaunchOutcome::Launched {
                name: "w2".into(),
                pane: None,
                seed_delivered: None
            }
        );
        // An intentionally seedless launch: "unattempted" is NOT a failure
        // when the request carried no message - it stays a birth whose seed
        // fact reads false.
        assert_eq!(
            decode_launch_outcome(
                true,
                r#"{"name":"w3","pane_id":9,"seed":"unattempted"}"#,
                ""
            ),
            LaunchOutcome::Launched {
                name: "w3".into(),
                pane: Some(9),
                seed_delivered: Some(false)
            }
        );
        // The door's pre-birth refusal: deliberate retry is safe.
        assert_eq!(
            decode_launch_outcome(
                false,
                "",
                "fno agents spawn: capacity refused: no free slot"
            ),
            LaunchOutcome::Refused("fno agents spawn: capacity refused: no free slot".into())
        );
        // Exit 0 with no readable receipt: never a birth, never a refusal.
        assert_eq!(
            decode_launch_outcome(true, "", ""),
            LaunchOutcome::Unknown("spawn exited 0 with no readable receipt".into())
        );
        // A recovery-required receipt leaves birth unresolved: the child may
        // exist, so this is Unknown, never Refused.
        assert!(matches!(
            decode_launch_outcome(true, r#"{"name":"w4","pane_id":3,"recovered":true}"#, ""),
            LaunchOutcome::Unknown(_)
        ));
    }

    #[test]
    fn dispatch_timeout_exceeds_required_harness_binding_window() {
        let contract: toml::Value = toml::from_str(include_str!(
            "../../../cli/src/fno/agents/harness_capabilities.toml"
        ))
        .unwrap();
        let max_binding_ms = contract["harness"]
            .as_table()
            .unwrap()
            .values()
            .filter_map(|caps| caps.get("session_binding"))
            .filter(|binding| binding["required"].as_bool() == Some(true))
            .filter_map(|binding| binding["timeout_ms"].as_integer())
            .max()
            .unwrap() as u64;
        assert!(dispatch_timeout() >= Duration::from_millis(max_binding_ms + 15_000));
    }
}
