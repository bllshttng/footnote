//! `fno do target start`: the native port of the Python verb. One-verb
//! cold-start: resolve the node, run the pre-allocation gates (hold,
//! containment, foreign holder), `fno worktree ensure` the isolated tree,
//! bind an existing manifest (idempotent re-run / successor takeover), then
//! call the native init by its door with the flags forwarded - `--steps`
//! included, which the Python verb never carried (the driver set
//! `TARGET_TURN_MODE` inline; ruling d-b7ef7d2d puts the flag here).
//!
//! The parity contract is `tests/golden/target_start/`, captured from the
//! Python leg before its deletion, asserted by `tests/target_start_parity.rs`
//! through the `target-start` door. Every receipt, refusal and timeout keeps
//! the Python text; the 540s deadline and its per-stage grace keep their
//! values. The claim-acquiring leg stays the init helper's (one acquire per
//! session, unchanged); this module only reads claims, plus the one
//! successor re-acquire path.
//!
//! Known port degradations, each fail-closed (documented for the review):
//! - `_classify_worktree_occupancy`'s transcript leg (`fno.agents.watchdog`
//!   tail facts) has no native twin, so a linked worktree whose dirt is
//!   stale-but-unreapable answers `unknown` and refuses takeover instead of
//!   reading transcript engagement.
//! - The codex-desktop handoff's worktree-policy read walks the same config
//!   candidate order as `backlog::settings` (env > global > default) without
//!   Python's `_match_project_entry` slug machinery; an unreadable or
//!   invalid policy answers "no native handoff", the direction the Python
//!   caller already takes on a policy error.

use regex::Regex;
use serde_json::{json, Value};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use crate::backlog::target_binding::{self, Phase, Request};
use crate::paths::AgentsHome;

/// Below the Claude Bash tool's 600s call cap, well below the 10+ minute
/// stalls this bound targets. Keep the Python value.
const START_DEADLINE_S: u64 = 540;
/// A stage's process group dies this long before the watchdog would.
const STAGE_GRACE_S: u64 = 15;
/// Bounds the setup hook well below the 10+ minute cleanup-leg stalls.
const SETUP_HOOK_TIMEOUT_S: u64 = 120;
const CODEX_NATIVE_HANDOFF_EXIT: i32 = 75;

const USAGE: &str = "usage: fno do target start <node|text> [--plan-path <path>] \
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
        code: 1,
    }
}

fn fail_code(message: impl Into<String>, code: i32) -> Fail {
    Fail {
        message: message.into(),
        code,
    }
}

/// The parsed verb surface. The node is the one positional.
struct Args {
    node: String,
    plan_path: Option<String>,
    size: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    beastmode: bool,
    no_merge: bool,
    deliverables: Option<i64>,
    steps: bool,
}

fn parse_args(rest: &[String]) -> Result<Args, Fail> {
    let mut node: Option<String> = None;
    let mut plan_path = None;
    let mut size = None;
    let mut model = None;
    let mut harness = None;
    let mut beastmode = false;
    let mut no_merge = false;
    let mut deliverables = None;
    let mut steps = false;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                print!(
                    "\nCold-start a worktree-isolated target session in ONE verb.\n\n\
                     --steps  run this target one instructed step at a time; the\n\
                     \x20        flag forwards to init, which stamps turn_mode.\n"
                );
                return Err(Fail {
                    message: String::new(),
                    code: 0,
                });
            }
            "--plan-path" => plan_path = Some(next_value(&mut it, tok)?),
            "--size" => size = Some(next_value(&mut it, tok)?),
            "--model" | "-m" => model = Some(next_value(&mut it, tok)?),
            "--harness" | "-H" => harness = Some(next_value(&mut it, tok)?),
            // Retired spelling: the harness axis is --harness/-H. A value is
            // consumed so the error names the axis; any non-empty value refuses.
            "--provider" => {
                let value = next_value(&mut it, tok)?;
                if !value.trim().is_empty() {
                    return Err(fail_code(
                        "--provider was split at the axis rename: the CLI binary is \
                         --harness/-H; a model vendor is only routable at spawn \
                         (`fno agents spawn --provider <vendor> --model <m>`).",
                        2,
                    ));
                }
            }
            "--beastmode" | "--beast" => beastmode = true,
            "--no-merge" => no_merge = true,
            "--deliverables" => {
                let raw = next_value(&mut it, tok)?;
                let parsed: i64 = raw.trim().parse().map_err(|_| {
                    fail_code(
                        format!(
                            "fno do target start: invalid --deliverables {raw:?}; \
                             expected a positive integer."
                        ),
                        2,
                    )
                })?;
                deliverables = Some(parsed);
            }
            "--steps" => steps = true,
            other => {
                if other.starts_with("--") {
                    return Err(usage_fail(&format!("unknown flag {other}")));
                }
                if node.is_some() {
                    return Err(usage_fail(&format!(
                        "unexpected extra argument {other:?}; the verb takes one node"
                    )));
                }
                node = Some(other.to_string());
            }
        }
    }
    let node = match node {
        Some(n) if !n.trim().is_empty() => n,
        _ => return Err(usage_fail("the verb takes a node id or feature text")),
    };
    Ok(Args {
        node,
        plan_path,
        size,
        model,
        harness,
        beastmode,
        no_merge,
        deliverables,
        steps,
    })
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
        message: format!("usage: fno do target start: {detail}\n{USAGE}"),
        code: 2,
    }
}

/// The public entry: the door hands the argv after `do target start`.
pub fn run(rest: &[String]) -> i32 {
    let args = match parse_args(rest) {
        Ok(a) => a,
        Err(f) => return finish_fail(&f),
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(START_DEADLINE_S);
    match run_body(&args, deadline) {
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

fn run_body(args: &Args, deadline: Instant) -> Result<i32, Fail> {
    let mut node = args.node.clone();
    let mut plan_path = args.plan_path.clone();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Bare id: init binds. Scope may fork a child, so a held parent refuses
    // first - only for a multi-word input (a scope) or an explicit in-review
    // allowance, exactly as the Python body gated it.
    let allow_in_review = std::env::var("TARGET_ALLOW_IN_REVIEW").as_deref() == Ok("1");
    if node.split_whitespace().count() > 1 || allow_in_review {
        let dispatch_node =
            crate::target_init::resolve_dispatch_node(Some(&node), plan_path.as_deref(), None);
        refuse_dispatch_hold(dispatch_node.as_ref(), None)?;
        let receipt = target_binding::prepare(
            &Request {
                input: node.clone(),
                node: None,
                plan_path: plan_path.clone(),
                phase: Phase::Start,
                allow_in_review,
            },
            &cwd,
        );
        if !receipt.message.is_empty() {
            eprintln!("{}", receipt.message);
        }
        if receipt.verdict == "refused" || (receipt.verdict == "forked" && is_linked_worktree(&cwd))
        {
            if let Some(next) = &receipt.next {
                eprintln!("next: {next}");
            }
            return Err(Fail {
                message: String::new(),
                code: receipt.exit_code(),
            });
        }
        if receipt.verdict == "forked" {
            node = receipt.effective_node.unwrap_or(node);
            plan_path = receipt.effective_plan.or(plan_path);
        }
    }

    // Boundary: already isolated -> no-op, create nothing. But first refuse
    // if a DIFFERENT live session holds this node's claim: this cwd is that
    // session's worktree and sharing its git index corrupts the build.
    if is_linked_worktree(&cwd) {
        let node_id = resolve_node_id(&node, None);
        if let Some((_, _, Some(holder))) = classify_holder(&node_id) {
            print_foreign_holder_park(&node_id, &holder, &cwd);
            return Err(Fail {
                message: String::new(),
                code: 1,
            });
        }
        if let Some(canonical) = codex_native_repo(&cwd) {
            return start_codex_native(
                &canonical,
                &cwd,
                &node_id,
                args,
                plan_path.as_deref(),
                deadline,
            );
        }
        if under_codex_worktrees(&cwd) {
            return Err(refuse(
                "fno do target start: this linked worktree is under CODEX_HOME, but \
                 native ownership could not be verified for the current Desktop \
                 thread and canonical repository; refusing to no-op or claim it.",
            ));
        }
        println!("already isolated at {}; nothing created.", cwd.display());
        // Receipt-only here: nothing branches off the base on this path, so an
        // unresolvable remote degrades to the in-place spelling instead of
        // refusing a bind the tree does not need the ref for.
        let base_label = remote_base_ref(&cwd, false).unwrap_or_else(|| "in-place".to_string());
        if bind_worktree(
            &node_id,
            &cwd,
            &base_label,
            false,
            false,
            args.beastmode,
            args.no_merge,
            args.steps,
        )? {
            return Ok(0);
        }
        // No manifest in this tree (a stranded session): bind the EXISTING
        // tree instead of creating one - skip ensure and setup entirely.
        return init_from_worktree(
            &node_id,
            &cwd,
            &base_label,
            false,
            false,
            "",
            args,
            plan_path.as_deref(),
            deadline,
        );
    }

    let repo_root_s = git_out(&cwd, &["rev-parse", "--show-toplevel"]);
    let Some(repo_root_s) = repo_root_s else {
        return Err(refuse(format!(
            "fno do target start: {} is not a git repository.",
            cwd.display()
        )));
    };
    let repo_root = PathBuf::from(repo_root_s);
    // One graph read feeds the slug resolution, the find, and the hold gate.
    // An unreadable graph on a node-shaped argument refuses; feature text
    // keeps its proceed path.
    let entries = crate::target_init::graph_rows(&cwd);
    if entries.is_none() && crate::target_init::input_may_name_a_node(&node) {
        return Err(fail_code(
            "fno do target start: dispatch-hold-invalid: backlog graph is \
             unreadable; refusing to assume unheld. Nothing was claimed. Fix or \
             restore graph.json and retry.",
            2,
        ));
    }
    let node = resolve_node_id(&node, entries.as_deref());
    let start_node = entries.as_deref().and_then(|rows| {
        rows.iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(node.as_str()))
            .cloned()
    });
    refuse_dispatch_hold(start_node.as_ref(), entries.as_deref())?;
    // Redirect a contained node BEFORE `worktree ensure`: by init's gate the
    // worktree and branch would already exist beside the refusal.
    redirect_if_contained(start_node.as_ref(), entries.as_deref())?;
    let name = wt_name(&node);

    // Codex Desktop owns the only supported native worktree transition; the
    // retry from the registered linked worktree ran in the branch above. The
    // handoff receipt always exits 75, so nothing past this block runs native.
    if codex_desktop_handoff_policy(&repo_root) {
        if let Some((_, _, Some(holder))) = classify_holder(&node) {
            print_foreign_holder_park(&node, &holder, &repo_root);
            return Err(Fail {
                message: String::new(),
                code: 1,
            });
        }
        let Some(native_base) = remote_base_ref(&repo_root, true) else {
            return Err(Fail {
                message: String::new(),
                code: 1,
            });
        };
        let base = base_receipt(&repo_root, &native_base)?;
        request_codex_native_handoff(&repo_root, &node, &base, deadline);
    }

    // 1. Create/reuse the worktree off origin/main via the ensure verb (the
    //    one create/reuse path). Forward the current session's harness so a
    //    claude cold-start lands harness-native; no ambient marker omits it
    //    and ensure degrades to the external base. Plain `fno` off PATH,
    //    never `fno-py`.
    let ambient_harness = crate::claims::resolve_identity().1;
    let mut ensure_cmd = Command::new("fno");
    ensure_cmd
        .args(["worktree", "ensure", "--repo"])
        .arg(&repo_root)
        .args(["--name", &name]);
    if let Some(harness) = ambient_harness.filter(|h| !h.is_empty()) {
        ensure_cmd.args(["--harness", &harness]);
    }
    let ens =
        crate::bounded_cmd::output_with_timeout_result(ensure_cmd, stage_timeout_secs(deadline));
    let Ok(ens) = ens else {
        return Err(refuse(
            "fno do target start: worktree ensure failed (step: ensure): could not run fno",
        ));
    };
    if ens.status.code().is_none() {
        stage_timeout_exit("ensure", "Nothing was claimed.");
    }
    let ens_out = String::from_utf8_lossy(&ens.stdout).trim().to_string();
    let ens_err = String::from_utf8_lossy(&ens.stderr).to_string();
    if !ens.status.success() || ens_out.is_empty() {
        return Err(refuse(format!(
            "fno do target start: worktree ensure failed (step: ensure): {}",
            if ens_err.trim().is_empty() {
                "no path on stdout"
            } else {
                ens_err.trim()
            }
        )));
    }
    let wt_path = PathBuf::from(&ens_out);
    // Whether the tree is THIS invocation's creation decides the init-failure
    // receipt: ensure stamps created=true|false; an installed ensure that
    // predates the token is read through its reuse wording instead.
    let created_this_run = match Regex::new(r"\bcreated=(true|false)\b")
        .ok()
        .and_then(|re| re.captures(&ens_err))
    {
        Some(c) => c.get(1).map_or(false, |m| m.as_str() == "true"),
        None => !ens_err.contains("reusing"),
    };
    // ensure names the branch's provenance (continued/salvaged/fresh); a
    // re-dispatched worker must see it is continuing.
    let from_note = Regex::new(r" base=(\S+)")
        .ok()
        .and_then(|re| re.captures(&ens_err))
        .and_then(|c| c.get(1))
        .map(|m| format!("  from={}", m.as_str()))
        .unwrap_or_default();

    // policy=never: ensure returned the repo main checkout itself. Init still
    // runs, in place.
    let in_place = resolve_soft(&wt_path) == resolve_soft(&repo_root);

    // 2. Link the remaining shared non-fno state (the harness config dirs,
    //    the vault) via the canonical setup hook. Non-fatal: name a failure,
    //    never abort.
    if !in_place {
        let (rc, tail) = run_setup_worktree_hook(&repo_root, &wt_path);
        if rc != 0 && rc != -1 {
            eprintln!("fno do target start: setup-worktree.sh exited {rc} (non-fatal): {tail}");
        }
    }

    let base_label = if in_place {
        "in-place".to_string()
    } else {
        match remote_base_ref(&repo_root, false) {
            Some(b) => b,
            None => {
                return Err(Fail {
                    message: String::new(),
                    code: 1,
                })
            }
        }
    };

    // Bind an existing manifest (idempotent re-run: never double-claim or
    // error) or fall through to a fresh init against this tree.
    if bind_worktree(
        &node,
        &wt_path,
        &base_label,
        in_place,
        !in_place,
        args.beastmode,
        args.no_merge,
        args.steps,
    )? {
        return Ok(0);
    }
    // 3. Init the session FROM the worktree (binds owner_cwd, claims the node
    //    exactly once - the helper's one-call claim).
    init_from_worktree(
        &node,
        &wt_path,
        &base_label,
        in_place,
        created_this_run,
        &from_note,
        args,
        plan_path.as_deref(),
        deadline,
    )
}

fn stage_timeout_secs(deadline: Instant) -> u64 {
    let left = deadline
        .checked_duration_since(Instant::now())
        .map(|d| d.saturating_sub(std::time::Duration::from_secs(STAGE_GRACE_S)))
        .unwrap_or_default();
    std::cmp::max(1, left.as_secs())
}

/// The Python `_stage_timeout_exit` spelling; typer.Exit(124).
fn stage_timeout_exit(step: &str, extra: &str) -> ! {
    eprintln!(
        "fno do target start: step: {step} exceeded the {START_DEADLINE_S}s start \
         bound; its process group was killed. {extra}"
    );
    std::process::exit(124);
}

/// `git -C <cwd> <args>` stdout, trimmed, or None (the `_git_out` twin).
fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `Path.resolve()` without the existence requirement.
fn resolve_soft(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// True when `cwd` is inside a git LINKED worktree (git-dir != common-dir):
/// the location verdict `ok` condition in pure git terms.
fn is_linked_worktree(cwd: &Path) -> bool {
    let Some(gdir) = git_out(cwd, &["rev-parse", "--git-dir"]) else {
        return false;
    };
    let Some(common) = git_out(cwd, &["rev-parse", "--git-common-dir"]) else {
        return false;
    };
    resolve_soft(&abs_against(cwd, &gdir)) != resolve_soft(&abs_against(cwd, &common))
}

fn abs_against(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Filesystem-safe worktree name from a node id/slug or feature text
/// (`_wt_name`): non-name runs become one hyphen, strip, bound at 60, strip
/// again - truncation can land on a hyphen.
fn wt_name(node: &str) -> String {
    let lowered = node.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_sep = false;
    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push('-');
            prev_sep = true;
        }
    }
    let stripped = out.trim_matches('-').to_string();
    let bounded: String = stripped.chars().take(60).collect();
    let stripped = bounded.trim_matches('-').to_string();
    if stripped.is_empty() {
        "target".to_string()
    } else {
        stripped
    }
}

/// The verified remote default-branch ref (`_remote_base_ref`). With `fetch`,
/// one refresh per (cwd, remote) per process; a fetch timeout warns and
/// carries on (the receipt's own base line reports `behind=unmeasured`), any
/// other fetch failure refuses.
fn remote_base_ref(cwd: &Path, fetch: bool) -> Option<String> {
    if fetch {
        let (ok, err) = refresh_remote(cwd, "origin");
        if !ok && err == TIMED_OUT {
            eprintln!(
                "fno do target start: remote refresh timed out; \
                 resolving the base against possibly stale refs."
            );
        } else if !ok {
            eprintln!(
                "fno do target start: could not refresh remote base: {}",
                if err.is_empty() {
                    "git fetch origin failed"
                } else {
                    &err
                }
            );
            return None;
        }
    }
    for candidate in ["origin/main", "origin/master"] {
        let spec = format!("{candidate}^{{commit}}");
        if git_out(cwd, &["rev-parse", "--verify", &spec]).is_some() {
            return Some(candidate.to_string());
        }
    }
    eprintln!("fno do target start: neither origin/main nor origin/master resolves.");
    None
}

/// Remote ref plus the actual fork commit, never a symbolic base claim.
fn base_receipt(cwd: &Path, base: &str) -> Result<String, Fail> {
    let Some(fork) = git_out(cwd, &["merge-base", "HEAD", base]) else {
        return Err(refuse(format!(
            "fno do target start: HEAD has no merge base with {base}; refusing \
             to report false base provenance."
        )));
    };
    Ok(format!("{base}@{}", &fork[..fork.len().min(12)]))
}

const TIMED_OUT: &str = "fetch timed out after 60s";

/// The Python fetch ran under `timeout=60`; keep the value.
const REMOTE_FETCH_BUDGET_S: u64 = 60;

/// Keys are `cwd|remote`. The Python twin keys the refspec too; start's two
/// callers both fetch the whole remote, so one key shape serves both sets.
fn fetch_state() -> &'static Mutex<(
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
)> {
    static STATE: std::sync::LazyLock<
        Mutex<(
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
        )>,
    > = std::sync::LazyLock::new(|| {
        Mutex::new((
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        ))
    });
    &STATE
}

/// One fetch per (repo, remote) per process, 60s bound. A timeout is not a
/// fetch verdict: it lands in the second set and callers tell the two apart.
fn refresh_remote(cwd: &Path, remote: &str) -> (bool, String) {
    let key = format!("{}|{remote}", cwd.display());
    if let Ok(state) = fetch_state().lock() {
        if state.0.contains(&key) {
            return (true, String::new());
        }
        if state.1.contains(&key) {
            return (false, TIMED_OUT.to_string());
        }
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(["fetch", "--quiet", remote]);
    let out = crate::bounded_cmd::output_with_timeout_result(cmd, REMOTE_FETCH_BUDGET_S);
    match out {
        Err(e) => (false, e.to_string()),
        Ok(o) if o.status.success() => {
            if let Ok(mut state) = fetch_state().lock() {
                state.0.insert(key);
            }
            (true, String::new())
        }
        Ok(o) if o.status.signal().is_some() => {
            // The bound killed the group and git never answered. A timeout is
            // not a fetch verdict, and the key lands in the second set so the
            // next caller of this remote pays no second 60s.
            if let Ok(mut state) = fetch_state().lock() {
                state.1.insert(key);
            }
            (false, TIMED_OUT.to_string())
        }
        Ok(o) => (false, String::from_utf8_lossy(&o.stderr).trim().to_string()),
    }
}

/// `_truthful_base`: the base label with a MEASURED distance, or an explicit
/// unmeasured marker. `measure=false` is the idempotent path's spelling: no
/// network. ONE renderer decides the shape of every base receipt.
fn truthful_base(cwd: &Path, base_label: &str, measure: bool) -> String {
    if base_label == "in-place" {
        return base_label.to_string();
    }
    if !measure {
        return unmeasured_base(base_label, "idempotent-path-does-no-network");
    }
    let bare = base_label.rsplit_once('@').map_or(base_label, |(b, _)| b);
    let Some((remote, _)) = bare.split_once('/') else {
        // No `<remote>/<branch>` shape means the label names a LOCAL ref, and
        // `HEAD..<local-branch>` is exactly the stale-ref silent zero this
        // function exists to refuse.
        return unmeasured_base(base_label, "base-names-no-remote-ref");
    };
    let (ok, err) = refresh_remote(cwd, remote);
    if !ok {
        return unmeasured_base(
            base_label,
            if err == TIMED_OUT {
                "fetch-timed-out"
            } else {
                "fetch-failed"
            },
        );
    }
    let count =
        git_out(cwd, &["rev-list", "--count", &format!("HEAD..{bare}")]).unwrap_or_default();
    if !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) {
        format!("{base_label} behind={count}")
    } else {
        unmeasured_base(base_label, "rev-list-failed")
    }
}

fn unmeasured_base(base_label: &str, why: &str) -> String {
    format!("{base_label} behind=unmeasured:{why}")
}

/// scripts/setup/setup-worktree.sh inside the new worktree; (-1, "") when the
/// script is absent (silently tolerated), 124 when its group was killed.
fn run_setup_worktree_hook(repo_root: &Path, worktree_path: &Path) -> (i32, String) {
    let script = repo_root
        .join("scripts")
        .join("setup")
        .join("setup-worktree.sh");
    if !script.exists() {
        return (-1, String::new());
    }
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .current_dir(worktree_path)
        .env("CANONICAL", repo_root)
        .env("WORKTREE", worktree_path);
    match crate::bounded_cmd::output_with_timeout_result(cmd, SETUP_HOOK_TIMEOUT_S) {
        Ok(out) if out.status.code() == Some(0) => (0, String::new()),
        Ok(out) => {
            let raw = if out.stderr.is_empty() {
                String::from_utf8_lossy(&out.stdout).to_string()
            } else {
                String::from_utf8_lossy(&out.stderr).to_string()
            };
            let tail: String = raw
                .chars()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            (out.status.code().unwrap_or(1), tail)
        }
        Err(_) => (-1, String::new()),
    }
}

// ---------------------------------------------------------------------------
// Claim classification + the bind path
// ---------------------------------------------------------------------------

/// The manifest for a worktree: the worktree slice of the space when that
/// exists, else the checkout-local `.fno/target-state.md`, else None. A
/// symlinked manifest reads as absent (the init helper owns that file).
fn worktree_manifest(wt_path: &Path) -> Option<PathBuf> {
    let space = crate::paths::worktree_space_dir(wt_path).join("target-state.md");
    if space.is_file() && !space.is_symlink() {
        return Some(space);
    }
    let legacy = wt_path.join(".fno").join("target-state.md");
    if legacy.is_file() && !legacy.is_symlink() {
        return Some(legacy);
    }
    None
}

/// The manifest's claimed `graph_node_id`, or None if absent/null/unreadable.
fn manifest_node_id(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let re = Regex::new(r"(?m)^graph_node_id\s*:\s*(.+)$").ok()?;
    let val = re.captures(&text)?.get(1)?.as_str().trim().to_string();
    let val = val.trim_matches('"').trim_matches('\'').to_string();
    if val.is_empty() || val == "null" {
        None
    } else {
        Some(val)
    }
}

/// The manifest's turn mode; None when the line is absent, which is an
/// ordinary (looping) run - the helper writes the line only for steps.
fn manifest_turn_mode(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let re = Regex::new(r"(?m)^turn_mode\s*:\s*(.+)$").ok()?;
    let val = re.captures(&text)?.get(1)?.as_str().trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// The graph node row for an exact id, or None. An archived row reads as
/// absent, exactly like the Python `_find_node` readback.
fn find_node(node_id: &str, entries: Option<&[Value]>) -> Option<Value> {
    let rows = match entries {
        Some(rows) => rows.to_vec(),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            crate::target_init::graph_rows(&cwd)?
        }
    };
    let hit = crate::backlog::node_ref::find_node(&rows, node_id)?;
    if truthy(hit.get("archived_at")) {
        return None;
    }
    Some(hit.clone())
}

/// `claim_status` for `node:<id>` as `(state, record)`. A corrupted claim
/// reads as free (a re-acquire candidate) so an unreadable claims dir never
/// wedges start - the Python probe's exception arm.
fn read_node_claim(
    node_id: &str,
) -> (
    crate::claims::ClaimState,
    Option<crate::claims::ClaimRecord>,
) {
    let (state, rec) = crate::claims::status(&format!("node:{node_id}"), None);
    if matches!(state, crate::claims::ClaimState::Corrupted) {
        return (crate::claims::ClaimState::Free, None);
    }
    (state, rec)
}

/// True iff `holder` is THIS session's identity (any claim state). Four arms,
/// mirroring init-target-state.sh's `claim_owner_id`: the driver-assigned run
/// id, the codex thread id, the resolved self identity, then durable session
/// pid + machine. An uncapturable own pid reads as not-ours (park /
/// re-acquire, never assume ownership).
fn holder_is_ours(holder: &str, rec: &crate::claims::ClaimRecord) -> bool {
    for env_var in ["TARGET_SESSION_ID", "CODEX_THREAD_ID"] {
        if let Ok(own) = std::env::var(env_var) {
            if !own.is_empty() && holder == format!("target-session:{own}") {
                return true;
            }
        }
    }
    let home = AgentsHome::from_env();
    let owned =
        crate::spawn_context::resolve_self_identity(&|k| std::env::var(k).ok(), None, None, &home);
    if let Some(sid) = owned.session_id.map(|s| s.trim().to_string()) {
        if !sid.is_empty() && holder == format!("target-session:{sid}") {
            return true;
        }
    }
    if rec.pid_unavailable {
        return false;
    }
    let (own_pid, _harness) = crate::spawn_context::session_identity_ambient(std::process::id());
    let same_machine = crate::claims::is_same_machine(&rec.host, rec.machine_id.as_deref());
    matches!(own_pid, Some(p) if rec.pid == Some(p as i32)) && same_machine
}

/// `(verdict, state word, record)` for `node:<id>` from THIS session's view:
/// verdict in {ours, foreign_live, dead_predecessor, free}. Liveness is
/// pid-based, so a busy owner whose TTL lapsed still reads live. Read-only;
/// a probe failure reads as free.
fn classify_node_claim(
    node_id: &str,
) -> (&'static str, String, Option<crate::claims::ClaimRecord>) {
    let (state, rec) = read_node_claim(node_id);
    let word = state.as_str().to_string();
    let Some(rec) = rec else {
        return ("free", word, None);
    };
    if matches!(state, crate::claims::ClaimState::Free) {
        return ("free", word, Some(rec));
    }
    if holder_is_ours(&rec.holder, &rec) {
        return ("ours", word, Some(rec));
    }
    if matches!(
        state,
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
    ) {
        return ("foreign_live", word, Some(rec));
    }
    ("dead_predecessor", word, Some(rec))
}

/// The claim row iff a DIFFERENT live/suspect session holds the node.
fn classify_holder(
    node_id: &str,
) -> Option<(&'static str, String, Option<crate::claims::ClaimRecord>)> {
    let (verdict, word, rec) = classify_node_claim(node_id);
    (verdict == "foreign_live").then_some((verdict, word, rec))
}

/// Loud park naming the live holder so a second session does not assume the
/// node went idle. Both start exits call this so the message is identical.
fn print_foreign_holder_park(node_id: &str, rec: &crate::claims::ClaimRecord, wt_path: &Path) {
    eprintln!(
        "fno do target start: node {node_id} is held by a live session\n\
         \x20 {holder} (pid={pid}, host={host}).\n\
         \x20 Refusing to share its worktree at {wt} (would corrupt a shared git index).\n\
         \x20 If this is your session, cd {wt} to continue it;\n\
         \x20 otherwise wait for it to release the claim, or pick another node.",
        holder = rec.holder,
        pid = rec.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
        host = rec.host,
        wt = wt_path.display(),
    );
}

/// Classify and re-bind an existing manifest. Ok(true) when the caller is
/// done; Ok(false) means no manifest exists here and the caller should run
/// init against this tree.
#[allow(clippy::too_many_arguments)]
fn bind_worktree(
    node: &str,
    wt_path: &Path,
    base_label: &str,
    in_place: bool,
    node_match_guaranteed: bool,
    beastmode: bool,
    no_merge: bool,
    steps: bool,
) -> Result<bool, Fail> {
    let Some(manifest) = worktree_manifest(wt_path) else {
        return Ok(false);
    };
    if steps && manifest_turn_mode(&manifest).as_deref() != Some("steps") {
        warn_steps_dropped(&manifest);
    }
    // A manifest means init ran. Classify the live node claim from this
    // session's view: foreign-live -> park; ours -> idempotent
    // already-claimed; a dead predecessor -> re-acquire under this session so
    // the lockfile never keeps naming a dead pid that silently expires
    // (successor-takeover gap: two sessions once shared one worktree because
    // start short-circuited here without re-acquiring the claim).
    let (verdict, state_word, claim_info) = classify_node_claim(node);
    if verdict == "foreign_live" {
        print_foreign_holder_park(
            node,
            claim_info
                .as_ref()
                .expect("foreign_live carries its record"),
            wt_path,
        );
        return Err(Fail {
            message: String::new(),
            code: 1,
        });
    }
    // In-place (policy=never) manifests live in the SHARED canonical .fno, and
    // an isolated caller reached this tree without ensure reserving it for
    // THIS node - in both cases the fast-path's "manifest => THIS node's init
    // ran" invariant does not hold. A node mismatch is another node's session;
    // refuse rather than re-acquire under its roof.
    let mnode = manifest_node_id(&manifest);
    if in_place || !node_match_guaranteed {
        if let Some(mnode) = &mnode {
            if mnode != node {
                return Err(refuse(format!(
                    "fno do target start: {} belongs to node {mnode}, not {node}; \
                     refusing to run in place under another node's session. Cancel \
                     it (fno do target cancel) or isolate a worktree.",
                    manifest.display()
                )));
            }
        }
    }
    // Ghost node: the manifest references a node no longer in the graph
    // (superseded / removed). Never re-acquire a claim for a ghost. A
    // free-text/plan-only session (graph_node_id null) has no node by design
    // and is NOT a ghost - skip so a valid rerun proceeds.
    if let Some(mnode) = &mnode {
        if find_node(mnode, None).is_none() {
            return Err(refuse(format!(
                "fno do target start: node {mnode} is not in the backlog graph \
                 (superseded or removed); refusing to re-acquire its claim. Cancel \
                 the stale session (fno do target cancel) or pick a live node."
            )));
        }
    }
    if verdict == "ours" {
        let holder = claim_info
            .as_ref()
            .map(|r| r.holder.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "?".to_string());
        println!(
            "worktree={}  base={}  node=already-claimed holder={holder} state={state_word}",
            wt_path.display(),
            truthful_base(wt_path, base_label, false),
        );
        if beastmode {
            warn_if_authority_not_granted(wt_path);
        }
        if no_merge {
            warn_no_merge_dropped();
        }
        return Ok(true);
    }
    if is_linked_worktree(wt_path) {
        let (occupancy, occupancy_info) = classify_worktree_occupancy(wt_path);
        if occupancy != "available" {
            let remedy = if occupancy == "unknown" {
                " No automatic path re-marks this worktree available; a human must \
                 clear it (fno agents watchdog for the fleet sweep, fno agents \
                 workspace worktree cleanup for merged leftovers) and retry."
            } else {
                ""
            };
            return Err(refuse(format!(
                "fno do target start: refusing takeover of {}: {} {}{}",
                wt_path.display(),
                occupancy,
                occupancy_info,
                remedy
            )));
        }
    }
    // A successor inheriting a predecessor's worktree, or a stale-free claim:
    // re-acquire under this session so the lockfile names a live holder.
    reacquire_node_claim(node, wt_path)?;
    let prior = match claim_info.as_ref() {
        Some(r) => format!(
            "prior holder {} (state={state_word})",
            if r.holder.is_empty() {
                "?"
            } else {
                r.holder.as_str()
            }
        ),
        None => "no prior claim".to_string(),
    };
    println!(
        "worktree={}  base={}  node=reacquired (successor took over from {prior})",
        wt_path.display(),
        truthful_base(wt_path, base_label, false),
    );
    eprintln!("cd {} to continue the pipeline.", wt_path.display());
    if beastmode {
        warn_if_authority_not_granted(wt_path);
    }
    if no_merge {
        warn_no_merge_dropped();
    }
    Ok(true)
}

/// The node-claim TTL in ms (init-target-state.sh mirror): `2h`, overridable
/// via `TARGET_CLAIM_TTL` as `30m` / `3600s`.
fn claim_ttl_ms() -> i64 {
    let raw = std::env::var("TARGET_CLAIM_TTL").unwrap_or_default();
    let trimmed = raw.trim();
    if let Some(re) = Regex::new(r"^(\d+)\s*([hms])$").ok().as_ref() {
        if let Some(c) = re.captures(trimmed) {
            let n: i64 = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let mult = match c.get(2).map(|m| m.as_str()) {
                Some("h") => 3600,
                Some("m") => 60,
                _ => 1,
            };
            return n * mult * 1000;
        }
    }
    2 * 60 * 60 * 1000
}

/// Re-acquire `node:<id>` under THIS session (successor takeover of a
/// predecessor's dead/stale claim). The Python twin also stamps the do row on
/// acquire; the native `acquire` emits its own audit event.
fn reacquire_node_claim(node_id: &str, wt_path: &Path) -> Result<(), Fail> {
    let key = format!("node:{node_id}");
    let home = AgentsHome::from_env();
    let owned =
        crate::spawn_context::resolve_self_identity(&|k| std::env::var(k).ok(), None, None, &home);
    let Some(session_id) = owned
        .session_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        // Not the user-cancel sentinel (an identity refusal is not a user
        // cancel), and not the predecessor's manifest either: it is
        // write-once per session with the init hook as sole owner. The
        // stderr line is this refusal's trace.
        return Err(refuse(format!(
            "fno do target start: holder_unattributable for {key}; the current \
             harness session could not be proven, so no successor claim was written."
        )));
    };
    let holder = format!("target-session:{session_id}");
    let (durable_pid, _harness) =
        crate::spawn_context::session_identity_ambient(std::process::id());
    let mut opts = crate::claims::AcquireOpts {
        ttl_ms: Some(claim_ttl_ms()),
        reason: Some("target start successor re-acquire".to_string()),
        ..Default::default()
    };
    if let Some(p) = durable_pid {
        opts.pid = Some(p);
    } else {
        opts.pid_unavailable = true;
    }
    match crate::claims::acquire(&key, &holder, opts) {
        crate::claims::AcquireOutcome::Acquired(_) => Ok(()),
        crate::claims::AcquireOutcome::HeldByOther { holder, pid, host } => {
            print_foreign_holder_park(
                node_id,
                &crate::claims::ClaimRecord {
                    holder,
                    pid,
                    host,
                    ..placeholder_record(&key)
                },
                wt_path,
            );
            Err(Fail {
                message: String::new(),
                code: 1,
            })
        }
        crate::claims::AcquireOutcome::Error(exc) => Err(refuse(format!(
            "fno do target start: cannot re-acquire {key}: {exc}. The claim state \
             is unreadable or contended; refusing to risk a duplicate claim. Clear \
             it (fno agents claim release {key} --force -R <why>) and retry."
        ))),
    }
}

/// The minimal record shape the park printer reads, for a holder named by an
/// acquire refusal rather than read off disk.
fn placeholder_record(key: &str) -> crate::claims::ClaimRecord {
    crate::claims::ClaimRecord {
        schema_version: 1,
        key: key.to_string(),
        holder: String::new(),
        acquired_at: 0,
        pid: None,
        host: String::new(),
        pid_unavailable: false,
        expires_at: None,
        reason: None,
        harness: None,
        session_id: None,
        pid_provenance: None,
        machine_id: None,
        metadata: Default::default(),
    }
}

/// Join meaningful worktree dirt with recent transcript activity. Only a
/// clean tree or positively stale transcript makes a free/stale claim
/// available for takeover. The transcript leg has no native twin, so a
/// dirty-but-unreapable tree answers unknown: takeover refuses, the safe
/// direction (a live worker's tree is never taken over on this port's say-so).
fn classify_worktree_occupancy(wt_path: &Path) -> (String, String) {
    let verdict = crate::worktree_reapable::reapable(&wt_path.to_string_lossy());
    if verdict.reapable {
        return ("available".to_string(), String::new());
    }
    if verdict.reason == "probe-failed" {
        return (
            "unknown".to_string(),
            format!(
                "{{'reason': 'probe-failed', 'detail': '{}'}}",
                verdict.detail.replace('\'', "'")
            ),
        );
    }
    (
        "unknown".to_string(),
        "{'reason': 'transcript-activity-unavailable'}".to_string(),
    )
}

/// Name a --no-merge that did not take: start's idempotent early returns
/// create no manifest, and the manifest is write-once.
fn warn_no_merge_dropped() {
    eprintln!(
        "WARNING: --no-merge did NOT take - this session wrote no manifest, and \
         the manifest is write-once. Merge posture is whatever the existing \
         manifest says. Verify with:\n  sed -n \
         's/^auto_merge_approved:[[:space:]]*//p' \"$(fno-agents state path \
         target-state)\"\nand if it is not `false`, do not rely on this flag."
    );
}

/// Name a --steps that did not take. The idempotent and successor paths
/// re-bind an existing write-once manifest, so a mode the caller asked for
/// and the manifest did not record stays a silent divergence unless this
/// prints.
fn warn_steps_dropped(manifest: &Path) {
    eprintln!(
        "WARNING: --steps did NOT take - the manifest at {} is write-once and \
         does not carry turn_mode: steps. The stop gate keeps its normal \
         looping posture for this session. To run steps mode, cancel this \
         session (fno do target cancel) and re-start the node with --steps.",
        manifest.display()
    );
}

/// Name a --beastmode that did not take. The verdict comes from the orient
/// module's own predicate, so this warning and the `attended` line can never
/// disagree about whether the grant holds.
fn warn_if_authority_not_granted(project_root: &Path) {
    let raw = crate::target_orient::read_manifest(project_root);
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
             session will not act with authority.\nAuthority needs a LIVE CLAIM: \
             a free-text run claims no node, and a stale claim proves nothing, \
             so an abandoned session would be indistinguishable from a live \
             one. The grant is refused rather than left to outlive its session.\n\
             Re-run against a backlog node (`fno do target start --beastmode \
             <node>`), or continue without authority."
        );
        return;
    }
    let fix = if raw.is_some() {
        "The manifest is write-once and one already existed, so the flag was a \
         no-op. To run with authority, finish or cancel this session first \
         (`/fno:cancel-target`), then start a fresh one."
    } else {
        "No manifest was written, so nothing consumed the flag. Run `fno do \
         target init --beastmode --input <node>` to claim this session with a \
         grant."
    };
    eprintln!("--beastmode did NOT take - this session has no authority grant.\n{fix}");
}

// ---------------------------------------------------------------------------
// The hold and containment gates
// ---------------------------------------------------------------------------

/// Refuse a named dispatch when its plan ancestry carries a hold. An external
/// tracker backend carries no graph-resident hold, so the gate skips there.
/// Every refusal keeps the Python text and the exit-2 value.
fn refuse_dispatch_hold(node: Option<&Value>, entries: Option<&[Value]>) -> Result<(), Fail> {
    let Some(node) = node else {
        return Ok(());
    };
    if matches!(
        std::env::var("FNO_TRACKER_BACKEND").as_deref(),
        Ok(backend) if !backend.is_empty() && backend != "graph"
    ) {
        return Ok(());
    }
    let owned;
    let rows: &[Value] = match entries {
        Some(rows) => rows,
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match crate::target_init::graph_rows(&cwd) {
                Some(rows) => {
                    owned = rows;
                    &owned
                }
                None => {
                    let id = node.get("id").and_then(Value::as_str).unwrap_or("unknown");
                    return Err(fail_code(
                        format!(
                            "fno do target: dispatch-hold-invalid:{id}: backlog graph \
                             is unreadable; refusing to assume unheld."
                        ),
                        2,
                    ));
                }
            }
        }
    };
    let by_id: std::collections::BTreeMap<String, Value> = rows
        .iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), e.clone()))
        })
        .collect();
    let Some(verdict) = crate::backlog_ready::hold_verdict_receipt(node, &by_id) else {
        return Ok(());
    };
    if !verdict.held {
        let missing = verdict
            .detail
            .contains("missing under its existing plan root");
        let remedy = if missing {
            "Repair the node with: fno backlog update <id> plan_path=<path>, or \
             restore the plan file."
        } else {
            "fix or remove the declaration, then retry."
        };
        return Err(fail_code(
            format!(
                "fno do target: {}: {}. The plan cannot prove that dispatch is \
                 unheld; {remedy} Nothing was claimed.",
                verdict.guard_reason, verdict.detail
            ),
            2,
        ));
    }
    let exempt_line = if verdict.exempt.is_empty() {
        String::new()
    } else {
        format!("  exempt: {}\n", verdict.exempt)
    };
    Err(fail_code(
        format!(
            "fno do target: dispatch held by plan {}\n  reason: {}\n  set_by: {}\n  \
             release_when: {}\n  review_on: {}\n{exempt_line}Remove dispatch_hold \
             only after the release condition is confirmed. Nothing was claimed.",
            verdict.owner, verdict.reason, verdict.set_by, verdict.release_when, verdict.review_on
        ),
        2,
    ))
}

/// Route a named contained node to the delivery unit that owns its PR. A
/// REDIRECT, not a refusal: naming a node is consent; the answer is which
/// node owns it. Exits before any allocation, so nothing is claimed and no
/// manifest is written. The message text names init: the Python twin shares
/// it across both verbs verbatim.
fn redirect_if_contained(node: Option<&Value>, entries: Option<&[Value]>) -> Result<(), Fail> {
    let Some(node) = node else {
        return Ok(());
    };
    let Some(owner) = node
        .get("contained_in")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Ok(());
    };
    let nid = node
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("that node");
    let Some(owner_node) = find_node(&owner, entries) else {
        return Err(fail_code(
            format!(
                "fno do target init: {nid} is marked as shipping inside {owner}, but \
                 no node {owner} exists.\nThat containment is stale and nothing will \
                 clear it automatically. Clear it with `fno backlog update {nid} \
                 parent=null`, then dispatch this node on its own. Nothing was \
                 claimed."
            ),
            2,
        ));
    };
    let superseded = truthy(owner_node.get("superseded_by"));
    let deferred = truthy(owner_node.get("deferred_at"));
    let completed = truthy(owner_node.get("completed_at"));
    if (superseded || deferred) && !completed {
        let state = if superseded { "superseded" } else { "deferred" };
        return Err(fail_code(
            format!(
                "fno do target init: {nid} is marked as shipping inside {owner}, but \
                 {owner} is {state} and will never ship it.\nThat containment is \
                 stale. Clear it with `fno backlog update {nid} parent=null`, then \
                 dispatch this node on its own. Nothing was claimed."
            ),
            2,
        ));
    }
    if completed {
        let where_note = owner_node
            .get("pr_number")
            .and_then(Value::as_i64)
            .map(|p| format!(" in PR #{p}"))
            .unwrap_or_default();
        return Err(fail_code(
            format!(
                "fno do target init: {nid} already shipped inside {owner}{where_note}, \
                 which is done - there is nothing left to build here.\n{nid} was \
                 folded into {owner} as a contained node, so it has no PR of its own \
                 and is not separately costed. Nothing was claimed."
            ),
            2,
        ));
    }
    Err(fail_code(
        format!(
            "fno do target init: {nid} ships inside {owner}'s PR; run `/fno:target \
             {owner}`.\n{nid} was folded into {owner} as a contained node, so it \
             has no PR of its own and is not separately costed. Nothing was \
             claimed."
        ),
        2,
    ))
}

/// Python truthiness for the graph-row flags the gates read: None, false,
/// zero, and the empty string are falsy; every other JSON value (including
/// the strings "null" and "False") is truthy, exactly as the Python dict
/// reads.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().map_or(false, |f| f != 0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(_)) => true,
    }
}

/// Resolve a slug / bare-hex input to its canonical backlog node id: the
/// deterministic exact tiers only. A non-match (free text) returns the arg
/// unchanged - init accepts text - so this only ever upgrades a resolvable
/// id, never blocks one. Any read failure falls through to raw.
fn resolve_node_id(node: &str, entries: Option<&[Value]>) -> String {
    let owned;
    let rows: &[Value] = match entries {
        Some(rows) => rows,
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match crate::target_init::graph_rows(&cwd) {
                Some(rows) => {
                    owned = rows;
                    &owned
                }
                None => return node.to_string(),
            }
        }
    };
    match crate::backlog::node_ref::resolve_tiers(rows, node) {
        Some(hit) => hit
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(node)
            .to_string(),
        None => node.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The codex-desktop handoff legs
// ---------------------------------------------------------------------------

fn codex_sessions_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("FNO_CODEX_SESSIONS_DIR") {
        if !override_dir.is_empty() {
            return PathBuf::from(override_dir);
        }
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        if !home.is_empty() {
            return expand_user(&home).join("sessions");
        }
    }
    expand_user("~/.codex/sessions")
}

/// Rollout jsonl for a codex `session_id`, or None. Fast path: codex embeds
/// the session uuid in the rollout filename, so a filename substring match
/// wins without opening a file. Fallback: scan newest-first for a rollout
/// whose first `session_meta` line names the id.
fn codex_rollout_for_session(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut stack = vec![codex_sessions_dir()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .map_or(false, |n| {
                    n.starts_with("rollout-") && n.ends_with(".jsonl")
                })
            {
                paths.push(path);
            }
        }
    }
    for path in &paths {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map_or(false, |n| n.contains(session_id))
        {
            return Some(path.clone());
        }
    }
    let mut by_mtime: Vec<(std::time::SystemTime, PathBuf)> = paths
        .into_iter()
        .map(|p| {
            let mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (mtime, p)
        })
        .collect();
    by_mtime.sort_by(|a, b| b.0.cmp(&a.0));
    for (_mtime, path) in by_mtime {
        if let Some(meta) = codex_rollout_meta(&path) {
            if meta.0 == session_id {
                return Some(path);
            }
        }
    }
    None
}

/// `(session_id, payload)` from a rollout's first line, or None.
fn codex_rollout_meta(path: &Path) -> Option<(String, Value)> {
    let text = std::fs::read_to_string(path).ok()?;
    let first = text.lines().next()?;
    let record: Value = serde_json::from_str(first).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = record.get("payload")?;
    let id = payload.get("id").and_then(Value::as_str)?.to_string();
    Some((id, payload.clone()))
}

/// Canonical repo iff `cwd` is this Desktop chat's app-owned worktree.
/// Location alone is deliberately insufficient: an external allocator can
/// create a linked worktree below `CODEX_HOME/worktrees` without making Codex
/// Desktop its lifecycle owner. The rollout originator authenticates the
/// Desktop chat; git's common dir authenticates registration and yields the
/// stable canonical repository identity used for project roll-up.
fn codex_native_repo(cwd: &Path) -> Option<PathBuf> {
    let (session_id, harness) = crate::claims::resolve_identity();
    if harness.as_deref() != Some("codex") {
        return None;
    }
    let session_id = session_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let meta = codex_rollout_for_session(&session_id).and_then(|p| codex_rollout_meta(&p))?;
    if meta.1.get("originator").and_then(Value::as_str) != Some("Codex Desktop") {
        return None;
    }
    let resolved = resolve_soft(cwd);
    let codex_home = resolve_soft(
        &std::env::var("CODEX_HOME")
            .map(|s| expand_user(&s))
            .unwrap_or_else(|_| expand_user("~/.codex")),
    );
    let native_root = codex_home.join("worktrees");
    if resolved == native_root || !resolved.starts_with(&native_root) {
        return None;
    }
    if !is_linked_worktree(&resolved) {
        return None;
    }
    let common = git_out(&resolved, &["rev-parse", "--git-common-dir"])?;
    let common_dir = resolve_soft(&abs_against(&resolved, &common));
    if common_dir.file_name().and_then(|n| n.to_str()) != Some(".git") {
        return None;
    }
    let canonical = resolve_soft(common_dir.parent()?);
    let recorded = meta.1.get("cwd").and_then(Value::as_str)?;
    if resolve_soft(&expand_user(recorded)) != canonical {
        return None;
    }
    if !codex_project_assignment(&session_id, &resolved, &canonical) {
        return None;
    }
    let registered = git_out(&canonical, &["worktree", "list", "--porcelain"])?;
    let listed: std::collections::HashSet<PathBuf> = registered
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|line| resolve_soft(&expand_user(line)))
        .collect();
    if listed.contains(&resolved) && listed.contains(&canonical) {
        Some(canonical)
    } else {
        None
    }
}

/// Whether `cwd` is inside the app's reserved worktree namespace.
fn under_codex_worktrees(cwd: &Path) -> bool {
    let codex_home = resolve_soft(
        &std::env::var("CODEX_HOME")
            .map(|s| expand_user(&s))
            .unwrap_or_else(|_| expand_user("~/.codex")),
    );
    let native_root = codex_home.join("worktrees");
    let resolved = resolve_soft(cwd);
    resolved != native_root && resolved.starts_with(&native_root)
}

/// True iff Desktop maps this exact thread/cwd to the canonical project. The
/// assignment overlay is read-only, strict, and fail-closed; Footnote never
/// creates, repairs, or writes this private app state.
fn codex_project_assignment(session_id: &str, cwd: &Path, canonical: &Path) -> bool {
    let codex_home = std::env::var("CODEX_HOME")
        .map(|s| expand_user(&s))
        .unwrap_or_else(|_| expand_user("~/.codex"));
    let codex_home = resolve_soft(&codex_home);
    let Ok(state) = std::fs::read_to_string(codex_home.join(".codex-global-state.json")) else {
        return false;
    };
    let Ok(state) = serde_json::from_str::<Value>(&state) else {
        return false;
    };
    let Some(assignment) = state
        .get("thread-project-assignments")
        .and_then(|m| m.get(session_id))
    else {
        return false;
    };
    if assignment.get("projectKind").and_then(Value::as_str) != Some("local") {
        return false;
    }
    let recorded = match assignment.get("cwd").and_then(Value::as_str) {
        Some(c) => resolve_soft(&expand_user(c)),
        None => return false,
    };
    if recorded != resolve_soft(cwd) {
        return false;
    }
    let project_id = match assignment.get("projectId") {
        Some(Value::String(p)) => p.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return false,
    };
    let Some(roots) = state
        .get("local-projects")
        .and_then(|m| m.get(&project_id))
        .and_then(|p| p.get("rootPaths"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let canonical_resolved = resolve_soft(canonical);
    roots.iter().any(|root| {
        root.as_str()
            .map(|r| resolve_soft(&expand_user(r)) == canonical_resolved)
            .unwrap_or(false)
    })
}

/// `~` expansion for the recorded paths.
fn expand_user(path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(rest);
            }
        }
    }
    PathBuf::from(path)
}

/// True iff this is a canonical Codex Desktop Local chat with the
/// harness-native worktree policy: the only shape the supported same-thread
/// handoff serves. A confirmed Desktop origin with no verified project
/// assignment is the one hard refusal (the external fallback would split
/// Remote roll-up).
fn codex_desktop_handoff_policy(repo_root: &Path) -> bool {
    let (session_id, harness) = crate::claims::resolve_identity();
    if harness.as_deref() != Some("codex") {
        return false;
    }
    let Some(session_id) = session_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    let Some((_id, meta)) =
        codex_rollout_for_session(&session_id).and_then(|p| codex_rollout_meta(&p))
    else {
        return false;
    };
    if meta.get("originator").and_then(Value::as_str) != Some("Codex Desktop") {
        return false;
    }
    let recorded = match meta.get("cwd").and_then(Value::as_str) {
        Some(c) => resolve_soft(&expand_user(c)),
        None => return false,
    };
    if recorded != resolve_soft(repo_root) {
        return false;
    }
    if codex_requested_worktree_policy().as_deref() != Some("harness-native") {
        return false;
    }
    if !codex_project_assignment(&session_id, repo_root, repo_root) {
        eprintln!(
            "fno do target start: Codex Desktop origin is confirmed, but this \
             thread has no verified assignment to the canonical project; refusing \
             external fallback because it would split Remote roll-up."
        );
        std::process::exit(1);
    }
    true
}

/// The requested (pre-degradation) worktree policy word: env > the config
/// candidates' `worktree.policy` > the built-in default. An invalid word
/// reads as no native handoff, the direction the Python caller's policy
/// error arm already takes.
fn codex_requested_worktree_policy() -> Option<String> {
    const VALID: [&str; 3] = ["never", "harness-native", "external"];
    if let Ok(env_policy) = std::env::var("FNO_WORKTREE_POLICY") {
        let word = env_policy.trim().to_string();
        return VALID.contains(&word.as_str()).then_some(word);
    }
    let mut policy: Option<String> = None;
    for cfg in crate::backlog::settings::config_candidates() {
        if let Some(word) = cfg
            .get("worktree")
            .and_then(|w| w.get("policy"))
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            policy = Some(word);
            break;
        }
    }
    let word = policy.unwrap_or_else(|| "harness-native".to_string());
    VALID.contains(&word.as_str()).then_some(word)
}

/// Emit the supported same-thread Desktop handoff receipt and stop (exit 75).
fn request_codex_native_handoff(repo_root: &Path, node: &str, base: &str, _deadline: Instant) -> ! {
    println!(
        "native-handoff=required project={} canonical={} base={} node={node}\n\
         Use `/worktree` or **Hand off -> Worktree** in Codex Desktop, select the \
         remote main branch, then rerun:\n  fno do target start {node}\nNo worktree, \
         branch, manifest, or claim was created by Footnote.",
        repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        repo_root.display(),
        truthful_base(repo_root, base, true),
    );
    std::process::exit(CODEX_NATIVE_HANDOFF_EXIT);
}

/// Finish target bootstrap inside a worktree Codex Desktop already owns.
fn start_codex_native(
    canonical: &Path,
    cwd: &Path,
    node: &str,
    args: &Args,
    plan_path: Option<&str>,
    deadline: Instant,
) -> Result<i32, Fail> {
    let base = prepare_codex_native_branch(cwd, node)?;
    let (rc, tail) = run_setup_worktree_hook(canonical, cwd);
    if rc != 0 && rc != -1 {
        return Err(fail_code(
            format!(
                "fno do target start: setup-worktree.sh exited {rc}: {tail}; refusing \
                 to initialize against unverified shared state."
            ),
            if rc > 0 { rc } else { 1 },
        ));
    }
    if let Some(manifest) = worktree_manifest(cwd) {
        let manifest_node = manifest_node_id(&manifest);
        let requested_graph_node = find_node(node, None);
        if requested_graph_node.is_none() {
            let text = std::fs::read_to_string(&manifest).unwrap_or_default();
            let explicitly_unclaimed = Regex::new(r#"(?m)^graph_node_id\s*:\s*(?:null|''|"")\s*$"#)
                .map(|re| re.is_match(&text))
                .unwrap_or(false);
            if manifest_node.is_some() || !explicitly_unclaimed {
                return Err(refuse(format!(
                    "fno do target start: {} does not contain a valid unclaimed \
                     free-text target manifest; refusing native resume.",
                    manifest.display()
                )));
            }
            println!(
                "worktree={}  app-owned=codex  .fno=ok  base={}  node=unclaimed",
                cwd.display(),
                truthful_base(cwd, &base, true)
            );
            if args.beastmode {
                warn_if_authority_not_granted(cwd);
            }
            return Ok(0);
        }
        if manifest_node.as_deref() != Some(node) {
            return Err(refuse(format!(
                "fno do target start: {} does not record the requested node {node} \
                 (found {}); refusing native resume.",
                manifest.display(),
                manifest_node.unwrap_or_else(|| "null".to_string())
            )));
        }
        let (verdict, _state_word, info) = classify_node_claim(node);
        if verdict == "foreign_live" {
            print_foreign_holder_park(
                node,
                info.as_ref().expect("foreign_live carries its record"),
                cwd,
            );
            return Err(Fail {
                message: String::new(),
                code: 1,
            });
        }
        if let Some(mnode) = &manifest_node {
            if find_node(mnode, None).is_none() {
                return Err(refuse(format!(
                    "fno do target start: node {mnode} is not in the backlog graph; \
                     refusing to restore a ghost claim."
                )));
            }
        }
        if verdict == "ours" {
            let holder = info
                .as_ref()
                .map(|r| r.holder.clone())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "?".to_string());
            println!(
                "worktree={}  app-owned=codex  .fno=ok  base={}  node=already-claimed \
                 holder={holder} state={}",
                cwd.display(),
                truthful_base(cwd, &base, true),
                codex_resume_state_word(node),
            );
            if args.beastmode {
                warn_if_authority_not_granted(cwd);
            }
            return Ok(0);
        }
        reacquire_node_claim(node, cwd)?;
        println!(
            "worktree={}  app-owned=codex  .fno=ok  base={}  node=reacquired",
            cwd.display(),
            truthful_base(cwd, &base, true)
        );
        return Ok(0);
    }

    // include_difficulty mirrors the pinned-harness rule at the worktree
    // start site: an operator --harness pin reaches the worker's spawn argv
    // and stands the capacity grid down, so the band resolves here.
    let (resolved_model, source) = resolve_node_model(
        node,
        args.model.as_deref(),
        args.harness.as_deref(),
        args.harness.is_some(),
    );
    let mut init_args: Vec<String> = vec!["--input".into(), node.to_string()];
    if let Some(p) = plan_path {
        init_args.extend(["--plan-path".into(), p.to_string()]);
    }
    if let Some(size) = &args.size {
        init_args.extend(["--size".into(), size.clone()]);
    }
    if let Some(m) = &resolved_model {
        init_args.extend(["--model".into(), m.clone()]);
    }
    if let Some(h) = &args.harness {
        init_args.extend(["--harness".into(), h.clone()]);
    }
    if args.beastmode {
        init_args.push("--beastmode".into());
    }
    if args.no_merge {
        init_args.push("--no-merge".into());
    }
    if let Some(n) = args.deliverables {
        init_args.extend(["--deliverables".into(), n.to_string()]);
    }
    if args.steps {
        init_args.push("--steps".into());
    }
    let code = run_bounded_init(&init_args, cwd, deadline, node)?;
    if code != 0 {
        return Err(fail_code(
            format!(
                "fno do target start: target init failed in app-owned worktree \
                 (exit {code}); no replacement worktree was allocated."
            ),
            code,
        ));
    }
    let Some(manifest) = worktree_manifest(cwd) else {
        return Err(refuse(
            "fno do target start: target init exited successfully but no target \
             manifest exists; refusing success receipt.",
        ));
    };
    let created_node = manifest_node_id(&manifest);
    let claim_state = match created_node {
        None => {
            let text = std::fs::read_to_string(&manifest).unwrap_or_default();
            let explicitly_unclaimed = Regex::new(r#"(?m)^graph_node_id\s*:\s*(?:null|''|"")\s*$"#)
                .map(|re| re.is_match(&text))
                .unwrap_or(false);
            if !explicitly_unclaimed {
                return Err(refuse(format!(
                    "fno do target start: target init exited successfully but {} \
                     does not contain valid claim provenance; refusing success receipt.",
                    manifest.display()
                )));
            }
            "unclaimed".to_string()
        }
        Some(created) => {
            if created != node {
                return Err(refuse(format!(
                    "fno do target start: target init recorded node {created}, not \
                     {node}; refusing success receipt."
                )));
            }
            let (verdict, _word, _info) = classify_node_claim(&created);
            if verdict != "ours" {
                return Err(refuse(format!(
                    "fno do target start: target init did not leave this thread \
                     owning node:{created} (claim={verdict}); refusing success receipt."
                )));
            }
            "claimed".to_string()
        }
    };
    let model_note = resolved_model
        .map(|m| format!("  model={m} ({source})"))
        .unwrap_or_default();
    println!(
        "worktree={}  app-owned=codex  .fno=ok  base={}  node={claim_state}{model_note}",
        cwd.display(),
        truthful_base(cwd, &base, true)
    );
    Ok(0)
}

/// The `state=` half of the codex resume receipt, re-read at print time.
fn codex_resume_state_word(node_id: &str) -> String {
    classify_node_claim(node_id).1
}

/// Attach an app-created detached worktree to this target's node branch.
fn prepare_codex_native_branch(cwd: &Path, node: &str) -> Result<String, Fail> {
    let branch = codex_node_branch(node);
    let Some(base) = remote_base_ref(cwd, true) else {
        return Err(Fail {
            message: String::new(),
            code: 1,
        });
    };
    let current = git_out(cwd, &["branch", "--show-current"]).unwrap_or_default();
    if current == branch {
        if worktree_manifest(cwd).is_none() {
            let head_sha = git_out(cwd, &["rev-parse", "HEAD"]);
            let base_sha = git_out(cwd, &["rev-parse", &base]);
            match (head_sha, base_sha) {
                (Some(h), Some(b)) if h == b => {}
                _ => {
                    return Err(refuse(format!(
                        "fno do target start: {branch} has no target manifest and \
                         HEAD does not equal refreshed {base}; refusing an unproven \
                         resume."
                    )))
                }
            }
        }
        return base_receipt(cwd, &base);
    }
    if !current.is_empty() {
        return Err(refuse(format!(
            "fno do target start: Codex worktree is already on branch {current}; \
             expected detached HEAD or {branch}."
        )));
    }
    let dirty = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output();
    match dirty {
        Ok(o) if o.status.success() && o.stdout.is_empty() => {}
        _ => {
            return Err(refuse(
                "fno do target start: refusing to attach a dirty detached Codex \
                 worktree; stash or discard its changes first.",
            ))
        }
    }
    let show_ref = format!("refs/heads/{branch}");
    if git_out(cwd, &["show-ref", "--verify", &show_ref]).is_some() {
        return Err(refuse(format!(
            "fno do target start: branch {branch} already exists but this detached \
             Codex worktree has no target manifest proving it is a resume; refusing \
             to claim an arbitrary branch."
        )));
    }
    let switched = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["switch", "-c", &branch, &base])
        .output();
    match switched {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Err(refuse(format!(
                "fno do target start: could not attach Codex worktree to {branch}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )))
        }
        Err(e) => {
            return Err(refuse(format!(
                "fno do target start: could not attach Codex worktree to {branch}: {e}"
            )))
        }
    }
    base_receipt(cwd, &base)
}

/// The node's branch, minted by the one Rust resolver (`backlog get <name>
/// --field _branch`'s owner). A non-node name degrades to `feature/<name>`.
fn codex_node_branch(name: &str) -> String {
    let entries = {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        crate::target_init::graph_rows(&cwd)
    };
    let Some(rows) = entries else {
        return format!("feature/{name}");
    };
    let Some(row) = crate::backlog::node_ref::resolve_tiers(&rows, name) else {
        return format!("feature/{name}");
    };
    crate::node_branch::resolve(row)
        .or_else(|| crate::node_branch::mint(row))
        .unwrap_or_else(|| format!("feature/{name}"))
}

// ---------------------------------------------------------------------------
// The init leg
// ---------------------------------------------------------------------------

/// `(model, decision_source)` for a node's model pin: explicit wins, the
/// node's pin next, then the route door (degrading to provider-default when
/// the door answers nothing). Strictly non-fatal: a dispatch never fails on
/// the routing layer.
fn resolve_node_model(
    node_id: &str,
    explicit: Option<&str>,
    provider: Option<&str>,
    include_difficulty: bool,
) -> (Option<String>, String) {
    if let Some(e) = explicit.filter(|e| !e.trim().is_empty()) {
        return (Some(e.to_string()), "explicit".to_string());
    }
    let node = find_node(node_id, None);
    let task_model = node
        .as_ref()
        .and_then(|n| n.get("model"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|m| !m.is_empty());
    if let Some(m) = task_model {
        return (Some(m), "task-pin".to_string());
    }
    let task_difficulty = if include_difficulty {
        node.as_ref()
            .and_then(|n| n.get("difficulty"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    let payload = json!({
        "mode": "dispatch_model",
        "explicit": explicit,
        "task_model": task_model,
        "task_difficulty": task_difficulty,
        "plan_model": Value::Null,
        "plan_difficulty": Value::Null,
        "provider": provider.unwrap_or("claude"),
    });
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let out = crate::route_gather::run_mode(&payload, &cwd);
    let model = out
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|m| !m.is_empty());
    match model {
        Some(m) => {
            let source = out
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (Some(m), source)
        }
        None => (None, "provider-default(no-difficulty)".to_string()),
    }
}

/// Run the native init door FROM the worktree: bounds the stage, propagates
/// the exit. The init output streams (the Python twin inherited stdio too),
/// so the orienter lands on start's own streams byte-for-byte.
fn run_bounded_init(
    init_args: &[String],
    wt_path: &Path,
    deadline: Instant,
    node: &str,
) -> Result<i32, Fail> {
    let Ok(self_exe) = std::env::current_exe() else {
        return Err(refuse(
            "fno do target start: could not resolve the init door (own binary)",
        ));
    };
    let mut cmd = Command::new(self_exe);
    use std::os::unix::process::CommandExt;
    cmd.arg("target-init")
        .args(init_args)
        .current_dir(wt_path)
        .process_group(0)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(refuse(format!(
                "fno do target start: could not run the init door: {e}"
            )))
        }
    };
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                // The child is its own group leader, so the kill reaches the
                // grandchildren a forking init left behind.
                timed_out = true;
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                break child
                    .wait()
                    .map_err(|e| refuse(format!("fno do target start: init wait failed: {e}")))?;
            }
            Err(e) => {
                return Err(refuse(format!(
                    "fno do target start: init wait failed: {e}"
                )))
            }
        }
    };
    if timed_out {
        stage_timeout_exit(
            "init",
            &format!(
                "Claim state is unknown: check fno agents claim status node:{node}. \
                 A rerun of fno do target start {node} resumes idempotently."
            ),
        );
    }
    let code = match status.code() {
        Some(c) => c,
        None => status.signal().map(|s| 128 + s).unwrap_or(1),
    };
    Ok(code)
}

/// Init the session FROM an existing worktree: binds owner_cwd and claims the
/// node exactly once (the helper's one-call claim).
#[allow(clippy::too_many_arguments)]
fn init_from_worktree(
    node: &str,
    wt_path: &Path,
    base_label: &str,
    in_place: bool,
    created_this_run: bool,
    from_note: &str,
    args: &Args,
    plan_path: Option<&str>,
    deadline: Instant,
) -> Result<i32, Fail> {
    // Project the node's model pin into init's dispatch pin. An explicit -m
    // wins; when the operator pinned --harness the worker's spawn argv carries
    // it, which stands the capacity grid down, so the difficulty band resolves
    // statically here; unpinned, the band defers to that grid. Never blocks.
    let (model, source) = resolve_node_model(
        node,
        args.model.as_deref(),
        args.harness.as_deref(),
        args.harness.is_some(),
    );
    let mut init_args: Vec<String> = vec!["--input".into(), node.to_string()];
    if args.no_merge {
        init_args.push("--no-merge".into());
    }
    if let Some(p) = plan_path {
        init_args.extend(["--plan-path".into(), p.to_string()]);
    }
    if let Some(size) = &args.size {
        init_args.extend(["--size".into(), size.clone()]);
    }
    if let Some(m) = &model {
        init_args.extend(["--model".into(), m.clone()]);
    }
    if let Some(h) = &args.harness {
        init_args.extend(["--harness".into(), h.clone()]);
    }
    if args.beastmode {
        init_args.push("--beastmode".into());
    }
    if let Some(n) = args.deliverables {
        init_args.extend(["--deliverables".into(), n.to_string()]);
    }
    // The real --steps flag: forwarded so init stamps the stop gate's steps
    // mode. No environment variable ahead of it.
    if args.steps {
        init_args.push("--steps".into());
    }
    let code = run_bounded_init(&init_args, wt_path, deadline, node)?;
    if code != 0 {
        if created_this_run && !in_place {
            // One receipt line the run currently lacks: the refused init
            // leaves a fresh tree holding an init-time manifest and no claim,
            // which a later reader cannot tell from a live session. The tree
            // is NOT deleted - deletion is the more dangerous mistake.
            eprintln!(
                "fno do target start: target init failed (step: init, exit {code}); \
                 worktree at {} is created but unclaimed; reclaim with: fno agents \
                 workspace worktree archive {}",
                wt_path.display(),
                wt_path.display()
            );
        } else {
            eprintln!(
                "fno do target start: target init failed (step: init, exit {code}); \
                 worktree at {} predates this run and was left untouched.",
                wt_path.display()
            );
        }
        return Err(Fail {
            message: String::new(),
            code,
        });
    }
    // Receipt - one parse-friendly line a memory-less agent acts on. When a
    // model was resolved, record it + its decision_source so the dispatch is
    // auditable; absent -> the bare line, byte-identical.
    let model_note = model
        .map(|m| format!("  model={m} ({source})"))
        .unwrap_or_default();
    println!(
        "worktree={}  base={}  node=claimed{from_note}{model_note}",
        wt_path.display(),
        truthful_base(wt_path, base_label, true)
    );
    eprintln!("cd {} to continue the pipeline.", wt_path.display());
    Ok(0)
}
