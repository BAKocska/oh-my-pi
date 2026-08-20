# Secret guard

## What the pi original did

[`@josephyoung/pi-heimdall`](../../.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md#L273) guarded against accidental secret exposure, protected `.env` files, enforced command policies, and attempted to sandbox shell commands.

## The omp shape

This port declares only extension-configured value rules from `settings.secret_rules`; the common credential detectors remain Core data. Static shell reads are decided from the canonical `BashIR.reads` `PathRef` facts, and the Core `read` tool is checked through its resolved `CoreTool` target. A proven `.env*` or key-file read is denied in PRECHECK. Parse failures, truncated analyses, dynamic evaluation, and dynamic read paths abstain in PRECHECK and return `RequireApproval(ApprovalSpec(...))` immediately from APPROVAL, so an indirect command such as `cat $(find . -name '*.env')` cannot become an implicit allow. The durable ticket is Core-owned; Python never waits for the answer (`docs/py/05-hooks.md` §§3.2–3.4 and `docs/py/06-policy.md` §“The admission subject”).

There is deliberately no output-scrubbing regex pipeline here. `BashIR.source`, `BashArg.text`, and `PathRef.lexical` remain unredacted policy truth, while Core masks every policy-emitted reason, approval field, and journal record (`docs/py/06-policy.md` §“Secret redaction interaction”, lines 1554–1596). The OBSERVE hook calls that same Core masker only to increment a telemetry counter when a call contains a redaction hit; it neither returns nor stores the masked text. Built-in GitHub, GitLab, OpenAI, and other credential patterns are not copied into the extension.

The original sandbox contribution is deleted. The v1 ruling explicitly ships no sandbox enforcement and states that extensions are not a security boundary (`docs/py/06-policy.md` §“Open questions”, lines 2570–2580), so this worked example is an admission policy only.

Settings are JSON strings because the manifest schema is scalar: `protected_paths` supplies path globs and `secret_rules` supplies objects with `pattern`, `kind`, `mode`, `label`, and optional `replacement` fields.

## Gaps

- `omp.secrets`, `omp.secrets.declare`, `omp.secrets.mask`, `omp.SecretRule`, `omp.SecretKind`, and `omp.SecretMode` are documented by `docs/py/06-policy.md` §“Secret redaction interaction” (lines 1554–1575) but absent from the frozen layer: `crates/py/python/omp/__init__.py:285-299` imports the public submodules without `secrets`, and `crates/py/python/omp/__init__.py:940-1031` exports neither the module nor the three rule types. There is no `crates/py/python/omp/secrets.py`. Until those symbols land, activation and the redaction-hit observer cannot execute against the real frozen package.
