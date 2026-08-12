---
name: think
description: "Investigate a question against primary sources - repo source code, specs, first-party APIs - and write cited findings to one Markdown file. Use when: 'research this', 'think through this', 'how does this break' (what-if), 'several lenses on this' (panel), 'what owns this' (class). Prefix `bg` or `subagent` to run it off the main thread."
argument-hint: "[bg|subagent] [what-if|panel|class] <question | node-id>"
---

# Think

Research, not planning. `/fno:blueprint` owns the plan and its approval.

Read the argument left to right: an optional substrate token, an optional brief token, then the question (node id, slug, or free text).

**Substrate.** `bg` hands the question to a background worker (`fno think dispatch <node>`) and returns its receipt immediately. `subagent` runs the process below in a subagent. Neither token: run it here, inline.

**Brief.** The question put to the sources. The process never varies; only this row does.

| brief | the question |
|---|---|
| (none) | what is true about X? |
| `what-if` | how does X break? Read the real error paths, boundaries, and concurrency, and cite each failure mode you FIND; an imagined one does not count. |
| `panel` | what does X look like from several lenses? Each lens reads the code it owns and cites it: the security lens reads the auth path, the ops lens reads the failure path. |
| `class` | what owns X, and what are all the sites that decide it? Enumerate every site, then say which ones disagree. |

## The process

1. **Ground.** Run `fno think inspect "<question>" --json` and read the whole receipt. It reports source status, never a verdict: an unavailable search is not "nothing found", and a stale schema is not "no DB change". Inspect what it cites before deciding anything is relevant.
2. **Investigate primary sources** - source code, specs, first-party APIs - never a secondary write-up of them. Follow every claim back to the source that owns it. Every claim you write carries a `file:line` or a URL; a claim you could not trace is written down as untraced.
3. **Write one Markdown file** at the path `fno plan path --slug "<slug>" [--node <id>]` prints, citing each claim's source. List only sources you actually read.

Print the file path and stop. Turning findings into a plan is `/fno:blueprint <path>`.
