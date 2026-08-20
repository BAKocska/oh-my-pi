"""Structured questionnaire device with harness-owned headless degradation."""

from __future__ import annotations

from dataclasses import dataclass as _dataclass
from typing import TYPE_CHECKING as _TYPE_CHECKING

import omp
from omp import ui

if _TYPE_CHECKING:
    from omp import Context


@_dataclass(frozen=True, slots=True)
class Choice:
    """One selectable answer offered for a question."""

    value: str
    label: str | None = None
    description: str | None = None
    preview: str | None = None


@_dataclass(frozen=True, slots=True)
class Question:
    """One choice, multi-choice, or free-form questionnaire item."""

    id: str
    question: str
    header: str | None = None
    context: str | None = None
    options: tuple[Choice, ...] = ()
    multi: bool = False
    allow_freeform: bool = True
    allow_note: bool = False
    recommended: str | None = None


@_dataclass(frozen=True, slots=True)
class AskUserArgs:
    """Final effective arguments for the questionnaire device."""

    questions: tuple[Question, ...]


@_dataclass(frozen=True, slots=True)
class Answer:
    """One completed answer returned by the presentation client."""

    question_id: str
    selected: tuple[str, ...] = ()
    freeform: str | None = None
    note: str | None = None
    timed_out: bool = False


@_dataclass(frozen=True, slots=True)
class AskUserVerdict:
    """Answers, cancellation state, and questions to relay when UI is unavailable."""

    answers: tuple[Answer, ...] = ()
    questions_for_model: tuple[Question, ...] = ()
    cancelled: bool = False
    reason: str | None = None


def _dialog_question(question: Question) -> ui.AskQuestion:
    return ui.AskQuestion(
        id=question.id,
        question=question.question,
        header=question.header,
        context=ui.md(question.context) if question.context else None,
        options=tuple(
            ui.SelectItem(
                value=choice.value,
                label=choice.label,
                desc=choice.description,
                preview=ui.md(choice.preview) if choice.preview else None,
            )
            for choice in question.options
        ),
        multi=question.multi,
        allow_freeform=question.allow_freeform,
        allow_note=question.allow_note,
        recommended=question.recommended,
    )


@omp.device("ask_user", family="questionnaire", rev=1)
async def ask_user(args: AskUserArgs, ctx: Context) -> AskUserVerdict:
    """Present all questions once and return structured answers or a relay verdict."""

    del ctx
    outcome = await ui.ask_user(
        tuple(_dialog_question(question) for question in args.questions),
        options=ui.DialogOptions(timeout=omp.Duration("10m")),
    )

    if outcome.cancelled:
        reason = outcome.reason.value if outcome.reason is not None else None
        return AskUserVerdict(
            questions_for_model=(
                args.questions
                if outcome.reason is ui.DialogCancel.UNAVAILABLE
                else ()
            ),
            cancelled=True,
            reason=reason,
        )

    return AskUserVerdict(
        answers=tuple(
            Answer(
                question_id=answer.question_id,
                selected=answer.selected,
                freeform=answer.freeform,
                note=answer.note,
                timed_out=answer.timed_out,
            )
            for answer in outcome.answers
        )
    )
