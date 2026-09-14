//! The mux keystroke's launch machinery (x-3873 change 3): board selection
//! inputs, the door argv, the bounded shell-out, and the notice mapping.
//! Lifted out of `server.rs` as its own module named by the question it
//! answers - "how does a mux dispatch launch a node?" - so the shrink-only
//! ratchet on `server.rs` is not fed by new code.

use std::time::Duration;

/// Bounded + fail-open (the digest_overlay idiom): read the board, launch the
/// door, turn the outcome into the client notice. An empty return says nothing
/// (the launched pane speaks for itself); every error path yields a visible
/// notice rather than a silent no-op (x-6f77).
pub(crate) fn dispatch_timeout() -> Duration {
    Duration::from_secs(75)
}

/// The launch argv for a dispatch (x-3873 change 3, step 3): pure so a unit
/// test can pin it. The door is the ONE launcher (x-e53e) and now the mux's
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
    // (x-c914) The client's session-local active account rides the same flag
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

/// One bounded `fno` shell-out capturing both streams (x-3873 change 3): the
/// board reads need stdout; the spawn leg needs the door's stderr for the
/// refusal-reason mapping. `None` on a timeout or a failed spawn - the caller
/// renders its own notice for that.
pub(crate) async fn run_fno_captured(
    argv: &[&str],
    timeout: Duration,
) -> Option<(bool, String, String)> {
    let mut command = crate::process_admission::tokio_command(argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => None,
        Ok(Err(_)) => None,
        Ok(Ok(o)) => Some((
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )),
    }
}

/// Map a dispatch launch to the one-line client notice (x-3873 change 3,
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
        let mut receipt: Option<serde_json::Value> = None;
        for line in stdout.lines() {
            let line = line.trim();
            if !line.starts_with('{') {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("pane_id").is_some() {
                    receipt = Some(v);
                }
            }
        }
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
    let mut detail = crate::server::first_line_or(stderr, "");
    if detail.is_empty() {
        detail = crate::server::first_line_or(stdout, "");
    }
    if detail.chars().count() > 160 {
        detail = detail.chars().take(160).collect();
    }
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
        assert!(node_identity("").is_none());
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
