//!  The `fno mux rows` porcelain: the one row-set receipt. Prints
//! the row set the server last derived, each row carrying the server's
//! paint verdict. Named by the question it answers: what rows exist, and
//! why would a row not paint.

use std::ffi::OsString;

use super::{run_on_existing_server, take_common_flags, ControlVerb, EXIT_OK, EXIT_USAGE};

/// `fno mux rows [--json]` : the one row-set receipt. Prints the
/// row set the server last derived, each row carrying the paint verdict.
/// With no server, run_on_existing_server refuses naming the session
/// (never an empty success that reads as zero rows).
pub fn rows(args: &[OsString], env_session: Option<&str>) -> i32 {
    let (session, json, rest) = match take_common_flags(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno mux rows: {e}");
            return EXIT_USAGE;
        }
    };
    if let Some(verb) = rest.first() {
        eprintln!("fno mux rows: unknown argument {verb} (flags: --json, --session)");
        return EXIT_USAGE;
    }
    run_on_existing_server(
        session.as_deref(),
        env_session,
        json,
        ControlVerb::AgentRowsGet,
    )
}

pub fn render_receipt(rows: &[crate::proto::AgentRowReceipt], json: bool) -> i32 {
    if json {
        // Machine-first: the full receipt, one object per row. `pane`
        // names the substrate (null = paneless) and `reason` the
        // server's paint verdict (null = would paint). ONLY the array
        // on stdout, so a pipe into json.load sees clean JSON.
        println!(
            "{}",
            serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
        );
    } else {
        // The operator-readable rendering: one line per row, the
        // paint verdict last, then the summary line.
        for r in rows {
            let squad = r
                .squad
                .as_ref()
                .map(|(id, name)| match name.as_deref() {
                    Some(n) if !n.is_empty() => n.to_string(),
                    _ => format!("id {id}"),
                })
                .unwrap_or_else(|| "~ elsewhere".into());
            let pane = r
                .pane
                .map(|p| p.to_string())
                .unwrap_or_else(|| "paneless".into());
            let reason = r.reason.as_deref().unwrap_or("would paint");
            let harness = r.harness.as_deref().unwrap_or("-");
            let resumable = r.resumable.unwrap_or(false);
            println!(
                "{} harness={} squad={} pane={} exited={} tombstone={} resumable={} reason={}",
                r.name, harness, squad, pane, r.exited, r.tombstone, resumable, reason
            );
        }
        println!(
            "rows: {} ({} paneless)",
            rows.len(),
            rows.iter().filter(|r| r.pane.is_none()).count()
        )
    }
    EXIT_OK
}
