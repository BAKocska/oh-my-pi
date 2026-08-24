//! Focused provider and regime CONTROL contract proof.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn provider_regime_control_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import dataclasses
import importlib
import json
import os

import omp
regimes = importlib.import_module("omp.regimes")
provider_module = importlib.import_module("omp.provider")
from omp._registry import freeze_declarations


route = omp.RouteSpec(
    "primary",
    "local://contract-provider",
    omp.Api.LOCAL,
    transport=omp.Transport.LOCAL,
)
image_model = omp.ModelSpec(
    "image-v1",
    "Image v1",
    ("primary",),
    operations=frozenset({omp.Operation.GENERATE_IMAGE}),
)
spec = omp.ProviderSpec(
    "dev.contract.provider",
    "Contract Provider",
    (route,),
    models=(image_model,),
)
handle = omp.provider(spec)


@handle
class ProviderCallbacks:
    @omp.hook("models_discover")
    async def discover(self, query):
        return {"provider": query["provider"]}


@dataclasses.dataclass(frozen=True)
class RetryState:
    attempts: int = 0


@omp.regime(
    "contract-regime",
    on=omp.SETTLE,
    lifetime="session",
    state=RetryState,
    max_steps=3,
    owns=("mode",),
    sets={"toolset": "read-only"},
    )
def retry_regime(ctx, next_):
    assert ctx.event.point is omp.SETTLE
    assert ctx.event.turn == 3
    ctx.context.append(omp.user_text("again"))
    ctx.state.replace(RetryState(ctx.state.value.attempts + 1))
    return next_.retry()


@omp.regime(
    "effects-only",
    on=omp.IDLE,
    when=omp.when.checkpoint_active(),
)
def effects_only(ctx, next_):
    ctx.context.append(omp.user_text("checkpoint"))


@omp.regime("isolated-sibling", on=omp.SETTLE)
def isolated_sibling(ctx, next_):
    assert "uncommitted" not in ctx.event
    ctx.settings.set("model", "small")
    return next_.complete()


@omp.regime("double-control", on=omp.SETTLE)
def double_control(ctx, next_):
    next_.retry()
    next_.complete()


@omp.regime("failing-draft", on=omp.SETTLE)
def failing_draft(ctx, next_):
    ctx.context.append(omp.user_text("must be discarded"))
    raise RuntimeError("callback failed")


snapshot = freeze_declarations()
provider_rows = provider_module._sealed_provider_declarations()
assert provider_rows[0]["id"] == spec.id
assert provider_rows[0]["activation"] == "eager-prompt"
assert provider_rows[0]["callbacks"][0]["when"]["provider"] == [spec.id]
regime_table = regimes._sealed_regime_declaration(9)
assert regime_table["generation"] == 9
assert regime_table["point_table"] == [point.value for point in omp.Point]
assert regime_table["control_table"] == [
    "retry", "wait", "reject", "cancel", "complete", "fail"
]
assert regime_table["effect_table"] == [
    "append_context", "rewrite_context", "require_tool", "set_scoped", "replace_state"
]
manifest = next(row for row in regime_table["manifests"] if row["id"] == "contract-regime")
assert manifest["revision"] == 1
assert manifest["points"] == ["settle"]
assert manifest["lifetime"] == "session"
assert manifest["max_steps"] == 3
assert manifest["owns"] == ["mode"]
assert json.loads(manifest["sets"]) == {"toolset": "read-only"}
assert manifest["state_family"].endswith(".RetryState")
assert manifest["state_revision"] == 1
effects_manifest = next(
    row for row in regime_table["manifests"] if row["id"] == "effects-only"
)
assert json.loads(effects_manifest["when"]) == {
    "point": "context",
    "checkpoint_active": True,
}
assert snapshot.providers and snapshot.regimes


class Backend:
    def __init__(self):
        self.calls = []
        self.effects = []

    def intent_effect(self, operation, arguments):
        self.effects.append((operation, arguments))

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.provider.models":
            return [{
                "id": "dev.contract.provider/image-v1",
                "provider": "dev.contract.provider",
                "model": "image-v1",
                "name": "Image v1",
                "facets": ["image_gen"],
                "outputs": ["image"],
            }]
        if operation == "omp.provider.is_authenticated":
            return True
        if operation == "omp.provider.request":
            return {
                "images": [{"hash": "00" * 32, "size": 3}],
                "cost_nanos_usd": 17,
            }
        if operation in {"omp.provider.replace", "omp.provider.retract"}:
            return None
        if operation == "omp.regimes.start":
            return {
                "id": "activation-1",
                "regime": arguments["regime_id"],
                "extension": "dev.contract",
                "status": "queued" if arguments["queue"] else "active",
            }
        if operation == "omp.regimes.active":
            return [{
                "id": "activation-1",
                "regime": "contract-regime",
                "extension": "dev.contract",
                "status": "active",
            }]
        if operation == "omp.regimes.stop":
            return arguments["activation_id"] == "activation-1"
        raise AssertionError(operation)


async def exercise():
    backend = Backend()
    token = omp._control_backend.set(backend)
    try:
        cards = await handle.models()
        assert cards[0].facets == frozenset({omp.Facet.IMAGE_GEN})
        assert await handle.is_authenticated()
        result = await handle.request(
            omp.Operation.GENERATE_IMAGE,
            omp.ImageRequest(
                "draw a circle",
                omp.Dimensions(32, 32),
                omp.ImageFormat.PNG,
            ),
        )
        assert isinstance(result, omp.ImageResult)
        assert result.images[0] == omp.BlobRef(bytes(32), 3)
        activation = await omp.regimes.start(
            "contract-regime", state=RetryState(), queue=False
        )
        assert (activation.id, activation.regime, activation.status) == (
            "activation-1", "contract-regime", "active"
        )
        assert await omp.regimes.active() == (
            omp.RegimeRecord(
                "activation-1", "contract-regime", "dev.contract", "active"
            ),
        )
        assert await activation.stop()
        assert await omp.regimes.stop(activation.id)
        await handle.replace(spec)
    finally:
        omp._control_backend.reset(token)


asyncio.run(exercise())
callback_name = provider_rows[0]["callbacks"][0]["name"]
assert asyncio.run(
    provider_module.dispatch_provider_callback(
        spec.id, callback_name, {"provider": spec.id}
    )
) == {"provider": spec.id}

os.environ["OMP_EXT_HOST_GENERATION"] = "9"
os.environ["OMP_EXT_SESSION_GENERATION"] = "11"
try:
    draft = asyncio.run(regimes.dispatch_regime_apply(
        "contract-regime",
        omp.SETTLE,
        b'{"turn":3,"state_revision":1}',
        b'{"attempts":1}',
        activation_id="activation-1",
        regime_revision=1,
        event_revision=1,
        state_revision=1,
        committed_steps=1,
    ))
    assert draft["control"] == {"kind": "retry", "props": {}}
    assert [effect["kind"] for effect in draft["effects"]] == [
        "append_context", "replace_state"
    ]
    assert json.loads(draft["effects"][0]["payload"]) == [{
        "seq": 0,
        "created_at_ms": 0,
        "message": {
            "role": "ROLE_USER",
            "parts": [{"text": "again"}],
        },
        "props": {},
    }]
    state_effect = draft["effects"][1]
    assert state_effect["state_revision"] == 1
    assert json.loads(state_effect["payload"]) == {"attempts": 2}

    effects_draft = asyncio.run(regimes.dispatch_regime_apply(
        "effects-only", omp.IDLE, b'{}', activation_id="activation-2",
    ))
    assert effects_draft["control"] is None
    assert [effect["kind"] for effect in effects_draft["effects"]] == [
        "append_context"
    ]

    sibling_draft = asyncio.run(regimes.dispatch_regime_apply(
        "isolated-sibling", omp.SETTLE, b'{}', activation_id="activation-3",
    ))
    assert sibling_draft["control"] == {"kind": "complete", "props": {}}
    assert [effect["kind"] for effect in sibling_draft["effects"]] == ["set_scoped"]
    assert sibling_draft["effects"][0]["name"] == "model"
    assert json.loads(sibling_draft["effects"][0]["payload"]) == "small"

    try:
        asyncio.run(regimes.dispatch_regime_apply(
            "double-control", omp.SETTLE, b'{}', activation_id="activation-4",
        ))
    except omp.RegimeContractError as error:
        assert "already sealed" in str(error)
    else:
        raise AssertionError("a second next_ control was accepted")

    try:
        asyncio.run(regimes.dispatch_regime_apply(
            "failing-draft", omp.SETTLE, b'{}', activation_id="activation-5",
        ))
    except RuntimeError as error:
        assert str(error) == "callback failed"
    else:
        raise AssertionError("a failed callback emitted its staged draft")
finally:
    os.environ.pop("OMP_EXT_HOST_GENERATION")
    os.environ.pop("OMP_EXT_SESSION_GENERATION")
"#
				),
				None,
				None,
			)
		})
		.expect("provider and regime contracts hold");
}
