//! Focused provider and campaign CONTROL contract proof.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn provider_campaign_control_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import importlib
import json
import os
import struct

import omp
campaigns = importlib.import_module("omp.campaigns")
host_module = importlib.import_module("omp._host")
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


@omp.campaign("contract-campaign", at=omp.SETTLE, rev=2)
def react(event):
    return omp.Continue(inject=event["inject"])


snapshot = freeze_declarations()
provider_rows = provider_module._sealed_provider_declarations()
assert provider_rows[0]["id"] == spec.id
assert provider_rows[0]["activation"] == "eager-prompt"
assert provider_rows[0]["callbacks"][0]["when"]["provider"] == [spec.id]
campaign_table = campaigns._sealed_campaign_declaration(9)
assert campaign_table["generation"] == 9
assert campaign_table["manifests"][0]["id"] == "contract-campaign"
assert campaign_table["manifests"][0]["rev"] == 2
assert snapshot.providers and snapshot.campaigns


class Backend:
    def __init__(self):
        self.calls = []
        self.effects = []

    def intent_effect(self, operation, arguments):
        self.effects.append((operation, arguments))

    async def request(self, operation, arguments):
        json.dumps(arguments)
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
        if operation == "omp.campaigns.engage":
            return {
                "id": "eng-1",
                "campaign": arguments["campaign"],
                "extension": "dev.contract",
                "state": None,
                "queued": arguments["queue"],
            }
        if operation == "omp.campaigns.active":
            return [{
                "id": "eng-1",
                "campaign": "contract-campaign",
                "extension": "dev.contract",
                "state": None,
                "queued": False,
            }]
        if operation == "omp.campaigns.disengage":
            return arguments["engagement"] == "eng-1"
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
        intent = omp.Intent(omp.IntentKind.SERVICE_TIER, payload="priority")
        omp.intents.set("contract", intent)
        assert not omp.intents.declared("contract")
        omp.intents.clear("contract")
        assert not omp.intents.declared("contract")
        assert backend.effects == [
            ("omp.intents.set", {"key": "contract", "intents": (intent,)}),
            ("omp.intents.clear", {"key": "contract"}),
        ]
        assert not any(call[0].startswith("omp.intents.") for call in backend.calls)
        engagement = await campaigns.engage("contract-campaign")
        assert engagement.id == "eng-1"
        assert await campaigns.active() == (engagement,)
        assert await campaigns.disengage(engagement.id)
        await handle.replace(spec)
    finally:
        omp._control_backend.reset(token)
    request_call = next(call for call in backend.calls if call[0] == "omp.provider.request")
    assert request_call[1]["operation"] == "generate_image"
    assert request_call[1]["request"]["dimensions"] == {"width": 32, "height": 32}


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
    read_fd, write_fd = os.pipe()
    host = host_module.Host(write_fd)
    intent = omp.Intent(omp.IntentKind.SERVICE_TIER, payload="priority")
    host.intent_effect("omp.intents.set", {"key": "contract", "intents": (intent,)})
    size = struct.unpack("!I", os.read(read_fd, 4))[0]
    frame = json.loads(os.read(read_fd, size))
    os.close(read_fd)
    os.close(write_fd)
    assert frame == {
        "kind": "IntentEffect",
        "body": {
            "effect": {
                "operation": "omp.intents.set",
                "arguments": {
                    "key": "contract",
                    "intents": [{
                        "kind": "service_tier",
                        "on_unsupported": "unspecified",
                        "priority": 0,
                        "payload": "priority",
                    }],
                },
            },
            "authority": {"host_generation": 9, "session_generation": 11},
        },
    }

    reaction = asyncio.run(campaigns.dispatch_campaign_react(
        "contract-campaign",
        omp.SETTLE,
        b'{"inject":"again"}',
        engagement_id="eng-1",
        campaign_rev=2,
        event_rev=1,
        host_generation=9,
        session_generation=11,
    ))
    assert reaction["engagement_id"] == "eng-1"
    assert reaction["campaign_rev"] == 2
    assert reaction["verdict"] == "continue"
    try:
        asyncio.run(campaigns.dispatch_campaign_react(
            "contract-campaign",
            omp.SETTLE,
            b'{}',
            engagement_id="eng-1",
            campaign_rev=2,
            host_generation=8,
            session_generation=11,
        ))
    except omp.CampaignContractError:
        pass
    else:
        raise AssertionError("stale campaign generation was accepted")
finally:
    os.environ.pop("OMP_EXT_HOST_GENERATION")
    os.environ.pop("OMP_EXT_SESSION_GENERATION")
"#
				),
				None,
				None,
			)
		})
		.expect("provider and campaign contracts hold");
}
