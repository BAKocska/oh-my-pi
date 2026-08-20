"""Named model and device profiles backed by the session journal."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass

import omp
from omp import ui


@dataclass(frozen=True, slots=True)
class Profile:
    """One configured model, route, thinking level, and device set."""

    name: str
    model: omp.ModelRef
    route: omp.RouteRef
    thinking: omp.Effort
    devices: frozenset[str]


@omp.entry_kind("examples.profiles.applied", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class ProfileApplied:
    """Record the complete profile selected for subsequent turns."""

    name: str
    model: omp.ModelRef
    route: omp.RouteRef
    thinking: omp.Effort
    devices: tuple[str, ...]


def _mapping(value: object, field: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise ValueError(f"profile {field} must be a table")
    return value


def _profiles(ctx: omp.Context) -> tuple[Profile, ...]:
    """Decode and validate the profile tables in extension settings."""

    raw_profiles = ctx.settings.get("profiles", ())
    if not isinstance(raw_profiles, Sequence) or isinstance(raw_profiles, (str, bytes)):
        raise ValueError("settings.profiles must be an array of tables")

    profiles: list[Profile] = []
    names: set[str] = set()
    for raw_profile in raw_profiles:
        profile = _mapping(raw_profile, "definition")
        model = _mapping(profile.get("model"), "model")
        route = _mapping(profile.get("route"), "route")
        name = str(profile.get("name", "")).strip()
        if not name:
            raise ValueError("profile names must not be empty")
        if name in names:
            raise ValueError(f"duplicate profile name: {name}")

        raw_devices = profile.get("devices", ())
        if not isinstance(raw_devices, Sequence) or isinstance(raw_devices, (str, bytes)):
            raise ValueError(f"profile {name!r} devices must be an array")
        devices = frozenset(str(path).strip() for path in raw_devices)
        if "" in devices:
            raise ValueError(f"profile {name!r} contains an empty device path")

        profiles.append(
            Profile(
                name=name,
                model=omp.ModelRef(
                    provider=str(model.get("provider", "")),
                    api=str(model.get("api", "")),
                    model=str(model.get("model", "")),
                ),
                route=omp.RouteRef(
                    provider=str(route.get("provider", "")),
                    route=str(route.get("route", "")),
                ),
                thinking=omp.Effort(str(profile.get("thinking", ""))),
                devices=devices,
            )
        )
        if not all(
            (
                profiles[-1].model.provider,
                profiles[-1].model.api,
                profiles[-1].model.model,
                profiles[-1].route.provider,
                profiles[-1].route.route,
            )
        ):
            raise ValueError(f"profile {name!r} has an incomplete model or route reference")
        names.add(name)

    if not profiles:
        raise ValueError("settings.profiles must define at least one profile")
    return tuple(profiles)


def _latest_applied() -> ProfileApplied | None:
    """Return the latest durable profile selection in this session."""

    for record in reversed(omp.journal.entries(ProfileApplied)):
        if isinstance(record.value, ProfileApplied):
            return record.value
    return None


def _availability_deltas(
    profiles: Sequence[Profile], previous: ProfileApplied | None, selected: Profile
) -> tuple[omp.AvailabilityDelta, ...]:
    """Build the minimal deterministic mounted-set transition for a profile."""

    desired = selected.devices
    if previous is None:
        changed = set().union(*(profile.devices for profile in profiles))
    else:
        changed = set(previous.devices) ^ desired
    return tuple(
        omp.AvailabilityDelta(
            path=path,
            mounted=path in desired,
            reason=None if path in desired else f"disabled by profile {selected.name}",
        )
        for path in sorted(changed)
    )


async def _complete_profile_names(
    query: ui.ArgQuery, ctx: omp.Context
) -> tuple[ui.CompletionItem, ...]:
    """Complete the first command argument from configured profile names."""

    if query.argv:
        return ()
    prefix = query.prefix.casefold()
    try:
        profiles = _profiles(ctx)
    except (TypeError, ValueError):
        return ()
    return tuple(
        ui.CompletionItem(
            insert=profile.name,
            label=profile.name,
            desc=(
                f"{profile.model.provider}/{profile.model.model} · "
                f"thinking {profile.thinking.value}"
            ),
            group="Profiles",
        )
        for profile in sorted(profiles, key=lambda item: item.name.casefold())
        if profile.name.casefold().startswith(prefix)
    )


@omp.command(
    "profile",
    description="Atomically select a configured model and device profile",
    args=(ui.Arg("name", "Configured profile name", usage="<profile>"),),
    hint="<profile>",
    arg_completions=_complete_profile_names,
)
async def profile(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Select one configured profile and persist the complete selection."""

    if len(inv.argv) != 1:
        return ui.Consumed(ui.text("Usage: /profile <profile>"))
    try:
        profiles = _profiles(ctx)
    except (TypeError, ValueError) as error:
        return ui.Consumed(ui.text(f"Invalid profile settings: {error}"))

    selected = next((item for item in profiles if item.name == inv.argv[0]), None)
    if selected is None:
        return ui.Consumed(ui.text(f"Unknown profile: {inv.argv[0]}"))

    previous = _latest_applied()
    deltas = _availability_deltas(profiles, previous, selected)
    await omp.devices.set_availability(*deltas)
    omp.journal.append(
        ProfileApplied(
            name=selected.name,
            model=selected.model,
            route=selected.route,
            thinking=selected.thinking,
            devices=tuple(sorted(selected.devices)),
        )
    )
    return ui.Consumed(ui.text(f"Profile selected: {selected.name}"))


@omp.hook(
    "turn_start",
    phase=omp.HookPhase.TRANSFORM,
    order=0,
    on_failure=omp.OnFailure.DEFER,
)
async def apply_profile(
    event: omp.TurnStartEvent, ctx: omp.Context
) -> omp.Modify | omp.Defer:
    """Apply the latest durable profile's model and route to this turn."""

    del event, ctx
    selected = _latest_applied()
    if selected is None:
        return omp.Defer("no profile has been selected")
    return omp.Modify(
        patch={"model": selected.model, "route": selected.route},
        reason=f"profile {selected.name}",
    )
