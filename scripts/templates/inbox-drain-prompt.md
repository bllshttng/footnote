# Inbox Drain Prompt

You are draining an inbox in a headless `claude -p --bare` session. Your job:

1. Run `fno mail unread --json` to list unread threads in the local
   project's `inbox/` directory.
2. Run `fno mail drain --json --max 10`. The drain dispatches each
   thread by its `kind`:
   - `heads-up` -> LLM triage; create a graph node on success; mark thread read.
   - `question` -> drop a wake-signal; **leave thread UNREAD** for a human to handle.
   - `fyi` (default) -> log `inbox_fyi` event to `convo-signals.jsonl`; mark read.
   - `fyi` with `persist_to_memory: true` -> write a recipient memory file; mark read.
3. Capture the JSON list of `DrainResult` entries from stdout. Each
   entry has `thread_id`, `kind`, `action`, `thread_path`, optional
   `node_id` / `memory_path` / `error`.
4. After processing, exit. Do NOT iterate beyond `--max-turns` (set by
   the caller; default 12).

Constraints:
- Use ONLY `Bash(fno *)` and `Read` tools. Do not edit code, do not call git.
- If `fno` exits non-zero, log the failure to stderr and continue.
- Do not invoke `/think`, `/blueprint`, or any other slash command. Bare
  mode does not load skills.
- Deprecated kinds (`notification`, `lesson`, `answer`, `complete`) no
  longer exist. The CLI rejects them at send time.

## Per-thread layout

Each thread is one file at
`~/your-vault/internal/agents/{recipient}/inbox/{YYYY-MM-DD}-{slug}.md` with
YAML frontmatter (`thread_id`, `from`, `to`, `kind`, `created`,
`read_at`, optional `replies_to` / `persist_to_memory` / refs) and one
or more `## msg-{id} · {ts} · from:{sender}` body blocks. Replies
append to the same file rather than creating a new one.
