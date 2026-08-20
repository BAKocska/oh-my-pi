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
    "sessions", "journal", "index", "diagnostics", "urls", "devices",
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
registry_module.configure_manifest(extension="acme-ext")

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
    ValueError,
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
    ValueError,
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
response = omp.env.HttpResponse(200, {"content-type": "application/json"}, b'{"ok": true}')
assert response.json() == {"ok": True}
asyncio.run(
    expect_raises_async(
        TypeError, omp.env.proc.ensure("invalid-ready", "true", ready=object())
    )
)
asyncio.run(expect_raises_async(omp.NotWiredError, omp.env.http_get("https://example.test")))
expect_raises(omp.NotWiredError, lambda: omp.env.Process("p", 1).endpoint)

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
asyncio.run(expect_raises_async(omp.NotWiredError, omp.devices.list()))
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
    ValueError,
    lambda: omp.ProviderSpec(
        "overlay-provider", "Conflict", (),
        model_overlays=(model_overlay, model_overlay),
    ),
)
expect_raises(
    ValueError,
    lambda: omp.provider(overlay_spec),
)
expect_raises(
    ValueError,
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

# FREEZE evaluates deferred availability exactly once and seals the projection.
snapshot = registry_module.freeze_declarations()
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
