//! The `fno agents org` group: the people spelling of the role verbs,
//! rewritten lexically to the argv that answers today. The old spellings
//! forward with one stderr line for the alias release, then retire (see
//! scripts/ci/retired-commands.txt).

use std::ffi::OsString;

/// What the front door does with an invocation this module owns.
#[derive(Debug, PartialEq, Eq)]
pub enum Org {
    /// Run this (possibly rewritten) argv: forwarded to the Python CLI.
    Forward(Vec<OsString>),
    /// Print this help on stdout, exit 0.
    Help(String),
    /// Print this refusal on stderr, exit 2.
    Refuse(String),
}

const ACTIONS: &[&str] = &[
    "init", "checkin", "history", "term", "verdict", "escalate", "drain", "shape", "done",
    "cancel", "faq",
];

const HELP: &str = "fno agents org - the org chart of roles and their lifecycle

  fno agents org                                     the org chart
  fno agents org promote <session> --scope <scope>   grant a role to a live session
  fno agents org init                                write the role manifest (step 1 of the lead skill)
  fno agents org checkin [--name NAME] [--theme THEME] [--keep-name-from OLD-SCOPE]
                                                     one lead beat
  fno agents org history | term | verdict | escalate | drain | shape | done | cancel | faq
  fno agents org rundown [--out PATH]                the per-team rundown page
  fno agents org vacancies                           roles whose holder is gone
  fno agents org fold <scope>                        one role's scope fold
";

/// The old verb spellings and their `org` forms, for the one stderr line.
const OLD: &[(&str, &str)] = &[
    ("crown", "fno agents org promote <session> --scope <scope>"),
    ("court", "fno agents org"),
    ("court-fold", "fno agents org fold"),
    ("court-orphans", "fno agents org vacancies"),
    ("king", "fno agents org <action>"),
    ("king-checkin", "fno agents org checkin"),
    ("king-history", "fno agents org history"),
    ("reign-ledger", "fno agents org rundown"),
];

fn os(text: &str) -> OsString {
    OsString::from(text)
}

/// The one stderr line an old verb spelling prints while it still answers.
fn old_spelling_notice(verb: &str) -> Option<String> {
    OLD.iter()
        .find(|(old, _)| *old == verb)
        .map(|(old, instead)| {
            format!("fno agents {old} is now {instead}; the old spelling goes after one release.")
        })
}

/// `--out <pages>/rundown.html` for `org rundown` when the state root is
/// faithful; None otherwise (the ledger then writes its own default, and
/// the web bridge writes nothing).
#[cfg(not(test))]
fn rundown_out_arg() -> Option<OsString> {
    let (root, faithful) = crate::org_root::lead_state_root();
    if !faithful {
        return None;
    }
    Some(OsString::from(
        root.join("pages")
            .join("rundown.html")
            .display()
            .to_string(),
    ))
}

#[cfg(test)]
fn rundown_out_arg() -> Option<OsString> {
    None
}

/// `org rundown` argv: `king ledger` plus the default --out when the caller
/// gave none and the state root resolved faithfully.
fn rundown_argv(rest: &[OsString], default_out: Option<OsString>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![os("agents"), os("king"), os("ledger")];
    let has_out = rest.iter().any(|a| {
        a.to_str()
            .map(|s| s == "--out" || s.starts_with("--out="))
            .unwrap_or(false)
    });
    argv.extend(rest.iter().cloned());
    if !has_out {
        if let Some(path) = default_out {
            argv.push(os("--out"));
            argv.push(path);
        }
    }
    argv
}

/// The spawn door answers `--promote` natively, so a spawn argv is not
/// claimed here: it dispatches through the normal front unchanged, and the
/// retired `--crown`/`--succeed` spellings reach the door's one-release alias
/// path, which prints the notice.
fn org(rest: &[OsString]) -> Org {
    let first = rest.first().and_then(|a| a.to_str());
    let tail: Vec<OsString> = rest.iter().skip(1).cloned().collect();
    let mut court: Vec<OsString> = vec![os("agents"), os("court")];
    court.extend(rest.iter().cloned());
    match first {
        None => Org::Forward(vec![os("agents"), os("court")]),
        Some("-h" | "--help") => Org::Help(HELP.to_string()),
        Some(flag) if flag.starts_with('-') => Org::Forward(court),
        Some("promote") => {
            let mut argv: Vec<OsString> = vec![os("agents"), os("crown")];
            argv.extend(tail);
            Org::Forward(argv)
        }
        Some("rundown") => Org::Forward(rundown_argv(&tail, rundown_out_arg())),
        Some("vacancies") => {
            let mut argv: Vec<OsString> = vec![os("agents"), os("court-orphans")];
            argv.extend(tail);
            Org::Forward(argv)
        }
        Some("fold") => {
            let mut argv: Vec<OsString> = vec![os("agents"), os("court-fold")];
            argv.extend(tail);
            Org::Forward(argv)
        }
        Some(action) if ACTIONS.contains(&action) => {
            let mut argv: Vec<OsString> = vec![os("agents"), os("king"), os(action)];
            argv.extend(tail);
            Org::Forward(argv)
        }
        Some(word) => Org::Refuse(format!(
            "fno agents org {word}: unknown action. Actions: init, checkin, history, term, \
             verdict, escalate, drain, shape, done, cancel, faq, promote, rundown, vacancies, \
             fold. `fno agents org --help` shows them."
        )),
    }
}

/// The lexical claim: Some whenever this argv belongs to the `org` group or
/// an old role-verb spelling.
pub fn classify(args: &[OsString]) -> Option<Org> {
    if args.first().and_then(|a| a.to_str()) != Some("agents") {
        return None;
    }
    let verb = args.get(1).and_then(|a| a.to_str())?;
    if verb == "org" {
        return Some(org(&args[2..]));
    }
    if verb == "promote" {
        return Some(Org::Refuse(
            "fno agents promote is now fno agents org promote <session> --scope <scope>".into(),
        ));
    }
    let notice = old_spelling_notice(verb)?;
    eprintln!("{notice}");
    Some(Org::Forward(args.to_vec()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_forward_and_refusal_answers_at_the_front_door() {
        fn org_bare_and_flags_forward_the_court_argv() {
            assert_eq!(
                classify(&osv(&["agents", "org"])),
                Some(Org::Forward(osv(&["agents", "court"])))
            );
            assert_eq!(
                classify(&osv(&["agents", "org", "-J"])),
                Some(Org::Forward(osv(&["agents", "court", "-J"])))
            );
            assert_eq!(
                classify(&osv(&["agents", "org", "-n", "--json"])),
                Some(Org::Forward(osv(&["agents", "court", "-n", "--json"])))
            );
        }

        fn org_help_is_native_and_names_the_actions() {
            match classify(&osv(&["agents", "org", "--help"])) {
                Some(Org::Help(text)) => {
                    assert!(text.contains("promote"), "{text}");
                    assert!(text.contains("rundown"), "{text}");
                }
                other => panic!("expected Help, got {other:?}"),
            }
        }

        fn org_promote_forwards_the_crown_argv() {
            assert_eq!(
                classify(&osv(&[
                    "agents", "org", "promote", "folio", "--scope", "fno"
                ])),
                Some(Org::Forward(osv(&[
                    "agents", "crown", "folio", "--scope", "fno"
                ])))
            );
        }

        fn org_rundown_appends_the_default_out_only_when_absent() {
            let default = Some(OsString::from("/s/pages/rundown.html"));
            assert_eq!(
                rundown_argv(&osv(&[]), default.clone()),
                osv(&["agents", "king", "ledger", "--out", "/s/pages/rundown.html"])
            );
            assert_eq!(
                rundown_argv(&osv(&["--out", "/x.html"]), default.clone()),
                osv(&["agents", "king", "ledger", "--out", "/x.html"])
            );
            assert_eq!(
                rundown_argv(&osv(&["--out=/x.html"]), default),
                osv(&["agents", "king", "ledger", "--out=/x.html"])
            );
        }

        fn org_vacancies_and_fold_forward_the_old_sweeps() {
            assert_eq!(
                classify(&osv(&["agents", "org", "vacancies", "--json"])),
                Some(Org::Forward(osv(&["agents", "court-orphans", "--json"])))
            );
            assert_eq!(
                classify(&osv(&["agents", "org", "fold", "fno"])),
                Some(Org::Forward(osv(&["agents", "court-fold", "fno"])))
            );
        }

        fn every_lifecycle_action_forwards_the_king_action() {
            for action in ACTIONS {
                assert_eq!(
                    classify(&osv(&["agents", "org", action, "--flag"])),
                    Some(Org::Forward(osv(&["agents", "king", action, "--flag"])))
                );
            }
        }

        fn an_unknown_org_word_refuses_listing_the_actions() {
            match classify(&osv(&["agents", "org", "frobnicate"])) {
                Some(Org::Refuse(message)) => {
                    assert!(message.contains("checkin"), "{message}");
                    assert!(message.contains("rundown"), "{message}");
                }
                other => panic!("expected Refuse, got {other:?}"),
            }
        }

        fn every_old_spelling_forwards_unchanged_with_a_notice() {
            for (verb, instead) in OLD {
                assert_eq!(
                    classify(&osv(&["agents", verb, "-J"])),
                    Some(Org::Forward(osv(&["agents", verb, "-J"])))
                );
                let notice = old_spelling_notice(verb).unwrap();
                assert!(notice.contains(instead), "{notice}");
                assert!(notice.contains("one release"), "{notice}");
            }
        }

        fn bare_promote_refuses_naming_the_group_form() {
            match classify(&osv(&["agents", "promote", "x"])) {
                Some(Org::Refuse(message)) => {
                    assert!(message.contains("fno agents org promote"), "{message}");
                }
                other => panic!("expected Refuse, got {other:?}"),
            }
        }

        fn argv_this_module_does_not_own_is_none() {
            assert_eq!(classify(&osv(&["mux", "ls"])), None);
            assert_eq!(classify(&osv(&["agents", "whoami"])), None);
            assert_eq!(classify(&osv(&["agents", "spawn", "n"])), None);
            assert_eq!(classify(&osv(&["agents", "spawn", "--promote", "x"])), None);
            assert_eq!(classify(&osv(&[])), None);
        }
        org_bare_and_flags_forward_the_court_argv();
        org_help_is_native_and_names_the_actions();
        org_promote_forwards_the_crown_argv();
        org_rundown_appends_the_default_out_only_when_absent();
        org_vacancies_and_fold_forward_the_old_sweeps();
        every_lifecycle_action_forwards_the_king_action();
        an_unknown_org_word_refuses_listing_the_actions();
        every_old_spelling_forwards_unchanged_with_a_notice();
        bare_promote_refuses_naming_the_group_form();
        argv_this_module_does_not_own_is_none();
    }
    use super::*;

    fn osv(texts: &[&str]) -> Vec<OsString> {
        texts.iter().map(OsString::from).collect()
    }
}
