# sibling_callers

The context from code below lists the files the plan's named symbols appear in. Fail when the plan changes a shared function and names only the caller the report named, with other callers of that name in the context below. Pass when the plan names the other callers and says what each one does with the change, or shows there are none.

Thin evidence: pass, because a reader with no code context cannot know that a caller was missed.
