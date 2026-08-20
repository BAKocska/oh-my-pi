"""Organization conventions with a principal-local denial fallback."""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass

import omp

_ORGANIZATION = omp.StateScope.ORGANIZATION
_USER = omp.StateScope.USER


@omp.entry_kind(
    "examples.org-registry.convention", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class ConventionChange:
    """Journal one organization convention mutation or local fallback reset."""

    name: str
    value: str
    deleted: bool = False
    fallback_cleared: bool = False


@dataclass(frozen=True, slots=True)
class RegistryArgs:
    """Set, remove, get, or list shared convention pins."""

    op: str
    name: str | None = None
    value: str | None = None


@dataclass(frozen=True, slots=True)
class RegistryItem:
    """One effective convention and the scope that supplied it."""

    name: str
    value: str
    scope: str


@dataclass(frozen=True, slots=True)
class RegistryResult:
    """Return the effective durable registry and any write degradation."""

    op: str
    items: tuple[RegistryItem, ...]
    organization_readable: bool
    write_scope: str | None = None


def _fold(
    records: Iterable[object], *, fallback: bool
) -> dict[str, ConventionChange]:
    """Fold one durable journal while retaining deletion markers."""

    values: dict[str, ConventionChange] = {}
    for record in records:
        change = getattr(record, "value", None)
        if not isinstance(change, ConventionChange):
            continue
        if fallback and change.fallback_cleared:
            values.pop(change.name, None)
        else:
            values[change.name] = change
    return values


async def _registry() -> tuple[dict[str, RegistryItem], bool]:
    """Read shared truth, then overlay this principal's denied-write fallback."""

    organization_changes: dict[str, ConventionChange] = {}
    organization_readable = True
    try:
        organization = await omp.state.entries(
            ConventionChange, scope=_ORGANIZATION
        )
    except omp.StateScopeDenied:
        organization_readable = False
    else:
        organization_changes = _fold(organization, fallback=False)

    user = await omp.state.entries(ConventionChange, scope=_USER)
    user_changes = _fold(user, fallback=True)

    values = {
        name: RegistryItem(name=name, value=change.value, scope=_ORGANIZATION.value)
        for name, change in organization_changes.items()
        if not change.deleted
    }
    for name, change in user_changes.items():
        if change.deleted:
            values.pop(name, None)
        else:
            values[name] = RegistryItem(name=name, value=change.value, scope=_USER.value)
    return values, organization_readable


async def _append(change: ConventionChange) -> omp.StateScope:
    """Prefer the shared journal and degrade only on the documented denial."""

    try:
        await omp.state.append(change, scope=_ORGANIZATION)
    except omp.StateScopeDenied:
        await omp.state.append(change, scope=_USER)
        return _USER

    await omp.state.append(
        ConventionChange(
            name=change.name,
            value="",
            fallback_cleared=True,
        ),
        scope=_USER,
    )
    return _ORGANIZATION


def _selected(
    values: dict[str, RegistryItem], name: str | None
) -> tuple[RegistryItem, ...]:
    """Select one pin or return the whole registry in stable name order."""

    if name is not None:
        item = values.get(name)
        return () if item is None else (item,)
    return tuple(values[key] for key in sorted(values))


def _mutation(args: RegistryArgs) -> ConventionChange:
    """Validate one journal mutation without inventing implicit values."""

    if args.name is None or not args.name.strip():
        raise ValueError(f"{args.op} requires a non-empty name")
    name = args.name.strip()
    if args.op == "set":
        if args.value is None or not args.value.strip():
            raise ValueError("set requires a non-empty value")
        return ConventionChange(name=name, value=args.value.strip())
    return ConventionChange(name=name, value="", deleted=True)


@omp.device("org_registry", family="organization", rev=1, place="host")
async def org_registry(args: RegistryArgs, ctx: omp.Context) -> RegistryResult:
    """Read or mutate organization conventions with a user-scope fallback."""

    del ctx
    if args.op not in {"set", "remove", "get", "list"}:
        raise ValueError("op must be set, remove, get, or list")
    if args.op == "list" and (args.name is not None or args.value is not None):
        raise ValueError("list accepts neither name nor value")
    if args.op == "get":
        if args.name is None or not args.name.strip() or args.value is not None:
            raise ValueError("get requires a non-empty name and no value")
    if args.op == "remove" and args.value is not None:
        raise ValueError("remove accepts no value")

    write_scope: omp.StateScope | None = None
    if args.op in {"set", "remove"}:
        write_scope = await _append(_mutation(args))

    values, organization_readable = await _registry()
    name = args.name.strip() if args.op == "get" and args.name is not None else None
    return RegistryResult(
        op=args.op,
        items=_selected(values, name),
        organization_readable=organization_readable,
        write_scope=None if write_scope is None else write_scope.value,
    )
