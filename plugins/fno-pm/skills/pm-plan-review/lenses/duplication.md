# duplication

Does the code this plan adds already exist, or can an existing module be extended, configured, or refactored so the feature falls out?

To fail: name a module and a relationship, such as "already implements X", "owns Y", or "is the second copy of Z", with a quoted line from the context. Advice to consider consolidation is not an answer. A reader that over-reports duplication turns every feature into a refactor.

Pass: the context names no existing module in that relationship, or the plan already extends it.

Thin evidence: pass.
