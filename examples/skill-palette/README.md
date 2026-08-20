# Skill palette

## What the pi original did

`pi-skill-palette` provided a VS Code-style command palette for selecting and applying skills (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:286`). It combined skill discovery, picker UX, application, and recency in extension code.

## The omp shape

Skills are content, and discovery is core-owned. The two bundled `SKILL.md` files are declared by `[[skills]]` rows in `omp.toml`; the extension does not scan skill directories or inject their contents. `/palette` is only UX over that inventory: it opens the native `ui.select` picker with overlay sizing, ranks choices by typed USER-scoped `omp.state` entries, and returns `ui.Prompt` naming the selected `skill://` resource. Core resolves that resource and supplies the skill to the model. `docs/py/07-ui.md` §4.10 documents `select` and `SelectItem`; §4.15 documents `ui.Prompt`. `docs/py/08-context.md:778-779` makes the stable skills slot a core-rendered inventory of `skill://<name>` references, and `docs/py/09-journal.md` §`omp.state` defines USER state as durable across the principal's projects.

Deleted mechanisms are extension-side filesystem discovery, arbitrary TUI components, direct prompt mutation, and a private JSON recency file. The palette stores only selection events; the journal remains the sole durable truth.

## Gaps

None — every symbol this port needs is frozen.
