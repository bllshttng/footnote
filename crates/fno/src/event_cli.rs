//! Native storage-level verbs under `fno doctor event`.
//!
//! The Python `fno doctor event emit` keeps its rich surface (validation,
//! attestation stamping, mirrors, parent push) and delegates storage to
//! `emit-envelope` here. `find`/`audit`/`gc` routing follows in the reader
//! cutover wave, intercepted by the same predicate; until then the Python
//! implementations stay authoritative for those names. No top-level verb
//! exists (law d-fe66560a); the group stays nested under `doctor`.

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;

/// The verbs the native surface serves today. `find`/`audit`/`gc` join when
/// their Python output contracts are ported (reader cutover wave).
pub const NATIVE_EVENT_SUBCOMMANDS: &[&str] = &["emit-envelope", "export"];

/// Classify `fno doctor event <sub> ...` for the front door: `Some(rest)`
/// runs natively, `None` forwards to the Python CLI.
pub fn classify_doctor_event(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.len() < 3 {
        return None;
    }
    let a0 = args[0].to_str()?;
    let a1 = args[1].to_str()?;
    let a2 = args[2].to_str()?;
    if a0 == "doctor" && a1 == "event" && NATIVE_EVENT_SUBCOMMANDS.contains(&a2) {
        Some(args[2..].to_vec())
    } else {
        None
    }
}

/// Parse and run one native event subcommand; returns the exit code.
pub fn run(args: &[OsString]) -> i32 {
    let sub = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let rest: &[OsString] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "emit-envelope" => run_emit_envelope(rest),
        "export" => run_export(rest),
        _ => {
            eprintln!("error: expected a subcommand (emit-envelope | export)");
            2
        }
    }
}

fn run_emit_envelope(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut file: Option<PathBuf> = None;
    let mut requested_id: Option<String> = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let tok = match tok.to_str() {
            Some(t) => t,
            None => continue,
        };
        match tok {
            "--events" => journal = it.next().map(PathBuf::from),
            "--id" => requested_id = it.next().map(|v| v.to_string_lossy().into_owned()),
            "--file" => file = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    let Some(journal) = journal else {
        eprintln!("error: --events <events.jsonl> is required (it names the sibling store)");
        return 2;
    };
    // The envelope arrives on stdin or from --file; exact bytes are stored
    // byte-for-byte, never re-serialized.
    let mut envelope = String::new();
    let read_result = match &file {
        Some(path) => std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut envelope)),
        None => std::io::stdin().read_to_string(&mut envelope),
    };
    if let Err(e) = read_result {
        eprintln!("error: could not read the envelope: {e}");
        return 1;
    }
    let requested = requested_id.as_deref();
    let result = fno_event_store::append_envelope(&journal, envelope.trim(), requested);
    match result {
        Ok(r) => {
            let receipt = serde_json::json!({
                "success": true,
                "store": r.store.display().to_string(),
                "event_id": r.event_id,
                "seq": r.seq,
                "retention_class": r.retention_class,
                "inserted": r.inserted,
            });
            println!("{receipt}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_export(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        let tok = match tok.to_str() {
            Some(t) => t,
            None => continue,
        };
        match tok {
            "--events" => journal = it.next().map(PathBuf::from),
            "--out" => out = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    let journal = match journal {
        Some(j) => j,
        None => {
            eprintln!("error: --events is required");
            return 2;
        }
    };
    let out = match out {
        Some(o) => o,
        None => {
            eprintln!("error: --out is required");
            return 2;
        }
    };
    match fno_event_store::export_jsonl(&journal, &out) {
        Ok(n) => {
            let payload = serde_json::json!({
                "success": true,
                "store": fno_event_store::store_path(&journal).display().to_string(),
                "exported": n,
                "path": out.display().to_string(),
                "snapshot": true,
            });
            println!("{payload}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn classify_admits_only_native_subcommands() {
        assert!(classify_doctor_event(&mk(&[
            "doctor",
            "event",
            "emit-envelope",
            "--events",
            "/tmp/x"
        ]))
        .is_some());
        assert!(
            classify_doctor_event(&mk(&["doctor", "event", "export", "--out", "/tmp/x"])).is_some()
        );
        // Python-owned names keep forwarding to the Python CLI.
        assert!(
            classify_doctor_event(&mk(&["doctor", "event", "emit", "-t", "blocked"])).is_none()
        );
        assert!(classify_doctor_event(&mk(&["doctor", "event", "gc", "--dry-run"])).is_none());
        assert!(classify_doctor_event(&mk(&["doctor", "event"])).is_none());
        assert!(classify_doctor_event(&mk(&["doctor"])).is_none());
    }
}
