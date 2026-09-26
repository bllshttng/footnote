//! The plan-doc writer, ported from the Python `fno.plan._project`/
//! `_rollup`/`_status` legs and the stamp module, served by the graph keeper.

pub mod codec;
pub mod keeper;
pub mod lock;
pub mod node_accessors;
pub mod project;
pub mod rollup;
pub mod stamp;
pub mod status;

/// `datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")`.
pub fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day, hour, min, sec) = crate::events::civil_from_unix(dur.as_secs());
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Write a projected doc, degrading to a warning (the Python converger never
/// raises).
pub(crate) fn write_or_warn(
    target: &std::path::Path,
    fields: &codec::Fields,
    rest: &str,
    warnings: &mut Vec<String>,
) {
    if let Err(e) = codec::write_plan_file(target, fields, rest) {
        warnings.push(format!(
            "warning: plan projection failed for {}: {e}",
            target.display()
        ));
    }
}
