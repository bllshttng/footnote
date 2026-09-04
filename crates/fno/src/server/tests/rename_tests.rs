use super::*;

// ---- sideline row rename: refusals before any subprocess ----

#[test]
fn rename_agent_grammar_and_resolver_refusals_send_notice_spawn_nothing() {
    // An illegal label is a NOTICE before any resolution, and the
    // resolver's unknown/external refusals reuse the StopAgent wording.
    // None of the three spawns a subprocess - `agent_rename_action` is
    // only reached on a resolved, legal target (the exit path is not
    // observable here; the notice assertions are the positive markers).
    let mut core = empty_core();
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.command(
        1,
        Command::RenameAgent {
            name: "w1".into(),
            new_name: "bad label!".into(),
        },
    );
    assert!(
        drain_notice(&mut rx).unwrap().contains("1-64 letters"),
        "the grammar refusal names the rule"
    );

    core.command(
        1,
        Command::RenameAgent {
            name: "no-such-row".into(),
            new_name: "fine-label".into(),
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("no such agent"));

    core.agents = vec![bg_row("ext-row", "/tmp", Some("ext00001"))];
    core.agents[0].external = true;
    core.command(
        1,
        Command::RenameAgent {
            name: "ext-row".into(),
            new_name: "fine-label".into(),
        },
    );
    assert!(drain_notice(&mut rx).unwrap().contains("external"));
}
