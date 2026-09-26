//! How `fno agents resume` picks the argv for a claude row: attach a live
//! supervisor, relaunch an exited one, or refuse naming the cause.

use std::fs;

use serde_json::Value;

use crate::claude_ask::{liveness_probe, locate_session, ClaudeHome};
use crate::resume_wake::is_uuid_shaped;
use crate::truth_probe::family1_truth_state_for_resume;

/// The claude arm of `resume` (Fix 1): liveness-probe first, then pick the
/// argv. A live (incl. idle) supervisor -> `claude attach <short_id>` (today's
/// behavior); a dead/absent one -> `claude --resume <uuid>` in the recorded cwd.
/// Probe reality (locate_session + a 250 ms socket connect), never the registry
/// `status` field: a stale-exited row whose supervisor is actually alive must
/// attach, not `--resume` into a second writer on one transcript. The chosen lane
/// is printed to stderr before returning so the operator always knows which
/// fired. `Err(code)` carries the exit code for the uuid-absent refusal.
/// Returns `(argv, claim_uuid)`. `claim_uuid` is `Some(uuid)` only for the
/// dead-arm (`claude --resume`), which the caller must guard with the
/// `session:<uuid>` single-writer claim before exec; the live attach arm returns
/// `None` (claude's own supervisor owns attach safety).
pub(crate) fn claude_resume_argv(
    claude_home: &ClaudeHome,
    entry: &Value,
    name: &str,
) -> Result<(Vec<String>, Option<String>), i32> {
    claude_resume_argv_with_truth(claude_home, entry, name, family1_truth_state_for_resume)
}

pub(crate) fn claude_resume_argv_with_truth<F>(
    claude_home: &ClaudeHome,
    entry: &Value,
    name: &str,
    truth_fn: F,
) -> Result<(Vec<String>, Option<String>), i32>
where
    F: Fn(&str) -> Option<String>,
{
    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    // `claude_session_uuid` never serializes (skip_serializing), so an adopted
    // typed row - `serde_json::to_value(&RegistryEntry)`, the manifest adopt
    // path - carries only `harness_session_id`. Fall back to it, mirroring
    // `resume_session_id`, instead of refusing the row as inconclusive.
    let uuid = entry
        .get("claude_session_uuid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            entry
                .get("harness_session_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");
    let has_uuid = is_uuid_shaped(uuid);

    let socket_live = !short_id.is_empty()
        && locate_session(claude_home, short_id)
            .map(|loc| liveness_probe(&loc.messaging_socket_path))
            .unwrap_or(false);
    // Probe on the canonical uuid whenever one is recorded. This used to also
    // short-circuit on an empty short_id, so a pane worker (no short_id by
    // design: _validate_single_live_ref enforces mux XOR worker XOR bg) never
    // probed and reported "liveness is inconclusive" for a session whose uuid
    // was resolvable - the bug. The attach arm below gates on a present
    // short_id, so dropping the short_id term lets a mux row probe without ever
    // issuing a bare `claude attach ""`.
    let truth_state = if socket_live || uuid.is_empty() {
        None
    } else {
        truth_fn(uuid)
    };
    let live = socket_live
        || matches!(
            truth_state.as_deref(),
            Some("working" | "watching" | "your-move")
        );
    let dead = matches!(truth_state.as_deref(), Some("done" | "stalled"));

    if live && !short_id.is_empty() {
        // The caller decides whether to print the command, deliver through
        // control.sock, or use a mux pane; downstream output names the action.
        eprintln!("fno agents resume: {name} is live");
        let argv = crate::harness_capabilities::render_session_argv_with_ids(
            "claude",
            "interactive_attach",
            None,
            Some(short_id),
        )
        .map_err(|_| 13)?;
        Ok((argv, None))
    } else if dead && has_uuid {
        // this arm RELAUNCHES (the live arm above only attaches), so it
        // is the one door on this verb that can lose a route. A row that records
        // one gets it re-applied through `--settings`, the same mechanism the
        // original spawn used; a recorded file that is gone refuses rather than
        // relaunching on the default Anthropic account, which works, bills the
        // wrong vendor, and reports nothing.
        let route_settings = entry
            .get("route_settings_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let mut argv = crate::harness_capabilities::render_session_argv(
            "claude",
            "interactive_resume",
            Some(uuid),
        )
        .map_err(|_| 13)?;
        if let Some(path) = route_settings {
            // Present is not enough. The file is the auth-scrub floor with the
            // route written on top, and claude reads an empty settings value as
            // UNSET - so a floor-only or malformed file hands claude a settings
            // file that selects nothing and the worker comes back on the default
            // account in silence. That is the same outcome as a missing file, so
            // it takes the same refusal. Python's `read_route_settings` applies
            // the identical rule; a check here that only tested existence would
            // make these two doors disagree while the docs call them equivalent.
            let usable = fs::read_to_string(path).ok().and_then(|raw| {
                serde_json::from_str::<Value>(&raw).ok().map(|v| {
                    v.get("env").and_then(Value::as_object).is_some_and(|env| {
                        env.values()
                            .any(|x| x.as_str().is_some_and(|s| !s.is_empty()))
                    })
                })
            });
            if usable != Some(true) {
                let why = match usable {
                    None => "cannot be read as a route settings file",
                    _ => "records no route",
                };
                eprintln!(
                    "fno agents resume: {name} was launched on the route recorded at \
                     {path}, and it {why}; refusing to relaunch it on the default \
                     account. Re-spawn with an explicit --route/-P to choose one."
                );
                return Err(13);
            }
            eprintln!("fno agents resume: restoring recorded route from {path}");
            argv.splice(1..1, ["--settings".into(), path.into()]);
        }
        eprintln!("fno agents resume: {name} has exited - resuming in your terminal");
        Ok((argv, Some(uuid.to_string())))
    } else if !has_uuid {
        // No resumable uuid and no live socket to attach through: name the cause.
        // AC2: an id-less row is a definite "nothing to resume", never the
        // "liveness is inconclusive" that printed an unrunnable empty-id hint and
        // hid the real bug.
        eprintln!("fno agents resume: {name} has no session id recorded; nothing to resume.");
        Err(13)
    } else if live {
        // Probe-live but no short_id to attach through: a pane/mux worker that
        // is already running. There is no resume action here - `claude attach`
        // needs a short_id this row does not carry, and relaunching would open a
        // second writer on one transcript. Do not call this "inconclusive": the
        // probe just answered live, and the old hint sent the operator to re-run
        // a probe whose answer contradicts the message.
        eprintln!(
            "fno agents resume: {name} is live but has no attach short_id \
             (a pane worker); it is already running - drive it via its mux session, \
             or re-spawn with `fno agents spawn`."
        );
        Err(13)
    } else {
        // has_uuid but neither attachable-live nor affirmatively dead: genuinely
        // inconclusive (a silent-unreachable worker that may still be alive).
        // Name the uuid the operator can probe, not the empty short_id the old
        // hint interpolated.
        eprintln!(
            "fno agents resume: {name} liveness is inconclusive; refusing to open a second writer. Run 'fno agents truth {uuid}'."
        );
        Err(13)
    }
}
