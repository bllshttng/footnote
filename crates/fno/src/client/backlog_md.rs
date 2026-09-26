//! The detail pane's markdown renderer. Pure, line-oriented, no
//! dependency: one markdown doc in, styled `BLine`s out.

use super::backlog_board::trunc;
use super::backlog_style::BLine;
use super::node_detail::wrap_line;

pub(crate) fn md_lines(text: &str, w: usize, cap: usize) -> Vec<BLine> {
    let mut all = render(text, w);
    if all.len() > cap {
        let more = all.len() - cap;
        all.truncate(cap);
        all.push(BLine::meta(format!("\u{2026} {more} more lines")));
    }
    all
}

fn render(text: &str, w: usize) -> Vec<BLine> {
    let mut out: Vec<BLine> = Vec::new();
    let src: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    // Frontmatter: a leading `---` block folds to one meta line.
    if src.first().is_some_and(|l| l.trim_end() == "---") {
        i = 1;
        let mut n = 0usize;
        while i < src.len() && src[i].trim_end() != "---" {
            i += 1;
            n += 1;
        }
        if i < src.len() {
            i += 1;
        }
        out.push(BLine::meta(format!("frontmatter: {n} lines")));
    }
    let mut para: Vec<&str> = Vec::new();
    let mut fence = false;
    while i < src.len() {
        let line = src[i];
        i += 1;
        if fence {
            if line.trim_start().starts_with("```") {
                fence = false;
            } else {
                out.push(BLine::meta(trunc(line, w)));
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            flush(&mut para, &mut out, w);
            fence = true;
            continue;
        }
        // Headings: `#`..`######` followed by a space.
        let hash_run = line.chars().take_while(|&c| c == '#').count();
        let head_like = (1..=6).contains(&hash_run) && line[hash_run..].starts_with(' ');
        if head_like {
            flush(&mut para, &mut out, w);
            if !out.is_empty() {
                out.push(BLine::plain(String::new()));
            }
            out.push(BLine::head(inline(line[hash_run..].trim_start())));
            continue;
        }
        // Tables and blockquotes: dim, truncated, never wrapped.
        let t = line.trim_start();
        if t.starts_with('|') || t == ">" || t.starts_with("> ") {
            flush(&mut para, &mut out, w);
            if t.starts_with('|') {
                out.push(BLine::meta(trunc(line, w)));
            } else {
                out.push(BLine::meta(trunc(
                    &format!("\u{2502} {}", t.trim_start_matches('>')),
                    w,
                )));
            }
            continue;
        }
        // List items: `-`/`*`/`+` or `N.` after a leading indent.
        let indent = line.len() - line.trim_start().len();
        let rest = &line[indent..];
        let bullet = rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ");
        let digits = rest.chars().take_while(|&c| c.is_ascii_digit()).count();
        let numbered = digits > 0 && rest[digits..].starts_with(". ");
        if bullet || numbered {
            flush(&mut para, &mut out, w);
            let mark_len = if bullet { 2 } else { digits + 2 };
            let mark: String = rest.chars().take(mark_len).collect();
            let body_text = inline(&rest[mark_len..]);
            let wrap_w = w.saturating_sub(indent + 2).max(8);
            let mut wrapped: Vec<String> = Vec::new();
            wrap_line(&body_text, wrap_w, &mut wrapped);
            let marker = if bullet { "\u{2022}" } else { mark.trim_end() };
            for (li, l) in wrapped.into_iter().enumerate() {
                let padded = if li == 0 {
                    format!("{marker} {l}")
                } else {
                    format!("{}{}", " ".repeat(indent + 2), l)
                };
                out.push(BLine::plain(format!("{}{}", " ".repeat(indent), padded)));
            }
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut para, &mut out, w);
            continue;
        }
        para.push(line);
    }
    flush(&mut para, &mut out, w);
    out
}

/// Push the accumulated paragraph's wrapped lines: consecutive text
/// lines join into one paragraph, then wrap at `w`.
fn flush(para: &mut Vec<&str>, out: &mut Vec<BLine>, w: usize) {
    if para.is_empty() {
        return;
    }
    let joined = para.join(" ");
    let owned = inline(&joined);
    flush_body(&owned, out, w);
    para.clear();
}

/// Wrap one inline-stripped paragraph at `w` and push the plain lines.
fn flush_body(owned: &str, out: &mut Vec<BLine>, w: usize) {
    let mut wrapped: Vec<String> = Vec::new();
    wrap_line(owned, w, &mut wrapped);
    for l in wrapped {
        out.push(BLine::plain(l));
    }
}

/// The inline pass: strip `**`, `__`, backticks, and turn `[text](url)`
/// into `text`.
fn inline(s: &str) -> String {
    let no_links = strip_links(s);
    let mut s = no_links;
    for m in ["**", "__", "`"] {
        s = s.replace(m, "");
    }
    s
}

/// `[text](url)` -> `text`.
fn strip_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(close) = find_char(&chars, i + 1, ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = find_char(&chars, close + 2, ')') {
                        for &c in &chars[i + 1..close] {
                            out.push(c);
                        }
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == needle)
}

#[cfg(test)]
mod tests {
    use super::super::backlog_style::BRole;
    use super::*;

    const W: usize = 60;
    const CAP: usize = 400;

    #[test]
    fn empty_and_whitespace_render_zero_lines() {
        assert!(md_lines("", W, CAP).is_empty());
        assert!(md_lines("   \n  \n", W, CAP).is_empty());
    }

    #[test]
    fn frontmatter_folds_to_one_meta_line() {
        let doc = "---\nstatus: ready\nsize: L\n---\n\n# Plan\n\nbody text";
        let lines = md_lines(doc, W, CAP);
        assert_eq!(lines[0].text, "frontmatter: 2 lines");
        let heads = lines
            .iter()
            .filter(|l| l.default_role == BRole::Head)
            .count();
        assert_eq!(heads, 1, "one heading");
        assert!(lines.iter().any(|l| l.text == "Plan"));
        assert!(!lines.iter().any(|l| l.text.contains("status:")));
    }

    #[test]
    fn headings_strip_hashes_and_get_a_blank_line_before() {
        let doc = "intro text\n## Step one\nmore";
        let lines = md_lines(doc, W, CAP);
        let at = lines
            .iter()
            .position(|l| l.text == "Step one")
            .expect("heading line");
        assert_eq!(lines[at - 1].text, "");
        assert_eq!(lines[at].default_role, BRole::Head);
        assert!(!lines.iter().any(|l| l.text.contains("##")));
        assert!(lines.iter().any(|l| l.text == "more"));
    }

    #[test]
    fn fenced_code_drops_the_fences_and_dims_the_inner_lines() {
        let doc = "before\n```rust\nlet x = 1;\nlet y = 2;\n```\nafter";
        let lines = md_lines(doc, W, CAP);
        assert!(!lines.iter().any(|l| l.text.contains("```")));
        assert!(lines.iter().any(|l| l.text == "let x = 1;"));
        assert!(lines.iter().any(|l| l.text == "let y = 2;"));
        assert!(lines.iter().any(|l| l.text == "after"));
    }

    #[test]
    fn tables_and_blockquotes_render_dim_and_truncated() {
        let doc = "| col | col |\n|---|---|\n| a | b |\n> quoted words\nplain";
        let lines = md_lines(doc, W, CAP);
        let table = lines
            .iter()
            .find(|l| l.text.contains("| col | col |"))
            .expect("table row kept");
        assert!(table.roles.is_empty() && table.default_role == BRole::Meta);
        let quote = lines
            .iter()
            .find(|l| l.text.starts_with('\u{2502}'))
            .expect("bar");
        assert!(quote.text.contains("quoted words"));
    }

    #[test]
    fn bullets_keep_their_indent_and_wrap_with_a_hanging_indent() {
        let long = format!("- {}end", "a word ".repeat(30));
        let lines = md_lines(&long, 40, CAP);
        assert!(
            lines[0].text.starts_with("\u{2022} a word"),
            "first item line: {:?}",
            lines[0].text
        );
        for l in lines.iter().skip(1) {
            assert!(l.text.starts_with("  "), "hang: {:?}", l.text);
        }
    }

    #[test]
    fn numbered_items_keep_their_numbers() {
        let doc = "3. third thing\n4. fourth";
        let lines = md_lines(doc, W, CAP);
        assert_eq!(lines[0].text, "3. third thing");
        assert_eq!(lines[1].text, "4. fourth");
    }

    #[test]
    fn inline_marks_strip_and_links_keep_their_text() {
        let doc = "see **bold** and __ul__ and `code` and [the doc](http://x/y.md) end";
        let text = md_lines(doc, W, CAP)[0].text.clone();
        assert!(
            text.contains("see bold and ul and code and the doc end"),
            "{text}"
        );
        for bad in ["**", "__", "`", "http://"] {
            assert!(!text.contains(bad), "{bad} survives: {text}");
        }
    }

    #[test]
    fn paragraphs_wrap_to_the_width() {
        let para = "word ".repeat(60);
        let lines = md_lines(para.trim(), 30, CAP);
        assert!(lines.len() >= 5, "long paragraph wraps: {lines:?}");
        for l in &lines {
            assert!(l.text.chars().count() <= 30, "row width held: {:?}", l.text);
        }
    }

    #[test]
    fn the_cap_ends_with_a_true_remaining_count() {
        let para = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let lines = md_lines(&para, W, 10);
        assert_eq!(lines.len(), 11, "cap plus one ellipsis line");
        assert_eq!(lines[10].text, "\u{2026} 20 more lines");
        assert_eq!(lines[9].text, "line9");
    }

    #[test]
    fn an_unterminated_fence_still_terminates() {
        let doc = "a\n```\nleft open\nforever";
        let lines = md_lines(doc, W, CAP);
        assert!(lines.iter().any(|l| l.text == "left open"));
        assert!(lines.iter().any(|l| l.text == "forever"));
    }
}
