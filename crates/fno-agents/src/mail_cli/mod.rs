//! The `mail` front verbs on the Rust leg: the do-not-disturb hold, its
//! release timer, the pane-send renderer, the session seed-provenance leaf,
//! the `team` broadcast, the outbound sent view, the withdraw leaf, the
//! unread listing, the ack leaf, the raw prompt-line send, and the drain-self
//! leaf. Dispatch only; the verbs live in [hold], [pane_prepare],
//! [seed_provenance], [team], [sent], [withdraw], [unread], [ack],
//! [send_raw], and [drain_self].
pub mod ack;
pub mod drain_self;
pub mod hold;
pub mod pane_prepare;
pub mod seed_provenance;
pub mod send;
pub mod send_raw;
pub mod sent;
pub mod team;
pub mod unread;
pub mod withdraw;

pub fn run(rest: &[String]) -> i32 {
    let first = rest.first().map(String::as_str);
    if rest.is_empty() || first == Some("--help") || first == Some("-h") {
        print_help();
        return if rest.is_empty() { 2 } else { 0 };
    }
    match rest[0].as_str() {
        "hold" | "dnd" => hold::run_hold(&rest[1..]),
        "hold-release" => hold::run_hold_release(&rest[1..]),
        "seed-provenance" => seed_provenance::run(&rest[1..]),
        "team" => team::run_team(&rest[1..]),
        "withdraw" => withdraw::run(&rest[1..]),
        "sent" => sent::run_sent(&rest[1..]),
        "send" => send::run(&rest[1..]),
        "pane-prepare" => pane_prepare::run(&rest[1..]),
        "unread" => unread::run(&rest[1..]),
        "ack" => ack::run_ack(&rest[1..]),
        "drain-self" => drain_self::run_drain_self(&rest[1..]),
        other => {
            eprintln!(
                "fno-agents mail: unknown verb {other:?} (hold | dnd | hold-release | seed-provenance | team | withdraw | sent | send | pane-prepare | unread | ack | drain-self)"
            );

            2
        }
    }
}

fn print_help() {
    println!("fno-agents mail - the do-not-disturb hold, team broadcast, sent view, withdraw, unread listing, ack, raw prompt-line send, and drain-self for agent mail");
    println!("  hold [--minutes N | --for N] [--status] [--off]   alias: dnd");
    println!("  hold-release --handle KEY [--poll-s S]");
    println!("  seed-provenance                                 payload from FNO_SEED_PROV_* env");
    println!("  team --scope <all|leads|<role>|project:<p>> <message>");
    println!("  withdraw MSG_ID [--from-name NAME]");
    println!("  sent [--unclaimed] [--json] [--from-name LABEL]");
    println!("  pane-prepare --session-id S --pane N [--harness H] [--style-exception R]");
    println!("  send [--check] [--self-ok] [--review-request] [--style-exception <r>] [--origin <o>] <name> <payload>   raw prompt-line lane");
    println!("  unread [--name NAME | -n NAME] [--json | -J]");
    println!("  ack MSG_ID [--name NAME]                        alias front: fno mail ack");
    println!("  drain-self [--json]");
}
