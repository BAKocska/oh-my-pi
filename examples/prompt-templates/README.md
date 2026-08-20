## What the pi original did

`pi-prompt-template-model` loaded frontmatter-configured Markdown prompts, exposed them as slash commands, selected model roles and thinking levels, and ran chained or looping subagents. Its boomerang loops used session-tree navigation and cross-extension events (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:62`).

## The omp shape

Templates remain workspace content under the directory selected by `[settings.templates_dir]` (default `.omp/templates`); they are not imported Python modules. The single `/tpl` decorator command discovers bounded `*.md` entries with `omp.env.find.files`, reads each body through `EnvPath.read_text`, and turns nested paths such as `review/security.md` into the subcommand name `review:security`. These are DATA-plane document reads rather than local `open()` calls, so they work in remote environments (`docs/py/11-env.md` §§“Typed locations” and “Workspace search”). Dynamic command completion returns template names with each template's `args` value as its usage ghost (`docs/py/07-ui.md` §4.15 “Argument completion”).

A template uses scalar frontmatter followed by Markdown:

```markdown
---
description: Review a change in two passes
role: reviewer
thinking: hi
args: <path> [focus]
submit: false
chain: 2
---
Review $1. Pay special attention to $ARGUMENTS.
```

`role` names the bundled or project agent definition, `thinking` is `off`, `lo`, `med`, or `hi`, `args` supplies completion ghost text, `submit` chooses immediate prompt injection (`true`) or composer population via `ui.Prompt(..., submit=False)`, and `chain` is a bounded count from zero through eight. A nonzero chain spawns supervised leaf `SubagentSpec` runs sequentially with the frontmatter role and thinking level; each completed result becomes the next step's task, and the last result follows the template's `submit` choice (`docs/py/12-agents.md` §§`SubagentSpec` and `agents.spawn`). This replaces `ctx.navigateTree`, `pi.events`, and child processes with Core-owned child handles and attribution.

Expansion implements `$1`, `$2`, `$@`, and `$ARGUMENTS` in one regex pass. Inserted argument text is never scanned again, and unreferenced arguments are appended after the prompt. Those are the frozen roadmap rules: quote-aware tokenization, positional/all-argument substitution, non-recursion, and trailing-argument append (`.plan/feature-map/ROADMAP.md:581-583`; also `docs/py/07-ui.md:1639-1649`).

Handlebars is permanently absent from decorator commands: `docs/py/07-ui.md` §6.4 explicitly declines a second template language for Python handlers. Omp's native file-based Markdown commands keep Handlebars because content-only commands cannot compute, but this `/tpl` decorator port deliberately supports only the roadmap dollar substitutions above.

## Gaps

None.
