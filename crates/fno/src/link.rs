//! URL detection and open-safety for clickable links in mux panes (x-a2d0).
//!
//! Two sources feed the same model. An app can declare a link explicitly with
//! OSC 8, which `alacritty_terminal` already parses into `Cell::hyperlink()`;
//! everything else is plain text, which is the common case (a printed PR URL
//! from `gh`, a session link from a coding agent). [`find_urls`] covers the
//! second. Detection is pure and unit-tested; the grid walk lives in
//! [`crate::vt::Pane::link_at`] and [`open_url`] is the one impure function
//! here, kept in this file so the scheme allowlist and the call that acts on it
//! cannot drift apart.

/// Schemes we will hand to the platform opener. Deliberately just these two.
///
/// This is a trust boundary, not a nicety: an OSC 8 URI is arbitrary bytes
/// chosen by whatever is running in the pane, and `open(1)` on macOS will
/// happily launch an application, mount a URL handler, or reveal a `file://`
/// path. A pane rendering hostile output must not be able to turn a click into
/// anything but a web navigation, so the allowlist is checked on both sides of
/// the wire (server before send, client before exec).
const OPENABLE_SCHEMES: [&str; 2] = ["http://", "https://"];

/// Characters that may appear inside a URL. RFC 3986's unreserved + reserved
/// sets plus `%`; everything else (whitespace, controls, `<>"{}|\^` and the
/// backtick) terminates the match. Quotes and angle brackets are excluded on
/// purpose - they are how prose and markup wrap a URL, never part of one.
fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Trailing characters trimmed off a detected URL. A URL at the end of a
/// sentence ("opened https://example.com/pr/700.") is the normal case, and
/// `is_url_char` accepts all of these mid-URL, so the scan has to give them
/// back at the tail. `)` is handled separately - see [`trim_tail`].
const TRAILING_PUNCT: [char; 8] = ['.', ',', ';', ':', '!', '?', '\'', '"'];

/// The URL's length in CHARS after trailing punctuation is given back.
///
/// Closing brackets are only trimmed when unbalanced, so a Wikipedia-style
/// `https://en.wikipedia.org/wiki/Foo_(bar)` keeps its `)` while a
/// parenthesised `(see https://example.com/x)` does not eat the closer.
fn trim_tail(chars: &[char]) -> usize {
    let mut end = chars.len();
    while end > 0 {
        let c = chars[end - 1];
        if TRAILING_PUNCT.contains(&c) {
            end -= 1;
            continue;
        }
        if matches!(c, ')' | ']' | '}') {
            let open = match c {
                ')' => '(',
                ']' => '[',
                _ => '{',
            };
            let opens = chars[..end].iter().filter(|&&x| x == open).count();
            let closes = chars[..end].iter().filter(|&&x| x == c).count();
            if closes > opens {
                end -= 1;
                continue;
            }
        }
        break;
    }
    end
}

/// Every plain-text URL in `text`, as half-open CHAR index ranges.
///
/// Char indices rather than byte indices because the caller maps each range
/// back to a grid cell, and cells are chars. Ranges never overlap and are
/// returned in order.
pub fn find_urls(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let Some(scheme_len) = scheme_at(&chars, i) else {
            i += 1;
            continue;
        };
        let mut end = i + scheme_len;
        while end < chars.len() && is_url_char(chars[end]) {
            end += 1;
        }
        // Require at least one host character after the scheme, so a bare
        // "https://" in prose is not offered as a link.
        let end = i + trim_tail(&chars[i..end]);
        if end > i + scheme_len {
            out.push((i, end));
            i = end;
        } else {
            i += scheme_len;
        }
    }
    out
}

/// The length of an openable scheme starting at `i`, if one starts there.
/// Case-insensitive: `HTTPS://` is a URL a terminal can legitimately print.
fn scheme_at(chars: &[char], i: usize) -> Option<usize> {
    OPENABLE_SCHEMES.iter().find_map(|scheme| {
        let n = scheme.chars().count();
        let matches = chars.len() >= i + n
            && chars[i..i + n]
                .iter()
                .zip(scheme.chars())
                .all(|(c, s)| c.to_ascii_lowercase() == s);
        matches.then_some(n)
    })
}

/// Whether `url` may be handed to the platform opener.
///
/// Rejects anything outside [`OPENABLE_SCHEMES`], anything carrying whitespace
/// or a control character (an argv element with a newline is how a "URL"
/// becomes two arguments to something downstream), and anything absurdly long.
pub fn is_openable(url: &str) -> bool {
    const MAX_URL_LEN: usize = 4096;
    if url.len() > MAX_URL_LEN {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    if !OPENABLE_SCHEMES.iter().any(|s| lower.starts_with(s)) {
        return false;
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    // A scheme with no host is not navigable and is the shape a truncated or
    // synthesised URI takes.
    OPENABLE_SCHEMES
        .iter()
        .any(|s| lower.starts_with(s) && url.len() > s.len())
}

/// A URL shortened for a one-line notice, so a 4000-character link cannot
/// scroll the status line off the screen.
pub fn for_notice(url: &str) -> String {
    const MAX: usize = 60;
    if url.chars().count() <= MAX {
        return url.to_string();
    }
    let head: String = url.chars().take(MAX - 1).collect();
    format!("{head}…")
}

/// Hand `url` to the platform opener, blocking until it exits. `Err` carries
/// one human line for the status notice.
///
/// Re-checks [`is_openable`] even though the server filtered before sending:
/// this is the call that actually launches something, and a check at the point
/// of action is the one that cannot be bypassed by a future caller. The URL is
/// passed as a single argv element, never through a shell, and `is_openable`
/// having required an `http(s)://` prefix is also what stops it being read as
/// an option by the opener.
pub fn open_url(url: &str) -> Result<(), String> {
    open_url_with(spawn_opener, url)
}

/// The opener name for this platform. Named separately so the test that
/// asserts the argv can name the same binary the real path would use.
fn opener_bin() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// [`open_url`] with the exec injected, mirroring `clipboard::deliver_with`:
/// the refusal is testable without a process, and the ACCEPT path is testable
/// without launching a browser on whatever machine runs the suite. The seam
/// exists so the argv - the part that actually matters for safety - is asserted
/// rather than assumed.
fn open_url_with<S>(spawn: S, url: &str) -> Result<(), String>
where
    S: FnOnce(&str, &str) -> Result<std::process::ExitStatus, std::io::Error>,
{
    if !is_openable(url) {
        return Err(format!("refused to open {}", for_notice(url)));
    }
    let opener = opener_bin();
    match spawn(opener, url) {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{opener} exited {}", s.code().unwrap_or(-1))),
        Err(e) => Err(format!("{opener}: {e}")),
    }
}

/// The real exec. One argv element for the URL, no shell, stdio detached so a
/// chatty `xdg-open` cannot scribble over the client's alternate screen.
fn spawn_opener(opener: &str, url: &str) -> Result<std::process::ExitStatus, std::io::Error> {
    use std::process::{Command, Stdio};
    Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        find_urls(text)
            .into_iter()
            .map(|(a, b)| chars[a..b].iter().collect())
            .collect()
    }

    #[test]
    fn finds_a_bare_url() {
        assert_eq!(
            urls("see https://github.com/bllshttng/footnote/pull/700 now"),
            ["https://github.com/bllshttng/footnote/pull/700"]
        );
    }

    #[test]
    fn gives_back_sentence_punctuation() {
        // The exact shape a PR link takes at the end of a line of prose.
        assert_eq!(
            urls("opened https://example.com/pr/700."),
            ["https://example.com/pr/700"]
        );
        assert_eq!(urls("https://example.com/a,"), ["https://example.com/a"]);
        assert_eq!(urls("<https://example.com/a>"), ["https://example.com/a"]);
    }

    #[test]
    fn keeps_a_balanced_closing_paren_but_not_a_wrapping_one() {
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Foo_(bar)"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        assert_eq!(
            urls("(see https://example.com/x)"),
            ["https://example.com/x"]
        );
    }

    #[test]
    fn finds_several_and_reports_char_ranges() {
        let text = "a https://x.test b http://y.test c";
        assert_eq!(urls(text), ["https://x.test", "http://y.test"]);
        let ranges = find_urls(text);
        assert_eq!(ranges[0], (2, 16));
        assert_eq!(&text[2..16], "https://x.test");
    }

    #[test]
    fn char_ranges_are_char_indices_not_byte_indices() {
        // A multi-byte prefix makes the two disagree; the grid walk indexes
        // cells, so char indices are the contract.
        let text = "→ https://example.com";
        let (start, end) = find_urls(text)[0];
        assert_eq!(start, 2, "one arrow char + one space");
        let chars: Vec<char> = text.chars().collect();
        assert_eq!(
            chars[start..end].iter().collect::<String>(),
            "https://example.com"
        );
    }

    #[test]
    fn ignores_a_scheme_with_no_host() {
        assert!(urls("bare https:// nothing").is_empty());
    }

    #[test]
    fn uppercase_scheme_is_still_a_url() {
        assert_eq!(urls("HTTPS://Example.COM/a"), ["HTTPS://Example.COM/a"]);
    }

    #[test]
    fn non_web_schemes_are_never_detected() {
        for text in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com",
            "mailto:a@b.test",
        ] {
            assert!(urls(text).is_empty(), "{text} must not linkify");
        }
    }

    #[test]
    fn is_openable_rejects_everything_outside_the_allowlist() {
        assert!(is_openable("https://example.com"));
        assert!(is_openable("http://example.com/a?b=c#d"));
        assert!(is_openable("HTTPS://EXAMPLE.COM"));

        // The OSC 8 attack surface: a pane chooses these bytes.
        assert!(!is_openable("file:///etc/passwd"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable("x-man-page://ls"));
        assert!(!is_openable(""));
        assert!(!is_openable("https://"), "scheme with no host");
        assert!(!is_openable("https://a.test/x\ny"), "embedded newline");
        assert!(!is_openable("https://a.test/ x"), "embedded space");
        assert!(!is_openable(&format!(
            "https://a.test/{}",
            "x".repeat(5000)
        )));
    }

    #[test]
    fn open_url_refuses_before_spawning_anything() {
        // The refusal arm is the security-relevant half and needs no process.
        // A pass would exec a browser, so only the refusal is asserted here.
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "", "https://"] {
            let err = open_url(bad).expect_err("must refuse");
            assert!(err.starts_with("refused to open"), "{bad} -> {err}");
        }
    }

    /// An exit status without launching anything: `true`/`false` are the two
    /// smallest real processes on every platform this runs on.
    fn status(ok: bool) -> std::process::ExitStatus {
        std::process::Command::new(if ok { "true" } else { "false" })
            .status()
            .unwrap()
    }

    #[test]
    fn open_url_passes_the_url_as_one_argv_element_to_the_platform_opener() {
        // The safety-relevant assertion: the URL reaches the opener whole, as a
        // single argument, with no shell in between. Injected rather than
        // exec'd for real so the suite never opens a browser.
        let seen = std::cell::RefCell::new(None);
        let out = open_url_with(
            |opener, url| {
                *seen.borrow_mut() = Some((opener.to_string(), url.to_string()));
                Ok(status(true))
            },
            "https://example.com/a b".replace(' ', "%20").as_str(),
        );
        assert!(out.is_ok(), "{out:?}");
        let (opener, url) = seen.into_inner().expect("opener was invoked");
        assert_eq!(opener, opener_bin());
        assert_eq!(url, "https://example.com/a%20b", "URL passed whole");
    }

    #[test]
    fn open_url_never_spawns_for_a_refused_url() {
        let mut spawned = false;
        let out = open_url_with(
            |_, _| {
                spawned = true;
                Ok(status(true))
            },
            "file:///etc/passwd",
        );
        assert!(out.is_err());
        assert!(!spawned, "a refused URL must not reach the opener at all");
    }

    #[test]
    fn open_url_reports_a_failing_opener() {
        let out = open_url_with(|_, _| Ok(status(false)), "https://example.com");
        assert!(out.unwrap_err().contains("exited"), "non-zero surfaces");
        let out = open_url_with(
            |_, _| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope")),
            "https://example.com",
        );
        assert!(out.unwrap_err().contains("nope"), "spawn error surfaces");
    }

    #[test]
    fn for_notice_bounds_the_status_line() {
        assert_eq!(for_notice("https://a.test"), "https://a.test");
        let long = format!("https://a.test/{}", "x".repeat(500));
        assert_eq!(for_notice(&long).chars().count(), 60);
        assert!(for_notice(&long).ends_with('…'));
    }
}
