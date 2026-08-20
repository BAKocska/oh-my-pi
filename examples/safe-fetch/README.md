# Safe fetch

## What the pi original did

`@juicesharp/rpiv-web-tools` added web search and page fetching with selectable hosted or self-hosted providers. Its defensive fetch path refused private, loopback, and cloud-metadata destinations before retrieving a page (`catalog.md`, web/research entry).

## The omp shape

This port keeps only that defensive half as the soft `fetch_page` device. It parses an HTTP(S) URL, resolves the hostname, validates every returned address, and only then opens a connection pinned to one of those validated addresses. Loopback, link-local, RFC 1918/ULA private, multicast, unspecified, reserved, and known cloud-metadata destinations produce a typed `FetchPageFault` naming the rule. Redirects are handled one response at a time, so the next hostname is resolved and checked before the next request; automatic redirects cannot create the classic public-to-private bypass. The response body is capped at 2 MiB before HTML is converted to Markdown.

The manifest requests `env.net`, the body runs at `place="env"`, its effects declare network access, and the invocation checks `ctx.require(omp.env.Capability.NET)`. Those gates compose with the Environment-side capability posture, but the address check itself is the device's own diligence, not a policy engine or a security boundary. `docs/py/11-env.md:1374-1385` says a Python allowlist is not enforcement for ambient syscalls; the 2026-08-20 ruling at `docs/py/11-env.md:2219-2225` makes brokered HTTP the wired path and labels direct sockets only as a fallback. There is no separate “sandbox v1” implemented here.

Large extracted pages are returned whole. `FetchPage.__spill__` delegates oversized verdicts to the central spill gate (`docs/py/02-verdicts.md:1015-1085`), which stores an `ArtifactRef` and retains the original bytes. The pi-style temporary file, returned path, cleanup burden, and extension-owned truncation are deleted.

## Gaps

- `omp.env.http_get` (`crates/py/python/omp/env.py:968-983`) has neither a no-redirect option nor a redirect/final-URL field on `omp.env.HttpResponse` (`crates/py/python/omp/env.py:925-936`). That frozen shape cannot satisfy per-hop resolve-and-validate while using the broker required by the 2026-08-20 ruling (`docs/py/11-env.md:2219-2225`), because the current Rust client may follow redirects before Python can inspect each `Location`. This example therefore uses pinned one-hop standard-library requests after `env.net` scope validation; the frozen broker needs an explicit one-hop redirect contract to remove that fallback.
