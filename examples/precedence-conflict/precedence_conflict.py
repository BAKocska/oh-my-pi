"""Mechanical collision probes for the frozen declaration registries."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable

import omp
from omp._registry import DeclarationRegistry, DeviceDefinition
from omp.ui import _device_renderers


@dataclass(frozen=True, slots=True)
class ProbeObservation:
    """One documented collision rule and the frozen registry's observation."""

    collision: str
    documented: str
    observed: str
    conformant: bool


async def _device_body(args: object, ctx: omp.Context) -> object:
    return args


async def _handler(payload: object, ctx: omp.Context) -> None:
    return None


def _prompt_one(ctx: omp.PromptContext) -> str:
    return "one"


def _prompt_two(ctx: omp.PromptContext) -> str:
    return "two"


def _render_one(view: object, ctx: omp.ui.RenderCtx) -> None:
    return None


def _render_two(view: object, ctx: omp.ui.RenderCtx) -> None:
    return None


def _definition(
    name: str,
    family: str,
    precedence: int,
    replaces: str | None = None,
) -> DeviceDefinition:
    return DeviceDefinition(
        name=name,
        family=family,
        rev=1,
        place=omp.Place.HOST,
        summary=None,
        docs=None,
        schema=None,
        examples=(),
        available=None,
        precedence=precedence,
        replaces=replaces,
        intents=(),
        effects=None,
        tier=omp.Tier.WRITE,
        deadline=None,
        aliases=None,
        body=_device_body,
    )


def _raises(error_type: type[BaseException], operation: Callable[[], object]) -> str:
    try:
        operation()
    except error_type as error:
        return str(error)
    except BaseException as error:  # pragma: no cover - makes a wrong taxonomy explicit
        raise AssertionError(
            f"expected {error_type.__name__}, got {type(error).__name__}: {error}"
        ) from error
    raise AssertionError(f"expected {error_type.__name__}")


def _device_observations() -> tuple[ProbeObservation, ProbeObservation, ProbeObservation]:
    ordered = DeclarationRegistry()
    ordered.configure_manifest(extension="publisher-a/extension-a")
    ordered.register_tool(
        "precedence_probe",
        "family_a",
        1,
        object(),
        definition=_definition("precedence_probe", "family_a", omp.Precedence.DEFAULT),
    )
    ordered.register_tool(
        "precedence_probe",
        "family_b",
        1,
        object(),
        definition=_definition(
            "precedence_probe",
            "family_b",
            omp.Precedence.ENHANCEMENT,
            replaces="publisher-a/extension-a",
        ),
    )
    definitions = ordered.device_definitions()
    assert len(definitions) == 2
    assert not hasattr(ordered, "resolve_device")

    separate_a = DeclarationRegistry()
    separate_a.configure_manifest(extension="publisher-a/extension-a")
    separate_b = DeclarationRegistry()
    separate_b.configure_manifest(extension="publisher-b/extension-b")
    for registry, family in ((separate_a, "family_a"), (separate_b, "family_b")):
        registry.register_tool(
            "separate_probe",
            family,
            1,
            object(),
            definition=_definition(
                "separate_probe", family, omp.Precedence.ENHANCEMENT
            ),
        )
    assert len(separate_a.device_definitions()) == 1
    assert len(separate_b.device_definitions()) == 1

    tied = DeclarationRegistry()
    tied.configure_manifest(extension="publisher-a/extension-a")
    tied.register_tool(
        "tie_probe",
        "family_a",
        1,
        object(),
        definition=_definition("tie_probe", "family_a", omp.Precedence.DEFAULT),
    )
    tie_message = _raises(
        omp.PrecedenceConflict,
        lambda: tied.register_tool(
            "tie_probe",
            "family_b",
            1,
            object(),
            definition=_definition(
                "tie_probe",
                "family_b",
                omp.Precedence.DEFAULT,
                replaces="publisher-a/extension-a",
            ),
        ),
    )
    assert repr(("tie_probe", "family_a", 1)) in tie_message
    assert repr(("tie_probe", "family_b", 1)) in tie_message
    assert "publisher-a/extension-a" in tie_message
    assert not hasattr(omp, "PrecedenceTie")

    core_message = _raises(
        omp.DeviceNameError,
        lambda: omp.device(
            "grep", precedence=int(omp.Precedence.CORE) + 1
        ),
    )
    assert "Precedence.CORE" in core_message

    return (
        ProbeObservation(
            "devices, ordered precedence",
            "core registry chooses the live claimant and retains qualified shadows; frozen Python enforces intra-extension claims only",
            "the frozen registry retains both local declarations and deliberately exposes no cross-extension resolver",
            True,
        ),
        ProbeObservation(
            "devices, equal precedence",
            "PrecedenceConflict names both claimant keys and the source package",
            "PrecedenceConflict names both device keys and source package 'publisher-a/extension-a'",
            True,
        ),
        ProbeObservation(
            "core-name claim above CORE",
            "DeviceNameError at load",
            "DeviceNameError raised before decoration",
            True,
        ),
    )


def _renderer_observation() -> ProbeObservation:
    key = ("precedence_renderer_probe", "probe", 7)
    _device_renderers.pop(key, None)
    try:
        omp.renderer(key[0], family=key[1], rev=key[2])(_render_one)
        message = _raises(
            omp.DuplicateRenderer,
            lambda: omp.renderer(key[0], family=key[1], rev=key[2])(
                _render_two
            ),
        )
        assert repr(key) in message
        assert "_render_one" in message
        assert "_render_two" in message
        assert issubclass(omp.DuplicateRenderer, omp.DuplicateRegistration)
    finally:
        _device_renderers.pop(key, None)
    return ProbeObservation(
        "renderers, same (name, family, rev)",
        "DuplicateRenderer at import, with incumbent and second claimant named",
        "DuplicateRenderer subclasses DuplicateRegistration and names _render_one and _render_two",
        True,
    )


def _claimant_registries() -> tuple[DeclarationRegistry, DeclarationRegistry]:
    first = DeclarationRegistry()
    first.configure_manifest(extension="publisher-a/extension-a")
    second = DeclarationRegistry()
    second.configure_manifest(extension="publisher-b/extension-b")
    return first, second


def _declaration_observations() -> tuple[ProbeObservation, ProbeObservation, ProbeObservation]:
    commands = DeclarationRegistry()
    commands.register_command("probe", (), "", (), None, None, _handler)
    command_message = _raises(
        omp.DuplicateRegistration,
        lambda: commands.register_command("probe", (), "", (), None, None, _handler),
    )
    assert "'probe'" in command_message
    assert "precedence_conflict._handler" in command_message
    for claimant in _claimant_registries():
        claimant.register_command("cross-probe", (), "", (), None, None, _handler)
        assert len(claimant.command_definitions()) == 1


    shortcuts = DeclarationRegistry()
    shortcuts.register_shortcut("ctrl+alt+p", "one", "", None, _handler)
    shortcut_message = _raises(
        omp.DuplicateRegistration,
        lambda: shortcuts.register_shortcut(
            "ctrl+alt+p", "two", "", None, _handler
        ),
    )
    assert "ctrl+alt+p" in shortcut_message
    assert "precedence_conflict._handler" in shortcut_message
    for claimant in _claimant_registries():
        claimant.register_shortcut("ctrl+alt+x", "cross", "", None, _handler)
        assert len(claimant.shortcut_definitions()) == 1


    prompts = DeclarationRegistry()
    prompts.register_prompt_slot("memory", 10, omp.SlotClass.STABLE, _prompt_one)
    prompts.register_prompt_slot("memory", 10, omp.SlotClass.STABLE, _prompt_two)
    prompt_rows = prompts.snapshot().prompt_slots
    assert len(prompt_rows) == 2
    assert {row.renderer for row in prompt_rows} == {_prompt_one, _prompt_two}
    for claimant, renderer in zip(
        _claimant_registries(), (_prompt_one, _prompt_two), strict=True
    ):
        claimant.register_prompt_slot(
            "memory", 10, omp.SlotClass.STABLE, renderer
        )
        assert len(claimant.snapshot().prompt_slots) == 1


    return (
        ProbeObservation(
            "commands, same name",
            "the core arbitrates cross-extension sources; frozen intra-extension duplicates name the incumbent holder",
            "same-registry duplicate raises DuplicateRegistration naming precedence_conflict._handler as holder",
            True,
        ),
        ProbeObservation(
            "shortcuts, same chord",
            "manifest precedence shadows one extension; core incumbents are refused with ShortcutError",
            "same-registry duplicate raises DuplicateRegistration naming the chord and incumbent handler",
            False,
        ),
        ProbeObservation(
            "prompt slots, same slot and priority",
            "contributions are additive; ties order by (layer, publisher, extension_id)",
            "both contributions are accepted, but the local snapshot has no claimant ordering data",
            False,
        ),
    )


def _provider_observation() -> ProbeObservation:
    providers = DeclarationRegistry()
    providers.register_provider("precedence-provider", object(), priority=10)
    message = _raises(
        omp.DuplicateRegistration,
        lambda: providers.register_provider(
            "precedence-provider", object(), priority=10
        ),
    )
    assert "precedence-provider" in message
    assert "ProviderDefinition" in message
    assert [row.id for row in providers.provider_definitions()] == [
        "precedence-provider"
    ]
    for claimant in _claimant_registries():
        claimant.register_provider("cross-provider", object(), priority=10)
        assert [row.id for row in claimant.provider_definitions()] == [
            "cross-provider"
        ]
    return ProbeObservation(
        "providers, equal priority",
        "activation fails naming both declarations; provider id remains absent",
        "second local registration fails immediately with DuplicateRegistration naming the incumbent ProviderDefinition, and the first provider remains present",
        False,
    )


def smoke() -> tuple[ProbeObservation, ...]:
    """Drive every collision class expressible by the frozen Python registries."""

    observations = (
        *_device_observations(),
        _renderer_observation(),
        *_declaration_observations(),
        _provider_observation(),
    )
    assert len(observations) == 8
    assert sum(observation.conformant for observation in observations) == 5
    return observations
