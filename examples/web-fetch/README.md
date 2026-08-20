# Web fetch

## What the pi original did

`@mrclrchtr/supi-web` provided three tools for turning public pages into Markdown and searching or fetching focused Context7 library documentation. Each tool carried custom call/result renderers, truncated its own output, and returned a readable temporary artifact path for the remainder.

## The omp shape

This port keeps one soft `fetch_web` device because the worked port in `docs/py/02-verdicts.md` §1 defines the fetch leaf but no `docs_lookup` sub-device. The executor returns the complete structured truth—URL, HTTP status, title, and extracted text—while `prompt()` alone sizes the model-facing view to `PromptCaps.maximum_text_bytes` through `omp.Budget`.

The central spill gate deletes all `mkdtemp` and temporary-path returns: when the payload exceeds the inline limit, the harness preserves it whole and substitutes a `BlobRef` automatically. There is no extension-owned spill lifecycle or truncation. The device runs at `place="env"` and uses `urllib` directly; `env.net` is still declared, but its allowlist enforcement ships nothing in v1 under the 2026-08-19 no-sandbox ruling, so this extension is not a security boundary.

## Gaps

None.
