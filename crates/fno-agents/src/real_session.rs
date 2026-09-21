//! Answers "did a real run produce this row?" for the corrections corpus.
//! Measured 2026-09-18: 360 of 418 live corrections.log rows carry a
//! per-test temp-dir postmortem path that leaked past `append_corrections_pointer`'s
//! log ladder, so the monthly packet renders 86 percent dead pointers. The
//! root cause is the WRITER, and this module is the predicate the writer
//! consults; readers (autocorrect-pack.sh) filter the rows already on disk.
//!
//! The module reads no environment: the caller (the corrections pointer
//! writer) resolves the home ladder once and passes the postmortems root
//! in, so the same fact is not carried by a second reader.

use std::path::{Path, PathBuf};

/// The postmortems root the corrections corpus at `home` owns, resolved
/// through the same home ladder the corrections log uses: an explicit
/// FNO_HOME value replaces ~/.fno wholesale (mirroring
/// `corrections_log_path`), else home/.fno. None when no home is known -
/// the same shape that makes the log ladder return early, so there is
/// nothing for a row to be real against.
pub(crate) fn postmortems_root_for_home(
    fno_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    match fno_home {
        Some(p) => Some(p.join("postmortems")),
        None => home.map(|h| h.join(".fno/postmortems")),
    }
}

/// Containment, never a name match: a run is real when the postmortem path
/// lies under the postmortems root of the same home the log resolved
/// through. A temp-dir postmortem with a real-home log is the fixture case.
/// Never match on the string "/tmp" or "/var/folders": a run whose whole
/// FNO_HOME sits in a temp dir under test is real relative to that home,
/// and a name match would refuse it.
pub(crate) fn is_real_run(root: Option<&Path>, postmortem: &Path) -> bool {
    match root {
        Some(root) => postmortem.starts_with(root),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_run_is_containment_under_home_postmortems() {
        // AC1-HP predicate half: a postmortem under the resolved
        // ~/.fno/postmortems of the same home the log resolved through.
        let root = Path::new("/ops/home/.fno/postmortems");
        assert!(is_real_run(
            Some(root),
            Path::new("/ops/home/.fno/postmortems/2026-09-21.md")
        ));
        // A sibling directory of the postmortems root is outside it.
        assert!(!is_real_run(
            Some(root),
            Path::new("/ops/home/.fno/postmortems-archive/pm.md")
        ));
    }

    #[test]
    fn temp_dir_postmortem_with_real_home_log_is_the_fixture_case() {
        // AC1-EDGE: the 360-row shape. The postmortem lives in a per-test
        // temp dir while the log resolved through a real home.
        let root = Path::new("/Users/op/.fno/postmortems");
        assert!(!is_real_run(
            Some(root),
            Path::new("/var/folders/T/.tmpABCD/postmortems/pm.md")
        ));
    }

    #[test]
    fn fno_home_replaces_the_whole_fno_dir() {
        // AC1-ERR: containment, never a /tmp name match. A run whose whole
        // FNO_HOME is a temp dir is real when the postmortem sits under
        // that home's postmortems dir.
        let fno_home = Path::new("/var/folders/T/.tmpRS");
        assert_eq!(
            postmortems_root_for_home(Some(fno_home), Some(Path::new("/Users/op"))),
            Some(fno_home.join("postmortems"))
        );
        assert!(is_real_run(
            Some(&fno_home.join("postmortems")),
            Path::new("/var/folders/T/.tmpRS/postmortems/pm.md")
        ));
        // A different temp dir is still not this home's corpus.
        assert!(!is_real_run(
            Some(&fno_home.join("postmortems")),
            Path::new("/var/folders/T/.tmpOTHER/pm.md")
        ));
    }

    #[test]
    fn no_home_is_never_real() {
        // The log ladder returns early with no home; the predicate matches
        // that shape rather than inventing a corpus.
        assert_eq!(postmortems_root_for_home(None, None), None);
        assert!(!is_real_run(None, Path::new("/tmp/pm.md")));
    }
}
