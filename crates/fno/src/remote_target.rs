//! The front door's cross-host intercept.
//!
//! When a positional target carries `@`, the invocation is cross-host:
//! `agents mail send <name>@<host> ...` and `agents peek <handle>@<host>
//! ...` exec the sibling `fno-agents mail-remote` command. A `mail send` target
//! that is a plain name is claimed too: the relay resolves it (local live-row
//! check first, then the shared store's name-to-host binding) and forwards
//! byte-verbatim when nothing binds, so the local command's ladder, holds, and
//! refusal voice stay the only path for a name it knows. `org promote` with
//! an `@host` target execs the sibling `org-remote` command, the cross-machine
//! hand-off. `agents spawn` claims when the fno-owned `--name` value carries
//! `@` (`--name <name>@<host>`, or `--name @<host>` to mint the name
//! remotely) and execs the sibling `spawn-remote` command; words after a
//! bare `--` fence are never scanned. Everything else forwards verbatim.

use std::ffi::OsString;

/// The full argv, `agents` leading. The claimed tail runs from the command
/// onward (`send|peek` first), the shape `mail_remote::run` routes on;
/// `None` forwards byte-verbatim.
pub fn classify(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.first()?.to_str()? != "agents" {
        return None;
    }
    // The relay's own same-host forward (the twin const lives in
    // mail_remote.rs): claiming it again recursed every send until the
    // machine ran out of processes. Clear it so a daemon or session the
    // local command starts never inherits the bypass.
    if std::env::var_os("FNO_MAIL_LOCAL_FORWARD").is_some() {
        std::env::remove_var("FNO_MAIL_LOCAL_FORWARD");
        return None;
    }
    let tail: Vec<OsString>;
    match args.get(1)?.to_str()? {
        "mail" => {
            if !matches!(args.get(2)?.to_str()?, "send" | "peek") {
                return None;
            }
            let target = args.get(3)?.to_str()?;
            if args.get(2)?.to_str()? == "send" {
                // `@host` routes cross-host; a plain name goes to the relay's
                // resolver (local row first, then the store binding), which
                // forwards byte-verbatim when nothing binds. Peek stays
                // @-only: the node's scope is send-side resolution.
                if !target.contains('@') && !is_plain_name_target(target) {
                    return None;
                }
            } else if !target.contains('@') {
                return None;
            }
            tail = args[2..].to_vec();
        }
        "peek" => {
            let target = args.get(2)?.to_str()?;
            if !target.contains('@') {
                return None;
            }
            tail = args[1..].to_vec();
        }
        _ => return None,
    }
    Some(tail)
}

/// The claim's shape test for a bare `mail send` target: no `@` (those route
/// cross-host directly), no path or scope separators, and not a session shape
/// (8-hex short id, 32+-char hex or dashed full id), so a session address
/// never leaves the local ladder for the extra exec hop. Letters, digits,
/// dots, dashes, underscores, 2+ chars. The twin on the resolver side
/// (`mail_remote::is_plain_name_target`) re-checks and forwards byte-verbatim
/// on any mismatch, so drift between the two halves only ever widens the
/// claim toward the safe direction.
pub fn is_plain_name_target(token: &str) -> bool {
    if token.len() < 2
        || token.starts_with('-')
        || token.contains('/')
        || token.contains(':')
        || token
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
    {
        return false;
    }
    // A dashed uuid strips to one hex run, so the hex test reads the
    // dash-stripped token; the length bounds catch the bare 8-hex short id
    // and the 32-hex full id, dashed or not.
    let stripped: String = token.chars().filter(|c| *c != '-').collect();
    let hexish = stripped.chars().all(|c| c.is_ascii_hexdigit());
    if hexish && (stripped.len() == 8 || stripped.len() >= 32) {
        return false;
    }
    true
}

/// The cross-machine hand-off claim: `agents org promote <name>@<host> ...`
/// execs the sibling `org-remote` command (the same relay pattern as mail).
/// Targets without `@` forward to the local command, today's behavior.
pub fn classify_org(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.first()?.to_str()? != "agents" {
        return None;
    }
    // The ssh hop's remote leg lands here too: `agents org-remote
    // record-succession ...` execs the sibling command directly.
    if args.get(1)?.to_str()? == "org-remote" {
        return Some(args[2..].to_vec());
    }
    if args.get(1)?.to_str()? != "org" || args.get(2)?.to_str()? != "promote" {
        return None;
    }
    let target = args.get(3)?.to_str()?;
    if !target.contains('@') {
        return None;
    }
    // The tail keeps the command: org_remote::run routes on it.
    Some(args[2..].to_vec())
}

/// The cross-host spawn claim: `agents spawn --name <name>@<host> ...`
/// execs the sibling `spawn-remote` command. The positional is the prompt,
/// which carries `@` freely, so the address rides only in the `--name`
/// value (space or `=` spelling), a flag fno owns; the scan stops at a
/// bare `--` fence.
pub fn classify_spawn(args: &[OsString]) -> Option<Vec<OsString>> {
    if args.first()?.to_str()? != "agents" {
        return None;
    }
    if args.get(1)?.to_str()? != "spawn" {
        return None;
    }
    let mut i = 2;
    while let Some(word) = args.get(i).and_then(|a| a.to_str()) {
        if word == "--" {
            return None;
        }
        if word == "--name" {
            let value = args.get(i + 1).and_then(|a| a.to_str())?;
            if value.contains('@') {
                return Some(args[2..].to_vec());
            }
        } else if let Some(v) = word.strip_prefix("--name=") {
            if v.contains('@') {
                return Some(args[2..].to_vec());
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn classify_claims_only_at_host_targets() {
        // A mail send with an @host target is claimed; the tail runs from
        // the command onward, the shape mail_remote::run routes on.
        let argv = classify(&os(&["agents", "mail", "send", "nemo@mbp3", "hello"]))
            .expect("@host send is claimed");
        assert_eq!(argv, os(&["send", "nemo@mbp3", "hello"]));
        // A peek with an @host handle is claimed.
        let argv = classify(&os(&["agents", "peek", "nemo@mbp3", "-n", "5"]))
            .expect("@host peek is claimed");
        assert_eq!(argv, os(&["peek", "nemo@mbp3", "-n", "5"]));
        // Bare targets forward byte-verbatim.
        assert!(classify(&os(&["agents", "mail", "send", "nemo", "hello"])).is_some());
        assert!(classify(&os(&["agents", "peek", "nemo"])).is_none());
        // Other agents commands and other mail subcommands forward.
        assert!(classify(&os(&["agents", "claim", "status"])).is_none());
        assert!(classify(&os(&["agents", "mail", "list"])).is_none());
        // A plain-name send is claimed; a session-shaped target is not.
        let argv = classify(&os(&["agents", "mail", "send", "quinlan", "hello"]))
            .expect("plain name is claimed");
        assert_eq!(argv, os(&["send", "quinlan", "hello"]));
        assert!(classify(&os(&["agents", "mail", "send", "04b5621d", "hello"])).is_none());
        assert!(classify(&os(&[
            "agents",
            "mail",
            "send",
            "dde96ee5-64d9-4ce4-bbf8-c7c38116d9f3",
            "m"
        ]))
        .is_none());
        // The org claim fires only for promote with an @host target, and
        // carries the tail WITHOUT the group word (the command routes on
        // `promote` first). The ssh hop's remote leg execs `agents
        // org-remote ...` directly.
        let argv = classify_org(&os(&[
            "agents",
            "org",
            "promote",
            "quinlan@imac2d2",
            "--scope",
            "fno",
        ]))
        .expect("org promote @host is claimed");
        assert_eq!(argv, os(&["promote", "quinlan@imac2d2", "--scope", "fno"]));
        let argv = classify_org(&os(&[
            "agents",
            "org-remote",
            "record-succession",
            "--scope",
            "fno",
        ]))
        .expect("org-remote tail is claimed");
        assert_eq!(argv, os(&["record-succession", "--scope", "fno"]));
        assert!(classify_org(&os(&[
            "agents", "org", "promote", "quinlan", "--scope", "fno"
        ]))
        .is_none());
        assert!(classify_org(&os(&["agents", "org", "grant", "quinlan@imac2d2"])).is_none());
        assert!(classify_org(&os(&["agents", "org", "promote"])).is_none());
    }

    #[test]
    fn classify_spawn_claims_only_an_at_name_value() {
        // The claim carries the tail without the group and command words.
        let argv = classify_spawn(&os(&[
            "agents",
            "spawn",
            "--name",
            "quinlan@mbp3",
            "fix",
            "it",
        ]))
        .expect("@host name is claimed");
        assert_eq!(argv, os(&["--name", "quinlan@mbp3", "fix", "it"]));
        // `@host` alone mints remotely, and the typer equals spelling
        // claims the same: both relay.
        let argv = classify_spawn(&os(&["agents", "spawn", "--name", "@mbp3", "hi"]))
            .expect("@host-only name is claimed");
        assert_eq!(argv, os(&["--name", "@mbp3", "hi"]));
        let argv = classify_spawn(&os(&["agents", "spawn", "--name=q@mbp3", "hi"]))
            .expect("equals spelling is claimed");
        assert_eq!(argv, os(&["--name=q@mbp3", "hi"]));
        // A plain name, a flag value with an @ that is not a --name value,
        // and an @ riding in the prompt positional all forward.
        assert!(classify_spawn(&os(&["agents", "spawn", "--name", "quinlan", "hi"])).is_none());
        assert!(classify_spawn(&os(&["agents", "spawn", "mail me at a@b"])).is_none());
        // The fence hides harness flags: a --name after it is the harness's.
        assert!(classify_spawn(&os(&["agents", "spawn", "--", "--name", "x@y"])).is_none());
        assert!(classify_spawn(&os(&["agents", "spawn"])).is_none());
    }

    #[test]
    fn plain_name_shape_excludes_session_shapes() {
        assert!(is_plain_name_target("Quinlan"));
        assert!(is_plain_name_target("ellery-mbp"));
        assert!(is_plain_name_target("a.b"));
        assert!(!is_plain_name_target("04b5621d"));
        assert!(!is_plain_name_target(
            "dde96ee5-64d9-4ce4-bbf8-c7c38116d9f3"
        ));
        assert!(!is_plain_name_target("a/b"));
        assert!(!is_plain_name_target("a:b"));
        assert!(!is_plain_name_target("n"));
        // A flag-led send names no target in slot 3.
        assert!(!is_plain_name_target("--from-name"));
        assert!(classify(&os(&[
            "agents",
            "mail",
            "send",
            "--from-name",
            "fno/quiet-recovery",
            "--to-lead",
            "scope-a",
            "body",
        ]))
        .is_none());
    }
}
