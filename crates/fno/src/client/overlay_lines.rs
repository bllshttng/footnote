//! The sideline overlay line builders the client shares: the navigator's
//! catalog, the peek transcript overlay, and the age cell. Moved out of
//! client.rs for the file-budget ratchet; re-exported so every call site
//! stays put.

use super::*;

const PEEK_OVERLAY_W: usize = 72;

const NAV_OVERLAY_W: usize = 54;

/// Build the navigator overlay lines: a top `find › <query>  [chip]`
/// line, then one line per FILTERED row with a leading state glyph and the
/// cursor row marked `▸`. A row that matched an identity token invisible in
/// its label appends that token as a `·<token>` suffix, so a hit
/// always shows WHY it hit - a row that appears arbitrary reads as a bug. An
/// empty result renders a single `no matches` line (the key handler BELs).
/// `rows` is pre-filtered; `cursor` is pre-clamped.
pub(crate) fn nav_overlay_lines(rows: &[NavRow], nav: &NavView) -> Vec<String> {
    let chip = match nav.state_filter {
        None => "all",
        Some(PaneState::Blocked) => "blocked",
        Some(PaneState::Working) => "working",
        Some(PaneState::DoneUnseen) => "done",
        Some(PaneState::Unmeasured) => "unmeasured",
        Some(PaneState::Idle) => "idle",
        Some(PaneState::Empty) => "empty",
    };
    let mut lines = vec![pad_to(
        &format!(" find › {}   [{chip}]", nav.query),
        NAV_OVERLAY_W,
    )];
    if rows.is_empty() {
        lines.push(pad_to("   no matches", NAV_OVERLAY_W));
        return lines;
    }
    let q = nav.query.to_lowercase();
    for (i, r) in rows.iter().enumerate() {
        let marker = if i == nav.cursor { '▸' } else { ' ' };
        // The reason token: a match-key token that contains the query while
        // the label does not. A query that hits the visible label appends
        // nothing (AC4-UI: the row renders exactly as before).
        let label_lower = r.label.to_lowercase();
        let label = if q.is_empty() || label_lower.contains(&q) {
            r.label.clone()
        } else {
            match r
                .match_key
                .split(' ')
                .find(|t| t.contains(&q) && !label_lower.contains(t))
            {
                Some(token) => format!("{} ·{token}", r.label),
                None => r.label.clone(),
            }
        };
        lines.push(pad_to(
            &format!(" {marker} {} {}", nav_glyph(r.state), label),
            NAV_OVERLAY_W,
        ));
    }
    lines
}

/// The table's last-activity cell: the same buckets as [`humanize_ago`],
/// right-justified to a fixed width of 4 so the column never reflows when a
/// value rolls from `59m` to `1h`. An absent reading renders EMPTY, like the
/// tail cell - the table never fabricates a placeholder value, and `0s` would
/// be a fabricated one. (`fno agents list` prints `?` for the same absence;
/// that lane's rows are one line each, where a blank reads as a bug.)
pub(crate) fn humanize_age(secs: Option<u64>) -> String {
    let body = match secs {
        None => String::new(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86_400 => format!("{}h", s / 3600),
        // Capped at 999d (review finding: an uncapped day count breaks the
        // fixed-width-4 invariant once a row is silent for 1000+ days). A row
        // that old is a display curiosity, not a case worth a 5th column.
        Some(s) => format!("{}d", (s / 86_400).min(999)),
    };
    format!("{body:>4}")
}

/// Build the read-only peek overlay lines: a header (badge glyph + name
/// + full wrapped status sentence), the answerable block when the row is
/// blocked (prompt + numbered options, reused verbatim), a divider, then the
/// transcript body (" loading…" until it arrives, "no activity yet" for an empty
/// one, error/timeout text rendered verbatim as body lines) and a footer hint.
/// `agent` is the LIVE row re-read per frame; `None` means it vanished between
/// key and frame (a transient single frame - the key handler re-anchors/closes).
pub(crate) fn peek_overlay_lines(
    agent: Option<&AgentRow>,
    peek: &PeekView,
    reply: Option<&str>,
    now_secs: u64,
) -> Vec<String> {
    let Some(a) = agent else {
        return vec![pad_to(" peek · row gone", PEEK_OVERLAY_W)];
    };
    // Sanitize every external-sourced line (transcript body, scraped reason)
    // before it becomes overlay cells (codex review): `fno agents peek` reads
    // raw on-disk transcript text that can carry ANSI escapes / C0 controls, and
    // the peek path does NOT VT-parse (unlike pane output), so an unstripped
    // ESC/CR would reach the operator's terminal. Tabs become spaces; every
    // other control char is dropped (a residual bracket-code is harmless text).
    fn sanitize_peek_line(s: &str) -> String {
        s.chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect()
    }
    // the peek header reuses the sideline row's lattice state.
    // `agent_lattice_state` is both exit- and seen-aware (it routes the non-exit
    // case through `pane_state`), so the peek, the row, and the rollups agree
    // and no call site re-derives the precedence.
    let glyph = lattice_glyph(agent_lattice_state(a)).0;
    // The account glyph rides the peek header next to the name, same
    // vocabulary as the selector row.
    let mut header = match a.account.as_deref() {
        Some(acct) => format!(" {glyph} {}  @{acct}", a.name),
        None => format!(" {glyph} {}", a.name),
    };
    // Additive header labels, each present only when its data exists (no
    // placeholder dashes): `changed Ns ago` (a future stamp / clock skew clamps
    // to `0s` via saturating_sub) and `PR #N`.
    if let Some(updated) = a.updated_at {
        header.push_str(&format!(
            " · changed {} ago",
            humanize_ago(now_secs.saturating_sub(updated))
        ));
    }
    if let Some(pr) = a.pr {
        header.push_str(&format!(" · PR #{pr}"));
    }
    if a.started_at.is_some() {
        header.push_str(&format!(
            " · up {}",
            row_meter::up_cell(a.started_at, now_secs)
        ));
    }
    let mut lines = vec![pad_to(&header, PEEK_OVERLAY_W)];
    if let Some(reason) = a.reason.as_deref().filter(|s| !s.is_empty()) {
        let mut wrapped = Vec::new();
        wrap_line(
            &sanitize_peek_line(reason),
            PEEK_OVERLAY_W - 3,
            &mut wrapped,
        );
        for wl in wrapped {
            lines.push(pad_to(&format!("   {wl}"), PEEK_OVERLAY_W));
        }
    }
    // answerable block: prompt + numbered options, mirroring the needs-me
    // overlay's body so a blocked peek reads identically. Digit answers (US3)
    // act on exactly these options.
    if let Some(ans) = &a.answerable {
        lines.push(pad_to("", PEEK_OVERLAY_W));
        if !ans.prompt.is_empty() {
            lines.push(pad_to(
                &format!("   {}", ans.prompt.replace('\n', " ")),
                PEEK_OVERLAY_W,
            ));
        }
        for o in &ans.options {
            lines.push(pad_to(
                &format!("     {}. {}", o.idx, o.label),
                PEEK_OVERLAY_W,
            ));
        }
    }
    lines.push(pad_to("", PEEK_OVERLAY_W)); // divider before the transcript
    match &peek.body {
        None => lines.push(pad_to("   loading…", PEEK_OVERLAY_W)),
        Some(body) if body.is_empty() => lines.push(pad_to("   no activity yet", PEEK_OVERLAY_W)),
        Some(body) => {
            for l in body {
                lines.push(pad_to(
                    &format!(" {}", sanitize_peek_line(l)),
                    PEEK_OVERLAY_W,
                ));
            }
        }
    }
    // The reply input (`m`) replaces the footer while open; else the
    // footer swaps by row state (attach is a dead end on an exited row - the bug
    // US6 closes - so it becomes `r respawn`; `m reply` shows in both).
    match reply {
        Some(buf) => lines.push(pad_to(
            &format!(" reply: {buf}_ (⏎ send · esc cancel)"),
            PEEK_OVERLAY_W,
        )),
        None => lines.push(pad_to(
            if a.exited {
                " j/k peek · m reply · r respawn · esc back"
            } else {
                " j/k peek · digit answers · m reply · ⏎ attach · esc back"
            },
            PEEK_OVERLAY_W,
        )),
    }
    lines
}
