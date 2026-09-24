//! What did the caller ask? The loop-check argument parser.

use super::*;

/// CLI flags parsed for `loop-check`. The three required paths are
/// non-optional by construction (fu-4faa3d): `parse_args` validates them and
/// returns `Err` on absence, so downstream code cannot forget to check.
#[derive(Debug)]
pub(crate) struct LoopCheckArgs {
    pub(super) state_path: PathBuf,
    pub(super) transcript_path: PathBuf,
    pub(super) cwd: PathBuf,
    /// Override for the GLOBAL settings file (default $HOME/.fno/
    /// settings.yaml). Tests point it at a nonexistent path for hermeticity.
    pub(super) global_settings_path: Option<PathBuf>,
    pub(super) events_path: Option<PathBuf>,
    pub(super) global_events_path: Option<PathBuf>,
    pub(super) settings_path: Option<PathBuf>,
    pub(super) ledger_path: Option<PathBuf>,
    /// Override for the fleet GitHub request budget ledger (default
    /// `$HOME/.fno/locks/github-request-budget.json`). Same hermeticity door
    /// as `--global-events`: tests pin it so a fire never reads (or seeds)
    /// the developer's live machine budget.
    pub(super) gh_budget_ledger: Option<PathBuf>,
    pub(super) now_override: Option<String>,
    pub(super) gh_bin: String,
    pub(super) git_bin: String,
    /// Override for the ambient author harness (default: the env markers read
    /// by `claims::resolve_harness`). `--author-harness none` pins "no harness".
    /// Every other ambient input here already had an override, and this one did
    /// not, so a test inherited whatever harness ran it: the four review-gate
    /// cases passed in CI and failed under `cargo test` from inside Claude Code,
    /// where the marker floors a self-review reviewer they do not expect.
    pub(super) author_harness_override: Option<String>,
    /// When set, the full Stop-hook JSON payload is read from stdin so
    /// `last_assistant_message` becomes the primary intent channel
    ///. Flag-gated so manual terminal invocations never hang
    /// on a stdin read.
    pub(super) hook_input_stdin: bool,
    /// Which driver's `done()` this fire evaluates. `target` asks whether one
    /// deliverable shipped; `king` asks whether the board is clean. They share
    /// the engine and nothing else, so `king` routes to its own decision path
    /// before the target-shaped manifest read rather than branching inside it.
    pub(super) driver: String,
    /// Override for the `fno` binary the king arm shells for its board. Same
    /// idiom as `gh_bin` / `git_bin`, and for the same reason: a test may not
    /// depend on what happens to be installed.
    pub(super) fno_bin: String,
    /// Override for the shared stop-gate read bound, in milliseconds. Tests
    /// inject a small bound so a sleeping fake wedges for ~1s instead of 30;
    /// any value <= 0 keeps the default. Production never passes it.
    pub(super) read_timeout_ms: Option<u64>,
    /// The harness whose session asked (`--harness`), and that session's id
    /// (`--harness-session`). Both must be present for the session-binding
    /// gate to run; with either absent the engine answers as it always has,
    /// so every existing caller keeps its exact behavior.
    pub(super) harness: Option<String>,
    pub(super) harness_session: Option<String>,
}

pub(crate) fn parse_args(args: &[String]) -> Result<LoopCheckArgs, String> {
    let mut state_path: Option<PathBuf> = None;
    let mut transcript_path: Option<PathBuf> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut global_settings_path: Option<PathBuf> = None;
    let mut events_path: Option<PathBuf> = None;
    let mut global_events_path: Option<PathBuf> = None;
    let mut settings_path: Option<PathBuf> = None;
    let mut ledger_path: Option<PathBuf> = None;
    let mut gh_budget_ledger: Option<PathBuf> = None;
    let mut now_override: Option<String> = None;
    let mut gh_bin =
        std::env::var("FNO_LOOPCHECK_GH_BIN").unwrap_or_else(|_| "fno-gh-loopcheck".to_string());
    let mut git_bin = std::env::var("FNO_LOOPCHECK_GIT_BIN").unwrap_or_else(|_| "git".to_string());
    let mut author_harness_override: Option<String> = None;
    let mut hook_input_stdin = false;
    let mut driver = "target".to_string();
    let mut harness: Option<String> = None;
    let mut harness_session: Option<String> = None;
    let mut fno_bin = std::env::var("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|_| "fno".to_string());
    // Env-as-default like the two bin overrides, so the real shell shim can be
    // driven end to end against a wedged child at a test bound without the
    // shim having to forward a flag it does not know about.
    let mut read_timeout_ms: Option<u64> = std::env::var("FNO_LOOPCHECK_READ_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());

    // Skip the "loop-check" verb itself if present
    let args = if args.first().map(|s| s.as_str()) == Some("loop-check") {
        &args[1..]
    } else {
        args
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        // Support both --flag value and --flag=value forms. Unknown flags are
        // tolerated (AC5-FR: forward-compat for the shim).
        if let Some(val) = try_flag_value(arg, "--state", args, &mut i) {
            state_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--transcript", args, &mut i) {
            transcript_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--cwd", args, &mut i) {
            cwd = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--events", args, &mut i) {
            events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--global-events", args, &mut i) {
            global_events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--settings", args, &mut i) {
            settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--global-settings", args, &mut i) {
            global_settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--ledger", args, &mut i) {
            ledger_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--gh-budget-ledger", args, &mut i) {
            gh_budget_ledger = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(arg, "--now", args, &mut i) {
            now_override = Some(val);
        } else if let Some(val) = try_flag_value(arg, "--gh-bin", args, &mut i) {
            gh_bin = val;
        } else if let Some(val) = try_flag_value(arg, "--read-timeout-ms", args, &mut i) {
            // A parse failure falls back to the default rather than refusing:
            // the bound is a safety ceiling, and a malformed override must
            // never wedge or kill the fire over an argument nobody validates
            // in production.
            read_timeout_ms = val.parse::<u64>().ok();
        } else if let Some(val) = try_flag_value(arg, "--git-bin", args, &mut i) {
            git_bin = val;
        } else if let Some(val) = try_flag_value(arg, "--author-harness", args, &mut i) {
            author_harness_override = Some(val);
        } else if let Some(val) = try_flag_value(arg, "--driver", args, &mut i) {
            driver = val;
        } else if let Some(val) = try_flag_value(arg, "--fno-bin", args, &mut i) {
            fno_bin = val;
        } else if let Some(val) = try_flag_value(arg, "--harness", args, &mut i) {
            harness = Some(val);
        } else if let Some(val) = try_flag_value(arg, "--harness-session", args, &mut i) {
            harness_session = Some(val);
        } else if arg == "--hook-input-stdin" {
            // Bare boolean flag (no value): try_flag_value would consume the
            // next token as a value, so it is matched directly.
            hook_input_stdin = true;
        }
        i += 1;
    }

    // Required-flag validation lives here (AC5-ERR), not downstream in decide().
    let state_path = state_path.ok_or_else(|| "--state is required".to_string())?;
    let transcript_path = transcript_path.ok_or_else(|| "--transcript is required".to_string())?;
    let cwd = cwd.ok_or_else(|| "--cwd is required".to_string())?;

    // Fail closed on an unknown driver. Tolerating one would run the target
    // gate against a manifest it cannot satisfy and burn to NoProgress while
    // looking like it was working.
    if driver != "target" && driver != "king" {
        return Err(format!(
            "unknown --driver '{driver}'; supported: 'target', 'king'"
        ));
    }

    Ok(LoopCheckArgs {
        state_path,
        transcript_path,
        cwd,
        global_settings_path,
        events_path,
        global_events_path,
        settings_path,
        ledger_path,
        gh_budget_ledger,
        now_override,
        gh_bin,
        git_bin,
        author_harness_override,
        hook_input_stdin,
        driver,
        fno_bin,
        read_timeout_ms,
        harness,
        harness_session,
    })
}

pub(crate) fn try_flag_value(
    arg: &str,
    flag: &str,
    args: &[String],
    i: &mut usize,
) -> Option<String> {
    if arg == flag {
        *i += 1;
        args.get(*i).cloned()
    } else if let Some(val) = arg.strip_prefix(&format!("{flag}=")) {
        Some(val.to_string())
    } else {
        None
    }
}
