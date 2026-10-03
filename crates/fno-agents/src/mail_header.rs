//! The delivered mail header: one backticked first line,
//! `` `@candor · msg-f7aa93 · short summary` ``, where the `<fno_mail>` tag
//! used to open the turn.
//!
//! The header replaces the tag at the source (the envelope renderer writes
//! it; no display-time rewrite). One line carries the three facts every
//! machine reader keyed off the tag for: the shape (this is mail, never an
//! operator turn), the msg id (reply resolution and drain dedup join on it),
//! and a short summary. The full body follows on the next lines, wholly
//! visible - nothing in a turn is hidden.
//!
//! A body can never forge one: any line shaped like a header inside a sent
//! body refuses the send, the same rule as the tag-in-body refusal it
//! replaces (the forged tag could read as a second message; so can the
//! header line).

use serde_json::Value;

/// The summary cut: at most this many words of the body's first sentence.
pub const SUMMARY_MAX_WORDS: usize = 12;

/// Which spelling a delivered header's sender takes. One data field per
/// harness row in the capability contract (the composer check sets it), so
/// the renderer branches on data, never on a harness name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderForm {
    /// `` `@candor · msg-f7aa93 · ...` `` - the default; `@` sits inside
    /// inline code so harness mention pickers never fire.
    Mention,
    /// `` `candor · msg-f7aa93 · ...` `` - the fallback for a harness whose
    /// composer check failed on the `@` form.
    Plain,
}

impl HeaderForm {
    /// The contract row's spelling; an unknown row reads as the default.
    pub fn from_contract(value: &str) -> Self {
        match value {
            "plain" => HeaderForm::Plain,
            _ => HeaderForm::Mention,
        }
    }
}

/// The body's summary: its first sentence, cut to at most 12 words. A body
/// with no sentence end, or one that opens with a code fence or a bare URL,
/// takes its first line cut at 12 words; a body empty after trimming
/// summarizes as `(empty)`.
pub fn summary_of(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    // A backtick in the cut text renders as a single quote, so the summary
    // can never close the header's inline code early (AC6-HP).
    let first_sentence = first_sentence_of(trimmed).replace('`', "'");
    cut_words(&first_sentence, SUMMARY_MAX_WORDS)
}

#[derive(serde::Deserialize)]
pub struct HeldMessage {
    pub sender: String,
    pub sent_at: String,
    pub id: String,
    pub body: String,
}

#[derive(serde::Deserialize)]
pub struct HeldRelease {
    pub held_for_s: i64,
    pub harness: Option<String>,
    pub messages: Vec<HeldMessage>,
}

/// Render one held-mail delivery with the original message identities intact.
/// The framing line describes the delay; every following header belongs to
/// the sender and id of one message from the bus.
pub fn render_held_release(release: &HeldRelease) -> String {
    let mut messages: Vec<&HeldMessage> = release.messages.iter().collect();
    messages.sort_by(|left, right| {
        match (parse_sent_at(&left.sent_at), parse_sent_at(&right.sent_at)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    let sent: Vec<String> = messages
        .iter()
        .map(|message| local_sent_time(&message.sent_at))
        .collect();
    let sent_range = match (sent.first(), sent.last()) {
        (Some(first), Some(last)) if first != last => format!("{first} to {last}"),
        (Some(first), _) => first.clone(),
        _ => "unknown".to_string(),
    };
    let minutes = if release.held_for_s > 0 {
        (release.held_for_s + 59) / 60
    } else {
        0
    };
    let count = messages.len();
    let form = release
        .harness
        .as_deref()
        .and_then(crate::harness_capabilities::packaged_mail_header_at)
        .map(|at| {
            if at {
                HeaderForm::Mention
            } else {
                HeaderForm::Plain
            }
        })
        .unwrap_or(HeaderForm::Mention);
    let mut lines = vec![format!(
        "{count} held messages · sent {sent_range} · held {minutes}m"
    )];
    for message in messages {
        let body = unwrap_held_body(&message.body);
        let (body, existing_header) = strip_leading_header(&body);
        let summary = existing_header
            .as_deref()
            .and_then(header_summary)
            .unwrap_or_else(|| summary_of(&body));
        let body = strip_summary_prefix(&body, &summary);
        lines.push(render_header(
            form,
            crate::system_sender::canonical(&message.sender),
            &message.id,
            &summary,
        ));
        lines.push(body);
    }
    lines.join("\n")
}

fn local_sent_time(value: &str) -> String {
    parse_sent_at(value)
        .map(|stamp| {
            stamp
                .with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn parse_sent_at(value: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(value).ok()
}

fn unwrap_held_body(body: &str) -> String {
    let trimmed = body.trim();
    let Some(block) = paired_envelope_block(trimmed) else {
        return body.to_string();
    };
    if block != trimmed {
        return body.to_string();
    }
    let Some(open_end) = block.find('>') else {
        return body.to_string();
    };
    block[open_end + 1..block.len() - "</fno_mail>".len()].to_string()
}

fn strip_leading_header(body: &str) -> (String, Option<String>) {
    let Some((header, rest)) = body.split_once('\n') else {
        return (body.to_string(), None);
    };
    if !is_header_line(header) {
        return (body.to_string(), None);
    }
    (rest.to_string(), Some(header.to_string()))
}

fn header_summary(header: &str) -> Option<String> {
    let (inner, _) = split_header_span(header.trim())?;
    let (_, _, summary) = header_fields(inner)?;
    Some(cut_words(summary, SUMMARY_MAX_WORDS).replace('`', "'"))
}

fn strip_summary_prefix(body: &str, summary: &str) -> String {
    let leading_len = body.len() - body.trim_start().len();
    let (leading, content) = body.split_at(leading_len);
    let Some(rest) = content.strip_prefix(summary) else {
        return body.to_string();
    };
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .or_else(|| rest.strip_prefix(' '))
        .unwrap_or(rest);
    format!("{leading}{rest}")
}

/// The first sentence: up to the first `.`, `!` or `?` that ends a word
/// (so `e.g.` mid-line does not end one), else the first line.
fn first_sentence_of(text: &str) -> String {
    let head = text.lines().next().unwrap_or(text);
    let mut end = head.len();
    for (idx, ch) in head.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let boundary = head[idx + ch.len_utf8()..]
                .chars()
                .next()
                .map(|next| next.is_whitespace() || next == '`')
                .unwrap_or(true);
            // A single letter before the period (an initial, or the g in
            // e.g.) means the period is not a sentence end.
            let word_len = head[..idx]
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric())
                .count();
            if boundary && word_len != 1 {
                end = idx + ch.len_utf8();
                break;
            }
        }
    }
    head[..end].trim().to_string()
}

/// The first `max` words, words split on whitespace.
fn cut_words(text: &str, max: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max {
        return text.trim().to_string();
    }
    words[..max].join(" ")
}

/// The rendered header line, backticks included, no trailing newline.
pub fn render_header(form: HeaderForm, sender: &str, msg_id: &str, summary: &str) -> String {
    let who = match form {
        HeaderForm::Mention => format!("@{sender}"),
        HeaderForm::Plain => sender.to_string(),
    };
    format!("`{who} · {msg_id} · {summary}`")
}

/// The header's inner span and what follows it: the opening backtick through
/// the first closing backtick whose remainder is the line's end or the
/// transcript's one-line body separator " ⏎ " (a turn renders on ONE
/// physical line, header then separator then body). `None` when no closer
/// reads.
fn split_header_span(trimmed: &str) -> Option<(&str, &str)> {
    let rest = trimmed.strip_prefix('`')?;
    let mut from = 0;
    while let Some(rel) = rest[from..].find('`') {
        let close = from + rel;
        let tail = &rest[close + 1..];
        if tail.is_empty() || tail.starts_with(crate::mail_inject::NEWLINE_GLYPH) {
            return Some((&rest[..close], tail));
        }
        from = close + 1;
    }
    None
}

/// The span's three fields: sender, id, summary. The summary may itself
/// carry a backtick or a " · " separator, so it is everything after the
/// second separator, not the third split field.
fn header_fields(inner: &str) -> Option<(&str, &str, &str)> {
    let mut parts = inner.splitn(3, " · ");
    Some((parts.next()?, parts.next()?, parts.next()?))
}

/// True when the whole line reads as a delivered-mail header: one backticked
/// span of exactly `sender · id · summary`, the sender `@name` or `name` with
/// no spaces, the middle id `fmail-` plus 12 hex (the message-id form the
/// mux-messages group rules on) or a legacy `msg-…` token that still
/// resolves. The span closes at the line's end or before the " ⏎ " body
/// separator; the summary may carry a backtick or " · ". Both header forms
/// match; this is the reader's shape test and the forged-body detector.
pub fn is_header_line(line: &str) -> bool {
    let trimmed = line.trim();
    let Some((inner, _tail)) = split_header_span(trimmed) else {
        return false;
    };
    let Some((sender, id, summary)) = header_fields(inner) else {
        return false;
    };
    let bare = sender.strip_prefix('@').unwrap_or(sender);
    if bare.is_empty() || bare.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let id_ok = match id.strip_prefix("fmail-") {
        Some(hex) => hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => id.strip_prefix("msg-").is_some_and(|rest| !rest.is_empty()),
    };
    id_ok && !summary.trim().is_empty()
}

/// True when any line of `body` is shaped like a delivered header. A send
/// carrying one refuses: a body cannot forge a second message's first line.
pub fn body_holds_header_line(body: &str) -> bool {
    body.lines().any(is_header_line)
}

/// How a delivered turn is framed. `Header` is the new first line, `LegacyTag`
/// the `<fno_mail ...>` open this design retires (old transcripts keep it),
/// `CrossSession` the ask-lane relay framing that never changes. `Bare` is
/// everything else - an operator turn, a plain command, transcript prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Header,
    LegacyTag,
    CrossSession,
    Bare,
}

/// The held-mail release turn's framing line: `3 held messages · sent 17:24
/// to 18:23 · held 9m` - no `@`, no backticks, never a header itself. Each
/// held message follows under its own header line, oldest first by sent
/// time (the locked held-mail ruling).
pub fn is_held_release_line(line: &str) -> bool {
    held_release_count(line).is_some()
}

fn held_release_count(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    let rest = trimmed.strip_suffix('m')?;
    let mut parts = rest.split(" · ");
    let (head, sent, held) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let held_ok = held
        .strip_prefix("held ")
        .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_digit()));
    let (count, tail) = head.split_once(" held messages")?;
    if !held_ok
        || !sent.starts_with("sent ")
        || !tail.is_empty()
        || count.is_empty()
        || !count.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    count.parse().ok().filter(|count| *count > 0)
}

/// True when a complete held-release turn carries the declared number of
/// original message headers, with each body following its own header.
pub fn is_held_release_turn(text: &str) -> bool {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return false;
    };
    let Some(expected) = held_release_count(first) else {
        return false;
    };
    let rest: Vec<&str> = lines.collect();
    let mut index = 0;
    for _ in 0..expected {
        if !rest.get(index).is_some_and(|line| is_header_line(line)) {
            return false;
        }
        index += 1;
        while index < rest.len() && !is_header_line(rest[index]) {
            if is_held_release_line(rest[index]) {
                return false;
            }
            index += 1;
        }
    }
    index == rest.len()
}

/// Classify a delivered turn's framing from its head. The one classifier the
/// Rust readers call directly and the Python readers reach through
/// `mail-envelope --classify`, so no second shape test exists anywhere. A
/// held release turn is Header-framed: its first line is the framing line,
/// and every message under it carries a real header.
pub fn classify(text: &str) -> Framing {
    let head = text.trim_start();
    if opens_tag(head, "<cross-session-message") {
        return Framing::CrossSession;
    }
    if opens_tag(head, "<fno_mail") {
        return Framing::LegacyTag;
    }
    if head.lines().next().is_some_and(is_header_line) {
        return Framing::Header;
    }
    if head.lines().next().is_some_and(is_held_release_line) {
        return Framing::Header;
    }
    Framing::Bare
}

/// True when `head` starts with `tag` at a delimiter (space, `>`, newline,
/// end), case-insensitive - the same boundary rule the injection door's
/// framing test uses, so a `<fno_mailicious` prefix never reads as a tag.
fn opens_tag(head: &str, tag: &str) -> bool {
    let head_lower = head.to_lowercase();
    let tag_lower = tag.to_lowercase();
    match head_lower.strip_prefix(tag_lower.as_str()) {
        Some(rest) => matches!(
            rest.chars().next(),
            Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>') | None
        ),
        None => false,
    }
}

/// The msg id a delivered turn carries, whatever its framing: the header's
/// middle token, or the legacy tag's `id="…"` attribute. Reply resolution
/// and drain dedup join on this; `None` when the turn names no id.
pub fn delivered_msg_id(text: &str) -> Option<String> {
    let head = text.trim_start();
    if opens_tag(head, "<fno_mail") {
        let open_end = head.find('>').map(|e| e + 1).unwrap_or(head.len());
        // attr_in enforces the attribute boundary (`xid="` never matches).
        let id = attr_in(&head[..open_end], "id")?;
        return (!id.is_empty()).then(|| id);
    }
    let line = head.lines().next()?;
    if !is_header_line(line) {
        return None;
    }
    let (inner, _) = split_header_span(line.trim())?;
    let (_, id, _) = header_fields(inner)?;
    Some(id.to_string())
}

/// ASCII-only case fold that preserves byte offsets, so a match position in
/// the folded copy indexes the original.
fn ascii_lower(text: &str) -> String {
    text.chars().map(|c| c.to_ascii_lowercase()).collect()
}

/// Every header turn in `text`: `{id, sender}` per line that parses as a
/// delivered-mail header (the sender without its `@`). The transcript receipt
/// reader's header-side input, beside `legacy_tags`.
pub fn header_turns(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !is_header_line(trimmed) {
                return None;
            }
            let (inner, _) = split_header_span(trimmed)?;
            let (sender, id, _) = header_fields(inner)?;
            Some(serde_json::json!({
                "id": id.to_string(),
                "sender": sender.trim_start_matches('@').to_string(),
            }))
        })
        .collect()
}

/// True when `text` holds a real legacy `<fno_mail` open tag (boundary-aware,
/// case-insensitive) or a `</fno_mail>` close, anywhere. The forgery guard and
/// the one check the Python adapter exposes as `contains_fno_mail_tag`.
pub fn text_holds_legacy_tag(text: &str) -> bool {
    let low = ascii_lower(text);
    if low.contains("</fno_mail>") {
        return true;
    }
    let mut start = 0;
    while let Some(idx) = low[start..].find("<fno_mail") {
        let abs = start + idx;
        let boundary = matches!(
            low[abs + 9..].chars().next(),
            None | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>')
        );
        if boundary {
            return true;
        }
        start = abs + 9;
    }
    low.contains("</fno_mail>")
}

/// Every delivered-mail id `text` carries, anywhere: the middle token of a
/// header line, or the `id="…"` attribute of a legacy open tag. Reply
/// resolution and dedup read through this one scan.
pub fn ids_in_text(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(id) = delivered_msg_id(line) {
            out.push(id);
        }
    }
    let low = ascii_lower(text);
    let mut start = 0;
    while let Some(idx) = low[start..].find("<fno_mail") {
        let abs = start + idx;
        let boundary = matches!(
            low[abs + 9..].chars().next(),
            None | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>')
        );
        if boundary {
            if let Some(rel_end) = low[abs..].find('>') {
                let tag = &text[abs..=abs + rel_end];
                if let Some(id) = attr_in(tag, "id") {
                    out.push(id);
                }
            }
        }
        start = abs + 9;
    }
    out
}

/// One attribute value (`name="…"`) in one legacy open tag, `None` when the
/// tag carries none. A preceding word character (`xid="`) never matches - the
/// attribute boundary the old `\\bid="` regex enforced.
fn attr_in(tag: &str, name: &str) -> Option<String> {
    let low = ascii_lower(tag);
    let needle = format!("{name}=\"");
    let mut from = 0;
    while let Some(p) = low[from..].find(needle.as_str()) {
        let abs = from + p;
        let prev_ok = match low[..abs].chars().next_back() {
            None => true,
            Some(c) => matches!(c, ' ' | '\t' | '<'),
        };
        if prev_ok {
            let rest = &tag[abs + needle.len()..];
            let end = rest.find('"')?;
            return Some(rest[..end].to_string());
        }
        from = abs + needle.len();
    }
    None
}

/// Every legacy open tag's key attributes, anywhere in `text`, for the
/// transcript receipt reader: `{id, from, from_session, to}` per real open
/// tag (`null` when an attribute is absent; the id may be empty).
pub fn legacy_tags(text: &str) -> Vec<Value> {
    let low = ascii_lower(text);
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(idx) = low[start..].find("<fno_mail") {
        let abs = start + idx;
        let boundary = matches!(
            low[abs + 9..].chars().next(),
            None | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>')
        );
        if boundary {
            if let Some(rel_end) = low[abs..].find('>') {
                let tag = &text[abs..=abs + rel_end];
                out.push(serde_json::json!({
                    "id": attr_in(tag, "id").unwrap_or_default(),
                    "from": attr_in(tag, "from"),
                    "from_session": attr_in(tag, "from_session"),
                    "to": attr_in(tag, "to"),
                }));
            }
        }
        start = abs + 9;
    }
    out
}

/// The paired legacy envelope block `<fno_mail …>…</fno_mail>` when the open
/// tag carries an `id` attribute - the drain-dedup key's input. `None` with no
/// paired block or no id: a pre-redesign producer is un-dedupable.
pub fn paired_envelope_block(text: &str) -> Option<String> {
    let low = ascii_lower(text);
    let open = {
        let mut start = 0;
        loop {
            let idx = low[start..].find("<fno_mail")?;
            let abs = start + idx;
            let boundary = matches!(
                low[abs + 9..].chars().next(),
                None | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>')
            );
            if boundary {
                break abs;
            }
            start = abs + 9;
        }
    };
    let open_end = open + low[open..].find('>')?;
    attr_in(&text[open..=open_end], "id")?;
    let close = open_end + low[open_end..].find("</fno_mail>")?;
    Some(text[open..close + "</fno_mail>".len()].to_string())
}

/// The single-line relay wire form: `<fno_mail from="…" …> body` on ONE line,
/// no close tag - the ask-lane hop's shape. Returns `(from_session, body)`;
/// `from=` must be the first attribute, at most one space separates it from
/// the body.
pub fn relay_parse_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    let low = ascii_lower(line);
    let rest = low.strip_prefix("<fno_mail")?;
    let ws = rest.len() - rest.trim_start().len();
    if ws == 0 {
        return None;
    }
    if !rest[ws..].starts_with("from=\"") {
        return None;
    }
    let open_end = low.find('>')?;
    let from_rest = &line[9 + ws + 6..open_end];
    let from_end = from_rest.find('"')?;
    let body = line[open_end + 1..]
        .strip_prefix([' ', '\t'])
        .unwrap_or(&line[open_end + 1..]);
    Some((&from_rest[..from_end], body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_headers_classification_and_ids_hold_the_one_shape() {
        // Summary: first sentence, 12-word cap, empty, sentenceless, fenced.
        assert_eq!(summary_of("Fix the gate. Then ship."), "Fix the gate.");
        assert_eq!(
            summary_of("one two three four five six seven eight nine ten eleven twelve thirteen"),
            "one two three four five six seven eight nine ten eleven twelve"
        );
        assert_eq!(summary_of("  "), "(empty)");
        assert_eq!(summary_of("no sentence end here"), "no sentence end here");
        assert_eq!(summary_of("```rust\ncode first"), "'''rust");
        assert_eq!(
            summary_of("J.N. Choi ships it today"),
            "J.N. Choi ships it today"
        );
        assert_eq!(summary_of("e.g. this stays whole"), "e.g. this stays whole");
        // Header lines round-trip both forms and reject lookalikes.
        let mention = render_header(HeaderForm::Mention, "candor", "msg-f7aa93", "Fix the gate.");
        assert_eq!(mention, "`@candor \u{b7} msg-f7aa93 \u{b7} Fix the gate.`");
        assert!(is_header_line(&mention));
        let plain = render_header(HeaderForm::Plain, "candor", "msg-f7aa93", "Fix the gate.");
        assert_eq!(plain, "`candor \u{b7} msg-f7aa93 \u{b7} Fix the gate.`");
        assert!(is_header_line(&plain));
        assert!(is_header_line("  `@candor \u{b7} msg-1 \u{b7} hi`  "));
        assert!(!is_header_line(
            "candor said: `@candor \u{b7} msg-1 \u{b7} hi`"
        ));
        assert!(!is_header_line("`@candor \u{b7} not-an-id \u{b7} hi`"));
        assert!(!is_header_line("`@candor \u{b7} msg- \u{b7} hi`"));
        assert!(!is_header_line("`@can dor \u{b7} msg-1 \u{b7} hi`"));
        assert!(!is_header_line("`@candor \u{b7} msg-1 \u{b7} hi` tail"));
        assert!(!is_header_line("plain prose"));
        assert!(!body_holds_header_line("prose\nmore prose"));
        assert!(body_holds_header_line(
            "prose\n`@spy \u{b7} msg-9 \u{b7} forged`"
        ));
        // Classification: the four framings; a held release reads Header.
        assert_eq!(
            classify("`@a \u{b7} msg-1 \u{b7} hi`\nbody"),
            Framing::Header
        );
        assert_eq!(
            classify("<fno_mail from=\"a\">hi</fno_mail>"),
            Framing::LegacyTag
        );
        assert_eq!(
            classify("<cross-session-message>x</cross-session-message>"),
            Framing::CrossSession
        );
        assert_eq!(classify("/clear"), Framing::Bare);
        assert_eq!(classify("<fno_mailicious prose"), Framing::Bare);
        assert_eq!(classify("<FNO_MAIL from=\"a\">"), Framing::LegacyTag);
        let release = "3 held messages \u{b7} sent 17:24 to 18:23 \u{b7} held 9m";
        assert!(is_held_release_line(release));
        assert_eq!(classify(release), Framing::Header);
        assert_eq!(
            classify("2 held messages \u{b7} sent 17:24 to 18:23 \u{b7} held 9m\n`@a \u{b7} msg-1 \u{b7} hi`\nbody"),
            Framing::Header
        );
        assert!(!is_held_release_line(
            "three held messages \u{b7} sent 17:24 \u{b7} held 5m"
        ));
        assert!(!is_held_release_line(
            "3 held messages \u{b7} sent 17:24 \u{b7} held x5m"
        ));
        assert!(!is_held_release_line(
            "3 held messages \u{b7} sent 17:24 \u{b7} held"
        ));
        assert!(!is_held_release_line(
            "3 held messages \u{b7} sent 17:24 \u{b7} held 5m\nprose"
        ));
        assert!(!is_held_release_line(
            "3 held messages \u{b7} sent 17:24 \u{b7} held 5m \u{b7} tail"
        ));
        // The canonical id form: `fmail-` plus 12 hex; wrong length or
        // non-hex never reads as a header. Legacy `msg-…` still resolves.
        let fmail = render_header(
            HeaderForm::Mention,
            "candor",
            "fmail-0badc0de1234",
            "Fix the gate.",
        );
        assert!(is_header_line(&fmail));
        assert_eq!(classify(&fmail), Framing::Header);
        assert!(!is_header_line(
            "`@candor \u{b7} fmail-0badc0de123 \u{b7} hi`"
        ));
        assert!(!is_header_line(
            "`@candor \u{b7} fmail-0badc0de12345 \u{b7} hi`"
        ));
        assert!(!is_header_line(
            "`@candor \u{b7} fmail-zzzzzzzzzzzz \u{b7} hi`"
        ));
        assert_eq!(
            delivered_msg_id(&fmail),
            Some("fmail-0badc0de1234".to_string())
        );
        // The id a reply or dedup joins on, both shapes.
        assert_eq!(
            delivered_msg_id("`@a \u{b7} msg-f7aa93 \u{b7} hi`\nbody"),
            Some("msg-f7aa93".to_string())
        );
        assert_eq!(
            delivered_msg_id("<fno_mail from=\"a\" id=\"msg-1\">\nhi\n</fno_mail>"),
            Some("msg-1".to_string())
        );
        assert_eq!(delivered_msg_id("no id here"), None);
        assert_eq!(delivered_msg_id(release), None);
        // The legacy id read enforces the attribute boundary: `valid_id="`
        // never stands in for `id="`.
        assert_eq!(
            delivered_msg_id("<fno_mail from=\"a\" valid_id=\"x\" id=\"msg-1\">hi</fno_mail>"),
            Some("msg-1".to_string())
        );
        // The adapter's other read facts, per text.
        assert!(text_holds_legacy_tag(
            "prose <fno_mail from=\"a\">x</fno_mail>"
        ));
        assert!(!text_holds_legacy_tag("<fno_mailicious prose"));
        assert!(!text_holds_legacy_tag("<FNO_MAILBOX>"));
        assert!(text_holds_legacy_tag("body</fno_mail>"));
        assert_eq!(
            ids_in_text("prose\n`@a \u{b7} fmail-0badc0de1234 \u{b7} hi`\n<fake>x"),
            vec!["fmail-0badc0de1234"]
        );
        assert_eq!(
            ids_in_text("mid <fno_mail from=\"a\" id=\"msg-77\">x</fno_mail> tail"),
            vec!["msg-77"]
        );
        assert_eq!(ids_in_text("no ids"), Vec::<String>::new());
        assert_eq!(
            paired_envelope_block("prose <fno_mail from=\"a\" id=\"msg-1\">hi</fno_mail> tail"),
            Some("<fno_mail from=\"a\" id=\"msg-1\">hi</fno_mail>".to_string())
        );
        assert_eq!(paired_envelope_block("no close <fno_mail id=\"x\">"), None);
        assert_eq!(
            paired_envelope_block("<fno_mail from=\"a\">no id</fno_mail>"),
            None
        );
        assert_eq!(
            relay_parse_line("<fno_mail from=\"s1\" harness=\"codex\"> hop body"),
            Some(("s1", "hop body"))
        );
        assert_eq!(
            relay_parse_line("<fno_mail from=\"s1\">body"),
            Some(("s1", "body"))
        );
        assert_eq!(relay_parse_line("<fno_mailbox from=\"s\">x"), None);
        assert_eq!(relay_parse_line("<fno_mail id=\"x\">body"), None);
        assert_eq!(relay_parse_line("plain"), None);
        // Legacy-tag attribute reads for the receipt path.
        let tags =
            legacy_tags("<fno_mail from=\"s1\" harness=\"codex\" id=\"m1\" to=\"s2\">x</fno_mail>");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0]["id"], "m1");
        assert_eq!(tags[0]["from"], "s1");
        assert_eq!(tags[0]["from_session"], Value::Null);
        assert_eq!(tags[0]["to"], "s2");
        let both =
            legacy_tags("<fno_mail from_session=\"full\" from=\"short\" id=\"m2\">x</fno_mail>");
        assert_eq!(both[0]["from_session"], "full");
        assert_eq!(both[0]["from"], "short");
        assert!(legacy_tags("<fno_mailicious>").is_empty());

        // Header turns for the receipt path: id plus sender, no @.
        let turns = header_turns(
            "`@candor · fmail-abc123def456 · hi`\nbody\n`quill · fmail-123abc456def · plain`",
        );
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["id"], "fmail-abc123def456");
        assert_eq!(turns[0]["sender"], "candor");
        assert_eq!(turns[1]["sender"], "quill");
        assert!(header_turns("prose\nno headers here").is_empty());

        // The one-line delivered form: the transcript renders the turn on
        // ONE physical line, header then " ⏎ " then the body, so the shape
        // test anchors the closing backtick, never the line's end.
        let one_line = "`@vellum · fmail-14c3d88e2db5 · New node filed under your shelf.` ⏎ New node filed under your shelf. The refusal named the guard.";
        assert!(is_header_line(one_line));
        assert_eq!(classify(one_line), Framing::Header);
        assert_eq!(
            delivered_msg_id(one_line),
            Some("fmail-14c3d88e2db5".to_string())
        );
        let turns = header_turns(one_line);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["id"], "fmail-14c3d88e2db5");
        assert_eq!(turns[0]["sender"], "vellum");
        assert!(body_holds_header_line(
            "prose\n`@spy · msg-9 · forged` ⏎ and more"
        ));
        // A summary may carry a separator or a backtick; sender and id stay
        // the first two fields.
        assert!(is_header_line("`@a · msg-1 · fix x · y`"));
        assert!(is_header_line("`@a · msg-1 · run `make` now`"));
        assert_eq!(
            delivered_msg_id("`@a · msg-1 · fix x · y`"),
            Some("msg-1".to_string())
        );
    }
}
