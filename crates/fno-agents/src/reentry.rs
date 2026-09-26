//! The one resolver of a Claude re-entry.
//!
//! Every door that re-enters a Claude session (`fno agents attach`, the dead
//! and live resume arms, the mux attach/ResumeAgent gestures, the recovery
//! verb) must restore the SAME launch context the worker was spawned with or
//! refuse before launching anything. Before this module each door rebuilt its
//! own provider argv, and the account binding dispatch applies at spawn was
//! applied on none of them - a worker launched under one account was
//! reattached under whatever namespace the caller happened to sit in, which
//! either missed the transcript (loud) or billed the wrong account (silent).
//!
//! The contract, stated once here:
//!
//! - The route contract is the FULL launch context: harness session id,
//!   launch account, `CLAUDE_CONFIG_DIR`, `route_settings_path`, cwd, and the
//!   recorded substrate. A re-entry either restores all of it or refuses
//!   naming the missing piece. Missing evidence never means "default
//!   Anthropic".
//! - Secrets never cross this boundary. The plan carries PATHS and ids; the
//!   route-settings file is opened only to prove it still records a route,
//!   and no value read from it is ever emitted.
//! - Account truth lives in the Python account store, so account resolution
//!   shells `fno config accounts show <id> --print-binding` (the same
//!   shells-don't-reimplement rule the loop's account picker follows). The
//!   ambient `CLAUDE_CONFIG_DIR` is never consulted.
//! - A proven default row (`launch_account: "default"`, no route file) keeps
//!   the historical bare behavior: no account prefix, no settings flag. A
//!   ROUTED or non-Anthropic row with an UNKNOWN account refuses, because
//!   guessing a namespace is the wrong-bill door this module exists to close.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::claude_ask::JobListing;
use crate::state::{load_registry, MuxRef, Registry, RegistryEntry};

/// Exit code for a refused re-entry: the evidence is named on stderr, no argv
/// was constructed, and nothing launched.
pub const REENTRY_REFUSED_EXIT: i32 = 3;

/// The transitions this resolver serves. One vocabulary so a plan's consumer
/// can tell an attach (re-enter a live session) from a resume (relaunch a
/// dead one) from a recover (operator-selected id) without re-deriving it.
/// A revive is the mux tap: it attaches a running job and relaunches any
/// other one first, then attaches, in one argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReentryTransition {
    Attach,
    Resume,
    Recover,
    Revive,
}

impl ReentryTransition {
    pub fn as_str(self) -> &'static str {
        match self {
            ReentryTransition::Attach => "attach",
            ReentryTransition::Resume => "resume",
            ReentryTransition::Recover => "recover",
            ReentryTransition::Revive => "revive",
        }
    }

    /// True only for RECOVER, the explicit chooser. An attach re-enters a
    /// session that is already running (the row's own transport key is the
    /// target), and a smart resume continues the row's PRIMARY id - the
    /// canonical address delivery already follows - recording any different
    /// observed id as the related id rather than demanding a selection.
    /// Recovery exists precisely to select between two valid ids, so it
    /// refuses until the caller names one.
    fn requires_selection(self) -> bool {
        matches!(self, ReentryTransition::Recover)
    }

    /// True for the arms that START a process. An attach joins a session that
    /// is already running, keyed by the job's transport id, so it spends no
    /// credential and cannot bill the wrong account. The account gate refuses
    /// only where a launch could go to the wrong lane; refusing on attach
    /// strands a job the caller can physically reach.
    fn starts_a_process(self) -> bool {
        !matches!(self, ReentryTransition::Attach)
    }
}

/// The machine-readable re-entry plan. `argv` and `env` carry only ids and
/// paths; `resolved` is the positive marker a consumer must assert before
/// spawning anything.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReentryPlan {
    pub resolved: bool,
    pub transition: String,
    /// "attach" | "respawn" | "bg-resume". The transition is the
    /// caller's INTENT; this is what the plan actually does, and a consumer
    /// decides how to run it from here.
    pub mechanism: String,
    pub name: String,
    pub fno_id: Option<String>,
    /// The backlog node this row works, straight off the registry entry's own
    /// `node` field -- a different axis from `fno_id` (the thread/session
    /// identity). `mesh_identity_assignments` folds this into `FNO_NODE` on
    /// relaunch, so a caller that reaches for `fno_id` there stamps a session
    /// id where a graph node id belongs.
    pub node: Option<String>,
    /// The selected harness session id (primary, or the related id when the
    /// caller selected it).
    pub session_id: String,
    /// The 8-hex claude jobId where one is derivable ("" otherwise).
    pub short_id: String,
    /// "default" or the account id, exactly as the row records it.
    pub launch_account: String,
    /// The account's config dir, or None when the row rides the default
    /// namespace.
    pub claude_config_dir: Option<String>,
    pub route_settings_path: Option<String>,
    pub cwd: String,
    pub substrate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mux: Option<MuxRef>,
    /// The provider invocation, shell-safe. Contains ids and file PATHS
    /// only; a route's credential stays inside the 0600 settings file the
    /// argv names.
    pub argv: Vec<String>,
    /// Env the caller must apply for the argv to land in the right
    /// namespace. Only ever `CLAUDE_CONFIG_DIR`; a value is a path, never a
    /// credential.
    pub env: BTreeMap<String, String>,
}

impl ReentryPlan {
    /// Copy this plan's `--model`/`--effort` pin onto `argv`, the argv a door
    /// built itself, and hand the plan back.
    pub fn carry_pins(self, argv: &mut Vec<String>) -> Self {
        let value_after = |flag: &str| -> Option<String> {
            self.argv
                .iter()
                .position(|t| t == flag)
                .and_then(|i| self.argv.get(i + 1))
                .cloned()
        };
        let model = value_after("--model");
        let effort = value_after("--effort");
        crate::resume_pin::append_axes(argv, model.as_deref(), effort.as_deref());
        self
    }
}

/// How an account id resolves: its config dir when the lane has one. `Err`
/// carries the refusal receipt (unknown account, unresolvable record).
pub type AccountBinding = dyn Fn(&str) -> Result<Option<String>, String>;

/// The production account resolver: shells the one implementation of account
/// truth (the Python store) and parses its secret-free `--print-binding`
/// projection. Same rule as the loop's picker: never reimplement the store.
pub fn shell_account_binding(account_id: &str) -> Result<Option<String>, String> {
    let out = std::process::Command::new("fno")
        .args(["config", "accounts", "show", account_id, "--print-binding"])
        .output()
        .map_err(|e| format!("could not run `fno config accounts show`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .next_back()
            .unwrap_or("no reason given");
        return Err(reason.to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines().map(str::trim) {
        if let Some(dir) = line.strip_prefix("CLAUDE_CONFIG_DIR=") {
            if !dir.is_empty() {
                return Ok(Some(dir.to_string()));
            }
        }
    }
    // Exit 0 with no config-dir line: the account exists and rides the
    // default namespace (an api-key lane). The id resolving is the fact; the
    // namespace is the default slot.
    Ok(None)
}

/// Prove the recorded route-settings file still records a route. Reads it,
/// checks that at least one non-empty value other than the provider STAMP
/// exists, and emits nothing it read - the file holds a live credential and
/// the plan is a loggable artifact.
pub fn validate_route_settings(path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("route settings file {path} is unreadable: {e}"))?;
    let payload: Value = serde_json::from_str(&text)
        .map_err(|e| format!("route settings file {path} is malformed: {e}"))?;
    let env = payload
        .get("env")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("route settings file {path} has no env mapping"))?;
    let carries_route = env.iter().any(|(k, v)| {
        // The stamp NAMES a route; it never is one. Counting it would let a
        // scrub-floor-only file pass and relaunch with no endpoint, no auth
        // and no model - the silent wrong-bill shape.
        k != "FNO_ROUTE_PROVIDER" && v.as_str().is_some_and(|s| !s.is_empty())
    });
    if !carries_route {
        return Err(format!(
            "route settings file {path} carries only the scrub floor; the route it recorded is gone"
        ));
    }
    Ok(())
}

/// The cwd a relaunch must run in: the dir whose projects-dir slug actually
/// holds the transcript, not the (possibly stale) recorded registration cwd.
/// A session that ran EnterWorktree after registration has its transcript
/// under the worktree's project dir, while the recorded cwd is the
/// pre-EnterWorktree canonical - relaunching there looks for the transcript
/// in the wrong dir and lands on the wrong branch.
///
/// Tries the recorded cwd (which must also still EXIST) then its git
/// worktrees; the first whose `<projects>/<slug>/<uuid>.jsonl` exists wins.
/// On a miss it globs every project dir for the transcript and takes the
/// first existing directory the transcript itself records, newest file and
/// newest record first. Last fallback is the recorded cwd, naming the branch
/// on stderr (a probe that does not name the store it read is the trap the
/// king's own SKILL.md warns about).
pub(crate) fn resolve_resume_cwd(
    claude_home: &crate::claude_ask::ClaudeHome,
    recorded: &str,
    uuid: &str,
) -> std::path::PathBuf {
    if uuid.is_empty() {
        return std::path::PathBuf::from(recorded);
    }
    let projects = claude_home.projects_dir();
    let transcript = format!("{}.jsonl", uuid);
    let recorded_pb = std::path::PathBuf::from(recorded);
    let recorded_slug = crate::claude_ask::claude_cwd_slug(&recorded_pb);
    // Probe the recorded cwd first: the common case (no EnterWorktree) keeps
    // the transcript under its own project dir, and a stat is far cheaper
    // than spawning `git worktree list` on every resume. Only on a miss do
    // we enumerate candidates. A missing dir never wins, whatever its slug
    // holds: launching there would refuse anyway.
    if recorded_pb.is_dir() && projects.join(&recorded_slug).join(&transcript).exists() {
        return recorded_pb;
    }
    let candidates =
        crate::manifest_lookup::git_worktree_paths(Path::new(recorded)).unwrap_or_default();
    for cand in &candidates {
        let slug = crate::claude_ask::claude_cwd_slug(cand);
        if projects.join(&slug).join(&transcript).exists() {
            eprintln!(
                "fno agents resume: cwd resolved from the transcript's project dir ({})",
                cand.display()
            );
            return cand.clone();
        }
    }
    // Last probe before the fallback: the transcript names its own cwd on
    // every record. Newest transcript file first, newest record first; the
    // first entry whose directory still exists wins. The recorded
    // registration cwd may predate a worktree move no `git worktree list`
    // knows about.
    let mut hits: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    if let Ok(per_project) = std::fs::read_dir(&projects) {
        for proj in per_project.flatten() {
            let f = proj.path().join(&transcript);
            if !f.is_file() {
                continue;
            }
            let modified = f
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            hits.push((modified, f));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    // Only the newest records matter (the newest record carries the cwd the
    // session ended in), so read the file's TAIL, not the whole transcript.
    const TAIL_BYTES: u64 = 256 * 1024;
    for (_, f) in &hits {
        let Ok(mut file) = std::fs::File::open(f) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if std::io::Seek::seek(
            &mut file,
            std::io::SeekFrom::Start(len.saturating_sub(TAIL_BYTES)),
        )
        .is_err()
        {
            continue;
        }
        let mut text = String::new();
        if std::io::Read::read_to_string(&mut file, &mut text).is_err() {
            continue;
        }
        for line in text.lines().rev() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                if Path::new(cwd).is_dir() {
                    eprintln!(
                        "fno agents resume: cwd resolved from the transcript's own cwd record ({})",
                        cwd
                    );
                    return std::path::PathBuf::from(cwd);
                }
            }
        }
    }
    eprintln!(
        "fno agents resume: no transcript found under any candidate for {uuid}; \
         using the recorded cwd ({})",
        recorded
    );
    recorded_pb
}

fn derived_short_id(session_id: &str) -> String {
    // The claude jobId is the leading 8 hex of the session UUID by
    // construction; re-derive it only when the id actually has that shape.
    let lead = session_id.split('-').next().unwrap_or("");
    if lead.len() == 8 && lead.bytes().all(|b| b.is_ascii_hexdigit()) {
        lead.to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn substrate_of(entry: &RegistryEntry) -> &'static str {
    if entry.mux.is_some() {
        "pane"
    } else if entry.host_mode.as_deref() == Some(crate::state::HOST_MODE_INTERACTIVE) {
        "daemon"
    } else {
        "bg"
    }
}

/// Resolve one row's re-entry plan, or refuse naming the missing evidence.
///
/// Read-only: no registry write, no launch. `select_session` is the explicit
/// id selection (`--session`); it must name the row's primary or related id.
pub fn resolve_reentry_with(
    registry: &Registry,
    name: &str,
    transition: ReentryTransition,
    select_session: Option<&str>,
    account_binding: &AccountBinding,
    claude_home: &crate::claude_ask::ClaudeHome,
    cwd_override: Option<&str>,
) -> Result<ReentryPlan, String> {
    resolve_reentry_inner(
        registry,
        name,
        transition,
        select_session,
        account_binding,
        claude_home,
        cwd_override,
    )
    .map(|(plan, _)| plan)
}

/// The resolver's body: the plan plus the route recovery it performed, so the
/// registry-writing wrapper can persist what a bare row's transcript proved.
fn resolve_reentry_inner(
    registry: &Registry,
    name: &str,
    transition: ReentryTransition,
    select_session: Option<&str>,
    account_binding: &AccountBinding,
    claude_home: &crate::claude_ask::ClaudeHome,
    cwd_override: Option<&str>,
) -> Result<(ReentryPlan, Option<crate::route_recovery::Recovered>), String> {
    if name.trim().is_empty() {
        return Err("no agent named".to_string());
    }
    let candidates: Vec<&RegistryEntry> =
        registry.entries.iter().filter(|e| e.name == name).collect();
    let entry = match candidates.as_slice() {
        [] => return Err(format!("no registry row named {name:?}")),
        [one] => *one,
        _ => {
            return Err(format!(
                "agent name {name:?} is ambiguous across {} rows; name one row exactly",
                candidates.len()
            ))
        }
    };
    if entry.harness.as_deref().is_some_and(|h| h != "claude") {
        return Err(format!(
            "row {name:?} is a {:?} row; the re-entry resolver is claude-only",
            entry.harness
        ));
    }

    // Session identity: primary first, one optional related. An explicit
    // selection must name one of them. A LAUNCHING transition on a two-id row
    // refuses until the caller selects - recovery chooses a valid id, never a
    // winner, and resume has the same two valid ids in front of it.
    let primary = entry.harness_session_id.clone().unwrap_or_default();
    let related = entry.related_session_id.clone().unwrap_or_default();
    let session_id = match select_session {
        Some(id) => {
            if !id.is_empty() && (primary == id || (!related.is_empty() && related == id)) {
                id.to_string()
            } else {
                let a = if primary.is_empty() {
                    "-"
                } else {
                    primary.as_str()
                };
                let b = if related.is_empty() {
                    "-"
                } else {
                    related.as_str()
                };
                return Err(format!(
                    "session {id:?} is not one of the ids row {name:?} records (primary {a}, related {b})"
                ));
            }
        }
        None => {
            if transition.requires_selection() && !primary.is_empty() && !related.is_empty() {
                return Err(format!(
                    "row {name:?} holds two valid session ids ({primary}, {related}); name one with --session"
                ));
            }
            if !primary.is_empty() {
                primary.clone()
            } else if !related.is_empty() {
                related.clone()
            } else {
                return Err(format!(
                    "row {name:?} records no harness session id; nothing to re-enter"
                ));
            }
        }
    };
    if session_id.is_empty() {
        return Err(format!(
            "row {name:?} records no harness session id; nothing to re-enter"
        ));
    }
    // The claude jobId IS sessionId[:8] by construction, so a SELECTED id
    // derives its own short id: on a two-id row, `recover --session <related>`
    // must resolve the RELATED transport key, not the primary's cached one.
    // `entry.short_id` is only a cache of the primary's derivation.
    let short_id = if select_session.is_some_and(|s| Some(s) != entry.harness_session_id.as_deref())
    {
        derived_short_id(&session_id)
    } else if !entry.short_id.is_empty() {
        entry.short_id.clone()
    } else {
        derived_short_id(&session_id)
    };
    // Route evidence: a recorded path must still hold a route.
    if let Some(path) = entry.route_settings_path.as_deref() {
        if !path.is_empty() {
            validate_route_settings(path)?;
        }
    }

    // One binding read serves both the listing root and the config dir.
    let binding = entry
        .launch_account
        .as_deref()
        .filter(|id| *id != "default")
        .map(account_binding);
    // A revive of a running job IS an attach, so it takes the attach rules
    // below; any other job relaunches first and takes the launch rules.
    let listing = if transition == ReentryTransition::Attach || short_id.is_empty() {
        JobListing::Unread
    } else {
        let root = binding.clone().and_then(|b| b.ok().flatten());
        claude_home.listed_job(&short_id, root.as_deref().map(Path::new))
    };
    // Without the listing a live session cannot be told from a dead one, and
    // a bg resume of a live one starts a copy the pane never stops.
    if transition == ReentryTransition::Revive
        && listing == JobListing::Unread
        && !short_id.is_empty()
    {
        return Err(format!(
            "row {name:?}: `claude agents --json --all` could not be read, so job {short_id} \
             cannot be told live or dead; tap again once it answers"
        ));
    }
    let transition = if transition == ReentryTransition::Revive && listing.is_running() {
        ReentryTransition::Attach
    } else {
        transition
    };
    if short_id.is_empty() && transition == ReentryTransition::Attach {
        return Err(format!(
            "row {name:?} carries no transport key (short_id) and the session id derives none; claude attach has no target"
        ));
    }

    // A bare row (no recorded route, default-or-absent launch account) that
    // starts a process recovers its birth route from the transcript and the
    // recorded route files. `?` turns a refusal into the resolver's Err, so
    // no argv is ever built for a default-endpoint launch. An attach starts
    // no process - a live session's route was fixed at its launch.
    let recovered = if transition.starts_a_process()
        && entry
            .route_settings_path
            .as_deref()
            .is_none_or(|p| p.is_empty())
        && matches!(entry.launch_account.as_deref(), None | Some("default"))
    {
        crate::route_recovery::recover(
            crate::resume_pin::RowPins::from_entry(entry),
            &session_id,
            &claude_home.projects_dir(),
        )?
    } else {
        None
    };
    let plan_route: Option<String> = recovered
        .as_ref()
        .map(|r| r.route_settings_path.clone())
        .or_else(|| entry.route_settings_path.clone().filter(|p| !p.is_empty()));

    // The account axis. Routed or non-Anthropic rows refuse on an unknown
    // account; a proven default row keeps the historical bare behavior.
    let routed = recovered.is_some()
        || entry
            .route_settings_path
            .as_deref()
            .is_some_and(|p| !p.is_empty());
    let non_anthropic = entry
        .provider
        .as_deref()
        .is_some_and(|p| !p.is_empty() && p != "anthropic");
    let launch_account = entry
        .launch_account
        .clone()
        .or_else(|| recovered.as_ref().map(|_| "default".to_string()));
    let claude_config_dir = match launch_account.as_deref() {
        None if transition.starts_a_process() && (routed || non_anthropic) => {
            return Err(format!(
                "row {name:?} is {} and records no launch account; re-entering it would guess a namespace - re-spawn the worker",
                if routed { "routed" } else { "on a non-Anthropic provider" }
            ))
        }
        None | Some("default") => None,
        Some(id) => match binding.unwrap_or_else(|| account_binding(id)) {
            // An account that resolves to NO config dir is an api-key lane:
            // its credential lives in env the secret-free binding never
            // carries, so a plan built here would launch WITHOUT the account's
            // key - the silent wrong-bill shape this module exists to close.
            // Refuse and name the remedy; the plan never guesses an overlay.
            Ok(None) if transition.starts_a_process() => {
                return Err(format!(
                    "launch account {id:?} on row {name:?} rides an api-key lane with no config dir; \
                     its credential cannot ride a re-entry plan - re-enter under a config-dir lane \
                     or re-spawn the worker"
                ))
            }
            Ok(dir) => dir,
            // An account the store no longer resolves carries no dir to apply.
            // An attach falls back to the default root and lets `claude
            // attach` answer for the job itself; the plan keeps the recorded
            // id as billing provenance.
            Err(_) if !transition.starts_a_process() => None,
            Err(reason) => {
                return Err(format!(
                    "launch account {id:?} recorded on row {name:?} no longer resolves: {reason}"
                ))
            }
        },
    };

    // cwd: a relaunch needs a working directory that exists; an attach does
    // too (claude resolves the session against its project dir). An explicit
    // --cwd replacement (the operator re-homing a row whose worktree moved)
    // outranks the recorded value for both the check and the plan. A launch
    // with no override resolves the cwd from where the transcript actually
    // lives, so plan and launch agree on the directory claude keys the
    // session to.
    let cwd = match cwd_override.filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        _ if transition.starts_a_process() => {
            resolve_resume_cwd(claude_home, entry.cwd.as_str(), &session_id)
                .to_string_lossy()
                .into_owned()
        }
        _ => entry.cwd.clone(),
    };
    if !Path::new(&cwd).is_dir() {
        return Err(format!(
            "row {name:?} cwd {:?} is unreachable; re-entry would launch somewhere that does not exist",
            cwd
        ));
    }

    let mut argv: Vec<String> = Vec::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if let Some(dir) = claude_config_dir.as_deref() {
        env.insert("CLAUDE_CONFIG_DIR".to_string(), dir.to_string());
    }
    let mechanism: String;
    match transition {
        ReentryTransition::Attach => {
            mechanism = "attach".to_string();
            argv.push("claude".into());
            argv.push("attach".into());
            argv.push(short_id.clone());
        }
        ReentryTransition::Resume | ReentryTransition::Recover | ReentryTransition::Revive => {
            // A job `claude agents --json --all` lists restarts under
            // `claude respawn`, running or stopped. An unlisted one, or an
            // unreadable listing, comes back under its own id with
            // `claude --bg --resume` (a live session answers with a copy
            // notice the launcher refuses). A pane row takes the same arms:
            // a revival never opens a foreground `claude --resume`.
            if short_id.is_empty() {
                return Err(format!(
                    "row {name:?} derives no claude jobId from session {session_id}; \
                     no transport key for respawn or bg-resume"
                ));
            }
            let listed = matches!(listing, JobListing::Listed(_));
            // Respawn replays the job's SAVED launch, so a routed plan may
            // take it only when the saved launch still carries the plan's
            // route. A job whose saved flags name no settings file (a bad
            // relaunch rewrote them) takes bg-resume, which pushes the
            // recorded route itself.
            let job_keeps_route = match &plan_route {
                None => true,
                Some(route) => {
                    saved_job_settings(&claude_home.jobs_dir_for(&short_id).join("state.json"))
                        .is_some_and(|saved| &saved == route)
                }
            };
            if listed && job_keeps_route {
                mechanism = "respawn".to_string();
                argv.push("claude".into());
                argv.push("respawn".into());
                argv.push(short_id.clone());
            } else {
                mechanism = "bg-resume".to_string();
                argv.push("claude".into());
                argv.push("--bg".into());
                argv.push("--resume".into());
                argv.push(session_id.clone());
            }
        }
    }
    // The route rides `--settings` on every arm except `respawn`. `claude
    // respawn` restarts the job from its own saved launch, ignores extra
    // arguments with a warning (measured: "extra arguments ignored:
    // --settings ..."), and the job's saved launch already carries the route
    // the original spawn applied - appending the flag now would teach a route
    // story the binary drops. `claude --resume` and `claude --bg --resume`
    // start a process from the ambient namespace, so the recorded route must
    // ride along.
    if mechanism != "respawn" {
        if let Some(path) = plan_route.as_deref() {
            argv.push("--settings".into());
            argv.push(path.to_string());
        }
    }

    // The model pin. An unpinned `claude --resume` lands on the account
    // default, so the resume arms ask resume_pin which model the session
    // comes back on: the row's request, else the transcript's birth identity.
    // A routed plan never pins (the route owns the argv); an unresolvable row
    // leaves the argv as today - `fno agents resume` carries no model flag to
    // override a refusal with, so a pin that cannot resolve cannot block an
    // attended resume. The explicit-token guard keeps a `--model`/`--effort`
    // an earlier arm added first (the pane-to-thread transition on this same
    // arm plans to carry the live writer's own axes).
    if mechanism == "bg-resume" {
        let projects_base = claude_config_dir
            .as_deref()
            .map(|d| Path::new(d).join("projects"))
            .unwrap_or_else(|| claude_home.projects_dir());
        let transcript = crate::claude_drive::find_transcript_in(&projects_base, &session_id);
        let pins = crate::resume_pin::RowPins::from_entry(entry);
        let lookup: crate::resume_pin::RouteProviderOf<'_> =
            &|m| crate::claude_adopt::provider_from_route_settings(m);
        match crate::resume_pin::resolve(
            Some(pins),
            transcript.as_deref(),
            routed,
            &session_id,
            lookup,
        ) {
            Ok(pin) => {
                crate::resume_pin::append_axes(
                    &mut argv,
                    pin.argv_model.as_deref(),
                    pin.effort.as_deref(),
                );
            }
            Err(unpinned) => {
                // A revival the resolver cannot pin - a bare re-created row
                // whose transcript answers nothing - still comes back on the
                // axes its reap receipt rendered, never on the account
                // default. A lost-route refusal keeps today's door instead:
                // a model without its provider routes on the wrong account.
                if unpinned.lost_route.is_none() {
                    if let Some((model, effort)) = receipt_resume_axes(&session_id) {
                        crate::resume_pin::append_axes(
                            &mut argv,
                            model.as_deref(),
                            effort.as_deref(),
                        );
                    }
                }
            }
        }
    }

    if transition == ReentryTransition::Revive {
        argv = relaunch_then_attach(argv, &short_id);
    }

    Ok((
        ReentryPlan {
            resolved: true,
            transition: transition.as_str().to_string(),
            mechanism,
            name: entry.name.clone(),
            fno_id: entry.fno_id.clone(),
            node: entry.node.clone(),
            session_id,
            short_id,
            launch_account: launch_account.unwrap_or_else(|| "unknown".to_string()),
            claude_config_dir,
            route_settings_path: plan_route,
            cwd: cwd.to_string(),
            substrate: substrate_of(entry).to_string(),
            mux: entry.mux.clone(),
            argv,
            env,
        },
        recovered,
    ))
}

/// The `--settings <path>` a saved claude job replays, read from
/// `jobs/<short>/state.json`'s `respawnFlags`. `None` on any read or parse
/// miss, or when the saved flags name no settings file.
fn saved_job_settings(state: &Path) -> Option<String> {
    let text = std::fs::read_to_string(state).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    let flags = v.get("respawnFlags")?.as_array()?;
    let mut it = flags.iter().filter_map(|f| f.as_str());
    while let Some(tok) = it.next() {
        if tok == "--settings" {
            return it.next().map(str::to_string);
        }
    }
    None
}

/// One argv that runs the relaunch, then becomes `claude attach <short>`:
/// the revive's pane shows the session it just brought back. The ids ride as
/// positional arguments, never spliced into the script text.
fn relaunch_then_attach(relaunch: Vec<String>, short_id: &str) -> Vec<String> {
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        r#""$@" && exec claude attach "$0""#.to_string(),
        short_id.to_string(),
    ];
    argv.extend(relaunch);
    argv
}

/// The model axes a reap receipt recorded, for a revival the row itself
/// cannot pin. Reads the claude receipt for `session_id` and harvests the
/// `--model` / `--effort` token pairs its rendered resume argv carries.
/// `None` when no receipt answers or it names no axes.
fn receipt_resume_axes(session_id: &str) -> Option<(Option<String>, Option<String>)> {
    let path = crate::receipt::reap_receipt_path_for(
        &crate::paths::AgentsHome::from_env(),
        "claude",
        session_id,
    );
    let receipt = crate::receipt::read_reap_receipt(&path).ok()?;
    let mut tokens = receipt.resume_argv.iter();
    let mut model = None;
    let mut effort = None;
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--model" => model = tokens.next().cloned(),
            "--effort" => effort = tokens.next().cloned(),
            _ => {}
        }
    }
    (model.is_some() || effort.is_some()).then_some((model, effort))
}

/// The registry-reading wrapper the CLI action calls: load, resolve, refuse
/// on an unreadable store ("registry incomplete" is evidence missing, never
/// an empty answer).
pub fn resolve_reentry(
    registry_path: &Path,
    name: &str,
    transition: ReentryTransition,
    select_session: Option<&str>,
    cwd_override: Option<&str>,
) -> Result<ReentryPlan, String> {
    let registry = load_registry(registry_path)
        .map_err(|e| format!("registry unreadable at {}: {e}", registry_path.display()))?;
    let (plan, recovered) = resolve_reentry_inner(
        &registry,
        name,
        transition,
        select_session,
        &shell_account_binding,
        &crate::claude_ask::ClaudeHome::from_env(),
        cwd_override,
    )?;
    if let Some(r) = &recovered {
        match crate::route_recovery::persist(registry_path, &plan.session_id, r) {
            Ok(true) => eprintln!(
                "fno agents: restored route {} ({}) from the transcript's birth identity; recorded on the row",
                r.provider, r.model
            ),
            Ok(false) => {}
            Err(e) => eprintln!(
                "fno agents: restored route {} ({}); row not updated: {e}",
                r.provider, r.model
            ),
        }
    }
    Ok(plan)
}

/// The `holder <session-id>...` action: recognized when the first arg is the
/// word, at least one id follows, and every id is a lowercase UUID. The shape
/// check keeps the word off the agent-name path: `reentry-plan holder` alone
/// still resolves an agent named `holder`, and `holder <uuid>` is an
/// "unexpected argument" error today, so nothing that works now changes. A
/// `holder` arg followed by a non-UUID falls through to the existing parser
/// and its error.
fn holder_action_ids(args: &[String]) -> Option<&[String]> {
    match args.split_first() {
        Some((first, rest)) if first == "holder" => {
            if !rest.is_empty() && rest.iter().all(|a| crate::resume_wake::is_uuid_shaped(a)) {
                Some(rest)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The `fno-agents reentry-plan` machine action:
/// `reentry-plan <name> [--transition attach|resume|recover] [--session <id>]`
/// or the `reentry-plan holder <session-id>...` action, which answers the
/// claude session records without naming a registry row. Exit 0 prints the
/// answer as one JSON object; exit 3 prints the refusal on stderr and
/// constructs no argv.
pub fn run_reentry_plan(args: &[String], home: &crate::paths::AgentsHome) -> i32 {
    if let Some(ids) = holder_action_ids(args) {
        return crate::claude_sessions::run_holder_action(ids);
    }
    let mut name: Option<&str> = None;
    let mut transition = ReentryTransition::Resume;
    let mut select_session: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--transition" => match it.next().map(String::as_str) {
                Some("attach") => transition = ReentryTransition::Attach,
                Some("resume") => transition = ReentryTransition::Resume,
                Some("recover") => transition = ReentryTransition::Recover,
                Some("revive") => transition = ReentryTransition::Revive,
                other => {
                    eprintln!(
                        "reentry-plan: unknown --transition {other:?} (attach|resume|recover|revive)"
                    );
                    return 2;
                }
            },
            "--session" => match it.next() {
                Some(id) => select_session = Some(id.clone()),
                None => {
                    eprintln!("reentry-plan: --session needs a value");
                    return 2;
                }
            },
            other if !other.starts_with('-') && name.is_none() => name = Some(other),
            other => {
                eprintln!("reentry-plan: unexpected argument {other:?}");
                return 2;
            }
        }
    }
    let Some(name) = name else {
        eprintln!("reentry-plan: an agent name is required");
        return 2;
    };
    match resolve_reentry(
        &home.registry_json(),
        name,
        transition,
        select_session.as_deref(),
        None,
    ) {
        Ok(plan) => {
            match serde_json::to_string_pretty(&plan) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("reentry-plan: could not serialize the plan: {e}");
                    return 1;
                }
            }
            0
        }
        Err(reason) => {
            eprintln!("reentry: refused: {reason}");
            REENTRY_REFUSED_EXIT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_ask::ClaudeHome;
    use crate::claude_roster::{ClaudeAgentRow, ClaudeAgentsSnapshot};
    use crate::state::Registry;

    const SECRET: &str = "zai-secret-token";

    /// A claude home whose `claude agents --json --all` lists `shorts`: the
    /// one fact the respawn arm reads. An empty listing is the unlisted case.
    fn staged_home(shorts: &[&str]) -> (tempfile::TempDir, ClaudeHome) {
        let dir = tempfile::tempdir().unwrap();
        let home = ClaudeHome::at(dir.path()).with_listing(ClaudeAgentsSnapshot::known(
            shorts
                .iter()
                .map(|s| ClaudeAgentRow::new(s, Some("stopped")))
                .collect(),
        ));
        (dir, home)
    }

    fn row(name: &str) -> RegistryEntry {
        RegistryEntry {
            substrate: None,
            node: None,
            name: name.into(),
            short_id: String::new(),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            project_root: String::new(),
            session_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: crate::AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-08-27T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            keeper_child_pid: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            origin: None,
            spawn_trigger: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            legacy_claude_short_id: None,
            harness: Some("claude".into()),
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            launch_account: None,
            related_session_id: None,
            sandbox_posture: None,
            git_grant: None,
            ..Default::default()
        }
    }

    fn reg(entries: Vec<RegistryEntry>) -> Registry {
        Registry {
            schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
            entries,
        }
    }

    fn binding_ok(id: &str) -> Result<Option<String>, String> {
        if id == "makers" {
            Ok(Some("/acct/makers/cfg".into()))
        } else {
            Err(format!("account {id:?} is not registered"))
        }
    }

    fn write_route(path: &std::path::Path, floor_only: bool) {
        let env = if floor_only {
            serde_json::json!({
                "ANTHROPIC_API_KEY": "",
                "ANTHROPIC_BASE_URL": "",
                "FNO_ROUTE_PROVIDER": "zai",
            })
        } else {
            serde_json::json!({
                "ANTHROPIC_BASE_URL": "https://repro.invalid/api/anthropic",
                "ANTHROPIC_AUTH_TOKEN": SECRET,
                "FNO_ROUTE_PROVIDER": "zai",
            })
        };
        std::fs::write(path, serde_json::json!({"env": env}).to_string()).unwrap();
    }

    /// A usable zai route file for glm-5.3-flash[1m]: what route_file_for
    /// and provider_from_route_settings both search for.
    fn write_glm_route(path: &std::path::Path) {
        let env = serde_json::json!({
            "ANTHROPIC_MODEL": "glm-5.3-flash[1m]",
            "ANTHROPIC_BASE_URL": "https://repro.invalid/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": SECRET,
            "FNO_ROUTE_PROVIDER": "zai",
        });
        std::fs::write(path, serde_json::json!({"env": env}).to_string()).unwrap();
    }

    /// Stage the saved claude job state the respawn guard reads: the
    /// `respawnFlags` its state.json carries.
    fn stage_job_flags(home: &std::path::Path, short: &str, flags: &[&str]) {
        let jobs = home.join(".claude").join("jobs").join(short);
        std::fs::create_dir_all(&jobs).unwrap();
        let payload = serde_json::json!({ "state": "idle", "respawnFlags": flags });
        std::fs::write(jobs.join("state.json"), payload.to_string()).unwrap();
    }

    /// A glm-born transcript for `sid` under a projects base, as the birth
    /// pin this whole feature reads.
    fn stage_glm_birth_transcript(projects: &std::path::Path, sid: &str) -> std::path::PathBuf {
        let slug = projects.join("staged-project");
        std::fs::create_dir_all(&slug).unwrap();
        let path = slug.join(format!("{sid}.jsonl"));
        std::fs::write(
            &path,
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"glm-5.3-flash[1m]","marketingName":null}},"type":"attachment"}"#,
        )
        .unwrap();
        path
    }

    fn dir_str(p: &std::path::Path) -> &str {
        p.to_str().unwrap()
    }

    #[test]
    fn reentry_plan_resolves_a_complete_routed_glm_row() {
        let dir = std::env::temp_dir().join("reentry-test-route-a.json");
        write_route(&dir, false);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let (tmp, home) = staged_home(&["aaaaaaaa"]);
        // The saved launch still carries the row's route, so the respawn
        // guard lets `claude respawn` replay it (AC5-HP's healthy half).
        stage_job_flags(
            tmp.path(),
            "aaaaaaaa",
            &["--settings", &dir.to_string_lossy()],
        );
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert!(plan.resolved);
        assert_eq!(plan.mechanism, "respawn");
        assert_eq!(plan.claude_config_dir.as_deref(), Some("/acct/makers/cfg"));
        let want_path = dir.to_string_lossy().to_string();
        assert_eq!(
            plan.route_settings_path.as_deref(),
            Some(want_path.as_str())
        );
        // Respawn argv stays bare: `claude respawn` restarts the job from its
        // own saved launch and IGNORES extra arguments (measured warning), so
        // the recorded route rides the job state, not this argv. The route
        // file is still recorded on the plan for consumers that need it.
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "respawn".to_string(),
                "aaaaaaaa".to_string()
            ]
        );
        assert_eq!(
            plan.route_settings_path.as_deref(),
            Some(dir.to_string_lossy().to_string()).as_deref()
        );
        assert_eq!(
            plan.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/acct/makers/cfg")
        );

        // The ATTACH arm is a fresh interactive launch: there the recorded
        // route DOES ride the argv as --settings.
        let mut e2 = row("glm");
        e2.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e2.short_id = "aaaaaaaa".into();
        e2.provider = Some("zai".into());
        e2.launch_account = Some("makers".into());
        e2.route_settings_path = Some(dir.to_string_lossy().to_string());
        e2.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let attach = resolve_reentry_with(
            &reg(vec![e2]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(
            attach.argv,
            vec![
                "claude".to_string(),
                "attach".to_string(),
                "aaaaaaaa".to_string(),
                "--settings".to_string(),
                dir.to_string_lossy().to_string(),
            ]
        );
        // Secrets never cross the boundary: the token lives in the 0600 file
        // the plan only NAMES.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains(SECRET));
    }

    #[test]
    fn reentry_plan_refuses_an_unknown_account_on_a_routed_row() {
        let (_tmp, home) = staged_home(&[]);
        let dir = std::env::temp_dir().join("reentry-test-route-b.json");
        write_route(&dir, false);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.provider = Some("zai".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("no launch account"), "{err}");
    }

    #[test]
    fn attach_resolves_past_a_routed_row_with_no_launch_account() {
        // The same row the test above refuses on RESUME. An attach starts no
        // process, so the namespace it would have guessed is never applied.
        let (_tmp, home) = staged_home(&[]);
        let dir = std::env::temp_dir().join("reentry-test-route-attach.json");
        write_route(&dir, false);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.provider = Some("zai".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "attach");
        assert!(
            !plan.env.contains_key("CLAUDE_CONFIG_DIR"),
            "{:?}",
            plan.env
        );
    }

    #[test]
    fn reentry_plan_refuses_a_missing_route_file() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some("/nonexistent/route-settings/gone.json".into());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_floor_only_route_file() {
        let (_tmp, home) = staged_home(&[]);
        let dir = std::env::temp_dir().join("reentry-test-floor.json");
        write_route(&dir, true);
        let mut e = row("glm");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("scrub floor"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_an_unreachable_cwd() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("dead");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        e.cwd = "/no/such/dir/anywhere".into();
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "dead",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("unreachable"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_row_with_no_session_identity() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("blank");
        e.launch_account = Some("default".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "blank",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("no harness session id"), "{err}");
    }

    #[test]
    fn reentry_plan_keeps_a_proven_default_row_bare() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("plain");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "plain",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.argv, vec!["claude", "attach", "aaaaaaaa"]);
        assert_eq!(plan.mechanism, "attach");
        assert!(plan.env.is_empty());
        assert_eq!(plan.launch_account, "default");
    }

    #[test]
    fn reentry_plan_keeps_a_legacy_default_row_on_its_historical_behavior() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("legacy");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        // launch_account None + provider None + no route = proven legacy
        // default-Anthropic shape; the historical bare attach survives.
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "legacy",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.argv, vec!["claude", "attach", "aaaaaaaa"]);
        assert_eq!(plan.mechanism, "attach");
        assert!(plan.env.is_empty());
    }

    #[test]
    fn reentry_plan_pins_the_reap_receipts_model_axes_on_a_bare_row() {
        // A re-created bare row (no model, no route, no live transcript)
        // cannot pin its model, so the revival would land on the account
        // default. The receipt the reaper wrote carries the axes the
        // original launch ran with; the bg-resume plan harvests them.
        let _guard = crate::path_test_guard();
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("bare");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        // The receipt lives under the AGENTS home (FNO_AGENTS_HOME is that
        // root), exactly where the reaper wrote it.
        let agents_tmp = tempfile::tempdir().unwrap();
        let receipts = agents_tmp.path().join("reap-receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        std::fs::write(
            receipts.join("claude-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.json"),
            r#"{"row_name":"bare","short_id":"aaaaaaaa","harness":"claude",
                "harness_session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "cwd":"/tmp","log_path":null,
                "created_at":"2026-09-24T10:00:00Z","reaped_at":"2026-09-25T10:00:00Z",
                "resume":"claude --bg --resume aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee --model opus --effort high",
                "resume_argv":["claude","--bg","--resume","aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","--model","opus","--effort","high"]}"#,
        )
        .unwrap();
        let saved = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", agents_tmp.path());
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "bare",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        );
        match &saved {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        let plan = plan.unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert!(
            plan.argv.windows(2).any(|w| w == ["--model", "opus"]),
            "the receipt's model pin rides the argv: {:?}",
            plan.argv
        );
        assert!(
            plan.argv.windows(2).any(|w| w == ["--effort", "high"]),
            "the receipt's effort pin rides the argv: {:?}",
            plan.argv
        );
    }

    #[test]
    fn reentry_plan_names_both_ids_and_requires_selection_to_launch() {
        let (_tmp, home) = staged_home(&["aaaaaaaa", "11111111"]);
        let mut e = row("forked");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.related_session_id = Some("11111111-2222-3333-4444-555555555555".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        let r = &reg(vec![e]);

        let err = resolve_reentry_with(
            r,
            "forked",
            ReentryTransition::Recover,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("two valid session ids"), "{err}");

        // An unrecorded id is refused naming BOTH recorded ids.
        let err = resolve_reentry_with(
            r,
            "forked",
            ReentryTransition::Recover,
            Some("99999999-9999-9999-9999-999999999999"),
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("aaaaaaaa-bbbb"), "{err}");
        assert!(err.contains("11111111-2222"), "{err}");

        // Either recorded id resolves; neither replaces the other on the row.
        for id in [
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "11111111-2222-3333-4444-555555555555",
        ] {
            let plan = resolve_reentry_with(
                r,
                "forked",
                ReentryTransition::Recover,
                Some(id),
                &binding_ok,
                &home,
                None,
            )
            .unwrap();
            assert_eq!(plan.session_id, id);
        }

        // Attach needs no selection: it targets the row's own transport key.
        let plan = resolve_reentry_with(
            r,
            "forked",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        // A smart resume also needs no selection: it continues the primary
        // (the canonical address delivery follows) and never demands one.
        let plan = resolve_reentry_with(
            r,
            "forked",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    #[test]
    fn reentry_plan_refuses_a_dead_account() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("orphan");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("removed-acct".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "orphan",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("removed-acct") && err.contains("no longer resolves"),
            "{err}"
        );
    }

    #[test]
    fn attach_resolves_past_a_dead_account() {
        // the operator's blocked portal press. The row's pinned
        // account no longer resolves, but the job is running and the attach
        // reaches it by transport id. The plan carries no namespace and keeps
        // the recorded id as billing provenance.
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("king-119e-reaper");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("removed-acct".into());
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "king-119e-reaper",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "attach");
        assert_eq!(plan.launch_account, "removed-acct");
        assert!(
            !plan.env.contains_key("CLAUDE_CONFIG_DIR"),
            "{:?}",
            plan.env
        );
    }

    #[test]
    fn attach_still_carries_a_resolvable_config_dir() {
        // The positive control for the two tests above: scoping the REFUSAL
        // must not delete the BINDING. A resolvable account still names its
        // namespace on attach, so a job under a non-default root is reachable.
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("pinned");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("makers".into());
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "pinned",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(
            plan.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/acct/makers/cfg")
        );
    }

    #[test]
    fn reentry_plan_refuses_an_api_key_lane_account_it_cannot_restore() {
        let (_tmp, home) = staged_home(&[]);
        // The binding resolves the id but carries no config dir: an api-key
        // lane whose credential the secret-free plan never carries. Launching
        // bare would silently drop the account's key - refuse instead.
        let api_key_lane = |id: &str| -> Result<Option<String>, String> {
            if id == "keyacct" {
                Ok(None)
            } else {
                Err(format!("account {id:?} is not registered"))
            }
        };
        let mut e = row("keyed");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("keyacct".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "keyed",
            ReentryTransition::Resume,
            None,
            &api_key_lane,
            &home,
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("api-key lane") && err.contains("keyacct"),
            "{err}"
        );
    }

    #[test]
    fn reentry_plan_honors_a_replacement_cwd_over_an_unreachable_recorded_one() {
        let (_tmp, home) = staged_home(&["aaaaaaaa"]);
        // --cwd re-homes a row whose recorded worktree is gone: the operator's
        // live replacement outranks the recorded dir for the check and the plan.
        let mut e = row("moved");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        e.cwd = "/no/such/dir/anywhere".into();
        let live = std::env::temp_dir().to_string_lossy().to_string();
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "moved",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            Some(&live),
        )
        .unwrap();
        assert_eq!(plan.cwd, live);
    }

    #[test]
    fn reentry_plan_refuses_an_ambiguous_or_missing_row() {
        let (_tmp, home) = staged_home(&[]);
        let e1 = row("dup");
        let e2 = row("dup");
        let err = resolve_reentry_with(
            &reg(vec![e1, e2]),
            "dup",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");

        let err = resolve_reentry_with(
            &reg(vec![row("other")]),
            "ghost",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("no registry row"), "{err}");
    }

    #[test]
    fn reentry_plan_refuses_a_non_claude_row() {
        let (_tmp, home) = staged_home(&[]);
        let mut e = row("cx");
        e.harness = Some("codex".into());
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "cx",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        assert!(err.contains("claude-only"), "{err}");
    }

    /// The shared helper snapshots git's path; hand-rolling it resolved by name.
    fn _git(repo: &Path, args: &[&str]) {
        let out = crate::git_test_helpers::git_run(args, repo).unwrap();
        assert!(out.status.success(), "git {args:?} failed in {repo:?}");
    }

    #[test]
    fn resolve_resume_cwd_picks_the_transcripts_worktree_over_the_stale_recorded_cwd() {
        // Shells git: a sibling test blanks PATH, so this is the PATH-dependent
        // work PATH_TEST_MUTEX covers.
        let _p = crate::path_test_guard();
        // Registered at the canonical checkout; transcript under a worktree's
        // project dir (the EnterWorktree case). Resume must resolve to the
        // worktree, not the pre-EnterWorktree recorded cwd.
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize: macOS houses tempfile under /var/folders (a symlink to
        // /private/var/folders), and `git` records the resolved /private/var
        // path while the test's PathBuf carries /var - the slugs would diverge.
        let home = tmp.path().canonicalize().unwrap();
        let canonical = home.join("repo");
        std::fs::create_dir_all(&canonical).unwrap();
        _git(&canonical, &["init", "-q"]);
        _git(&canonical, &["config", "user.email", "t@t"]);
        _git(&canonical, &["config", "user.name", "t"]);
        _git(&canonical, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let wt = home.join("wt");
        _git(
            &canonical,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                wt.to_str().unwrap(),
            ],
        );

        let uuid = "9d2874cb-9365-48c0-aeb6-9e1d244f4cd3";
        let wt_project = ClaudeHome::at(&home)
            .projects_dir()
            .join(crate::claude_ask::claude_cwd_slug(&wt));
        std::fs::create_dir_all(&wt_project).unwrap();
        std::fs::write(wt_project.join(format!("{uuid}.jsonl")), "[]").unwrap();

        let resolved = resolve_resume_cwd(&ClaudeHome::at(home), canonical.to_str().unwrap(), uuid);
        assert_eq!(
            resolved, wt,
            "resolved to the transcript's worktree, not the recorded cwd"
        );
    }

    #[test]
    fn resolve_resume_cwd_falls_back_to_recorded_when_no_transcript_exists() {
        // No transcript under any candidate: fall back to the recorded cwd and
        // say so on stderr. An absent number beats a guessed one.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let recorded = home.join("recorded");
        std::fs::create_dir_all(&recorded).unwrap();

        let resolved = resolve_resume_cwd(
            &ClaudeHome::at(home),
            recorded.to_str().unwrap(),
            "deadbeef-0000-0000-0000-000000000000",
        );
        assert_eq!(resolved, recorded);
    }

    #[test]
    fn resolve_resume_cwd_confirms_recorded_when_its_slug_holds_the_transcript() {
        // The transcript under the recorded cwd's own slug confirms it; no
        // worktree enumeration needed.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let recorded = home.join("recorded");
        std::fs::create_dir_all(&recorded).unwrap();
        let uuid = "aaaaaaaa-0000-0000-0000-000000000000";
        let project = ClaudeHome::at(&home)
            .projects_dir()
            .join(crate::claude_ask::claude_cwd_slug(&recorded));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{uuid}.jsonl")), "[]").unwrap();

        let resolved = resolve_resume_cwd(&ClaudeHome::at(home), recorded.to_str().unwrap(), uuid);
        assert_eq!(resolved, recorded);
    }

    #[test]
    fn reentry_plan_bg_resumes_a_dead_bg_row_under_its_own_id() {
        // Job state gone, no mux ref: the plan is the same-id bg resume the
        // probe measured, not a refusal.
        let mut e = row("gone");
        e.harness_session_id = Some("9a1b2c3d-eeee-ffff-0000-111122223333".into());
        e.short_id = "9a1b2c3d".into();
        e.launch_account = Some("default".into());
        let (_tmp, home) = staged_home(&[]); // not listed
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "gone",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "--bg".to_string(),
                "--resume".to_string(),
                "9a1b2c3d-eeee-ffff-0000-111122223333".to_string(),
            ]
        );
    }

    #[test]
    fn reentry_plan_bg_resume_rides_the_recorded_route_and_namespace() {
        // A routed bg row on a config-dir account: the bg resume starts a NEW
        // process from the ambient namespace, so both the route file and the
        // config dir must ride the plan.
        let dir = std::env::temp_dir().join("reentry-test-route-bg.json");
        write_route(&dir, false);
        let mut e = row("routed");
        e.harness_session_id = Some("9a1b2c3d-eeee-ffff-0000-111122223333".into());
        e.short_id = "9a1b2c3d".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let (_tmp, home) = staged_home(&[]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "routed",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(
            plan.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/acct/makers/cfg")
        );
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "--bg".to_string(),
                "--resume".to_string(),
                "9a1b2c3d-eeee-ffff-0000-111122223333".to_string(),
                "--settings".to_string(),
                dir.to_string_lossy().to_string(),
            ]
        );
    }

    fn paned_plan(listed: &[&str]) -> ReentryPlan {
        let mut e = row("paned");
        e.harness_session_id = Some("9a1b2c3d-eeee-ffff-0000-111122223333".into());
        e.short_id = "9a1b2c3d".into();
        e.launch_account = Some("default".into());
        e.mux = Some(MuxRef {
            session: "main".into(),
            pane_id: 0,
        });
        let (_tmp, home) = staged_home(listed);
        resolve_reentry_with(
            &reg(vec![e]),
            "paned",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap()
    }

    fn revive_with(listing: ClaudeAgentsSnapshot) -> Result<ReentryPlan, String> {
        let mut e = row("tapped");
        e.harness_session_id = Some("9a1b2c3d-eeee-ffff-0000-111122223333".into());
        e.short_id = "9a1b2c3d".into();
        e.launch_account = Some("default".into());
        let dir = tempfile::tempdir().unwrap();
        let home = ClaudeHome::at(dir.path()).with_listing(listing);
        resolve_reentry_with(
            &reg(vec![e]),
            "tapped",
            ReentryTransition::Revive,
            None,
            &binding_ok,
            &home,
            None,
        )
    }

    fn revive_plan(state: Option<&str>) -> ReentryPlan {
        let rows = state
            .map(|s| ClaudeAgentRow::new("9a1b2c3d", Some(s)))
            .into_iter()
            .collect();
        revive_with(ClaudeAgentsSnapshot::known(rows)).unwrap()
    }

    #[test]
    fn a_revive_refuses_when_the_listing_cannot_be_read() {
        // A bg resume of a session that is in fact live starts a copy the
        // pane never stops, so an unread listing refuses instead.
        let err = revive_with(ClaudeAgentsSnapshot::unknown("timed out")).unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
    }

    #[test]
    fn a_revive_attaches_a_running_job() {
        for running in ["working", "blocked", "idle"] {
            let plan = revive_plan(Some(running));
            assert_eq!(plan.transition, "attach", "{running}");
            assert_eq!(plan.argv, vec!["claude", "attach", "9a1b2c3d"], "{running}");
        }
    }

    #[test]
    fn a_revive_respawns_a_listed_dead_job_then_attaches() {
        for dead in ["stopped", "failed", "done"] {
            let plan = revive_plan(Some(dead));
            assert_eq!(plan.transition, "revive", "{dead}");
            assert_eq!(plan.mechanism, "respawn", "{dead}");
            assert_eq!(
                plan.argv,
                vec![
                    "sh",
                    "-c",
                    r#""$@" && exec claude attach "$0""#,
                    "9a1b2c3d",
                    "claude",
                    "respawn",
                    "9a1b2c3d",
                ],
                "{dead}"
            );
        }
    }

    #[test]
    fn a_revive_bg_resumes_an_unlisted_job_then_attaches() {
        let plan = revive_plan(None);
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(
            &plan.argv[..7],
            &[
                "sh",
                "-c",
                r#""$@" && exec claude attach "$0""#,
                "9a1b2c3d",
                "claude",
                "--bg",
                "--resume",
            ]
        );
        assert_eq!(plan.argv[7], "9a1b2c3d-eeee-ffff-0000-111122223333");
    }

    #[test]
    fn reentry_plan_never_resumes_a_mux_row_as_a_foreground_pane() {
        // A pane row the listing does not know comes back as a background
        // job under its own id, never a foreground `claude --resume`.
        let plan = paned_plan(&[]);
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "--bg".to_string(),
                "--resume".to_string(),
                "9a1b2c3d-eeee-ffff-0000-111122223333".to_string(),
            ]
        );
    }

    #[test]
    fn reentry_plan_respawns_a_listed_job_whatever_its_files_say() {
        // `claude agents --json --all` lists the job, so it respawns. No
        // file under jobs/ was staged: the listing alone decides.
        let plan = paned_plan(&["9a1b2c3d"]);
        assert_eq!(plan.mechanism, "respawn");
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "respawn".to_string(),
                "9a1b2c3d".to_string(),
            ]
        );
    }

    #[test]
    fn reentry_plan_resolves_a_lost_cwd_from_the_transcripts_own_record() {
        // The recorded cwd is gone and no git worktree knows the session. The
        // transcript itself names the live directory; the plan must use it.
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::at(tmp.path()).with_listing(ClaudeAgentsSnapshot::known(vec![]));
        let live = tmp.path().join("live-dir");
        std::fs::create_dir_all(&live).unwrap();
        let uuid = "9a1b2c3d-eeee-ffff-0000-111122223333";
        // Transcript under the slug of the MISSING recorded dir: the first
        // probe must not win just because the slug matches.
        let recorded = tmp.path().join("gone-dir");
        let slug = crate::claude_ask::claude_cwd_slug(&recorded);
        let project = ClaudeHome::at(tmp.path()).projects_dir().join(slug);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{uuid}.jsonl")),
            format!("{{\"cwd\":\"{}\"}}\n", live.display()),
        )
        .unwrap();

        let mut e = row("moved");
        e.harness_session_id = Some(uuid.into());
        e.short_id = "9a1b2c3d".into();
        e.launch_account = Some("default".into());
        e.cwd = recorded.to_string_lossy().to_string();
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "moved",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.cwd, live.to_string_lossy().to_string());
    }

    #[test]
    fn reentry_plan_derives_the_selected_id_short_id() {
        // A two-id row: `recover --session <related>` must resolve the
        // RELATED transport key (the jobId IS sessionId[:8]), not the
        // primary's cached short_id - a respawn on the wrong key revives the
        // wrong session.
        let mut e = row("forked");
        e.harness_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.related_session_id = Some("11111111-2222-3333-4444-555555555555".into());
        e.short_id = "aaaaaaaa".into();
        e.launch_account = Some("default".into());
        let (_tmp, home) = staged_home(&["aaaaaaaa", "11111111"]);

        let plan = resolve_reentry_with(
            &reg(vec![e.clone()]),
            "forked",
            ReentryTransition::Recover,
            Some("11111111-2222-3333-4444-555555555555"),
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.short_id, "11111111");
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "respawn".to_string(),
                "11111111".to_string()
            ]
        );

        // No selection: the primary keeps its cached short_id, byte-identical
        // to the historical derivation.
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "forked",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.short_id, "aaaaaaaa");
        assert_eq!(plan.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    #[test]
    fn reentry_plan_carries_the_backlog_node_not_the_thread_ref() {
        // The row a resume relaunch stamps FNO_NODE from must be the backlog
        // node id (entry.node), never fno_id (the thread/session identity) --
        // the two are unrelated axes and mesh_identity_assignments folds
        // whatever this plan carries into FNO_NODE.
        let mut e = row("thread-worker");
        e.node = Some("x-aaaa".into());
        e.fno_id = Some("5bab90bc-1391-4b94-8e5a-bfb663268506".into());
        e.harness_session_id = Some("5bab90bc-1391-4b94-8e5a-bfb663268506".into());
        e.short_id = "5bab90bc".into();

        let (_tmp, home) = staged_home(&["5bab90bc"]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "thread-worker",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.node.as_deref(), Some("x-aaaa"));
    }

    /// An empty route dir, under the env lock: the resume-pin lookup misses,
    /// which is the unrouted shape these tests isolate from the machine.
    fn empty_route_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let routes = tempfile::tempdir().unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", routes.path());
        let path = routes.path().to_path_buf();
        (routes, path)
    }

    fn has_flag(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn ac3_hp_unrouted_row_pins_the_requested_model_on_bg_resume() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_routes, _path) = empty_route_dir();
        let mut e = row("opus");
        e.harness_session_id = Some("cccccccc-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "cccccccc".into();
        e.requested_model = Some("claude-opus-5".into());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        // No job dir staged: the bg-resume arm is the plan.
        let (_tmp, home) = staged_home(&[]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "opus",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert!(has_flag(&plan.argv, "--model", "claude-opus-5"));
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
    }

    #[test]
    fn ac3_edge_routed_row_pins_nothing() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("reentry-test-route-pin.json");
        write_route(&dir, false);
        let (_routes, _path) = empty_route_dir();
        let mut e = row("glm");
        e.harness_session_id = Some("dddddddd-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "dddddddd".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.requested_model = Some("glm-5.2".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        // No job dir: the bg-resume arm carries the route as --settings and
        // no --model, because the route owns the argv model.
        let (_tmp, home) = staged_home(&[]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert!(!plan.argv.iter().any(|t| t == "--model"));
        assert!(plan.argv.iter().any(|t| t == "--settings"));
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
    }

    #[test]
    fn ac3_edge_job_dir_respawn_argv_stays_bare() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_routes, _path) = empty_route_dir();
        let mut e = row("opus");
        e.harness_session_id = Some("eeeeeeee-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "eeeeeeee".into();
        e.requested_model = Some("claude-opus-5".into());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        // The staged job dir routes the plan to respawn, which restarts the
        // saved launch; the pin never touches it.
        let (_tmp, home) = staged_home(&["eeeeeeee"]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "opus",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "respawn");
        assert!(!plan.argv.iter().any(|t| t == "--model"));
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
    }

    #[test]
    fn bare_glm_row_with_poisoned_job_recovers_route_as_bg_resume() {
        // AC3-HP: a bare glm row, a saved job replaying the poisoned
        // `--model claude-opus-5-5` launch, and a usable zai route file:
        // the plan takes bg-resume, carries `--settings <route>` and no
        // `--model`, and the plan records the recovered route and the
        // default launch account. (On origin/main this runs respawn with a
        // bare argv - the silent opus launch.)
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("reentry-test-recover-route.json");
        write_glm_route(&dir);
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", std::env::temp_dir());
        let mut e = row("bare-glm");
        e.harness_session_id = Some("77770007-2222-3333-4444-555555555555".into());
        e.short_id = "77770007".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let (tmp, home) = staged_home(&["77770007"]);
        stage_job_flags(tmp.path(), "77770007", &["--model", "claude-opus-5-5"]);
        stage_glm_birth_transcript(&home.projects_dir(), "77770007-2222-3333-4444-555555555555");
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "bare-glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(plan.route_settings_path.as_deref(), Some(dir_str(&dir)));
        assert_eq!(plan.launch_account, "default");
        assert!(
            plan.argv
                .windows(2)
                .any(|w| w[0] == "--settings" && w[1] == dir_str(&dir)),
            "{:?}",
            plan.argv
        );
        assert!(!plan.argv.iter().any(|t| t == "--model"));
    }

    #[test]
    fn bare_glm_row_without_usable_route_refuses_with_recipe() {
        // AC3-ERR: the same bare row over only a sandboxed zai route file:
        // the resolver refuses naming the spawn --resume override, and no
        // argv is built.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let routes = std::env::temp_dir().join(format!(
            "reentry-sand-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&routes).unwrap();
        let mut sand = serde_json::json!({
            "env": {
                "ANTHROPIC_MODEL": "glm-5.3-flash[1m]",
                "ANTHROPIC_BASE_URL": "https://repro.invalid/api/anthropic",
                "ANTHROPIC_AUTH_TOKEN": SECRET,
                "FNO_ROUTE_PROVIDER": "zai",
            }
        });
        sand["sandbox"] = serde_json::json!({"workspace": "/w"});
        std::fs::write(routes.join("zai.json"), sand.to_string()).unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &routes);
        let mut e = row("bare-glm");
        e.harness_session_id = Some("77770008-2222-3333-4444-555555555555".into());
        e.short_id = "77770008".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let (tmp, home) = staged_home(&["77770008"]);
        stage_job_flags(tmp.path(), "77770008", &["--model", "claude-opus-5-5"]);
        stage_glm_birth_transcript(&home.projects_dir(), "77770008-2222-3333-4444-555555555555");
        let err = resolve_reentry_with(
            &reg(vec![e]),
            "bare-glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap_err();
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&routes).ok();
        assert!(
            err.contains(
                "fno agents spawn --resume 77770008-2222-3333-4444-555555555555 -P zai -m 'glm-5.3-flash[1m]'"
            ),
            "{err}"
        );
    }

    #[test]
    fn bare_anthropic_row_keeps_mechanism_and_pins_birth_model() {
        // AC3-EDGE: a bare row born on an Anthropic model recovers nothing;
        // the mechanism choice is unchanged, no --settings is added, and the
        // birth model rides --model on the bg-resume arm.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_routes, _path) = empty_route_dir();
        let mut e = row("bare-anthropic");
        e.harness_session_id = Some("77770009-2222-3333-4444-555555555555".into());
        e.short_id = "77770009".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let (_tmp, home) = staged_home(&[]);
        let projects = home.projects_dir();
        let slug = projects.join("staged-project");
        std::fs::create_dir_all(&slug).unwrap();
        std::fs::write(
            slug.join("77770009-2222-3333-4444-555555555555.jsonl"),
            r#"{"type":"attachment","attachment":{"type":"model","identity":{"modelId":"claude-opus-5","marketingName":"Opus 5"}},"type":"attachment"}"#,
        )
        .unwrap();
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "bare-anthropic",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(plan.route_settings_path, None);
        assert!(
            plan.argv
                .windows(2)
                .any(|w| w[0] == "--model" && w[1] == "claude-opus-5"),
            "{:?}",
            plan.argv
        );
        assert!(!plan.argv.iter().any(|t| t == "--settings"));
    }

    #[test]
    fn attach_on_bare_glm_row_recovers_nothing() {
        // AC3-EDGE: an attach starts no process, so a live session's route
        // was fixed at its launch: recovery neither fires nor refuses.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_routes, _path) = empty_route_dir();
        let mut e = row("bare-glm");
        e.harness_session_id = Some("77770010-2222-3333-4444-555555555555".into());
        e.short_id = "77770010".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let (_tmp, home) = staged_home(&[]);
        stage_glm_birth_transcript(&home.projects_dir(), "77770010-2222-3333-4444-555555555555");
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "bare-glm",
            ReentryTransition::Attach,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        assert_eq!(plan.mechanism, "attach");
        assert_eq!(plan.route_settings_path, None);
        assert!(!plan.argv.iter().any(|t| t == "--settings"));
    }

    #[test]
    fn routed_row_with_job_missing_route_takes_bg_resume() {
        // AC5-ERR: a routed row whose saved job's respawnFlags carry
        // `--model claude-opus-5-5` and no --settings takes bg-resume, which
        // pushes the row's own route.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_routes, _path) = empty_route_dir();
        let dir = std::env::temp_dir().join("reentry-test-route-c.json");
        write_route(&dir, false);
        let mut e = row("glm");
        e.harness_session_id = Some("77770011-2222-3333-4444-555555555555".into());
        e.short_id = "77770011".into();
        e.provider = Some("zai".into());
        e.launch_account = Some("makers".into());
        e.route_settings_path = Some(dir.to_string_lossy().to_string());
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        let (tmp, home) = staged_home(&["77770011"]);
        stage_job_flags(tmp.path(), "77770011", &["--model", "claude-opus-5-5"]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "glm",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        assert_eq!(plan.mechanism, "bg-resume");
        assert!(
            plan.argv
                .windows(2)
                .any(|w| w[0] == "--settings" && w[1] == dir_str(&dir)),
            "{:?}",
            plan.argv
        );
    }

    #[test]
    fn resolve_reentry_persists_recovered_route_on_the_row() {
        // AC4-HP: the registry-writing wrapper records the recovered route
        // on the row found by session id; `model` stays untouched.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("reentry-test-recover-route.json");
        write_glm_route(&dir);
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", std::env::temp_dir());
        let home_dir = std::env::temp_dir().join(format!(
            "reentry-persist-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home_dir).unwrap();
        stage_glm_birth_transcript(
            &home_dir.join(".claude").join("projects"),
            "77770012-2222-3333-4444-555555555555",
        );
        let mut e = row("bare-glm");
        e.harness_session_id = Some("77770012-2222-3333-4444-555555555555".into());
        e.short_id = "77770012".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let reg_path = home_dir.join("registry.json");
        crate::state::update_registry(&reg_path, |r| {
            r.entries.push(e);
        })
        .unwrap();
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home_dir);
        let plan = resolve_reentry(&reg_path, "bare-glm", ReentryTransition::Resume, None, None);
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        match &saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let plan = plan.unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        let loaded = crate::state::load_registry(&reg_path).unwrap();
        let stored = loaded
            .entries
            .iter()
            .find(|x| {
                x.harness_session_id.as_deref() == Some("77770012-2222-3333-4444-555555555555")
            })
            .unwrap();
        assert_eq!(stored.provider.as_deref(), Some("zai"));
        assert_eq!(stored.requested_model.as_deref(), Some("glm-5.3-flash[1m]"));
        assert_eq!(stored.launch_account.as_deref(), Some("default"));
        assert_eq!(stored.route_settings_path.as_deref(), Some(dir_str(&dir)));
        assert_eq!(stored.model, None);
        std::fs::remove_dir_all(&home_dir).ok();
    }

    #[test]
    fn unwritable_registry_still_returns_recovered_plan() {
        // AC4-ERR: a registry the wrapper cannot write still returns the
        // recovered plan; the stderr line names the failed write.
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("reentry-test-recover-route.json");
        write_glm_route(&dir);
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", std::env::temp_dir());
        let home_dir = std::env::temp_dir().join(format!(
            "reentry-unwritable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home_dir).unwrap();
        stage_glm_birth_transcript(
            &home_dir.join(".claude").join("projects"),
            "77770013-2222-3333-4444-555555555555",
        );
        let mut e = row("bare-glm");
        e.harness_session_id = Some("77770013-2222-3333-4444-555555555555".into());
        e.short_id = "77770013".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let reg_path = home_dir.join("registry.json");
        crate::state::update_registry(&reg_path, |r| {
            r.entries.push(e);
        })
        .unwrap();
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home_dir);
        std::fs::set_permissions(&home_dir, std::fs::Permissions::from_mode(0o555))
            .expect("chmod the staged home read-only");
        let plan = resolve_reentry(&reg_path, "bare-glm", ReentryTransition::Resume, None, None);
        let original_mode = std::fs::metadata(&home_dir)
            .expect("stat the staged home")
            .permissions()
            .mode();
        std::fs::set_permissions(&home_dir, std::fs::Permissions::from_mode(original_mode))
            .expect("restore the staged home's mode");
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        match &saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let plan = plan.unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert_eq!(plan.route_settings_path.as_deref(), Some(dir_str(&dir)));
        std::fs::remove_dir_all(&home_dir).ok();
    }

    #[test]
    fn ac3_edge_unresolvable_row_keeps_todays_argv() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_routes, _path) = empty_route_dir();
        let mut e = row("unpinned");
        e.harness_session_id = Some("ffffffff-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        e.short_id = "ffffffff".into();
        e.cwd = std::env::temp_dir().to_string_lossy().to_string();

        // No model axis and no transcript the resolver can find: the pin
        // cannot resolve, and the argv is exactly today's.
        let (_tmp, home) = staged_home(&[]);
        let plan = resolve_reentry_with(
            &reg(vec![e]),
            "unpinned",
            ReentryTransition::Resume,
            None,
            &binding_ok,
            &home,
            None,
        )
        .unwrap();
        assert_eq!(plan.mechanism, "bg-resume");
        assert!(!plan.argv.iter().any(|t| t == "--model"));
        assert_eq!(
            plan.argv,
            vec![
                "claude".to_string(),
                "--bg".to_string(),
                "--resume".to_string(),
                "ffffffff-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()
            ]
        );
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
    }

    fn pinned_plan(model: &str, effort: &str) -> ReentryPlan {
        ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "resume".into(),
            name: "w".into(),
            fno_id: None,
            node: None,
            session_id: "123e4567-0000-0000-0000-000000000000".into(),
            short_id: "123e4567".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: "/tmp".into(),
            substrate: "pane".into(),
            mux: None,
            argv: vec![
                "claude".into(),
                "--resume".into(),
                "123e4567-0000-0000-0000-000000000000".into(),
                "--model".into(),
                model.into(),
                "--effort".into(),
                effort.into(),
            ],
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn ac4_hp_carry_pins_copies_the_plan_pin_onto_the_door_argv() {
        let mut argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "u".to_string(),
        ];
        let plan = pinned_plan("claude-opus-5", "high");
        let returned = plan.clone().carry_pins(&mut argv);
        assert_eq!(returned, plan);
        assert_eq!(
            argv.iter().filter(|t| *t == "--model").count(),
            1,
            "exactly one --model"
        );
        assert_eq!(argv.iter().filter(|t| *t == "--effort").count(), 1);
        assert!(argv.contains(&"claude-opus-5".to_string()));
        assert!(argv.contains(&"high".to_string()));
    }

    #[test]
    fn ac4_edge_carry_pins_never_duplicates_an_explicit_flag() {
        // An argv that already names --model keeps it; a plan with no pin
        // (attach, or a routed plan) leaves the argv unchanged.
        let mut argv = vec!["claude".to_string(), "--model".to_string(), "m".to_string()];
        pinned_plan("claude-opus-5", "high").carry_pins(&mut argv);
        assert_eq!(argv.iter().filter(|t| *t == "--model").count(), 1);
        assert!(argv.contains(&"m".to_string()));
        assert_eq!(argv.iter().filter(|t| *t == "--effort").count(), 1);

        let bare = ReentryPlan {
            argv: vec!["claude".to_string()],
            ..pinned_plan("claude-opus-5", "high")
        };
        let mut untouched = vec!["claude".to_string()];
        let returned = bare.carry_pins(&mut untouched);
        assert_eq!(untouched, vec!["claude".to_string()]);
        assert_eq!(returned.argv, vec!["claude".to_string()]);
    }

    #[test]
    fn holder_action_ids_recognizes_only_the_word_plus_uuids() {
        let uuid = "bb2731c9-ad46-4303-a80d-152c68e91a4e";
        let good = vec!["holder".to_string(), uuid.to_string()];
        assert_eq!(
            holder_action_ids(&good).map(|r| r.to_vec()),
            Some(vec![uuid.to_string()])
        );

        let two = vec![
            "holder".to_string(),
            uuid.to_string(),
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string(),
        ];
        assert!(holder_action_ids(&two).is_some());

        // `holder` alone resolves an agent named holder on the name path.
        assert!(holder_action_ids(&["holder".to_string()]).is_none());
        // A non-UUID after the word falls through to the parser's error.
        assert!(holder_action_ids(&["holder".to_string(), "repro-row".to_string()]).is_none());
        // Other first words never route here.
        assert!(holder_action_ids(&["resume".to_string(), uuid.to_string()]).is_none());
        assert!(holder_action_ids(&[]).is_none());
    }
}
