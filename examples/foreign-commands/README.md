# Foreign commands

## What the pi original did

`pi-unify-cmd` discovered slash-command files written for Claude Code, Codex, Gemini CLI, and similar agents, adapted their metadata, and exposed their prompt bodies as local slash commands.

## The omp shape

This port reads only configured `.claude/commands`, `.codex/prompts`, and `.gemini/commands` trees through `omp.env`; it parses bounded UTF-8 Markdown frontmatter and prompt bodies and never imports or executes foreign Python, JavaScript, shell, plugin manifests, or hooks. Descriptions carry their layout and source path. `${PLUGIN_ROOT}` expansion is allowed only when its normalized and canonical result remains under the configured plugin root, and every listed entry is canonicalized and checked against its scan root before reading. Unrecognized roots produce `W-FOREIGN-ROOT` and are skipped; escapes produce `W-PATH-ESCAPE` and are refused.

This is the content-only adapter boundary consistent with `docs/py/14-deploy.md` §6.6: foreign `.claude`/`.codex`/`.gemini` extension roots are reported as `W-FOREIGN-ROOT` and are never loaded as Python distributions (lines 3096-3100). Only prompt text inside an explicitly recognized command layout is data here; foreign code is never an extension load. Command handlers return `omp.ui.Prompt`, retaining only non-recursive `$ARGUMENTS`, `$@`, and positional `$N` text substitution from `docs/py/07-ui.md` §6.6 (lines 2542-2545).

The original's ambient discovery and executable adapter/plugin mechanisms are deleted. Roots must be explicit settings, Environment reads require `env.fs.read` plus `env.doc.read`, and no foreign source can acquire a code path.

## Gaps

- Runtime command registration is not wireable. `omp.command` / `omp.ui.command` writes only to the import-time collector at `crates/py/python/omp/ui/__init__.py:820-838`, but configured settings and `omp.env` are available only in callback context. The collector is frozen before activation (`docs/py/00-overview.md:1161-1169`), and `RegisterUi` rejects commands absent from the manifest declaration table (`docs/py/07-ui.md:2305-2316`). Consequently `load_foreign_commands` can discover and locally decorate handlers, and the smoke proves that logic, but a real activated host cannot receive those dynamic command declarations. The missing frozen surface is a host-mediated `register_command(name, description, handler)` operation (or an equivalent pre-FREEZE content-import declaration) that admits configured command names and updates dispatch after discovery.
- `docs/py/14-deploy.md` §6.6 does not actually state “content only, never code.” It says foreign extension roots are reported and not loaded (lines 3096-3100), while §3.3.2 separately says compatibility precedence applies to skills, rules, agents, and prompts rather than extensions (lines 860-865). The requested content-only command-import rule is compatible with those passages, but the exact normative sentence requested by this port is absent and should be added to distinguish data adapters from extension discovery.
