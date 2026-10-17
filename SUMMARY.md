# SUMMARY - the wrapped mail send port

Branch `feature/x-ca41-mail-port-send-leaf`, two commits on top of origin/main:
`3837990249` (the Rust port, goldens, capture driver, differential parity
test) and `de9ebd78f2` (front flip, Python body deletion, test re-lensing).

## What shipped

The wrapped `mail send` verb lives on the Rust leg now. `send/` is a
directory module split by question: resolution (the five-store reach sweep),
engine (the dispatch machinery and live transports), lanes (the
address-shape lanes), and verify (landing, escalation, liveness). The front
claims flag-led sends and session-shaped targets; @host and plain-name
targets stay the cross-host relay's, --help stays Python's, and cmd_send is
a forward shell that execs the native verb so the relay's byte-verbatim
forward and the typer route keep working.

26 goldens frozen from the live Python leg replay against the Rust leg in
`mail_send_parity.rs` (characterization stage at this head; the capture
driver stays as the lineage doc). The frozen-safe gates pass on this tree:
parity provenance, file budget, flag registry, seam crossings (116/116),
internal refs.

## Decisions worth a reviewer's attention

- **Store-based resolution.** The Python resolver walked live process
  tables (psutil) as one store. The port drops that store and keeps the
  five file-based stores plus session_truth; the inject-as-probe rung
  covers the process-only case (a confirmed inject is its own receipt). A
  plain-name miss now demotes to durable instead of exit 16 in the one
  case where only the process table knew the session.
- **Wake via the native resume door.** `_wake_rung` maps to
  `fno agents resume --message`; the reason tokens are the door's own.
- **Receipts stay doors.** Every receipt renders through the `mail-receipt`
  door (json/demotion/not-landed/attended/debounce), so the wording is
  single-source and the Rust leg stays byte-identical with the frozen
  goldens.
- **dispatch_send stayed in dispatch.py.** The Python orchestrator is now
  dead code there; deleting it was out of this leaf's file list. The Rust
  twin is the live path for the front-routed verb.
- **Thread-viewport force.** A thread-substrate row (no mux/pane ref)
  refuses the force lane rather than resolving a retask viewport; the
  pane-lane gate (option-prompt refusal) runs client-side over a pane
  read.
- **resolve_self_model returns "unknown".** The model provenance stamp is
  a documented gap, not a guess.

## Known cost of the freeze

No local cargo build, test, clippy, or install ran on this tree; CI does
the first compile of ~6,000 new Rust lines plus the parity test. The
fix-forward rounds are expected.

## Expected CI findings, pre-declared

- The CliRunner tests that drive the deleted cmd_send body (test_send.py,
  test_send_deliver_gate.py, test_send_inbox_kind.py, test_to_lead.py,
  test_to_project.py, test_bus_addressed.py, test_store_fallback.py,
  test_mail_cli.py, test_note_notify.py, test_doctor_dead_letter.py) hit
  the forward leaf and fail. The CI red list is the re-lensing work list;
  each guard's contract either moved native (covered by the goldens) or
  belongs to a helper that stayed.
- The diff budget reads large (+7350/-1301) because a port adds the Rust
  leg before the ledger can see the deletions net out; the Python side of
  the ledger is the deleted body plus the job modules.

## CI round one (d6e6705d) and the fix commit

The first compile raised 91 errors and 7 warnings, fixed in `c27d56d1d7`
from the CI log alone (the freeze still held). The bulk was field
visibility after the module split; the real finds underneath: the
switchboard exchange spoke invented RPC params the daemon refuses (the
real contract is to/from/mirror/recipient_identity plus a delivered and
identity_verified verdict), and the graph reads named readers that do not
exist.

Two gaps stay open on purpose, both filed for a follow-up wave:
the harness-store probe leg (store_fallback's claude/codex/opencode
scans) reads clean and empty, so a session that only lives in a harness
store resolves as unknown; and the a2a autonomous relay kickoff stays
Python-side, with the Rust send verb gating to the observed single hop.
The node filing retried after the graph's version conflicts clear.
