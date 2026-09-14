# Blueprint judge lenses (x-9983)

You are a reader with no stake in this plan. You grade one question only. First quote the plan lines you rely on. Then give a reason in one or two sentences. End with one line: VERDICT: pass, fail or unknown. Do not grade format, headings, length or style. If the plan makes the question moot, pass and say why. An answer of none is a claim. Judge it like any other.

## persona

Does a real person in this fleet hit the problem, and does the plan show where that evidence came from?

Pass: the plan names who is hit and a present cost tied to a source the plan cites. The who is one of: the operator, a crowned king, a worker session, a plugin user.

Fail: a generic who ("users"). A cost with no source. A person who hits a different problem. Nobody named at all, which means the plan was written for no one.

Thin evidence: unknown.

## surface_fit

Does this plan build a new surface where an existing verb, skill or config already covers most of the need?

Pass: the plan extends, configures or composes a listed surface. Or it names the surface it declined and why.

Fail: a surface in the context covers most of the need, and the plan neither names it nor says why not.

Thin evidence: pass.

## uncovered_case

Is there a realistic input or state that breaks the design as written and that the plan neither handles nor names?

Fail: name the case and quote the plan step it breaks. A reachable case includes two sessions at once, a moved index or branch, and an empty or stale input the plan already relies on.

Pass: every case you can reach from the plan's own statements is handled or named.

Thin evidence: unknown.

## deletable

Can you delete one task, file, flag or config and still ship the stated goal?

Fail: name it, quote it, and name the goal it does not serve.

Pass: every piece serves the stated goal, or the plan already names its own cut.

Thin evidence: pass.

## duplication

Does the code this plan adds already exist, or can an existing module be extended, configured, or refactored so the feature falls out?

To fail: name a module and a relationship, such as "already implements X", "owns Y", or "is the second copy of Z", with a quoted line from the context. Advice to consider consolidation is not an answer. A reader that over-reports duplication turns every feature into a refactor.

Pass: the context names no existing module in that relationship, or the plan already extends it.

Thin evidence: pass.
