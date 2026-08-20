---
name: repository-rules
description: Always-applied engineering and decision-record rules for this repository.
alwaysApply: true
hide: false
disable-model-invocation: true
---

# Repository rules

1. Preserve the repository's existing architecture and naming before introducing a new pattern.
2. Fix causes rather than suppressing symptoms, and remove obsolete paths when replacing behavior.
3. Keep changes bounded to the requested behavior; do not add speculative abstractions or dependencies.
4. Run the manifest-declared conformity checks before declaring a change complete.
5. Treat files matched by the declared decision paths as durable decisions: update them only intentionally, and supersede rather than silently contradict prior decisions.
6. Keep rule prose declarative. A rule may describe a command, but rule text itself is never imported or executed as Python.
