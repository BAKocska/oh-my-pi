from __future__ import annotations

import omp
from omp import (
    Api,
    AuthMode,
    AuthSpec,
    CredentialSource,
    Duration,
    ProviderSpec,
    RouteSpec,
)

# GAP: documented by docs/py/13-inference.md §TrustDomain and RouteLimits,
# but absent from the frozen provider surface.
from omp import TrustDomain

_PROXY_NAME = "cursor-bridge"
_PROXY_ENDPOINT = "http://127.0.0.1:43659/v1"
_PROXY_SCRIPT = "cursor-bridge --listen 127.0.0.1:43659"

_CURSOR_SPEC = ProviderSpec(
    id="cursor",
    name="Cursor",
    routes=(
        RouteSpec(
            id="bridge",
            base_url=_PROXY_ENDPOINT,
            api=Api.OPENAI_CHAT,
            trust=TrustDomain.loopback(),
            auth=AuthSpec(
                mode=AuthMode.BEARER,
                sources=(CredentialSource.session(),),
            ),
        ),
    ),
)


@omp.provider(_CURSOR_SPEC)
class CursorBridge:
    """Declare the ordinary local route served by the foreign-wire bridge."""


@omp.hook("extension_activate")
async def _start_proxy(
    payload: omp.ExtensionActivateEvent,
    ctx: omp.Context,
) -> None:
    proc = await omp.env.proc.ensure(
        _PROXY_NAME,
        _PROXY_SCRIPT,
        restart="on-failure",
        ready={"log": r"listening on"},
    )

    # GAP: omp.creds.mint_scoped and Process.send_secret are documented by
    # docs/py/13-inference.md §§Credentials and the pi-cursor worked port, but
    # neither surface is present in the frozen Python layer.
    token = await omp.creds.mint_scoped(
        "bridge",
        ttl=Duration("15m"),
        provider="cursor",
    )
    await proc.send_secret("OMP_BRIDGE_TOKEN", token.token)
