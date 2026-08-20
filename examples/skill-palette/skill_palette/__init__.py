"""A durable command palette over this extension's bundled skill declarations."""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass

import omp
from omp import ui


_SCOPE = omp.StateScope.USER


@dataclass(frozen=True, slots=True)
class Skill:
    """One manifest-declared skill shown by the palette."""

    name: str
    description: str
    path: str

    @property
    def uri(self) -> str:
        """Return the core-owned resource reference for this skill."""

        return f"skill://{self.name}"


_BUNDLED_SKILLS = (
    Skill(
        "palette-focused-review",
        "Review a change by prioritizing correctness, regressions, and evidence.",
        "skill_palette/skills/focused-review/SKILL.md",
    ),
    Skill(
        "palette-change-summary",
        "Summarize a change as behavior, risk, and verification.",
        "skill_palette/skills/change-summary/SKILL.md",
    ),
)


class DeclarationIntrospectionUnavailable(RuntimeError):
    """The frozen package snapshot does not expose content declarations."""


@omp.entry_kind(
    "examples.skill-palette.selected", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class SkillSelected:
    """Record one skill selection for user-scoped recency ranking."""

    name: str


def _declared_skills() -> tuple[Skill, ...]:
    """Read this package's skill rows or report the frozen introspection gap."""

    distribution = omp.packages.own()
    declarations = getattr(distribution, "declarations", None)
    if declarations is None:
        raise DeclarationIntrospectionUnavailable(
            "omp.packages.Distribution exposes files but not manifest declarations"
        )

    skills: list[Skill] = []
    for row in declarations:
        if not isinstance(row, Mapping) or row.get("kind") != "skills":
            continue
        skills.append(
            Skill(
                name=str(row["name"]),
                description=str(row["description"]),
                path=str(row["path"]),
            )
        )
    return tuple(skills)


def _palette_skills() -> tuple[Skill, ...]:
    """Return declared skills, using the manifest mirror until introspection lands."""

    try:
        skills = _declared_skills()
    except (omp.PackageError, DeclarationIntrospectionUnavailable):
        return _BUNDLED_SKILLS
    return skills


def _recent_names(records: Iterable[object]) -> tuple[str, ...]:
    """Reduce newest-first unique skill names from ascending state records."""

    recent: list[str] = []
    for record in reversed(tuple(records)):
        value = getattr(record, "value", None)
        if isinstance(value, SkillSelected) and value.name not in recent:
            recent.append(value.name)
    return tuple(recent)


def _rank_skills(skills: Iterable[Skill], recent: Iterable[str]) -> tuple[Skill, ...]:
    """Rank recently selected skills first, then use stable name order."""

    positions = {name: index for index, name in enumerate(recent)}
    after_recency = len(positions)
    return tuple(
        sorted(
            skills,
            key=lambda skill: (
                positions.get(skill.name, after_recency),
                skill.name.casefold(),
                skill.name,
            ),
        )
    )


@omp.command(
    "palette",
    description="Select and apply a bundled skill",
)
async def palette(args: ui.Invocation, ctx: omp.Context) -> ui.Consumed | ui.Prompt:
    """Open the skill picker, persist recency, and submit a skill reference."""

    del args, ctx
    skills = _palette_skills()
    records = await omp.state.entries(SkillSelected, scope=_SCOPE)
    ordered = _rank_skills(skills, _recent_names(records))
    outcome = await ui.select(
        "Skill palette",
        tuple(
            ui.SelectItem(
                value=skill.name,
                label=skill.name,
                desc=skill.description,
                preview=ui.text(skill.uri),
            )
            for skill in ordered
        ),
        options=ui.DialogOptions(
            help="Type to filter · Enter applies · Esc cancels",
            overlay=ui.OverlayOptions(width=72, max_height=18),
        ),
    )
    if outcome.cancelled or outcome.value is None:
        return ui.Consumed()

    selected = next((skill for skill in skills if skill.name == outcome.value), None)
    if selected is None:
        return ui.Consumed(ui.text("The selected skill is no longer declared."))

    await omp.state.append(SkillSelected(selected.name), scope=_SCOPE)
    return ui.Prompt(
        f"Apply {selected.uri} to this turn. Read it and follow its instructions.",
        submit=True,
    )
