---
name: palette-focused-review
description: Review a change by prioritizing correctness, regressions, and evidence.
---

# Focused review

Review the requested change rather than redesigning the surrounding system.

1. State the observable contract the change must preserve.
2. Trace affected inputs through their real call paths.
3. Report correctness, security, or regression findings in severity order with exact evidence.
4. Distinguish observed facts from inference.
5. If no material defect remains, say so and name the verification performed.
