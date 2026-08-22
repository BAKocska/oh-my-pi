"""Deterministic prompt templating and canonicalization (the scribe engine).

The same Jinja-flavored template language omp uses for every prompt it
composes: ``{{ expr }}``, ``{% statement %}``, ``{# comment #}`` over a
props ``dict``, plus the opt-in post-render canonicalization pass used on
system prompts before hashing and journaling. Rendering is pure — no clock,
no environment, no randomness — and the helper set is the fixed builtin
registry, so a template renders identical bytes for identical props on
every host.
"""

from __future__ import annotations

from typing import Any

from _omp import (
    Template,
    TemplateError,
    _scribe_canonicalize,
)

def render(
    source: str, props: dict[str, Any] | None = None, *, name: str = "template"
) -> str:
    """Compile and render ``source`` in one shot.

    Compiles on every call; hold a :class:`Template` for repeated renders.
    Raises :class:`TemplateError` on a syntax error, an unknown helper, an
    undefined value reaching a strict sink, or a shape mismatch.
    """
    return Template(source, name=name).render(props)


def canonicalize(text: str) -> str:
    """Canonicalize rendered prompt text.

    Outside code fences and inline code spans: strips HTML comments,
    trims trailing whitespace, collapses blank-line runs, compacts GFM
    table separators, and aliases RFC 2119 phrasing. Opt-in by design:
    :meth:`Template.render` never applies it.
    """
    return _scribe_canonicalize(text)


__all__ = (
    "Template",
    "TemplateError",
    "canonicalize",
    "render",
)
