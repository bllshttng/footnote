//! Screen-manifest scrape sweep: the fallback rung of the badge lattice
//! (pane-exit > inside-leg hook > **screen-manifest** > liveness).
//!
//! G4 deleted grid screen-scraping, so an agent that never emits the per-turn
//! inside-leg hook (foreign CLIs, partial-lifecycle agents) dropped to bare
//! alive/dead. This module restores the herdr-style fallback: for each
//! registry row that is mux-hosted AND hook-less, the daemon reads the pane's
//! rendered screen through the mux script API (`fno mux pane ls/read --json`,
//! the same one-shot control verbs scripts use - no mux proto change, no
//! fno-agents<->fno crate dependency in either direction), evaluates the
//! provider's detection manifest ([`crate::manifest`]) against it, and stores
//! the verdict as [`state::ScreenStateReport`] on the row.
//!
//! Arbitration is per-capability, not per-moment (herdr's "no two competing
//! sources of truth"): a row that carries ANY `inside_leg` report - live or
//! TTL-lapsed - is never scraped; its TTL lapse degrades to liveness-only
//! exactly as before. The inside-leg store path clears `screen_state` on the
//! capability flip, and the sweep's write closure re-checks under the
//! registry lock, so a scrape verdict can never shadow a hook.
//!
//! Registry writes are change-gated: a verdict is written when the detected
//! state differs from the stored one, or when the stored stamp is due a
//! freshness refresh (half the reader TTL) - never per sweep per pane. A pane
//! that vanishes (or a mux that stops answering: panes live in the server
//! process, an unreachable server means no panes) clears the stored verdict,
//! so the badge degrades to liveness rather than pinning a stale state; a
//! dead daemon's last verdict ages out via the reader-side TTL.

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::json;

use crate::events::EventEmitter;
use crate::manifest::{load_manifest, AnswerablePrompt, Manifest, Verdict};
use crate::paths::AgentsHome;
use crate::readiness::ScreenView;
use crate::state::{self, Registry, ScreenStateReport};
use crate::AgentStatus;

/// Reader-trust TTL stamped into every verdict (`ScreenStateReport::ttl_ms`).
/// Generous next to the sweep cadence (the daemon's 5s idle tick) so a live
/// daemon always refreshes well before it lapses; tight next to a human
/// glancing at a sideline after the daemon died.
pub const SCREEN_STATE_TTL_MS: u64 = 120_000;

/// Refresh a steady (unchanged) verdict's stamp once it is older than this -
/// half the TTL, so freshness never races the reader's aging gate.
const REFRESH_AFTER_SECS: u64 = SCREEN_STATE_TTL_MS / 2 / 1000;

/// One row the sweep will scrape: mux-hosted, hook-less, not terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrapeTarget {
    pub name: String,
    pub provider: String,
    pub session: String,
    pub pane_id: u64,
    pub last: Option<ScreenStateReport>,
}

/// The eligibility filter (per-capability arbitration). Pure over a loaded
/// registry so the gate is unit-testable without a daemon or a mux.
pub fn scrape_targets(reg: &Registry) -> Vec<ScrapeTarget> {
    reg.entries
        .iter()
        .filter(|e| {
            // ANY inside-leg report (live or lapsed) marks the row
            // hook-capable: the hook owns the signal, TTL lapse degrades to
            // liveness-only, never to a scrape (no authority flapping).
            // Non-live statuses are excluded too: Orphaned (failed
            // reachability probe) and Failed (panicked task) already say the
            // backend is not live, so scraping their last screen would badge
            // a dead pane (codex P2).
            e.inside_leg.is_none()
                && !matches!(
                    e.status,
                    AgentStatus::Exited
                        | AgentStatus::PermanentDead
                        | AgentStatus::Orphaned
                        | AgentStatus::Failed
                )
        })
        .filter_map(|e| {
            e.mux.as_ref().map(|m| ScrapeTarget {
                name: e.name.clone(),
                provider: e.harness_name().to_string(),
                session: m.session.clone(),
                pane_id: m.pane_id,
                last: e.screen_state.clone(),
            })
        })
        .collect()
}

/// What the sweep decided for one target.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing to write (verdict unchanged and fresh, or no evidence yet).
    Hold,
    /// Clear the stored verdict (pane gone / unreadable).
    Clear,
    /// Store this verdict.
    Write(ScreenStateReport),
}

/// True when a stored verdict's stamp is due a freshness refresh. An
/// unparseable stamp counts as due (rewriting it repairs the row).
fn stamp_due_refresh(last: &ScreenStateReport, now_secs: u64) -> bool {
    match state::rfc3339_like_to_secs(&last.at) {
        Some(at) => now_secs.saturating_sub(at) > REFRESH_AFTER_SECS,
        None => true,
    }
}

/// The write-on-change core: fold an evaluation outcome into a [`Decision`].
/// Pure so every branch is unit-testable.
///
/// - No rule matched: hold. The engine never guesses (readiness Open Question
///   #9); the stored verdict stays and ages out via its TTL if the screen
///   never matches again.
/// - `skip_state_update` rule matched (e.g. claude's ctrl+o pager): hold the
///   current state, refreshing its stamp if due so a held state does not age
///   out mid-pager.
/// - State changed: write. State unchanged: write only when the stamp is due
///   a refresh.
pub fn decide(
    last: Option<&ScreenStateReport>,
    verdict: Option<Verdict<'_>>,
    answerable: Option<AnswerablePrompt>,
    now_secs: u64,
    now_stamp: &str,
) -> Decision {
    let Some(v) = verdict else {
        return Decision::Hold;
    };
    if v.skip_state_update {
        return match last {
            // Hold: `..l.clone()` carries the prior answerable through a pager
            // hold, so the queue keeps the last-good payload while state is held.
            Some(l) if stamp_due_refresh(l, now_secs) => Decision::Write(ScreenStateReport {
                at: now_stamp.to_string(),
                seq: l.seq + 1,
                ..l.clone()
            }),
            _ => Decision::Hold,
        };
    }
    match last {
        // A changed answer payload (same blocked state, different options)
        // rewrites too, so the queue reflects a re-prompt without waiting for the
        // stamp refresh. The send-time fingerprint is still the safety authority.
        Some(l)
            if l.state == v.state
                && l.answerable == answerable
                && !stamp_due_refresh(l, now_secs) =>
        {
            Decision::Hold
        }
        _ => Decision::Write(ScreenStateReport {
            state: v.state.to_string(),
            rule: v.rule_id.to_string(),
            seq: last.map_or(1, |l| l.seq + 1),
            at: now_stamp.to_string(),
            ttl_ms: Some(SCREEN_STATE_TTL_MS),
            answerable,
        }),
    }
}

/// The `fno` front-door binary (the Rust mux owner), same resolution as the
/// active-backlog supervisor and the Python spawn back half: `FNO_BIN`
/// overrides for tests and non-PATH installs. `var_os` (not `var`) so a path
/// with non-UTF-8 bytes passes through to `Command` unmangled (gemini MEDIUM).
fn fno_bin() -> std::ffi::OsString {
    std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"))
}

/// `fno mux pane ls --session <s> --json` -> pane_id -> OSC title. `None`
/// when the session is unreachable (no server, skewed binary, bad output) -
/// the caller treats that as "no panes", which clears verdicts: panes live in
/// the server process, so an unanswerable server has no live panes to badge.
///
/// ponytail: no subprocess timeout - the mux CLI bounds its own socket
/// reads/writes, so a wedged server errors instead of hanging; a hung
/// FNO_BIN stalls only this sweep thread (the in-flight gate skips further
/// sweeps rather than piling them up).
fn mux_pane_ls(bin: &std::ffi::OsStr, session: &str) -> Option<BTreeMap<u64, Option<String>>> {
    let out = Command::new(bin)
        .args(["mux", "pane", "ls", "--session", session, "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let panes: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        panes
            .iter()
            .filter_map(|p| {
                Some((
                    p.get("pane_id")?.as_u64()?,
                    p.get("title").and_then(|t| t.as_str()).map(String::from),
                ))
            })
            .collect(),
    )
}

/// `fno mux pane read <pane> --session <s> --json` -> the pane's rendered
/// grid text. `None` on any failure (dead pane, unreachable server).
fn mux_pane_read(bin: &std::ffi::OsStr, session: &str, pane: u64) -> Option<String> {
    let out = Command::new(bin)
        .args([
            "mux",
            "pane",
            "read",
            &pane.to_string(),
            "--session",
            session,
            "--json",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    reply.get("text")?.as_str().map(String::from)
}

/// What the locked write should do for one row, re-checked under the registry
/// lock against the state the row is in NOW (not the snapshot the sweep read).
#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteDisposition {
    /// A capability flip landed since the snapshot: the hook is senior, clear
    /// any scrape verdict so it can never shadow the hook.
    HookFlip,
    /// The row still points at the pane we scraped: apply the verdict.
    Apply,
    /// The row was re-homed or removed+recreated with the same name since the
    /// snapshot (its mux ref no longer matches what we scraped): skip, so the
    /// old pane's verdict never lands on the current pane (codex P2).
    Skip,
}

/// Decide the locked-write disposition for one row. Pure so the arbitration
/// re-checks (hook flip, mux-ref match) are unit-testable without a daemon.
fn write_disposition(e: &state::RegistryEntry, expect_ref: &(String, u64)) -> WriteDisposition {
    if e.inside_leg.is_some() {
        return WriteDisposition::HookFlip;
    }
    let cur_ref = e.mux.as_ref().map(|m| (m.session.clone(), m.pane_id));
    if cur_ref.as_ref() == Some(expect_ref) {
        WriteDisposition::Apply
    } else {
        WriteDisposition::Skip
    }
}

/// One sweep pass: load -> filter -> read screens -> evaluate -> batch the
/// changed rows into one locked registry write. Synchronous by design (file
/// IO + subprocesses); the daemon runs it under `spawn_blocking` off the
/// idle tick, gated so at most one sweep is in flight.
///
/// `notify_on_blocked` (config.mux.notify_on_blocked, x-dd84) fires one OS
/// notification when a scraped verdict ENTERS `blocked`; the manifest path has
/// no `done`, so notify_on_done is not plumbed here.
pub fn scrape_sweep(home: &AgentsHome, emitter: &EventEmitter, notify_on_blocked: bool) {
    let Ok(reg) = state::load_registry(&home.registry_json()) else {
        return;
    };
    let targets = scrape_targets(&reg);
    if targets.is_empty() {
        return;
    }
    let bin = fno_bin();
    let override_dir = home.manifests_dir();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let now_stamp = crate::daemon::now_rfc3339_like();

    // Per-provider manifest cache (a parse-bad manifest logs once per sweep,
    // not once per pane) and per-session pane listing (one `ls` per session).
    let mut manifests: BTreeMap<String, Option<Manifest>> = BTreeMap::new();
    let mut sessions: BTreeMap<String, Option<BTreeMap<u64, Option<String>>>> = BTreeMap::new();

    // (name, expected-mux-ref, verdict). The ref is re-verified under the
    // lock so a row removed+recreated (or re-homed to a new pane) with the
    // same name since the snapshot never gets the old pane's verdict (codex P2).
    let mut changes: Vec<(String, (String, u64), Option<ScreenStateReport>)> = Vec::new();
    for t in &targets {
        let manifest = manifests.entry(t.provider.clone()).or_insert_with(|| {
            match load_manifest(&t.provider, Some(&override_dir)) {
                Some(Ok(m)) => Some(m),
                Some(Err(e)) => {
                    // Present-but-malformed (a bad hand-authored override)
                    // fails loud in the event log, never silently falls back.
                    let _ = emitter.emit(
                        "screen_state_change",
                        &json!({"provider": t.provider, "error": e.to_string()}),
                    );
                    None
                }
                // Unknown provider: no manifest, never scraped (liveness-only).
                None => None,
            }
        });
        let Some(manifest) = manifest else {
            continue;
        };
        let panes = sessions
            .entry(t.session.clone())
            .or_insert_with(|| mux_pane_ls(&bin, &t.session));
        let evidence = panes
            .as_ref()
            .and_then(|p| p.get(&t.pane_id))
            .map(|title| (title.clone(), mux_pane_read(&bin, &t.session, t.pane_id)));
        let decision = match evidence {
            // Pane absent from the listing, or its read failed: no screen to
            // trust. Clear a stored verdict; a never-badged row stays silent.
            None | Some((_, None)) => {
                if t.last.is_some() {
                    Decision::Clear
                } else {
                    Decision::Hold
                }
            }
            Some((title, Some(text))) => {
                let view = ScreenView {
                    visible_text: &text,
                    // The manifest engine reads regions of text + OSC strings;
                    // cursor position is a readiness-detector concern.
                    cursor_row: 0,
                    cursor_col: 0,
                    osc_title: title.as_deref(),
                    // The mux surfaces titles (PaneInfo.title) but not OSC 9;4
                    // progress; no bundled rule reads osc_progress today.
                    osc_progress: None,
                };
                let (verdict, answerable) = match manifest.evaluate_answerable(&view) {
                    Some((v, a)) => (Some(v), a),
                    None => (None, None),
                };
                decide(t.last.as_ref(), verdict, answerable, now_secs, &now_stamp)
            }
        };
        let expect_ref = (t.session.clone(), t.pane_id);
        match decision {
            Decision::Hold => {}
            Decision::Clear => changes.push((t.name.clone(), expect_ref, None)),
            Decision::Write(rep) => changes.push((t.name.clone(), expect_ref, Some(rep))),
        }
    }
    if changes.is_empty() {
        return;
    }
    // Badge-transition notify intents (x-dd84): (agent name, matched rule).
    // Captured under the flock from prev-vs-new screen_state; fired after the
    // write so a slow notifier can never stall the sweep.
    let mut blocked_notifs: Vec<(String, String)> = Vec::new();
    let write = state::update_registry(&home.registry_json(), |r| {
        for (name, expect_ref, rep) in &changes {
            if let Some(e) = r.find_mut(name) {
                match write_disposition(e, expect_ref) {
                    WriteDisposition::HookFlip => e.screen_state = None,
                    WriteDisposition::Apply => {
                        if notify_on_blocked {
                            if let Some(new_rep) = rep {
                                let prev_blocked = e
                                    .screen_state
                                    .as_ref()
                                    .is_some_and(|s| s.state == "blocked");
                                if new_rep.state == "blocked" && !prev_blocked {
                                    blocked_notifs.push((name.clone(), new_rep.rule.clone()));
                                }
                            }
                        }
                        e.screen_state = rep.clone();
                    }
                    WriteDisposition::Skip => {}
                }
            }
        }
    });
    if write.is_err() {
        return; // nothing published; next sweep retries
    }
    for (name, rule) in blocked_notifs {
        crate::daemon::notify_transition(name, rule);
    }
    for (name, _, rep) in &changes {
        let _ = emitter.emit(
            "screen_state_change",
            &json!({
                "name": name,
                "state": rep.as_ref().map(|r| r.state.clone()),
                "rule": rep.as_ref().map(|r| r.rule.clone()),
                "seq": rep.as_ref().map(|r| r.seq),
                "cleared": rep.is_none(),
            }),
        );
    }
}

/// The hidden `fno-agents detect` debug verb (precedent: the hidden `claim`
/// verb - matched with `matches!` in bin/client.rs, out of CLIENT_VERB_USAGE
/// and the routable-verb parity guard). `detect explain <agent>` prints which
/// authority currently badges the agent and, for screen-manifest, the matched
/// rule + stored verdict + age. Read-only over the registry; no live
/// re-evaluation in v1.
pub fn run_detect(args: &[String]) -> i32 {
    let (Some("explain"), Some(name)) = (args.first().map(String::as_str), args.get(1)) else {
        eprintln!("usage: fno-agents detect explain <agent>");
        return 2;
    };
    let home = AgentsHome::from_env();
    let reg = match state::load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents: detect: registry read failed: {e}");
            return 1;
        }
    };
    let Some(entry) = reg.find(name) else {
        eprintln!("fno-agents: detect: no such agent: {name}");
        return 1;
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = |stamp: &str| -> String {
        match state::rfc3339_like_to_secs(stamp) {
            Some(at) => format!("{}s", now_secs.saturating_sub(at)),
            None => format!("unparseable stamp {stamp:?}"),
        }
    };
    println!("agent: {} (provider {})", entry.name, entry.harness_name());
    // Mirrors the reader lattice: pane-exit > hook (capability) >
    // screen-manifest > liveness.
    if matches!(
        entry.status,
        AgentStatus::Exited | AgentStatus::PermanentDead
    ) {
        println!("authority: pane-exit (status {:?})", entry.status);
    } else if let Some(leg) = &entry.inside_leg {
        if leg.is_live_at(now_secs) {
            println!(
                "authority: hook (inside-leg report: state {:?}, seq {}, age {})",
                leg.state,
                leg.seq,
                age(&leg.received_at)
            );
        } else {
            println!(
                "authority: liveness (hook report lapsed: seq {}, age {}; \
                 row is hook-capable so the screen-manifest rung stays off)",
                leg.seq,
                age(&leg.received_at)
            );
        }
    } else if let Some(ss) = &entry.screen_state {
        if ss.is_live_at(now_secs) {
            println!(
                "authority: screen-manifest (rule {:?} -> state {:?}, seq {}, age {}, ttl {:?}ms)",
                ss.rule,
                ss.state,
                ss.seq,
                age(&ss.at),
                ss.ttl_ms
            );
        } else {
            println!(
                "authority: liveness (scrape verdict lapsed: rule {:?}, age {})",
                ss.rule,
                age(&ss.at)
            );
        }
    } else {
        println!("authority: liveness (no hook report, no scrape verdict)");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn rep(state: &str, at: &str, seq: u64) -> ScreenStateReport {
        ScreenStateReport {
            state: state.into(),
            rule: "r".into(),
            seq,
            at: at.into(),
            ttl_ms: Some(SCREEN_STATE_TTL_MS),
            answerable: None,
        }
    }

    fn entry(name: &str, provider: &str) -> state::RegistryEntry {
        state::RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            legacy_provider: provider.into(),
            harness: None,
            harness_session_id: None,
            cwd: "/tmp/x".into(),
            project_root: String::new(),
            session_id: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-07-02T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
        }
    }

    const NOW_STAMP: &str = "2026-07-02T00:10:00Z";
    fn now_secs() -> u64 {
        state::rfc3339_like_to_secs(NOW_STAMP).unwrap()
    }

    // -- eligibility ------------------------------------------------------

    #[test]
    fn scrape_targets_selects_only_hookless_live_mux_rows() {
        let mut reg = Registry::default();
        // Eligible: mux-hosted, no inside_leg, live.
        let mut ok = entry("scrapeme", "codex");
        ok.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 7,
        });
        reg.entries.push(ok);
        // Hook-capable (even a lapsed report): never scraped.
        let mut hooked = entry("hooked", "claude");
        hooked.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 8,
        });
        hooked.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: "2020-01-01T00:00:00Z".into(),
            ttl_ms: Some(1),
        });
        reg.entries.push(hooked);
        // Not mux-hosted: nothing to read.
        reg.entries.push(entry("worker", "codex"));
        // Non-live statuses are excluded: Exited/PermanentDead (pane-exit
        // fact) and Orphaned/Failed (backend not live - codex P2).
        for (i, status) in [
            AgentStatus::Exited,
            AgentStatus::PermanentDead,
            AgentStatus::Orphaned,
            AgentStatus::Failed,
        ]
        .into_iter()
        .enumerate()
        {
            let mut dead = entry(&format!("dead{i}"), "codex");
            dead.mux = Some(state::MuxRef {
                session: "main".into(),
                pane_id: 90 + i as u64,
            });
            dead.status = status;
            reg.entries.push(dead);
        }

        let targets = scrape_targets(&reg);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "scrapeme");
        assert_eq!(targets[0].session, "main");
        assert_eq!(targets[0].pane_id, 7);
    }

    #[test]
    fn write_disposition_rechecks_hook_flip_and_mux_ref() {
        // The locked-write re-checks against the row's CURRENT state, not the
        // snapshot the sweep read.
        let scraped = ("main".to_string(), 7u64);

        // Row still on the scraped pane -> Apply.
        let mut row = entry("r", "codex");
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 7,
        });
        assert_eq!(write_disposition(&row, &scraped), WriteDisposition::Apply);

        // A capability flip landed since the snapshot -> HookFlip (clear).
        row.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: NOW_STAMP.into(),
            ttl_ms: None,
        });
        assert_eq!(
            write_disposition(&row, &scraped),
            WriteDisposition::HookFlip
        );

        // Row re-homed to a new pane since the snapshot -> Skip (codex P2:
        // never stamp the old pane's verdict onto the new pane).
        let mut rehomed = entry("r", "codex");
        rehomed.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 8,
        });
        assert_eq!(
            write_disposition(&rehomed, &scraped),
            WriteDisposition::Skip
        );

        // Row lost its mux ref entirely -> Skip (not this pane anymore).
        let mut unhosted = entry("r", "codex");
        unhosted.mux = None;
        assert_eq!(
            write_disposition(&unhosted, &scraped),
            WriteDisposition::Skip
        );
    }

    // -- decide: write-on-change ------------------------------------------

    fn verdict_from<'m>(manifest: &'m Manifest, text: &str) -> Option<Verdict<'m>> {
        let view = ScreenView {
            visible_text: text,
            cursor_row: 0,
            cursor_col: 0,
            osc_title: None,
            osc_progress: None,
        };
        manifest.evaluate(&view)
    }

    fn one_rule_manifest(state: &str, needle: &str, skip: bool) -> Manifest {
        Manifest::parse(&format!(
            "[[rule]]\nid = \"r\"\nstate = \"{state}\"\npriority = 100\n\
             region = \"whole_recent\"\nskip_state_update = {skip}\n\
             gate = {{ contains = \"{needle}\" }}\n"
        ))
        .unwrap()
    }

    #[test]
    fn decide_first_verdict_writes_seq_one_with_ttl() {
        let m = one_rule_manifest("working", "esc to interrupt", false);
        let d = decide(
            None,
            verdict_from(&m, "esc to interrupt"),
            None,
            now_secs(),
            NOW_STAMP,
        );
        let Decision::Write(rep) = d else {
            panic!("expected Write, got {d:?}");
        };
        assert_eq!(rep.state, "working");
        assert_eq!(rep.rule, "r");
        assert_eq!(rep.seq, 1);
        assert_eq!(rep.at, NOW_STAMP);
        assert_eq!(rep.ttl_ms, Some(SCREEN_STATE_TTL_MS));
    }

    #[test]
    fn decide_unchanged_fresh_verdict_holds_no_churn() {
        // Same state, stamp 10s old (< refresh threshold): no write.
        let m = one_rule_manifest("working", "busy", false);
        let last = rep("working", "2026-07-02T00:09:50Z", 4);
        let d = decide(
            Some(&last),
            verdict_from(&m, "busy"),
            None,
            now_secs(),
            NOW_STAMP,
        );
        assert_eq!(d, Decision::Hold);
    }

    #[test]
    fn decide_unchanged_stale_stamp_refreshes() {
        // Same state but the stamp is past the refresh threshold: rewrite so
        // the reader-side TTL never lapses under a live daemon.
        let m = one_rule_manifest("working", "busy", false);
        let last = rep("working", "2026-07-02T00:00:00Z", 4);
        let d = decide(
            Some(&last),
            verdict_from(&m, "busy"),
            None,
            now_secs(),
            NOW_STAMP,
        );
        let Decision::Write(new) = d else {
            panic!("expected refresh Write, got {d:?}");
        };
        assert_eq!(new.state, "working");
        assert_eq!(new.seq, 5);
        assert_eq!(new.at, NOW_STAMP);
    }

    #[test]
    fn decide_state_change_writes_immediately() {
        let m = one_rule_manifest("blocked", "Do you want to proceed?", false);
        let last = rep("working", NOW_STAMP, 2);
        let d = decide(
            Some(&last),
            verdict_from(&m, "Do you want to proceed?"),
            None,
            now_secs(),
            NOW_STAMP,
        );
        let Decision::Write(new) = d else {
            panic!("expected Write, got {d:?}");
        };
        assert_eq!(new.state, "blocked");
        assert_eq!(new.seq, 3);
    }

    #[test]
    fn decide_no_match_holds_engine_never_guesses() {
        let m = one_rule_manifest("working", "busy", false);
        let last = rep("idle", NOW_STAMP, 1);
        assert_eq!(
            decide(
                Some(&last),
                verdict_from(&m, "nothing here"),
                None,
                now_secs(),
                NOW_STAMP
            ),
            Decision::Hold
        );
        assert_eq!(
            decide(
                None,
                verdict_from(&m, "nothing here"),
                None,
                now_secs(),
                NOW_STAMP
            ),
            Decision::Hold
        );
    }

    // -- detect explain (hidden debug verb) --------------------------------

    /// AC: bad usage exits 2; an unknown agent exits 1 with a one-line error;
    /// a known agent explains and exits 0. Takes the crate-wide env lock
    /// (FNO_AGENTS_HOME mutation).
    #[test]
    fn run_detect_explain_exit_codes() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        let home = AgentsHome::from_env();
        let mut scraped = entry("scrapee", "codex");
        scraped.screen_state = Some(rep("idle", "2026-07-02T00:00:00Z", 1));
        state::update_registry(&home.registry_json(), |r| r.entries.push(scraped)).unwrap();

        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run_detect(&s(&[])), 2, "missing op is a usage error");
        assert_eq!(run_detect(&s(&["explain"])), 2, "missing agent name");
        assert_eq!(run_detect(&s(&["explain", "ghost"])), 1, "unknown agent");
        assert_eq!(run_detect(&s(&["explain", "scrapee"])), 0);

        std::env::remove_var("FNO_AGENTS_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- sweep end-to-end over a stubbed mux CLI ---------------------------

    /// Full sweep pass against a stub FNO_BIN: first sweep writes the verdict,
    /// an unchanged screen holds (no churn), a changed screen rewrites, and a
    /// vanished pane clears. Takes the crate-wide env lock (FNO_BIN mutation).
    #[test]
    fn scrape_sweep_writes_updates_and_clears_via_stub_mux() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-scrape-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home = AgentsHome::at(dir.join("agents"));
        home.ensure_root().unwrap();

        // One hook-less codex pane in session "main".
        let mut row = entry("scrapee", "codex");
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 7,
        });
        state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();

        // Stub mux CLI: `pane ls` and `pane read` answer from files the test
        // rewrites between sweeps.
        let ls_path = dir.join("ls.json");
        let read_path = dir.join("read.json");
        let stub = dir.join("fno-stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\ncase \"$3\" in\nls) cat {} ;;\nread) cat {} ;;\nesac\n",
                ls_path.display(),
                read_path.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("FNO_BIN", &stub);

        let live_pane =
            r#"[{"pane_id":7,"squad_id":1,"tab_id":1,"cwd":"/w","child_pid":42,"title":null}]"#;
        // codex.toml idle rule: a lone composer prompt on the last line.
        std::fs::write(&ls_path, live_pane).unwrap();
        std::fs::write(&read_path, r#"{"pane_id":7,"text":"some scrollback\n› "}"#).unwrap();
        let emitter = EventEmitter::new(home.events_jsonl(), "test");

        scrape_sweep(&home, &emitter, false);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let v = reg.entries[0].screen_state.clone().expect("verdict stored");
        assert_eq!(v.state, "idle");
        assert_eq!(v.seq, 1);

        // Unchanged screen, fresh stamp: no write (seq stays).
        scrape_sweep(&home, &emitter, false);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.entries[0].screen_state.as_ref().unwrap().seq, 1);

        // Screen flips to working (codex busy line): rewrite.
        std::fs::write(
            &read_path,
            r#"{"pane_id":7,"text":"Working (3s • esc to interrupt)"}"#,
        )
        .unwrap();
        scrape_sweep(&home, &emitter, false);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let v = reg.entries[0].screen_state.clone().expect("verdict kept");
        assert_eq!(v.state, "working");
        assert_eq!(v.seq, 2);

        // Pane vanishes from the listing: verdict cleared (degrade to
        // liveness, never a stale badge).
        std::fs::write(&ls_path, "[]").unwrap();
        scrape_sweep(&home, &emitter, false);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.entries[0].screen_state, None);

        std::env::remove_var("FNO_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decide_skip_state_update_holds_current_and_keeps_it_fresh() {
        // A pager-style rule must not flip the state; with a stale stamp it
        // refreshes the HELD state (not the rule's own `state`).
        let m = one_rule_manifest("idle", "Showing detailed transcript", true);
        let held = rep("working", "2026-07-02T00:00:00Z", 6);
        let d = decide(
            Some(&held),
            verdict_from(&m, "Showing detailed transcript"),
            None,
            now_secs(),
            NOW_STAMP,
        );
        let Decision::Write(new) = d else {
            panic!("expected refresh Write, got {d:?}");
        };
        assert_eq!(new.state, "working", "held state, not the rule's");
        assert_eq!(new.seq, 7);
        // Fresh stamp -> pure hold. No prior state -> nothing to hold.
        let fresh = rep("working", NOW_STAMP, 6);
        assert_eq!(
            decide(
                Some(&fresh),
                verdict_from(&m, "Showing detailed transcript"),
                None,
                now_secs(),
                NOW_STAMP
            ),
            Decision::Hold
        );
        assert_eq!(
            decide(
                None,
                verdict_from(&m, "Showing detailed transcript"),
                None,
                now_secs(),
                NOW_STAMP
            ),
            Decision::Hold
        );
    }
}
