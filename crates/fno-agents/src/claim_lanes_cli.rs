//! The hidden `claim lane-*` operations over the native lane-slot library.
//!
//! The wheel claims CLI's `--lane` mode execs these, so the lane cap has one
//! implementation (crate::lanes) once the python lanes module deletes.

use serde_json::json;

use crate::lanes;

fn usage() -> i32 {
    eprintln!("usage: fno-agents claim lane-acquire --lane ID --max-lanes N [--ttl 1h] [--domain D] [--json]\n       fno-agents claim lane-release --lane ID [--json]\n       fno-agents claim lane-count [--json]");
    2
}

/// `claim lane-acquire`: exit 1 on a full cap (the same retry-later code as
/// a held claim), exit 2 on validation, JSON or one receipt line otherwise.
pub fn run_lane_acquire(args: &[String]) -> i32 {
    let mut lane: Option<String> = None;
    let mut max_lanes: Option<usize> = None;
    let mut ttl_ms: Option<i64> = None;
    let mut domain: Option<String> = None;
    let mut json_out = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--lane" => match args.get(index + 1) {
                Some(value) => {
                    lane = Some(value.clone());
                    index += 1;
                }
                None => return usage(),
            },
            "--max-lanes" => match args.get(index + 1).and_then(|v| v.parse().ok()) {
                Some(value) => {
                    max_lanes = Some(value);
                    index += 1;
                }
                None => return usage(),
            },
            "--ttl" => match args
                .get(index + 1)
                .and_then(|v| crate::claims::parse_ttl_ms(v))
            {
                Some(value) => {
                    ttl_ms = Some(value);
                    index += 1;
                }
                None => {
                    eprintln!("validation error: --ttl must parse (e.g. 1h, 30m)");
                    return 2;
                }
            },
            "--domain" => match args.get(index + 1) {
                Some(value) => {
                    domain = Some(value.clone());
                    index += 1;
                }
                None => return usage(),
            },
            "--json" | "-J" => json_out = true,
            _ => return usage(),
        }
        index += 1;
    }
    let Some(lane) = lane else {
        return usage();
    };
    let Some(max_lanes) = max_lanes else {
        eprintln!("lane-acquire requires --max-lanes");
        return usage();
    };
    let extra_metadata = domain.map(|domain| {
        let mut metadata = serde_json::Map::new();
        metadata.insert("domain".to_string(), serde_json::Value::String(domain));
        metadata
    });
    match lanes::acquire_lane_slot(max_lanes, &lane, ttl_ms, None, extra_metadata, None) {
        Ok(Some(claim)) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "key": claim.key,
                        "holder": claim.holder,
                        "lane_id": lane,
                        "domain": claim.metadata.get("domain"),
                    })
                );
            } else {
                println!("acquired lane slot {} for lane {lane}", claim.key);
            }
            0
        }
        Ok(None) => {
            eprintln!("lane cap full (max_lanes={max_lanes})");
            1
        }
        Err(error) => {
            eprintln!("validation error: {error}");
            2
        }
    }
}

/// `claim lane-release`: silent success when the lane holds none.
pub fn run_lane_release(args: &[String]) -> i32 {
    let mut lane: Option<String> = None;
    let mut json_out = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--lane" => match args.get(index + 1) {
                Some(value) => {
                    lane = Some(value.clone());
                    index += 1;
                }
                None => return usage(),
            },
            "--json" | "-J" => json_out = true,
            _ => return usage(),
        }
        index += 1;
    }
    let Some(lane) = lane else {
        return usage();
    };
    if let Err(error) = lanes::release_lane_slot(&lane, None) {
        eprintln!("Error: {error}");
        return 1;
    }
    if json_out {
        println!("{}", json!({"lane_id": lane, "released": true}));
    } else {
        println!("released lane {lane}");
    }
    0
}

/// `claim lane-count`: the derived observability count, never the cap gate.
pub fn run_lane_count(args: &[String]) -> i32 {
    let json_out = args.iter().any(|arg| arg == "--json" || arg == "-J");
    let count = lanes::active_lane_count(None);
    if json_out {
        println!("{}", json!({"active_lanes": count}));
    } else {
        println!("{count}");
    }
    0
}

/// `claim lane-reconcile --lane ID --pid P`: the worker's target-init
/// re-anchor (LD#8). Exit 0 including the no-op shapes (no slot, no pid),
/// matching the python arm's never-break-init contract.
pub fn run_lane_reconcile(args: &[String]) -> i32 {
    let mut lane: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--lane" => match args.get(index + 1) {
                Some(value) => {
                    lane = Some(value.clone());
                    index += 1;
                }
                None => return usage(),
            },
            "--pid" => match args.get(index + 1).and_then(|v| v.parse().ok()) {
                Some(value) => {
                    pid = Some(value);
                    index += 1;
                }
                None => return usage(),
            },
            _ => return usage(),
        }
        index += 1;
    }
    let Some(lane) = lane else {
        return usage();
    };
    match lanes::reconcile_lane_slot(&lane, pid, None) {
        Ok(Some(claim)) => {
            println!("reconciled lane slot {} for lane {lane}", claim.key);
            0
        }
        Ok(None) => {
            println!("no slot to reconcile for lane {lane}");
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}
