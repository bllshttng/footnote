//! Registry reader for the sideline agent rows (4a-G2, brief US2).
//!
//! The mux is a READER of the fno-agents registry (brief Locked 5): an
//! off-loop interval task parses `~/.fno/agents/registry.json` and hands the
//! core loop a derived row set; the core joins rows to live panes via the
//! `mux` ref at layout time (pane-exit fact beats any badge). Nothing here
//! ever blocks the core loop, and the render path never touches the file
//! (the origin freeze class rule).
//!
//! The registry is dual-language (fno-agents Rust daemon + Python fno), and
//! its FILE is the contract this module consumes - deliberately parsed via
//! `serde_json::Value` with tolerant field access rather than importing the
//! fno-agents crate: the mux needs five fields, not the daemon.

use std::path::PathBuf;

use crate::proto::{AgentBadge, AnswerablePrompt};

/// One registry row as the sideline consumes it: badge already TTL-derived
/// (the reader knows "now"); the pane-exit fact is joined later, on the core
/// loop, where the live pane set lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryAgent {
    pub name: String,
    pub cwd: String,
    /// Registry status is terminal (exited/permanent-dead).
    pub exited: bool,
    /// In-TTL inside-leg badge; `None` = liveness-only. Never a scraped guess.
    pub badge: Option<AgentBadge>,
    pub reason: Option<String>,
    /// The `mux` ref, when this row is pane-hosted: (session, pane_id).
    pub mux: Option<(String, u64)>,
    /// (x-c929) The answerable-prompt payload from the scrape rung, present only
    /// when this row is `blocked` on a numbered menu the daemon could extract;
    /// `None` for a hook-badged block or a focus-only blocked prompt.
    pub answerable: Option<AnswerablePrompt>,
    /// The `claude attach <id>` target: the claude bg-session jobId
    /// (`claude_short_id`) that lets a paneless watch-only row be attached into
    /// a mux pane. `None` for a row with no jobId (non-claude, or a claude row
    /// that never recorded one). Present regardless of `mux`, but only the
    /// watch-only (paneless) click path consumes it - a pane-hosted row focuses
    /// its pane instead.
    pub attach_id: Option<String>,
}

/// The registry path, resolved exactly as fno-agents' `AgentsHome::from_env`
/// does (`FNO_AGENTS_HOME` > `$HOME/.fno/agents` > `./.fno/agents`).
pub fn registry_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("registry.json");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("agents").join("registry.json")
}

/// Parse the fixed `YYYY-MM-DDThh:mm:ssZ` UTC stamp the registry writes back
/// to epoch seconds (a focused copy of fno-agents' `rfc3339_like_to_secs` -
/// Hinnant days-from-civil; the mux reads the FILE contract, not the crate).
/// Any other shape is `None`, so a malformed stamp ages the badge out (fails
/// closed) rather than pinning a stale `working`.
fn rfc3339_like_to_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut val = 0i64;
        for &ch in b.get(lo..hi)? {
            if !ch.is_ascii_digit() {
                return None;
            }
            val = val * 10 + i64::from(ch - b'0');
        }
        Some(val)
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// True while an inside-leg report is authoritative at `now_secs` (contract
/// v2 / AC-X2-2): no `ttl_ms` never self-ages; a TTL'd report expires once
/// `received_at + ttl_ms` passes; an unparseable `received_at` is expired.
fn report_is_live(received_at: &str, ttl_ms: Option<u64>, now_secs: u64) -> bool {
    let Some(ttl_ms) = ttl_ms else { return true };
    match rfc3339_like_to_secs(received_at) {
        Some(recv) => now_secs.saturating_sub(recv).saturating_mul(1000) <= ttl_ms,
        None => false,
    }
}

/// Derive the sideline row set from raw registry JSON at `now_secs`. Pure so
/// the whole lattice derivation is unit-testable without a file or a clock.
/// A malformed document yields `None` (the caller keeps its last-good rows -
/// a torn concurrent write must not blank the sideline).
pub fn derive_rows(raw: &str, now_secs: u64) -> Option<Vec<RegistryAgent>> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let rows = doc
        .get("agents")
        .or_else(|| doc.get("entries"))?
        .as_array()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(name) = row.get("name").and_then(|v| v.as_str()) else {
            continue; // tolerate an alien row; the registry owners validate
        };
        let cwd = row.get("cwd").and_then(|v| v.as_str()).unwrap_or_default();
        let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let exited = matches!(status, "exited" | "permanent-dead" | "permanent_dead");
        let mux = row.get("mux").and_then(|m| {
            Some((
                m.get("session")?.as_str()?.to_string(),
                m.get("pane_id")?.as_u64()?,
            ))
        });
        // The claude bg jobId, when present, is the `claude attach <id>` target
        // for a paneless row (only claude rows carry it - codex/gemini use their
        // own session-id fields and a different lane).
        let attach_id = row
            .get("claude_short_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (badge, reason, answerable) = match row.get("inside_leg") {
            Some(leg) if !leg.is_null() => {
                let live = report_is_live(
                    leg.get("received_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    leg.get("ttl_ms").and_then(|v| v.as_u64()),
                    now_secs,
                );
                if live {
                    let badge = match leg.get("state").and_then(|v| v.as_str()) {
                        Some("working") => Some(AgentBadge::Working),
                        Some("blocked") => Some(AgentBadge::Blocked),
                        Some("done") => Some(AgentBadge::Done),
                        _ => None,
                    };
                    let reason = leg
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    // Hook-badged blocks carry no answer payload in v1 (the
                    // grammar is a scrape-rung concern); focus-only.
                    (badge, reason, None)
                } else {
                    (None, None, None) // TTL lapsed -> liveness-only (AC2-ERR)
                }
            }
            // Screen-manifest rung (v7): consulted ONLY when no inside_leg
            // report exists at all - a hook-capable row (even TTL-lapsed)
            // never falls through to a scrape verdict (per-capability
            // arbitration; the writer clears screen_state on the flip, this
            // is the reader-side defense in depth).
            _ => match row.get("screen_state") {
                Some(ss) if !ss.is_null() => {
                    let live = report_is_live(
                        ss.get("at").and_then(|v| v.as_str()).unwrap_or(""),
                        ss.get("ttl_ms").and_then(|v| v.as_u64()),
                        now_secs,
                    );
                    if live {
                        // Manifest vocabulary is working|idle|blocked. `idle`
                        // maps to no badge (a plain live row): AgentBadge has
                        // no Idle variant and adding one is a proto bump,
                        // serialized behind the v7 wire work. Anything
                        // malformed fails badge-closed.
                        let badge = match ss.get("state").and_then(|v| v.as_str()) {
                            Some("working") => Some(AgentBadge::Working),
                            Some("blocked") => Some(AgentBadge::Blocked),
                            _ => None,
                        };
                        // The matched rule doubles as the human hint, the way
                        // an inside-leg reason does.
                        let reason = badge
                            .is_some()
                            .then(|| ss.get("rule").and_then(|v| v.as_str()).map(str::to_string))
                            .flatten();
                        // Answer payload only for a live `blocked` scrape verdict
                        // that carried one; a malformed payload degrades to
                        // focus-only (extraction is additive - never blanks the
                        // badge). from_value tolerates the missing/extra field.
                        let answerable = if badge == Some(AgentBadge::Blocked) {
                            ss.get("answerable").filter(|v| !v.is_null()).and_then(|v| {
                                serde_json::from_value::<AnswerablePrompt>(v.clone()).ok()
                            })
                        } else {
                            None
                        };
                        (badge, reason, answerable)
                    } else {
                        (None, None, None) // TTL lapsed -> liveness-only
                    }
                }
                _ => (None, None, None),
            },
        };
        out.push(RegistryAgent {
            name: name.to_string(),
            cwd: cwd.to_string(),
            exited,
            badge,
            reason,
            mux,
            answerable,
            attach_id,
        });
    }
    // Stable order so row-set equality (the change gate) and the rendered
    // sideline are deterministic across ticks.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

/// The reader's between-tick memory. The interval task itself lives in
/// server.rs (it owns the `CoreMsg` sender); this holds the mtime-gated
/// document cache and the last-sent row set so the derivation stays pure and
/// unit-testable here.
#[derive(Default)]
pub struct ReaderState {
    cached_raw: Option<String>,
    cached_stamp: Option<(std::time::SystemTime, u64)>,
    last_sent: Option<Vec<RegistryAgent>>,
}

impl ReaderState {
    /// The stamp of the currently-cached document (the reader's mtime+len
    /// gate: the caller skips the file read when this matches a fresh stat).
    pub fn cached_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.cached_stamp
    }

    /// One tick: fold in a fresh stat/read (both taken OFF the core loop by
    /// the caller) and return the row set to publish, or `None` when nothing
    /// changed. TTL aging re-derives from the cached document every tick, so
    /// a badge can lapse without a file write (AC2-ERR). A malformed document
    /// keeps the last-good rows (a torn concurrent write must not blank the
    /// sideline); a vanished file empties them.
    pub fn tick(
        &mut self,
        stamp: Option<(std::time::SystemTime, u64)>,
        read_if_changed: impl FnOnce() -> Option<String>,
        now_secs: u64,
    ) -> Option<Vec<RegistryAgent>> {
        if stamp != self.cached_stamp {
            self.cached_stamp = stamp;
            match (read_if_changed(), stamp) {
                (Some(raw), _) => self.cached_raw = Some(raw),
                (None, None) => self.cached_raw = None,
                (None, Some(_)) => {} // read raced a writer: keep last-good
            }
        }
        let rows = match &self.cached_raw {
            Some(raw) => derive_rows(raw, now_secs)
                .or_else(|| self.last_sent.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        if self.last_sent.as_ref() != Some(&rows) {
            self.last_sent = Some(rows.clone());
            Some(rows)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(rows: &str) -> String {
        format!(r#"{{"schema_version": 6, "agents": [{rows}]}}"#)
    }

    const NOW: u64 = 1_800_000_000; // 2027-01-15T08:00:00Z-ish

    #[test]
    fn agent_rows_badge_lattice_derives_from_registry() {
        // In-TTL report -> badge; lapsed TTL -> liveness-only (AC2-ERR);
        // terminal status -> exited; mux ref carried through.
        let raw = reg(&format!(
            r#"{{"name":"badged","cwd":"/w","status":"live",
                 "mux":{{"session":"main","pane_id":7}},
                 "inside_leg":{{"state":"blocked","seq":3,"reason":"perm prompt",
                                "received_at":"{recent}","ttl_ms":60000}}}},
               {{"name":"lapsed","cwd":"/w","status":"live",
                 "inside_leg":{{"state":"working","seq":9,
                                "received_at":"2020-01-01T00:00:00Z","ttl_ms":60000}}}},
               {{"name":"gone","cwd":"/x","status":"exited"}}"#,
            recent = "2027-01-15T07:59:30Z",
        ));
        // NOW after the recent stamp but inside its 60s TTL.
        let now = rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap();
        let rows = derive_rows(&raw, now).unwrap();
        assert_eq!(rows.len(), 3);
        let badged = rows.iter().find(|r| r.name == "badged").unwrap();
        assert_eq!(badged.badge, Some(AgentBadge::Blocked));
        assert_eq!(badged.reason.as_deref(), Some("perm prompt"));
        assert_eq!(badged.mux, Some(("main".into(), 7)));
        assert!(!badged.exited);
        let lapsed = rows.iter().find(|r| r.name == "lapsed").unwrap();
        assert_eq!(lapsed.badge, None, "TTL lapse ages to liveness-only");
        let gone = rows.iter().find(|r| r.name == "gone").unwrap();
        assert!(gone.exited);
    }

    #[test]
    fn agent_rows_no_ttl_report_never_self_ages() {
        let raw = reg(r#"{"name":"pinless","cwd":"/w","status":"live",
                "inside_leg":{"state":"done","seq":1,
                              "received_at":"2020-01-01T00:00:00Z"}}"#);
        let rows = derive_rows(&raw, NOW).unwrap();
        assert_eq!(rows[0].badge, Some(AgentBadge::Done));
    }

    #[test]
    fn claude_short_id_becomes_the_attach_target() {
        // A claude bg row's jobId (`claude_short_id`) is the `claude attach <id>`
        // target; a row without one (or with an empty one) is not attachable.
        let raw = reg(
            r#"{"name":"bg","cwd":"/w","status":"live","claude_short_id":"c19cd2c3"},
               {"name":"plain","cwd":"/w","status":"live"}"#,
        );
        let rows = derive_rows(&raw, NOW).unwrap();
        let bg = rows.iter().find(|r| r.name == "bg").unwrap();
        assert_eq!(bg.attach_id.as_deref(), Some("c19cd2c3"));
        let plain = rows.iter().find(|r| r.name == "plain").unwrap();
        assert_eq!(plain.attach_id, None, "no jobId -> not attachable");
    }

    #[test]
    fn agent_rows_malformed_doc_is_none_and_alien_rows_skip() {
        assert_eq!(derive_rows("not json", NOW), None);
        assert_eq!(derive_rows(r#"{"agents": 3}"#, NOW), None);
        // A row without a name is skipped, not fatal.
        let rows = derive_rows(
            &reg(r#"{"cwd":"/w"}, {"name":"ok","cwd":"/w","status":"live"}"#),
            NOW,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ok");
    }

    #[test]
    fn agent_rows_screen_state_rung_badges_only_hookless_rows() {
        // The screen-manifest rung (v7): a hook-less row with a fresh scrape
        // verdict badges (blocked/working); `idle` renders as a plain live
        // row (no AgentBadge::Idle until the next proto bump); a lapsed or
        // malformed verdict ages to liveness-only; and ANY inside_leg report
        // - even TTL-lapsed - keeps a leftover verdict from badging (the hook
        // is unconditionally senior).
        let raw = reg(&format!(
            r#"{{"name":"scraped-blocked","cwd":"/w","status":"live",
                 "screen_state":{{"state":"blocked","rule":"permission_prompt",
                                  "seq":2,"at":"{recent}","ttl_ms":120000}}}},
               {{"name":"scraped-idle","cwd":"/w","status":"live",
                 "screen_state":{{"state":"idle","rule":"idle_prompt",
                                  "seq":1,"at":"{recent}","ttl_ms":120000}}}},
               {{"name":"scraped-lapsed","cwd":"/w","status":"live",
                 "screen_state":{{"state":"working","rule":"busy","seq":1,
                                  "at":"2020-01-01T00:00:00Z","ttl_ms":120000}}}},
               {{"name":"scraped-corrupt","cwd":"/w","status":"live",
                 "screen_state":{{"state":"working","rule":"busy","seq":1,
                                  "at":"garbage","ttl_ms":120000}}}},
               {{"name":"hook-wins","cwd":"/w","status":"live",
                 "inside_leg":{{"state":"working","seq":9,
                                "received_at":"2020-01-01T00:00:00Z","ttl_ms":60000}},
                 "screen_state":{{"state":"blocked","rule":"leftover","seq":1,
                                  "at":"{recent}","ttl_ms":120000}}}}"#,
            recent = "2027-01-15T07:59:30Z",
        ));
        let now = rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap();
        let rows = derive_rows(&raw, now).unwrap();
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        let blocked = get("scraped-blocked");
        assert_eq!(blocked.badge, Some(AgentBadge::Blocked));
        assert_eq!(blocked.reason.as_deref(), Some("permission_prompt"));
        assert_eq!(get("scraped-idle").badge, None, "idle = plain live row");
        assert_eq!(get("scraped-lapsed").badge, None, "TTL lapse ages out");
        assert_eq!(get("scraped-corrupt").badge, None, "corrupt stamp closed");
        assert_eq!(
            get("hook-wins").badge,
            None,
            "a hook-capable row (even lapsed) never badges from a scrape verdict"
        );
    }

    #[test]
    fn agent_rows_malformed_stamp_fails_badge_closed() {
        let raw = reg(r#"{"name":"bad-stamp","cwd":"/w","status":"live",
                "inside_leg":{"state":"working","seq":1,
                              "received_at":"garbage","ttl_ms":60000}}"#);
        let rows = derive_rows(&raw, NOW).unwrap();
        assert_eq!(rows[0].badge, None, "corrupt stamp must not pin a badge");
    }

    #[test]
    fn agent_rows_are_name_sorted_for_deterministic_layouts() {
        let raw = reg(r#"{"name":"zeta","cwd":"/w","status":"live"},
               {"name":"alpha","cwd":"/w","status":"live"}"#);
        let rows = derive_rows(&raw, NOW).unwrap();
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[1].name, "zeta");
    }

    // x-c929: a live `blocked` scrape verdict with an `answerable` payload parses
    // it onto the row; a blocked verdict without one, and a hook-badged block,
    // are both focus-only (no answer payload in v1).
    #[test]
    fn agent_rows_carry_answerable_payload_on_blocked_scrape() {
        let fp = ["7"; 32].join(",");
        let raw = reg(&format!(
            r#"{{"name":"answerable","cwd":"/w","status":"live",
                 "screen_state":{{"state":"blocked","rule":"permission_prompt",
                   "seq":2,"at":"{recent}","ttl_ms":120000,
                   "answerable":{{"prompt":"Do you want to proceed?",
                     "options":[{{"idx":"1","label":"Yes","keystroke":[49]}},
                                {{"idx":"2","label":"No","keystroke":[50]}}],
                     "fingerprint":[{fp}],"region_lines":8}}}}}},
               {{"name":"focus-only","cwd":"/w","status":"live",
                 "screen_state":{{"state":"blocked","rule":"live_blocked_form",
                   "seq":1,"at":"{recent}","ttl_ms":120000}}}},
               {{"name":"hook-blocked","cwd":"/w","status":"live",
                 "inside_leg":{{"state":"blocked","seq":3,
                   "received_at":"{recent}","ttl_ms":60000}}}}"#,
            recent = "2027-01-15T07:59:30Z",
        ));
        let now = rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap();
        let rows = derive_rows(&raw, now).unwrap();
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        let a = get("answerable");
        assert_eq!(a.badge, Some(AgentBadge::Blocked));
        let ans = a
            .answerable
            .as_ref()
            .expect("answerable parsed onto the row");
        assert_eq!(ans.options.len(), 2);
        assert_eq!(ans.options[0].keystroke, b"1");
        assert_eq!(ans.region_lines, 8);
        // Blocked but no payload -> focus-only.
        assert_eq!(get("focus-only").badge, Some(AgentBadge::Blocked));
        assert!(get("focus-only").answerable.is_none());
        // A hook-badged block carries no answer payload in v1.
        assert_eq!(get("hook-blocked").badge, Some(AgentBadge::Blocked));
        assert!(get("hook-blocked").answerable.is_none());
    }
}
