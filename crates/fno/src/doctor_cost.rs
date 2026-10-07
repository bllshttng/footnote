//! Native doctor telemetry health and reported-cost export.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fno doctor cost", disable_help_subcommand = true)]
struct Args {
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Report receiver health and recent Claude worker coverage.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Export reported request cost by UTC day, session, model and skill.
    Export {
        #[arg(long, required = true)]
        csv: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

pub fn classify(args: &[OsString]) -> Option<Vec<OsString>> {
    (args.first().and_then(|a| a.to_str()) == Some("doctor")
        && args.get(1).and_then(|a| a.to_str()) == Some("cost"))
    .then(|| args[2..].to_vec())
}

fn home() -> PathBuf {
    crate::agents_view::registry_path()
        .parent()
        .expect("registry path has a parent")
        .to_path_buf()
}

fn read_health() -> Result<serde_json::Value, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    crate::otel_read::health(
        &home(),
        crate::digest_overlay::section_bool(&cwd, "telemetry", "claude_otel", true),
    )
}

pub fn default_report(args: &[OsString]) {
    if args.first().and_then(|a| a.to_str()) != Some("doctor")
        || !(args.len() == 1 || (args.len() == 2 && args[1].to_str() == Some("--json")))
    {
        return;
    }
    match read_health() {
        Ok(value) => eprintln!("fno doctor: {}", crate::otel_read::health_line(&value)),
        Err(error) => eprintln!("fno doctor: telemetry READER FAILED: {error}"),
    }
}

pub fn run(args: &[OsString]) -> i32 {
    let parsed = match Args::try_parse_from(
        std::iter::once(OsString::from("fno doctor cost")).chain(args.iter().cloned()),
    ) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    match parsed.action.unwrap_or(Action::Status { json: false }) {
        Action::Status { json } => match read_health() {
            Ok(value) => {
                let text = if json {
                    value.to_string()
                } else {
                    crate::otel_read::health_line(&value)
                };
                if let Err(error) = writeln!(std::io::stdout().lock(), "{text}") {
                    eprintln!("cost status: {error}");
                    return 2;
                }
                match value.get("status").and_then(|v| v.as_str()) {
                    Some("healthy" | "idle" | "off") => 0,
                    _ => 1,
                }
            }
            Err(error) => {
                eprintln!("cost status: READER FAILED: {error}");
                2
            }
        },
        Action::Export { output, .. } => {
            let db = home().join("otel/otel.db");
            match crate::otel_read::export_csv(&db) {
                Ok((csv, count)) => {
                    let result = match output {
                        Some(path) => std::fs::write(path, csv),
                        None => std::io::stdout().lock().write_all(csv.as_bytes()),
                    };
                    if let Err(error) = result {
                        eprintln!("cost export: {error}");
                        return 2;
                    }
                    eprintln!("cost export: {count} group(s) from {}", db.display());
                    if count == 0 {
                        eprintln!("cost export: no stored request costs are available");
                    }
                    0
                }
                Err(error) => {
                    eprintln!("cost export: {error}");
                    2
                }
            }
        }
    }
}
