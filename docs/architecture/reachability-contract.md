# The reachability contract

The module docstring of `cli/src/fno/agents/reachability.py` opened as the module's own contract note; it lives here now so the module ships code, not essays. Nothing below changed in content; the rules are restated in the same words the code was written against.

## One question per word

Six surfaces used to answer "is this agent live" and they answered four different questions, all rendering into the same word: `list`/`truth` asked has it produced output recently (transcript), `status` asked what lifecycle state we last WROTE (stored enum), `top` asked is there an OS process (process census), `mail-inject` asked can I put text in front of it right now (control socket), and `peek` asked nothing about liveness at all (it reads transcript CONTENT). They disagreed because they measure different things, so collapsing them into one word destroyed information rather than adding it. The module keeps the one question a supervisor actually asks (is the agent REACHABLE) and makes every surface report the BASIS it answered from, so a reader can tell which question was answered.

## The rules, all load-bearing

Positive evidence comes only from transcript activity age; no other signal may raise a verdict toward `reachable` (the registry's own PID SEMANTICS rule generalized: a live process may still be unreachable, so process liveness can falsify and can never establish). Falsifiers are monotone toward `unreachable`; one may lower a verdict and never raise one. `unknown` is terminal and no consumer may coerce it to either pole. Basis and age are part of the value; a bare `live` is unprintable. Positive evidence expires: transcript activity certifies liveness only inside `TRANSCRIPT_EVIDENCE_S`, past which an active tail demotes to `unknown` (basis `stale-transcript`), never `unreachable` (x-c1a3).

## Why silence is never `unreachable`

This registry lists REACHABLE agents; it is not a process table. "Orphaned" means unreachable, not dead, so a row is never condemned for being quiet. A transcript can only ever supply POSITIVE evidence of activity; its absence is absence of evidence, not evidence of absence. A silent row with no falsifier available (89 percent of rows carry no pid at all) resolves `unknown` with its age attached, never `unreachable`; only an affirmative falsifier condemns a row. That is what makes the destructive rule un-rederivable rather than merely remembered: absence of a pane, absence of a pid, and absence of recent output all contribute exactly nothing, so "no pane means safe to reap" cannot be reconstructed by editing a threshold.

Note the asymmetry that keeps this honest: a row with no pane recorded, or a mux that cannot answer, is an ABSENCE and condemns nothing; a mux affirmatively reporting that a pane exited is EVIDENCE and does condemn. Suppressing a falsifier is never the fix for a wrong falsifier: the answer is to consult the right authority, not to stop asking.

## Progress is a second axis, never a fourth reachability value

A worker taking its turn, a worker that parked after finishing, and a worker alive but unable to think (handed a model its endpoint cannot serve) all classify `reachable` here, correctly, because all three ARE reachable. `classify_progress` answers the orthogonal question (advancing, awaiting the operator, parked, or refused) in its own `progress`/`progress_basis` fields, mirroring the verdict-plus-basis shape rather than widening `WIRE_STATUS` to a fourth word.

## A reading about one artifact is not a verdict about the agent

A missing transcript file proves that a file is missing at that path, nothing else. Every probe in the module follows that rule (`pid_falsifier` and `pane_falsifier` each return None, not a death verdict, when their own evidence is absent or unreadable), and `classify_progress` follows it for progress: an unresolved or missing transcript classifies `unknown` on both axes, never `refused` and never `parked`. Only the classifiers in the module may answer either axis; a reader that derives liveness from bare file existence elsewhere is rebuilding the same mistake one layer over.

## Why transcript age is necessary but never sufficient

It is the only surface that never lied (argv, pid, the daemon record, and state.json were each caught lying about a live session in one evening; see `fno.agents.session_truth`). It has two limits, and both are why it is the sole POSITIVE term rather than the whole answer. Resolution: the liveness axis is a low-pass filter with a two-hour window, so it cannot separate "dead 43 minutes" from "thinking for 43 seconds", and every false-live lives in that gap, which is why the age always rides along. And it measures FILE WRITES, not conversation: a transcript can be touched by a stub write, a resume attempt, or a tool result with no live session behind it.
