# What this probes

This adversarial conformance probe feeds malformed values to pure extension-owned decode boundaries and records the exact exception or disposition. It performs no effects and does not append to the journal.

`smoke()` also drives the exported `IncomingParams.args()` boundary with host-shaped finalization envelopes. The example gate has no live CONTROL host, so these rows verify the frozen Python layer's strict typed outcome handling (`ArgIssue` → `ArgFault`, decoded values, and `Repair` capture), not the host parser's internal production of those envelopes.

## Boundary matrix

| Boundary | Input | Re-observed result |
|---|---|---|
| TML structure | `é<row></bad>` | Refused with `omp.ui.TmlError: unexpected closing tag </bad>`; `at=7`, the UTF-8 byte offset |
| TML unknown tag | `<future-panel><text>x</text></future-panel>` | Accepted with source intact for renderer-side `CustomElement` degradation |
| TML raw controls | `<text>bad\x00\x1b[2A</text>` | Accepted for renderer-side decomposition and control stripping |
| TML text controls | `bad\x00\x1b[2A` through `ui.text` | Accepted after C0 controls are removed locally |
| TML depth | 65 nested `<x>` elements | Refused with `omp.ui.TmlError: TML_MAX_DEPTH exceeded: 65 > 64` |
| TML bytes | 262145 UTF-8 bytes | Refused with `omp.ui.TmlError: TML_MAX_BYTES exceeded: 262145 > 262144` before markup scanning |
| TML source type | non-UTF-8 `bytes` | Refused with `TypeError: Tml.raw expects str` |
| Device args: duplicate canonical | `{"path":"a","path":"b"}` | Finalizer envelope is exposed as `omp.params.ArgFault`, `path=("path",)`, `kind=AMBIGUOUS` |
| Device args: canonical + alias | `{"path":"a","file_path":"b"}` | Finalizer envelope is exposed as `ArgFault(AMBIGUOUS)` at `path` |
| Device args: two aliases | `{"file_path":"a","filename":"b"}` | Finalizer envelope is exposed as `ArgFault(AMBIGUOUS)` at `path` |
| Device args: undeclared top-level key | `{"path":"a","extra":1}` | Accepted and preserved in the canonical object; the old closed-schema expectation was incorrect |
| Device args: coercible wrong type | `{"limit":"42"}` with `Coerce.INTEGER` | Accepted as `limit=42`; `repairs()` records `RepairKind.COERCION` at `limit` |
| Device args: non-coercible wrong type | `{"limit":[]}` with `Coerce.INTEGER` | Finalizer envelope is exposed as `ArgFault(TYPE_MISMATCH)`, `found=array`, with example `42` |
| Selector inverted range | `5-3` | Refused with `omp.urls.SelectorError`; message names end/start ordering |
| Selector zero line | `0` | Refused with `omp.urls.SelectorError`; message names the one-indexed rule |
| Selector negative line | `-1` | Refused with `omp.urls.SelectorError`; message includes the selector grammar |
| URL unknown scheme | `future+transport://resource` | Host-only in this smoke: the mandated example harness installs a FILE-only native URL vocabulary |
| URL encoded selector delimiter | `file://notes%3A5` | Host-only for the same harness-vocabulary reason |
| URL malformed percent triplet | `file://notes%ZZ` | Host-only for the same harness-vocabulary reason |
| Journal undeclared kind | `UndeclaredEntry()` | Host-only append path; `UnknownEntryKind` is now exported |
| Journal canonical JSON | `{"count":"wrong"}` | Accepted by `journal.decode` as `{"count": "wrong"}` because strictness is canonical-byte strictness, not application-schema validation |
| Duration | `not-a-duration` | Host-only in this smoke because the harness supplies an inert native `Duration`; the native constructor maps the malformed unit string to `ValueError` |
| Shortcut empty key | `ctrl+` | Refused with `omp.ui.ShortcutError`; message names the malformed chord |
| Shortcut duplicate modifier | `ctrl+ctrl+x` | Refused with `omp.ui.ShortcutError`; message names the malformed chord |
| Shortcut unknown modifier | `hyper+x` | Refused with `omp.ui.ShortcutError`; message names the malformed chord |
| Reserved parameter | `do_` | Refused with `omp.devices.SchemaError` naming `do_` and the reserved-name rule |
| Reserved parameter | `future_` | Refused with `omp.devices.SchemaError` naming `future_` and the trailing-underscore rule |

## Closure records

1. **TML limits and byte offsets — closed.** The defect was that the local validator neither enforced `TML_MAX_DEPTH` nor reported UTF-8 byte offsets. `crates/py/python/omp/ui/__init__.py:110-120` now computes byte offsets and rejects oversized UTF-8 sources before scanning; `crates/py/python/omp/ui/__init__.py:149-153` rejects the 65th opening element. Re-observation gives `at=7`, depth `65 > 64`, and bytes `262145 > 262144`.

2. **Argument-finalizer surface — closed.** The defect was the absence of the exported typed family, which made every extension-visible finalization outcome untestable. `crates/py/python/omp/params.py:34-108` implements `ArgIssueKind`, `ArgIssue`, and `ArgFault`; `crates/py/python/omp/params.py:263-291` implements repair records; `crates/py/python/omp/params.py:410-623` implements `IncomingParams`; and `crates/py/python/omp/__init__.py:716-750,1662` exports the family. Re-observation shows ambiguous and type-mismatch envelopes become path-addressed `ArgFault`s, while coercion produces the typed value and a retained `Repair`.

3. **Reserved device parameter names — closed.** The defect was that schema extraction accepted `do_` and trailing-underscore names. `crates/py/python/omp/_registry.py:1626-1634` now rejects either form with `SchemaError` before registration. Both specimens are refused and their messages name the offending parameter and rule.

4. **Strict journal surface — closed.** The defect was that `UnknownEntryKind`, `EntryUndecodable`, and `journal.decode` were missing. They now exist at `crates/py/python/omp/journal.py:79-132,183-207` and are publicly exported. Canonical `{"count":"wrong"}` re-decodes unchanged. The earlier probe expectation that `decode` validates an application payload schema was wrong: the recorded Revision 2 contract at `docs/py/09-journal.md:445-461` defines strictness as exact canonical-byte decoding into plain Python values.

5. **Duration classification — probe expectation corrected, no product defect.** The old finding conflated direct value construction with manifest admission. `docs/py/00-overview.md:716-727` assigns `ManifestError` to an unparseable config value at ADMIT; direct `Duration(...)` remains a value constructor, whose malformed string branch maps to `ValueError` at `crates/py/src/bindings.rs:159-166`. No exception suppression or probe weakening is involved.

6. **Top-level extra-key classification — probe expectation corrected.** The old row called a top-level `extra` member a closed-schema violation. The recorded strict-finalization rule at `docs/py/03-params.md:444-445` explicitly tolerates and preserves unknown top-level members; `additional_properties=True` governs declared dict-shaped fields (`docs/py/03-params.md:474-480`). The row now asserts preservation instead of a refusal.

## Smoke

The smoke asserts every locally reachable refusal, the exact TML ceilings and UTF-8 byte offset, typed finalizer-envelope handling and repair retention, canonical journal decode, reserved-name activation failures, selectors, shortcuts, and control stripping. The URL vocabulary, native `Duration`, journal append, and production finalizer parser remain host/native-owned under the mandated no-I/O example harness and are labeled host-only rather than mocked as local execution.

**Still-open findings: none.**
