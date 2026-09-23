# Vocabulary: user, superuser, and the reserved token operator

This document states what the words mean in this repo, where each sense lives, and which senses stay spelled `operator` forever. The authority an agent can never synthesize is called **superuser** (law d-cfc62071, operator's ruling of 2026-09-14). It was formerly spelled `operator authority`.

## Is this page for you?

You are about to rename, grep-and-replace, or otherwise "fix" one of these words. Or you are writing a new string that names the authority or addresses the human. This page owns the vocabulary decision and the senses table. Following it keeps the authority guards, the wire values, and the shell-operator homonym intact.

Not for: how the config wizard runs or how the inbox queue is acked. Those live in the configuration guide and the inbox help.

The words carry different meanings, and the difference is load-bearing. This page is the one place that says what each word means, so nobody fixes a wording gripe with a repo-wide substitution.

**User** is who we are talking to: the human on the other side. `config.user.name` is what the machine calls them, and `display_name()` in `cli/src/fno/user.py` is the one reader: configured name, else `git config user.name`, else "you". Set the name with `fno config set user.name <name>`. The first-run wizard asks for it. The human's own queue verb is `fno inbox user`. The pre-rename spelling `fno inbox operator` still works as a hidden alias.

**Superuser** is the authority an agent can never synthesize for itself. Every live string, gate, doc and skill spells it superuser. `--authority operator` stays the wire value that opens the superuser lane.

**Operator** is a reserved token. It is never the word for the authority, and it is never the address. The senses it still names are in the table below.

## Why user and superuser must stay separate

The AGENTS.md pitfall needs both words to say what it says. It reads: "`fno agents mail send` injects as user-shaped text, indistinguishable from superuser typing." A flat rename collapses the human being addressed into the authority being claimed. The sentence that warns about confusing them then stops warning about anything.

## The senses of "operator"

Measured 2026-09-11: 5408 hits across 1012 files. Measured 2026-09-15 for the authority rename: 43 hits for the two patterns outside tests. Every one is either renamed to superuser or a retired form listed below. A future sweep starts here, not at sed.

| Sense | Where, with the count | Renamed? |
|---|---|---|
| Authority | the phrase `operator authority` and `operator-authored`: renamed across strings, gates, docs, skills, hooks and test pins; the decide refusal now reads "cannot record under superuser authority" | Yes. Law d-cfc62071 renames it superuser. |
| Retired trailer literals | the byte-exact trailer consts in `crates/fno-agents/src/mail_inject.rs` ("Retired form, still accepted for queued records"), the `{standing}` builder that renders them, the structural-refusal message that cites them, and that gate's test fixtures | No. They must match stored bodies byte for byte, and the forgery gate must keep matching the shapes attackers quote. |
| Wire value | the origin string in ledgers and envelopes: 80 sites in `cli/src` and `crates/*/src`, plus the env pins `FNO_OPERATOR_SESSION_ID`, `FNO_OPERATOR_HARNESS`, `FNO_OPERATOR_TRANSCRIPT`, `FNO_OPERATOR_CAPTURE_DIR`, and the `--authority operator` value | No. A data migration with no gain the human can see. |
| Persisted graph data | `source_kind: operator_request`: 12 source files plus live rows in `graph.json` | No. Stored values; the display layer can relabel without touching them. |
| Shipped config keys | `routing.operator_access` and `routing.operator_view`, written in `crates/fno-agents/src/route_slot.rs` | No. They are already in users' config.toml files. |
| Shell homonym | `hooks/king-delegation-guard.sh`, where operator means a redirect operator, not a person | No. Nothing to rename. |

## Judged keep-list: authority-adjacent phrasings that stay

Each phrasing below was weighed against the table during the authority rename and keeps the word operator on purpose.

- **operator ruling** - a dated decision citation ("operator ruling 2026-08-13"). It records who ruled. It is history, not a live gate. New rulings say superuser in their prose.
- **operator override** - config precedence the human set (`EVENTS_SCHEMA_PATH`, `FNO_AGENTS_BIN`). It names a setting, not the authority a gate checks.
- **queue lanes** - the king board's operator-request lane, the `fno inbox user` queue's internal "operator lane" file name, and the origin-keyed voter lane. They hang off the wire value and the stored rows.
- **operator typing** for pane provenance - keystroke provenance in `mux_cli.rs`, not the mail-probe warning. The mail-probe pitfall family says superuser typing.

## A deliberate omission

`crates/fno-agents/src/finalize.rs` keeps its generic human-facing string. A Rust copy of `display_name()` is a second implementation of one behavior. A parity harness forcing the two to agree costs more than the string staying generic.

## Before you run sed

The address sense is renamed to `user` in skill prose; `scripts/ci/check-operator-address.sh` keeps it there.

Read the tables first. Every row except the authority sense, which is done, is out of scope by decision, not by oversight. A rename of the address sense goes through `cli/src/fno/user.py` (`UserBlock`, `display_name()`), the three address strings that import it, and the `fno inbox user` verb. It does not touch the reserved senses, the wire values, the graph rows, the shipped config keys, or the shell homonym.
