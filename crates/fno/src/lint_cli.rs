//! Native `fno doctor lint style` classification for the front door. The
//! `style` check runs in the sibling fno-agents binary's hidden
//! `style-check` verb; every other `doctor lint` check keeps forwarding to
//! the Python CLI. Same split as `doctor event`: the Python leg keeps the
//! names it still owns until their cutover, and the classified argv passes
//! to the sibling byte-verbatim.

use std::ffi::OsString;

/// Classify `fno doctor lint style ...` for the front door: `Some(rest)`
/// execs the sibling fno-agents `style-check` verb with `rest` (the flags
/// AFTER the check name; the classifier strips the whole classified
/// prefix); `None` forwards to the Python CLI.
pub fn classify_doctor_lint_style(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 3 {
        return None;
    }
    let a0 = args[0].to_str()?;
    let a1 = args[1].to_str()?;
    let a2 = args[2].to_str()?;
    if a0 != "doctor" || a1 != "lint" || a2 != "style" {
        return None;
    }
    Some(args[3..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn classify_admits_only_lint_style() {
        let rest = classify_doctor_lint_style(&mk(&["doctor", "lint", "style", "--stdin"]))
            .expect("style classifies");
        assert_eq!(rest, mk(&["--stdin"]));
        assert!(classify_doctor_lint_style(&mk(&["doctor", "lint", "style"])).is_some());
        // Every other check keeps forwarding to the Python CLI.
        assert!(classify_doctor_lint_style(&mk(&["doctor", "lint", "menu-caps"])).is_none());
        assert!(classify_doctor_lint_style(&mk(&["doctor", "lint"])).is_none());
        // A different tree keeps forwarding.
        assert!(classify_doctor_lint_style(&mk(&["doctor", "event", "rows"])).is_none());
        assert!(classify_doctor_lint_style(&mk(&["doctor"])).is_none());
        assert!(classify_doctor_lint_style(&mk(&[])).is_none());
    }
}
