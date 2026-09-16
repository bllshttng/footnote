//! Native claim command helpers.
//!
//! Wave 13 keeps the claim decision in Rust and gives the Python compatibility
//! layer one JSON door. Wave 14 moves these helpers from the lockfile adapter
//! to the graph store without changing the command contract.

use crate::claims::{self, ClaimState};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn claims_dir(root: Option<&Path>) -> Result<PathBuf, String> {
    claims::claims_dir_for(root).ok_or_else(|| "claims root is unavailable".to_string())
}

fn archive_path(path: &Path) -> Result<PathBuf, String> {
    let archive = path
        .parent()
        .ok_or_else(|| "claim path has no parent".to_string())?
        .join(".expired");
    std::fs::create_dir_all(&archive).map_err(|error| error.to_string())?;
    let stamp = claims::now_ms();
    let name = path
        .file_name()
        .ok_or_else(|| "claim path has no filename".to_string())?
        .to_string_lossy();
    Ok(archive.join(format!("{name}.{stamp}")))
}

pub fn force_release(
    key: &str,
    reason: &str,
    root: Option<&Path>,
) -> Result<Value, String> {
    if key.is_empty() {
        return Err("key must be non-empty".to_string());
    }
    if reason.trim().is_empty() {
        return Err("reason must be non-empty for force-release".to_string());
    }
    let path = claims::claim_path(key, root)?;
    if !path.exists() {
        return Ok(json!({
            "key": key,
            "path": path,
            "archived": false,
            "force_released": false,
            "previous_holder": Value::Null,
        }));
    }
    let previous_holder = claims::read_claim_file(&path)
        .ok()
        .map(|record| record.holder);
    let destination = archive_path(&path)?;
    std::fs::rename(&path, &destination).map_err(|error| error.to_string())?;
    Ok(json!({
        "key": key,
        "path": path,
        "archived": true,
        "force_released": true,
        "previous_holder": previous_holder,
    }))
}

pub fn reap(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    let directory = claims_dir(root)?;
    let records = if directory.is_dir() {
        claims::list_in(std::slice::from_ref(&directory), None, true)?
    } else {
        Vec::new()
    };
    let mut reaped = 0usize;
    let mut would_reap = 0usize;
    let mut failures = Vec::new();
    for record in records {
        if claims::classify(&record, None) != ClaimState::Stale {
            continue;
        }
        would_reap += 1;
        if !apply {
            continue;
        }
        let path = claims::claim_path(&record.key, root)?;
        let destination = archive_path(&path)?;
        match std::fs::rename(&path, &destination) {
            Ok(()) if !path.exists() && destination.exists() => reaped += 1,
            Ok(()) => failures.push(format!("archive verification failed: {}", path.display())),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    Ok(json!({
        "apply": apply,
        "scanned": would_reap,
        "would_reap": would_reap,
        "reaped": reaped,
        "reap_failed": failures,
        "root": directory,
    }))
}
