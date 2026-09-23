//! `fno-agents` client entrypoint (Wave 3). Parses a verb + flags into a
//! JSON-RPC request, lazy-starts the daemon, forwards the request, prints the
//! result, and maps the daemon's error code to a process exit code.
//!
//! This is the thin Rust client the Python `fno agents <verb>` wrapper (Wave 6)
//! will exec. Power users can call it directly. The argv surface here is the
//! minimum that exercises every Wave 3 daemon verb end-to-end; the rich flag
//! surface (`--stream`, `--watch`, ...) lands with its verbs in later waves.

use clap::Parser as _;
use fno_agents::cli_args::{refusal_line, RestartArgs, SpawnAxes};
use fno_agents::client::resolve_daemon_bin;
use fno_agents::client::{
    call, call_if_running, check_daemon_drift, drift_from_status, ClientError,
};
use fno_agents::drift::{drift_warning, DriftState};
use fno_agents::paths::AgentsHome;
use fno_agents::protocol::{ErrorCode, Request, ResponsePayload};
use fno_agents::provider::{known_providers_csv, KNOWN_PROVIDERS};
use fno_agents::spawn_gate::machine_reading_notes;
use fno_agents::usage::{verb_help, verb_usage, CLIENT_VERB_USAGE};
use serde_json::{json, Map, Value};
use std::io::IsTerminal;

const ALL_CLIENT_ACTIONS: &[&str] = &[
    "--emit-schema",
    "active-backlog-receipt",
    "adopt",
    "announce",
    "canonical-check",
    "ask",
    "attach",
    "authorized-merge",
    "backlog-note",
    "backlog-notes",
    "bash-census",
    "board",
    "claim",
    "codex-assign-project",
    "compaction",
    "component-verdict",
    "provider-cap",
    "source-pin",
    "court-orphans",
    "court-fold",
    "detect",
    "digest",
    "distress-scan",
    "drive",
    "drive-authority",
    "evals-macro",
    "evidence-gate",
    "law-match",
    "finalize",
    "fleet-incident",
    "graph-get",
    "grid",
    "help",
    "host",
    "judge",
    "kill-check",
    "king-checkin",
    "king-escalation-text",
    "king-history",
    "reign-ledger",
    "route-slot",
    "list",
    "logs",
    "loop",
    "loop-check",
    "loops",
    "mail-inject",
    "manifest-eval",
    "manifest-for-session",
    "name-codes",
    "name-mint",
    "name-parse",
    "needs",
    "node-origin",
    "node-route",
    "notify-watch",
    "orphan-reap",
    "feed",
    "ping",
    "pr-heal",
    "probe-run",
    "honesty-sweep",
    "prove-it-verdicts",
    "test-run",
    "promote",
    "publish-review",
    "reap",
    "roster-reap",
    "reconcile",
    "reclaim",
    "plugin-install",
    "recover",
    "registry-json",
    "reentry-plan",
    "rename",
    "reign-shape",
    "reign-state",
    "report",
    "review-coverage",
    "review-summary",
    "census",
    "restart",
    "resume",
    "resume-argv",
    "review-start",
    "rm",
    "scratch",
    "session-start-bytes",
    "spawn",
    "spawn-gate",
    "spawn-overlay",
    "spawn-axes",
    "fallback-chain",
    "state",
    "status",
    "stop",
    "subscribe",
    "task-context-gate",
    "task-context-payload",
    "task-context-prepare",
    "task-context-revalidate",
    "task-context-show",
    "task-context-stage",
    "territory-rows",
    "territory-verdict",
    "trace",
    "verify-evidence",
    "version",
    "wait",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Transport-only early dispatches, before the runtime builds: the shrink
    // law (d-fe66560a) bars new client verbs; each module's doc carries its shape.
    if args.first().map(String::as_str) == Some("backlog-update") {
        std::process::exit(fno_agents::backlog::patch::run_update(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("question-intake") {
        std::process::exit(fno_agents::question_intake::run_question_intake());
    }
    // The SessionStart reconcile sweep execs here; see backlog::orphan_plans.
    if args.first().map(String::as_str) == Some("backlog-orphan-plans") {
        std::process::exit(fno_agents::backlog::orphan_plans::run_orphan_plans(
            &args[1..],
        ));
    }
    // scripts/validate-plan.sh shells HERE and reads the E/W/X/O/U line
    // protocol back.
    if args.first().map(String::as_str) == Some("surface-check") {
        std::process::exit(fno_agents::surface_check::run_surface_check(&args[1..]));
    }
    // cli/src/fno/pr/_sync_canonical.py transports HERE through verb_call:
    // the post-merge canonical sync + its catch-up sweep and staleness
    // alarm, native. Registers no verb (the shrink law allows no new
    // action); the transport reaches it through resolve_binary like the
    // other early arms.
    if args.first().map(String::as_str) == Some("sync-canonical") {
        std::process::exit(fno_agents::sync_canonical::run_sync_canonical_verb(
            &args[1..],
        ));
    }
    // `launch-workdir`: the spawn door's launch-cwd resolution (see
    // launch_workdir.rs doc). Transport-only, like sync-canonical: registers
    // no client action (the shrink law allows none); Python's
    // ensure_launch_workdir reaches it through verb_call, and a `hold`
    // answer is a valid exit-0 answer the caller renders.
    if args.first().map(String::as_str) == Some("launch-workdir") {
        std::process::exit(fno_agents::launch_workdir::run_launch_workdir(&args[1..]));
    }
    // `worktree-reapable`: the worktree-removal gate, daemon-free, transport-only
    // (no client action - shrink law); callers: worktree_gate.py, worktree-reapable.sh.
    if args.first().map(String::as_str) == Some("worktree-reapable") {
        std::process::exit(fno_agents::worktree_reapable::run_client(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("pending-session-row") {
        std::process::exit(fno_agents::pending_session_row::run(&args[1..]));
    }
    // hooks/context-run.sh is the only caller.
    if args.first().map(String::as_str) == Some("context-run") {
        std::process::exit(fno_agents::context_run::run_context_run(&args[1..]));
    }
    // A fire answers in microseconds; the runtime never builds for one.
    if args.first().map(String::as_str) == Some("hook") {
        std::process::exit(fno_agents::hook::dispatch(&args[1..]));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    // The Python evals phase reaches this arm through resolve_binary.
    if args.first().map(String::as_str) == Some("evals-arm") {
        std::process::exit(fno_agents::evals_arm::run_evals_arm(&args[1..]));
    }
    // `evals-trend` and `corrections-verify`: transport-only folds (the
    // shrink law allows no new `run` arm); their module docs carry the
    // contract, autocorrect-pack.sh embeds the verify one.
    if args.first().map(String::as_str) == Some("evals-trend") {
        std::process::exit(fno_agents::evals_trend::run_evals_trend(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("corrections-verify") {
        std::process::exit(fno_agents::corrections_verify::run(&args[1..]));
    }
    // `evals-attempt`: the native attempt-eligibility verdict for eval history
    // rows. Transport-only, dispatched here like evals-trend: the
    // Python runner asks it per attempt at write time; the report re-asks it
    // over whole-history batches at read time.
    if args.first().map(String::as_str) == Some("evals-attempt") {
        std::process::exit(fno_agents::eval_attempt::run_evals_attempt(&args[1..]));
    }
    // `pr-park`: the park-record owner behind `fno do pr watch`; dispatches
    // here like evals-arm because the shrink law bars a new `run` arm.
    if args.first().map(String::as_str) == Some("pr-park") {
        std::process::exit(fno_agents::pr_park::run(&args[1..]));
    }
    let code = rt.block_on(run(args));
    std::process::exit(code);
}

async fn run(args: Vec<String>) -> i32 {
    if args.is_empty() {
        print_help();
        return 0;
    }
    let verb = args[0].as_str();
    if matches!(verb, "-h" | "--help" | "help") {
        print_help();
        return 0;
    }

    // `version` / `-V` / `--version`: report which commit this binary was built
    // from -- the prerequisite for Rust-side `fno doctor`
    // staleness. `--json` emits the machine surface `fno doctor` reads off the
    // resolved binary path. Side-effect-free, like `--emit-schema`/`help`: it
    // never starts the daemon and is NOT a routable daemon verb, so it stays out
    // of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS (callers invoke the binary
    // directly). Matched here rather than as a dispatch arm so the routable-verb
    // parity guard (test_rust_client_verbs_match_client_rs) does not see it.
    if matches!(verb, "version" | "-V" | "--version") {
        let json = fno_agents::json_output::requested(&args[1..]);
        fno_agents::version::print_version(json);
        return 0;
    }

    // `mail-inject` is the one-shot LIVE-DELIVERY verb `fno agents mail send` calls to
    // inject a turn into a live `claude --bg` session over the daemon control.sock
    // (node). Binary-direct (Python `_deliver_live` subprocess), NOT a
    // routable `fno agents` verb -- matched with `matches!` (like `version`) so the
    // parity guard (test_rust_client_verbs_match_client_rs) does not see it and it
    // stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS. Connects to an existing
    // daemon; never lazy-starts one.
    if matches!(verb, "mail-inject") {
        return fno_agents::mail_inject::run_mail_inject(&args[1..]).await;
    }

    if matches!(verb, "manifest-eval") {
        return fno_agents::manifest::run_manifest_eval(&args[1..]);
    }

    // `orphan-reap` is the hidden reap lever for orphaned cargo test binaries:
    // the daemon's 300s sweep and a human on a wedged box both run
    // it binary-direct. Same `matches!` treatment as `mail-inject` so it stays
    // out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS and the routable-verb
    // parity guard - it reads one ps snapshot and prints, it is not an
    // `fno agents` verb.
    if matches!(verb, "orphan-reap") {
        return fno_agents::orphan_reap::run_orphan_reap(&args[1..]);
    }

    // `reentry-plan` is the INTERNAL machine resolver behind every
    // Claude re-entry door: the Rust/Python attach+resume arms and
    // the mux gestures consume its verdict instead of each rebuilding a
    // provider argv. Matched with `matches!` (like `claim`/`detect`) so it
    // stays out of the routable-verb parity sets - it is not an `fno agents`
    // verb, and adding one needs the Python surface to grow with it.
    if matches!(verb, "reentry-plan") {
        return fno_agents::reentry::run_reentry_plan(&args[1..], &AgentsHome::from_env());
    }

    // `resume-argv` is the INTERNAL machine verb behind the mux resume
    // gesture: the server shells it for the codex lane instead of
    // re-deriving the declared form and losing the writable-roots grant.
    // Same `matches!` treatment as `reentry-plan` - it is not an `fno agents`
    // verb, so the routable-verb parity guard never sees it.
    if matches!(verb, "resume-argv") {
        return fno_agents::pane_relaunch::run_resume_argv(&args[1..]);
    }

    if matches!(verb, "manifest-for-session") {
        return fno_agents::manifest_lookup::run_manifest_for_session(&args[1..]);
    }

    // `name-mint`/`name-parse`/`name-codes` are the INTERNAL machine verbs
    // behind the delegation flip: Python's naming.py shells them so the
    // vocabulary tables, mint, and parse own exactly one implementation.
    // Matched with `matches!` like `reentry-plan` - they are not `fno agents`
    // verbs, so the routable-verb parity sets never see them.
    if matches!(verb, "name-mint") {
        return fno_agents::naming::run_name_mint(&args[1..]);
    }
    if matches!(verb, "name-parse") {
        return fno_agents::naming::run_name_parse();
    }
    if matches!(verb, "name-codes") {
        return fno_agents::naming::run_name_codes(&args[1..]);
    }

    // `review-summary` is the display-line author for a pre-push reviewed PR:
    // the /pr create worker runs it against the attestation journal (global
    // state root by default) and appends its stdout to the body. Same
    // `matches!` treatment as `claim`/`detect` so the
    // routable-verb parity guard does not see it - it reads one file and
    // prints, it is not an `fno agents` verb.
    if matches!(verb, "review-summary") {
        return fno_agents::review_summary::run_review_summary(&args[1..]);
    }

    // `component-verdict` is the HIDDEN decision verb for deployed-component
    // convergence: reads one JSON request on stdin (expected rev +
    // per-component probes) and prints the per-component verdict. Binary-direct
    // transport for update/doctor, matched with `matches!` like `state` so it
    // stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS and the parity guard.
    if matches!(verb, "component-verdict") {
        return fno_agents::component_update::run_component_verdict(&args[1..]);
    }

    // `source-pin` is the HIDDEN decision verb for machine-wide update source
    // eligibility: resolve|record|sync. A refusal is data (exit 0,
    // decision: refuse), not a process error, so the Python transport can map
    // it to its own refusal message verbatim. Same `matches!` treatment as
    // `component-verdict` so the routable-verb parity guard does not see it.
    if matches!(verb, "source-pin") {
        return fno_agents::source_pin::run_source_pin(&args[1..]);
    }

    // `task-context-prepare`/`-gate`/`-stage`/`-show`/`-revalidate`/`-payload`
    // are the INTERNAL machine verbs behind the task-context execution binding
    // the doors (target init, resume receipt validate/show, spawn
    // payload adapter) shell them so every enforced decision (validation,
    // digest, stage monotonicity, live-source revalidation, declared-gate env
    // semantics, bounded payload render) is native. stdin-JSON like
    // evidence-gate, so the routable-verb parity sets never see them.
    if matches!(verb, "task-context-prepare") {
        return fno_agents::task_context::run_prepare(&args[1..]);
    }
    if matches!(
        verb,
        "task-context-gate"
            | "task-context-stage"
            | "task-context-show"
            | "task-context-revalidate"
            | "task-context-payload"
    ) {
        return match verb {
            "task-context-gate" => fno_agents::task_context::run_gate(&args[1..]),
            "task-context-stage" => fno_agents::task_context::run_stage(&args[1..]),
            "task-context-show" => fno_agents::task_context::run_show(&args[1..]),
            "task-context-payload" => fno_agents::task_context::run_payload(&args[1..]),
            _ => fno_agents::task_context::run_revalidate(&args[1..]),
        };
    }

    // `evidence-gate` is the hidden binary-direct transport for the ruling and
    // note evidence gates: the checker + bounded read runner ported
    // out of the file-budget-gated Python `fno.decide.evidence` module. Reads
    // one JSON request on stdin (lane, text, reads, root, timeout) and prints
    // one JSON answer on stdout; a refusal is data (`ok: false`), not a
    // process error. Same `matches!` treatment as `component-verdict`, so no
    // advertised fno verb is added.
    if matches!(verb, "evidence-gate") {
        return fno_agents::evidence::run_evidence_gate(&args[1..]);
    }

    // `law-match` is the hidden binary-direct transport for the question-to-law
    // matcher. Same `matches!` treatment as `evidence-gate`, so no advertised
    // fno verb is added.
    if matches!(verb, "law-match") {
        return fno_agents::law_match::run_law_match(&args[1..]);
    }

    // `king-escalation-text` is the hidden binary-direct transport for the
    // king escalation renderer (ported out of `fno.king.escalate`): Python
    // keeps the question fold and the liveness read, this side only renders.
    // Same `matches!` treatment as `law-match`, so no advertised fno verb is
    // added.
    if matches!(verb, "king-escalation-text") {
        return fno_agents::king_escalation::run_king_escalation_text(&args[1..]);
    }

    // `fleet-task` is the hidden binary-direct transport the Python reconcile
    // lanes ride through verb_call (fleet_task.rs). Same `matches!` treatment
    // as `law-match`: no advertised fno verb.
    if matches!(verb, "fleet-task") {
        return fno_agents::fleet_task::run_fleet_task(&args[1..]);
    }

    // `review-start` (hidden codex review verb, node): the app-server
    // `review/start` RPC; claude's `--raw /code-review` counterpart; no advertised verb.
    if matches!(verb, "review-start") {
        return fno_agents::codex_inject::run_review_start(&args[1..]).await;
    }

    // `scratch` is the scratch-shape sweep: `fno doctor scratch
    // sweep|report` routes here binary-direct via the Python leaf. Matched
    // with `matches!` like `mail-inject` so the routable-verb parity guard
    // does not see it - it is not an `fno agents` verb; the doctor group
    // owns its surface.
    if matches!(verb, "scratch") {
        return fno_agents::scratch::run_cli(&args[1..]);
    }

    // `codex-assign-project` is the hidden project-assignment verb:
    // resolve or create the repo's codex project for --cwd, and when
    // --thread-id is given, assign that bound thread to it. The Python headless
    // create lane shells this binary fire-and-forget after `thread.started`.
    // Same `matches!` treatment as `review-start` so it stays out of
    // CLIENT_VERB_USAGE / RUST_CLIENT_VERBS and the routable-verb parity guard.
    if matches!(verb, "codex-assign-project") {
        return fno_agents::codex_inject::run_codex_assign_project(&args[1..]).await;
    }

    // `claim` is the HIDDEN debug front over the native claims module
    // (`fno_agents::claims`): the cross-impl compatibility matrix drives the
    // Rust side of the lockfile protocol through it, and it doubles as an ops
    // escape hatch when the Python CLI is unavailable. Matched with `matches!`
    // (like `mail-inject`) so the routable-verb parity guard does not see it
    // and it stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS — `fno agents claim`
    // remains the only operator CLI for claims.
    if matches!(verb, "claim") {
        return fno_agents::claim_verbs::run_claim(&args[1..]);
    }

    // `detect` is the HIDDEN debug front over the screen-manifest fallback
    // authority (`fno_agents::scrape`): `detect explain <agent>` prints which
    // rung of the badge lattice currently badges the agent. Same `matches!`
    // treatment as `claim` so it stays out of CLIENT_VERB_USAGE /
    // RUST_CLIENT_VERBS and the parity guard.
    // `node-origin` is the HIDDEN transport verb over the request-origin
    // decision (fno_agents::node_origin): Python birth assembly posts birth
    // records and stamps the receipt. Same `matches!` treatment as `claim`
    // so the routable-verb parity guard does not see it; the verb IS
    // registered in ALL_CLIENT_ACTIONS because the verb-surface ratchet's
    // binary probe reads the unknown-verb refusal.
    if matches!(verb, "node-origin") {
        return fno_agents::node_origin::run_node_origin(&args[1..]);
    }

    if matches!(verb, "detect") {
        return fno_agents::scrape::run_detect(&args[1..]);
    }

    // Per-verb help: `fno agents <verb> --help` prints that verb's usage line
    // and exits 0, instead of the verb's arg parser erroring "unknown flag:
    // --help" / "takes no arguments". Only fires for a recognized
    // verb; an unknown verb falls through to its normal error path. The scan
    // stops at an `--argv`/`--` boundary so a `--help` inside a spawn/host argv
    // payload reaches the spawned command instead of being captured here.
    if is_help_request(&args[1..]) {
        // A verb with a full help body owns its --help; the one-line table
        // entry stays for the top-level list.
        if let Some(body) = verb_help(verb) {
            println!("{body}");
            return 0;
        }
        if let Some(usage) = verb_usage(verb) {
            println!("usage: fno-agents {usage}");
            return 0;
        }
    }

    // `--emit-schema` is a read-only introspection flag: prints the unified
    // envelope + status-v1 schema + known event kinds as JSON to stdout, then
    // exits 0. Used by scripts/check-event-schema-parity.sh. Must not start
    // the daemon or read any runtime state (AC2-HP: side-effect-free).
    if verb == "--emit-schema" {
        let schema = fno_agents::emit_schema_json();
        match serde_json::to_string_pretty(&schema) {
            Ok(s) => {
                println!("{s}");
                return 0;
            }
            Err(e) => {
                eprintln!("fno-agents --emit-schema: serialization error: {e}");
                return 1;
            }
        }
    }

    // `loop-check`: stop-hook decision verb (see loopcheck.rs module doc).
    // Direct dispatch; no daemon RPC.
    if verb == "loop-check" {
        return fno_agents::loopcheck::run_loop_check(&args[1..]);
    }

    // `loops`: daemon-free global pause sentinel owner. Direct dispatch keeps
    // the safety switch usable when the daemon itself is unavailable.
    if verb == "loops" {
        return fno_agents::loops_pause::run_loops(&args[1..]);
    }

    // `state path` is the shell-hook surface for project-space path resolution
    // (matched with `matches!` like `version`/`mail-inject`: a binary-direct
    // verb that stays out of CLIENT_VERB_USAGE / RUST_CLIENT_VERBS and so out
    // of the client<->router parity guard; the verb-surface ratchet's binary
    // probe reads the unknown-verb refusal, so the verb IS registered in
    // ALL_CLIENT_ACTIONS like every direct dispatch). No daemon, from cwd.
    if matches!(verb, "state") {
        return fno_agents::state_path::run(&args[1..]);
    }

    // `honesty-sweep`: declared-vs-measured sweeps over declared populations
    // (see its own doc in honesty_sweep.rs). Direct dispatch; no daemon RPC -
    // a sweep reads tables, manifests, or piped rows from disk.
    if verb == "honesty-sweep" {
        return fno_agents::honesty_sweep::run_honesty_sweep(&args[1..]);
    }

    // `probe-run`: see its own doc in acceptance_evidence.rs. Direct dispatch.
    if verb == "probe-run" {
        return fno_agents::acceptance_evidence::run_probe_run(&args[1..]);
    }

    // `prove-it-verdicts`: the one reader for terminal prove-it records
    // (see its own doc in prove_it_verdicts.rs). Direct dispatch; no
    // daemon RPC - a verdict read walks the graph and plan artifacts files.
    if verb == "prove-it-verdicts" {
        return fno_agents::prove_it_verdicts::run_prove_it_verdicts(&args[1..]);
    }

    // `test-run`: the native process-group owner behind `fno doctor test`
    // (see test_run.rs doc). Direct dispatch, no daemon RPC - a test run must
    // not depend on a live daemon to clean up after itself. Same `matches!`
    // treatment as `probe-run`/`state` so it stays out of CLIENT_VERB_USAGE /
    // RUST_CLIENT_VERBS and the routable-verb parity guard: not an `fno
    // agents` verb; callers are `cli/src/fno/test_runner.py` and the wrapper.
    if verb == "test-run" {
        return fno_agents::test_run::run_test_run(&args[1..]);
    }

    // `fleet-incident`: the durable fleet incident breaker (see
    // fleet_incident.rs doc). Direct dispatch, no daemon RPC: a stop must be
    // writable even when the daemon is the thing wedged. Python's `fno agents
    // incident` adapter relays it; the admission gates call the library in
    // process. Same `==` dispatch + ALL_CLIENT_ACTIONS registration as
    // `test-run`, so the parity tests stay in sync. The fleet GitHub request
    // budget rides THIS action as its `gh-budget` argument (law d-fe66560a
    // allows no new client action): an admit must answer before every gh
    // call, including when the daemon is the thing wedged, so it dispatches
    // here with no daemon RPC either.
    if verb == "fleet-incident" {
        if args.get(1).map(String::as_str) == Some("gh-budget") {
            return fno_agents::gh_budget::run_gh_budget(&args[2..]);
        }
        if args.len() < 2 {
            match fno_agents::gh_budget::run_gh_budget_stdin_door() {
                -1 => {}
                code => return code,
            }
        }
        return fno_agents::fleet_incident::run_fleet_incident(&args[1..]);
    }

    if verb == "compaction" {
        return fno_agents::compaction::run_compaction(&args[1..]);
    }

    // `provider-cap`: the armed cap actor's read + decide verbs (see
    // provider_cap.rs doc). Direct dispatch, no daemon RPC: a status read
    // computes on demand when no fresh daemon snapshot exists, and a decision
    // record must be writable when the daemon is the thing wedged.
    if verb == "provider-cap" {
        return fno_agents::provider_cap_verbs::run_provider_cap(&args[1..]);
    }

    // `announce`: fleet announcements (see announce.rs doc). Direct
    // dispatch; no daemon RPC - a send is one locked bus append, a read is a
    // scan + cursor write, and both must work when the daemon is wedged.
    if verb == "announce" {
        return fno_agents::announce::run_announce(&args[1..]);
    }

    // `capabilities` / `target-family` (change 2): read-only leaves
    // over the packaged capability table and the merge-posture family table,
    // `review-coverage`: standalone review_coverage producer (see its own doc
    // in loopcheck.rs). Direct dispatch like loop-check; no daemon RPC.
    if verb == "review-coverage" {
        return fno_agents::loopcheck::run_review_coverage(&args[1..]);
    }

    // `loop run`: unified driver loop (see loop_target.rs doc). Direct
    // dispatch like loop-check; no daemon RPC.
    if verb == "loop" {
        return fno_agents::loop_target::run_loop_verb(&args[1..]);
    }

    // `finalize`: terminal-only side-effect WRITER (see finalize.rs doc). Direct
    // dispatch; no daemon RPC.
    if verb == "finalize" {
        return fno_agents::finalize::run_finalize(&args[1..]);
    }

    // `kill-check`: Rust port of scripts/lib/kill-criteria.sh (see
    // kill_criteria.rs doc). Direct dispatch; no daemon RPC.
    if verb == "kill-check" {
        return fno_agents::kill_criteria::run_kill_check(&args[1..]);
    }

    // `authorized-merge`: the one merge/arm authorization (see
    // authorized_merge.rs doc). Direct dispatch; no daemon RPC. `fno do pr
    // merge` sends one JSON payload and reads one receipt back, so the merge
    // verb and finalize cannot answer "may this head merge?" differently.
    if verb == "authorized-merge" {
        return fno_agents::authorized_merge::run_authorized_merge(&args[1..]);
    }

    // `route-slot`: the delivery-slot resolver (see route_slot.rs doc). Direct
    // dispatch; no daemon RPC. Python's spawn seam, advance and the readouts
    // send one JSON payload and read {status, candidate, chain} back.
    if verb == "route-slot" {
        return fno_agents::route_slot::run_route_slot(&args[1..]);
    }

    // `spawn-gate`: the ONE spawn gate (see spawn_gate_verb.rs doc). Direct
    // dispatch; no daemon RPC. The Python transport sends one JSON payload
    // and reads the admit (with the held claim keys) or the refusal back.
    if verb == "spawn-gate" {
        return fno_agents::spawn_gate_verb::run_spawn_gate(&args[1..]);
    }

    // `spawn-overlay`: the harness-keyed spawn-defaults resolver (see
    // spawn_overlay.rs doc). Direct dispatch; no daemon RPC. Python's spawn
    // seam, the doctor readout and the failover walker send one JSON payload
    // and read the answer back.
    if verb == "spawn-overlay" {
        return fno_agents::spawn_overlay::run_spawn_overlay(&args[1..]);
    }

    // `spawn-axes`: the billing axes of the spawn seam (route/account/model),
    // decided in one place (see spawn_axes.rs doc). The Python front door
    // projects the seam's facts and applies the returned plan verbatim.
    if verb == "spawn-axes" {
        return fno_agents::spawn_axes::run_spawn_axes(&args[1..]);
    }

    // `permission-tokens`: the one permission vocabulary and mappability
    // answer (see codex_posture.rs). Direct dispatch; no daemon RPC. Python's
    // pane mapper, the spawn seam's mappability read and both front doors
    // call this so the tree holds ONE table, not three disagreeing copies.
    if verb == "permission-tokens" {
        return fno_agents::codex_posture::run_permission_tokens(&args[1..]);
    }

    // `sandbox-probe`: the pre-seating sandbox verdict (see sandbox_probe.rs).
    // Direct dispatch; no daemon RPC. Python's spawn gate (rust_runtime.py)
    // calls it and owns the exit-85 refusal; the verb never refuses on its
    // own - it answers, the caller judges.
    if verb == "sandbox-probe" {
        return fno_agents::sandbox_probe::run_sandbox_probe(&args[1..]);
    }

    // `fallback-chain`: the failover chain walk (see fallback_chain.rs doc).
    // Python resolves config and paths and serializes the candidate links;
    // this verb reads the provider runtime-state file, derives headroom
    // verdicts, and answers the eligible links with their walk-memory ids
    // and spawn flags.
    if verb == "fallback-chain" {
        return fno_agents::fallback_chain::run_fallback_chain(&args[1..]);
    }

    // `publish-review`: the reviewer lane's second GitHub identity (see
    // publish_review.rs doc). Direct dispatch; no daemon RPC. Python's emit
    // chokepoint and the hidden `fno pr publish-review` verb send one JSON
    // payload and read the answer back; the verb is binary-first, never an
    // auto-routed `fno agents` surface.
    if verb == "publish-review" {
        return fno_agents::publish_review::run_publish_review(&args[1..]);
    }

    // `canonical-check`: the canonical-sync divergence read. Direct
    // dispatch; no daemon RPC. The Python post-merge sync sends one JSON
    // payload and reads the answer back; binary-first like `publish-review`.
    if verb == "canonical-check" {
        return fno_agents::canonical_check::run_canonical_check(&args[1..]);
    }

    // `sync-canonical`: the post-merge canonical sync + its catch-up sweep
    // and staleness alarm, native. The Python `fno do pr sync-canonical`
    // transport sends one JSON payload and reads the answer back;
    // binary-first like `canonical-check`.
    if verb == "sync-canonical" {
        return fno_agents::sync_canonical::run_sync_canonical_verb(&args[1..]);
    }

    // `reign-state`/`reign-shape`: the reign reader and the shape rewrite (see
    // loop_reign.rs doc). Direct dispatch, daemon-free reads; the Python
    // `fno agents king shape` shell and escalate's client invoke the binary
    // directly rather than routing through the agents verb set.
    if matches!(verb, "reign-state") {
        return fno_agents::loop_reign::run_reign_state(&args[1..]);
    }
    if matches!(verb, "reign-shape") {
        return fno_agents::loop_reign::run_reign_shape_or_term(&args[1..]);
    }

    // `graph-get`/`bash-census`/`session-start-bytes`: daemon-free reads, not routable `fno agents` verbs (same reasoning as kill-check).
    if verb == "graph-get" {
        return fno_agents::graph_get::run_graph_get(&args[1..]);
    }
    // `judge`: the blueprint judge's grading half (lens prompts,
    // model spawn, verdict parsing). Daemon-free like graph-get; the Python
    // `fno doctor observer judge` / `sweep --judge` wrappers shell HERE and
    // own event emission (fno.events single-sourced there).
    if verb == "judge" {
        return fno_agents::blueprint_judge::run_judge(&args[1..]);
    }

    // `backlog-notes` (wave 3): inventory, digest migration, and
    // history readback over the note corpus. Direct dispatch, daemon-free.
    if verb == "backlog-notes" {
        return fno_agents::backlog::note_migrate::run_notes(&args[1..]);
    }

    // `backlog-note`: the native note action. Daemon-free write; the
    // Python `fno backlog note` bridge owns evidence checks, identity,
    // archived refusal, crown candidates and the mail transport, this action
    // owns the bounded-state policy, revision-checked replacement, history
    // routing, and the nobody-bound pre-write refusal (exit 3).
    if verb == "backlog-note" {
        return fno_agents::backlog::note_cli::run_note(&args[1..]);
    }
    // `court-orphans`: the orphan-crown sweep for `fno agents court`,
    // daemon-free read; `==` dispatch like graph-get, and registered in
    // ALL_CLIENT_ACTIONS like every direct dispatch the ratchet counts.
    if verb == "court-orphans" {
        return fno_agents::loop_reign::run_court_orphans(&args[1..]);
    }
    // `court-fold`: the crown scope fold for `fno agents court
    // --nodes` and the local board's court section, daemon-free like
    // court-orphans; the workers column rides the same native claim verdicts
    // `claim sweep` established, so a fold and the claims surface cannot
    // disagree about who holds a node.
    if verb == "court-fold" {
        return fno_agents::court_fold::run_court_fold(&args[1..]);
    }

    // `king-history`: the crown-scope reign_checkin readback for
    // `fno agents king history`. Daemon-free read, `==` dispatch like
    // court-fold: Python resolves the caller's crown scope and pins the
    // journal path (identity and paths are Python-owned), the native side
    // owns the scan so the file-budget Python-tree ratchet holds.
    // `--verdict` is the same journals read as a tenure verdict (law
    // d-fe66560a: the verdict rides this action as an argument, never a
    // new action).
    if verb == "king-history" {
        if args.iter().skip(1).any(|a| a == "--verdict") {
            return fno_agents::king_history::run_king_verdict(&args[1..]);
        }
        return fno_agents::king_history::run_king_history(&args[1..]);
    }

    // `king-checkin`: one verb runs the reign check-in body for
    // `fno agents king checkin`. Daemon-free beat like king-history:
    // Python resolves the caller's crown scope and the paths Python owns,
    // the native side gathers, prints, diffs and journals the row, reusing
    // the court-fold fold and the king-history scan in process.
    if verb == "king-checkin" {
        return fno_agents::king_checkin::run_king_checkin(&args[1..]);
    }
    // `evals-macro`: the macro-eval failure-pattern leaderboard for
    // `fno doctor evals macro`. Daemon-free read, `==` dispatch like
    // king-history: Python resolves the journal paths (identity and paths are
    // Python-owned), the native side owns the fold so the file-budget
    // Python-tree ratchet holds.
    if verb == "evals-macro" {
        return fno_agents::evals_macro::run_evals_macro(&args[1..]);
    }
    // `reign-ledger`: the reign ledger page for `fno agents king ledger`.
    // Same split as king-history: Python resolves the court and the paths,
    // the native side owns the page assembly, and the fold's scope_nodes ride
    // in the court JSON, so the page cannot disagree with the court.
    if verb == "reign-ledger" {
        return fno_agents::king_ledger::run_reign_ledger(&args[1..]);
    }
    if verb == "bash-census" {
        return fno_agents::bash_census::run_bash_census(&args[1..]);
    }
    // `intel`: the session-provenance fold, daemon-free read, == dispatch
    // like board/reclaim: never registered in ALL_CLIENT_ACTIONS (the action
    // list is shrink-only, d-fe66560a) and never routed by `fno agents`;
    // `fno doctor intel` shells HERE through resolve_binary.
    if verb == "intel" {
        return fno_agents::intel::run_intel(&args[1..]);
    }
    // `reclaim`: the machine janitor, daemon-free. Not a routable
    // `fno agents` verb; the Python surface is `fno doctor reclaim`, a thin
    // wrapper that shells HERE, and the daemon's daily sweep calls the gate
    // in-process.
    if verb == "reclaim" {
        return fno_agents::reclaim::run_reclaim(&args[1..], &AgentsHome::from_env());
    }
    // `plugin-install`: the filtered-stage installer for the plugin
    // harnesses, daemon-free. The Python surface `fno config plugin install`
    // shells HERE for claude, opencode and agy; codex stays on the Python
    // converge engine per the ship-phase ruling.
    if verb == "plugin-install" {
        return fno_agents::plugin_install::run_plugin_install(&args[1..]);
    }
    if verb == "session-start-bytes" {
        return fno_agents::session_start_bytes::run_session_start_bytes(&args[1..]);
    }

    // `territory-rows`: the territory fact set's daemon-free reads. Direct
    // dispatch like graph-get: the Python `fno config active-backlog-*`
    // passthroughs invoke the binary directly.
    if verb == "territory-rows" {
        return fno_agents::territory::run_territory_rows(&args[1..]);
    }
    if verb == "active-backlog-receipt" {
        return fno_agents::territory::run_active_backlog_receipt(&args[1..]);
    }
    if verb == "select-read" {
        return fno_agents::select_read::run(&args[1..]);
    }
    if verb == "territory-verdict" {
        return fno_agents::spawn_gate::run_territory_verdict(&args[1..]);
    }

    // `board`: the king board collector, read-only, daemon-free. Not a
    // routable `fno agents` verb (same `matches!` treatment as graph-get): the
    // Python surface is `fno inbox board` / `fno king board`, whose typer
    // command shells HERE and renders, and the stop hook reads the collector
    // in-process. Matched with `==` beside graph-get so the routable-verb
    // parity guard does not see it - no advertised fno verb is added.
    if verb == "board" {
        return fno_agents::king_board::run_board(&args[1..]);
    }

    // `notify-watch`: the operator-notice sampler, read-only,
    // daemon-free. Same `==` treatment as `board`: a read-only collector, not
    // a routable `fno agents` verb, so no advertised fno verb is added.
    if verb == "notify-watch" {
        return fno_agents::operator_notice::run_notify_watch_verb(&args[1..]);
    }

    // `verify-evidence`: Rust port of scripts/lib/verify-event-evidence.sh
    // (see verify_evidence.rs doc). Direct dispatch.
    if verb == "verify-evidence" {
        return fno_agents::verify_evidence::run_verify_evidence(&args[1..]);
    }

    // Retired at G4: the grid, the WebSocket drive surface, and the
    // interactive daemon PTY hosting behind `host`/`promote` were deleted when
    // the mux became the agent-PTY substrate. Each prints a one-line pointer to
    // the mux and exits non-zero, never a silent no-op (AC5-EDGE).
    if let Some(pointer) = retired_verb_pointer(verb) {
        eprintln!("{pointer}");
        return 2;
    }

    // Python-only verbs ported to the Rust client: these read state/registry/
    // event files directly (or print a stub) without a daemon RPC, so they
    // dispatch here before build_request. Byte-for-byte parity with the Python
    // implementations is the contract; see `fno_agents::client_verbs`.
    if verb == "drive-authority" {
        return fno_agents::client_verbs::run_drive_authority(&args[1..], &AgentsHome::from_env());
    }
    if verb == "trace" {
        return fno_agents::client_verbs::run_trace(&args[1..], &AgentsHome::from_env());
    }
    // registry-json: the daemon-free registry projection the hooks read.
    // Reads the registry file client-side and derives the served liveness
    // pair with the vendored freshness rule; starts nothing, so the Stop
    // hook's never-lazy-start promise still holds.
    if verb == "registry-json" {
        return fno_agents::registry_json::run_registry_json(&args[1..], &AgentsHome::from_env());
    }
    if verb == "ping" {
        return fno_agents::client_verbs::run_ping(&args[1..]);
    }
    // `resume --substrate thread` is the pane-to-thread LIFECYCLE move, not a
    // re-entry: it falls through to build_request, which routes it to the
    // daemon's agent.convert. The daemon owns it because the agent lock
    // serializes it and a mid-move claim must be pinned to a process that
    // outlives the client.
    if verb == "resume" && !fno_agents::resume_args::requests_conversion(&args[1..]) {
        // resume_wake's wake arms build their own runtimes and block_on them;
        // on this thread that panics inside the ambient runtime. A fresh
        // thread is legal in both contexts (gc_sweep::stop_row_process is
        // the same shape).
        let rest = args[1..].to_vec();
        let home = AgentsHome::from_env();
        return match std::thread::spawn(move || fno_agents::client_verbs::run_resume(&rest, &home))
            .join()
        {
            Ok(code) => code,
            Err(payload) => std::panic::resume_unwind(payload),
        };
    }
    if verb == "adopt" {
        return fno_agents::client_verbs::run_adopt(&args[1..], &AgentsHome::from_env());
    }
    if verb == "attach" {
        return fno_agents::attach::run_attach(&args[1..], &AgentsHome::from_env());
    }
    // `recover`: manual restoration of a recorded session under its
    // account/route; reads the registry and resolver directly, no daemon RPC.
    if verb == "recover" {
        return fno_agents::client_verbs::run_recover(&args[1..], &AgentsHome::from_env());
    }
    if verb == "logs" {
        return fno_agents::client_verbs::run_logs(&args[1..], &AgentsHome::from_env()).await;
    }
    // Inside-leg state push (E3.2): a per-turn hook reports {working|blocked|done}.
    // `report`: sends to an ALREADY-RUNNING daemon; must never lazy-start one.
    if verb == "report" {
        return fno_agents::client_verbs::run_report(&args[1..], &AgentsHome::from_env()).await;
    }
    // `wait`: poll registry.json directly for a state (no daemon RPC).
    if verb == "wait" {
        return fno_agents::wait::run_wait(&args[1..], &AgentsHome::from_env()).await;
    }

    // `pr-heal`: classify a red check, apply the mechanical fix; daemon-free.
    if matches!(verb, "pr-heal") {
        return fno_agents::heal::run_heal(&args[1..]);
    }
    if matches!(verb, "pr-push") {
        return fno_agents::pr_push::run_push(&args[1..]);
    }
    // `pr-body-check`: the repo's body guards, run before `gh pr create`.
    if matches!(verb, "pr-body-check") {
        return fno_agents::pr_body_check::run(&args[1..]);
    }
    if matches!(verb, "pr-rebase") {
        return fno_agents::pr_rebase::run_rebase(&args[1..]);
    }
    // `subscribe`: follow the daemon's own `events.jsonl` and stream registry
    // state transitions + pane exits as NDJSON. File-follow, no daemon RPC, so it
    // dispatches here before build_request.
    if verb == "subscribe" {
        return fno_agents::subscribe::run_subscribe(&args[1..], &AgentsHome::from_env()).await;
    }

    // `digest`: read-only "while you were gone" fold over events.jsonl +
    // ledger.json for a session. Never touches the daemon; exits 0 on empty.
    if verb == "digest" {
        return fno_agents::digest::run_digest(&args[1..], &AgentsHome::from_env()).await;
    }

    // `distress-scan`: pre-manifest <help> tag read (see distress.rs doc).
    // Direct dispatch, no daemon RPC; always exits 0.
    if verb == "distress-scan" {
        return fno_agents::distress::run_distress_scan(&args[1..]);
    }

    // `needs`: read-only needs-me-queue fold over events.jsonl +
    // ledger.json across ALL sessions, emitting review_wedged / budget_stop
    // items. Never touches the daemon; exits 0 on empty. The mux client shells
    // this off-loop when the prefix+a overlay opens.
    if verb == "needs" {
        return fno_agents::needs::run_needs(&args[1..], &AgentsHome::from_env()).await;
    }

    // `feed`: one projection joining questions.jsonl + graph.json
    // into an ordered feed whose rows carry the node id + session id the mux
    // deep link resolves. Read-only, daemon-free like `needs`: it dispatches
    // here before build_request. events.jsonl is deliberately NOT a source
    // (72% ticks; lifecycle derives from the graph, never copied).
    if verb == "feed" {
        return fno_agents::feed::run_feed(&args[1..], &AgentsHome::from_env()).await;
    }

    // `day`: daemon-free like `feed`; `--commit` appends the boundary row.
    // `matches!` treatment as `claim`: the verb is unregistered (the action
    // list is shrink-only, d-fe66560a); the inbox relay reaches it through
    // resolve_binary, and no advertised `fno agents day` spelling exists.
    if matches!(verb, "day") {
        return fno_agents::day::run_day(&args[1..], &AgentsHome::from_env());
    }

    // `status` reports on a *running* daemon: it must NOT lazy-start one just to
    // describe it as up. A down daemon is exit 13 (AC10-ERR).
    if verb == "status" {
        // `--json`/`-J` selects the machine payload; the default renders the
        // human arms table + daemon lines. Anything else is rejected rather
        // than silently ignored (Codex P3).
        //
        // d-fe66560a makes the action list shrink-only, so the read
        // leaves over the capability table and the merge-posture family ride
        // `status` as arguments instead of actions of their own. The Python
        // router rewrites `fno agents capabilities <h> ...` into
        // `fno-agents status --capabilities <h> ...`. With neither flag the
        // behavior is exactly the plain status read.
        let rest = &args[1..];
        let has_cap = rest.iter().any(|a| a.as_str() == "--capabilities");
        let has_family = rest.iter().any(|a| a.as_str() == "--target-family");
        if has_cap && has_family {
            eprintln!(
                "fno-agents: status takes one of --capabilities or --target-family, not both"
            );
            return 2;
        }
        if has_cap {
            let sub: Vec<String> = rest
                .iter()
                .filter(|a| a.as_str() != "--capabilities")
                .cloned()
                .collect();
            return fno_agents::capability_leaves::run_capabilities(&sub);
        }
        if has_family {
            let sub: Vec<String> = rest
                .iter()
                .filter(|a| a.as_str() != "--target-family")
                .cloned()
                .collect();
            return fno_agents::capability_leaves::run_target_family(&sub);
        }
        let json_out = rest.iter().any(|a| a == "--json" || a == "-J");
        let extras: Vec<&str> = rest
            .iter()
            .map(String::as_str)
            .filter(|a| *a != "--json" && *a != "-J")
            .collect();
        if !extras.is_empty() {
            eprintln!(
                "fno-agents: status takes no arguments other than --json/-J (got: {})",
                extras.join(" ")
            );
            return 2;
        }
        return run_status(json_out).await;
    }

    // `restart` swaps a stale daemon for one built from the current binary
    // SIGTERM the running daemon (graceful drain; PTY workers
    // survive), wait for the socket to clear, lazy-start fresh. Like `status`,
    // it does not fit the one-shot build_request path and dispatches here.
    // `--force` is the break-glass variant: SIGKILL the lockfile's
    // holder BEFORE any probe, because a wedged holder is exactly what the
    // probe cannot see.
    // `census`: one JSON row per long-lived process, the build-
    // staleness read that doctor/restart/update render. Composes the daemon
    // status (in-process), a ps walk of keepers, and `fno mux ls --json`.
    if verb == "census" {
        return fno_agents::census::run_verb(&args[1..]).await;
    }

    if verb == "restart" {
        let parsed = match RestartArgs::try_parse_from(&args[1..]) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{}", refusal_line("fno-agents restart", &e));
                return 2;
            }
        };
        if parsed.keepers_only {
            return fno_agents::restart_run::run_keepers_only(parsed.json.json).await;
        }
        return fno_agents::restart_run::run_restart(
            parsed.force,
            parsed.json.json,
            parsed.if_drifted,
            parsed.mux,
        )
        .await;
    }

    // `reap` is the manual dead-row GC: the SAME sweep the daemon runs
    // on its idle tick, on demand. It operates on the registry directly under the
    // shared flock, so it needs no running daemon and dispatches here before
    // build_request.
    if verb == "reap" {
        return run_reap(&args[1..]);
    }

    // The roster-side sweep (gap one): claude rows no fno row names.
    // Like `reap`, it operates directly on live surfaces so it needs no
    // running daemon. Dry-run by default; `--apply` executes.
    if verb == "roster-reap" {
        return run_roster_reap(&args[1..]);
    }

    // The cascade verdict per NAME: the squad store's Unknown
    // members carry no session id, so the fno side asks this verb for a
    // positive verdict per name instead of re-implementing the cascade.
    if verb == "node-route" {
        return run_node_route(&args[1..]);
    }

    // Capture the verb name so format_success can use it at the print site
    // without threading it through the protocol layer.
    let verb_owned = verb.to_string();

    // Task 3.1: capture --json before build_request strips it, and detect TTY.
    // --json is a client-side rendering flag and must NOT be forwarded to the daemon.
    // Stop scanning at `--argv`: a `--json`/`-J` in the spawned process's argv
    // payload must not trip client-side JSON rendering (gemini review, PR #431).
    let json_flag = args[1..]
        .iter()
        .take_while(|a| a.as_str() != "--argv")
        .any(|a| a == "--json" || a == "-J");
    let is_tty = std::io::stdout().is_terminal();
    // the P1 discovered-live-sessions lane is on by default for
    // `list`; --no-discovered opts out of the ~/.claude/sessions scan.
    let discover_flag = !args[1..]
        .iter()
        .take_while(|a| a.as_str() != "--argv")
        .any(|a| a == "--no-discovered");

    let (method, mut params) = match build_request(verb, &args[1..]) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };

    // Resolve the agent name from the PARSED params, not args.get(1): build_request
    // strips leading flags (and their values) when collecting positionals, so
    // `fno agents stop --force worker-A` yields name="worker-A". Reading args.get(1)
    // would capture "--force" and print the wrong success line (gemini-code-assist
    // high on PR #361). Falls back to the raw first positional for verbs that don't
    // set params.name (none of the formatted verbs hit that path today).
    let agent_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();

    let home = AgentsHome::from_env();
    // (AC20): the codex-thread-target lookup moved INTO the agent.ask
    // block below, derived from the registry read already performed there. The
    // old spot loaded the registry for EVERY client verb and swallowed read
    // failures at `.ok()`.

    // Claude `ask` is handled entirely client-side: claude is a
    // `claude --bg` shellout, not a daemon-PTY agent, so it bypasses the daemon
    // RPC. Only claude targets take this path; codex/gemini ask still routes to
    // the daemon below. Resolution: an existing registry row's provider, else
    // the `--provider` flag on first contact.
    if method == "agent.ask" {
        // Task 1.3a: ask never creates. Pre-check the registry before provider
        // resolution: if no row exists for the name, surface the unknown-agent
        // error (exit 16) regardless of --provider. This mirrors Python's
        // dispatch_ask after Task 1.1 (unknown-name check precedes provider
        // selection). Provider-mismatch logic (inside maybe_run_claude_ask) still
        // applies for existing rows.
        //
        // (AC20): ONE registry read for the whole ask path. The
        // codex-thread-target lookup below derives from THIS read - the old
        // second load ran for EVERY client verb and swallowed failures at
        // `.ok()`.
        use fno_agents::claude_ask::{emit_event, py_repr};
        use fno_agents::state::load_registry;
        // A corrupt/unreadable registry must surface as exit 12 ("registry
        // read failed"), NOT degrade to an empty registry where every name
        // looks unknown (exit 16 + a forensically wrong unknown-name
        // event). Python parity: dispatch_ask raises exit 12 on
        // (OSError, ValueError, RegistryVersionError); the lib dispatch
        // fns do the same. A MISSING file is not an error (load_registry
        // returns the default). Sigma-review finding, this PR.
        let registry = match load_registry(&home.registry_json()) {
            Ok(r) => r,
            Err(e) => {
                emit_event(
                    &home.events_jsonl(),
                    "agent_ask_failed",
                    &[
                        ("stage", "registry-read".into()),
                        ("name", agent_name.clone().into()),
                        ("error", e.to_string().into()),
                    ],
                );
                eprintln!("registry read failed: {e}");
                return 12;
            }
        };
        // A codex thread target falls through to the daemon ask below instead
        // of the unresolvable-create error.
        let codex_thread_target =
            fno_agents::codex_ask::is_codex_thread_target(&registry, &agent_name);
        {
            if registry.find_name_or_full_session_id(&agent_name).is_none() {
                // Event parity: Python's dispatch_ask emits agent_ask_failed
                // stage="unknown-name" before raising; this pre-check is the
                // only emitter on the Rust CLI path (the lib None-arms are
                // unreachable from here once this fires).
                emit_event(
                    &home.events_jsonl(),
                    "agent_ask_failed",
                    &[
                        ("stage", "unknown-name".into()),
                        ("name", agent_name.clone().into()),
                    ],
                );
                eprintln!(
                    "unknown agent {}; spawn it first: fno agents spawn {} --harness <harness>",
                    py_repr(&agent_name),
                    agent_name
                );
                return 16;
            }
        }

        if let Some(code) = maybe_run_claude_ask(&home, &params, &agent_name) {
            return code;
        }
        // Codex `ask` is handled client-side: codex is a
        // one-shot `codex exec --json` subprocess, not a PTY agent, so it
        // bypasses the daemon RPC. Same Option<i32> contract as claude.
        if let Some(code) = maybe_run_codex_ask(&home, &params, &agent_name) {
            return code;
        }
        // Gemini `ask` is handled client-side: gemini is a
        // one-shot `gemini -p --output-format json` subprocess. Same contract.
        if let Some(code) = maybe_run_gemini_ask(&home, &params, &agent_name) {
            return code;
        }
        // Agy `ask` is intercepted client-side (Phase C): agy is plain-text with
        // no session id, so a stateful resume is unsupported — this surfaces a
        // clear error directing the caller to `spawn --harness agy --once`.
        if let Some(code) = maybe_run_agy_ask(&home, &params, &agent_name) {
            return code;
        }
        // Opencode `ask` is intercepted client-side: opencode is
        // pane-hosted only in v1, so a stateful resume is unsupported — this
        // surfaces a clear error directing the caller to drive the pane
        // directly, rather than the generic "provider required for new
        // agent" text an existing opencode row would otherwise hit below
        // (that text is both wrong - the agent already exists - and a dead
        // end, since retrying with --harness opencode reproduces it).
        if let Some(code) = maybe_run_opencode_ask(&home, &params, &agent_name) {
            return code;
        }
        // Unconditional flip: `ask` now auto-routes to this
        // client for every provider, so an ask that matched none of the four
        // provider hooks is a create with no/unknown `--provider`. Surface
        // Python's `select_provider` exit-2 error here rather than falling
        // through to the daemon RPC, whose `handle_ask` PTY screen is the wrong
        // shape for `ask` (Locked Decision 3). The daemon path below is now
        // unreachable for `agent.ask`.
        if !codex_thread_target {
            return unresolvable_ask_exit(&params, &agent_name);
        }
    }

    // Task 1.3a: intercept `spawn` (NOT host/promote, which also map to
    // agent.spawn) to route claude -> dispatch_claude_spawn, and
    // codex/gemini + --once -> dispatch_codex_once / dispatch_gemini_once.
    // `host` and `promote` must fall through to the daemon RPC unchanged.
    if method == "agent.spawn" && verb_owned == "spawn" {
        // The seam gate comes FIRST. A spawn that skipped
        // the Python seam carries no configured route/model/effort/account,
        // so it goes back to the front door (FNO_AGENTS_RUNTIME=python stops
        // the loop: the re-exec crosses the seam, gets the marker, and comes
        // back marked). On exec failure this spawn's only decision path is
        // gone and its policy state is unknown, so it refuses rather than
        // falling through to a harness default the seam could have refused.
        if spawn_needs_python_seam(&params) {
            let err = exec_python_front(&args);
            eprintln!(
                "fno-agents: config.agents.profiles is read only by the Python \
                 spawn seam, and exec of 'fno agents spawn' failed: {err}. No \
                 configured route, model, effort or account was applied; policy \
                 state unknown; refusing."
            );
            return 2;
        }
        // 4a-G2: the `pane` substrate (the default) is mux-hosted now, and the
        // Python back half owns it (fno.agents.mux_spawn: front-half reuse +
        // `fno mux pane run` + the registry mux ref). The Python front door
        // already carves pane spawns out of the binary route (rust_runtime),
        // so this arm is only reached by a DIRECT `fno-agents spawn` call -
        // re-exec the Python CLI rather than falling through to the daemon
        // PTY host (retiring at G4; a silent daemon fallback is exactly what
        // AC1-ERR forbids). FNO_AGENTS_RUNTIME=python stops the front door
        // routing straight back here.
        let mut substrate = params
            .get("substrate")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| default_substrate(&params))
            .to_string();
        // `thread` is the public substrate name. The lower-level dispatch arms
        // retain their historical `bg` selector until their wire contract moves.
        if substrate == "thread" {
            substrate = "bg".to_string();
        }
        let substrate = substrate.as_str();
        if let Err(message) = validate_spawn_placement(&params, substrate) {
            eprintln!("{message}");
            return 2;
        }
        // The default view (mirrors the Python seam): a bare spawn that took
        // the built-in thread default from INSIDE a mux opens portal 0 on its
        // worker. An explicit --portal wins; outside a mux nothing auto-opens.
        if substrate == "bg"
            && params.get("substrate").is_none()
            && params.get("portal").is_none()
            && std::env::var("FNO_PANE")
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        {
            if let Some(obj) = params.as_object_mut() {
                obj.insert("portal".into(), Value::from(0u8));
            }
        }
        if substrate == "pane" {
            use fno_agents::claude_ask::py_repr;
            // Provider parity with the optional-provider Python resolver: a
            // MISSING --provider is legal on the pane substrate (the Python
            // re-exec resolves it from the invoking harness), so let None fall
            // through. An UNKNOWN provider is still a client-side exit 2 even
            // where the `fno` front door is absent (CI), matching the resolver's
            // downstream substrate-aware rejection.
            match params.get("provider").and_then(|v| v.as_str()) {
                None => {}
                Some(p) if !KNOWN_PROVIDERS.contains(&p) => {
                    eprintln!(
                        "unknown provider {}; supported: {}",
                        py_repr(p),
                        known_providers_csv()
                    );
                    return 2;
                }
                Some(_) => {}
            }
            // A marked call that still lands here must not forward the marker
            // into the Python CLI (an unknown flag there): strip it from the
            // re-exec argv. The seam gate above already sent unmarked spawns
            // back; this keeps even a hand-built marked pane argv clean.
            let pane_args: Vec<String> = args
                .iter()
                .filter(|a| !a.starts_with("--defaults-applied"))
                .cloned()
                .collect();
            let err = exec_python_front(&pane_args);
            eprintln!(
                "fno-agents: substrate 'pane' is mux-hosted via the Python CLI, \
                 but exec of 'fno agents spawn' failed: {err}. Install the fno \
                 front door or run `fno agents spawn ...` directly."
            );
            return 127;
        }
        // an --account spawn on ANY substrate resolves its four-lane env
        // overlay in Python (fno.agents.account_env); re-exec the Python CLI here
        // rather than the native Rust bg spawn below, so the resolver + refusals
        // live in exactly one place (pane already re-exec'd above). Without this
        // the flag silently vanishes on the Rust-intercepted bg/headless path -
        // the known "two path gates for a new provider field" drift class.
        // FNO_AGENTS_RUNTIME=python stops the Python front door bouncing back.
        if params.get("account").and_then(|v| v.as_str()).is_some() {
            let account_args: Vec<String> = args
                .iter()
                .filter(|a| !a.starts_with("--defaults-applied"))
                .cloned()
                .collect();
            let err = exec_python_front(&account_args);
            eprintln!(
                "fno-agents: --account resolution runs in the Python CLI, but \
                 exec of 'fno agents spawn' failed: {err}. Run `fno agents \
                 spawn ...` directly."
            );
            return 127;
        }
        if let Some(code) = maybe_run_spawn(&home, &params, &agent_name) {
            if code == 0 {
                // A placement failure never recolors the spawn verdict: the
                // receipt is the truth, the worker is live, and a nonzero here
                // reads as spawn failure to a retrying caller - a duplicate.
                if let Err(detail) = place_thread_portal_after_spawn(&params, &agent_name) {
                    eprintln!("{detail}");
                }
            }
            return code;
        }
        // No client-side handler matched: fall through to the daemon RPC below.
    }

    let daemon_bin = resolve_daemon_bin();
    // Forward the caller's cwd so a spawned worker launches in the user's
    // project, not the daemon's frozen home dir (fix/agents-host-cwd). Only
    // daemon-bound requests remain here; claude/codex `ask` already returned
    // above. On the rare current_dir() failure we leave params as-is and warn:
    // the daemon then uses its hardened temp-dir fallback (an obviously-wrong
    // /tmp launch) rather than silently adopting its own start dir.
    match std::env::current_dir() {
        Ok(caller) => {
            // the default (no explicit --cwd, no --here) stamps the
            // canonical repo root instead of the caller cwd for daemon-bound
            // codex/gemini spawn -- the same inversion as the client-side path.
            // An explicit --cwd wins, so when params already carries one we
            // resolve nothing and emit no redirect note (it would falsely claim a
            // redirect that ensure_request_cwd's keep-explicit guard never
            // performs -- review MEDIUM 4); --here keeps the caller cwd. --fresh
            // is an accepted no-op alias. ensure_request_cwd then leaves the
            // explicit --cwd intact.
            let (_fresh, here) = fresh_here_flags(&params);
            let explicit_cwd = params.get("cwd").is_some();
            // Only spawn consumes the launch dir: an `agent.ask` follows its
            // registered session and takes cwd as `_cwd`, so it never takes the
            // canonical default nor the redirect note (a false diagnostic for a
            // non-consuming op -- review). spawn keeps the inverted default.
            let stamp = if !explicit_cwd && !here && method == "agent.spawn" {
                match fno_agents::paths::canonical_repo_root(&caller) {
                    Some(canon) => {
                        note_fresh_redirect(&caller, &canon);
                        canon
                    }
                    None => caller,
                }
            } else {
                caller
            };
            ensure_request_cwd(&method, &mut params, &stamp);
        }
        Err(e) => eprintln!(
            "fno-agents: could not resolve current dir ({e}); daemon will pick a fallback cwd"
        ),
    }
    // (AC19): for a codex THREAD spawn the daemon RPC is what creates
    // the registry row, so the spawn gate is held across exactly this exchange
    // (acquire before the write, release after the response read) - one gate
    // evaluation per spawn, and the first spawn's row is counted before the
    // second is evaluated. Other verbs skip this entirely. Read before
    // `method`/`params` move into the request.
    //
    // Daemon-bound is DERIVED from the same capability contract the daemon
    // routes on (attach lane + a harness-owned server). Both binaries
    // embed the same packaged table, so this predicate and the daemon's route
    // cannot disagree the way a name test here and a derived route there
    // could: the next attach-with-server harness arrives with its state_dirs
    // attached and its gate run without this line learning its name.
    //
    // The provider default MUST match the daemon's, which is `codex` when the
    // param is absent (`handle_spawn`). A predicate requiring an explicit
    // "codex" here reads false for a spawn with no `-H`, while the daemon still
    // routes it to the codex thread lane - so the grant was never attached and
    // the gate below never ran, on the exact lane this node exists to fix.
    // Green gate, mute worker. Keep the two defaults identical. An unreadable
    // contract answers false here: the daemon refuses the same spawn, so no
    // grant or gate is skipped for a spawn the daemon would have served.
    let spawn_provider = params
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("codex");
    let daemon_bound_thread_spawn = method == "agent.spawn"
        && params.get("substrate").and_then(|v| v.as_str()) == Some("thread")
        && fno_agents::harness_capabilities::HarnessContract::packaged()
            .and_then(|contract| {
                Ok(contract.thread_lane(spawn_provider)? == "attach"
                    && contract.attach_needs_server(spawn_provider)?)
            })
            .unwrap_or(false);
    // Hop 1 of the state-root grant. The client inherits
    // FNO_WORKER_ADD_DIRS from the Python seam across `os.execv`, so it reads
    // the ALREADY-RESOLVED set with the same reader every other lane uses -
    // one resolver, one published value, now three readers.
    //
    // It has to travel as a param rather than as environment because the
    // daemon on the other end is long-lived and SHARED: it does not inherit
    // this spawn's environment, so a `state_dirs_from_env()` call over there
    // would read the daemon's own env instead of ours.
    if daemon_bound_thread_spawn {
        attach_codex_thread_state_dirs(&mut params);
    }
    // Snapshot before `params` moves into the request: the relocated gate
    // honors the same spawn-control flags the shared construction reads.
    let daemon_gate_flags = gate_flags_from_params(&params);
    // Same snapshot for the portal placement: it rides the
    // daemon's response, after the receipt.
    let thread_portal_params = if method == "agent.spawn"
        && params.get("substrate").and_then(|v| v.as_str()) == Some("thread")
    {
        Some(params.clone())
    } else {
        None
    };
    let req = Request::new(1, method, params);

    let daemon_spawn_gate = if daemon_bound_thread_spawn {
        let config_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match fno_agents::spawn_gate::run_gate(
            &config_cwd,
            &home.registry_json(),
            fno_agents::spawn_gate::GateInput {
                name: agent_name.clone(),
                substrate: "bg".into(),
                flags: daemon_gate_flags,
                ..Default::default()
            },
        ) {
            Ok(guard) => Some(guard),
            Err(refusal) => {
                if let Some(receipt) = &refusal.receipt {
                    println!("{receipt}");
                }
                return refusal.exit_code;
            }
        }
    } else {
        None
    };

    let call_result = if verb_owned == "rm" {
        // change 3: a stale daemon means the removal would be
        // executed by the OLD binary - the exact shape that left four
        // sessions stamped origin=adopted while their harness sessions
        // stayed alive. The notice moves onto the refusal path for rm:
        // non-zero, no `removed:` line, and the remedy named. `list` keeps
        // its advisory drift notice (a stale read is still a read).
        if let Some(w) = drift_warning(&check_daemon_drift(&home).await, None) {
            eprintln!("fno-agents: refusing rm: {w}");
            eprintln!("  the removal was not attempted; run `fno doctor update` (or restart the daemon) and retry");
            return 21;
        }
        call(&home, &daemon_bin, &req).await
    } else {
        call(&home, &daemon_bin, &req).await
    };
    drop(daemon_spawn_gate);
    match call_result {
        Ok(resp) => match resp.payload {
            ResponsePayload::Err(err) => {
                eprintln!("fno-agents: {}", err.message);
                exit_code_for(err.code)
            }
            ResponsePayload::Ok(result) => {
                // The loaded-thread answer rides `list --harness codex` as a
                // field instead of a hidden verb root. Gated on the codex
                // filter so no other list pays the app-server round trip; an
                // unreachable daemon becomes `available: false`, never a list
                // failure. The await must happen here - format_success is sync.
                let result = if verb_owned == "list"
                    && result["filters_applied"]["provider"].as_str() == Some("codex")
                {
                    let mut with_loaded = result;
                    with_loaded["codex_loaded"] = fno_agents::codex_inject::loaded_threads_block(
                        fno_agents::codex_inject::discover_loaded_threads().await,
                    );
                    with_loaded
                } else {
                    result
                };
                if let Some(line) = format_success(
                    &verb_owned,
                    &agent_name,
                    &result,
                    json_flag,
                    is_tty,
                    discover_flag,
                ) {
                    // ask FOLLOW-UP prints the reply verbatim with no added
                    // newline, matching Python `sys.stdout.write(result.reply or "")`
                    // (Codex P2 on PR #361 — relevant under FNO_AGENTS_RUNTIME=rust,
                    // the only path that routes ask to this client). Every other
                    // formatted output (ask create short_id, stop/rm/list/reconcile)
                    // keeps the trailing newline.
                    let ask_followup = verb_owned == "ask"
                        && !result
                            .get("created")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    if ask_followup {
                        print!("{line}");
                    } else {
                        println!("{line}");
                    }
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result).unwrap_or_default()
                    );
                }
                // Drift warning on read/removal verbs, stderr-only so a
                // `--json` stdout consumer stays clean. These verbs already
                // ensured a daemon is up via `call`; a freshly lazy-started one
                // reads Fresh, so no false warning. A separate status probe keeps
                // this off every other verb's hot path.
                if warns_on_daemon_drift(&verb_owned) {
                    let state = check_daemon_drift(&home).await;
                    if let Some(w) = drift_warning(&state, None) {
                        eprintln!("{w}");
                    }
                }
                // A refused stop is not a success. The daemon answers
                // `stopped: false` over a turn whose interrupt never settled,
                // and the mux viewport's `run_agent_action` reads ONLY this
                // exit code. A 0 there prints "stopped <name>" over a worker
                // that is still running, which is the report the daemon arm
                // refuses to make. `Busy` is the shape: the turn holds the
                // thread, and a retry can still reach it.
                if verb_owned == "stop"
                    && result.get("stopped").and_then(Value::as_bool) == Some(false)
                {
                    return 18;
                }
                // change 3: a receipt over a surviving harness row is
                // the "reports success while removing nothing" shape that
                // stamped four sessions origin=adopted. The renderer already
                // prints the survival in its notes; the exit code now says it
                // too. An already-absent harness row is a COMPLETED removal,
                // not a survivor - the removal asked for is total, so it
                // keeps exit 0 and the sideline never renders a false
                // failure for it.
                let harness_survives = result.get("harness_removed").and_then(Value::as_bool)
                    == Some(false)
                    && !result
                        .get("harness_reason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .contains("already absent");
                if verb_owned == "rm" && harness_survives {
                    return 21;
                }
                // The daemon-bound thread lane (codex and every other
                // attach-with-server harness): the RPC created the row, so the
                // portal places here, after the receipt, exactly as the
                // client-side lanes do.
                if let Some(place_params) = &thread_portal_params {
                    if let Err(detail) = place_thread_portal_after_spawn(place_params, &agent_name)
                    {
                        eprintln!("{detail}");
                    }
                }
                0
            }
        },
        Err(e) => {
            if verb_owned == "rm" && !agent_name.is_empty() {
                // A dead connection does not prove the daemon skipped the
                // removal; read the store rather than assert the outcome. An
                // unread store reports unknown instead of absent.
                let still_registered = match fno_agents::state::load_registry(&home.registry_json())
                {
                    Ok(reg) => Some(reg.find_name_or_full_session_id(&agent_name).is_some()),
                    Err(_) => None,
                };
                eprintln!(
                    "{}",
                    rm_failure_line(&agent_name, &e.to_string(), still_registered)
                );
            } else {
                eprintln!("fno-agents: {e}");
            }
            1
        }
    }
}

/// Route a claude `ask` to the client-side `claude --bg` path, bypassing the
/// daemon. Returns `Some(exit_code)` when the target is claude
/// (resolved from an existing registry row, else the `--provider` flag), or
/// `None` to fall through to the daemon RPC for codex/gemini.
fn maybe_run_claude_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    use fno_agents::claude_ask::{dispatch_claude_ask, ClaudeHome};
    use fno_agents::state::load_registry;

    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let registry = load_registry(&home.registry_json()).unwrap_or_default();
    let existing_provider = registry
        .find_name_or_full_session_id(name)
        .map(|e| e.harness_name().to_string());

    // Provider mismatch: an existing claude agent plus a conflicting --provider
    // flag. Python's select_provider rejects this as a mismatch; without the
    // check the registry value silently wins and the message is delivered to
    // the wrong provider/session on a stale or mistyped flag (Codex P2).
    if let (Some(ep), Some(pp)) = (existing_provider.as_deref(), provider_param) {
        if ep == "claude" && pp != "claude" {
            eprintln!(
                "fno-agents: agent {name:?} already exists with provider 'claude'; refusing to override with --provider {pp}"
            );
            return Some(2);
        }
    }

    let resolved = existing_provider.as_deref().or(provider_param);
    if resolved != Some("claude") {
        return None; // not a claude target; the daemon path handles it
    }

    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let from_name = params
        .get("from_name")
        .and_then(|v| v.as_str())
        .unwrap_or("fno");
    // ask is a follow-up to an already-registered session: dispatch_claude_ask
    // takes the cwd as `_cwd` and never launches in it. So resolve only an
    // explicit --cwd (canonicalized, Python's `Path(cwd).resolve()`; empty is
    // absent) or the caller cwd -- NEVER the canonical default or the redirect
    // note, which would be a false diagnostic for an operation that does not
    // consume the launch dir (review).
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(canonicalize_cwd)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
    let timeout = params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_secs);
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let claude_home = ClaudeHome::from_env();
    let outcome = dispatch_claude_ask(
        home,
        &claude_home,
        name,
        message,
        from_name,
        &cwd,
        yolo,
        timeout,
        &[],
    );
    // stderr/stdout carry exact bytes (newlines baked in); write verbatim.
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    Some(outcome.exit_code)
}

/// Invoking-harness env markers, highest priority first. Mirror of Python
/// `harness_identity.HARNESS_SESSION_MARKERS`; cross-language drift is caught by
/// the pytest `test_harness_markers_match_client_rs`, which reads this const from
/// source (the Rust unit test only guards Rust-internal edits).
#[allow(dead_code)]
const HARNESS_MARKERS: &[(&str, &str)] = &[
    ("CODEX_THREAD_ID", "codex"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_SESSION_ID", "codex"),
    ("GEMINI_SESSION_ID", "gemini"),
    ("OPENCODE_SESSION_ID", "opencode"),
];

/// Infer the dispatch provider when `--provider` is absent, mirroring Python
/// `infer_invoking_harness`: resolve when the present markers name exactly one
/// *distinct* harness. Multiple markers for the same harness (Codex's thread id
/// plus its legacy session id) agree; markers naming different harnesses, or
/// none, fall through to the builtin `claude`. Never guesses. `lookup` is
/// injectable so tests don't touch process-global env.
fn infer_dispatch_provider(lookup: impl Fn(&str) -> Option<String>) -> &'static str {
    match fno_agents::claims::resolve_harness_from(lookup).as_deref() {
        Some("claude") => "claude",
        Some("codex") => "codex",
        Some("gemini") => "gemini",
        Some("opencode") => "opencode",
        Some("agy") => "agy",
        _ => "claude",
    }
}

/// Route a codex `ask` to the client-side `codex exec` path, bypassing the
/// daemon. Returns `Some(exit_code)` when the target is codex
/// (resolved from an existing registry row, else the `--provider` flag), or
/// `None` to fall through to the next provider hook.
fn maybe_run_codex_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::codex_ask::maybe_run_codex_ask(home, params, name)
}

/// Route a gemini `ask` to the client-side `gemini -p` path, bypassing the
/// daemon. Returns `Some(exit_code)` when the target is gemini,
/// or `None` to fall through to the unresolvable-`ask` surface.
fn maybe_run_gemini_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::gemini_ask::maybe_run_gemini_ask(home, params, name)
}

/// Route an agy `ask` to the client-side stateless guard (Phase C). agy is
/// plain-text with no session id, so a stateful resume is unsupported; this
/// returns `Some(2)` with a redirect error for an agy target, else `None`.
fn maybe_run_agy_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::agy_ask::maybe_run_agy_ask(home, params, name)
}

/// Route an opencode `ask` to the client-side pane-only guard.
/// opencode is hosted as a pane with no client-side stateful resume; this
/// returns `Some(2)` with a redirect error for an opencode target, else `None`.
fn maybe_run_opencode_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    fno_agents::opencode_ask::maybe_run_opencode_ask(home, params, name)
}

fn validate_spawn_placement(params: &Value, substrate: &str) -> Result<(), String> {
    let squad = params.get("squad").and_then(Value::as_str);
    let split = params.get("split").and_then(Value::as_str);
    let at = params.get("at").and_then(Value::as_str);
    let tab = params.get("tab").and_then(Value::as_str);
    let portal = params.get("portal").and_then(Value::as_u64);

    if squad.is_some_and(|name| name.trim().is_empty()) {
        return Err("--workspace/-s needs a nonblank workspace name".into());
    }
    // A portal is the pane a thread hosts: the placement flags are
    // legal for a thread WHEN --portal names it, refused for a bare thread
    // where they mean nothing. Mirrors Python's placement_refusal, the one
    // contract both runtimes read; the portal's 0-255 range is enforced at
    // the parse arm, so a bad index never reaches this check.
    if portal.is_some() && substrate != "bg" {
        return Err(
            "--portal applies only to --substrate thread; a pane hosts its \
             own geometry and headless hosts no session at all"
                .into(),
        );
    }
    if at.is_some() && substrate == "bg" {
        return Err("--at applies only to --substrate pane (a thread has no calling pane)".into());
    }
    let placement_requested = squad.is_some() || split.is_some() || at.is_some() || tab.is_some();
    if placement_requested && substrate == "bg" && portal.is_none() {
        return Err("--workspace/-s, --split/-x, and --tab on --substrate \
             thread need --portal N: a thread hosts no pane until a portal \
             opens one, so the placement has nothing to place"
            .into());
    }
    if placement_requested && portal.is_none() && substrate != "pane" {
        return Err(
            "--workspace/-s, --split/-x, --at, and --tab apply only to --substrate pane \
             (bg/headless have no pane geometry)"
                .into(),
        );
    }
    if split.is_some_and(|direction| !matches!(direction, "left" | "right" | "up" | "down")) {
        return Err(format!(
            "--split/-x must be left, right, up, or down (got {:?})",
            split.unwrap_or_default()
        ));
    }
    if tab.is_some_and(|selector| selector.trim().is_empty()) {
        return Err("--tab needs a nonblank selector or pane-group name".into());
    }
    Ok(())
}

fn effective_spawn_message(message: &str, substrate: &str) -> String {
    if substrate == "pane" {
        message.to_owned()
    } else {
        fno_agents::spawn_payload::enrich_spawn_payload(message)
    }
}

fn validate_effort_for_spawn(
    provider: &str,
    substrate: &str,
    effort: Option<&str>,
) -> Result<(), String> {
    if substrate == "pane" {
        return Ok(());
    }
    let Some(value) = effort else {
        return Ok(());
    };
    if value.is_empty() {
        return Err("--effort must be non-empty".to_string());
    }
    if matches!(provider, "gemini") {
        return Err(format!(
            "harness {} has no reasoning-effort surface; omit --effort",
            provider
        ));
    }
    Ok(())
}

/// Route a `spawn` (NOT host/promote) to the appropriate client-side path.
///
/// names the session substrate as one axis with three values; this arm
/// routes the two non-default ones client-side and falls through for `pane`.
/// - `pane` (default): owned interactive daemon pane -> None (fall through).
/// - claude + `bg`: dispatch_claude_spawn (the detached `claude --bg` thread).
/// - claude + `headless`: dispatch_claude_headless (the `claude -p` one-shot).
/// - codex/gemini/agy/opencode + `headless`: dispatch_*_once (one-shot, client-side).
/// - opencode + `bg`: dispatch_opencode_serve (persistent session on a shared
///   `opencode serve`,; detached `run --attach` writer streams events).
/// - codex + `bg`: daemon-hosted app-server thread; gemini/agy + `bg`: hard error.
/// - no resolvable / unknown provider: stderr usage error + exit 2.
///
/// Returns `Some(exit_code)` when handled client-side, `None` to fall through.
/// One-call portal placement on the Rust lanes, the twin of the
/// Python `thread_portal.place_thread_portal`: a thread spawn with
/// `--portal` ends with the portal open, through the same `fno mux thread`
/// reach a manual second command would type. The worker is already live, so
/// a placement failure is reported AFTER the spawn receipt and never
/// un-spawns anyone; the exit code stays the spawn's, because a nonzero
/// here reads as spawn failure to a retrying caller - a duplicate worker.
fn place_thread_portal_after_spawn(params: &Value, name: &str) -> Result<(), String> {
    let Some(portal) = params.get("portal").and_then(Value::as_u64) else {
        return Ok(());
    };
    let mut args = vec![
        "mux".to_string(),
        "thread".to_string(),
        name.to_string(),
        "--portal".to_string(),
        portal.to_string(),
    ];
    for (flag, key) in [
        ("--workspace", "squad"),
        ("--split", "split"),
        ("--at", "at"),
        ("--tab", "tab"),
    ] {
        if let Some(v) = params.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                args.push(flag.to_string());
                args.push(v.to_string());
            }
        }
    }
    let out = std::process::Command::new(fno_agents::scrape::fno_bin())
        .args(&args)
        .output()
        .map_err(|e| {
            format!(
                "portal {portal} placement failed: {e} (the worker is live; \
                 place it with 'fno mux thread {name} --portal {portal}')"
            )
        })?;
    if !out.status.success() {
        let raw = if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        let detail = String::from_utf8_lossy(raw).trim().to_string();
        let detail = if detail.is_empty() {
            "no output".to_string()
        } else {
            detail
        };
        return Err(format!(
            "portal {portal} placement failed: {detail} (the worker is live; \
             place it with 'fno mux thread {name} --portal {portal}')"
        ));
    }
    if !out.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

/// The Python seam (rust_runtime.make_context -> inject_spawn_defaults) is
/// the only reader of config.agents.profiles. A spawn that skipped it carries
/// no configured route, model, effort or account, so it goes back to the
/// front door; the marker asserts the crossing and is parsed beside `--yolo`.
/// A marker is an upstream seam crossing, never proof a model is authorized -
/// the strict coordinate checks still own that.
fn spawn_needs_python_seam(params: &Value) -> bool {
    // FNO_SPAWN_GATE=0 is the operator bypass both gate implementations honor
    // (spawn_gate.rs, spawn_gate.py); it excuses the seam bounce the same way.
    if std::env::var_os("FNO_SPAWN_GATE").is_some_and(|v| v == "0") {
        return false;
    }
    params.get("defaults_applied").is_none()
}

/// Exec the Python front door with the given spawn argv. `fno` is the entry
/// point on a deployed machine; a bare venv install (CI runners included)
/// only ships `fno-py`, so a NotFound on the first candidate falls through
/// to the PATH-robust resolver ([`fno_agents::scrape::fno_py`]) - a bare
/// name here failed whenever the wheel bin was off PATH. Returns
/// the last exec error so the caller's refusal names reality.
fn exec_python_front(args: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(fno_agents::scrape::fno_bin())
        .arg("agents")
        .args(args)
        .env("FNO_AGENTS_RUNTIME", "python")
        .exec();
    if err.kind() == std::io::ErrorKind::NotFound {
        return std::process::Command::new(fno_agents::scrape::fno_py())
            .arg("agents")
            .args(args)
            .env("FNO_AGENTS_RUNTIME", "python")
            .exec();
    }
    err
}

// The `--agent` refusal for codex names the native role form, not generic advice.
fn agent_unsupported_line(provider: &str, agent: &str) -> Option<String> {
    let role = agent.strip_prefix("fno:").unwrap_or(agent);
    (provider == "codex").then(|| {
        format!(
            "--agent is not supported for harness 'codex': codex has no main-thread agent flag. \
             Its native form is the agent role .codex/agents/{role}.toml: seed the session to \
             call spawn_agent with agent_type {role}, or pass -H claude."
        )
    })
}

fn maybe_run_spawn(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    use fno_agents::agy_ask::dispatch_agy_once_with_effort;
    use fno_agents::claude_ask::{
        dispatch_claude_headless, dispatch_claude_spawn, py_repr, ClaudeHome,
    };
    use fno_agents::codex_ask::dispatch_codex_once;
    use fno_agents::gemini_ask::dispatch_gemini_once;
    use fno_agents::opencode_ask::dispatch_opencode_once;
    use fno_agents::state::load_registry;

    // refusal carrier, Rust lane: the verdict is computed HERE, in the
    // one owner of the grammar (`merge_posture`), so a direct `fno-agents
    // spawn` gets the same posture as a Python-fronted one. It is applied to
    // this process's env and every child env below inherits it (model_env_scrub
    // keeps TARGET_NO_MERGE as protected bookkeeping). Idempotent with the
    // Python lane's application (`harness_map.apply_merge_posture_env` in
    // `cmd_spawn`): both answer from the same table, so running twice
    // converges.
    if let Some(message) = params.get("message").and_then(|v| v.as_str()) {
        fno_agents::merge_posture::apply_env_from_message(message);
    }

    let provider_param = params.get("provider").and_then(|v| v.as_str());
    // `substrate` is a CLIENT-ONLY routing key: build_request validates and
    // inserts it (default `pane`) for the spawn verb and this is its sole
    // consumer. It is never forwarded in a daemon-bound request (the `pane`
    // fall-through below sends params WITHOUT it mattering; the daemon ignores
    // unknown params).
    let substrate = params
        .get("substrate")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| default_substrate(&params))
        .to_string();
    let substrate = if substrate == "thread" {
        "bg"
    } else {
        substrate.as_str()
    };

    // unwrap_or_default is acceptable HERE (unlike the ask pre-check, which
    // must exit 12 on a corrupt registry): this collision check is advisory;
    // the authoritative read happens again under the per-agent lock inside
    // dispatch_claude_spawn / dispatch_*_once, which surface a corrupt
    // registry as exit 12.
    let registry = load_registry(&home.registry_json()).unwrap_or_default();
    let existing_provider = registry.find(name).map(|e| e.harness_name().to_string());

    // Collision check: name already exists -> error.
    // Python: f"agent {name!r} already exists; ..." -> py_repr, not {:?}.
    if existing_provider.is_some() {
        eprintln!(
            "agent {} already exists; use 'fno agents rm {}' first or pick another name",
            py_repr(name),
            name
        );
        return Some(2);
    }

    // Resolve provider: explicit --provider > invoking-harness inference >
    // builtin `claude` (mirrors Python's resolve_dispatch_harness). A missing
    // flag no longer exits 2 -- that was the bg/headless split-brain vs pane,
    // which already infers via the Python re-exec.
    let provider = match provider_param {
        Some(p) => p,
        None => infer_dispatch_provider(|k| std::env::var(k).ok()),
    };

    // R3, the state-root grant gate. Placed HERE because every non-pane spawn
    // passes through this function exactly once, including the codex thread
    // spawn that returns None below and completes over the daemon RPC - one
    // gate evaluation per spawn, on both routes. `pane` is excluded because it
    // re-execs the Python CLI, whose own seam spends the grant.
    //
    // The public substrate name is what the capability table is keyed on; the
    // local `substrate` above has already been mapped to the historical `bg`
    // selector, so map it back rather than adding a second spelling to the
    // table.
    //
    // Skipped for a provider this binary does not know: the dispatch arms below
    // already refuse an unknown provider with exit 2 and the supported list,
    // which is the message that reader needs. Refusing first with grant advice
    // would shadow it with a diagnosis of the wrong problem.
    if substrate != "pane" && KNOWN_PROVIDERS.contains(&provider) {
        let declared_substrate = if substrate == "bg" {
            "thread"
        } else {
            substrate
        };
        let roots = fno_agents::claude_ask::state_dirs_from_env();
        if let Err(refusal) =
            fno_agents::spawn_gate::state_root_grant_gate(provider, declared_substrate, &roots)
        {
            return Some(refusal.exit_code);
        }
    }

    let message = effective_spawn_message(
        params.get("message").and_then(|v| v.as_str()).unwrap_or(""),
        substrate,
    );
    let from_name = params
        .get("from_name")
        .and_then(|v| v.as_str())
        .unwrap_or("fno");
    // --cwd > --here (caller) > default canonical; resolve_dispatch_cwd
    // canonicalizes an explicit --cwd and shells to git on the default path
    // (no --cwd, no --here). Resolve only for CLIENT-SIDE spawns, which are the
    // non-`pane` substrates (bg + headless).
    // The `pane` substrate falls through to the daemon RPC below, which resolves
    // canonical itself; resolving here too would double the git call and the
    // redirect note (review MEDIUM 3).
    // `surface_cwd` is the move decision resolve_dispatch_cwd already
    // made (the note condition), so the receipt's cwd field couples to the note
    // with no second comparison. pane re-execs Python and resolves canonical
    // itself, so it neither resolves cwd here nor surfaces it.
    let (cwd, surface_cwd) = if substrate == "pane" {
        (std::path::PathBuf::new(), false)
    } else {
        resolve_dispatch_cwd(params)
    };
    let timeout = params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_secs);
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Optional --model, forwarded to every provider's own --model (
    // wired codex/gemini/claude-headless; claude --bg was). Exact
    // passthrough appended to the worker argv.
    let model = params.get("model").and_then(|v| v.as_str());
    // permission mode for the bg/headless lanes. The pane substrate
    // never reaches here (it re-execs the Python CLI, which owns pane mapping);
    // this arm handles the claude bg/headless lanes only.
    let permission_mode = params.get("permission_mode").and_then(|v| v.as_str());
    let effort = params.get("effort").and_then(|v| v.as_str());
    // Tier-3 harness-native passthrough. add_dir has 3 real cells
    // (claude/codex/agy); agent/tools/deny_tools are claude-only on this
    // bg/headless lane. Every non-equivalent cell fails closed below (mirrors
    // --permission-mode /). The pane substrate re-execs the Python CLI,
    // which owns its own per-provider mapping + fail-closed for these.
    // Normalize empty-as-None once: an empty value is UNSET (the builders omit an
    // empty flag), so the guard below must not trip on `--add-dir=""` and the
    // bundle must carry None, not Some("").
    let empty_as_none = |k: &str| {
        params
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let add_dir = empty_as_none("add_dir");
    let agent = empty_as_none("agent");
    let tools = empty_as_none("tools");
    let deny_tools = empty_as_none("deny_tools");

    // Validate the provider FIRST so an unknown provider is a client-side
    // error (exit 2) for every substrate, never a fall-through to the daemon.
    if !KNOWN_PROVIDERS.contains(&provider) {
        eprintln!(
            "unknown provider {}; supported: {}",
            py_repr(provider),
            known_providers_csv()
        );
        return Some(2);
    }

    // AC5-ERR: one knob at a time (pane enforces this in Python; here for
    // bg/headless).
    if permission_mode.is_some() && yolo {
        eprintln!("--permission-mode and --yolo are mutually exclusive; pass one");
        return Some(2);
    }
    // Fail-closed (Locked Decision 1/2): only claude's bg/headless lanes accept
    // a mapped --permission-mode. gemini/agy one-shot lanes and the opencode
    // bg serve lane hardcode their own bypass form, so a mode here can't be
    // honored without a silent downgrade - reject it, pointing at the pane
    // substrate (which DOES map every provider's vocabulary). The codex
    // THREAD lane (substrate "bg" after the thread normalization) is exempt:
    // the shared app-server resolves the posture server-side
    // (resolve_thread_posture), so a mapped mode is native there.
    let codex_thread_lane = provider == "codex"
        && permission_mode
            .map(|mode| {
                // The capability table decides, through the one vocabulary
                // (see codex_posture.rs); a resolution problem answers
                // false, which degrades to the refusal below, never a
                // guessed yes. codex only: the shared app-server is the one
                // served thread destination, so a declared-thread harness
                // without one still refuses here, at the clearer gate.
                fno_agents::codex_posture::permission_mappable(provider, mode, substrate)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    if permission_mode.is_some() && provider != "claude" && !codex_thread_lane {
        let remedy = if provider == "codex" {
            "drop --permission-mode and pass -Y/--yolo"
        } else {
            "use --substrate pane"
        };
        eprintln!(
            "--permission-mode is not supported for harness {} on --substrate bg/headless (its one-shot lane hardcodes its own bypass form); {remedy}",
            py_repr(provider),
        );
        return Some(2);
    }
    if let Err(reason) = validate_effort_for_spawn(provider, substrate, effort) {
        eprintln!("{reason}");
        return Some(2);
    }
    // A fenced token naming a flag fno itself emits is two sources for one
    // value; the pane lane refuses that by name (pane_passthrough_tokens) and
    // so does this one, instead of letting argv order pick the winner.
    let carries_axis = |flag: &str| -> bool {
        let set = |k: &str| {
            params
                .get(k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .is_some()
        };
        match flag {
            "--permission-mode" => set("permission_mode"),
            "--effort" => set("effort"),
            "--add-dir" => set("add_dir"),
            "--agent" => set("agent"),
            "--tools" | "--allowedTools" => set("tools"),
            "--deny-tools" | "--disallowedTools" => set("deny_tools"),
            "--model" => set("model"),
            _ => false,
        }
    };
    for token in params
        .get("harness_args")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        let Some(flag) = token.strip_prefix('-') else {
            continue;
        };
        let flag = format!("--{}", flag.split('=').next().unwrap_or(flag));
        if carries_axis(&flag) {
            eprintln!(
                "refusing {flag} on both sides: fno emitted it from its own flag \
                 and the passthrough carries it too. Two sources for one value is \
                 the defect, not the collision. Drop one."
            );
            return Some(2);
        }
    }

    // fail-closed matrix for the client-owned bg/headless lanes (pane
    // re-execs Python, which guards there). A flag with no equivalent for the
    // resolved provider is a hard error BEFORE launch - never a silent drop.
    // Message shape mirrors --permission-mode. (opencode's bg lane DOES reach
    // these checks now: `--add-dir`/`--agent`-shaped flags on an opencode bg
    // spawn refuse HERE - the serve lane grants writable dirs itself, through
    // the per-session permission rules.)
    if substrate != "pane" {
        // No "use --substrate pane" advice: unlike --permission-mode, pane does
        // NOT map these cells any wider than bg/headless does (gemini --add-dir,
        // codex --agent fail closed on pane too), so that advice would mislead.
        let unsupported = |flag: &str| {
            eprintln!(
                "{} is not supported for harness {}; drop it or use a harness that maps it",
                flag,
                py_repr(provider)
            );
        };
        // --add-dir: claude/codex/agy map it; gemini has no verified equivalent.
        // (The codex thread lane carries it too: the client puts it ahead of
        // the state-root grant in params.state_dirs for the daemon.)
        if add_dir.is_some() && !matches!(provider, "claude" | "codex" | "agy") {
            unsupported("--add-dir");
            return Some(2);
        }
        // --agent / --tools / --deny-tools: claude-only on this lane.
        if agent.is_some() && provider != "claude" {
            match agent
                .as_deref()
                .and_then(|a| agent_unsupported_line(provider, a))
            {
                Some(line) => eprintln!("{line}"),
                None => unsupported("--agent"),
            }
            return Some(2);
        }
        if tools.is_some() && provider != "claude" {
            unsupported("--tools");
            return Some(2);
        }
        if deny_tools.is_some() && provider != "claude" {
            unsupported("--deny-tools");
            return Some(2);
        }
    }
    // fno's computed writable-dir set, published by the Python spawn seam. A
    // worker that cannot write ~/.fno takes no node claim, and the graph then
    // reads that node free while it works. Read once here so every lane below
    // carries it.
    let state_dirs = fno_agents::claude_ask::state_dirs_from_env();
    let harness_args: Vec<String> = params
        .get("harness_args")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // The claude-only bundle, resolved once for both claude lanes.
    let claude_flags = fno_agents::claude_ask::HarnessFlags {
        add_dir,
        state_dirs: &state_dirs,
        agent,
        allowed_tools: tools,
        disallowed_tools: deny_tools,
        passthrough: &harness_args,
    };

    // Spawn gate: cap + RAM floor for the CLIENT-SIDE substrates only.
    // `pane` re-execs into the Python CLI whose mirrored gate is the sole gate
    // on that path (exactly one gate evaluation per spawn, LD1). The guard is
    // held across dispatch so the next waiter's count includes the newcomer
    // (bg: the mutex until the roster/registry row exists; headless: the
    // worker:<name> slot claim for the call duration), then dropped.
    // (AC19): the codex THREAD spawn's registry row is created by the
    // DAEMON spawn RPC (the ("codex", "bg") arm below returns None), so a
    // guard held here drops before any row exists and two rapid thread spawns
    // both pass the cap. The gate moves to run(), wrapped around that RPC.
    let codex_thread_fallthrough = provider == "codex" && substrate == "bg";
    let mut gate_guard = if substrate == "pane" || codex_thread_fallthrough {
        None
    } else {
        let flags = gate_flags_from_params(&params);
        let config_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match fno_agents::spawn_gate::run_gate(
            &config_cwd,
            &home.registry_json(),
            fno_agents::spawn_gate::GateInput {
                name: name.to_string(),
                substrate: substrate.to_string(),
                flags,
                ..Default::default()
            },
        ) {
            Ok(g) => Some(g),
            Err(refusal) => {
                if let Some(receipt) = &refusal.receipt {
                    println!("{receipt}");
                }
                return Some(refusal.exit_code);
            }
        }
    };

    // Each provider module defines its OWN AskOutcome struct (nominally
    // distinct types), so `emit!` prints+returns inline per arm rather than via
    // one shared closure that could not name all four types.
    macro_rules! emit {
        ($outcome:expr) => {{
            let outcome = $outcome;
            if !outcome.stderr.is_empty() {
                eprint!("{}", outcome.stderr);
            }
            if !outcome.stdout.is_empty() {
                print!("{}", outcome.stdout);
            }
            Some(outcome.exit_code)
        }};
    }

    match (provider, substrate) {
        // pane (default): mux-hosted since 4a-G2. The caller intercepts pane
        // spawns BEFORE this fn and re-execs the Python CLI (mux_spawn back
        // half), so this arm is unreachable; None keeps the match total.
        (_, "pane") => None,

        // claude bg: the detached `claude --bg` thread (appears in `claude
        // agents`; attach/peek/reply; NOT a grid pane). claude-only by nature.
        ("claude", "bg") => {
            let claude_home = ClaudeHome::from_env();
            let daemon_receipt = match fno_agents::claude_ask::preflight_claude_daemon(&claude_home)
            {
                Ok(fno_agents::claude_ask::ClaudeDaemonPreflight::Ready(receipt)) => Some(receipt),
                Ok(fno_agents::claude_ask::ClaudeDaemonPreflight::NeedsBootstrap) => None,
                Err(error) => {
                    eprintln!(
                        "claude spawn refused: harness=claude observed=unreadable remedy=repair the Claude daemon roster: {error}"
                    );
                    if let Some(g) = gate_guard.as_mut() {
                        g.release();
                    }
                    return Some(13);
                }
            };
            // The refusal carrier rides the inherited env (see the note
            // above), so extra_env stays empty: a worker that drops the flag
            // post-compaction still folds the refusal at init.
            let mut outcome = dispatch_claude_spawn(
                home,
                &claude_home,
                name,
                &message,
                from_name,
                &cwd,
                yolo,
                timeout,
                &[],
                model,
                permission_mode,
                effort,
                claude_flags,
                surface_cwd,
            );
            if outcome.exit_code == 0 {
                let daemon_receipt = match daemon_receipt {
                    Some(receipt) => Ok(receipt),
                    None => fno_agents::claude_ask::ensure_claude_daemon(&claude_home),
                };
                match daemon_receipt {
                    Ok(receipt) => {
                        fno_agents::claude_ask::stamp_daemon_receipt(&mut outcome, &receipt)
                    }
                    Err(error) => {
                        outcome.exit_code = 13;
                        outcome.stdout.clear();
                        outcome.stderr.push_str(&format!(
                            "claude spawn refused after create: harness=claude observed=unreadable remedy=inspect the Claude daemon roster: {error}\n"
                        ));
                    }
                }
            }
            if !outcome.stderr.is_empty() {
                eprint!("{}", outcome.stderr);
            }
            if !outcome.stdout.is_empty() {
                print!("{}", outcome.stdout);
            }
            // Flush the receipt BEFORE the bounded QoS roster poll so
            // line-parsing consumers never wait on the demotion.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            // The roster row exists once dispatch returned: release the gate
            // NOW so the ~10s demotion poll never serializes other spawns
            // behind the spawn-gate mutex (codex P2).
            if let Some(g) = gate_guard.as_mut() {
                g.release();
            }
            if outcome.exit_code == 0 {
                // The bg worker is claude's child (its exec can't be wrapped);
                // demote post-hoc via the roster pid. short_id from the JSON
                // receipt line (parsed, not string-split — gemini HIGH).
                let parsed: Option<serde_json::Value> =
                    serde_json::from_str(outcome.stdout.trim()).ok();
                if let Some(sid) = parsed
                    .as_ref()
                    .and_then(|v| v.get("short_id"))
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                {
                    let config_cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    fno_agents::spawn_gate::qos_demote_bg_worker(&config_cwd, sid);
                }
            }
            Some(outcome.exit_code)
        }
        // claude headless: a truly headless `claude -p` one-shot (no thread, no
        // grid row; runs to completion and exits). The one place claude shells
        // `-p` (Locked Decision 4); ask/relay keep `--bg`.
        ("claude", "headless") => {
            let claude_home = ClaudeHome::from_env();
            emit!(dispatch_claude_headless(
                &claude_home,
                name,
                &message,
                from_name,
                &cwd,
                yolo,
                timeout,
                model,
                permission_mode,
                effort,
                claude_flags,
            ))
        }

        // codex/gemini/agy headless: the client-side one-shot (codex --exec /
        // gemini -p / agy -p).: --model is forwarded to each (exact
        // passthrough to the provider CLI's own --model).
        ("codex", "headless") => {
            // The daemon receipt is teardown telemetry for a one-shot spawn
            // (append_daemon_receipt only stamps stderr on success), never a
            // precondition: a codex whose app-server cannot boot (a stubbed
            // CLI on PATH, a partial install, CI) still runs `codex --exec`,
            // exactly like the Python lane that has no daemon step - refusing
            // here was exit-code drift against every such environment. The
            // daemon stays mandatory where it is load-bearing (inject/mail
            // ensure their own).
            let daemon_receipt = match fno_agents::codex_inject::ensure_codex_daemon() {
                Ok(result) => Some(result.receipt),
                Err(error) => {
                    eprintln!("spawn: harness=codex daemon-ensure degraded: {error}");
                    None
                }
            };
            let mut outcome = dispatch_codex_once(
                home,
                name,
                &message,
                from_name,
                &cwd,
                yolo,
                timeout,
                model,
                effort,
                add_dir,
                &harness_args,
            );
            if let Some(receipt) = daemon_receipt.as_ref() {
                fno_agents::codex_ask::append_daemon_receipt(&mut outcome, receipt);
            }
            emit!(outcome)
        }
        ("gemini", "headless") => emit!(dispatch_gemini_once(
            home, name, &message, from_name, &cwd, yolo, timeout, model,
        )),
        // opencode headless: the client-side one-shot
        // `opencode run --dangerously-skip-permissions` (wires the lane;
        // the bare `opencode` TUI stays the `pane` form). Stateless plain-text,
        // like agy. The flag is NOT `--auto`: that spelling is stale vendor
        // docs, it does not exist in `run --help`, and a comment naming it has
        // twice been read as proof this arm was never built.
        ("opencode", "headless") => emit!(dispatch_opencode_once(
            home,
            name,
            &message,
            from_name,
            &cwd,
            yolo,
            timeout,
            model,
            effort,
            &harness_args,
        )),

        // opencode bg: the serve-HTTP worker lane. A shared
        // `opencode serve` hosts a persistent session bound to the worker
        // cwd; a detached `opencode run --attach` writer streams the turn's
        // JSON events to the log. The spawn returns immediately - the session
        // on the serve IS the worker (steering/mail over the API is a filed
        // follow-up).
        ("opencode", "bg") => emit!(fno_agents::opencode_serve::dispatch_opencode_serve(
            home,
            name,
            &message,
            from_name,
            &cwd,
            model,
            effort,
            params.get("node").and_then(|v| v.as_str()),
        )),

        ("agy", "headless") => {
            // agy is stateless (plain text, no session id): a one-shot `agy -p`.
            // It ignores `yolo` (headless create always passes
            // --dangerously-skip-permissions) and honors optional effort/model.
            emit!(dispatch_agy_once_with_effort(
                home,
                name,
                &message,
                from_name,
                &cwd,
                model,
                effort,
                timeout,
                add_dir,
                &harness_args,
            ))
        }

        // Codex thread is supervisor-hosted by the daemon. Returning `None`
        // preserves the request's `substrate=thread` so the daemon can own the
        // held app-server process and register its full thread identity.
        ("codex", "bg") => None,

        // The three arms above stay NAMED rather than lane-routed, because
        // they are three different ownership models: claude's thread is
        // hosted by the detached client itself, codex's by its own
        // app-server, and opencode's by a serve-hosted HTTP session -
        // `thread_lane` classifies all three `attach` (each answers "the
        // harness owns the live session; a client re-attaches"), but that one
        // label can't tell a client-side re-attach apart from a daemon-owned
        // serve process, and each needs its own dispatch call. (Before,
        // opencode's row read `keeper` here - a stale answer this match arm
        // had to route around by name; the row now agrees with the dispatch
        // below.)
        //
        // The REFUSAL is derived, because a provider name list goes stale the
        // moment a lane is built and then misdirects the reader it was meant
        // to help. Hard error pointing to headless; never a silent substrate
        // swap.
        (other, "bg") => {
            eprintln!("{}", bg_substrate_refusal(other));
            Some(2)
        }

        // Unreachable: provider is validated known above and substrate is
        // validated to pane|bg|headless in build_request.
        _ => None,
    }
}

/// The `--substrate bg` refusal, derived from the capability contract instead
/// of a provider name list.
///
/// A name list cannot say what is actually true. A harness whose keeper lane
/// is built and journey-proven, but whose spawn arm is not, is refused here
/// today; a list reading "claude + codex + opencode" tells that reader the
/// harness has no thread lane, which is the opposite of its situation. The
/// lane is what the reader needs, and the contract already knows it.
///
/// Mirrors the wording `resolve_dispatch` uses in `harness_map.py`, so both
/// runtimes name the same gap: it is in fno, never a harness limitation.
fn bg_substrate_refusal(harness: &str) -> String {
    use fno_agents::claude_ask::py_repr;

    // Name `thread`, not the `bg` selector this match arm is keyed on. `bg` is
    // the deprecated alias, so a user who typed `--substrate thread` was being
    // refused in a vocabulary they did not use and are being moved off.
    let head = format!(
        "substrate 'thread' (detached interactive session) is unavailable on harness {}",
        py_repr(harness)
    );
    let tail = "use --substrate headless for a one-shot";
    let contract = fno_agents::harness_capabilities::HarnessContract::packaged().ok();
    // A refused command_surface (a deprecated harness, e.g. gemini) has no
    // dispatch lane at all - check this BEFORE thread_lane, which would
    // otherwise describe a retired harness as future lane work (PR 1355
    // review, P2). Mirrors harness_map._refused_reason's wording so both
    // runtimes name the same gap the same way.
    if let Some(caps) = contract.as_ref().and_then(|c| c.capabilities(harness).ok()) {
        if caps.command_surface == "refused" {
            return format!(
                "harness {} has no maintained footnote dispatch lane and is deprecated; \
                 route this work to its successor 'agy' (or a claude/codex/opencode harness) \
                 - no prose build brief is generated",
                py_repr(harness)
            );
        }
    }
    let lane = contract.and_then(|contract| contract.thread_lane(harness).ok());
    match lane {
        // No resume form at all, so there is no lane for fno to build.
        Some("none") => {
            format!("{head}: it declares no resume form, so no thread lane exists for it - {tail}")
        }
        Some(lane) => format!(
            "{head}: fno has not built this harness's {lane} lane spawn arm yet, and that gap is \
             in fno, never a harness limitation - {tail}"
        ),
        // An unreadable table is its own diagnosis, and naming a lane we could
        // not resolve would be a guess wearing a verdict's clothes.
        None => format!(
            "{head}: its thread lane could not be resolved from the capability contract - {tail}"
        ),
    }
}

/// Surface for an `ask` that resolved to no known provider: a create with no
/// `--provider` (or an unknown one). Reproduces Python's `select_provider`
/// exit-2 error text byte-for-byte (`dispatch.py` wraps both the
/// `_check_known_provider` ValueError and the "provider is required for new
/// agent" ValueError as `DispatchAskError(..., exit_code=2)`, which `cmd_ask`
/// prints to stderr verbatim). Never routes to the daemon (Locked Decision 3).
fn unresolvable_ask_exit(params: &Value, name: &str) -> i32 {
    use fno_agents::claude_ask::py_repr;
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let msg = match provider_param {
        // `select_provider` validates the requested provider FIRST, so an
        // unknown `--provider` surfaces the "unknown provider" error.
        Some(p) if !KNOWN_PROVIDERS.contains(&p) => format!(
            "unknown provider {}; supported: {}",
            py_repr(p),
            known_providers_csv()
        ),
        // New agent with no resolvable provider.
        _ => format!(
            "provider is required for new agent {}; pass --provider one of: {}",
            py_repr(name),
            known_providers_csv()
        ),
    };
    eprintln!("{}", msg);
    2
}

/// One-line pointers for the verbs retired at G4: the grid, the
/// WebSocket drive surface, and the interactive daemon PTY hosting behind
/// `host`/`promote` moved to the mux. Returns `None` for a live verb. Callers
/// print the pointer and exit non-zero so a script never reads a retired verb
/// as a silent success (AC5-EDGE).
fn retired_verb_pointer(verb: &str) -> Option<&'static str> {
    match verb {
        "grid" => Some(
            "fno agents grid was retired at G4: agent panes now live in the mux. \
             Open `fno mux`, or script panes with `fno mux pane ls|read|run|send|wait|kill`.",
        ),
        "drive" => Some(
            "fno agents drive was retired at G4: drive an agent pane in the mux. \
             Use `fno mux pane send <pane> --raw ...` for keystrokes (without --raw the payload is wrapped in an <fno_mail> envelope), or open `fno mux` and type into the pane.",
        ),
        "host" => Some(
            "fno agents host was retired at G4: spawn a mux-hosted agent pane with \
             `fno agents spawn --name <n> --substrate pane`.",
        ),
        "promote" => Some(
            "fno agents promote was retired at G4: the mux hosts agent panes; spawn one with \
             `fno agents spawn --name <n> --substrate pane`.",
        ),
        _ => None,
    }
}

/// Dispatch `fno-agents status`: probe an already-running daemon and print its
/// `status-v1.json` (with `--json`), or the human arms table plus daemon lines
/// (default). The control-plane arms readout is read straight from the event
/// journals, so it survives a down daemon: exit 13 still signals the daemon,
/// but the arms rows print either way.
async fn run_status(json_out: bool) -> i32 {
    let home = AgentsHome::from_env();
    let (mut arms, trace) = arms_readout(&home);
    let req = Request::new(1, "agent.status", Value::Object(Map::new()));
    match call_if_running(&home, &req).await {
        Ok(resp) => match resp.payload {
            ResponsePayload::Err(err) => {
                eprintln!("fno-agents: {}", err.message);
                exit_code_for(err.code)
            }
            ResponsePayload::Ok(mut result) => {
                // One drift read feeds both the stderr warning below and the
                // facts the arms rows are explained against.
                let drift = drift_from_status(&result);
                let uptime_s = result
                    .pointer("/daemon/uptime_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let drifted = matches!(drift, DriftState::Drifted { .. });
                fno_agents::arm_repair::explain(
                    &mut arms,
                    &fno_agents::tick_ledger::DaemonFacts::Up { uptime_s, drifted },
                    &trace,
                );
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "arms".into(),
                        serde_json::to_value(&arms).unwrap_or(Value::Null),
                    );
                    // The Rust-owned attention set beside the full list, so
                    // every consumer reads the same verdict.
                    obj.insert(
                        "arms_attention".into(),
                        serde_json::to_value(arms_attention(&arms)).unwrap_or(Value::Null),
                    );
                    // What work is stuck right now: hung verbs and dead
                    // flight holders, from the one stuck_work read.
                    obj.insert(
                        "stuck_work".into(),
                        fno_agents::stuck_work::status_value(
                            &std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
                        ),
                    );
                    // change 4: the drift verdict as a field, so the
                    // census reads it from JSON instead of regex-parsing the
                    // stderr sentence.
                    obj.insert(
                        "drift".into(),
                        json!(fno_agents::drift::drift_label(&drift)),
                    );
                }
                if json_out {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result).unwrap_or_default()
                    );
                } else {
                    print_status_human(&result, &arms);
                }
                // Drift warning, stderr-only so --json/automation
                // consumers of stdout are never contaminated. We already hold the
                // status payload, so classify from it without a second RPC.
                let pid = result
                    .get("daemon")
                    .and_then(|d| d.get("pid"))
                    .and_then(Value::as_u64)
                    .map(|p| p as u32);
                if let Some(w) = drift_warning(&drift, pid) {
                    eprintln!("{w}");
                }
                0
            }
        },
        Err(ClientError::DaemonNotRunning) => {
            // The arms table is exactly what a dead control plane needs to
            // show; print it beside the down-daemon signal rather than nothing.
            fno_agents::arm_repair::explain(
                &mut arms,
                &fno_agents::tick_ledger::DaemonFacts::Down,
                &trace,
            );
            let payload = degraded_status_payload(&arms);
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                print_status_human(&payload, &arms);
            }
            eprintln!("fno-agents: daemon not running");
            13
        }
        Err(e) => {
            // Unreachable daemon (socket error, timeout, ...): same degraded
            // shape as DaemonNotRunning - the arms readout stands on its own.
            // Daemon rules do not fire on Unknown, so stale rows read
            // `unexplained` rather than blaming a daemon of unknown health.
            fno_agents::arm_repair::explain(
                &mut arms,
                &fno_agents::tick_ledger::DaemonFacts::Unknown,
                &trace,
            );
            let payload = degraded_status_payload(&arms);
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                print_status_human(&payload, &arms);
            }
            eprintln!("fno-agents: {e}");
            1
        }
    }
}

/// The control-plane arms rows from the arm journals plus the pr_watch tick
/// trace the readout's cause rules consult.
fn arms_readout(
    home: &AgentsHome,
) -> (
    Vec<fno_agents::tick_ledger::ArmStatus>,
    fno_agents::tick_ledger::TickTrace,
) {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // The journal list is owned by tick_ledger::journals, so the readout
    // and the arm_watch fold the same files and cannot drift.
    let journals = fno_agents::tick_ledger::journals(home);
    let arms = fno_agents::tick_ledger::read_arms_starved(&journals, now_unix);
    let trace = fno_agents::tick_ledger::read_tick_trace_live(&journals, &arms, now_unix);
    (arms, trace)
}

/// The Rust-owned attention set: rows needing operator attention (unobserved,
/// stale, or failing), selected by the one predicate in `tick_ledger`. The
/// Python doctor consumes this list as its `red` set and never re-derives the
/// verdict from the legacy booleans.
fn arms_attention(
    arms: &[fno_agents::tick_ledger::ArmStatus],
) -> Vec<fno_agents::tick_ledger::ArmStatus> {
    arms.iter()
        .filter(|r| fno_agents::tick_ledger::needs_attention(r))
        .cloned()
        .collect()
}

/// The degraded status payload (daemon down or unreachable): the arms readout
/// stands on its own, carrying both the full row list and the Rust-owned
/// attention set, so a daemon's health never changes the payload schema.
fn degraded_status_payload(arms: &[fno_agents::tick_ledger::ArmStatus]) -> Value {
    json!({
        "schema_version": 1,
        "daemon": null,
        "arms": arms,
        "arms_attention": arms_attention(arms),
        "stuck_work": fno_agents::stuck_work::status_value(
            &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        ),
    })
}

/// The human render: one owned line per arm (red rows first-class), then the
/// daemon block the JSON payload carries. `explain` filled every row's
/// `line`, so the render prints them without re-formatting.
fn print_status_human(result: &Value, arms: &[fno_agents::tick_ledger::ArmStatus]) {
    println!("control-plane arms:");
    for arm in arms {
        println!("  {}", arm.line);
    }
    // The readout teaches its own detail verb (the loops table).
    println!("  loop detail: fno agents loops table");
    if let Some(stuck) = result.get("stuck_work") {
        for line in fno_agents::stuck_work::render_lines(stuck) {
            println!("{line}");
        }
    }
    let Some(daemon) = result.get("daemon").and_then(Value::as_object) else {
        return;
    };
    let state = daemon.get("state").and_then(Value::as_str).unwrap_or("?");
    let pid = daemon.get("pid").and_then(Value::as_u64);
    let uptime = daemon.get("uptime_secs").and_then(Value::as_u64);
    println!(
        "daemon: {state}{}{}",
        pid.map(|p| format!(" pid={p}")).unwrap_or_default(),
        uptime
            .map(|u| format!(" uptime={}s", u))
            .unwrap_or_default(),
    );
    if let Some(agents) = result.get("agents").and_then(Value::as_object) {
        println!(
            "agents: total={} by_status={}",
            agents.get("total").and_then(Value::as_u64).unwrap_or(0),
            agents
                .get("by_status")
                .map(|s| s.to_string())
                .unwrap_or_else(|| "{}".into())
        );
    }
    // Best-effort: a machine whose footprint cannot be read prints no line
    // rather than a stale or fabricated one. The keeper note names the path
    // the live store keeper runs from, beside the footer (AC13).
    let (machine_line, keeper_note) = machine_reading_notes();
    if let Some(line) = machine_line {
        println!("machine: {line}");
    }
    if let Some(note) = keeper_note {
        println!("{note}");
    }
}

/// `fno agents reap`: manual row retirement. Runs the same
/// `gc_sweep` the daemon runs on its idle tick, operating on the registry
/// directly under the shared flock (no daemon required), and reports what it
/// did: every row retired with its basis, and for each row KEPT, the named
/// gate holding it, so a stuck row is never silent and invisible, and a
/// zero-reap pass over a live fleet is never silent about the rows it kept.
/// The grace window is resolved from `config.agents.retire_grace_s` exactly
/// as the daemon does.
///
/// `--dry-run` runs the identical classification with no registry write and no
/// `agent_row_reaped` event - a reaper an operator cannot rehearse is one they
/// will not run.
fn run_reap(rest: &[String]) -> i32 {
    let json_out = rest.iter().any(|a| a == "--json" || a == "-J");

    if rest.iter().any(|arg| arg == "--state-files-only") {
        let apply = rest.iter().any(|arg| arg == "--apply");
        let explicit_dry_run = rest.iter().any(|arg| arg == "--dry-run");
        if apply && explicit_dry_run {
            eprintln!("fno-agents: reap --state-files-only cannot combine --apply and --dry-run");
            return 2;
        }
        let extras: Vec<&str> = rest
            .iter()
            .map(String::as_str)
            .filter(|arg| {
                !matches!(
                    *arg,
                    "--state-files-only" | "--apply" | "--dry-run" | "--json" | "-J"
                )
            })
            .collect();
        if !extras.is_empty() {
            eprintln!(
                "fno-agents: reap --state-files-only takes only --apply/--dry-run/--json (got: {})",
                extras.join(" ")
            );
            return 2;
        }
        let home = AgentsHome::from_env();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let summary = fno_agents::gc_sweep::reap_state_files_for_cwd(
            &home,
            &cwd,
            fno_agents::agents_config::state_reap_config(&cwd),
            apply,
        );
        print!(
            "{}",
            fno_agents::reap_render::render_state_files_reap(&summary, json_out)
        );
        return i32::from(fno_agents::gc_sweep::state_reap_has_failures(&summary));
    }

    // The verify probe (task 5): read-only audit of the receipts
    // store over `--since`, pinned to THIS build. Nonzero exit on any
    // unmet condition - empty window, stale build, partial effects - so the
    // plan's done probe cannot pass on CI-green alone.
    if rest.iter().any(|a| a == "--verify") {
        let since = match rest.iter().position(|a| a == "--since") {
            Some(i) => match rest
                .get(i + 1)
                .and_then(|v| fno_agents::duration::parse_duration_secs(v))
            {
                Some(secs) => secs,
                None => {
                    eprintln!(
                        "fno-agents: --since needs a duration like 24h (got: {:?})",
                        rest.get(i + 1).map(String::as_str).unwrap_or("")
                    );
                    return 2;
                }
            },
            None => 24 * 3600,
        };
        // Exactly --verify, --since <dur>, --expect-sessions <a,b>,
        // --json/-J: anything else is a usage error, checked with a plain
        // skip-list walk.
        let mut extras: Vec<&str> = Vec::new();
        let mut expected: Vec<String> = Vec::new();
        let mut cohort_given = false;
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--verify" | "--json" | "-J" => {}
                "--since" => {
                    i += 1; // the duration value rides with --since
                }
                "--expect-sessions" => {
                    cohort_given = true;
                    let value = rest.get(i + 1).map(String::as_str).unwrap_or("");
                    if value.is_empty() {
                        eprintln!(
                            "fno-agents: --expect-sessions needs a comma-separated session list (got: {:?})",
                            rest.get(i + 1).map(String::as_str).unwrap_or("")
                        );
                        return 2;
                    }
                    expected.extend(
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                    );
                    i += 1; // the cohort value rides with --expect-sessions
                }
                other => extras.push(other),
            }
            i += 1;
        }
        // A cohort given but normalizing to nothing ("," or whitespace) must
        // be a usage error, never a silently disabled check (codex P2).
        if cohort_given && expected.is_empty() {
            eprintln!(
                "fno-agents: --expect-sessions normalized to an empty cohort; name at least one session"
            );
            return 2;
        }
        if !extras.is_empty() {
            eprintln!(
                "fno-agents: reap --verify takes only --since/--expect-sessions/--json (got: {})",
                extras.join(" ")
            );
            return 2;
        }
        let home = AgentsHome::from_env();
        let report = fno_agents::gc_verify::verify(&home, since, &expected);
        if json_out {
            println!("{}", report.to_json());
        } else {
            println!(
                "verified {} of {} receipt(s) in the last {}s (build {})",
                report.verified.len(),
                report.checked,
                since,
                fno_agents::gc_verify::current_build()
            );
            for v in &report.verified {
                println!(
                    "  ok {} {} ({}) at {}",
                    v.harness, v.session_id, v.row_name, v.reaped_at
                );
                for effect in &v.effects {
                    println!("    {effect}");
                }
            }
            for p in &report.problems {
                println!("  REFUSED {}: {}", p.receipt, p.reason);
            }
            for s in &report.skipped {
                println!("  SKIP {}: {}", s.receipt, s.reason);
            }
        }
        return if report.passes() { 0 } else { 1 };
    }

    // The release verb: apply a ruling to one escalated hold
    // through the sweep's own door. Classify, refuse fresh holds and open
    // work by name, apply, print the whole-sweep receipt.
    if let Some(pos) = rest.iter().position(|a| a == "--release") {
        let handle = rest.get(pos + 1).map(String::as_str).unwrap_or("");
        if handle.is_empty() || handle.starts_with("--") {
            eprintln!(
                "fno-agents: reap --release needs a row handle (name, short id or session id)"
            );
            return 2;
        }
        // Everything except the flag pair and the handle is an error; only
        // --json is legal beside a release.
        let extras: Vec<String> = rest
            .iter()
            .enumerate()
            .filter(|(i, a)| *i != pos && *i != pos + 1 && !matches!(a.as_str(), "--json" | "-J"))
            .map(|(_, a)| a.clone())
            .collect();
        if !extras.is_empty() {
            eprintln!(
                "fno-agents: reap --release takes only a handle and --json (got: {})",
                extras.join(" ")
            );
            return 2;
        }
        let home = AgentsHome::from_env();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        return fno_agents::reap_release::run(&home, &cwd, handle);
    }

    let dry_run = rest.iter().any(|a| a == "--dry-run");
    let no_mux = rest.iter().any(|a| a == "--no-mux");
    let extras: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json" && *a != "-J" && *a != "--dry-run" && *a != "--no-mux")
        .collect();
    if !extras.is_empty() {
        eprintln!(
            "fno-agents: reap takes no arguments other than --json/--dry-run/--no-mux (got: {})",
            extras.join(" ")
        );
        return 2;
    }
    let home = AgentsHome::from_env();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let grace_secs = fno_agents::agents_config::retire_grace_secs(&cwd) as i64;
    // Dead crowns first, matching the arm's in-arm order.
    let crowns = fno_agents::crown_reap::production_sweep(&home, &cwd, !dry_run);
    let mut summary = if dry_run {
        fno_agents::daemon::gc_sweep_dry_run(&home, grace_secs)
    } else {
        // Source "daemon": the manual verb is the same operation as the tick.
        let emitter = fno_agents::events::EventEmitter::new(home.events_jsonl(), "daemon");
        fno_agents::daemon::gc_sweep(
            &home,
            &emitter,
            grace_secs,
            fno_agents::agents_config::reap_receipt_retain_days(&cwd),
        )
    };
    summary.mark_escalated(fno_agents::agents_config::hold_escalate_after(&cwd));
    summary.crowns = Some(crowns);

    // The dry-run JSON read also carries the census (task 4): who would
    // retire and what was seen, one read.
    let inventory = if dry_run && json_out {
        Some(fno_agents::gc_inventory::census(&home))
    } else {
        None
    };
    // The mux sideline sweep: the registry pass above reaps rows,
    // but ghost panes are the surface an operator SEES. The sweep body stays
    // the one prune verb (reused, not reimplemented); `--no-mux` skips it.
    // The manual verb keeps `--include-used-shells`: closing a human's spent
    // shells is an attended choice, never the daemon's default.
    let mux = if no_mux {
        fno_agents::reap_render::MuxSweep::Skipped
    } else {
        fno_agents::gc::mux_tab_sweep(dry_run, true)
    };
    print!(
        "{}",
        fno_agents::reap_render::render_reap_with_inventory(
            &summary,
            inventory.as_ref(),
            Some(&mux),
            json_out,
            dry_run
        )
    );
    0
}

/// `fno-agents roster-reap`: the roster-side sweep (gap one). Dry-run
/// by default; `--apply` executes. Takes only --json/--apply.
fn run_roster_reap(rest: &[String]) -> i32 {
    let json_out = rest.iter().any(|a| a == "--json" || a == "-J");
    let dry_run = !rest.iter().any(|a| a == "--apply");
    let extras: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json" && *a != "-J" && *a != "--apply")
        .collect();
    if !extras.is_empty() {
        eprintln!(
            "fno-agents: roster-reap takes no arguments other than --json/--apply (got: {})",
            extras.join(" ")
        );
        return 2;
    }
    let home = AgentsHome::from_env();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let grace_secs = fno_agents::agents_config::retire_grace_secs(&cwd) as i64;
    let scope = fno_agents::agents_config::roster_scope(&cwd);
    let summary = fno_agents::roster_reap::roster_reap(&home, &cwd, grace_secs, scope, dry_run);
    print!(
        "{}",
        fno_agents::roster_reap::render(&summary, json_out, dry_run)
    );
    if summary.nothing_resolved() {
        eprintln!(
            "fno-agents: roster-reap refused: probed {} row(s), the truth probe resolved none",
            summary.probed
        );
        return 1;
    }
    0
}

/// `fno-agents node-route --names a,b --json`: the provenance cascade
/// verdict per NAME. One graph read, one JSON answer per name:
/// `retire-eligible` when the cascade resolves a done node and the PR
/// confirm passes, `held` / `open` / `unresolved` otherwise. A name is
/// never resolved from anything but the declared sources.
fn run_node_route(rest: &[String]) -> i32 {
    let mut names: Vec<String> = Vec::new();
    let mut pairs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--names" => {
                let Some(value) = rest.get(i + 1) else {
                    eprintln!("fno-agents: node-route --names needs a comma-separated list");
                    return 2;
                };
                names.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(str::to_string),
                );
                i += 2;
            }
            "--pairs" => {
                let Some(value) = rest.get(i + 1) else {
                    eprintln!("fno-agents: node-route --pairs needs a comma-separated list");
                    return 2;
                };
                pairs.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                );
                i += 2;
            }
            "--json" | "-J" => i += 1,
            other => {
                eprintln!(
                    "fno-agents: node-route takes only --names/--pairs/--json (got: {other})"
                );
                return 2;
            }
        }
    }
    if names.is_empty() && pairs.is_empty() {
        eprintln!("fno-agents: node-route needs --names or --pairs");
        return 2;
    }
    let home = AgentsHome::from_env();
    let graph = fno_agents::gc_sweep::read_graph_entries(&home);
    let mut answers = serde_json::Map::new();
    for name in &names {
        let mut entry = fno_agents::state::RegistryEntry::new(
            None,
            fno_agents::state::Lineage::unproven("synthetic row for a read, never written"),
        );
        entry.name = name.clone();
        let answer = match &graph {
            None => serde_json::json!({"state": "graph-unreadable"}),
            Some(g) => {
                let verdict = fno_agents::gc_sweep::provenance_verdict(&entry, "", g, None, None);
                let node = verdict.route.node.clone();
                let basis = format!(
                    "via {}",
                    verdict
                        .route
                        .source
                        .map(|s| s.as_str())
                        .unwrap_or("sessions")
                );
                match (&verdict.work, &verdict.hold) {
                    (_, Some(hold)) => serde_json::json!({
                        "state": "held",
                        "node": node,
                        "reason": hold.as_str(),
                    }),
                    (fno_agents::graph_store::WorkState::AllDone { .. }, None) => {
                        serde_json::json!({"state": "retire-eligible", "node": node, "basis": basis})
                    }
                    (fno_agents::graph_store::WorkState::Open { node: n, status }, None) => {
                        serde_json::json!({"state": "open", "node": node, "reason": format!("{n} {status}")})
                    }
                    (fno_agents::graph_store::WorkState::NoProvenance, None) => {
                        serde_json::json!({"state": "unresolved"})
                    }
                }
            }
        };
        answers.insert(name.clone(), answer);
    }
    // The transcript-quiet pair leg: a session-keyed member whose registry
    // row went stale still reads LIVE while its transcript is fresh - the
    // same activity evidence the reap's quiet gate trusts. `harness:sid`
    // answers `live` (fresh), `quiet` (stale), or `unresolved` (no store
    // read); a name never consults a transcript it has no session for.
    if !pairs.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let grace_secs = fno_agents::agents_config::retire_grace_secs(&cwd) as i64;
        let mut store = fno_agents::gc_inventory::HarnessStoreIndex::default();
        for pair in &pairs {
            let Some((harness, sid)) = pair.split_once(':') else {
                answers.insert(
                    pair.clone(),
                    serde_json::json!({"state": "unresolved", "reason": "not harness:sid"}),
                );
                continue;
            };
            let mut entry = fno_agents::state::RegistryEntry::new(
                Some(sid.to_string()),
                fno_agents::state::Lineage::unproven("synthetic row for a read, never written"),
            );
            entry.harness = Some(harness.to_string());
            let answer = match store.matches(&entry) {
                // the age is the newest timestamped entry through the
                // shared probe, not a file stat.
                Some(hits) if !hits.is_empty() => {
                    // The same batched seam the sweeps take, not a deleted
                    // per-row wrapper: the entry's harness_session_id is set,
                    // so `row_handle` returns it and that is the key the map
                    // answers under.
                    let age = fno_agents::gc::probe_entry_ages(&[&entry])
                        .get(&fno_agents::gc::row_handle(&entry))
                        .copied()
                        .flatten();
                    match age {
                        Some(age) if age <= grace_secs => serde_json::json!({"state": "live"}),
                        Some(_) => serde_json::json!({"state": "quiet"}),
                        None => serde_json::json!({"state": "unresolved"}),
                    }
                }
                _ => serde_json::json!({"state": "unresolved"}),
            };
            answers.insert(pair.clone(), answer);
        }
    }
    println!("{}", serde_json::Value::Object(answers));
    0
}

/// Mint a random UUID (RFC-4122 v4) to pin an interactive claude `--session-id`.
/// The daemon refuses an interactive claude host without a pinned session id
/// (the single-writer claim + transcript discovery key on it); a fresh host
/// supplies one client-side.
// ponytail: v4 from getrandom (the OS CSPRNG), not the `uuid` crate.
// `--session-id` only needs a unique, well-formed UUID -- v7's time-ordering
// buys nothing for a session pin. getrandom is already in the tree, so this
// adds no compile cost and is cross-platform (unlike a `/dev/urandom` read).
fn mint_session_uuid() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // Never panic: mix wall-clock nanos with the pid. Collision is
        // implausible for a session pin and getrandom is the real path.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mix = nanos ^ ((std::process::id() as u128) << 96);
        b = mix.to_be_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC-4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Build (method, params) from a verb and its flags.
/// Apply the owned-interactive (drivable grid pane) defaults to a spawn/host
/// request. Sets `host_mode=interactive`; for claude additionally defaults the
/// PTY lane (`mode=interactive`) and mints a `session_id` when none is pinned or
/// resumed (the daemon's single-writer claim + transcript discovery key on it).
///
/// Shared by `host` (always interactive) and `spawn` (default for PTY providers
/// unless `--once`) so the claude mint lives in exactly ONE place. An
/// explicit `--mode` wins, so `--mode stream_json` opts a claude spawn back out
/// of the PTY lane. Non-claude providers get only `host_mode`; their create argv
/// stays byte-unchanged (the mint is claude-only, mirroring the host contract).
fn apply_interactive_defaults(params: &mut Map<String, Value>) {
    params.insert(
        "host_mode".into(),
        Value::String(fno_agents::state::HOST_MODE_INTERACTIVE.into()),
    );
    if params.get("provider").and_then(Value::as_str) == Some("claude") {
        // claude has two interactive lanes; default the owned-PTY pane unless the
        // caller explicitly picked one via --mode.
        if !params.contains_key("mode") {
            params.insert(
                "mode".into(),
                Value::String(fno_agents::state::CLAUDE_MODE_INTERACTIVE.into()),
            );
        }
        let is_pty_lane = params.get("mode").and_then(Value::as_str)
            == Some(fno_agents::state::CLAUDE_MODE_INTERACTIVE);
        if is_pty_lane && !params.contains_key("session_id") && !params.contains_key("resume_id") {
            params.insert("session_id".into(), Value::String(mint_session_uuid()));
        }
    }
}

/// The substrate a spawn with NO explicit `--substrate` gets: thread where the
/// harness seats one, else the closable pane. The Python seam (the public
/// `fno agents spawn` front door) resolves the SAME default in its own body
/// and opens the thread's default view through the mux thread verb, so this
/// helper only answers for a DIRECT binary call.
/// It seats only the three lanes this client itself routes (claude/codex
/// bg, opencode serve); a keeper-lane harness (pi, cursor-agent, grok, agy)
/// keeps the pane default here and seats its thread through the Python seam's
/// keeper carve-out instead. The harness default matches the daemon's
/// `handle_spawn` provider default (codex) so a bare direct call and the
/// daemon route cannot disagree.
fn default_substrate(params: &Value) -> &'static str {
    let harness = params
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("codex");
    match harness {
        "claude" | "codex" | "opencode" => "thread",
        _ => "pane",
    }
}

/// Join the typed `--add-dir` ahead of the seam-published state-root grant on
/// a daemon-bound codex thread spawn. The operator's own grant leads, the same
/// precedence the argv lanes give it. Extracted from `run` so the ordering
/// contract stays unit-testable.
fn attach_codex_thread_state_dirs(params: &mut Value) {
    let mut roots = fno_agents::claude_ask::state_dirs_from_env();
    if let Some(add_dir) = params
        .get("add_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        roots.insert(0, add_dir.to_string());
    }
    if !roots.is_empty() {
        params["state_dirs"] = Value::from(roots);
    }
}

fn build_request(verb: &str, rest: &[String]) -> Result<(String, Value), String> {
    let mut params = Map::new();
    let mut positional: Vec<String> = Vec::new();
    let mut argv: Option<Vec<String>> = None;
    // Where a `--` fence drained the remaining tokens, counted in pre-fence
    // positionals; spawn uses it to tell a passthrough tail from a seed.
    let mut fence_at: Option<usize> = None;

    // Click/Typer accepts `--flag=value` for every string option; the Python
    // path forwards e.g. `fno agents ask <name> <msg> --cwd=/repo --timeout=30
    // --from-name=bot --provider=codex` verbatim. Since `ask` now auto-routes to
    // this client for EVERY provider, the binary must accept the
    // equals form for ALL value-carrying flags, not just --provider/--from --
    // otherwise a routed `--cwd=...` / `--timeout=...` / `--from-name=...`
    // regresses to "unknown flag" instead of reaching the dispatch (Codex P2 on
    // PR #379; same regression class as PR #371's --provider=). Normalize
    // `--flag=value` into two tokens up front so the space-form match arms below
    // handle both syntaxes uniformly.
    const VALUE_FLAGS: &[&str] = &[
        "--provider",
        "--harness",
        "--from",
        "--cwd",
        "--message",
        "--name",
        "--node",
        "--session-id",
        "--status",
        "--progress",
        "--from-name",
        "--timeout",
        "--model",
        "--mode",
        "--substrate",
        "--workspace",
        "--squad",
        "--split",
        "--portal",
        "--tab",
        "--at",
        "--permission-mode",
        "--effort",
        "--add-dir",
        "--audit-actor",
        "--audit-reason",
        "--audit-request-id",
        "--audit-reclaimed-bytes",
        "--agent",
        "--tools",
        "--deny-tools",
        "--account",
        "--harness-arg",
    ];
    let mut normalized: Vec<String> = Vec::with_capacity(rest.len());
    let mut rest_iter = rest.iter();
    while let Some(tok) = rest_iter.next() {
        // Everything after a bare `--argv` is the provider command line, which
        // the `--argv` match arm below collects verbatim. Do NOT normalize
        // equals-form tokens in that payload -- a downstream tool's
        // `--timeout=5` must survive untouched (the prior per-token splitting
        // never reached the payload because `--argv` drained the iterator
        // first; the up-front pass would otherwise corrupt it). Copy the rest
        // verbatim and stop.
        if tok == "--argv" {
            normalized.push(tok.clone());
            normalized.extend(rest_iter.cloned());
            break;
        }
        // The bare `--` seed fence gets the same verbatim treatment: a
        // fenced `--timeout=5 do X` seed must reach the positional drain
        // as-is, not be split by the equals-form rewrite below.
        if tok == "--" {
            normalized.push(tok.clone());
            normalized.extend(rest_iter.cloned());
            break;
        }
        // the equals-form split is for LONG flags only. The short
        // value flags (-p/-c/-t) take a space-separated value (`-p claude`),
        // matching Click/Typer's short-option convention; the `-p=value` form is
        // intentionally not normalized here. The phone-motivating surface types
        // the space form, and shorts are additive aliases, not a new syntax.
        if tok.starts_with("--") {
            if let Some(eq) = tok.find('=') {
                if VALUE_FLAGS.contains(&&tok[..eq]) {
                    normalized.push(tok[..eq].to_string());
                    normalized.push(tok[eq + 1..].to_string());
                    continue;
                }
            }
        }
        normalized.push(tok.clone());
    }

    // on spawn the head's axis flags parse ONCE through the shared
    // SpawnAxes schema (the same parser the spawn-overlay verb runs), and the
    // axis tokens leave the loop's input below. The values seed the same
    // params and fields the old axis arms fed, after the loop.
    let axes = if verb == "spawn" {
        let (axes, fence) = SpawnAxes::scan(&normalized)?;
        normalized = SpawnAxes::strip_axes(&normalized, fence);
        Some(axes)
    } else {
        None
    };
    // three orthogonal axes. --harness/-H names the CLI binary,
    // --provider/-P the model VENDOR, --model the model at that vendor. The vendor
    // is held aside so a harness name typed there fails closed after the loop
    // (the historical confusion) rather than launching the wrong binary.
    // (Non-spawn verbs still collect --harness/--provider through the loop.)
    let mut harness_val: Option<String> = None;
    let mut vendor_val: Option<String> = None;
    let mut it = normalized.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            // --harness/-H is the CLI-binary axis, the --harness vocabulary the
            // rest of fno uses. -H no longer means headless (that is
            // --substrate headless / --headless / -p / --once now).
            "--harness" | "-H" => {
                harness_val = Some(it.next().ok_or("--harness needs a value")?);
            }
            // --provider/-P is the model-vendor axis. Capital P: -p is headless,
            // mirroring the harnesses' own one-shot short.
            "--provider" | "-P" => {
                vendor_val = Some(it.next().ok_or("--provider needs a value")?);
            }
            // Off `spawn`, -p was the provider short (the harness axis). That axis
            // is --harness/-H everywhere now, and -p/--headless is a spawn-only
            // one-shot, so -p is a loud tombstone here - never silently bound to
            // headless (the arm below) or to a harness. This arm must precede the
            // headless one, which also matches "-p".
            "-p" if verb != "spawn" => {
                return Err(format!(
                    "-p is not valid here; the one-shot short (--headless) is spawn-only, \
                     and the CLI binary is --harness/-H. \
                     (--provider/-p was split at the axis rename.)"
                ));
            }
            "--workspace" | "--squad" | "-s" => {
                params.insert("squad".into(), str_arg(&mut it, "-s/--workspace")?);
            }
            "--split" | "-x" => {
                params.insert("split".into(), str_arg(&mut it, "-x/--split")?);
            }
            // The portal placement trio, same spellings the mux
            // thread verb uses. Parsed here so the default Rust runtime
            // accepts what the help advertises; the placement itself runs
            // after the spawn receipt (place_thread_portal_after_spawn).
            "--portal" => {
                let raw = str_arg(&mut it, "--portal")?;
                match raw.as_str().and_then(|s| s.parse::<u8>().ok()) {
                    Some(n) => {
                        params.insert("portal".into(), Value::from(n));
                    }
                    None => return Err("--portal takes an index 0-255".into()),
                }
            }
            "--tab" => {
                params.insert("tab".into(), str_arg(&mut it, "--tab")?);
            }
            "--at" => {
                params.insert("at".into(), str_arg(&mut it, "--at")?);
            }
            "--from" => {
                // `promote <name> --from <session-uuid>`: the session to resume
                // interactively. Forwarded as `resume_id` (the daemon infers the
                // provider from the source row).
                params.insert("resume_id".into(), str_arg(&mut it, "--from")?);
            }
            "--cwd" | "-c" => {
                params.insert("cwd".into(), str_arg(&mut it, "--cwd")?);
            }
            "--message" => {
                params.insert("message".into(), str_arg(&mut it, "--message")?);
            }
            // the agent name rides a flag, so the single positional can be
            // the prompt. The seam normalizer mints one when the caller omits it,
            // so a spawn reaching here normally carries --name; the positional
            // fallback below keeps a direct `fno-agents spawn <name>` working.
            "--name" => {
                params.insert("name".into(), str_arg(&mut it, "--name")?);
            }
            "--node" => {
                params.insert("node".into(), str_arg(&mut it, "--node")?);
            }
            // The seam's node-reason receipt, set only when the seed
            // named a node the seam could not resolve. Forwarded so the mint
            // stamps why the row works no node; never read as a decision.
            "--node-reason" => {
                params.insert("node_reason".into(), str_arg(&mut it, "--node-reason")?);
            }
            "--session-id" => {
                params.insert("session_id".into(), str_arg(&mut it, "--session-id")?);
            }
            "--mode" => {
                // Disambiguates claude's two interactive-host lanes: `interactive`
                // (PTY pane, subscription-billed) vs the default stream-json adopt.
                // The daemon reads `mode`; codex/gemini ignore it. (`drive --mode`
                // is a different parser and never reaches build_request.)
                params.insert("mode".into(), str_arg(&mut it, "--mode")?);
            }
            "--status" => {
                params.insert("status".into(), str_arg(&mut it, "--status")?);
            }
            "--progress" => {
                params.insert("progress".into(), str_arg(&mut it, "--progress")?);
            }
            "--json" | "-J" => {
                // Task 3.1: --json is a client-side rendering flag. We recognize it
                // here so it is not rejected as "unknown flag". It is NOT forwarded
                // to the daemon as a param. The caller captures it separately.
                // -J is the global-register short for --json.
            }
            "--all" | "-A" => {
                params.insert("all".into(), Value::Bool(true));
            }
            "--discovered" | "--no-discovered" => {
                // client-side rendering flags for the `list`
                // discovered-live-sessions lane. Recognized here so they are not
                // rejected as unknown; captured separately at the call site and
                // never forwarded to the daemon.
            }
            "--force" | "-F" => {
                params.insert("force".into(), Value::Bool(true));
            }
            "--no-wait" => {
                // Spawn-gate escape: fail immediately at max_live
                // instead of queueing for a free slot. Client-side only.
                params.insert("no_wait".into(), Value::Bool(true));
            }
            "--model" | "-m" => {
                // Exact model name forwarded to the provider CLI's own --model:
                // claude --bg/-p, codex exec, gemini, agy (wired the
                // headless one-shots; claude --bg was). -m is the mobile
                // short. No fuzzy resolution.
                params.insert("model".into(), str_arg(&mut it, "-m/--model")?);
            }
            "--from-name" => {
                // NOTE: --from-name is accepted and forwarded to the daemon, but
                // the daemon's handle_ask currently ignores it (PTY path does not
                // apply the envelope wrapper yet). Accepted without error for
                // Python flag-parity; the daemon will wire it when the envelope
                // lands (Wave 5/6 follow-up).
                params.insert("from_name".into(), str_arg(&mut it, "--from-name")?);
            }
            "--yolo" | "-Y" => {
                // The daemon resolves yolo through resolve_thread_posture.
                params.insert("yolo".into(), Value::Bool(true));
            }
            // The Python spawn seam (rust_runtime
            // make_context -> inject_spawn_defaults) is the only reader of
            // config.agents.profiles. This token asserts it crossed upstream
            // and carries its enforcement verdict. Consumed here - never
            // forwarded, never read past the `--` fence - so no harness argv
            // or worker message can see it. Not in VALUE_FLAGS on purpose:
            // the bare token must not eat a neighbor as its value.
            "--defaults-applied" => {
                params.insert(
                    "defaults_applied".into(),
                    Value::String("unenforced".into()),
                );
            }
            other if other.starts_with("--defaults-applied=") => {
                // if/else, not a match: an inner `"word" =>` arm would read as
                // a phantom verb to the Python parity parser's arm scan.
                let v = &other["--defaults-applied=".len()..];
                if v != "enforced" && v != "unenforced" {
                    return Err("--defaults-applied takes 'enforced' or 'unenforced'".into());
                }
                params.insert("defaults_applied".into(), Value::String(v.into()));
            }
            "--permission-mode" => {
                // provider permission/approval mode. Parsed here so the
                // pane substrate (raw-arg re-exec to Python) is not blocked by an
                // unknown-flag error; bg/headless read it in maybe_run_spawn.
                // Mapping + fail-closed validation live at the spawn seam.
                params.insert(
                    "permission_mode".into(),
                    str_arg(&mut it, "--permission-mode")?,
                );
            }
            "--effort" => {
                params.insert("effort".into(), str_arg(&mut it, "--effort")?);
            }
            // Tier-3 harness-native passthrough. Parsed here (space +
            // equals form via VALUE_FLAGS) so the pane re-exec is not blocked by
            // an unknown-flag error; the mapping + fail-closed live at the spawn
            // seam (maybe_run_spawn) and the Python pane builder.
            "--add-dir" => {
                params.insert("add_dir".into(), str_arg(&mut it, "--add-dir")?);
            }
            "--agent" => {
                params.insert("agent".into(), str_arg(&mut it, "--agent")?);
            }
            "--tools" => {
                params.insert("tools".into(), str_arg(&mut it, "--tools")?);
            }
            "--deny-tools" => {
                params.insert("deny_tools".into(), str_arg(&mut it, "--deny-tools")?);
            }
            // The front door's carrier for a fenced `--` token a thread or
            // one-shot lane maps (codex: -c/--config/--add-dir). Repeatable;
            // the daemon-side harness_args parser owns the per-harness
            // vocabulary and refuses an unmapped token by name.
            "--harness-arg" => {
                let v = str_arg(&mut it, "--harness-arg")?;
                let items = params
                    .entry(String::from("harness_args"))
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(list) = items {
                    list.push(v);
                }
            }
            "--account" => {
                // per-spawn account selection. Parsed here so the spawn
                // arm is not blocked by an unknown-flag error; the four-lane
                // overlay resolution lives in Python (fno.agents.account_env), so
                // an account spawn re-execs the Python CLI on EVERY substrate (see
                // the spawn intercept) rather than duplicating the resolver here.
                params.insert("account".into(), str_arg(&mut it, "--account")?);
            }
            "--audit-actor" => {
                params.insert("audit_actor".into(), str_arg(&mut it, "--audit-actor")?);
            }
            "--audit-reason" => {
                params.insert("audit_reason".into(), str_arg(&mut it, "--audit-reason")?);
            }
            "--audit-request-id" => {
                params.insert(
                    "audit_request_id".into(),
                    str_arg(&mut it, "--audit-request-id")?,
                );
            }
            "--audit-reclaimed-bytes" => {
                let value = str_arg(&mut it, "--audit-reclaimed-bytes")?;
                let bytes = value
                    .as_str()
                    .and_then(|raw| raw.parse::<u64>().ok())
                    .ok_or("--audit-reclaimed-bytes needs a non-negative integer")?;
                params.insert("audit_reclaimed_bytes".into(), Value::from(bytes));
            }
            "--audit-worktree-touched" => {
                params.insert("audit_worktree_touched".into(), Value::Bool(true));
            }
            "--substrate" => {
                // The session-substrate selector: pane (owned-PTY,
                // default) | bg (claude --bg detached thread; opencode
                // serve-hosted session) |
                // headless (claude -p / codex --exec / agy -p one-shot). The
                // sole routing key the spawn arm reads (replaces --once).
                let v = str_arg(&mut it, "--substrate")?;
                match v.as_str() {
                    Some("pane") | Some("thread") | Some("headless") => {
                        params.insert("substrate".into(), v);
                    }
                    Some("bg") => {
                        eprintln!(
                            "warning: substrate value 'bg' is deprecated; use 'thread' instead; the alias will be removed after one release"
                        );
                        params.insert("substrate".into(), Value::String("thread".into()));
                    }
                    other => {
                        return Err(format!(
                            "--substrate must be one of: pane, thread, headless (bg is a deprecated alias; got {})",
                            other.unwrap_or("")
                        ));
                    }
                }
            }
            "--once" | "-o" => {
                // Back-compat alias: every live `--once` caller is a codex/gemini
                // one-shot, i.e. headless. Map it to --substrate headless so old
                // callers keep working without the conflated `once` boolean. An
                // explicit --substrate already present wins.
                params
                    .entry("substrate")
                    .or_insert_with(|| Value::String("headless".into()));
            }
            "--headless" | "-p" => {
                // Ergonomic front for --substrate headless. Same routing
                // key as --once; explicit --substrate already present wins. `-p`
                // mirrors the harnesses' own one-shot short; the vendor axis took
                // the capital -P so this letter could mean what it means in claude.
                params
                    .entry("substrate")
                    .or_insert_with(|| Value::String("headless".into()));
            }
            "--fresh" => {
                // Accepted no-op alias: the worker cwd already defaults
                // to the canonical repo root. Parsed for dispatcher compat.
                params.insert("fresh".into(), Value::Bool(true));
            }
            "--here" | "--in-place" => {
                // Explicit opt-in to the caller's cwd instead of the canonical
                // default: extend WIP right here.
                params.insert("here".into(), Value::Bool(true));
            }
            "--timeout" | "-t" => {
                let val = str_arg(&mut it, "--timeout")?;
                let secs: u64 = val
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| "--timeout needs a numeric value")?;
                params.insert("timeout".into(), Value::Number(secs.into()));
            }
            "--argv" => {
                // Everything after --argv is the provider command line. The
                // documented syntax is `--argv -- <cmd...>`; strip a single
                // leading `--` separator so the worker does not try to exec
                // the literal "--" as argv[0] (Codex P1).
                let mut rest: Vec<String> = it.by_ref().collect();
                if rest.first().map(|s| s == "--").unwrap_or(false) {
                    rest.remove(0);
                }
                argv = Some(rest);
            }
            "--" => {
                // End-of-options: everything after is positional (the seed
                // fence, same contract as the Python CLI's click parser).
                fence_at = Some(positional.len());
                for a in it.by_ref() {
                    positional.push(a);
                }
            }
            // The resume arm re-parses the original `rest` with
            // parse_conversion_args, which owns both flags; swallow them here
            // so the catch-all does not refuse them first.
            "--dry-run" | "--allow-new-id" if verb == "resume" => {}
            other if other.starts_with("--") => {
                return Err(format!("unknown flag: {other}"));
            }
            _ => positional.push(a),
        }
    }

    // On `spawn` the two flags are different axes: --harness is the CLI binary,
    // --provider the model vendor. Off spawn the vendor axis routes nothing, so
    // --provider/-P (the retired harness spelling AND the vendor short) is a
    // tombstone: exit 2 with the axis map, never silently forwarded.
    if let Some(v) = vendor_val {
        let v = v.trim().to_string();
        if verb == "spawn" {
            // A harness name on the vendor axis is refused BY NAME. This lane
            // never re-execs Python cmd_spawn, so without this a `--provider
            // claude` reaches the daemon as a vendor it cannot resolve.
            if KNOWN_PROVIDERS.contains(&v.as_str()) || v == "agy" || v == "opencode" {
                return Err(format!(
                    "{v} is a harness, not a provider; use --harness {v}"
                ));
            }
            // The vendor axis only means anything alongside a materialized route,
            // and routing lives in the Python spawn path (the front door keeps
            // every --provider spawn there). Reaching here means the binary was
            // driven directly: say what to run instead of failing downstream.
            return Err(format!(
                "--provider {v} names a model vendor; routing is applied by the fno \
                 CLI (`fno agents spawn ... --provider {v} --model <m>`), not by \
                 fno-agents directly"
            ));
        }
        return Err(format!(
            "--provider/-P was split at the axis rename: the CLI binary is --harness/-H; \
             a model vendor is only routable at spawn \
             (`fno agents spawn --provider {v} --model <m>`)."
        ));
    }
    if let Some(v) = harness_val {
        params.insert("provider".into(), Value::String(v));
    }
    if let Some(axes) = &axes {
        if let Some(v) = &axes.provider {
            let v = v.trim().to_string();
            if KNOWN_PROVIDERS.contains(&v.as_str()) || v == "agy" || v == "opencode" {
                return Err(format!(
                    "{v} is a harness, not a provider; use --harness {v}"
                ));
            }
            return Err(format!(
                "--provider {v} names a model vendor; routing is applied by the fno \
                 CLI (`fno agents spawn ... --provider {v} --model <m>`), not by \
                 fno-agents directly"
            ));
        }
        if let Some(h) = &axes.harness {
            params.insert("provider".into(), Value::String(h.clone()));
        }
        if let Some(m) = &axes.model {
            params.insert("model".into(), Value::String(m.clone()));
        }
        if let Some(e) = &axes.effort {
            params.insert("effort".into(), Value::String(e.clone()));
        }
        if let Some(a) = &axes.account {
            params.insert("account".into(), Value::String(a.clone()));
        }
        if let Some(sx) = &axes.substrate {
            // if/else, not a match: an inner `"word" =>` arm reads as a
            // phantom verb to the Python parity parser's arm scan.
            if sx == "pane" || sx == "thread" || sx == "headless" {
                params.insert("substrate".into(), Value::String(sx.clone()));
            } else if sx == "bg" {
                eprintln!(
                    "warning: substrate value 'bg' is deprecated; use 'thread' instead; the alias will be removed after one release"
                );
                params.insert("substrate".into(), Value::String("thread".into()));
            } else {
                return Err(format!(
                    "--substrate must be one of: pane, thread, headless (bg is a deprecated alias; got {sx})"
                ));
            }
        }
    }

    if let Some(av) = argv {
        params.insert(
            "argv".into(),
            Value::Array(av.into_iter().map(Value::String).collect()),
        );
    }

    let method = match verb {
        "spawn" => {
            // The client runs as a child of the spawning session, so its env
            // still carries the harness markers the daemon's scrubbed env
            // lost. Stamp the ambient parent edge onto the request so a
            // daemon mint reads the parent from HERE, never from its own
            // environment (node-provenance.md: capture is ambient).
            let (session, harness, cwd) = fno_agents::claims::ambient_parent_edge();
            if let Some(s) = session {
                params.insert("spawned_by_session".into(), Value::String(s));
            }
            if let Some(h) = harness {
                params.insert("spawned_by_harness".into(), Value::String(h));
            }
            if let Some(c) = cwd {
                params.insert("spawned_by_cwd".into(), Value::String(c));
            }
            // With --name the whole positional tail is the message; without it the
            // first positional is still the name (a direct `fno-agents spawn`
            // bypasses the seam normalizer that would have minted one).
            let msg_from = if params.contains_key("name") {
                0
            } else {
                let name = positional.first().ok_or("spawn needs a <name> or --name")?;
                params.insert("name".into(), Value::String(name.clone()));
                1
            };
            // A message already collected before the fence makes the fenced
            // tail provider passthrough (the same harness_args list
            // --harness-arg fills; the daemon-side vocabulary check stays the
            // trust boundary). Without a message before the fence the tail is
            // still the seed (the fenced `--timeout=5 do X` case).
            if let Some(pre_len) = fence_at {
                if pre_len > msg_from {
                    let tail = positional.split_off(pre_len);
                    let items = params
                        .entry(String::from("harness_args"))
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(list) = items {
                        list.extend(tail.into_iter().map(Value::String));
                    }
                }
            }
            if !params.contains_key("message") && positional.len() > msg_from {
                params.insert(
                    "message".into(),
                    Value::String(positional[msg_from..].join(" ")),
                );
            }
            // / spawn defaults to an owned interactive pane (the
            // `pane` substrate) for PTY-capable providers. Only `pane` gets the
            // interactive host_mode/mint; `bg` (claude --bg) and `headless`
            // (-p/--exec) are client-side one-shots that never touch the daemon
            // (byte-unchanged: no host_mode, no mint). An unknown provider keeps
            // today's behavior (the daemon's provider_for_pty errors as before).
            let substrate = params
                .get("substrate")
                .and_then(Value::as_str)
                .unwrap_or("pane");
            let pty_capable = params
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|p| KNOWN_PROVIDERS.contains(&p));
            if substrate == "pane" && pty_capable {
                apply_interactive_defaults(&mut params);
            }
            fno_agents::spawn_context::refuse_inherited_tier_remap(&params)?;
            fno_agents::spawn_context::stamp_spawn_lineage(&mut params)?;
            "agent.spawn"
        }
        "ask" => {
            let name = positional.first().ok_or("ask needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            if !params.contains_key("message") && positional.len() > 1 {
                params.insert("message".into(), Value::String(positional[1..].join(" ")));
            }
            "agent.ask"
        }
        // `host`/`promote` (interactive daemon PTY hosting) were retired at G4
        // and intercepted with a mux pointer before build_request; they
        // never reach this match.
        "list" => "agent.list",
        "status" => "agent.status",
        "stop" => {
            let name = positional.first().ok_or("stop needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            "agent.stop"
        }
        "rm" => {
            let name = positional.first().ok_or("rm needs a <name>")?;
            params.insert("name".into(), Value::String(name.clone()));
            "agent.rm"
        }
        "rename" => {
            fno_agents::rename::request(&mut params, &positional)?;
            "agent.rename"
        }
        // Only a `--substrate` resume reaches here; a bare resume is the
        // client-side re-entry, intercepted before build_request. The argv
        // is re-parsed through the resume parser rather than read off
        // `params`, so the one refusal vocabulary answers both doors.
        "resume" => {
            let parsed = fno_agents::resume_args::parse_conversion_args(rest)?;
            params.insert("name".into(), Value::String(parsed.name));
            params.insert("dry_run".into(), Value::Bool(parsed.dry_run));
            params.insert("allow_new_id".into(), Value::Bool(parsed.allow_new_id));
            "agent.convert"
        }
        "reconcile" => "agent.reconcile",
        other => {
            return Err(format!(
                "unknown verb: {other} (expected {})",
                ALL_CLIENT_ACTIONS.join("|")
            ))
        }
    };

    Ok((method.to_string(), Value::Object(params)))
}

/// Stamp the caller's working directory into daemon-bound spawn/ask requests.
///
/// The `fno-agents` daemon is a single long-lived process shared across every
/// project, so its own `std::env::current_dir()` is frozen to wherever it was
/// first lazy-started; it cannot stand in for "the directory the user ran the
/// command from". Only the client sits in the user's directory, so the client
/// must forward `cwd`; otherwise a worker spawned from project A lands in the
/// daemon's home project B (e.g. `fno agents host` opening codex in the wrong
/// repo). An explicit `--cwd` already in `params` always wins.
///
/// `agent.spawn` covers `spawn`/`host`/`promote`; `agent.ask` covers gemini's
/// first-contact auto-spawn (claude/codex `ask` resolve cwd client-side before
/// reaching this send path, so they never depend on it).
fn ensure_request_cwd(method: &str, params: &mut Value, cwd: &std::path::Path) {
    if method != "agent.spawn" && method != "agent.ask" {
        return;
    }
    // build_request always returns Value::Object for these methods; assert it
    // so a future caller passing a non-object is caught in debug rather than
    // silently skipping the cwd stamp.
    debug_assert!(params.is_object(), "spawn/ask params must be a JSON object");
    if let Some(obj) = params.as_object_mut() {
        if !obj.contains_key("cwd") {
            obj.insert(
                "cwd".to_string(),
                Value::String(cwd.to_string_lossy().into_owned()),
            );
        }
    }
}

/// Canonicalize a `--cwd` string to an absolute path, matching Python's
/// `Path(cwd).resolve()`: prefer `std::fs::canonicalize`, falling back to a
/// join against the caller cwd for a relative path that does not exist yet.
/// Extracted from the previously-duplicated claude-ask / spawn cwd blocks.
fn canonicalize_cwd(c: &str) -> std::path::PathBuf {
    std::fs::canonicalize(c).unwrap_or_else(|_| {
        let p = std::path::PathBuf::from(c);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
        }
    })
}

/// Read the `fresh` / `here` booleans a caller set via `--fresh` /
/// `--here`(`--in-place`). Both default to false: `--fresh` is an opt-in
/// mechanism, never on by default at the client layer (the policy layer decides
/// when to pass it -- AC3 keeps non-target verbs on caller cwd unless asked).
fn fresh_here_flags(params: &Value) -> (bool, bool) {
    let fresh = params
        .get("fresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let here = params
        .get("here")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (fresh, here)
}

/// The spawn-control flags one gate evaluation honors: the `--force` and
/// `--no-wait` CLI flags land in the spawn params as booleans. Both gate
/// constructions (the daemon-bound codex-thread gate and the shared one)
/// read through this, so neither can drop a flag.
fn gate_flags_from_params(params: &Value) -> fno_agents::spawn_gate::GateFlags {
    fno_agents::spawn_gate::GateFlags {
        force: params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        no_wait: params
            .get("no_wait")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Pure cwd precedence for a spawn/ask dispatch: explicit `--cwd` > `--here`
/// (caller) > default canonical. inverted the default: with no explicit
/// cwd source the worker lands on the canonical root, so the identical command
/// behaves the same regardless of where the launcher stands; `--here` is the
/// explicit opt-in to keep the caller's cwd. `--fresh` is an accepted no-op
/// alias (the default already resolves canonical). An unresolved canonical
/// (None) falls back to the caller cwd, the safe side. No git / env / IO, so the
/// precedence is unit-testable (Failure Modes > Invariants: `--cwd` is the
/// highest-priority cwd source and wins over everything).
fn effective_worker_cwd(
    explicit_cwd: Option<std::path::PathBuf>,
    _fresh: bool,
    here: bool,
    canonical: Option<std::path::PathBuf>,
    caller: std::path::PathBuf,
) -> std::path::PathBuf {
    if let Some(c) = explicit_cwd {
        return c; // explicit --cwd always wins
    }
    if here {
        return caller; // --here: explicit opt-in to the caller's cwd
    }
    canonical.unwrap_or(caller) // default: canonical; caller on resolution failure
}

/// One-line stderr note when the default (or `--fresh` alias) actually moves the
/// worker cwd off the caller's dir, so the redirect is never silent on any path,
/// default included (Locked Decision 5; Failure Modes > Errors).
fn note_fresh_redirect(caller: &std::path::Path, chosen: &std::path::Path) {
    if chosen != caller {
        eprintln!(
            "fno-agents: dispatching from canonical main (default) ({}); pass --here to stay in this worktree",
            chosen.display()
        );
    }
}

/// Resolve the worker cwd for a client-side (claude/codex) spawn/ask dispatch,
/// honoring `--cwd` > `--here` (caller) > default canonical. Shells to git only
/// on the default path (no `--cwd`, no `--here`); emits the redirect note on an
/// actual move. Returns `(cwd, moved)` where `moved` is exactly the note
/// condition, so a caller surfacing `cwd` in a receipt couples to the note with
/// no second, divergent comparison (; gemini review). Single source of cwd
/// truth for the two client-side dispatch blocks (claude `ask`, claude `spawn`).
fn resolve_dispatch_cwd(params: &Value) -> (std::path::PathBuf, bool) {
    let caller = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let explicit = params
        .get("cwd")
        .and_then(|v| v.as_str())
        // An empty --cwd is absent, never the empty-string path (Failure Modes >
        // Boundaries; Python's `if cwd:` twin). Without this, canonicalize_cwd("")
        // resolves to the caller dir and suppresses the canonical default -- the
        // exact worktree leak this change prevents (review).
        .filter(|s| !s.is_empty())
        .map(canonicalize_cwd);
    let (fresh, here) = fresh_here_flags(params);
    // Default path (no explicit --cwd, no --here) resolves canonical; --fresh is
    // now a no-op alias since canonical IS the default.
    let default_path = explicit.is_none() && !here;
    let canonical = if default_path {
        fno_agents::paths::canonical_repo_root(&caller)
    } else {
        None
    };
    let chosen = effective_worker_cwd(explicit.clone(), fresh, here, canonical, caller.clone());
    let moved = default_path && chosen != caller;
    if moved {
        note_fresh_redirect(&caller, &chosen);
    }
    (chosen, moved)
}

fn str_arg(
    it: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<Value, String> {
    it.next()
        .map(Value::String)
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// The stderr line a transport-level `rm` failure prints. The pre-exec removal
/// banner is past tense and the transport error alone reads like a completed
/// removal, so the line names the row and answers from the registry: a dead
/// connection does not prove the daemon skipped the write, and the store is
/// the receipt. `None` means the store itself could not be read, and the line
/// says so rather than wearing an absent verdict.
fn rm_failure_line(name: &str, err: &str, still_registered: Option<bool>) -> String {
    let verdict = match still_registered {
        Some(true) => "nothing was removed - the row is still registered",
        Some(false) => "the row is no longer registered",
        None => "the registry could not be read to confirm whether anything was removed",
    };
    format!("fno-agents: rm {name}: {err}; {verdict}")
}

/// Format a successful daemon response for human-readable stdout.
///
/// Returns `Some(line)` for verbs with a defined output contract, `None` for
/// verbs that still use the generic `serde_json::to_string_pretty` fallback.
///
/// - `stop`: prints `stopped: <name> (<short_id>)` using the `short_id` the
///   daemon now includes in every stop success payload. Falls back to
///   `stopped: <name>` when `short_id` is absent (e.g. an old daemon).
/// - `rm`: names each surface the daemon proved removed or unverified.
/// - `list`: Task 3.1 — JSON when `json_flag` or not a TTY; table otherwise.
/// - `reconcile`: Task 3.1 — JSON when `json_flag` or not a TTY; human summary otherwise.
fn format_success(
    verb: &str,
    name: &str,
    result: &Value,
    json_flag: bool,
    is_tty: bool,
    discover: bool,
) -> Option<String> {
    match verb {
        "ask" => {
            // Create path (first contact): daemon returns {created: true, short_id: "..."}.
            // Python prints exactly `<short_id>\n` (no banner).
            if result
                .get("created")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let short_id = result
                    .get("short_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Some(short_id.to_string())
            } else {
                // Follow-up path: print the reply verbatim (no added newline; println!
                // in the caller adds the newline, matching Python's behaviour).
                // A `reply: null` with `status: "in_flight"` is the codex
                // thread actor's bounded-ask receipt: the turn is still
                // driving, so say THAT instead of printing an empty line that
                // reads as an empty answer.
                if result.get("reply").is_none_or(Value::is_null)
                    && result.get("status").and_then(|v| v.as_str()) == Some("in_flight")
                {
                    let turn_id = result
                        .get("turn_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    return Some(format!(
                        "in flight: turn {turn_id} is still driving; the reply surfaces via \
                         agent_ask_done"
                    ));
                }
                let reply = result.get("reply").and_then(|v| v.as_str()).unwrap_or("");
                Some(reply.to_string())
            }
        }
        "stop" => {
            let outcome = result
                .get("interrupt")
                .and_then(|v| v.as_str())
                .filter(|outcome| *outcome != "no-turn");
            // The daemon REFUSED the stop: its interrupt never settled and the
            // turn is still driving in the worker's worktree. Saying "stopped"
            // here is the report-it-did-not-perform shape the daemon arm exists
            // to prevent, so the word never appears on this path.
            if result.get("stopped").and_then(Value::as_bool) == Some(false) {
                return Some(format!(
                    "stop refused: {name} is still running ({})",
                    outcome.unwrap_or("the interrupt never settled")
                ));
            }
            let mut line = match result.get("short_id").and_then(|v| v.as_str()) {
                Some(short_id) => format!("stopped: {name} ({short_id})"),
                None => format!("stopped: {name}"),
            };
            // A codex thread stop names what happened to the in-flight turn:
            // a bare "stopped" over an interrupt the daemon never confirmed is
            // the exact report-it-did-not-perform shape this field exists to
            // prevent. `no-turn` (nothing was driving) stays silent.
            if let Some(outcome) = outcome {
                line.push_str(&format!(" (turn {outcome})"));
            }
            line.push_str(&fno_agents::rm_receipt::stop_receipt_suffix(result));
            Some(line)
        }
        "rm" => fno_agents::rm_receipt::receipt(name, result),
        "rename" => fno_agents::rename::receipt(name, result),
        "list" => {
            let agents = &result["agents"];
            let fields_omitted = &result["fields_omitted"];
            let filters = result.get("filters_applied").cloned().unwrap_or_else(
                || json!({"cwd": null, "provider": null, "status": null, "progress": null}),
            );
            // merge the P1 host-local live-session lane. The Rust
            // client owns the rendered surface, so it shells out to the Python
            // helper (which has psutil's cross-platform reuse-safe liveness) and
            // folds the result in. Fail-open: an empty lane on any error.
            let discovered = if discover {
                fetch_discovered_sessions(
                    filters.get("cwd").and_then(|v| v.as_str()),
                    filters.get("provider").and_then(|v| v.as_str()),
                    filters.get("status").and_then(|v| v.as_str()),
                    filters.get("progress").and_then(|v| v.as_str()),
                )
            } else {
                Vec::new()
            };
            if json_flag || !is_tty {
                Some(render_list_json(
                    agents,
                    &filters,
                    fields_omitted,
                    &discovered,
                    result["truth_probe_asked"].as_u64(),
                    result["truth_probe_answered"].as_u64(),
                    result.get("codex_loaded"),
                ))
            } else {
                Some(render_list_table(
                    agents,
                    &discovered,
                    result["truth_probe_asked"].as_u64(),
                    result["truth_probe_answered"].as_u64(),
                ))
            }
        }
        "reconcile" => {
            if json_flag || !is_tty {
                Some(render_reconcile_json(result))
            } else {
                Some(render_reconcile_human(result))
            }
        }
        "spawn" => {
            // PTY-provider spawns now route through the daemon (owned
            // interactive pane) instead of the client-side claude `--bg` lane.
            // Emit the SAME compact single-line JSON receipt that lane produces
            // ({"name","short_id","harness","status"}). The harness axis is
            // reported under `harness`, never under a `provider` key (a provider
            // key holding a harness literal is the axis defect). The in-repo
            // receipt parsers (skills/target/scripts/dispatch-node.sh and
            // backlog/advance.py) read only `short_id`, so the rename is safe.
            // serde_json::to_string (NOT _pretty) keeps it one line for the
            // line-by-line `json.loads` consumers. `--once` spawns are handled
            // client-side and never reach here.
            let short_id = result
                .get("short_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let harness = result.get("harness").and_then(|v| v.as_str()).unwrap_or("");
            let status = result
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("live");
            let session_id = result
                .get("harness_session_id")
                .or_else(|| result.get("session_id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            let mut receipt = json!({
                "name": name,
                "short_id": short_id,
                "harness": harness,
                "status": status,
            });
            if let Some(session_id) = session_id {
                receipt["session_id"] = json!(session_id);
                if result.get("harness_session_id").is_some() {
                    receipt["harness_session_id"] = json!(session_id);
                }
            }
            Some(serde_json::to_string(&receipt).unwrap_or_default())
        }
        _ => None,
    }
}

/// Render agents list as Python-matching JSON (Task 3.1; discovered lane
///; provider key restored).
///
/// Shape (schema_version 6): `{"agents": [...], "count": N,
/// "discovered_sessions": [...], "discovered_count": M, "fields_omitted":
/// [...], "filters_applied": {...}, "schema_version": 6}`. Stays
/// byte-shape-aligned with Python's `format.render_json`.
const LIST_JSON_SCHEMA_VERSION: u32 = 7;

fn render_list_json(
    agents: &Value,
    filters_applied: &Value,
    fields_omitted: &Value,
    discovered: &[Value],
    truth_probe_asked: Option<u64>,
    truth_probe_answered: Option<u64>,
    codex_loaded: Option<&Value>,
) -> String {
    let count = agents.as_array().map(|a| a.len()).unwrap_or(0);
    // `codex_loaded` is additive and present only when the caller probed the
    // codex daemon (`--harness codex`); the key is omitted otherwise.
    let mut payload = json!({
        "agents": agents,
        "count": count,
        "discovered_sessions": discovered,
        "discovered_count": discovered.len(),
        "fields_omitted": fields_omitted,
        "filters_applied": filters_applied,
        "truth_probe_asked": truth_probe_asked,
        "truth_probe_answered": truth_probe_answered,
        "schema_version": LIST_JSON_SCHEMA_VERSION,
    });
    if let Some(block) = codex_loaded {
        payload["codex_loaded"] = block.clone();
    }
    serde_json::to_string_pretty(&payload).unwrap_or_default()
}

/// The `; released N claim(s); kept <key> (<observed>)` suffix a stop/rm line
/// carries when the daemon released or kept claims for the stopped worker
/// (change 5d). Empty when the receipt names neither, so a stop that
/// released nothing renders byte-identical to today.

/// Shell out to the Python `fno agents discovered-json` helper for the P1
/// discovered-live-sessions lane and return the rows.
///
/// The Rust client owns the `list` rendered surface, but discovery lives in
/// Python (it needs psutil's cross-platform process create-time for the
/// reuse-safe liveness the design requires; the Rust-native liveness degrades
/// to existence-only on macOS). Fail-open by contract: a missing `fno`, a
/// non-zero exit, or unparseable output yields an empty lane so `list` is
/// never broken by discovery (US5). `FNO_AGENTS_RUNTIME=python` pins the child
/// to the Python dispatch so it cannot recurse back into this binary.
fn fetch_discovered_sessions(
    cwd_filter: Option<&str>,
    provider_filter: Option<&str>,
    status_filter: Option<&str>,
    progress_filter: Option<&str>,
) -> Vec<Value> {
    use std::process::Command;

    // No live-only early return. It encoded "a discovered session is live by
    // definition", which the shared reachability verdict retired: a discovered
    // row whose process is provably gone now comes back `orphaned`, so the
    // early return made `--status orphaned` drop through Rust a row that Python
    // prints -- one runtime-dependent answer to one question. The row-level
    // filter at the bottom is the single place status is applied.
    let mut argv = vec!["agents".to_string(), "discovered-json".to_string()];
    if let Some(c) = cwd_filter {
        argv.push("--cwd".into());
        argv.push(c.into());
    }
    // Without this the rendered surface disagrees with the Python one:
    // `--harness claude` would list every discovered codex/opencode session.
    // An empty value is "no filter" on the Python side, so forwarding it would
    // make the two runtimes disagree again in the other direction.
    if let Some(p) = provider_filter.filter(|p| !p.is_empty()) {
        argv.push("--harness".into());
        argv.push(p.into());
    }

    // Every `fno agents list` on the box runs this, so several terminals asking
    // at once each paid their own Python cold start for one answer. The latch
    // is keyed on the argv above, so a run with a different `--cwd` or
    // `--harness` is a different flight and keeps its own answer.
    let key = fno_agents::single_flight::flight_key(&argv);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let flight = fno_agents::single_flight::run_or_join(
        &key,
        fno_agents::agents_config::single_flight_ttl(&cwd),
        fno_agents::agents_config::single_flight_join_budget(&cwd),
        // No outer deadline to subtract from: this path has no caller-supplied
        // budget, so the wait it may have spent changes nothing about the run.
        |_spent| {
            let mut cmd = Command::new(fno_agents::scrape::fno_bin());
            cmd.args(&argv);
            cmd.env("FNO_AGENTS_RUNTIME", "python");
            // Fail-open by contract, and the same rule the latch needs: only a
            // clean run is worth sharing, so a failure spends no cache entry
            // and the next caller retries for real.
            match cmd.output() {
                Ok(o) if o.status.success() => Some(o.stdout),
                _ => None,
            }
        },
    );
    let output = match flight.stdout {
        Some(bytes) => bytes,
        None => return Vec::new(),
    };
    let parsed: Value = match serde_json::from_slice(&output) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut rows: Vec<Value> = parsed
        .get("discovered_sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    retain_discovered_by_status(&mut rows, status_filter);
    retain_discovered_by_progress(&mut rows, progress_filter);
    rows
}

/// Keep only the discovered rows whose OWN verdict matches the requested one.
///
/// Split out so it is assertable without the shellout above. The test that
/// used to cover this pinned the early return (`a non-live filter yields
/// nothing`), and once that return was gone the same assertion still passed --
/// vacuously, because the subprocess answers with nothing under test. An
/// assertion that survives the behavior it describes is not covering it.
fn retain_discovered_by_status(rows: &mut Vec<Value>, status_filter: Option<&str>) {
    // A discovered session whose process is provably gone now comes back
    // `orphaned` (the shared reachability verdict), so a live-only run that
    // trusted the CALLER's filter alone would print an orphaned row under a
    // banner that says LIVE.
    if let Some(want) = status_filter {
        rows.retain(|r| r.get("status").and_then(Value::as_str) == Some(want));
    }
}

fn retain_discovered_by_progress(rows: &mut Vec<Value>, progress_filter: Option<&str>) {
    if let Some(want) = progress_filter {
        rows.retain(|r| r.get("progress").and_then(Value::as_str) == Some(want));
    }
}

/// Compact single-unit age for the CHECKED column: the largest whole unit of
/// the elapsed seconds -- `3s`, `4m`, `18h`, `2d` (plan, AC2-EDGE).
/// Negative input (a row reconciled in the "future" via clock skew) clamps to
/// `0s` rather than rendering a misleading negative age.
fn format_age_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Render `last_reconciled_at` (raw RFC3339, or None) as the CHECKED cell:
/// `never` when never probed, the compact age otherwise, or `?` when the stored
/// timestamp cannot be parsed (explicit, never blank -- Silent-Failure check).
fn render_checked(last_reconciled_at: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> String {
    match last_reconciled_at {
        None => "never".to_string(),
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(then) => format_age_secs((now - then.with_timezone(&chrono::Utc)).num_seconds()),
            Err(_) => "?".to_string(),
        },
    }
}

/// Display cap for the LAST MESSAGE cell, kept in step with Python's
/// `_LAST_MESSAGE_WIDTH` in cli/src/fno/agents/format.py (the two tables are
/// functional parallels, not byte-exact, but the cap is the one value worth
/// holding together).
const LAST_MESSAGE_WIDTH: usize = 40;

/// Right-aligned ellipsis truncation, chars not bytes (mirrors Python's
/// `_truncate`), so a long transcript line cannot own the table.
fn truncate_cell(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        s.chars().take(width).collect()
    } else {
        let mut t: String = s.chars().take(width - 1).collect();
        t.push('…');
        t
    }
}

/// Render agents list as a human-readable table (Task 3.1; CHECKED/PID added by
/// plan, Architecture C).
///
/// Columns: NAME HARNESS STATUS CHECKED PID EVENT AGE LAST MESSAGE CWD. CHECKED
/// is the relative age since the last reconcile probe (`never` when unprobed);
/// it replaces the old always-`-` LIVE column (AC5-UI). PID is the worker pid
/// for a PTY agent (`-` for a one-shot ask, which has no managed process).
/// EVENT AGE is the relative age of the transcript's newest activity and LAST
/// MESSAGE the flattened last-turn text - beside the state column on
/// purpose, so a row claiming to be busy while its transcript is hours old
/// shows the disagreement instead of hiding it. This is a functional table;
/// byte-exact match with Python is not required (Python's table is
/// time-dependent via relative timestamps).
fn render_list_table(
    agents: &Value,
    discovered: &[Value],
    truth_probe_asked: Option<u64>,
    truth_probe_answered: Option<u64>,
) -> String {
    // HARNESS, not PROVIDER: the column has always shown the harness, and the
    // old heading made a claude-hosted worker on a zai route read as running
    // on claude. Same rename on the Python renderer.
    // ADDRESS sits second, mirroring the Python renderer. `list` auto-routes
    // here whenever an installed binary is present, so a column added only to
    // the Python table would be missing from the surface nearly every reader
    // sees -- and the whole point of the column is that a reader with no
    // address copies NAME, whose durable write queues under a key no drain
    // reads. The value is read off the row (both projections emit `address`),
    // never re-derived, so the two tables cannot disagree.
    let headers = [
        "NAME",
        "ADDRESS",
        "HARNESS",
        "STATUS",
        "CHECKED",
        "PID",
        "EVENT AGE",
        "LAST MESSAGE",
        "CWD",
    ];
    let empty_arr = vec![];
    let rows = agents.as_array().unwrap_or(&empty_arr);
    let now = chrono::Utc::now();

    // Compute display values for each row
    let display: Vec<[String; 9]> = rows
        .iter()
        .map(|r| {
            let name = r["name"].as_str().unwrap_or("-").to_string();
            let address = r["address"].as_str().unwrap_or("-").to_string();
            let harness = r["harness"].as_str().unwrap_or("-").to_string();
            let status = r["status"].as_str().unwrap_or("-").to_string();
            let checked = render_checked(r["last_reconciled_at"].as_str(), now);
            let pid = r["pid"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            // `unknown`, not `never`: no transcript was READ, which is an
            // absent reading, not a claim that no event ever happened.
            let event_age = match r["last_event_at"].as_str() {
                Some(ts) => render_checked(Some(ts), now),
                None => "unknown".to_string(),
            };
            // The transcript's LAST turn, not the registry timestamp this
            // column was wired to for its whole life: that field is null on
            // many rows while the worker is mid-sentence, so a "last message"
            // column that never showed a message. Capped so one long line
            // cannot blow out CWD.
            let last_msg = r["last_message"]
                .as_str()
                .map(|s| truncate_cell(s, LAST_MESSAGE_WIDTH))
                .unwrap_or_else(|| "-".to_string());
            let cwd = r["cwd"].as_str().unwrap_or("-").to_string();
            [
                name, address, harness, status, checked, pid, event_age, last_msg, cwd,
            ]
        })
        .collect();

    // Column widths: max of header and data
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
        headers[5].len(),
        headers[6].len(),
        headers[7].len(),
        headers[8].len(),
    ];
    for row in &display {
        for (i, cell) in row.iter().enumerate() {
            // Chars, not bytes: the `{:<width$}` pad below counts chars, so a
            // byte width on non-ASCII text (a CJK cwd, an emoji message) pads
            // past the intended column and shoves the rest of the row wide.
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut lines = Vec::new();
    // The instrument's receipt, in the artifact itself: a total
    // outage must not read as a wall of `unknown` statuses the reader was
    // meant to trust. The daemon's stderr WARN is write-only; this line is
    // the one the operator actually sees.
    if truth_probe_asked.unwrap_or(0) > 0 && truth_probe_answered == Some(0) {
        lines.push(format!(
            "truth probe failed: 0 of {} rows answered; every STATUS below is unmeasured, not healthy",
            truth_probe_asked.unwrap_or(0)
        ));
    }
    // Header row
    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:width$}", h, width = widths[i]))
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(header_line.trim_end().to_string());
    // Data rows
    for row in &display {
        let data_line = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(data_line.trim_end().to_string());
    }
    let mut out = lines.join("\n") + "\n";
    if !discovered.is_empty() {
        out.push_str(&render_discovered_section(discovered));
    }
    out
}

/// Render the host-local discovered-live-sessions lane below the registry
/// table (AC1-UI). A blank line + banner make it visually
/// distinct. Columns: ADDRESS (the mailbox) LABEL (friendly alias) STATUS
/// PROJECT CWD.
///
/// ADDRESS leads and the alias is demoted to LABEL, matching the Python
/// renderer. The alias led this table for its whole life, which made it the
/// leftmost thing a reader copied, and `<project>-<short8>` is not an address.
/// The value is read off the row rather than derived here: `to_row` resolves it
/// from the session's own harness, so this renderer and the Python one cannot
/// answer differently about the same session.
fn render_discovered_section(discovered: &[Value]) -> String {
    let headers = ["ADDRESS", "LABEL", "STATUS", "PROJECT", "CWD"];
    let display: Vec<[String; 5]> = discovered
        .iter()
        .map(|r| {
            let address = r["address"].as_str().unwrap_or("-").to_string();
            let label = r["handle"].as_str().unwrap_or("-").to_string();
            let status = r["status"].as_str().unwrap_or("-").to_string();
            let project = r["project"].as_str().unwrap_or("-").to_string();
            let cwd = r["cwd"].as_str().unwrap_or("-").to_string();
            [address, label, status, project, cwd]
        })
        .collect();

    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
        headers[4].len(),
    ];
    for row in &display {
        for (i, cell) in row.iter().enumerate() {
            // Chars, not bytes: the `{:<width$}` pad below counts chars, so a
            // byte width on non-ASCII text (a CJK cwd, an emoji message) pads
            // past the intended column and shoves the rest of the row wide.
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut lines = Vec::new();
    lines.push(String::new()); // blank separator line
    lines.push(format!(
        "DISCOVERED LIVE SESSIONS ({}, host-local)",
        display.len()
    ));
    lines.push(
        headers
            .iter()
            .enumerate()
            .map(|(i, h)| format!("{:width$}", h, width = widths[i]))
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end()
            .to_string(),
    );
    for row in &display {
        lines.push(
            row.iter()
                .enumerate()
                .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end()
                .to_string(),
        );
    }
    lines.join("\n") + "\n"
}

/// Render reconcile result as Python-matching JSON (Task 3.1).
///
/// Shape: `{"scanned": N, "orphaned": [...], "recovered": [...], "skipped": [...], "errors": [...]}`
/// Matches Python cmd_reconcile's JSON payload exactly.
fn render_reconcile_json(result: &Value) -> String {
    // The daemon now returns scanned/orphaned/recovered/skipped/errors directly.
    let payload = json!({
        "scanned": result.get("scanned").cloned().unwrap_or(Value::Null),
        "orphaned": result.get("orphaned").cloned().unwrap_or_else(|| json!([])),
        "recovered": result.get("recovered").cloned().unwrap_or_else(|| json!([])),
        "skipped": result.get("skipped").cloned().unwrap_or_else(|| json!([])),
        "errors": result.get("errors").cloned().unwrap_or_else(|| json!([])),
    });
    serde_json::to_string(&payload).unwrap_or_default() + "\n"
}

/// Render reconcile result as human-readable summary (Task 3.1).
fn render_reconcile_human(result: &Value) -> String {
    let scanned = result["scanned"].as_u64().unwrap_or(0);
    let orphaned = result["orphaned"].as_array().map(|a| a.len()).unwrap_or(0);
    let recovered = result["recovered"].as_array().map(|a| a.len()).unwrap_or(0);
    let skipped = result["skipped"].as_array().map(|a| a.len()).unwrap_or(0);
    let errors = result["errors"].as_array().map(|a| a.len()).unwrap_or(0);
    format!(
        "scanned: {scanned}  orphaned: {orphaned}  recovered: {recovered}  skipped: {skipped}  errors: {errors}\n"
    )
}

/// Map a daemon error code to the design's verb exit codes.
fn exit_code_for(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::AgentNotFound | ErrorCode::AgentExists | ErrorCode::InvalidStatus => 13,
        ErrorCode::SpawnFailed => 14,
        ErrorCode::LockTimeout => 15,
        ErrorCode::Busy => 18,
        ErrorCode::InvalidParams | ErrorCode::MalformedFrame | ErrorCode::UnknownMethod => 2,
        ErrorCode::ChannelUnknown => 13,
        ErrorCode::Internal => 1,
        // Distinct from Internal on purpose: a caller racing daemon teardown
        // can retry, where a real Internal fault should not be retried blind.
        ErrorCode::ShuttingDown => 19,
        // Also distinct, and for the opposite reason: two fno builds disagree
        // about the registry schema, so retrying changes nothing. The repair is
        // a deploy or a redirect. A caller that folded this into Internal would
        // report a daemon fault, which is how the 2026-08-28 outage was
        // misdiagnosed twice.
        ErrorCode::SchemaMismatch => 20,
    }
}

fn warns_on_daemon_drift(verb: &str) -> bool {
    matches!(verb, "list" | "rm")
}

/// True when `--help`/`-h` appears in the verb's OWN options, i.e. before an
/// `--argv`/`--` payload boundary. A `--help` after that boundary belongs to a
/// spawned command's argv (e.g. `spawn wk --harness codex --argv -- tool
/// --help`) and must not be captured as our per-verb help request
/// (review: gemini HIGH / codex P2).
fn is_help_request(opts: &[String]) -> bool {
    let boundary = opts
        .iter()
        .position(|a| a == "--" || a == "--argv")
        .unwrap_or(opts.len());
    opts[..boundary].iter().any(|a| a == "--help" || a == "-h")
}

fn print_help() {
    println!(
        "{}",
        json!({
            "binary": "fno-agents",
            "verbs": CLIENT_VERB_USAGE,
        })
    );
}

#[cfg(test)]
#[path = "../client_tests.rs"]
mod tests;
