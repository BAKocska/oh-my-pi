# Prompt templating

`omp.scribe` is the template engine omp itself renders every prompt with — the system head sections, markdown prompt assets, recovery and steering fills, subagent composition, and user command templates (`crates/scribe`, `omp-scribe`) — exposed to extension Python. Use it wherever an extension composes prompt text: a `@omp.prompt_slot` body ([08-context.md](08-context.md)), a device docs body ([01-devices.md](01-devices.md)), a `CustomSummary` ([08-context.md](08-context.md)), or a subagent prompt ([12-agents.md](12-agents.md)).

Rendering is **pure**: output depends only on the template source and the props. No clock, no environment, no randomness, no I/O — a render performs no CONTROL or DATA operation, is legal in every `omp.InvocationPhase`, and renders identical bytes for identical props on every host. That is not a style preference: a prompt-slot body must survive the harness's double-render volatility check (`omp.prompts.VolatilePrompt`), and text built with `omp.scribe` passes it by construction.

## The surface

```python
import omp.scribe

template = omp.scribe.Template(
    "{% if findings %}{{ findings | length | pluralize('finding') }}:\n"
    "{{ findings | bullets }}{% endif %}",
    name="lint-summary",
)
text = template.render({"findings": ["unused import", "shadowed name"]})
```

### `omp.scribe.Template(source: str, *, name: str = "template")`

Compiles `source`, validating syntax and that every referenced filter, function, and block is a registered builtin. Compilation failures raise `TemplateError` immediately — never at render time. The compiled object is immutable and safe to share across threads; compile once at import or activation, render per use.

- `template.render(props: dict[str, Any] | None = None) -> str` — renders with `props`. Raises `TemplateError` when an undefined value reaches a strict sink or an operation is applied to the wrong shape.
- `template.name` — the name supplied at compile time, used in error messages.
- `template.referenced_keys` — sorted tuple of top-level prop names the template reads (static analysis; loop variables and `set` bindings excluded). Use it to build exactly the props a template needs.

### `omp.scribe.render(source, props=None, *, name="template") -> str`

One-shot compile-and-render. Compiles on every call; hold a `Template` for repeated renders.

### `omp.scribe.canonicalize(text: str) -> str`

The post-render canonicalization pass omp applies to system prompts before hashing and journaling. Outside code fences and inline code spans: strips HTML comments, trims trailing whitespace, collapses blank-line runs to one, compacts GFM table separators, and aliases RFC 2119 phrasing (`MUST NOT` → `NEVER`, ASCII arrow/operator digraphs → Unicode). **Opt-in by design** — `Template.render` never applies it. omp canonicalizes system prompts; command templates are rendered verbatim. Match that split: canonicalize text destined for a stable prompt prefix, leave user-authored command text alone.

### `omp.scribe.TemplateError`

Raised for every compile- and render-time failure; subclass of `omp.OmpError`. The message carries the template name, the 1-based `line:col`, and an underlined source snippet:

```text
undefined value `user.name` in `lint-summary` at 2:11
{{ user.name }}
   ^~~~~~~~~
```

## Props

`render` takes a `dict` with `str` keys. Values are the JSON shape: `None`, `bool`, `int` (64-bit signed; larger raises `OverflowError`), `float`, `str`, `list`/`tuple`, and nested `dict` with `str` keys. Any other type — including a dataclass or a `Duration` — raises `TypeError`; convert explicitly, so the template sees exactly what the model will.

Layering is ordinary dict merge. omp's own subagent bags are a child-wins **shallow** overlay — a patch key replaces the parent value wholesale, never deep-merges — and `{**parent, **patch}` is exactly that semantics in Python.

## The language

Delimiters are fixed: `{{ expr }}` emits, `{% statement %}` controls, `{# comment #}` disappears. Literal `{{` goes through `{% raw %}…{% endraw %}`.

| Form | Notes |
|---|---|
| `{% if e %} … {% elif e %} … {% else %} … {% endif %}` | conditions treat missing keys as falsy |
| `{% for x in e %} … {% endfor %}` | binds `loop.index0`, `loop.first`, `loop.last`; maps iterate `[key, value]` pairs in key order |
| `{% set x = e %}` | render-scoped assignment |
| `{% raw %} … {% endraw %}` | verbatim text |
| `{% xml "tag" %} … {% endxml %}` | `<tag>…</tag>` wrapper, elided when the trimmed body is empty |
| `{% codeblock "lang" %} … {% endcodeblock %}` | trimmed body in a fenced code block |

Expressions: literals (`"s"`, ints, floats, `true`/`false`/`none`), paths `a.b.c`, indexing `a[0]` / `a["k"]`, optional chaining `a?.b` (missing → `none`), comparisons `== != < <= > >=` (int/float coerce), `and or not`, membership `x in coll` (list contains / map has key / substring), string concat `~`, `+ -` on numbers, ternary `a if cond else b`, filter pipes `e | f(args)`, and function calls `f(args)`.

**Undefined semantics are fixed.** A missing lookup is *falsy* inside `if`/ternary conditions and `in`; it is a `TemplateError` with a span when emitted by `{{ }}`, concatenated with `~`, iterated, ordered, or passed to any filter other than `default`. `?.` makes the rest of an access chain lenient. This is the deliberate middle ground: control flow tolerates absence, output never silently swallows it.

**Whitespace is markdown-first.** A line holding only a statement or comment disappears with its newline, so control flow never leaves blank scars in prompt markdown. `{{- -}}` / `{%- -%}` trim all adjacent whitespace including newlines; one trailing template newline is dropped.

Display: `none` renders empty, floats use their natural form, lists and maps render as compact JSON.

## Builtins — the whole helper set

- Filters: `join(sep=", ")`, `length`, `default(fallback)` (replaces missing *and* `none`), `pluralize(singular, plural?)` → `"3 items"`, `json`, `escape_xml`, `trim`, `indent(n, first=true)` (`first=false` skips the first line, for embedding after a label), `bullets(marker="- ")`.
- Functions: `table(rows, headers?)` → GFM table (first row is the header when `headers` is omitted).
- Blocks: `xml`, `codeblock`.

There is **no custom-helper registration**. The registry is the fixed builtin set, shared engine-wide, and a Python callback inside a render would put arbitrary extension code — with arbitrary latency and arbitrary nondeterminism — inside a pass whose whole contract is purity. Compute in Python, pass the result as a prop. A helper the builtins genuinely cannot express is a feature request against `crates/scribe`, where it ships deterministic and allocation-aware for every consumer at once.

A template referencing an unknown filter, function, or block fails at **compile** time (`TemplateError`), not mid-render.
