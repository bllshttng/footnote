//! The codex pane arm (PR 2 of the pane-port split): the wrapped argv, the
//! daemon baseline, the two-oracle bind wait, the seed delivery and the
//! late-bind pass, ported from the Python leg (`mux_spawn.py`'s
//! `build_pane_argv` codex branch and bind section, `codex_pane.py`,
//! `codex_rollout.py`).
//!
//! Arm ownership is conservative, matching the row-7 carve-out: only the
//! plain and bounded lanes (no explicit geometry - squad/split/tab/at all
//! decline back to Python, whose post-run group placement is not ported).
//! Existing-pane attach and python-mode never reach the spine (seam
//! predicates upstream).
//!
//! Recorded narrowing, bounded by what the client door refuses before the
//! arm runs: `--yolo`/`-Y`/`--permission-mode` are refused axes at the door
//! (and the Python seam keeps their spawns on the Python leg), headless
//! never reaches a pane arm, and the role routes are seam-shielded too
//! (`_is_role_bearing_spawn`). The arm's gate is therefore the payload scan
//! plus the `--node` leg, and the identity leaf renders from
//! [`super::PaneContext::worker_name`].
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use super::{
    pane_ls_detailed, ArmBind, ArmPlan, MuxCli, PaneArm, PaneContext, PaneFacts, PaneRefusal,
};
use super::{FsMux, EXIT_REFUSAL, EXIT_SPAWN_FAILED};
use crate::claims::now_ms;
use crate::codex_inject::ReviewStartError;

/// `_CODE_PAYLOAD_PREFIXES` (`rust_runtime.py:621`): a seed opening with one
/// of these is a code workflow payload, and needs a resolved git grant.
const CODE_PAYLOAD_PREFIXES: [&str; 12] = [
    "/target",
    "/execute",
    "/tdd",
    "/fix",
    "/fno:target",
    "/fno:execute",
    "/fno:tdd",
    "/fno:fix",
    "$fno:target",
    "$fno:execute",
    "$fno:tdd",
    "$fno:fix",
];

/// `_CODEX_DAEMON_PROBE_INTERVAL_S` (`codex_pane.py:105`): a websocket round
/// trip; rate-limit the daemon oracle to one call per interval.
const CODEX_DAEMON_PROBE_INTERVAL_MS: i64 = 2_000;

/// `_BINDING_POLL_S` (`mux_spawn.py:2001`).
const BIND_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// The spawn-time late-bind pass budget (a bounded re-probe, not a second
/// window).
const LATE_BIND_BUDGET: Duration = Duration::from_millis(2_000);
const LATE_BIND_RETRY: Duration = Duration::from_millis(250);

/// The one codex pane arm. Stateless: the per-spawn state rides the
/// process-global baseline slot (the pane door answers one spawn per
/// process, so the slot holds exactly this spawn's baseline).
pub struct CodexArm;

/// The arm instance `pane_arm_for` returns; a static so the
/// `&'static dyn PaneArm` table stays allocation-free.
pub static CODEX_ARM: CodexArm = CodexArm;

impl CodexArm {
    /// The shapes this arm answers: the codex provider on every lane. Its
    /// bind wait and late bind are daemon-native; the group-tab move is the
    /// spine's post-run placement, shared with the simple arms.
    pub fn owns(ctx: &PaneContext) -> bool {
        ctx.provider == "codex"
    }
}

impl PaneArm for CodexArm {
    fn name(&self) -> &'static str {
        "codex"
    }

    /// The git-grant gate, the splice-site collision refusal, then daemon
    /// ensure, the loaded-ids baseline and the wrapped argv - the Python
    /// leg's ordering (the seam gate first, then `dispatch_spawn_pane`'s
    /// collision before `ensure_codex_daemon` at :3655).
    fn prepare(&self, ctx: &PaneContext) -> Result<ArmPlan, PaneRefusal> {
        refuse_without_git_grant(ctx)?;
        // Python's splice-site refusal (`mux_spawn.py:3507`) precedes the
        // daemon start (:3655): a colliding passthrough must refuse before
        // any daemon side effect, like the Python leg's ordering.
        refuse_passthrough_collision(
            &ctx.passthrough,
            &identity_leaf(Some(ctx.worker_name.as_str())),
        )
        .map_err(|line| PaneRefusal {
            line,
            exit_code: EXIT_REFUSAL,
            after_pane: None,
        })?;
        if let Err(error) = crate::codex_inject::ensure_codex_daemon() {
            return Err(PaneRefusal {
                line: format!("codex app-server daemon ensure failed: {error}; no pane spawned"),
                exit_code: EXIT_SPAWN_FAILED,
                after_pane: None,
            });
        }
        // The pre-spawn snapshot: the bind wait diffs the loaded set against
        // it, so a sibling pane's thread can never read as "the one new id".
        // An unreachable daemon stores None - distinct from empty, which
        // would let a loaded stranger bind (codex_pane.py:151-159).
        store_baseline(capture_baseline(ctx));
        // Pre-trust the launch cwd (`simple.rs`'s agy twin): the pane reaches
        // its prompt instead of parking on the folder-trust modal, whose row
        // carries no session id and stalled every spawn read behind it.
        ensure_codex_folder_trust(&ctx.cwd);
        Ok(build_wrapped(ctx))
    }

    /// The two-oracle bind wait, then the held seed. The spine owns the row
    /// write and the receipt; a seed failure KEEPS the pane (see
    /// [`seed_refusal`]).
    fn bind_and_seed(
        &self,
        ctx: &PaneContext,
        pane: &PaneFacts,
        plan: &ArmPlan,
    ) -> Result<ArmBind, PaneRefusal> {
        // The prepare() snapshot, taken. A later spawn overwrites the slot,
        // which is why the door answers one spawn per process.
        let baseline = take_baseline();
        if pane.child_pid.is_none() {
            // Nothing to correlate the rollout fd against; the row is left
            // id-less (mux_spawn.py:4118, the Codex P1/P2 guard).
            return Ok(ArmBind::Unbound {
                reason: "no-child-pid-to-correlate".to_string(),
            });
        }
        let mut probe = BindProbe::real(ctx, pane, baseline.as_ref());
        let bind = await_binding(
            Duration::from_millis(binding_window_ms()),
            &FsMux,
            pane,
            &mut probe,
        );
        if let ArmBind::Bound { session_id } = &bind {
            if let Some(seed) = &plan.held_seed {
                deliver_seed_sync(session_id, seed, &ctx.cwd)
                    .map_err(|error| seed_refusal(&error.to_string(), pane))?;
            }
        }
        Ok(bind)
    }

    /// The spine's late-bind pass: one bounded fd-oracle re-probe, the id
    /// for [`super::stamp_late_bind`]'s compare-and-set.
    fn late_bind(&self, _ctx: &PaneContext, pane: &PaneFacts) -> Option<String> {
        late_bind_probe(LATE_BIND_BUDGET, LATE_BIND_RETRY, pane.child_pid)
    }
}

/// The fd-oracle re-probe, split from [`CodexArm::late_bind`] so tests can
/// shrink the budget.
fn late_bind_probe(budget: Duration, step: Duration, child_pid: Option<u32>) -> Option<String> {
    let deadline = now_ms() + budget.as_millis() as i64;
    loop {
        if let Some(sid) = child_pid.and_then(rollout_session_id_for_tree) {
            return Some(sid);
        }
        if now_ms() >= deadline {
            return None;
        }
        std::thread::sleep(step);
    }
}

/// `_refuse_codex_code_spawn_without_git_grant` (`rust_runtime.py:685`):
/// a bounded codex CODE payload with no resolvable git COMMON dir refuses
/// before any pane or row exists.
///
/// Recorded narrowing: Python's gate also skips bypass postures
/// (`--yolo`/`-Y`/`--dangerously-bypass-approvals-and-sandbox`) and the
/// headless lane. Both are unreachable through the pane arm: the refused
/// axes (`--yolo`/`--permission-mode`) die at the client door before the arm
/// runs, headless never reaches a pane arm, and role-routed spawns are
/// seam-shielded (`_is_role_bearing_spawn`). The arm's gate is the payload
/// scan plus the `--node` leg (`rust_runtime.py:632`).
fn refuse_without_git_grant(ctx: &PaneContext) -> Result<(), PaneRefusal> {
    if !is_code_payload(&ctx.message) && ctx.node.is_none() {
        return Ok(());
    }
    if crate::provider::git_common_dir(&ctx.cwd).is_some() {
        return Ok(());
    }
    Err(PaneRefusal {
        line: format!(
            "fno agents spawn: Codex code payloads require a resolved git grant; cwd={}; \
             git rev-parse --git-common-dir failed; no worker launched",
            ctx.cwd.to_string_lossy()
        ),
        exit_code: EXIT_REFUSAL,
        after_pane: None,
    })
}

/// `_is_codex_code_payload`'s prefix leg (`rust_runtime.py:627`): the seed's
/// first whitespace word names a fno command.
fn is_code_payload(message: &str) -> bool {
    message
        .split_whitespace()
        .next()
        .is_some_and(|word| CODE_PAYLOAD_PREFIXES.contains(&word))
}

/// `ensure_agy_folder_trust`'s codex twin (`simple.rs`): best-effort upsert
/// of the launch cwd's trust key in `$CODEX_HOME/config.toml`, so a fresh
/// worker reaches its prompt instead of parking on the folder-trust modal.
/// Best-effort like the agy twin: any failure returns false and the pane
/// spawns anyway. A malformed existing config is left untouched rather than
/// clobbered.
fn ensure_codex_folder_trust(cwd: &std::path::Path) -> bool {
    let Some(home) = crate::codex_store::codex_home() else {
        return false;
    };
    let config = home.join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    let key = cwd.to_string_lossy().to_string();
    // A `projects` key that is not a table means the config is not the shape
    // codex reads; refuse rather than index through or replace it.
    if doc.get("projects").is_some_and(|p| !p.is_table_like()) {
        return false;
    }
    let trusted = doc
        .get("projects")
        .and_then(|p| p.get(&key))
        .and_then(|t| t.get("trust_level"))
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == "trusted");
    if trusted {
        return true;
    }
    doc["projects"][&key]["trust_level"] = toml_edit::value("trusted");
    write_atomic(&config, &doc.to_string()).is_ok()
}

/// Temp file plus rename: two stops of one session never leave a torn file.
fn write_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The wrapped provider argv and its held seed, byte-parity-checked against
/// the golden (`tests/golden/pane_spawn/codex_python_leg.json`,
/// `cases.argv_plain.pane_argv` from the first `codex` token onward, minus
/// the seed fence).
///
/// Token order is the Python leg's (`mux_spawn.py:951-978`): the declared
/// `interactive_create` head (`codex --remote unix://`), `-C <cwd>`, the
/// bounded posture tokens, hook trust, the passthrough tail, then the seed
/// fence. The seed is HELD out of the argv (`mux_spawn.py:3497-3500` strips
/// the fence; the seed rides a post-bind turn/start), the identity leaf
/// splices after the codex token when a name is set (the full-leg goldens
/// pin it: `plain_pane.pane_run_argv`), and provider passthrough appends
/// before the add-dir strip.
///
/// The git/plan/state-root `--add-dir` grants are computed by the Python leg
/// and then stripped again (`writable_dirs.py:95-115`,
/// `strip_remote_add_dirs`): codex >= 0.156.1 refuses `--add-dir` on a
/// `--remote` argv, so the pane argv carries none - which is why this
/// builder emits none. The git COMMON dir's surviving half is the refusal
/// gate above, not a grant.
pub(crate) fn build_wrapped(ctx: &PaneContext) -> ArmPlan {
    let mut argv =
        crate::harness_capabilities::render_session_argv_raw("codex", "interactive_create", None)
            .expect("embedded codex interactive_create capability");
    argv.extend(["-C".to_string(), ctx.cwd.to_string_lossy().into_owned()]);
    // The lane default posture ("the mode axis is retired; the lane default
    // is the only answer left", mux_spawn.py:952-956), routed through the
    // Rust owner so pane and thread cannot disagree.
    let posture = crate::codex_posture::permission_pane_tokens("codex", "workspace-write:never")
        .expect("the lane default posture is in the vocabulary");
    argv.extend(posture);
    argv.extend(crate::codex_ask::hook_trust_flag());
    // Provider passthrough appends after fno's own tokens (`mux_spawn.py:970`),
    // then the whole argv is add-dir-stripped (`strip_remote_add_dirs`,
    // writable_dirs.py:95): codex >= 0.156.1 refuses --add-dir on a --remote
    // argv, so a passthrough-carried grant dies with fno's own.
    argv.extend(ctx.passthrough.iter().cloned());
    strip_remote_add_dirs(&mut argv);
    // The identity leaf splices right after argv[0] (rule: right after the
    // codex token, before --remote; mux_spawn.py:3508).
    let leaf = identity_leaf(Some(ctx.worker_name.as_str()));
    if !leaf.is_empty() {
        argv.splice(1..1, leaf);
    }
    ArmPlan {
        wrapped: argv,
        held_seed: (!ctx.message.is_empty()).then(|| ctx.message.clone()),
        preassigned_id: None,
    }
}

/// The `FNO_AGENT_SELF` config-set leaf, `codex_shell_env_args`
/// (`codex_pane.py:82-102`): one `-c shell_environment_policy.set.<KEY>` token
/// pair, JSON-quoted like `json.dumps`. Empty name emits nothing: every
/// production path carries a minted or explicit name, and an empty leaf on a
/// degenerate direct call is a shape the goldens never pinned.
///
/// Recorded parity fact: production Python ALWAYS has a name at this splice
/// (the seam mints `--name` when the caller omits it), and the full-leg
/// goldens pin the leaf (`plain_pane.pane_run_argv`:
/// `codex -c shell_environment_policy.set.FNO_AGENT_SELF="w-plain" --remote ...`),
/// so the splice is parity-required, not a nice-to-have.
pub fn identity_leaf(name: Option<&str>) -> Vec<String> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Vec::new();
    };
    // `_ENV_KEY_RE` (codex_pane.py:95-101); the only key this arm emits is
    // FNO_AGENT_SELF, which always matches.
    shell_env_leaf("FNO_AGENT_SELF", name).unwrap_or_default()
}

/// One `-c shell_environment_policy.set.<KEY>="<value>"` pair. `None` when
/// the key is not a valid env key shape.
pub fn shell_env_leaf(key: &str, value: &str) -> Option<Vec<String>> {
    let valid = |bytes: &str| {
        let mut chars = bytes.chars();
        match chars.next() {
            Some(first) if first.is_ascii_alphabetic() || first == '_' => {
                chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            _ => false,
        }
    };
    if !valid(key) {
        return None;
    }
    let quoted = serde_json::to_string(value).ok()?;
    Some(vec![
        "-c".to_string(),
        format!("shell_environment_policy.set.{key}={quoted}"),
    ])
}

/// `refuse_passthrough_collision` (`mux_spawn.py:754-781`): a `--`
/// passthrough token naming a flag fno itself emitted is a named refusal,
/// never a silent last-wins. The `--` fence itself and value tokens are not
/// flags; an `=`-joined flag compares by name. Ported for the row-8 claude
/// arm and tests: the codex arm's context carries no passthrough tokens, so
/// nothing calls it yet (a `#[cfg(test)]`-only caller would read dead code).
pub fn refuse_passthrough_collision(
    passthrough: &[String],
    emitted: &[String],
) -> Result<(), String> {
    if passthrough.is_empty() {
        return Ok(());
    }
    let emitted_flags: std::collections::HashSet<&str> = emitted
        .iter()
        .filter(|tok| tok.starts_with('-') && tok.as_str() != "--")
        .filter_map(|tok| tok.split('=').next())
        .collect();
    for tok in passthrough {
        if !tok.starts_with('-') {
            continue;
        }
        let flag = tok.split('=').next().unwrap_or_default();
        if emitted_flags.contains(flag) {
            return Err(format!(
                "refusing {flag} on both sides: fno emitted it from its own flag and the \
                 passthrough carries it too. Two sources for one value is the defect, not \
                 the collision. Drop one."
            ));
        }
    }
    Ok(())
}

/// `strip_remote_add_dirs` (`writable_dirs.py:95-115`): drop `--add-dir`
/// grants from an argv that rides `--remote`; codex >= 0.156.1 refuses them
/// there. The note prints once anything dropped, like Python's.
fn strip_remote_add_dirs(argv: &mut Vec<String>) {
    if !argv.iter().any(|token| token == "--remote") {
        return;
    }
    let before = argv.len();
    let mut out: Vec<String> = Vec::with_capacity(before);
    let mut drop_next = false;
    for token in argv.drain(..) {
        if drop_next {
            drop_next = false;
        } else if token == "--add-dir" {
            drop_next = true;
        } else if token.starts_with("--add-dir=") {
            continue;
        } else {
            out.push(token);
        }
    }
    *argv = out;
    if argv.len() != before {
        eprintln!(
            "codex pane: dropped --add-dir grants; codex >= 0.156.1 refuses them on --remote"
        );
    }
}

/// The prepare() -> bind_and_seed() channel for the pre-spawn daemon
/// baseline. The pane door answers ONE spawn per process, so a single slot
/// holds exactly this spawn's snapshot; prepare() overwrites, bind_and_seed()
/// takes. `None` means the daemon could not answer pre-spawn - the oracle is
/// DISABLED for the spawn, never treated as empty (codex_pane.py:151-159).
fn baseline_slot() -> &'static Mutex<Option<HashSet<String>>> {
    use std::sync::OnceLock;
    static SLOT: OnceLock<Mutex<Option<HashSet<String>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn store_baseline(baseline: Option<HashSet<String>>) {
    *baseline_slot().lock().unwrap_or_else(|p| p.into_inner()) = baseline;
}

fn take_baseline() -> Option<HashSet<String>> {
    baseline_slot()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
}

/// The loaded-session ids the daemon reports for `ctx.cwd`, as a
/// realpath-compared set (`codex_pane.py:108-141`).
fn capture_baseline(ctx: &PaneContext) -> Option<HashSet<String>> {
    discover_loaded_for_cwd(&ctx.cwd).ok()
}

/// Sync form of `discover_loaded_threads`, filtered to threads whose cwd
/// realpath matches. Every `Err` is "the daemon could not answer" and rides
/// the same `'static str` vocabulary the inject lane uses.
fn discover_loaded_for_cwd(cwd: &Path) -> Result<HashSet<String>, &'static str> {
    let run = || -> Result<HashSet<String>, &'static str> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "io-error")?;
        runtime
            .block_on(crate::codex_inject::discover_loaded_threads())
            .map(|threads| {
                threads
                    .into_iter()
                    .filter(|thread| same_cwd(&thread.cwd, cwd))
                    .map(|thread| thread.session_id)
                    .collect()
            })
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        // A caller inside a runtime gets a helper thread, mirroring
        // codex_inject's own sync wrappers. The path clones in: threads
        // need 'static.
        let owned = cwd.to_path_buf();
        return std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => rt
                    .block_on(crate::codex_inject::discover_loaded_threads())
                    .map(|threads| {
                        threads
                            .into_iter()
                            .filter(|thread| same_cwd(&thread.cwd, &owned))
                            .map(|thread| thread.session_id)
                            .collect()
                    }),
                Err(_) => Err("io-error"),
            }
        })
        .join()
        .map_err(|_| "io-error")?;
    }
    run()
}

/// realpath-normalized cwd compare; raw compare is the fallback when either
/// side does not canonicalize (Python's os.path.realpath never errors).
fn same_cwd(reported: &str, ctx_cwd: &Path) -> bool {
    match (Path::new(reported).canonicalize(), ctx_cwd.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => reported == ctx_cwd.to_string_lossy(),
    }
}

/// `capabilities("codex").session_binding.timeout_ms`; the packaged table
/// ships 60000, and a contract that cannot be read degrades to the same
/// 60 s window rather than skipping the wait.
fn binding_window_ms() -> u64 {
    crate::harness_capabilities::HarnessContract::packaged()
        .and_then(|contract| {
            contract
                .capabilities("codex")
                .map(|caps| caps.session_binding.timeout_ms)
        })
        .unwrap_or(60_000)
}

/// The one new session id the daemon reports for this cwd, or `None` (zero,
/// or two-or-more - a sibling race). Pure form of `_codex_daemon_candidate`'s
/// diff (codex_pane.py:184-191).
fn sole_new_id(loaded: &HashSet<String>, baseline: &HashSet<String>) -> Option<String> {
    let new_ids: Vec<&String> = loaded.difference(baseline).collect();
    match new_ids.len() {
        1 => Some(new_ids[0].clone()),
        _ => None,
    }
}

/// The two-oracle bind probe (`codex_pane.py:194-300`), holding the
/// repeat-before-trust state the daemon leg needs: rollout-fd FIRST (process
/// ground), then the daemon leg rate-limited to one call each interval, whose
/// candidate must repeat on a second probe before it is trusted.
struct BindProbe<'a> {
    baseline: Option<&'a HashSet<String>>,
    fd_probe: Box<dyn FnMut() -> Option<String> + 'a>,
    daemon_probe: Box<dyn FnMut() -> Option<String> + 'a>,
    /// Liveness gate before a repeated daemon candidate is trusted: the
    /// detached daemon seeing a session does not prove the pane is up.
    /// `Some(false)` blocks the bind; unknown passes (Python trusts only a
    /// CONFIRMED absence).
    alive: Box<dyn Fn() -> Option<bool> + 'a>,
    now_ms_fn: Box<dyn Fn() -> i64 + 'a>,
    last_daemon_probe_ms: i64,
    prev_candidate: Option<String>,
}

impl<'a> BindProbe<'a> {
    /// The real probe: the fd oracle reads the pane child's tree, the daemon
    /// oracle diffs a fresh loaded-set against the baseline, liveness reads
    /// `pane ls`.
    fn real(
        ctx: &'a PaneContext,
        pane: &'a PaneFacts,
        baseline: Option<&'a HashSet<String>>,
    ) -> Self {
        let child_pid = pane.child_pid;
        let fd_probe = Box::new(move || child_pid.and_then(rollout_session_id_for_tree));
        let cwd = ctx.cwd.clone();
        let owned_baseline = baseline.cloned();
        let daemon_probe = Box::new(move || {
            let baseline = owned_baseline.as_ref()?;
            let loaded = discover_loaded_for_cwd(&cwd).ok()?;
            sole_new_id(&loaded, baseline)
        });
        let session = pane.session.clone();
        let pane_id = pane.pane_id;
        let alive = Box::new(move || pane_alive(&FsMux, &session, pane_id));
        BindProbe {
            baseline,
            fd_probe,
            daemon_probe,
            alive,
            now_ms_fn: Box::new(now_ms),
            last_daemon_probe_ms: 0,
            prev_candidate: None,
        }
    }

    /// The oracle-disabled state: no pre-spawn baseline, so the daemon leg
    /// is off for the whole spawn (the fd leg still runs).
    fn oracle_disabled(&self) -> bool {
        self.baseline.is_none()
    }

    /// One poll: the sole session id this pane proved, or `None`.
    fn probe(&mut self) -> Option<String> {
        if let Some(sid) = (self.fd_probe)() {
            return Some(sid);
        }
        let now = (self.now_ms_fn)();
        if now - self.last_daemon_probe_ms < CODEX_DAEMON_PROBE_INTERVAL_MS {
            // A rate-limit skip is not an observation.
            return None;
        }
        self.last_daemon_probe_ms = now;
        if self.oracle_disabled() {
            return None;
        }
        let candidate = (self.daemon_probe)();
        if candidate.is_none() || candidate != self.prev_candidate {
            // Not yet repeated: the next probe decides whether a single
            // candidate is real or a race seen half-way.
            self.prev_candidate = candidate;
            return None;
        }
        if (self.alive)() == Some(false) {
            // The candidate repeated but the pane is already gone.
            return None;
        }
        candidate
    }
}

/// The poll loop (`_await_pane_binding`, `mux_spawn.py:2081-2204`): probe,
/// then (after the first full look) the deadline, the tail capture, the
/// death check, and the deadline again - a slow mux must not buy itself
/// another bounded probe past the ceiling.
fn await_binding(
    window: Duration,
    mux: &dyn MuxCli,
    pane: &PaneFacts,
    probe: &mut BindProbe,
) -> ArmBind {
    let deadline = now_ms() + window.as_millis() as i64;
    let mut tail = String::new();
    let mut first_pass = true;
    loop {
        if let Some(sid) = probe.probe() {
            return ArmBind::Bound { session_id: sid };
        }
        if !first_pass && now_ms() >= deadline {
            return ArmBind::Unbound {
                reason: expired_reason(probe),
            };
        }
        let fresh = pane_tail(mux, pane);
        if !fresh.trim().is_empty() {
            tail = fresh;
        }
        if pane_alive(mux, &pane.session, pane.pane_id) == Some(false) {
            eprintln!(
                "codex pane bind: pane {} died before binding; last output:\n{tail}",
                pane.pane_id
            );
            return ArmBind::Died {
                reason: "pane-died-before-binding".to_string(),
            };
        }
        if now_ms() >= deadline {
            return ArmBind::Unbound {
                reason: expired_reason(probe),
            };
        }
        first_pass = false;
        std::thread::sleep(BIND_POLL_INTERVAL);
    }
}

/// The Unbound reason on a blown window: the disable note when the daemon
/// oracle never ran, the clock otherwise.
fn expired_reason(probe: &BindProbe) -> String {
    if probe.oracle_disabled() {
        "daemon: no pre-spawn baseline, oracle disabled for this spawn".to_string()
    } else {
        "binding-window-expired".to_string()
    }
}

/// Death oracle: `Some(false)` only on an absence from a NON-EMPTY `pane ls`
/// listing - an empty or unanswerable listing is UNKNOWN, never proof
/// (`mux_spawn.py`'s `_pane_absent_from_listing` asymmetry).
fn pane_alive(mux: &dyn MuxCli, session: &str, pane_id: u64) -> Option<bool> {
    let (rows, _detail) = pane_ls_detailed(session, mux);
    let rows = rows?;
    if rows.is_empty() {
        return None;
    }
    Some(
        rows.iter()
            .any(|row| row.get("pane_id").and_then(Value::as_u64) == Some(pane_id)),
    )
}

/// `mux pane read --server <session> --lines 40 <pane>`: the pane's tail,
/// kept so a death names what it saw.
fn pane_tail(mux: &dyn MuxCli, pane: &PaneFacts) -> String {
    let args: Vec<String> = [
        "mux".to_string(),
        "pane".to_string(),
        "read".to_string(),
        "--server".to_string(),
        pane.session.clone(),
        "--lines".to_string(),
        "40".to_string(),
        pane.pane_id.to_string(),
    ]
    .to_vec();
    match mux.run(&args, &[]) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
        Err(_) => String::new(),
    }
}

/// Sync form of `deliver_seed_via_codex_daemon` (async, so the sync bind
/// path wraps it like codex_inject's own callers).
fn deliver_seed_sync(thread_id: &str, seed: &str, cwd: &Path) -> Result<(), ReviewStartError> {
    let run = || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ReviewStartError::Reason("io-error"))?;
        runtime.block_on(crate::codex_inject::deliver_seed_via_codex_daemon(
            thread_id, seed, cwd,
        ))
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        let (id, text, dir) = (thread_id.to_string(), seed.to_string(), cwd.to_path_buf());
        return std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => rt.block_on(crate::codex_inject::deliver_seed_via_codex_daemon(
                    &id, &text, &dir,
                )),
                Err(_) => Err(ReviewStartError::Reason("io-error")),
            }
        })
        .join()
        .unwrap_or_else(|_| Err(ReviewStartError::Reason("io-error")));
    }
    run()
}

/// The seed-failure refusal. DEVIATION, recorded: Python's line carries
/// `{name!r}` and REAPS when the typed fallback cannot confirm the send; the
/// arm has no `name` in scope, and the typed fallback is not ported (row 9),
/// so the pane is KEPT: `after_pane: None` stops the spine's reap, and the
/// row was not yet written, leaving pane-without-row like Python's
/// unconfirmed path minus the reap.
fn seed_refusal(detail: &str, pane: &PaneFacts) -> PaneRefusal {
    PaneRefusal {
        line: format!(
            "agent seed never submitted after bind ({detail}); pane {} kept",
            pane.pane_id
        ),
        exit_code: EXIT_SPAWN_FAILED,
        after_pane: None,
    }
}

/// The fd oracle (`_codex_session_ids_for_pid`, `codex_rollout.py:9-53`):
/// every distinct codex session id open as a rollout in the child's tree.
/// Exactly one distinct id binds; zero or two return None (a same-cwd
/// sibling is never mis-identified).
fn rollout_session_id_for_tree(child_pid: u32) -> Option<String> {
    let table = crate::census::process_table().0;
    let mut pids = vec![child_pid];
    pids.extend(crate::census::descendants(&table, child_pid));
    let mut ids: HashSet<String> = HashSet::new();
    for pid in pids {
        for path in open_file_paths(pid as i32) {
            let Some(base) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if base.starts_with("rollout-") && base.ends_with(".jsonl") {
                if let Some(sid) = crate::context_window::rollout_session_id(&path) {
                    ids.insert(sid);
                }
            }
        }
    }
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

/// Every open file's path for one pid. NEW platform code (no crate helper
/// exists); empty on any failure - the oracle degrades, the spawn does not
/// break.
#[cfg(target_os = "macos")]
fn open_file_paths(pid: i32) -> Vec<PathBuf> {
    use std::mem;
    // sys/proc_info.h selector for the per-fd vnode info. libc exposes
    // PROC_PIDLISTFDS but not this one; the ABI value is stable on Darwin.
    const PROC_PIDFDVNODEINFO: libc::c_int = 3;
    let fdinfo_size = mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: null buffer, size 0: the call ANSWERS the needed buffer size.
    // errno is cleared first so a stale EPERM from an unrelated earlier call
    // cannot be honored (the claims.rs probe_pid idiom).
    unsafe { *libc::__error() = 0 };
    let list_size =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if list_size <= 0 {
        return Vec::new();
    }
    let count = list_size as usize / fdinfo_size;
    let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { mem::zeroed() }; count];
    // SAFETY: `fds` is a caller-owned zeroed buffer of exactly `list_size`
    // bytes; the kernel fills at most that.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr() as *mut libc::c_void,
            list_size,
        )
    };
    if written <= 0 {
        return Vec::new();
    }
    let filled = written as usize / fdinfo_size;
    let mut out = Vec::new();
    for fd in &fds[..filled] {
        let mut info: libc::proc_vnodepathinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        unsafe { *libc::__error() = 0 };
        // SAFETY: `info` is a caller-owned zeroed buffer of exactly `size`
        // bytes; `fd.proc_fd` names the fd this read describes.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDFDVNODEINFO,
                fd.proc_fd as u64,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if got != size {
            continue;
        }
        // vip_path is [c_char; MAXPATHLEN] (libc models it [[c_char; 32];
        // 32]); the zeroed tail guarantees NUL termination.
        let path = unsafe {
            std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char)
        };
        if let Ok(text) = path.to_str() {
            if !text.is_empty() {
                out.push(PathBuf::from(text));
            }
        }
    }
    out
}

/// Linux: the symlink targets under /proc/<pid>/fd.
#[cfg(target_os = "linux")]
fn open_file_paths(pid: i32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            out.push(target);
        }
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_file_paths(_pid: i32) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod codex_tests;
