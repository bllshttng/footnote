//! `fno-agents backlog <group> <action> ...` - one grouped backlog
//! dispatcher, the fold of the four direct actions (`backlog-update`,
//! `backlog-note`, `backlog-notes`, `graph-get`) into the canonical catalog.
//!
//! Transport-only, like `sync-canonical`: it registers no client action in
//! `ALL_CLIENT_ACTIONS` (the shrink law allows no new one), and the `fno`
//! front door execs this binary with `backlog` as the first token, so the
//! public `fno backlog` namespace lands here natively. The catalog in
//! [`super::commands`] owns every accepted spelling: grouped
//! (`backlog add node ...`) and legacy (`backlog add ...`) both resolve to
//! the legacy command the actions historically ran under.
//!
//! Routing has two tiers. A native tier serves the actions whose engine
//! owns the complete behavior of the invoked shape; the folded engines keep
//! their exact argv contracts, so internal callers (the Python bridges, the
//! mux snapshot read) move by swapping the first tokens. Every other
//! invocation rides the compatibility forward to the wheel's Python CLI,
//! unchanged, until its port lands with the rest of the epic.

use super::commands::{self, GROUPS};

/// The engine doors' argv shape: every native-engine caller (the Python
/// bridges, the note passthrough, the status fanout) passes `--graph` as
/// the FIRST tail token, and the public full-command surface never leads
/// with it - `note`'s extra-tolerant parser would otherwise swallow a stray
/// engine spelling as an ignored extra, so the first token is the one
/// discriminator that cannot collide with a user invocation.
const ENGINE_DOOR_HEAD: &str = "--graph";

/// One resolved invocation: the legacy command the catalog maps it to (a
/// `'static` catalog name), and the untouched tail after the command word.
struct Resolved<'a> {
    legacy: &'static str,
    tail: &'a [String],
}

/// Native auxiliary vocabulary: commands the dispatcher routes by engine
/// contract that the Python catalog never carried (the note corpus/history
/// reader rode the folded `backlog-notes` action, not a catalog command).
const NATIVE_AUX: &[&str] = &["notes"];

/// Resolve the head of `args` to a legacy command. Grouped spelling first
/// (`<group> <action>`, only when the second token IS one of that group's
/// actions - several group names are also legacy commands: `fno backlog get
/// x` is the legacy `get`, not a malformed grouped call), then the bare
/// legacy command itself, then the native aux vocabulary.
fn resolve_head(args: &[String]) -> Result<Resolved<'_>, String> {
    let first = match args.first() {
        Some(a) => a.as_str(),
        None => return Err(help_text()),
    };
    if let Some(aux) = NATIVE_AUX.iter().find(|a| **a == first) {
        return Ok(Resolved {
            legacy: aux,
            tail: &args[1..],
        });
    }
    if let Some(group) = GROUPS.iter().find(|g| g.name == first) {
        if let Some(action) = group
            .actions
            .iter()
            .find(|(a, _)| Some(*a) == args.get(1).map(String::as_str))
            .map(|(_, legacy)| *legacy)
        {
            return Ok(Resolved {
                legacy: action,
                tail: &args[2..],
            });
        }
    }
    match commands::from_legacy(first) {
        Some((group, action)) => Ok(Resolved {
            legacy: commands::resolve(group, action).expect("catalog round-trip"),
            tail: &args[1..],
        }),
        None => Err(format!(
            "fno-agents backlog: unknown command {first}. {}",
            help_text()
        )),
    }
}

fn help_text() -> String {
    let mut out = String::from("fno backlog <group> <action> [args...]\n");
    out.push_str("       fno backlog <legacy-command> [args...]  (compatibility spelling)\n\n");
    for g in GROUPS {
        out.push_str(&format!(
            "  {:<8} {:<32} {}\n",
            g.name,
            g.purpose,
            g.actions
                .iter()
                .map(|(a, _)| *a)
                .collect::<Vec<_>>()
                .join("|")
        ));
    }
    out.push_str(
        "\nActions not yet native forward to the Python surface until their port \
         lands; retired names refuse by name.\n",
    );
    out
}

/// Whether any engine marker appears in `tail`.
fn carries(tail: &[String], markers: &[&str]) -> bool {
    tail.iter().any(|a| markers.contains(&a.as_str()))
}

/// The run entry: returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let head = args.first().map(String::as_str);
    if head.is_none() || head == Some("-h") || head == Some("--help") {
        println!("{}", help_text());
        return 0;
    }
    let resolved = match resolve_head(args) {
        Ok(r) => r,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    match resolved.legacy {
        // The folded corpus/history reader: fully native, engine owns the
        // whole surface (inventory, migrate, history, stale, findings).
        "notes" => super::note_migrate::run_notes(resolved.tail),
        // The folded note action. A leading `--graph` is the engine door's
        // shape (bridge, passthrough, status fanout); the plain public
        // shape still carries Python-owned legs (evidence, identity,
        // reader walk, delivery) and rides the forward until its port.
        "note" if leads_with_engine_door(resolved.tail) => super::note_cli::run_note(resolved.tail),
        // The folded patch door: the same leading-`--graph` contract the
        // lifecycle door speaks. The full public flag surface stays
        // Python-owned until its port.
        "update" if leads_with_engine_door(resolved.tail) => {
            super::patch::run_update(resolved.tail)
        }
        // The folded batch read: the engine contract is a leading or
        // trailing `--graph`, several ids without the single-id render
        // flags, or the stdin tracker door (zero positionals: the engine's
        // own stdin arm decides). The single-id render ladder (field /
        // grouped / tiers / archive walk) is native.
        "get"
            if leads_with_engine_door(resolved.tail)
                || carries(resolved.tail, &["--graph"])
                || positional_count(resolved.tail) == 0
                || (positional_count(resolved.tail) > 1
                    && !carries(resolved.tail, &["--field", "--grouped", "--strict"])) =>
        {
            crate::graph_get::run_graph_get(resolved.tail)
        }
        "get" => super::get_cli::run(resolved.tail),
        // The search ladder is native; the FTS cache and the external
        // backend reads forward from inside.
        "find" => super::find_cli::run(resolved.tail),
        _ => forward_python(&resolved),
    }
}

/// Whether the tail opens with the engine door's `--graph` token.
fn leads_with_engine_door(tail: &[String]) -> bool {
    tail.first().map(String::as_str) == Some(ENGINE_DOOR_HEAD)
}

fn positional_count(tail: &[String]) -> usize {
    tail.iter().filter(|a| !a.starts_with('-')).count()
}

/// The compatibility forward: exec the wheel's Python CLI with the resolved
/// `fno backlog <legacy-command> ...` argv, stdio inherited, its exit code
/// returned. A grouped spelling arrives as the legacy command it resolves
/// to, so the Python surface still sees the shape it has always owned. The
/// one new seam crossing this fold pays; the resolver is the baselined
/// `scrape::fno_py`. Branch handlers (the native get and find forward their
/// external-backend and FTS branches through this same door).
pub fn forward_to_python(legacy: &str, tail: &[String]) -> i32 {
    let mut cmd = std::process::Command::new(crate::scrape::fno_py());
    cmd.arg("backlog").arg(legacy).args(tail);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("fno-agents backlog: the Python CLI could not be exec'd: {err}");
        eprintln!("       reinstall fno, run `fno doctor update --rust`, or set FNO_PY.");
        2
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("fno-agents backlog: the Python CLI could not be run: {err}");
            2
        }
    }
}

fn forward_python(resolved: &Resolved<'_>) -> i32 {
    forward_to_python(resolved.legacy, resolved.tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(args: &[&str]) -> Result<&'static str, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match resolve_head(&owned) {
            Ok(r) => Ok(r.legacy),
            Err(e) => Err(e),
        }
    }

    #[test]
    fn grouped_spellings_resolve_through_the_catalog() {
        assert_eq!(routed(&["add", "node", "t"]), Ok("add"));
        assert_eq!(routed(&["get", "node", "x-abc"]), Ok("get"));
        assert_eq!(routed(&["note", "state", "x"]), Ok("note"));
        assert_eq!(routed(&["update", "rank", "x"]), Ok("rank"));
    }

    #[test]
    fn legacy_spellings_resolve_to_themselves() {
        assert_eq!(routed(&["add", "t"]), Ok("add"));
        assert_eq!(routed(&["get", "x-abc"]), Ok("get"));
        assert_eq!(routed(&["notes", "history", "x"]), Ok("notes"));
    }

    #[test]
    fn a_non_action_second_token_falls_back_to_the_legacy_command() {
        // `fno backlog add nope` is the legacy add with title "nope", not a
        // malformed grouped call - byte-for-byte what Python answered today.
        assert_eq!(routed(&["add", "nope"]), Ok("add"));
        assert_eq!(routed(&["get", "x-abc"]), Ok("get"));
        assert_eq!(routed(&["add"]), Ok("add"));
    }

    #[test]
    fn an_unknown_command_refuses_with_the_catalog() {
        let err = routed(&["frobnicate"]).unwrap_err();
        assert!(err.contains("unknown command frobnicate"), "{err}");
    }

    #[test]
    fn the_native_aux_vocabulary_resolves_before_the_catalog() {
        assert_eq!(routed(&["notes", "history", "x"]), Ok("notes"));
    }

    #[test]
    fn a_grouped_spelling_forwards_as_the_legacy_command_it_resolves_to() {
        // The compat forward passes [legacy] + tail, so `fno backlog move
        // done x` reaches Python as `fno-py backlog done x` - the shape it
        // owns - never the grouped form it does not know.
        let owned: Vec<String> = ["move", "done", "x-abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = resolve_head(&owned).unwrap();
        assert_eq!(r.legacy, "done");
        assert_eq!(r.tail, &["x-abc".to_string()]);
    }

    #[test]
    fn the_note_route_splits_engine_markers_from_the_public_shape() {
        // The engine door opens with --graph: the bridges, the passthrough
        // branch, and the status fanout all lead with it.
        let engine: Vec<String> = [
            "note", "--graph", "/tmp/g", "--stdin", "--json", "--node", "x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(leads_with_engine_door(resolve_head(&engine).unwrap().tail));
        let public: Vec<String> = ["note", "x-abc", "text"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let public_tail = resolve_head(&public).unwrap().tail;
        assert!(!leads_with_engine_door(public_tail));
        // The over-match this test pins shut: a stray engine spelling
        // anywhere else in a public tail keeps the Python path, whose
        // parser ignores unknown extras - the shape today's behavior has.
        let stray: Vec<String> = ["note", "x-abc", "text", "--stdin"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let stray_tail = resolve_head(&stray).unwrap().tail;
        assert!(!leads_with_engine_door(stray_tail));
        assert!(!carries(stray_tail, &["--graph"]));
    }

    #[test]
    fn the_batch_get_contract_is_two_positionals_without_render_flags() {
        let batch: Vec<String> = ["get", "x-aaaa", "x-bbbb", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = resolve_head(&batch).unwrap();
        assert_eq!(positional_count(r.tail), 2);
        assert!(!carries(r.tail, &["--field", "--grouped", "--strict"]));
        let single: Vec<String> = ["get", "x-aaaa", "--field", "status"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = resolve_head(&single).unwrap();
        // A render flag in the tail keeps the single-id call on the Python
        // path even though the flag's VALUE also counts as a positional.
        assert!(carries(r.tail, &["--field", "--grouped", "--strict"]));
    }

    #[test]
    fn help_text_lists_every_group_and_the_compat_note() {
        let text = help_text();
        for g in GROUPS {
            assert!(text.contains(g.name), "group {} missing from help", g.name);
        }
        assert!(text.contains("forward to the Python surface"), "{text}");
    }

    #[test]
    fn empty_args_resolve_to_the_help_error() {
        let err = routed(&[]).unwrap_err();
        assert!(err.starts_with("fno backlog <group>"), "{err}");
    }
}
