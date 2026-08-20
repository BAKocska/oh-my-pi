# Vision describe

## What the pi original did

`@dougbots/pi-image-loader` read a local image and pasted a base64 representation into model-facing prose. `@smoose/pi-vision` let a text-only chat model ask a separately configured local Codex or Agy CLI to recognize the image. Together they supplied image context, but they duplicated provider selection and moved binary media through mechanisms that the model could not reliably interpret.

## The omp shape

The soft `describe_image` device accepts an `omp.EnvPath`, reads its bytes through the Environment, identifies PNG, JPEG, GIF, or WebP from the byte signature, and stores the content in `omp.env.blobs`. The inference request contains `omp.Part.text(...)` and a real `omp.BlobPart` made by `omp.Part.blob(omp.Spill(image, media_type="image/..."), ...)`; it never emits base64 prose. This follows `docs/py/11-env.md` §“Typed locations” and §“Blobs — omp.env.blobs”, `docs/py/02-verdicts.md` §“Projecting for the model”, and `docs/py/04-placement.md` §“omp.Spill”. The image `BlobRef`, digest, media type, and cache provenance remain typed verdict details while only the returned description is projected.

Inference goes only through `omp.agents.completion(role="vision")`. `vision` is a deployment-mapped capability role, not a vendor/model id; callers cannot supply a concrete model. `docs/py/12-agents.md:641-643` explicitly requires role selectors and rejects hardcoded vendor models as non-portable. The original CLI-provider selection and subprocess protocol are deleted.

A successful description is written once beneath `await omp.state_dir()` at a filename keyed by the `BlobRef.hex` content digest. The entry is stamped `truth=false`, validated before reuse, and atomically replaced; deleting or corrupting it merely causes another completion. The source image and returned verdict remain truth, while the env-colocated disk file is only a rebuildable content index, following `docs/py/09-journal.md:1230-1266`.

## Gaps

- `omp.agents.completion` has no frozen image-part request contract: `crates/py/python/omp/agents.py:155-167` types `prompt` only as `object`, while `docs/py/12-agents.md:633-652` defines no accepted `Part` sequence or image-capability behavior. `docs/py/02-verdicts.md:632-644` defines `BlobPart` for model-facing projection, but nothing freezes how that part reaches a one-shot completion. The example passes `(TextPart, BlobPart)` through the current generic parameter so the intended request is explicit, but a host dispatch arm cannot implement this port interoperably until the completion input shape is frozen.
- `omp.Spill` is a frozen-versus-docs signature divergence used by the media part: `crates/py/python/omp/placement.py:114-118` names its bytes field `value`, whereas `docs/py/04-placement.md:899-906` documents the constructor field as `buf`. This example uses the positional constructor, which is valid in the freeze, without inventing a compatibility alias.
