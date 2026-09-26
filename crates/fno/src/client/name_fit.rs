use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Fit an agent name without hiding the suffix that distinguishes its scope or
/// generation. The extra column in an odd budget goes to the tail.
pub(crate) fn fit_name(name: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if name.width() <= width {
        return name.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let available = width - 1;
    let head_width = available / 2;
    let tail_width = available - head_width;

    let mut head = String::new();
    let mut head_used = 0;
    for ch in name.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if head_used + char_width > head_width {
            break;
        }
        head.push(ch);
        head_used += char_width;
    }

    let mut tail = String::new();
    let mut tail_used = 0;
    for ch in name.chars().rev() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if tail_used + char_width > tail_width {
            break;
        }
        tail.push(ch);
        tail_used += char_width;
    }
    let tail: String = tail.chars().rev().collect();

    format!("{head}…{tail}")
}

pub(crate) fn pad_to(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count > width {
        let mut text: String = s.chars().take(width.saturating_sub(1)).collect();
        text.push('…');
        text
    } else {
        let mut text = s.to_string();
        text.push_str(&" ".repeat(width - count));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::fit_name;

    #[test]
    fn keeps_the_distinguishing_tail() {
        assert_eq!(fit_name("dispatch-fno-8bef7b", 12), "dispa…8bef7b");
        assert_eq!(fit_name("dispatch-etl-631fd8", 12), "dispa…631fd8");
        assert_eq!(fit_name("king-a792-control", 12), "king-…ontrol");
        assert_eq!(fit_name("king-a792-control-g2", 12), "king-…rol-g2");
    }

    #[test]
    fn preserves_fitting_names_and_handles_small_widths() {
        assert_eq!(fit_name("short", 5), "short");
        assert_eq!(fit_name("long", 1), "…");
        assert_eq!(fit_name("long", 0), "");
    }
}
