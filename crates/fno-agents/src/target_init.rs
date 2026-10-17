//! `fno do target init`: the native port of the Python verb, with a real
//! `--steps` flag. The bash helper stays; this module is the decision layer
//! the Python verb was: validate the call, run the gates, build the exact
//! helper environment, run the helper, propagate its exit, and print the
//! orientation report.
//!
//! The parity contract is `tests/golden/target_init/`, frozen from the
//! Python leg before its deletion and asserted by
//! `tests/target_init_parity.rs` through the `target-init` door. The
//! review-capability, dispatch-hold, and containment preflights are NOT
//! ported: when a node resolves they stay unset-stamped, so the helper's own
//! `fno do target check-*` gate calls (the one Python implementation) refuse
//! fail-closed in the same order; a free-text run resolves no node, every
//! one of those gates is a no-op for it, and `FNO_TARGET_INIT_GATED=1` says
//! so. The orientation report is ported here (the
//! `_print_orientation_report` post-step) because the golden scenarios print
//! it after a stub helper succeeded, where no bash hook could carry it.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "usage: fno do target init [--input <text|ab-id>] [--plan-path <path>] \
[--size S|M|L] [--model <m>] [--harness <h>] [--beastmode|--beast] [--no-merge] \
[--deliverables <n>] [--steps]\n";

/// A refusal: the message prints on stderr and the run exits with `code`.
struct Fail {
    message: String,
    code: i32,
}

fn refuse(message: impl Into<String>) -> Fail {
    Fail {
        message: message.into(),
        code: 2,
    }
}

/// The parsed verb surface. Every option the Python verb accepted, plus
/// `--steps`.
struct Args {
    input: Option<String>,
    plan_path: Option<String>,
    size: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    beastmode: bool,
    no_merge: bool,
    deliverables: Option<i64>,
    steps: bool,
}

/// Hand-rolled parse over the same spellings typer accepted, plus `--steps`.
/// An unknown flag or a bare positional exits 2 naming the offending token.
fn parse_args(rest: &[String]) -> Result<Args, Fail> {
    let mut args = Args {
        input: None,
        plan_path: None,
        size: None,
        model: None,
        harness: None,
        beastmode: false,
        no_merge: false,
        deliverables: None,
        steps: false,
    };
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--help" | "-h" => {
                print_help();
                return Err(Fail {
                    message: String::new(),
                    code: 0,
                });
            }
            "--input" => args.input = Some(next_value(&mut it, tok)?),
            "--plan-path" => args.plan_path = Some(next_value(&mut it, tok)?),
            "--size" => args.size = Some(next_value(&mut it, tok)?),
            "--model" | "-m" => args.model = Some(next_value(&mut it, tok)?),
            "--harness" | "-H" => args.harness = Some(next_value(&mut it, tok)?),
            // Retired spelling: the harness axis is --harness/-H. A value is
            // consumed so the error names the axis the way typer's tombstone
            // did, and any non-empty value refuses.
            "--provider" => {
                let value = next_value(&mut it, tok)?;
                if !value.trim().is_empty() {
                    return Err(refuse(
                        "fno do target init: --provider is retired; the harness axis is --harness/-H.",
                    ));
                }
            }
            "--beastmode" | "--beast" => args.beastmode = true,
            "--no-merge" => args.no_merge = true,
            "--deliverables" => {
                let raw = next_value(&mut it, tok)?;
                let parsed: i64 = raw
                    .trim()
                    .parse()
                    .map_err(|_| refuse(format!("fno do target init: invalid --deliverables {raw:?}; expected a positive integer.")))?;
                args.deliverables = Some(parsed);
            }
            "--steps" => args.steps = true,
            other => {
                if other.starts_with("--") {
                    return Err(usage_fail(&format!(
                        "unknown flag {other}; --steps is the one flag the Python verb did not carry"
                    )));
                }
                return Err(usage_fail(&format!(
                    "unexpected argument {other:?}; the verb takes flags only"
                )));
            }
        }
    }
    Ok(args)
}

fn next_value<'a, I>(it: &mut I, flag: &str) -> Result<String, Fail>
where
    I: Iterator<Item = &'a String>,
{
    match it.next() {
        Some(v) => Ok(v.clone()),
        None => Err(usage_fail(&format!("flag {flag} needs a value"))),
    }
}

fn usage_fail(detail: &str) -> Fail {
    Fail {
        message: format!("usage: fno do target init: {detail}\n{USAGE}"),
        code: 2,
    }
}

fn print_help() {
    print!("{USAGE}");
    print!(
        "\nBootstrap a target session via the canonical init script.\n\n\
         --steps  run this target one instructed step at a time; the stop gate\n\
         \x20        lets every stop through.\n"
    );
}

/// The public entry: the door hands the argv after `do target init`.
pub fn run(rest: &[String]) -> i32 {
    let args = match parse_args(rest) {
        Ok(a) => a,
        Err(f) => return finish_fail(&f),
    };
    match run_inner(&args) {
        Ok(code) => code,
        Err(f) => finish_fail(&f),
    }
}

fn finish_fail(f: &Fail) -> i32 {
    if !f.message.is_empty() {
        eprint!("{}", f.message);
        if !f.message.ends_with('\n') {
            eprintln!();
        }
    }
    f.code
}

fn run_inner(args: &Args) -> Result<i32, Fail> {
    // 1. Input required. A stub (empty input + plan) is exactly what the stop
    // hook archives, so refusing it here prevents the archive loop at the
    // source.
    if args.input.is_none() && args.plan_path.is_none() {
        return Err(refuse(
            "fno do target init: requires --input <text|ab-id> or --plan-path <path>.\n\
             Refusing to write a stub state file (empty input + plan); the stop \
             hook would archive it, and a re-running bootstrap loops.\n\
             Example: fno do target init --input \"fix the login redirect bug\"",
        ));
    }

    // 2. The denominator is a positive integer or absent. Zero/negative is
    // refused at parse, never stored.
    if let Some(n) = args.deliverables {
        if n <= 0 {
            return Err(refuse(format!(
                "fno do target init: --deliverables must be a positive integer (got \
                 {n}). Omit it for plan-backed runs; the plan's task count is the \
                 denominator."
            )));
        }
    }

    // 3. Size validation.
    let mut normalized_size = match &args.size {
        Some(raw) => {
            let upper = raw.trim().to_ascii_uppercase();
            if !matches!(upper.as_str(), "S" | "M" | "L") {
                return Err(refuse(format!(
                    "fno do target init: invalid --size '{raw}'; expected S, M, or L."
                )));
            }
            Some(upper)
        }
        None => None,
    };

    // 3b. Model validation: a whitespace-only --model is a typo, not a
    // choice. The Python body refused it at parse; the door keeps the
    // contract so the forward cannot smuggle an empty dispatch model in.
    if args.model.as_ref().map_or(false, |m| m.trim().is_empty()) {
        return Err(refuse(
            "fno do target init: --model must not be empty.".to_string(),
        ));
    }

    // 4. The plugin helper must exist before anything runs.
    let script_path = resolve_init_script();
    if !script_path.is_file() {
        return Err(refuse(format!(
            "fno do target init: needs the footnote plugin (skills + hooks), which \
             a bare `pip install fno` does not ship - the bundled CLI has no \
             pipeline to bootstrap.\n\
             Install the plugin and run from its checkout:\n\
             \x20 clone the footnote repo, then run `claude --plugin-dir \
             /path/to/footnote`\n\
             Or set CLAUDE_PLUGIN_ROOT / FNO_REPO_ROOT to an existing plugin \
             checkout. (resolved, not on disk: {})",
            script_path.display(),
        )));
    }

    // 5. The cwd repo (git discovery is the helper's business; the decision
    // layer needs the root for graph reads and the manifest).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // 6. One graph read feeds the resolver and the gates. A named dispatch
    // against an unreadable graph refuses: the hold state cannot be read,
    // and failing to read it must not read as unheld.
    let entries = graph_rows(&cwd);
    if entries.is_none()
        && (args
            .input
            .as_deref()
            .map(input_may_name_a_node)
            .unwrap_or(false)
            || args.plan_path.is_some())
    {
        return Err(refuse(
            "fno do target init: dispatch-hold-invalid: backlog graph is \
             unreadable; refusing to assume unheld. Nothing was claimed. Fix or \
             restore graph.json and retry.",
        ));
    }

    // 7. Dispatch-node resolution (exact ids only). The review-capability,
    // dispatch-hold, and containment gates stay PYTHON-owned: when a node
    // resolves, FNO_TARGET_INIT_GATED stays unset so the helper's own gate
    // calls (`fno do target check-review-gate`, `check-dispatch-hold`,
    // `check-contained`) refuse fail-closed in the same order; a free-text
    // run resolves no node, every one of those gates is a no-op for it, and
    // the stamp says so - byte-matching the Python env contract.
    let dispatch_node = resolve_dispatch_node(
        args.input.as_deref(),
        args.plan_path.as_deref(),
        entries.as_deref(),
    );

    // 8. The binding transport: a DECLARED required binding revalidates
    // natively before the claim. A refused verdict exits 1, a fork exits 3.
    let binding = dispatch_node.as_ref().map(|node| {
        let req = crate::backlog::target_binding::Request {
            input: args.input.clone().unwrap_or_default(),
            node: Some(node_str(Some(node), "id")),
            plan_path: args.plan_path.clone(),
            phase: crate::backlog::target_binding::Phase::Init,
            allow_in_review: std::env::var("TARGET_ALLOW_IN_REVIEW").as_deref() == Ok("1"),
        };
        crate::backlog::target_binding::prepare(&req, &cwd)
    });
    if let Some(receipt) = &binding {
        if !receipt.message.is_empty() {
            eprintln!("{}", receipt.message);
        }
        if receipt.verdict == "refused" || receipt.verdict == "forked" {
            if let Some(next) = &receipt.next {
                eprintln!("next: {next}");
            }
            return Err(Fail {
                message: String::new(),
                code: receipt.exit_code(),
            });
        }
    }

    // 8.5 Blast-radius modulation: a deterministic read of the plan's File
    // Ownership Map floors the size profile at M (high blast) or downgrades
    // an unpinned size to S (low) BEFORE the immutable manifest is written.
    // Gated on config.target.blast.enabled first, so the disabled path does
    // zero extra work (no classify, no scope read). Every failure degrades to
    // "unknown" -> no change.
    let blast_cfg = crate::target_blast::load_blast_cfg(&cwd);
    if blast_cfg.enabled {
        let blast_plan = crate::target_blast::resolve_plan_for_blast(
            args.plan_path.as_deref(),
            args.input.as_deref(),
            entries.as_deref(),
        );
        if let Some(blast_plan) = blast_plan {
            let repo_root = git_toplevel(&cwd);
            let (verdict, matched, _reason) = crate::target_blast::classify_plan_file(
                &blast_plan,
                &blast_cfg,
                repo_root.as_deref(),
                crate::target_blast::control_plane_scope_path().as_deref(),
            );
            // An existing TARGET_SIZE in the env is an operator pin too, not
            // only --size: a low-blast downgrade must never strip it.
            let env_size = std::env::var("TARGET_SIZE")
                .ok()
                .map(|v| v.trim().to_ascii_uppercase())
                .filter(|v| matches!(v.as_str(), "S" | "M" | "L"));
            let (effective, announce) = crate::target_blast::modulate_size(
                &verdict,
                normalized_size.is_some() || env_size.is_some(),
                normalized_size.as_deref().or(env_size.as_deref()),
                blast_cfg.downgrade,
                &matched,
            );
            if let Some(effective) = effective {
                normalized_size = Some(effective);
            }
            if let Some(announce) = announce {
                eprintln!("{announce}");
            }
        }
    }

    // 9. Plan pointer: explicit --plan-path must resolve; a node-bound plan
    // is back-filled when it resolves too.
    let plan_path = resolve_plan_pointer_stage(args, dispatch_node.as_ref(), &cwd)?;

    // 10. Scope denominator: a plan-less code node states its own scope.
    let mut deliverables = args.deliverables;
    let mut derived = false;
    if deliverables.is_none()
        && plan_path.as_deref().map_or(true, |p| p.trim().is_empty())
        && dispatch_node
            .as_ref()
            .map(|n| n.get("domain").and_then(|d| d.as_str()) == Some("code"))
            .unwrap_or(false)
    {
        let title = node_str(dispatch_node.as_ref(), "title");
        let details = node_str(dispatch_node.as_ref(), "details");
        let count = crate::target_denominator::derive_deliverables(&title, &details);
        deliverables = Some(count);
        derived = true;
        eprintln!(
            "fno do target init: no plan bound, so the deliverables count is \
             derived from the node's own details: {count}. Pass --deliverables N \
             to declare a different count."
        );
    }

    // 11. First-bind the graph pointer: a node input with no --plan-path
    // binds the node's own plan_path when it resolves; an explicit
    // --plan-path first-binds the graph node.
    if let Some(node) = dispatch_node.as_ref() {
        if let Some(plan) = &plan_path {
            bind_node_plan_path(node, plan);
        }
    }

    // 12. Build the exact helper environment and run the bash helper. The
    // gate stamp rides on whether a node resolved (see step 7).
    let code = run_helper(
        args,
        normalized_size.as_deref(),
        deliverables,
        dispatch_node.is_none(),
        binding.as_ref(),
    )?;

    // 13. Post steps on success: orientation report, lane reconcile,
    // denominator event. A failure here never changes the exit code.
    if code == 0 {
        print_orientation_report(&cwd);
        maybe_reconcile_lane_slot(&cwd);
        record_denominator_choice(
            plan_path.as_deref(),
            deliverables,
            dispatch_node.as_ref(),
            derived,
        );
        if args.beastmode {
            warn_if_authority_not_granted(&cwd);
        }
    }
    Ok(code)
}

/// The env-hint order `resolve_plugin_script` documents: CLAUDE_PLUGIN_ROOT,
/// FNO_REPO_ROOT, the persisted plugin-root pointer, then the running repo.
/// Both hints are authoritative: when set, the script is expected there and
/// we do NOT fall through.
/// The init script's resolution order: an explicit plugin root, then an
/// explicit repo root, then the install pointer beside HOME, then the cwd
/// repo's toplevel, then the bare relative path (the helper-exists check in
/// step 4 turns that into the plugin refusal).
fn resolve_init_script_from(
    plugin_root: Option<&str>,
    repo_root: Option<&str>,
    home: Option<&str>,
    toplevel: Option<&str>,
) -> PathBuf {
    const REL: &str = "hooks/helpers/init-target-state.sh";
    if let Some(root) = plugin_root {
        return PathBuf::from(root).join(REL);
    }
    if let Some(root) = repo_root {
        return PathBuf::from(root).join(REL);
    }
    if let Some(home) = home {
        let pointer = PathBuf::from(home).join(".fno/install/plugin-root");
        if let Ok(root) = std::fs::read_to_string(&pointer) {
            return PathBuf::from(root.trim()).join(REL);
        }
    }
    if let Some(root) = toplevel {
        if !root.is_empty() {
            return PathBuf::from(root).join(REL);
        }
    }
    PathBuf::from(REL)
}

fn resolve_init_script() -> PathBuf {
    let toplevel = || -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }();
    resolve_init_script_from(
        std::env::var("CLAUDE_PLUGIN_ROOT").ok().as_deref(),
        std::env::var("FNO_REPO_ROOT").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        toplevel.as_deref(),
    )
}

#[cfg(test)]
mod init_script_tests {
    use super::resolve_init_script_from;

    #[test]
    fn plugin_root_wins_over_every_other_hint() {
        let p =
            resolve_init_script_from(Some("/plugin"), Some("/repo"), Some("/home"), Some("/repo"));
        assert!(p.starts_with("/plugin"));
        assert!(p.ends_with("hooks/helpers/init-target-state.sh"));
    }

    #[test]
    fn repo_root_beats_home_and_toplevel() {
        let p = resolve_init_script_from(None, Some("/repo"), Some("/home"), Some("/other"));
        assert!(p.starts_with("/repo"));
    }

    #[test]
    fn toplevel_is_the_last_hint_before_the_bare_relative() {
        let from_toplevel = resolve_init_script_from(None, None, None, Some("/repo"));
        assert!(from_toplevel.starts_with("/repo"));
        // An empty toplevel behaves like no answer at all: both fall through
        // to the bare relative path.
        let expected = resolve_init_script_from(None, None, None, None);
        assert_eq!(
            expected,
            resolve_init_script_from(None, None, None, Some(""))
        );
    }
}

fn node_str(node: Option<&serde_json::Value>, key: &str) -> String {
    node.and_then(|n| n.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn run_helper(
    args: &Args,
    size: Option<&str>,
    deliverables: Option<i64>,
    gates_off: bool,
    binding: Option<&crate::backlog::target_binding::Receipt>,
) -> Result<i32, Fail> {
    let script_path = resolve_init_script();
    let mut cmd = Command::new("bash");
    cmd.arg(&script_path);
    // The helper inherits the caller's environment; the decision layer only
    // ADDS the contract below. An inherited TARGET_BEASTMODE must never
    // self-grant (spawns inherit the parent env wholesale), so the sole
    // authority is the flag.
    if gates_off {
        cmd.env("FNO_TARGET_INIT_GATED", "1");
    }
    cmd.env("TARGET_START", "1");
    cmd.env(
        "FNO_TARGET_BINDING",
        binding.map(|r| r.verdict).unwrap_or(""),
    );
    cmd.env(
        "TARGET_ADOPTED_PR",
        binding
            .and_then(|r| r.pr)
            .map(|pr| pr.to_string())
            .unwrap_or_default(),
    );
    // Change D: resolve `attended` from the substrate before the bash
    // manifest writer runs. An explicit TARGET_UNATTENDED still always wins.
    if std::env::var_os("TARGET_UNATTENDED").is_none() && substrate_marks_unattended() {
        cmd.env("TARGET_UNATTENDED", "1");
    }
    if let Some(input) = &args.input {
        cmd.env("TARGET_INPUT", input);
    }
    if let Some(plan) = &args.plan_path {
        cmd.env("TARGET_PLAN_PATH", plan);
    }
    if let Some(size) = size {
        cmd.env("TARGET_SIZE", size);
    }
    if let Some(model) = &args.model {
        if !model.is_empty() {
            cmd.env("TARGET_DISPATCH_MODEL", model);
        }
    }
    // The env var and the manifest field keep their spelling: both are read
    // by other processes and are a later wave's to move.
    if let Some(harness) = &args.harness {
        if !harness.is_empty() {
            cmd.env("TARGET_DISPATCH_PROVIDER", harness);
        }
    }
    if let Some(n) = deliverables {
        cmd.env("TARGET_DELIVERABLES", n.to_string());
    }
    // Sole authority: an inherited TARGET_BEASTMODE must never self-grant.
    cmd.env("TARGET_BEASTMODE", if args.beastmode { "1" } else { "" });
    // Asymmetric on purpose: --no-merge SETS the refusal but never clears an
    // inherited one, so a stray flag omission cannot upgrade a run to
    // mergeable.
    if args.no_merge {
        cmd.env("TARGET_NO_MERGE", "1");
    }
    // The real --steps flag: the stop gate's steps mode, stamped for the
    // helper process. No environment variable ahead of it.
    if args.steps {
        cmd.env("TARGET_TURN_MODE", "steps");
    }
    // Stamp the launcher's process-bound harness proof for the script and the
    // owned-identity verb it spawns. The pair is validated natively (a live
    // pid plus a known harness name); an inherited pair forwards as-is.
    let status = cmd.status();
    match status {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(exc) => Err(refuse(format!(
            "fno do target init: could not run the init helper ({}): {exc}",
            script_path.display()
        ))),
    }
}

/// The substrate read `env_marks_unattended` spells: a spawn stamp (FNO_AGENT_SELF
/// present, or FNO_BG truthy by value) with a substrate that is not an
/// operator-attendable one (`pane` is the only attended substrate) reads
/// unattended; an UNKNOWN substrate on a spawned worker reads unattended
/// too, the documented fail-toward-skip.
fn substrate_verdict_reads_unattended(
    agent_self: Option<&str>,
    bg: Option<&str>,
    substrate: Option<&str>,
) -> bool {
    let spawned = agent_self.map_or(false, |v| !v.trim().is_empty())
        || matches!(
            bg.unwrap_or_default().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    if !spawned {
        return false;
    }
    substrate.map_or(true, |s| s.trim() != "pane")
}

fn substrate_marks_unattended() -> bool {
    substrate_verdict_reads_unattended(
        std::env::var("FNO_AGENT_SELF").ok().as_deref(),
        std::env::var("FNO_BG").ok().as_deref(),
        std::env::var("FNO_AGENT_SUBSTRATE").ok().as_deref(),
    )
}

#[cfg(test)]
mod substrate_verdict_tests {
    use super::substrate_verdict_reads_unattended as verdict;

    #[test]
    fn pane_is_the_only_attended_substrate() {
        assert!(!verdict(Some("target-1"), None, Some("pane")));
        assert!(verdict(Some("target-1"), None, Some("thread")));
        assert!(verdict(Some("target-1"), None, Some("headless")));
        assert!(verdict(Some("target-1"), None, None));
        assert!(verdict(Some("target-1"), None, Some("  ")));
    }

    #[test]
    fn unspawned_env_is_attended_regardless() {
        assert!(!verdict(None, None, Some("thread")));
        assert!(!verdict(None, Some("0"), Some("thread")));
        assert!(!verdict(Some("  "), None, Some("headless")));
    }

    #[test]
    fn bg_truthy_spawns_too() {
        assert!(verdict(None, Some("1"), Some("thread")));
        assert!(verdict(None, Some("yes"), None));
        assert!(!verdict(None, Some("off"), Some("thread")));
    }
}

/// The repo root the blast read anchors relative paths against; `None` when
/// git cannot answer, which degrades the path normalization to its logical
/// fallback instead of failing the run.
pub(crate) fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// Change A: print the resolved situation report as init's first orientation
/// output. Reads the just-written manifest; strictly read-only and fully
/// non-fatal, exactly like the Python twin it replaces.
fn print_orientation_report(cwd: &Path) {
    let root = repo_root_of(cwd).unwrap_or_else(|| cwd.to_path_buf());
    println!("{}", crate::target_orient::report(&root));
}

/// Bind a parallel-mode lane slot to THIS worker's lifecycle (LD#8). The
/// manifest read is the pre-space checkout path the Python twin used, so a
/// space-regime run no-ops here the same way it always has. Strictly
/// non-fatal: never affects the init exit code the caller propagates.
fn maybe_reconcile_lane_slot(cwd: &Path) {
    let root = repo_root_of(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let Ok(text) = std::fs::read_to_string(root.join(".fno").join("target-state.md")) else {
        return;
    };
    let line = match text
        .lines()
        .find(|l| l.trim_start().starts_with("graph_node_id:"))
    {
        Some(line) => line,
        None => return,
    };
    let node_id = line
        .split_once(':')
        .map(|(_, v)| v.trim().trim_matches(['"', '\'']))
        .unwrap_or("");
    if node_id.is_empty() || node_id == "null" {
        return;
    }
    let (pid, _harness) = crate::spawn_context::session_identity_ambient(std::process::id());
    let _ = crate::lanes::reconcile_lane_slot(node_id, pid, None);
}

/// Emit a `target_denominator` event for the deliverables-1 ratio.
/// Best-effort: a recording failure never fails a successful init. A declared
/// `--deliverables` exit stays the load-bearing bypass risk; a derived count
/// records as its own kind so it never reads as a declaration in the ratio.
fn record_denominator_choice(
    plan: Option<&str>,
    deliverables: Option<i64>,
    node: Option<&serde_json::Value>,
    derived: bool,
) {
    use serde_json::json;
    let denominator = if plan.map(|p| !p.trim().is_empty()).unwrap_or(false) {
        "plan"
    } else if derived {
        "derived"
    } else if deliverables.is_some() {
        "deliverables"
    } else {
        "none"
    };
    let mut data = serde_json::Map::new();
    data.insert("denominator".into(), json!(denominator));
    if let Some(count) = deliverables {
        data.insert("count".into(), json!(count));
    }
    if let Some(node) = node {
        let title = node.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let details = node.get("details").and_then(|v| v.as_str()).unwrap_or("");
        data.insert(
            "enumerated".into(),
            json!(crate::target_denominator::enumerated_scope(title, details)),
        );
    }
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let event = json!({
        "ts": ts,
        "type": "target_denominator",
        "source": "target",
        "data": data,
    });
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let events = crate::paths::events_path(&root);
    if let Err(exc) = crate::event_store::append_envelope(&events, &event.to_string(), None) {
        // A measurement failure must not fail init; say where it went.
        eprintln!("fno do target init: denominator event not recorded: {exc}");
    }
}

/// Name a --beastmode that did not take. The verdict comes from the orient
/// module's own predicate, never a string match, so this warning and the
/// `attended` line can never disagree about whether the grant holds.
fn warn_if_authority_not_granted(cwd: &Path) {
    let root = repo_root_of(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let raw = crate::target_orient::read_manifest(&root);
    if crate::target_orient::authority_granted(&raw) {
        return;
    }
    let stamped = raw
        .as_ref()
        .and_then(|m| m.get("authority"))
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
        == Some("full");
    if stamped {
        eprintln!(
            "--beastmode was stamped but has NOTHING LIVE TO ANCHOR IT - this \
             session will not act with authority.\nAuthority needs a LIVE \
             CLAIM: a free-text run claims no node, and a stale claim proves \
             nothing, so an abandoned session would be indistinguishable from \
             a live one. The grant is refused rather than left to outlive its \
             session.\nRe-run against a backlog node (`fno do target start \
             --beastmode <node>`), or continue without authority."
        );
        return;
    }
    let fix = if raw.is_some() {
        "The manifest is write-once and one already existed, so the flag was a \
         no-op. To run with authority, finish or cancel this session first \
         (`/fno:cancel-target`), then start a fresh one."
    } else {
        "No manifest was written, so nothing consumed the flag. Run \
         `fno do target init --beastmode --input <node>` to claim this session \
         with a grant."
    };
    eprintln!("--beastmode did NOT take - this session has no authority grant.\n{fix}");
}

/// The shared graph rows read. One loader, both consumers: this module's
/// dispatch-node resolution and the orient report's node and boundary lines.
pub(crate) fn graph_rows(cwd: &Path) -> Option<Vec<serde_json::Value>> {
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
    let store = crate::backlog::api::Store::new(&graph_path);
    crate::graph_store::read_rows_where(
        &store.graph,
        &crate::backlog::RowQuery {
            with_blockers: true,
            ..Default::default()
        },
    )
    .ok()
}

/// The node-token shape that can name a backlog node: canonical or compact
/// prefix/hex, bare hex, or kebab slug. Spaced free text cannot name one.
/// Shared with the start port, which runs the same unreadable-graph gate.
pub(crate) fn input_may_name_a_node(text: &str) -> bool {
    let node_token = Regex::new(r"(?i)^[a-z][a-z0-9]{0,7}-?[0-9a-f]{4,8}$").unwrap();
    let bare_hex = Regex::new(r"^[0-9a-fA-F]{4,8}$").unwrap();
    let kebab = Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)+$").unwrap();
    text.split_whitespace()
        .any(|tok| node_token.is_match(tok) || bare_hex.is_match(tok) || kebab.is_match(tok))
}

/// `~` expansion.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// The node the input names, exact ids only. Free text and ambiguous matches
/// resolve to None, exactly like the Python resolver. A plan path matches
/// graph rows whose plan_path canonicalizes to the same file; a plan shared
/// by a delivery unit and its contained children narrows to the units. The
/// start port reuses this for its pre-ensure hold gate. Like the Python
/// resolver's default, entries=None reads the graph here, so a caller with
/// no resolution step of its own still gets a resolved node to hold-check.
pub(crate) fn resolve_dispatch_node(
    input: Option<&str>,
    plan_path: Option<&str>,
    entries: Option<&[serde_json::Value]>,
) -> Option<serde_json::Value> {
    let owned;
    let entries: &[serde_json::Value] = match entries {
        Some(rows) => rows,
        None => {
            owned = graph_rows(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            owned.as_deref()?
        }
    };
    let node_token = Regex::new(r"(?i)^[a-z][a-z0-9]{0,7}-?[0-9a-f]{4,8}$").unwrap();
    let tokens: Vec<String> = input
        .unwrap_or("")
        .split_whitespace()
        .filter(|t| node_token.is_match(t))
        .map(|t| t.to_lowercase())
        .collect();
    if !tokens.is_empty() {
        let mut matches: Vec<&serde_json::Value> = Vec::new();
        for entry in entries {
            if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                if tokens.contains(&id.to_lowercase()) {
                    matches.push(entry);
                }
            }
        }
        return if matches.len() == 1 {
            Some(matches[0].clone())
        } else {
            None
        };
    }
    let plan = plan_path?;
    let wanted = std::fs::canonicalize(expand_tilde(plan)).ok()?;
    let mut matches: Vec<&serde_json::Value> = Vec::new();
    for entry in entries {
        let candidate = match entry.get("plan_path").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };
        if let Ok(resolved) = std::fs::canonicalize(expand_tilde(candidate)) {
            if resolved == wanted {
                matches.push(entry);
            }
        }
    }
    if matches.len() > 1 {
        let units: Vec<&serde_json::Value> = matches
            .iter()
            .copied()
            .filter(|e| node_str(Some(e), "contained_in").is_empty())
            .collect();
        if units.len() == 1 {
            return Some(units[0].clone());
        }
        if units.is_empty() {
            let owners: std::collections::HashSet<String> = matches
                .iter()
                .map(|e| node_str(Some(e), "contained_in"))
                .collect();
            if owners.len() == 1 {
                return Some(matches[0].clone());
            }
        }
    }
    if matches.len() == 1 {
        Some(matches[0].clone())
    } else {
        None
    }
}

/// The plan-pointer stage: an explicit --plan-path must resolve to a
/// readable plan (fail closed before the write-once manifest exists); a node
/// input with no --plan-path back-fills the node's bound plan when THAT
/// resolves, and refuses when a named-but-dangling pointer would otherwise
/// close the only legal repair route.
fn resolve_plan_pointer_stage(
    args: &Args,
    node: Option<&serde_json::Value>,
    cwd: &Path,
) -> Result<Option<String>, Fail> {
    if let Some(explicit) = &args.plan_path {
        if !explicit.is_empty() {
            return match resolve_plan_pointer(explicit, cwd) {
                Some(bound) => Ok(Some(bound)),
                None => Err(refuse(format!(
                    "fno do target init: REFUSING because the explicit --plan-path \
                     does not resolve to a readable plan ({explicit}). \
                     Correct --plan-path and retry."
                ))),
            };
        }
    }
    if args.plan_path.is_some() {
        return Ok(args.plan_path.clone());
    }
    let Some(node) = node else {
        return Ok(None);
    };
    let bound_plan = node
        .get("plan_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if bound_plan.is_empty() {
        return Ok(None);
    }
    match resolve_plan_pointer(&bound_plan, cwd) {
        Some(bound) => Ok(Some(bound)),
        None => Err(refuse(format!(
            "fno do target init: REFUSING because the node's bound plan_path \
             does not resolve to a readable plan ({bound_plan}). \
             Repair the node with: fno backlog update <id> --plan-path <path>"
        ))),
    }
}

/// The plan pointer to bind, NORMALIZED, or None when it does not resolve.
/// `~` is expanded here (bash never expands a value), a `#group-N` anchor is
/// preserved on the way out, a relative pointer resolves against the repo
/// root, and a plan DIRECTORY resolves through its `00-INDEX.md` entry point
/// before the pointer is admitted.
fn resolve_plan_pointer(plan_path: &str, cwd: &Path) -> Option<String> {
    let (bare, anchor) = match plan_path.split_once('#') {
        Some((b, a)) => (b.trim().to_string(), Some(a.to_string())),
        None => (plan_path.trim().to_string(), None),
    };
    if bare.is_empty() {
        return None;
    }
    let mut resolved = expand_tilde(&bare);
    if !resolved.is_absolute() {
        let root = repo_root_of(cwd)?;
        resolved = root.join(resolved.strip_prefix("/").unwrap_or(&resolved));
    }
    let read_target = plan_entry_point(&resolved)?;
    std::fs::read_to_string(&read_target).ok()?;
    let mut out = resolved.to_string_lossy().to_string();
    if let Some(a) = anchor {
        out.push('#');
        out.push_str(&a);
    }
    Some(out)
}

/// A footnote plan can be a file or a directory whose entry point is
/// `00-INDEX.md`. The file whose READ proves the pointer.
fn plan_entry_point(resolved: &Path) -> Option<PathBuf> {
    if resolved.is_dir() {
        let index = resolved.join("00-INDEX.md");
        return index.is_file().then_some(index);
    }
    Some(resolved.to_path_buf())
}

/// The repo root of `cwd` (git), or None.
fn repo_root_of(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// First-bind the graph node's plan_path at init. Never overwrites a
/// non-empty stored value (a group child shares one plan); non-fatal on
/// failure, like the shell hook's lock-stamp posture. Rides the sibling
/// binary's own update verb with stdio captured, the way the Python verb
/// shelled `fno backlog update`.
fn bind_node_plan_path(node: &serde_json::Value, plan_path: &str) {
    let node_id = node_str(Some(node), "id");
    if node_id.trim().is_empty() {
        return;
    }
    let stored = node
        .get("plan_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !stored.is_empty() {
        let same = match (
            std::fs::canonicalize(expand_tilde(&stored)),
            std::fs::canonicalize(expand_tilde(plan_path)),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => stored == plan_path,
        };
        if !same {
            eprintln!(
                "fno do target init: graph node {node_id} is already bound to \
                 {stored}; leaving it untouched"
            );
        }
        return;
    }
    let Ok(self_exe) = std::env::current_exe() else {
        return;
    };
    let result = Command::new(self_exe)
        .args([
            "backlog",
            "update",
            &node_id,
            &format!("plan_path={plan_path}"),
        ])
        .stdin(Stdio::null())
        .output();
    match result {
        Ok(out) if out.status.success() => {
            eprintln!("fno do target init: bound graph node {node_id} to {plan_path}");
        }
        Ok(out) => {
            let detail = if out.stderr.is_empty() {
                String::from_utf8_lossy(&out.stdout).to_string()
            } else {
                String::from_utf8_lossy(&out.stderr).to_string()
            };
            let detail: String = detail.trim().chars().take(200).collect();
            eprintln!("WARNING: could not bind {plan_path} to {node_id}: {detail}");
        }
        Err(exc) => {
            eprintln!("WARNING: could not bind {plan_path} to {node_id}: {exc}");
        }
    }
}
