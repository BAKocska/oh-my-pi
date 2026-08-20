//! Embedded proof of the frozen Python extension surface.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn frozen_surface_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import dataclasses
import importlib
import typing

import omp
from omp._scope import Scope


def expect_raises(error_type, call):
    try:
        call()
    except error_type:
        return
    raise AssertionError(f"expected {error_type.__name__}")


async def expect_raises_async(error_type, awaitable):
    try:
        await awaitable
    except error_type:
        return
    raise AssertionError(f"expected {error_type.__name__}")


# Importability and public export closure.
for name in omp.__all__:
    getattr(omp, name)
for suffix in (
    "agents", "context", "policy", "limits", "telemetry", "provider",
    "env", "ui", "hooks", "events", "prompts", "packages",
    "sessions", "journal", "artifacts", "index", "diagnostics", "urls", "devices",
):
    module = importlib.import_module(f"omp.{suffix}")
    for name in module.__all__:
        value = getattr(module, name)
        if getattr(value, "__annotations__", None):
            try:
                typing.get_type_hints(value)
            except Exception as error:
                raise AssertionError(f"unresolved annotations: omp.{suffix}.{name}") from error

registry_module = importlib.import_module("omp._registry")
packages_module = importlib.import_module("omp.packages")
packages_module._install_snapshot(
    [
        {
            "name": "acme-ext",
            "version": "1.0.0",
            "extension_id": "acme-ext",
        }
    ],
    own="acme-ext",
)
registry_module.configure_manifest(
    extension="acme-ext",
    declarations=(
        {
            "kind": "skills",
            "path": "acme_ext/skills/review/SKILL.md",
            "metadata": {
                "name": "review",
                "description": "Review a change.",
            },
        },
    ),
)

# Compaction is a domain hook, not a phased observation hook.
@omp.hook("compaction")
async def compaction_hook(event):
    return None

expect_raises(
    omp.HookContractError,
    lambda: omp.hook("compaction", phase=omp.HookPhase.REVIEW),
)

@omp.hook("sandbox_profile", phase=omp.HookPhase.TRANSFORM)
async def sandbox_profile_hook(event):
    return None


@omp.hook("sandbox_violation")
async def sandbox_violation_hook(event):
    return None

# Bash IR pure behavior.
span_read = omp.Span(start=0, end=3, line=1, column=1)
span_write = omp.Span(start=9, end=12, line=1, column=10)
span_all = omp.Span(start=0, end=12, line=1, column=1)
read_ref = omp.PathRef(
    lexical="input.py",
    resolved="/w/input.py",
    absolute="/w/input.py",
    access=omp.Access.READ,
    origin=omp.PathOrigin.ARGV,
    command_index=0,
    outside_workspace=False,
    exists=True,
    dynamic=False,
    span=span_read,
)
write_ref = omp.PathRef(
    lexical="out",
    resolved=None,
    absolute="/w/out",
    access=omp.Access.WRITE,
    origin=omp.PathOrigin.REDIRECT,
    command_index=1,
    outside_workspace=True,
    exists=False,
    dynamic=True,
    span=span_write,
)
read_command = omp.BashCommandIR(
    index=0,
    name="cat",
    argv=(),
    dynamic_args=(),
    env=(),
    redirects=(),
    process_subs=(),
    reads=(read_ref,),
    writes=(),
    net=(),
    cwd="/w",
    depth=0,
    container=None,
    subshell=False,
    builtin=False,
    coreutil=True,
    external=False,
    read_only=True,
    interpreter_code=None,
    span=span_read,
)
write_command = omp.BashCommandIR(
    index=1,
    name="write",
    argv=(),
    dynamic_args=(),
    env=(),
    redirects=(),
    process_subs=(),
    reads=(),
    writes=(write_ref,),
    net=(),
    cwd="/w",
    depth=0,
    container=None,
    subshell=False,
    builtin=True,
    coreutil=False,
    external=False,
    read_only=False,
    interpreter_code=None,
    span=span_write,
)
pipeline = omp.BashPipeline(
    commands=(read_command, write_command),
    negated=False,
    timed=False,
    span=span_all,
)
command_list = omp.BashAndOrList(
    pipelines=(pipeline,),
    operators=(),
    separator=omp.Separator.SEQUENCE,
    span=span_all,
)
ir = omp.BashIR(
    source="cat x.py > o",
    rev=omp.BASH_IR_REV,
    parser_rev="test",
    parse_ok=True,
    parse_error=None,
    truncated=False,
    node_count=2,
    is_compound=False,
    has_dynamic_eval=False,
    lists=(command_list,),
    commands=(read_command, write_command),
    functions=(),
    reads=(read_ref,),
    writes=(write_ref,),
    net=(),
    opaque=(),
)
assert not ir.is_read_only()
assert ir.writes_outside(("/w",)) == (write_ref,)
assert ir.segment(0) == "cat"
unicode_command = dataclasses.replace(
    read_command,
    span=omp.Span(start=3, end=6, line=1, column=3),
)
unicode_ir = dataclasses.replace(
    ir,
    source="é;cat",
    commands=(unicode_command,),
)
assert unicode_ir.segment(0) == "cat"
assert ir.touches("*.py") == (read_ref,)
read_pipeline = dataclasses.replace(pipeline, commands=(read_command,))
read_list = dataclasses.replace(command_list, pipelines=(read_pipeline,))
read_only_ir = dataclasses.replace(
    ir,
    lists=(read_list,),
    commands=(read_command,),
    writes=(),
)
assert read_only_ir.is_read_only()

availability_calls = 0


def availability_probe():
    global availability_calls
    availability_calls += 1
    return omp.Availability(False, "offline")


@omp.device("offline_device", available=availability_probe)
async def offline_device():
    return None


assert availability_calls == 0


# Device declaration validation and direct awaited invocation.
@omp.device("surface_device")
async def surface_device():
    return 42


def duplicate_equal_precedence():
    @omp.device("surface_device")
    async def duplicate():
        return None


def duplicate_without_replaces():
    @omp.device("surface_device", precedence=omp.Precedence.FALLBACK)
    async def duplicate():
        return None


def core_precedence():
    @omp.device("core_claim", precedence=omp.Precedence.CORE)
    async def invalid():
        return None


def bad_device_name():
    @omp.device("Bad-Name")
    async def invalid():
        return None


def tool_collision():
    @omp.tool("surface_device", rev=2)
    async def duplicate():
        return None


def noncallable_device():
    omp.device("noncallable")(object())


expect_raises(omp.PrecedenceConflict, duplicate_equal_precedence)
expect_raises(omp.PrecedenceConflict, duplicate_without_replaces)
expect_raises(omp.DeviceNameError, core_precedence)
expect_raises(omp.DeviceNameError, bad_device_name)
expect_raises(omp.PrecedenceConflict, tool_collision)
expect_raises(TypeError, noncallable_device)
assert asyncio.run(surface_device()) == 42

# Telemetry identity, fail-open instruments, and declarative export.
telemetry = importlib.import_module("omp.telemetry")
counter = telemetry.counter("cache.hits", unit="1", description="d")
assert counter.name == "omp.ext.acme-ext.cache.hits"
expect_raises(
    telemetry.SubscriptionError,
    lambda: telemetry.counter("omp.reserved", unit="1", description="d"),
)
expect_raises(ValueError, lambda: counter.add(-1))
counter.add(1)
assert telemetry.counter("cache.hits", unit="1", description="d") is counter


class InstrumentSink:
    def __init__(self):
        self.samples = []

    def add(self, name, value, attrs):
        self.samples.append(("counter", name, value, attrs))

    def record(self, name, value, attrs):
        self.samples.append(("histogram", name, value, attrs))


instrument_sink = InstrumentSink()
telemetry._install_instrument_sink(instrument_sink)
counter.add(2, result="hit")
histogram = telemetry.histogram(
    "request.latency", unit="ms", description="d", boundaries=(1, 10)
)
histogram.record(4.5, route="primary")
assert instrument_sink.samples == [
    ("counter", "omp.ext.acme-ext.cache.hits", 2, {"result": "hit"}),
    (
        "histogram",
        "omp.ext.acme-ext.request.latency",
        4.5,
        {"route": "primary"},
    ),
]
telemetry._install_instrument_sink(None)
expect_raises(
    telemetry.ExportError,
    lambda: telemetry.export(
        telemetry.OtlpTarget(endpoint="https://x", protocol="grpc")
    ),
)
export_handle = telemetry.export(telemetry.OtlpTarget(endpoint="https://x"))
asyncio.run(expect_raises_async(omp.NotWiredError, export_handle.stats()))

# Agents values, validation, unwired host arms, and real local timer behavior.
assert [field.name for field in dataclasses.fields(omp.agents.Usage)] == [
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cache_write_tokens",
    "requests",
    "cost_usd",
    "wall",
]
spec = omp.agents.SubagentSpec(task="t")
assert spec.agent == "task"
assert spec.isolation is omp.agents.Isolation.CLEAN
assert spec.budget is None
expect_raises(omp.agents.SpawnDenied, lambda: omp.agents.SubagentSpec(task=" "))
assert "history://r" in str(
    omp.agents.AgentGone(
        "r", omp.agents.AgentStatus.ABORTED, "history://r"
    )
)


async def agents_contract():
    await expect_raises_async(omp.NotWiredError, omp.agents.spawn(spec))
    await expect_raises_async(
        omp.NotWiredError, omp.agents.send("peer", "message")
    )
    await expect_raises_async(omp.NotWiredError, omp.agents.rewind(None))

    fired = asyncio.Event()
    firings = 0

    async def callback():
        nonlocal firings
        firings += 1
        if firings == 2:
            fired.set()

    timer = omp.agents.timer(omp.Duration("1ms"), callback, repeat=True)
    await asyncio.wait_for(fired.wait(), timeout=1.0)
    timer.cancel()
    assert not timer.active


asyncio.run(agents_contract())

# Context and compaction values.
asyncio.run(expect_raises_async(omp.NotWiredError, omp.context.compact()))
assert omp.CustomSummary(summary="s", first_kept_id="m1").summary == "s"
assert issubclass(omp.CompactionBusy, omp.OmpError)
expect_raises(LookupError, omp.Context.current)
context_module = importlib.import_module("omp._context")
logs = []
context_module._install_log_sink(
    lambda level, message, fields: logs.append((level, message, fields))
)
scope = Scope(
    invocation="invocation",
    generation=1,
    principal=object(),
    phase=omp.InvocationPhase.OPEN,
    extension="acme-ext",
    session="session",
    event="before_call",
    settings={"token": "secret"},
    secret_settings=frozenset({"token"}),
)
ctx = omp.Context.from_scope(scope)
assert dict(ctx.settings) == {"token": "secret"}
ctx.log("info", "message", token="secret", count=1)
assert logs == [
    (
        "info",
        "message",
        {
            "token": "[REDACTED]",
            "count": 1,
            "extension": "acme-ext",
            "session": "session",
            "generation": 1,
            "event": "before_call",
        },
    )
]
context_module._install_log_sink(None)
expect_raises(omp.CapabilityError, lambda: ctx.require("missing:cap"))

# Provider payload and failover contracts.
provider_module = importlib.import_module("omp.provider")
assert [field.name for field in dataclasses.fields(provider_module.ProviderError)] == [
    "provider",
    "route",
    "model",
    "operation",
    "kind",
    "retryability",
    "status",
    "retry_after",
    "attempt",
    "committed",
    "message",
    "identity",
]
provider_error = provider_module.ProviderError(
    provider="p",
    route="r",
    model="m",
    operation=provider_module.Operation.CHAT,
    kind=provider_module.ErrorKind.RATE_LIMITED,
    retryability=provider_module.Retryability.AFTER_DELAY,
    status=429,
    retry_after=None,
    attempt=1,
    committed=False,
    message="limited",
    identity=None,
)
assert provider_error.retryability is provider_module.Retryability.AFTER_DELAY
assert provider_module.Failover.switch_model("openai/gpt-x").kind is provider_module.FailoverKind.SWITCH_MODEL
assert provider_module.ErrorKind.RATE_LIMITED.value == "rate_limited"

# Environment document events, spill metadata, and workers admin arm.
assert omp.env.DocEventKind.WATCH_RESCANNED.value == "watch_rescanned"
spill = omp.Spill(b"payload", media_type="text/plain")
assert spill.value == b"payload" and spill.media_type == "text/plain"
asyncio.run(expect_raises_async(omp.NotWiredError, omp.workers.restart("missing")))

# Streaming device frames: typed progress and terminal results round-trip.
update = omp.Update(stage="running")
done = omp.Done(update.payload, useless=True)
assert update.payload == {"stage": "running"}
assert done.result is update.payload and done.useless is True

# Search parsing and provider usage are phase-free domain hooks.
assert provider_module.Api.SEARCH_HTTP.value == "search_http"
assert all(
    name in provider_module.__all__ and getattr(provider_module, name)
    for name in ("SearchPage", "SearchQuery", "SearchResult", "UsageQuery", "UsageReport", "UsageScope", "UsageUnit", "UsageWindow")
)
usage_window = provider_module.UsageWindow(id="w")
assert provider_module.UsageReport(windows=(usage_window,)).windows == (usage_window,)
search_query = provider_module.SearchQuery(provider="x", query="omp", count=5)
search_result = provider_module.SearchResult("OMP", "https://example.test", "snippet", 1)
assert provider_module.SearchPage((search_result,)).results == (search_result,)
@omp.hook("provider_usage", provider="x")
def usage_projection(query):
    return None
@omp.hook("search_parse", provider="x")
def parse_search(query, response):
    return (search_result,)
assert usage_projection.__omp_hooks__[-1].phase == "domain"
assert parse_search.__omp_hooks__[-1].phase == "domain"
asyncio.run(expect_raises_async(omp.NotWiredError, omp.hooks.dispatch_hook("search_parse")))

# Telemetry queries and session lifecycle payloads are typed host-owned values.
telemetry_module = importlib.import_module("omp.telemetry")
predicate = telemetry_module.Eq("edit@hl.3")
step = telemetry_module.Step(
    kinds=(telemetry_module.Kind.TOOL_CALL,),
    tool="edit",
    where={"rev": predicate},
)
telemetry_query = telemetry_module.Query(match=(step,), select=("rev",))
assert isinstance(predicate, telemetry_module.Predicate)
assert telemetry_query.match[0].where["rev"] == predicate
row = telemetry_module.Row(events=(), bindings={}, session="s", turn=0, _values={"rev": "edit@hl.3"})
result = telemetry_module.QueryResult(
    rows=(row,), total=1, cursor=None, truncated=False, scanned_sessions=1,
    scanned_events=1, backfilled=False, floored=False, elapsed_ms=1,
)
assert row["rev"] == "edit@hl.3" and result.rows == (row,)
event_prefix = dict(
    kind=telemetry_module.Kind.SESSION_START, seq=1, at_ms=2, session="s", agent="main",
    depth=0, conversation="c", trace=None, principal="p", generation=1,
)
envelope = telemetry_module.Envelope(**event_prefix)
session_start = telemetry_module.SessionStart(
    **event_prefix, resumed=False, parent=None, cwd=None, place=omp.Place.ENV, remote=None,
    model="m", provider="p", devices=(), core_tools=(), extensions=(), schema_rev="1",
    prompt=object(), registry_hash="hash",
)
turn_start = telemetry_module.TurnStart(
    **(event_prefix | {"kind": telemetry_module.Kind.TURN_START}), turn=0, trigger="user",
    input_chars=1, input_parts=1, attachments=0, model="m", effort=None,
)
turn_end = telemetry_module.TurnEnd(
    **(event_prefix | {"kind": telemetry_module.Kind.TURN_END}), turn=0, steps=1, requests=1,
    calls=0, tokens=telemetry_module.Tokens(total=1), cost=None, latency_ms=1,
    stop=telemetry_module.StopReason.END_TURN, tools_used=(), faults=0, interrupted=False,
    context=telemetry_module.ContextSnapshot(1, 0, 0, None, 10, 0.1),
)
session_end = telemetry_module.SessionEnd(
    **(event_prefix | {"kind": telemetry_module.Kind.SESSION_END}), reason="exit", turns=1,
    requests=1, calls=0, tokens=telemetry_module.Tokens(total=1), cost=None, wall_ms=1,
    faults=0, issues=0,
)
assert envelope.session == session_start.session == turn_start.session == turn_end.session == session_end.session
asyncio.run(expect_raises_async(omp.NotWiredError, telemetry_module.query(telemetry_query)))

# Credentials: manifest-scoped host arms expose typed metadata and scoped tokens.
creds_module = importlib.import_module("omp.creds")
assert omp.creds is creds_module
assert all(callable(getattr(creds_module, name)) for name in (
    "list", "store", "refresh", "clear", "disable", "enable", "report_block",
    "usage", "mint_scoped", "import_oauth", "reveal",
))
credential_meta = omp.CredentialMeta(1, "example", None, omp.CredentialKind.API_KEY)
scoped_token = omp.ScopedToken("scoped", 123)
assert credential_meta.kind.value == "api_key"
assert scoped_token.token == "scoped" and scoped_token.expires_at_ms == 123
asyncio.run(expect_raises_async(omp.NotWiredError, omp.creds.usage()))

# UI commands, transcript activation, and renderer collisions.
async def complete_managed(query, ctx):
    return ()
@omp.command(
    "managed",
    aliases=("pm",),
    description="Manage prompts",
    args=(omp.ui.Arg("name", "Prompt name", "<name>"),),
    hint="/managed <name>",
    arg_completions=complete_managed,
)
async def managed(inv, ctx):
    return None
command_row = {
    row.name: row for row in registry_module.registry.snapshot().commands
}["managed"]
assert command_row.aliases == ("pm",)
assert command_row.args == (omp.ui.Arg("name", "Prompt name", "<name>"),)
assert command_row.hint == "/managed <name>"
assert command_row.arg_completions is complete_managed
assert command_row.description == "Manage prompts"
assert command_row.handler is managed

activations = []
@omp.ui.on_activate("card")
async def activate_card(event, ctx):
    activations.append((event, ctx))
activation = omp.ui.Activation("card.dynamic", omp.ui.ActivationSource.MOUSE)
asyncio.run(omp.ui._dispatch_activation(activation, "context"))
assert activations == [(activation, "context")]
assert omp.ui.ActivationSource.KEY.value == "key"

@omp.renderer("__duplicate_ui__", family="ui", rev=1)
def first_ui_renderer(view, ctx):
    return None
def register_duplicate_ui_renderer():
    @omp.renderer("__duplicate_ui__", family="ui", rev=1)
    def duplicate_ui_renderer(view, ctx):
        return None
expect_raises(omp.ui.DuplicateRenderer, register_duplicate_ui_renderer)
assert omp.DuplicateRenderer is omp.ui.DuplicateRenderer

# Argument metadata: Field and Coerce lower once into the per-revision registry.
argument_field = omp.Field(
    "Requested issue count.",
    alias=("issueCount",),
    coerce=(omp.Coerce.INTEGER, omp.Coerce.STRIP),
    expected="a positive integer",
    example="3",
)
assert argument_field.description == "Requested issue count."
assert argument_field.additional_properties is False
assert argument_field.alias == ("issueCount",)
assert argument_field.coerce == (omp.Coerce.INTEGER, omp.Coerce.STRIP)
assert tuple(member.value for member in omp.Coerce) == (
    "loose_bool", "integer", "number", "string", "singleton",
    "json_string", "strip", "csv", "null_elision",
)
@omp.device("arg_metadata_device", family="arg-contract", rev=3)
async def arg_metadata_device(
    count: typing.Annotated[int, argument_field],
):
    return count


# Discovery and trust: typed declarations and phase-free model projection.
assert all(
    getattr(omp, name) is getattr(provider_module, name)
    for name in (
        "DiscoveryDefaults", "DiscoveryKind", "DiscoveryPage", "DiscoveryQuery",
        "DiscoverySpec", "LoginRequest", "Pagination", "ProviderHandle",
        "RedirectTrust", "RefreshReason", "RefreshRequest", "RouteLimits",
        "SignRequest", "TrustDomain",
    )
)
assert tuple(member.value for member in omp.DiscoveryKind) == (
    "openai_models", "google_models", "ollama_tags", "account_models", "specialized",
)
assert tuple(member.value for member in omp.RedirectTrust) == (
    "deny", "same_origin", "public_only",
)
expect_raises(
    omp.SpecError,
    lambda: omp.DiscoverySpec(
        omp.DiscoveryKind.SPECIALIZED, "/models", "models",
        interval=omp.Duration("1s"),
    ),
)
discovery_spec = omp.DiscoverySpec(
    omp.DiscoveryKind.SPECIALIZED, "/models", "models",
    interval=omp.Duration("5s"),
)
assert discovery_spec.pagination == omp.Pagination.single_page()
defaults = omp.DiscoveryDefaults(routes=("local",))
assert defaults.cost == omp.Cost.free()
assert defaults.operations == frozenset({omp.Operation.CHAT})
https_route = omp.RouteSpec("remote", "https://example.test/v1", omp.Api.OPENAI_CHAT)
in_process_route = omp.RouteSpec(
    "usage", "local://synthetic-provider", omp.Api.LOCAL,
    transport=omp.Transport.LOCAL,
)
loopback_route = omp.RouteSpec(
    "local", "http://127.0.0.1:1234/v1", omp.Api.OPENAI_CHAT,
    discovery=discovery_spec, trust=omp.TrustDomain.loopback(),
    limits=omp.RouteLimits(max_context_tokens=8192),
)
assert https_route.trust.origin == "https://example.test"
assert in_process_route.trust.origin == "local://synthetic-provider"
assert loopback_route.trust.origin == "http://127.0.0.1:1234"
expect_raises(
    omp.SpecError,
    lambda: omp.RouteSpec(
        "remote-plain", "http://example.test/v1", omp.Api.OPENAI_CHAT,
        trust=omp.TrustDomain.loopback(),
    ),
)
query = omp.DiscoveryQuery(
    "local", "local", None, None, provider_module.DiscoveryTrigger.MANUAL,
)
page = omp.DiscoveryPage(models=(), authoritative=True)
assert query.route == "local" and page.authoritative
discovery_provider = omp.ProviderSpec(
    "discovery-test", "Discovery Test", (loopback_route,),
    discovery_defaults=defaults,
)
discovery_handle = omp.provider(discovery_provider)
assert discovery_handle.id == "discovery-test"
asyncio.run(expect_raises_async(omp.NotWiredError, discovery_handle.retract()))
asyncio.run(
    expect_raises_async(
        omp.NotWiredError, discovery_handle.replace(discovery_provider)
    )
)
@omp.hook("models_discover", provider="discovery-test")
def discover_models(query, ctx):
    return page
assert discover_models.__omp_hooks__[-1].phase == "domain"

# Environment processes: shared restart policy, combined readiness, and deferred HTTP egress.
restart_policy = omp.env.RestartPolicy(policy=omp.Restart.ON_FAILURE)
assert restart_policy.policy is omp.Restart.ON_FAILURE
log_ready = omp.env.ReadyLog(pattern="x")
tcp_ready = omp.env.ReadyTcp(port=1)
ping_ready = omp.env.ReadyPing(nonce=7)
all_ready = omp.env.ReadyAll(log_ready, tcp_ready)
assert all_ready.probes == (log_ready, tcp_ready)
assert isinstance(ping_ready, omp.env.ReadyPing)
assert omp.env.ProcState.STARTING.value == "starting"
assert omp.env.Lifecycle.EXIT.value == "exit"
completed = omp.env.Completed(
    omp.env.Outcome.EXITED, 0, "", omp.Duration("1ms"), b"ok", None, False,
)
process_info = omp.env.ProcessInfo("p", 1, omp.env.ProcState.RUNNING, completed)
process_output = omp.env.ProcessOutput(1, omp.env.Channel.STDOUT, b"ok", 1)
assert process_info.status is completed and process_output.data == b"ok"
response = omp.env.HttpResponse(
    200,
    {"content-type": "application/json"},
    b'{"ok": true}',
    "https://example.test/final",
)
assert response.json() == {"ok": True}
assert response.final_url == "https://example.test/final"
asyncio.run(
    expect_raises_async(
        TypeError, omp.env.proc.ensure("invalid-ready", "true", ready=object())
    )
)
asyncio.run(expect_raises_async(omp.NotWiredError, omp.env.http_get("https://example.test")))
process = omp.env.Process("p", 7)
expect_raises(omp.NotWiredError, lambda: process.endpoint)
asyncio.run(expect_raises_async(omp.NotWiredError, process.restart()))
assert hasattr(omp.env.Run, "stdin") and not hasattr(omp.env.Run, "write")

class ProcessProbeBackend:
    def __init__(self):
        self.calls = []

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.env.Process.restart":
            return {"name": "p", "generation": 8}
        return process_info

    def stream(self, operation, arguments):
        self.calls.append((operation, arguments))
        return ()

async def exercise_process_fence(backend):
    restarted = await process.restart()
    assert restarted.name == "p" and restarted.generation == 8
    await process.info()
    async for _ in process.output(after=2):
        pass
    async for _ in process.states():
        pass
    await process.send(b"x")
    await process.send_secret("token", "secret")
    await process.signal("SIGTERM")
    await process.stop(grace=omp.Duration("1s"))
    await omp.env.Run(b"run").stdin(b"x")

process_backend = ProcessProbeBackend()
process_binding = omp.env._binding.set((process_backend, None))
asyncio.run(exercise_process_fence(process_backend))
omp.env._binding.reset(process_binding)
process_operations = {
    operation for operation, _ in process_backend.calls
    if operation.startswith("omp.env.Process.")
}
assert process_operations == {
    "omp.env.Process.restart", "omp.env.Process.info", "omp.env.Process.output",
    "omp.env.Process.states", "omp.env.Process.send", "omp.env.Process.send_secret",
    "omp.env.Process.signal", "omp.env.Process.stop",
}
assert all(
    arguments["generation"] == 7
    for operation, arguments in process_backend.calls
    if operation.startswith("omp.env.Process.")
)
assert ("omp.env.Run.stdin", {"run": b"run", "data": b"x"}) in process_backend.calls

# Secrets: typed declarations and Core-owned masking fail closed without host arms.
assert omp.secrets is not None
secret_rule = omp.SecretRule(
    "TOKEN", kind=omp.SecretKind.ENV, mode=omp.SecretMode.REDACT,
    label="credential", replacement="[secret]",
)
assert secret_rule.pattern == "TOKEN" and secret_rule.replacement == "[secret]"
assert tuple(member.value for member in omp.SecretKind) == ("literal", "regex", "env")
assert tuple(member.value for member in omp.SecretMode) == ("obfuscate", "redact")
expect_raises(omp.NotWiredError, lambda: omp.secrets.declare(secret_rule))
expect_raises(omp.NotWiredError, lambda: omp.secrets.mask("TOKEN"))

# Residual closures: catalog, Environment values, journal projections, and URL reads.
devices_module = importlib.import_module("omp.devices")
assert {"provenance", "slotted", "schema_bytes", "schema_tokens"} <= set(
	devices_module.DeviceInfo.__dataclass_fields__
)
assert not asyncio.iscoroutinefunction(omp.devices.list)
assert any(row.name == "surface_device" for row in omp.devices.list())
pty = omp.env.Pty(rows=24, columns=80)
assert pty.rows == 24 and pty.columns == 80 and pty.terminal == "xterm-256color"
path_meta = omp.env.PathMeta(
    omp.EnvPath("src"), omp.env.FileKind.DIRECTORY, 0,
)
assert path_meta.kind is omp.env.FileKind.DIRECTORY
asyncio.run(expect_raises_async(omp.NotWiredError, omp.env.worktree()))
expect_raises(omp.NotWiredError, lambda: omp.journal.latest("missing"))
expect_raises(
    omp.NotWiredError,
    lambda: omp.journal.fold("missing", lambda state, _entry: state, 0),
)
assert asyncio.iscoroutinefunction(omp.urls.read)

# Turn inference selection: thinking patches and scope-backed route/effort.
assert "thinking" in {
    field.name for field in dataclasses.fields(omp.TurnStartEvent)
}
selected_model = omp.ModelRef("provider", "api", "model")
selected_route = omp.RouteRef("provider", "route")
turn_selection = omp.TurnStartEvent(
    turn_id="turn",
    turn_index=1,
    prompt_hash="prompt",
    toolset_hash="tools",
    enabled_tools=(),
    input_mode=omp.TurnInputMode.FULL,
    model=selected_model,
    route=selected_route,
    thinking=omp.Effort.MEDIUM,
    deadline=None,
    attempt=1,
    prompt_changed=False,
    toolset_changed=False,
)
thinking_patch = omp.Modify(patch={"thinking": omp.Effort.HIGH})
assert dataclasses.replace(
    turn_selection, **thinking_patch.patch
).thinking is omp.Effort.HIGH
unknown_patch = omp.Modify(patch={"not_a_turn_field": True})
expect_raises(
    TypeError, lambda: dataclasses.replace(turn_selection, **unknown_patch.patch)
)
selection_scope = dataclasses.replace(
    scope,
    model=selected_model,
    route=selected_route,
    thinking=omp.Effort.HIGH,
)
selection_context = omp.Context.from_scope(selection_scope)
assert selection_context.model is selected_model
assert selection_context.route is selected_route
assert selection_context.thinking is omp.Effort.HIGH

# Sessions: typed lineage, indexed cost, and host-owned mutation requests.
assert omp.SandboxSessionKind is omp.policy.SandboxSessionKind
assert omp.SessionKind is omp.sessions.SessionKind
assert omp.SessionLink is omp.sessions.SessionLink
assert omp.SessionNotFound is omp.sessions.SessionNotFound
assert not hasattr(omp.sessions, "UsageCost")
session_cost = omp.sessions.Cost(
    nanos_usd=2_500_000_000, estimated=True,
    input_nanos_usd=1_000_000_000, output_nanos_usd=1_500_000_000,
)
assert session_cost.usd == 2.5
assert typing.get_type_hints(omp.sessions.SessionInfo)["cost"] is omp.sessions.Cost
session_link = omp.SessionLink("child", "parent", 17)
assert (session_link.id, session_link.parent, session_link.at) == (
    "child", "parent", 17,
)
expect_raises(
    dataclasses.FrozenInstanceError,
    lambda: setattr(session_link, "parent", None),
)
for session_call in (
    omp.sessions.get("missing"),
    omp.sessions.lineage("missing"),
    omp.sessions.resume("missing"),
    omp.sessions.rename("missing", "New title"),
    omp.sessions.delete("missing"),
):
    asyncio.run(expect_raises_async(omp.NotWiredError, session_call))

# Schedules: payload-bearing delivery and schedule attribution.
schedule_trigger = omp.agents.Every(
	omp.Duration("60s"), jitter=omp.Duration("5s"), align=True,
)
schedule_delivery = omp.agents.Inject(
	prompt="poll chat replies",
	mode=omp.agents.DeliveryMode.NEXT_TURN,
	visible=True,
)
assert [field.name for field in dataclasses.fields(schedule_trigger)] == [
	"interval", "jitter", "align",
]
assert [field.name for field in dataclasses.fields(schedule_delivery)] == [
	"prompt", "mode", "visible",
]
assert schedule_delivery.prompt == "poll chat replies"
assert "schedule_id" in {
	field.name for field in dataclasses.fields(omp.BeforeAgentStartEvent)
}
asyncio.run(
	expect_raises_async(
		omp.NotWiredError,
		omp.agents.schedule("chat-poll", schedule_trigger, schedule_delivery),
	)
)
# Approvals: frozen external registration and idempotent late resolution.
@omp.approver(
    "test-approver",
    kinds=(omp.ApprovalKind.EXEC,),
    timeout=omp.Duration("30s"),
    unreachable=omp.Unreachable.FAIL_CLOSED,
)
async def test_approver(ticket, ctx):
    return None

approver_definition = {
    definition.name: definition
    for definition in registry_module.registry.snapshot().approvers
}["test-approver"]
assert approver_definition.handler is test_approver
assert approver_definition.kinds == (omp.ApprovalKind.EXEC,)
approval_decision = omp.ApprovalDecision(
    False, omp.PolicyScope.ONCE, omp.ApprovalSource.EXTERNAL,
    "test-approver", "denied", False,
)
asyncio.run(expect_raises_async(omp.NotWiredError, omp.policy.pending()))
asyncio.run(
    expect_raises_async(
        omp.NotWiredError, omp.policy.decide("ticket-1", approval_decision)
    )
)

# Provider catalog: overlays, successor rotation, ADC, refresh, and image requests.
image_dimensions = omp.Dimensions(1024, 1024)
image_caps = omp.ImageCaps(
    frozenset({omp.ImageFeature.GENERATE}),
    (image_dimensions,),
    frozenset({omp.ImageFormat.PNG}),
)
image_model = omp.ModelSpec(
    "image-1", "Image One", (), operations=frozenset({omp.Operation.GENERATE_IMAGE}),
    image=image_caps,
)
assert typing.get_type_hints(omp.ModelSpec)["image"] == omp.ImageCaps | None
image_request = omp.ImageRequest("draw a circle", image_dimensions, omp.ImageFormat.PNG, 2)
assert omp.ImageResult((), 17).cost_nanos_usd == 17

model_patch = omp.ModelPatch(display_name="Friendly Base")
model_overlay = omp.ModelOverlay(
    omp.ModelRef("overlay-provider", "openai", "base"), patch=model_patch
)
scoped_alias = omp.ScopedAlias(
    "overlay-provider",
    omp.CatalogAlias("fast", "base", "workspace shorthand", "extension"),
)
overlay_spec = omp.ProviderSpec(
    "overlay-provider", "Overlay Provider", (),
    aliases=(scoped_alias,), model_overlays=(model_overlay,),
)
overlay_handle = omp.provider(overlay_spec, extends="overlay-provider")
assert overlay_handle.id == "overlay-provider"
class OverlayProvider:
    pass
assert overlay_handle(OverlayProvider) is OverlayProvider
assert OverlayProvider.__omp_provider_extends__ == "overlay-provider"
expect_raises(
    omp.SpecError,
    lambda: omp.ProviderSpec(
        "overlay-provider", "Conflict", (),
        model_overlays=(model_overlay, model_overlay),
    ),
)
expect_raises(
    omp.SpecError,
    lambda: omp.provider(overlay_spec),
)
expect_raises(
    omp.SpecError,
    lambda: omp.ProviderSpec(
        "overlay-provider", "Alias Conflict", (),
        aliases=(
            scoped_alias,
            omp.ScopedAlias(
                "overlay-provider",
                omp.CatalogAlias("fast", "other", "conflict", "extension"),
            ),
        ),
    ),
)

adc = omp.CredentialSource.application_default(
    project_env="VERTEX_PROJECT", location_env="VERTEX_LOCATION"
)
assert adc.kind == "application_default"
assert adc.options["project_env"] == "VERTEX_PROJECT"
rotation = omp.Failover.rotate_account("identity-next", cooldown=omp.Duration("5s"))
assert rotation.target == "identity-next" and rotation.kind is omp.FailoverKind.ROTATE_ACCOUNT

@omp.hook("provider_refresh", provider="overlay-provider")
async def refresh_provider(req, ctx):
    return None

assert refresh_provider.__omp_hooks__[-1].phase == "domain"
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        overlay_handle.request(omp.Operation.GENERATE_IMAGE, image_request),
    )
)

# UI residuals: clipboard effects, frozen shortcuts, and host-fed overlay events.
assert omp.shortcut is omp.ui.shortcut
expect_raises(omp.ui.ShortcutError, lambda: omp.shortcut("ctrl+alt"))

@omp.shortcut(
    "SHIFT+CTRL+X",
    action_id="copy-cut.cut",
    description="Cut composer text",
    when=frozenset({omp.ui.Phase.IDLE}),
)
async def copy_cut_shortcut(action, ctx):
    return None

shortcut_definition = {
    definition.action_id: definition
    for definition in registry_module.registry.snapshot().shortcuts
}["copy-cut.cut"]
assert shortcut_definition.chord == "ctrl+shift+x"
assert shortcut_definition.description == "Cut composer text"
assert shortcut_definition.when == frozenset({omp.ui.Phase.IDLE})
assert shortcut_definition.handler is copy_cut_shortcut

clipboard_effects = []
omp.ui._install_effect_sink(clipboard_effects.append)
omp.ui.set_clipboard("copied text")
omp.ui._install_effect_sink(None)
assert clipboard_effects == [
    {"kind": "set_clipboard", "body": {"text": "copied text"}}
]

watched_kinds = (
    omp.ui.EventKind.HIGHLIGHTED,
    omp.ui.EventKind.CHANGED,
    omp.ui.EventKind.FILTERED,
    omp.ui.EventKind.PRESSED,
)
watched_events = tuple(omp.ui.OverlayEvent(kind) for kind in watched_kinds)
assert tuple(event.kind for event in watched_events) == watched_kinds

highlighted_event = omp.ui.OverlayEvent(
    omp.ui.EventKind.HIGHLIGHTED,
    id="threads",
    value="thread-2",
    values={"threads": "thread-2"},
)
assert highlighted_event.query is None

async def overlay_event_request(kind, **body):
    assert kind == "overlay_events" and body == {"id": "side-chat"}
    async def host_events():
        yield highlighted_event
        yield {"kind": "cancel", "values": {}}
    return host_events()

async def collect_overlay_events():
    handle = omp.ui.OverlayHandle("side-chat")
    return [event async for event in handle.events()]

original_ui_request = omp.ui._request
omp.ui._request = overlay_event_request
try:
    overlay_events = asyncio.run(collect_overlay_events())
finally:
    omp.ui._request = original_ui_request
assert overlay_events == [
    highlighted_event,
    omp.ui.OverlayEvent(omp.ui.EventKind.CANCEL),
]

# Env HTTP: scoped GET, POST, and PUT host arms remain explicit when unwired.
assert {"http_get", "http_post", "http_put"} <= set(omp.env.__all__)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        omp.env.http_get(
            "https://example.test",
            timeout=omp.Duration("2s"),
            headers={"accept": "application/json"},
        ),
    )
)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        omp.env.http_post(
            "https://example.test",
            body=b"{}",
            headers={"content-type": "application/json"},
            timeout=omp.Duration("2s"),
        ),
    )
)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        omp.env.http_put(
            "https://example.test",
            body=b"{}",
            headers={"content-type": "application/json"},
            timeout=omp.Duration("2s"),
        ),
    )
)

# Round 5 devices: child declarations, synchronous snapshots, and slot budget.
@surface_device.subtool("inspect/detail")
async def inspect_surface_device():
    """Inspect one nested surface-device leaf."""
    return None


declared_device_rows = omp.devices.list(mounted_only=False)
assert str(inspect_surface_device.path) == "surface_device/inspect/detail"
assert any(
    row.path == inspect_surface_device.path for row in declared_device_rows
)
assert omp.devices.HARD_SLOT_BUDGET == 8
assert omp.HARD_SLOT_BUDGET == 8

host_catalog_row = dataclasses.replace(
    declared_device_rows[0],
    name="host_catalog_device",
    identity="host_catalog_device@host/1",
    path=omp.ToolPath("host_catalog_device"),
)
devices_module._install_catalog_view((host_catalog_row,))
try:
    merged_device_rows = omp.devices.list(mounted_only=False)
finally:
    devices_module._install_catalog_view(None)
assert merged_device_rows[0] is host_catalog_row
assert any(row.path == inspect_surface_device.path for row in merged_device_rows)

# Round 5 merged catalog: resolved cards and a typed host-fed watch stream.
catalog_card = omp.ModelCard(
    id="acme/reasoner",
    provider="acme",
    model="reasoner",
    name="Acme Reasoner",
    family="acme",
    facets=frozenset({omp.Facet.CHAT}),
    inputs=frozenset({omp.Modality.TEXT}),
    outputs=frozenset({omp.Modality.TEXT}),
    reasoning=True,
    efforts=(omp.Effort.LOW, omp.Effort.HIGH),
    context_window=131072,
    max_output_tokens=8192,
    pricing=(omp.Price(omp.PriceUnit.MTOK_INPUT, 250_000_000),),
    availability=provider_module.Availability.AVAILABLE,
    source=omp.ModelCard.Source.EXTENSION,
    blocked_until_ms=None,
    deprecated=False,
    updated_at_ms=1234,
    supports_tools=True,
    props={"acme/tier": "pro"},
)
assert catalog_card.id == "acme/reasoner"
assert catalog_card.source is omp.ModelCard.Source.EXTENSION
assert catalog_card.pricing[0].unit is omp.PriceUnit.MTOK_INPUT
catalog_cursor = omp.Cursor(epoch=b"catalog-epoch", generation=7)
catalog_event = omp.ModelEvent(cursor=catalog_cursor, upserted=catalog_card)
assert catalog_event.upserted is catalog_card
asyncio.run(
    expect_raises_async(omp.NotWiredError, omp.models())
)

async def collect_unwired_model_events():
    return [event async for event in omp.watch_models(catalog_cursor)]

asyncio.run(
    expect_raises_async(omp.NotWiredError, collect_unwired_model_events())
)

async def catalog_control_request(operation, **arguments):
    assert operation == "omp.provider.watch_models"
    assert arguments == {"since": catalog_cursor}
    async def host_model_events():
        yield catalog_event
        yield {
            "cursor": {"epoch": b"catalog-epoch", "generation": 8},
            "removed_id": catalog_card.id,
        }
    return host_model_events()

async def collect_model_events():
    return [event async for event in omp.WatchModels(catalog_cursor)]

original_provider_control_request = provider_module._provider_control_request
provider_module._provider_control_request = catalog_control_request
try:
    catalog_events = asyncio.run(collect_model_events())
finally:
    provider_module._provider_control_request = original_provider_control_request
assert catalog_events == [
    catalog_event,
    omp.ModelEvent(
        cursor=omp.Cursor(epoch=b"catalog-epoch", generation=8),
        removed_id=catalog_card.id,
    ),
]
assert all(isinstance(event, omp.ModelEvent) for event in catalog_events)

# Round 5 UI: typed message folds and host-composed renderer decoration.
message_view = omp.MessageView(
    id="message-1",
    kind="assistant",
    role="assistant",
    text="original",
)
assert omp.MessageView is omp.ui.MessageView
assert dataclasses.is_dataclass(message_view)
assert (message_view.id, message_view.kind, message_view.role, message_view.text) == (
    "message-1", "assistant", "assistant", "original",
)

@omp.renderer("__decorated_ui__", family="ui", rev=1, decorates=True)
def decorated_ui_renderer(view, ctx):
    return omp.ui.text("augmentation")

decorated_registration = omp.ui._device_renderers[("__decorated_ui__", "ui", 1)]
assert decorated_registration.function is decorated_ui_renderer
assert decorated_registration.decorates is True
assert decorated_registration.reduce is None
assert decorated_ui_renderer.__omp_renderer_decorates__ is True

# Round 5 telemetry: prompt slot facts, request timings/content, and coalescing survive freeze.
slot_fingerprint = telemetry_module.PromptSlotFingerprint(
	digest="ab" * 16,
	size_bytes=128,
	band=omp.SlotClass.STABLE,
)
prompt_fingerprint = telemetry_module.PromptFingerprint(
	digest="cd" * 16,
	slots={"workspace": slot_fingerprint},
	changed=("workspace",),
	prefix_stable_bytes=64,
	cache_key="session-key",
	retention="short",
	mode="explicit",
	ttl="thirty_minutes",
	breakpoint="latest_stable_message",
	breakpoint_indices=(0,),
)
degradation = telemetry_module.Degradation(
	what="sampling.top_k",
	detail="provider omitted top-k",
	action=telemetry_module.DegradeAction.DROPPED,
)
model_request = telemetry_module.ModelRequest(
	seq=7,
	usage=telemetry_module.Tokens(input=4, output=2, total=6),
	prompt=prompt_fingerprint,
	served_model="acme/reasoner",
	latency_ms=120,
	ttft_ms=30,
	degraded=(degradation,),
)
assert prompt_fingerprint.slots["workspace"] == slot_fingerprint
assert slot_fingerprint.size_bytes == 128 and slot_fingerprint.band is omp.SlotClass.STABLE
assert model_request.latency_ms == 120 and model_request.ttft_ms == 30
assert model_request.degraded == (degradation,)
assert model_request.request_content is None and model_request.response_content is None
captured_request = dataclasses.replace(
	model_request,
	request_content=b"request",
	response_content=b"response",
)
assert captured_request.request_content == b"request"
assert captured_request.response_content == b"response"

def request_coalesce_key(event):
	return event.served_model

@telemetry_module(
	[telemetry_module.Kind.MODEL_REQUEST],
	overflow=telemetry_module.Overflow.COALESCE_BY_KEY,
	coalesce_key=request_coalesce_key,
)
async def coalesced_request_sink(event, ctx):
	return None

telemetry_snapshot = registry_module.registry.snapshot()
coalesced_definition = next(
	definition
	for definition in telemetry_snapshot.telemetry
	if definition.handler is coalesced_request_sink
)
assert coalesced_definition.coalesce_key is request_coalesce_key
assert coalesced_definition.overflow == telemetry_module.Overflow.COALESCE_BY_KEY.value

# Round 5 provider declarations, media operations, and typed completion parts.
assert issubclass(omp.SpecError, omp.ExtensionError)
assert not issubclass(omp.SpecError, ValueError)
assert tuple(omp.CacheRetention) == (
    omp.CacheRetention.REQUEST,
    omp.CacheRetention.SESSION,
    omp.CacheRetention.SHORT,
    omp.CacheRetention.LONG,
)
cache_caps = omp.PromptCacheCaps(
    frozenset({omp.CacheRetention.SESSION, omp.CacheRetention.SHORT}),
    min_prefix_tokens=256,
    max_breakpoints=4,
)
assert cache_caps.min_prefix_tokens == 256 and cache_caps.max_breakpoints == 4
assert not hasattr(cache_caps, "minimum_prefix_tokens")

speech_caps = omp.SpeechCaps(
    frozenset({omp.SpeechFeature.STREAMING, omp.SpeechFeature.VOICE_SELECTION}),
    ("alloy",),
    frozenset({omp.AudioFormat.MP3}),
    (24_000,),
)
transcription_caps = omp.TranscriptionCaps(
    frozenset({
        omp.TranscriptionFeature.TIMESTAMPS,
        omp.TranscriptionFeature.LANGUAGE_HINT,
    }),
    frozenset({omp.AudioFormat.MP3, omp.AudioFormat.WAV}),
    omp.Duration("1h"),
)
speech_model = omp.ModelSpec(
    "round5-speech",
    "Round 5 Speech",
    (),
    operations=frozenset({omp.Operation.SPEAK, omp.Operation.TRANSCRIBE}),
    speech=speech_caps,
    transcription=transcription_caps,
)
assert typing.get_type_hints(omp.ModelSpec)["speech"] == omp.SpeechCaps | None
assert typing.get_type_hints(omp.ModelSpec)["transcription"] == omp.TranscriptionCaps | None
expect_raises(
    omp.SpecError,
    lambda: omp.ProviderSpec(
        "duplicate-models",
        "Duplicate Models",
        (),
        models=(speech_model, speech_model),
    ),
)

media_blob = omp.BlobRef(bytes(32), 3)
speech_request = omp.SpeechRequest(
    "round5-speech", "hello", "alloy", omp.AudioFormat.MP3,
)
speech_result = omp.SpeechResult(media_blob, omp.AudioFormat.MP3, 11)
transcription_request = omp.TranscriptionRequest("round5-speech", media_blob, "en")
transcription_result = omp.TranscriptionResult("hello", "en", 13)
assert speech_result.audio is media_blob and transcription_result.text == "hello"

bare_spec = omp.ProviderSpec(
    "round5-bare-provider",
    "Round 5 Bare Provider",
    (),
    models=(speech_model,),
)
bare_handle = omp.provider(bare_spec)
bare_definition = next(
    definition
    for definition in registry_module.registry.snapshot().providers
    if definition.id == bare_handle.id
)
assert bare_definition.spec is bare_spec and bare_definition.implementation is None
assert (bare_definition.priority, bare_definition.extends, bare_definition.replaces) == (
    0, None, None,
)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        bare_handle.request(omp.Operation.SPEAK, speech_request),
    )
)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        bare_handle.request(omp.Operation.TRANSCRIBE, transcription_request),
    )
)
completion_parts = (
    omp.Part.text("describe the image"),
    omp.Part.blob(media_blob, alt="image"),
)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        omp.agents.completion(completion_parts, role="vision"),
    )
)
asyncio.run(
    expect_raises_async(TypeError, omp.agents.completion((object(),), role="vision"))
)

# Dynamic commands retain full static-command metadata and use the host registration arm.
async def complete_dynamic(query, ctx):
    return ()
async def invoke_dynamic(invocation, ctx):
    return omp.ui.Prompt("dynamic")
dynamic_spec = omp.ui.CommandMountSpec(
    "foreign-prompt",
    invoke_dynamic,
    aliases=("fp",),
    description="Imported prompt",
    args=(omp.ui.Arg("topic", "Prompt topic", "<topic>"),),
    hint="/foreign-prompt <topic>",
    arg_completions=complete_dynamic,
)
assert dynamic_spec.aliases == ("fp",)
assert dynamic_spec.args == (omp.ui.Arg("topic", "Prompt topic", "<topic>"),)
assert dynamic_spec.hint == "/foreign-prompt <topic>"
assert dynamic_spec.arg_completions is complete_dynamic
asyncio.run(
    expect_raises_async(omp.NotWiredError, omp.ui.dynamic_mount(dynamic_spec))
)
class DynamicCommandBackend:
    def __init__(self):
        self.calls = []
    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        return tuple(spec.name for spec in arguments["commands"])
dynamic_backend = DynamicCommandBackend()
dynamic_control_token = omp._control_backend.set(dynamic_backend)
try:
    assert asyncio.run(omp.ui.dynamic_mount(dynamic_spec)) == ("foreign-prompt",)
finally:
    omp._control_backend.reset(dynamic_control_token)
assert dynamic_backend.calls == [
    ("omp.ui.dynamic_mount", {"commands": (dynamic_spec,)})
]
assert omp.ui._command_handlers["foreign-prompt"] is invoke_dynamic

# R-invoke: host composition opens a fresh, independently gated call.
assert asyncio.iscoroutinefunction(omp.devices.invoke)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        omp.devices.invoke(
            "notes/append",
            {"value": "draft"},
            deadline=omp.Duration("2s"),
        ),
    )
)

# Round 6 renderer inputs carry copied, read-only presentation state.
presentation_source = {"calm.enabled": True}
render_ctx = omp.ui.RenderCtx(
    width=80,
    charset=omp.ui.Charset.UNICODE,
    appearance=omp.ui.Appearance.DARK,
    graphics=omp.ui.Graphics.CELLS,
    hyperlinks=True,
    focused=False,
    collapsed=True,
    place=omp.ui.RenderPlace.TRANSCRIPT,
    presentation=presentation_source,
)
message_input = omp.MessageView(
    id="calm-message",
    kind="reasoning",
    role="assistant",
    text="thinking",
    presentation=presentation_source,
)
device_input = omp.View(
    identity=omp.ToolIdentity("read", omp.Rev.parse("1")),
    call_id="calm-call",
    updates=(),
    state=None,
    verdict=None,
    elapsed=omp.Duration("1ms"),
    phase=omp.InvocationPhase.OPEN,
    presentation=presentation_source,
)
presentation_source["calm.enabled"] = False
def mutate_presentation(render_input):
    render_input.presentation["calm.enabled"] = False
for render_input in (render_ctx, message_input, device_input):
    assert render_input.presentation == {"calm.enabled": True}
    expect_raises(
        TypeError,
        lambda render_input=render_input: mutate_presentation(render_input),
    )
assert omp.ui.RenderCtx(
    width=80,
    charset=omp.ui.Charset.UNICODE,
    appearance=omp.ui.Appearance.DARK,
    graphics=omp.ui.Graphics.CELLS,
    hyperlinks=False,
    focused=False,
    collapsed=False,
    place=omp.ui.RenderPlace.EXPORT,
).presentation == {}
assert omp.MessageView(
    id="default-message",
    kind="notice",
    role=None,
    text="",
).presentation == {}
assert omp.View(
    identity=omp.ToolIdentity("read", omp.Rev.parse("1")),
    call_id="default-call",
    updates=(),
    state=None,
    verdict=None,
    elapsed=omp.Duration("1ms"),
    phase=omp.InvocationPhase.OPEN,
).presentation == {}

# Detached outcomes retain the authoritative Environment owner and register through JobBoard.
job_ref = omp.JobRef(
	id="process:indexer:7",
	owner_kind="named_process",
	owner_name="indexer",
	owner_generation=7,
	description="knowledge index",
	media_type="application/vnd.omp.knowledge-index+json",
	lifetime="session",
)
assert dataclasses.is_dataclass(job_ref)
assert dataclasses.is_dataclass(omp.Detached(job_ref))
assert not hasattr(job_ref, "__dict__")
expect_raises(
	dataclasses.FrozenInstanceError,
	lambda: setattr(job_ref, "owner_generation", 8),
)
assert omp.Detached(job_ref).job is job_ref
assert (
	job_ref.id,
	job_ref.owner_kind,
	job_ref.owner_name,
	job_ref.owner_generation,
	job_ref.description,
	job_ref.media_type,
	job_ref.lifetime,
) == (
	"process:indexer:7",
	"named_process",
	"indexer",
	7,
	"knowledge index",
	"application/vnd.omp.knowledge-index+json",
	"session",
)

async def detached_frames():
	yield omp.Update(stage="walking")
	yield omp.Done("settled")

asyncio.run(
	expect_raises_async(
		omp.NotWiredError,
		omp.jobs.register(detached_frames(), ctx),
	)
)
class JobBoardBackend:
	def __init__(self):
		self.calls = []
	async def request(self, operation, arguments):
		self.calls.append((operation, arguments))
		return job_ref
job_board_backend = JobBoardBackend()
job_board_token = omp._control_backend.set(job_board_backend)
registered_frames = detached_frames()
try:
	assert asyncio.run(omp.jobs.register(registered_frames, ctx)) is job_ref
finally:
	omp._control_backend.reset(job_board_token)
assert job_board_backend.calls == [
	(
		"omp.jobs.register",
		{"frames": registered_frames, "context": ctx},
	)
]

# Round 6 router: decorated and mounted routes freeze their own projections.
surface_route_effects = omp.Effects(documents=omp.DocEffects(read=True))


@surface_device.subtool(
    "inspect/annotated",
    family="surface-routes",
    place="env",
    precedence=omp.Precedence.ENHANCEMENT,
    tier=omp.Tier.READ,
    effects=surface_route_effects,
    docs="Annotated child documentation.",
    summary="Inspect annotated input.",
)
async def inspect_annotated_surface_device(
    count: typing.Annotated[
        int,
        omp.Field(
            alias=("routeCount",),
            expected="a route count",
            description="Number of routes to inspect.",
        ),
    ],
):
    return count


surface_router = omp.router("mounted")


@surface_router.subtool("status/detail")
async def mounted_surface_status():
    return "mounted"


(mounted_surface_status_device,) = surface_device.mount(surface_router)
late_router = omp.router("late")


@late_router.subtool("route")
async def late_surface_route():
    return None


# Round 6 hosted tools and Core-owned realtime establishment are fully typed.
hosted_tools = frozenset({
    omp.HostedTool.WEB_SEARCH,
    omp.HostedTool.CODE_EXECUTION,
    omp.HostedTool.RETRIEVAL,
    omp.HostedTool.URL_CONTEXT,
    omp.HostedTool.DEEP_RESEARCH,
})
hosted_chat = omp.ChatCaps(hosted_tools=hosted_tools)
assert hosted_chat.hosted_tools == hosted_tools
assert typing.get_type_hints(omp.ChatCaps)["hosted_tools"] == (
    omp.Cap | frozenset[omp.HostedTool]
)

realtime_features = frozenset({
    omp.RealtimeFeature.AUDIO_IN,
    omp.RealtimeFeature.AUDIO_OUT,
    omp.RealtimeFeature.TEXT,
    omp.RealtimeFeature.TOOLS,
    omp.RealtimeFeature.SERVER_VAD,
    omp.RealtimeFeature.SEMANTIC_VAD,
    omp.RealtimeFeature.INTERRUPTION,
})
realtime_caps = omp.RealtimeCaps(
    realtime_features,
    ("alloy",),
    frozenset({omp.Transport.WEBRTC}),
)
realtime_model = omp.ModelSpec(
    "round6-realtime",
    "Round 6 Realtime",
    (),
    operations=frozenset({omp.Operation.REALTIME}),
    realtime=realtime_caps,
)
assert realtime_model.realtime is realtime_caps
assert typing.get_type_hints(omp.ModelSpec)["realtime"] == omp.RealtimeCaps | None

turn_detection = omp.TurnDetection(
    omp.RealtimeTurnDetectionMode.SERVER_VAD,
    threshold=0.5,
    silence_ms=500,
    prefix_padding_ms=300,
)
realtime_request = omp.RealtimeRequest(
    instructions="Answer briefly.",
    modalities=(omp.RealtimeModality.TEXT, omp.RealtimeModality.AUDIO),
    voice="alloy",
    input_audio=omp.Setting.require(omp.AudioFormat.PCM16),
    output_audio=omp.Setting.prefer(omp.AudioFormat.PCM16),
    turn_detection=omp.Setting.require(turn_detection),
    tools=("lookup",),
    negotiation=omp.NegotiationPolicy(
        emulation=omp.EmulationPolicy.ALLOW_LOSSLESS,
        unknown=omp.UnknownCapabilityPolicy.ALLOW_PREFERENCES,
        vendor_option_mismatch=omp.MismatchPolicy.DROP_PREFERRED,
    ),
)
assert realtime_request.input_audio.kind is omp.SettingKind.REQUIRE
assert realtime_request.output_audio.kind is omp.SettingKind.PREFER
realtime_session = omp.RealtimeSession(
    "rtc_round6",
    omp.RealtimeEndpointRef("endpoint_round6"),
    omp.RealtimeCredentialRef("credential_round6"),
    2_000_000_000_000,
    omp.Transport.WEBRTC,
)
assert realtime_session.transport is omp.Transport.WEBRTC
asyncio.run(
    expect_raises_async(
        TypeError,
        bare_handle.request(omp.Operation.REALTIME, speech_request),
    )
)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        bare_handle.request(omp.Operation.REALTIME, realtime_request),
    )
)

# Env HTTP redirects are bounded per verb and every response identifies its final URL.
assert all(
    verb.__kwdefaults__["redirects"] == 10
    for verb in (omp.env.http_get, omp.env.http_post, omp.env.http_put)
)
expect_raises(
    TypeError,
    lambda: asyncio.run(omp.env.http_get("https://example.test", redirects=True)),
)
expect_raises(
    ValueError,
    lambda: asyncio.run(omp.env.http_get("https://example.test", redirects=11)),
)
assert tuple(field.name for field in dataclasses.fields(omp.env.HttpResponse)) == (
    "status",
    "headers",
    "body",
    "final_url",
)

# The durable call outcome is a closed, public four-arm union.
argument_issue = {"path": ("query",), "expected": "a non-empty string"}
args_rejected = omp.ArgsRejected(argument_issue)
cancelled = omp.Aborted({"reason": "user cancelled"}, omp.AbortKind.CANCELLED)
assert args_rejected.issue is argument_issue
assert cancelled.kind is omp.AbortKind.CANCELLED
assert {
    typing.get_origin(arm) or arm for arm in typing.get_args(omp.CallOutcome)
} == {omp.Ok, omp.Faulted, omp.ArgsRejected, omp.Aborted}
expect_raises(
    ValueError,
    lambda: omp.Aborted(
        {"reason": "policy denied"},
        omp.AbortKind.POLICY_DENIED,
    ),
)

# Artifact references are typed throughout the public journal and namespace.
artifact_ref = omp.ArtifactRef(
    id="7",
    hash="blake3-report",
    media_type="text/plain",
    byte_len=12,
)
assert str(artifact_ref.url) == "artifact://7"
assert omp.artifacts.url(artifact_ref) == artifact_ref.url
assert typing.get_type_hints(omp.JournalEntry)["artifact"] == omp.ArtifactRef | None
assert tuple(field.name for field in dataclasses.fields(omp.ArtifactRef)) == (
    "id",
    "hash",
    "media_type",
    "byte_len",
)
assert {
    "put",
    "open_write",
    "adopt",
    "get",
    "open",
    "read",
    "stat",
    "list",
    "pin",
    "url",
}.issubset(omp.artifacts.__all__)
asyncio.run(
    expect_raises_async(
        omp.NotWiredError,
        omp.artifacts.get(artifact_ref),
    )
)

# Catalog notices remain message tokens and expose their ruled explanatory echo.
context_usage = omp.ContextUsage(
    total_tokens=120,
    context_window=1_000,
    reserve_tokens=100,
    usable_tokens=900,
    fraction=120 / 900,
    prompt_head_tokens=20,
    device_catalog_tokens=10,
    message_tokens=80,
    catalog_notice_tokens=7,
    media_tokens=10,
    compaction_epoch=2,
    threshold_fraction=0.8,
    in_flight=False,
)
assert context_usage.catalog_notice_tokens == 7
assert (
    tuple(field.name for field in dataclasses.fields(omp.ContextUsage)).index(
        "catalog_notice_tokens"
    )
    == tuple(field.name for field in dataclasses.fields(omp.ContextUsage)).index(
        "message_tokens"
    )
    + 1
)

# Configured manifest content is typed, frozen, and enumerable without a walk.
assert omp.ContentDeclaration.__dataclass_params__.frozen
assert tuple(field.name for field in dataclasses.fields(omp.ContentDeclaration)) == (
    "kind",
    "path",
    "metadata",
)
assert tuple(kind.value for kind in omp.ContentKind) == (
    "skills",
    "rules",
    "context-files",
    "prompts",
)
(content_row,) = omp.packages.own().declarations
assert content_row.kind is omp.ContentKind.SKILLS
assert content_row.path == "acme_ext/skills/review/SKILL.md"
assert dict(content_row.metadata) == {
    "name": "review",
    "description": "Review a change.",
}

# Projection hooks can drop whole result parts without discarding typed verdicts.
drop_parts = omp.DropParts(
    ids=("tool-result:42",),
    reason="historical useless result exceeds the projection budget",
)
assert drop_parts.ids == ("tool-result:42",)
assert drop_parts.reason.startswith("historical")
assert omp.DropParts.__dataclass_params__.frozen
assert tuple(field.name for field in dataclasses.fields(omp.DropParts)) == (
    "ids",
    "reason",
)
assert typing.get_type_hints(omp.ContextPatch)["drop_parts"] == list[omp.DropParts]
drop_patch = omp.ContextPatch()
assert drop_patch.is_empty()
drop_patch.drop_parts.append(drop_parts)
combined_patch = omp.ContextPatch(
    prune=[omp.Prune(ids=("stale-message",))],
    replace=[
        omp.Replace(
            ids=("verbose-message",),
            parts=(omp.Part.text("summary"),),
        )
    ],
).merge(drop_patch)
assert combined_patch.drop_parts == [drop_parts]
assert not combined_patch.is_empty()

# Hard-quota faults retain the quota identity and atomic receipt snapshot.
quota_receipt = omp.resources()
quota_error = omp.QuotaExceeded(
	quota="journal.appends",
	receipt=quota_receipt,
)
assert quota_error.quota == "journal.appends"
assert quota_error.receipt is quota_receipt
assert "journal.appends" in str(quota_error)
assert issubclass(omp.QuotaExceeded, omp.OmpError)
assert "QuotaExceeded" in omp.__all__

# Journal failures preserve their documented family and partial-append detail.
journal_entry_id = omp.EntryId(session="session-1", index=7)
journal_error = omp.JournalError(
    "only a prefix was appended",
    appended=[journal_entry_id],
)
assert omp.JournalError is omp.journal.JournalError
assert issubclass(omp.JournalError, omp.OmpError)
assert issubclass(omp.StateScopeDenied, omp.JournalError)
assert str(journal_error) == "only a prefix was appended"
assert journal_error.appended == [journal_entry_id]
assert "JournalError" in omp.__all__
assert "JournalError" in omp.journal.__all__

# FREEZE evaluates deferred availability exactly once and seals the projection.
snapshot = registry_module.freeze_declarations()
assert bare_definition in snapshot.providers
assert ("surface_device/inspect/detail", "", 1) in snapshot.tools
assert ("surface_device/inspect/annotated", "surface-routes", 1) in snapshot.tools
assert ("surface_device/mounted/status/detail", "", 1) in snapshot.tools
child_definitions = {
    child.path: child.definition for child in snapshot.child_device_definitions
}
bare_child = child_definitions["inspect/detail"]
assert bare_child.place == omp.Place.HOST
assert bare_child.family == surface_device.family
assert bare_child.precedence == surface_device.precedence
assert bare_child.tier == omp.Tier.WRITE
assert bare_child.effects is None
overridden_child = child_definitions["inspect/annotated"]
assert overridden_child.place == omp.Place.ENV
assert overridden_child.family == "surface-routes"
assert overridden_child.precedence == omp.Precedence.ENHANCEMENT
assert overridden_child.tier == omp.Tier.READ
assert overridden_child.effects is surface_route_effects
assert overridden_child.docs == "Annotated child documentation."
assert overridden_child.summary == "Inspect annotated input."
(route_count_spec,) = overridden_child.arg_specs
assert route_count_spec.path == ("count",)
assert route_count_spec.aliases == ("routeCount",)
assert route_count_spec.expected == "a route count"
assert route_count_spec.description == "Number of routes to inspect."
mounted_child = child_definitions["mounted/status/detail"]
assert mounted_child.family == surface_device.family
assert mounted_child.place == omp.Place.HOST
assert mounted_child.tier == omp.Tier.WRITE
assert str(mounted_surface_status_device.path) == (
    "surface_device/mounted/status/detail"
)
assert mounted_child.body is mounted_surface_status
expect_raises(omp.DeclarationSealed, lambda: surface_device.mount(late_router))
assert shortcut_definition in snapshot.shortcuts
assert approver_definition in snapshot.approvers
assert availability_calls == 1
assert surface_device.mounted
assert not offline_device.mounted
device_states = {
    key: (mounted, reason) for key, mounted, reason in snapshot.device_states
}
assert device_states[("offline_device", "", 1)] == (False, "offline")
argument_specs = dict(snapshot.arg_specs)
(count_spec,) = argument_specs[("arg_metadata_device", "arg-contract", 3)]
assert count_spec.path == ("count",)
assert count_spec.aliases == ("issueCount",)
assert count_spec.coerce == (omp.Coerce.INTEGER, omp.Coerce.STRIP)
assert count_spec.expected == "a positive integer" and count_spec.example == "3"
assert count_spec.description == "Requested issue count."
assert not count_spec.additional_properties
assert registry_module.registry.arg_specs(
    "arg_metadata_device", "arg-contract", 3
) == (count_spec,)
"#
				),
				None,
				None,
			)
		})
		.expect("frozen omp surface contract");
}
