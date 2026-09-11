//! The off-loop registry reader's between-tick memory (x-c914, x-688b):
//! mtime-gated document caches, per-source last-good rows, per-account
//! roster caches, and the read_ok signal that gates the daemon-side
//! registry-absence death rule. Pure over the bytes the server's scan
//! task feeds it, so the derivation is unit-testable here.

use super::*;

/// The reader's between-tick memory. The interval task itself lives in
/// server.rs (it owns the `CoreMsg` sender); this holds the mtime-gated
/// document caches (registry + roster) and the last-sent MERGED row set so
/// the derivation stays pure and unit-testable here.
#[derive(Default)]
pub struct ReaderState {
    reg_raw: Option<String>,
    reg_stamp: Option<(std::time::SystemTime, u64)>,
    roster_raw: Option<String>,
    roster_stamp: Option<(std::time::SystemTime, u64)>,
    /// (x-688b) The registry leg resolved for the death rule: parsed bytes,
    /// real last-good rows, or a confirmed absence. A present-but-garbage
    /// document with nothing last-good reads false.
    reg_ok: bool,
    /// (x-688b) The same resolved fact for the roster leg.
    roster_ok: bool,
    /// Last successfully-derived rows per source, so a torn concurrent write
    /// keeps that source's last-good instead of blanking it (the merged
    /// `last_sent` alone can't distinguish which source went stale).
    last_good_reg: Option<Vec<RegistryAgent>>,
    last_good_roster: Option<Vec<RosterWorker>>,
    /// (x-c914) Per-isolated-account roster caches, keyed by account id. Each
    /// isolated account's `<config_dir>/daemon/roster.json` is stamp-gated and
    /// parsed independently so a torn/corrupt one keeps ITS last-good without
    /// blanking the default roster or the other accounts (AC1-FR per source).
    isolated: std::collections::HashMap<String, IsolatedRoster>,
    last_sent: Option<Vec<RegistryAgent>>,
    /// (x-688b) The read_ok published with `last_sent`, so a readability flip
    /// with unchanged rows still republishes.
    last_sent_ok: bool,
}

/// (x-c914) One isolated account's roster cache: the mtime stamp gate plus the
/// already-parsed+tagged workers (re-parsed only when the stamp moves).
#[derive(Default)]
struct IsolatedRoster {
    stamp: Option<(std::time::SystemTime, u64)>,
    last_good: Option<Vec<RosterWorker>>,
}

/// (x-c914) One isolated account's per-tick roster read, assembled by the
/// server's off-loop scanner (the same stat+conditional-read the default
/// roster uses): `raw` is `Some` only when `stamp` moved past the cache.
pub struct IsolatedRead {
    pub account: String,
    pub stamp: Option<(std::time::SystemTime, u64)>,
    pub raw: Option<String>,
}

impl ReaderState {
    /// The stamp of the currently-cached registry document (the reader's
    /// mtime+len gate for the registry read).
    pub fn reg_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.reg_stamp
    }

    /// The stamp of the currently-cached roster document (the reader's
    /// mtime+len gate for the roster read).
    pub fn roster_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.roster_stamp
    }

    /// (x-c914) The cached stamp of `account`'s isolated roster, so the
    /// server's scanner gates that dir's read the same way it gates the
    /// default roster. `None` for a never-seen account (its first scan reads).
    pub fn isolated_stamp(&self, account: &str) -> Option<(std::time::SystemTime, u64)> {
        self.isolated.get(account).and_then(|c| c.stamp)
    }

    /// (x-688b) Both primary stores resolved: PARSED bytes, real last-good
    /// rows, or a confirmed absence. A present-but-garbage document with no
    /// last-good reads false, and so does a present file whose read keeps
    /// failing (its stamp never advances) - the daemon-side registry-absence
    /// death rule must stay inert in exactly those states.
    pub fn read_ok(&self) -> bool {
        self.reg_ok && self.roster_ok
    }

    /// One tick: fold fresh stats/reads of BOTH files (taken OFF the core loop
    /// by the caller, each behind its own mtime+len gate) and return the
    /// merged row set to publish, or `None` when the merged set is unchanged.
    /// TTL aging re-derives from the cached registry every tick, so a badge
    /// can lapse without a file write. For each source: a torn/garbage
    /// document keeps that source's last-good rows; a vanished file empties
    /// them (the two cases are distinct, AC2-FR).
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        reg_stamp: Option<(std::time::SystemTime, u64)>,
        reg_read: impl FnOnce() -> Option<String>,
        roster_stamp: Option<(std::time::SystemTime, u64)>,
        roster_read: impl FnOnce() -> Option<String>,
        isolated: Vec<IsolatedRead>,
        now_secs: u64,
    ) -> Option<Vec<RegistryAgent>> {
        // Advance the cached stamp ONLY when the read resolves (fresh bytes, or
        // a confirmed vanish). A changed stamp whose read came back empty is a
        // raced/failed read: leave the stamp behind so the next tick's scan gate
        // (stamp != cached) re-attempts the SAME stamp instead of freezing the
        // last-good rows until an unrelated later write happens to move mtime.
        // (x-688b) A no-file-on-either-side agreement is a confirmed absence
        // too (the startup-before-first-write state never enters the arms);
        // the derivation below folds absence into `reg_ok`/`roster_ok`.
        if reg_stamp != self.reg_stamp {
            match (reg_read(), reg_stamp) {
                (Some(raw), _) => {
                    self.reg_raw = Some(raw);
                    self.reg_stamp = reg_stamp;
                }
                (None, None) => {
                    self.reg_raw = None; // vanished
                    self.reg_stamp = None;
                }
                (None, Some(_)) => {} // raced/failed read: keep last-good AND retry next tick
            }
        }
        if roster_stamp != self.roster_stamp {
            match (roster_read(), roster_stamp) {
                (Some(raw), _) => {
                    self.roster_raw = Some(raw);
                    self.roster_stamp = roster_stamp;
                }
                (None, None) => {
                    self.roster_raw = None;
                    self.roster_stamp = None;
                }
                (None, Some(_)) => {}
            }
        }

        // (x-688b) Completeness is PARSE success, not byte presence: a
        // present-but-garbage registry with no last-good rows reads NOT ok,
        // and so does a file whose read keeps failing (cached bytes absent
        // while this tick still SAW the file). Last-good rows carry the ok
        // forward across a torn write (the same fail-safe the row cache
        // itself uses).
        let reg_derived = self
            .reg_raw
            .as_deref()
            .and_then(|raw| derive_rows(raw, now_secs));
        self.reg_ok = match (&self.reg_raw, &reg_derived) {
            (None, _) => reg_stamp.is_none(), // nothing cached AND nothing there
            (Some(_), Some(_)) => true,
            (Some(_), None) => self
                .last_good_reg
                .as_ref()
                .is_some_and(|good| !good.is_empty()),
        };
        let mut reg_rows = match &self.reg_raw {
            // Last-good only rescues a PRESENT-but-garbage document (the torn
            // write); a vanished file empties its source (AC2-EDGE) - falling
            // back here would resurrect rows over a confirmed absence.
            Some(_) => reg_derived
                .or_else(|| self.last_good_reg.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // fno-truth junior badge (x-4a48): fill the no-badge/Idle gap for a
        // bg /target worker between turns from its claim + loop_check recency.
        if let Some(raw) = &self.reg_raw {
            overlay_truth_badges(&mut reg_rows, &build_truth_badges(raw, now_secs));
        }
        self.last_good_reg = Some(reg_rows.clone());

        let roster_parsed = self.roster_raw.as_deref().and_then(parse_roster);
        self.roster_ok = match (&self.roster_raw, &roster_parsed) {
            (None, _) => roster_stamp.is_none(),
            (Some(_), Some(_)) => true,
            (Some(_), None) => self
                .last_good_roster
                .as_ref()
                .is_some_and(|good| !good.is_empty()),
        };
        let roster = match &self.roster_raw {
            Some(_) => roster_parsed
                .or_else(|| self.last_good_roster.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        self.last_good_roster = Some(roster.clone());

        // (x-c914) Fold each isolated account's roster into the union, tagging
        // its workers with the source account. Same stamp-gate + per-source
        // last-good contract as the default roster above; a torn/corrupt file
        // keeps THIS account's last-good and never blanks the others (AC1-FR),
        // a vanished file empties just this account (AC2-EDGE).
        let mut all = roster;
        for r in isolated {
            let cache = self.isolated.entry(r.account.clone()).or_default();
            // Parse + tag ONLY when the stamp moved; an unchanged tick reuses the
            // cached tagged workers (gemini review: no re-parse of an unchanged
            // roster per tick, N accounts x every second). A torn/garbage read
            // keeps last-good (AC1-FR), a vanished file empties it (AC2-EDGE).
            if r.stamp != cache.stamp {
                match (r.raw, r.stamp) {
                    (Some(raw), _) => {
                        if let Some(mut ws) = parse_roster(&raw) {
                            for w in &mut ws {
                                w.account = Some(r.account.clone());
                            }
                            cache.last_good = Some(ws);
                        } // else garbage bytes: keep last-good (AC1-FR)
                        cache.stamp = r.stamp;
                    }
                    (None, None) => {
                        cache.last_good = None;
                        cache.stamp = None;
                    }
                    (None, Some(_)) => {} // raced/failed read: keep last-good, retry
                }
            }
            if let Some(workers) = &cache.last_good {
                all.extend(workers.iter().cloned());
            }
        }

        let rows = merge_rows(reg_rows, &all);
        // (x-688b) Publish when the READ STATE moves too, not only the rows:
        // a readability flip with unchanged last-good rows must still reach
        // the core, or `agents_read_ok` goes stale over a file nobody
        // re-mentions.
        if self.last_sent.as_ref() != Some(&rows) || self.last_sent_ok != self.read_ok() {
            self.last_sent = Some(rows.clone());
            self.last_sent_ok = self.read_ok();
            Some(rows)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(n: u64) -> Option<(std::time::SystemTime, u64)> {
        Some((std::time::UNIX_EPOCH, n))
    }

    /// (x-688b, codex P1) A present-but-garbage registry with no last-good
    /// rows is NOT a successful read: the absence death rule must stay inert
    /// over a document that was never derived.
    #[test]
    fn a_garbage_registry_with_no_last_good_reads_not_ok() {
        let mut state = ReaderState::default();
        state.tick(
            stamp(1),
            || Some("not json at all".into()),
            None,
            || None,
            Vec::new(),
            1_000,
        );
        assert!(!state.read_ok(), "garbage bytes with nothing derived");
        // The same bytes stay not-ok on the next tick.
        state.tick(stamp(1), || None, None, || None, Vec::new(), 1_000);
        assert!(!state.read_ok());
    }

    /// The row change gate answers "did the registry change", never "did the
    /// clock move": derived rows carry the measurement instant, so two ticks
    /// over the same stamp+bytes derive equal rows and the second publishes
    /// nothing. This is the test that failed before the `liveness_age_s`
    /// field stopped ticking.
    #[test]
    fn an_unchanged_registry_publishes_nothing_as_time_passes() {
        let mut state = ReaderState::default();
        let raw = r#"{"schema_version": 6, "agents": [{"name": "w", "cwd": "/tmp", "status": "running", "liveness": "alive", "liveness_measured_at": "2027-01-15T07:59:00Z"}]}"#;
        let first = state.tick(
            stamp(1),
            || Some(raw.to_string()),
            None,
            || None,
            Vec::new(),
            1_800_000_000,
        );
        assert!(first.is_some(), "the first tick always publishes");
        let second = state.tick(
            stamp(1),
            || panic!("an unchanged stamp must not re-read"),
            None,
            || None,
            Vec::new(),
            1_800_000_001,
        );
        assert!(
            second.is_none(),
            "one second later the same bytes derive equal rows: nothing to publish"
        );
    }

    /// (x-688b, codex P1) A readability flip with unchanged rows must still
    /// publish: garbage-from-startup (not ok) turning into a valid empty
    /// registry (ok) keeps the row set identical, and the core would never
    /// learn the read succeeded.
    #[test]
    fn a_read_state_flip_republishes_even_with_unchanged_rows() {
        let mut state = ReaderState::default();
        let first = state.tick(
            stamp(1),
            || Some("garbage".into()),
            None,
            || None,
            Vec::new(),
            1_000,
        );
        assert!(first.is_some(), "the first tick always publishes");
        assert!(!state.read_ok());
        let unchanged = state.tick(stamp(1), || None, None, || None, Vec::new(), 1_000);
        assert!(
            unchanged.is_none(),
            "same bytes, same ok: nothing to publish"
        );
        let flipped = state.tick(
            stamp(2),
            || Some(r#"{"agents":[]}"#.into()),
            None,
            || None,
            Vec::new(),
            1_000,
        );
        assert!(
            flipped.is_some(),
            "rows are still empty but the read state moved"
        );
        assert!(state.read_ok(), "a valid empty registry reads ok");
    }
}
