---
name: file-reading-strategy
description: Guidelines for efficient file reading — adapt strategy to task context.
always_inject: true
---

# File Reading Strategy (IMPORTANT)

**Adapt your reading strategy to the task at hand.** Different tasks require different approaches — don't apply a one-size-fits-all "read less" policy.

## When to Read Files Fully

Read the entire file (use a large `limit` or multiple reads to cover the whole file) when the task requires **global understanding**:

- **Reviewing / understanding a file**: User asks to "read this file", "explain this code", "walk me through this", "summarize this article"
- **Refactoring**: Need to understand the full structure before making changes
- **Code review**: Need to see the complete implementation
- **Reading documentation / articles / configs**: The user wants the full content
- **Small files (<500 lines)**: Just read them fully — the overhead of multiple partial reads costs more than reading once

**For these tasks, reading the whole file in one or two calls is far more efficient than piecemeal reading that requires multiple LLM round-trips.** Each extra LLM round-trip costs ~2-5 seconds of latency and additional tokens — always cheaper to read more in a single call.

## When to Use Targeted Reading

Use `grep` + `offset`/`context` for **surgical, lookup-oriented tasks**:

1. **Finding specific code**: "Where is the login handler?", "Find the bug in the parsing logic"
2. **Checking a specific function**: "What does `parse_config` return?"
3. **Verifying a fix**: After making a change, grep to confirm it's correct
4. **Large unfamiliar codebases**: When you need to locate relevant code in a huge file (>1000 lines) and don't know the structure yet

For these tasks: `grep` to find the line, then `read(path, offset=N, context=15)` to read around it.

## Practical Guidelines

- **Default `limit` is 500 lines, max is 2000.** Use `limit=2000` when you need to read most of a file — this is often cheaper than 2+ partial reads.
- **When reading a file for understanding**, start with a generous read (e.g. `limit=2000`). If the file is longer, continue with subsequent offsets. Don't artificially limit yourself to tiny chunks.
- **Avoid the anti-pattern of reading 50-100 lines at a time.** This creates multiple LLM round-trips and is extremely slow. Read 500-2000 lines at a time when you need the content.
- **Batch file reads**: If you need to read multiple files for understanding, read them all in parallel in a single response.
- **When the user says "read this file" or "show me this code"**, they expect to see the whole thing — read it fully, not just a snippet.
