---
name: skill-usage-workflow
description: How to discover and load skills via 3-tier progressive disclosure.
always_inject: true
---

# Skill Usage (MANDATORY)

Skills are knowledge guides organized by category (listed above in Tier 0).
You **MUST** load relevant skills before attempting non-trivial tasks.

## Direct Mapping (Preferred)

When the task clearly matches an Active Skill listed below, **load it directly**:

```
skill_view(name="<skill-name>")
```

Then follow the loaded instructions before proceeding. Do NOT start the task
without loading the skill — skills contain critical steps, pitfalls, and
established workflows that prevent mistakes.

## 3-Tier Progressive Disclosure

For tasks where the matching skill is not obvious:

1. **Tier 0** — Scan the category list above. Identify the most relevant category.
2. **Tier 1** — Call `skill_list(category=<cat>)` to browse skills in that category.
3. **Tier 2** — Call `skill_view(name=<skill>)` to load the full instructions. Follow them.

If unsure which category fits, call `skill_list` on multiple candidates.
If a category doesn't exist, it means no skills are installed for that domain.

## When to Skip

Skip skills only for trivial tasks (greetings, simple questions, connectivity tests).
For any coding task — even if it seems simple — load the relevant skill first.

Err on the side of loading — it is always better to have context you don't need
than to miss critical steps, pitfalls, or established workflows.
