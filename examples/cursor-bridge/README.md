## What the pi original did

`@rahularya01/pi-cursor` registered Cursor as a native provider and implemented Cursor's Connect/protobuf-over-HTTP/2 streaming protocol, including stateful agent frames and mid-stream interaction queries. It silently harvested credentials through four tiers: the macOS Keychain via `security find-generic-password -s cursor-access-token`; VS Code's `globalStorage/state.vscdb` SQLite database; the Linux equivalents; and the same desktop paths inside WSL under `/mnt/c/Users/<user>/AppData/…`.

## The omp shape

This is the class (c) provider shape from `docs/py/13-inference.md` §§“Providers are data; code is the cold path only”, “Credentials: scoped, and secret-free by default”, and “`@rahularya01/pi-cursor` — class (c)”. `@omp.provider` declares an ordinary OpenAI-chat route at the bridge's loopback endpoint. `extension_activate` atomically ensures the Environment-owned `cursor-bridge` named process with readiness and restart declarations, mints its short-lived `bridge` facet through the declared `MintScopedToken`-class credential flow, and injects that value through the process secret channel. The bridge alone translates the local supported dialect to Cursor's foreign protocol. **ZERO Python bytes on the token path.** There is no Python request handler, stream wrapper, frame decoder, or response parser.

The original four-tier Keychain/SQLite/Linux/WSL harvest cascade is deleted rather than ported. Authentication comes from the declared Cursor session credential after an explicit `/login cursor` flow (or an explicitly granted import), and the proxy receives only the facet-scoped bridge token, never the stored credential. The open answer recorded in `docs/py/13-inference.md` §“Open questions” remains explicit: omp does not represent that proprietary continuation state belongs to a supervised bridge, so a bridge restart must be treated as requiring reseed unless and until that contract is resolved.

## Gaps

- `omp.creds.mint_scoped` and its `ScopedToken` result are required by `docs/py/13-inference.md` §“Credentials” (the `MintScopedToken` proxy flow at lines 1525-1564), but the frozen root surface has no `creds` facade in `crates/py/python/omp/__init__.py:442-492,930-932`.
- `omp.env.Process.send_secret` (and the worked port's dynamic `Process.endpoint`) are required by `docs/py/13-inference.md` §“`@rahularya01/pi-cursor` — class (c)” at lines 1848-1869, but the frozen handle ends at `send`, `signal`, and `stop` in `crates/py/python/omp/env.py:755-786`. The port retains the documented secret-channel call and uses a fixed loopback endpoint; it does not substitute stdin or expose the stored credential.
