//! `spawn-remote`: the cross-host spawn relay command. One ssh hop runs the
//! spawn ON the target host: the remote machine's own `fno agents spawn`
//! answers with its own gate, registry, config, and lanes, and its output
//! and exit code pass through byte-verbatim. A target whose host normalizes
//! to this machine strips the `@host` and execs the local front door, so
//! the local command's refusals stay the only voice. No store lane and no
//! queue: a spawn is a command execution, not a payload; an unreachable
//! host fails loudly and the caller retries.
//!
//! The sending machine's spawn gate admits the caller before the claim. A
//! `--cwd` or `--add-dir` value names a path ON THE TARGET HOST: the relay
//! forwards byte-verbatim and the remote command validates.

use crate::mail_bus::normalize_host;
use crate::mail_remote::{fno_binary, split_target, target_error};
use std::ffi::OsString;
use std::process::Command;

/// What the relay execs, decided without touching the network. Pure, so the
/// same-host and cross-host shapes are testable without ssh.
pub struct SpawnPlan {
    /// The target host, normalized.
    pub host: String,
    /// True when the target is this machine: exec the stripped argv locally.
    pub same_host: bool,
    /// The remote (or local-forward) command words, beginning `agents spawn`.
    pub words: Vec<String>,
}

/// Build the plan from the relay tail (everything after `spawn-remote`).
/// `local_host` arrives pre-resolved, so tests pin the shapes without env
/// reads. The `--name` value is the only slot that may carry the `@host`
/// address; every other word rides in order. `--name @<host>` drops the flag
/// from the exec'd words, so the remote slug mint answers and no recursion
/// survives the strip.
pub fn spawn_plan(args: &[String], local_host: &str) -> Result<SpawnPlan, String> {
    let mut name_at: Option<(usize, String, usize)> = None;
    let mut i = 0;
    while i < args.len() {
        let word = args[i].as_str();
        // `taken` is the flag's argv span: the bare form is two slots
        // (flag, value), the typer equals spelling is one.
        let mut hit = |v: &str, taken: usize| -> Result<(), String> {
            if v.contains('@') && name_at.is_some() {
                return Err("spawn takes one cross-host --name".to_string());
            }
            if v.contains('@') {
                name_at = Some((i, v.to_string(), taken));
            }
            Ok(())
        };
        if word == "--name" {
            let value = args
                .get(i + 1)
                .ok_or_else(|| "--name needs a value".to_string())?;
            hit(&value, 2)?;
            i += 2;
        } else if let Some(v) = word.strip_prefix("--name=") {
            hit(v, 1)?;
            i += 1;
        } else {
            i += 1;
        }
    }
    let Some((idx, value, taken)) = name_at else {
        return Err("usage: fno agents spawn --name <name>@<host> [args...] <prompt>".to_string());
    };
    let (name, host) = if let Some(rest) = value.strip_prefix('@') {
        if rest.is_empty() {
            return Err(target_error(&value));
        }
        (String::new(), rest.to_string())
    } else {
        split_target(&value)?
    };
    let host = normalize_host(&host);
    let same_host = host == normalize_host(local_host);
    // The --name value loses its @host part; the flag disappears when the
    // name is empty. Every other word keeps its order byte-verbatim.
    let mut words: Vec<String> = vec!["agents".to_string(), "spawn".to_string()];
    for (i, word) in args.iter().enumerate() {
        if i == idx {
            if !name.is_empty() {
                words.push("--name".to_string());
                words.push(name.clone());
            }
        } else if !(taken == 2 && i == idx + 1) {
            words.push(word.clone());
        }
    }
    Ok(SpawnPlan {
        host,
        same_host,
        words,
    })
}

/// Binary-direct entry from `fno-agents spawn-remote <tail...>`, where the
/// tail is the spawn argv after the command word.
pub fn run(args: &[String]) -> i32 {
    let local_host = crate::claims::hostname();
    let plan = match spawn_plan(args, &local_host) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("fno agents spawn: {e}");
            return 2;
        }
    };
    if plan.same_host {
        let mut argv: Vec<OsString> = vec![fno_binary()];
        for word in &plan.words {
            argv.push(OsString::from(word));
        }
        // No @ name rides, so the front door forwards, never re-enters.
        return crate::mail_remote::exec_forward(&argv, "fno agents spawn", false);
    }
    let argv = crate::mail_remote::ssh_argv(&plan.host, &plan.words);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("fno agents spawn: ssh to {} failed: {e}", plan.host);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One table-style declaration: the CI delta caps new test declarations.
    #[test]
    fn spawn_relay_plans_its_shapes() {
        // Cross-host: the ssh argv runs the remote machine's own front door
        // with the @host part stripped and every other word in order.
        let plan = spawn_plan(
            &[
                "--name".to_string(),
                "quinlan@mbp3".to_string(),
                "fix".to_string(),
                "it".to_string(),
            ],
            "imac2d2",
        )
        .expect("plans");
        assert!(!plan.same_host);
        assert_eq!(plan.host, "mbp3");
        let line = crate::mail_remote::ssh_argv(&plan.host, &plan.words).join(" ");
        assert!(line.contains("'agents' 'spawn'"));
        assert!(line.contains("'--name' 'quinlan'"));
        assert!(
            !line.contains("'quinlan@mbp3'"),
            "the host part never rides"
        );
        // `@host` alone mints remotely: the flag disappears, no @ rides.
        let plan = spawn_plan(
            &["--name".to_string(), "@mbp3".to_string(), "hi".to_string()],
            "imac2d2",
        )
        .expect("plans");
        assert!(!plan.words.contains(&"--name".to_string()));
        assert!(!plan.words.iter().any(|w| w.contains('@')));
        // Same-host: the stripped words carry no @, so the forward cannot
        // re-enter this relay.
        let plan = spawn_plan(
            &[
                "--name".to_string(),
                "q@MBP3.local".to_string(),
                "hi".to_string(),
            ],
            "mbp3",
        )
        .expect("plans");
        assert!(plan.same_host);
        assert!(!plan.words.iter().any(|w| w.contains('@')));
        assert!(plan.words.contains(&"q".to_string()));
        // A second plain --name rides untouched beside the cross-host one.
        let plan = spawn_plan(
            &[
                "--name".to_string(),
                "q@mbp3".to_string(),
                "--name".to_string(),
                "plain".to_string(),
            ],
            "imac2d2",
        )
        .expect("plans");
        assert!(
            !plan.same_host,
            "the @-name carries the address; the plain one rides"
        );
        assert!(plan.words.contains(&"q".to_string()));
        assert!(plan.words.contains(&"plain".to_string()));
        // The typer equals spelling parses the same as the bare form.
        let plan =
            spawn_plan(&["--name=q@mbp3".to_string(), "hi".to_string()], "imac2d2").expect("plans");
        assert!(!plan.same_host);
        assert!(plan.words.contains(&"q".to_string()));
        assert!(!plan.words.iter().any(|w| w.contains('@')));
        // Each refusal shape: no @ name, a double @, empty host, bare @,
        // missing value.
        assert!(spawn_plan(&["hi".to_string()], "x").is_err());
        assert!(spawn_plan(&["--name".to_string(), "q@a@b".to_string()], "x").is_err());
        assert!(spawn_plan(&["--name".to_string(), "q@".to_string()], "x").is_err());
        assert!(spawn_plan(&["--name".to_string(), "@".to_string()], "x").is_err());
        assert!(spawn_plan(&["--name".to_string()], "x").is_err());
    }
}
