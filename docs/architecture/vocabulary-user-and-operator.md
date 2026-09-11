# Vocabulary: user and operator

Two words, two meanings, and the difference is load-bearing. This page is the one place that says what each word means, so nobody fixes a wording gripe with a repo-wide substitution.

**User** is who we are talking to: the human on the other side. `config.user.name` is what the machine calls them, and `display_name()` in `cli/src/fno/user.py` is the one reader: configured name, else `git config user.name`, else "you". Set the name with `fno config set user.name <name>`. The first-run wizard asks for it. The human's own queue verb is `fno inbox user`. The pre-rename spelling `fno inbox operator` still works as a hidden alias.

**Operator** is the authority an agent can never synthesize for itself. It stays spelled operator everywhere it means that.

## Why the two words must stay separate

The AGENTS.md pitfall needs both words to say what it says. It reads: "`fno agents mail send` injects as user-shaped text, indistinguishable from operator typing." A flat rename collapses the human being addressed into the authority being claimed. The sentence that warns about confusing them then stops warning about anything.

## The five senses of "operator"

Measured 2026-09-11: 5408 hits across 1012 files. A future sweep starts here, not at sed.

| Sense | Where, with the count | Renamed? |
|---|---|---|
| Authority | the phrase `operator authority`: 40 hits in 21 files, e.g. `cli/src/fno/mail/envelope.py`, `crates/fno-agents/src/mail_inject.rs`, `scripts/lib/drive-authority.sh` | No. It stays. |
| Wire value | the origin string in ledgers and envelopes: 80 sites in `cli/src` and `crates/*/src`, plus the env pins `FNO_OPERATOR_SESSION_ID`, `FNO_OPERATOR_HARNESS`, `FNO_OPERATOR_TRANSCRIPT`, `FNO_OPERATOR_CAPTURE_DIR` | No. A data migration with no gain the human can see. |
| Persisted graph data | `source_kind: operator_request`: 12 source files plus live rows in `graph.json` | No. Stored values; the display layer can relabel without touching them. |
| Shipped config keys | `routing.operator_access` and `routing.operator_view`, written in `crates/fno-agents/src/route_slot.rs` | No. They are already in users' config.toml files. |
| Shell homonym | `hooks/king-delegation-guard.sh`, where operator means a redirect operator, not a person | No. Nothing to rename. |

## A deliberate omission

`crates/fno-agents/src/finalize.rs` keeps its generic human-facing string. A Rust copy of `display_name()` is a second implementation of one behavior. A parity harness forcing the two to agree costs more than the string staying generic.

## Before you run sed

Read the table first. Every row except the address sense is out of scope by decision, not by oversight. A rename of the address sense goes through `cli/src/fno/user.py` (`UserBlock`, `display_name()`), the three address strings that import it, and the `fno inbox user` verb. It does not touch the authority word, the wire values, the graph rows, the shipped config keys, or the shell homonym.
