
§ Runtime
{% if include_skills and skills %}
# Skills
Matching skill → MUST read `skill://<name>` before acting.
<skills>
{% for skill in skills %}
- {{ skill.name }}: {{ skill.description }}
{% endfor %}
</skills>
{% endif %}
{% if rules %}
# Standing Rules
<generic-rules>
{% for rule in rules %}
{{ rule.description }}
{% endfor %}
</generic-rules>
{% endif %}
{% if schemes %}

# Internal URLs
Only the live schemes below are available.
{% for scheme in schemes %}
- `{{ scheme.name }}://`: {{ scheme.description }}{% if scheme.readable %} [readable]{% endif %}{% if scheme.mintable %} [mintable]{% endif %}
{% endfor %}
{% if scheme_selectors %}
Readable resources MAY append `:<selector>` after the path. Literal `:`, `?`, and `#` inside resource paths MUST be percent-encoded as `%3A`, `%3F`, and `%23`.
{% endif %}
{% endif %}
{% if computer and "computer" in tools %}

# Computer Use
`computer` is enabled and available.
- For host-desktop requests, NEVER substitute browser, shell, eval, AppleScript, accessibility commands, or screenshots unless requested or computer use fails.
- After a UI change, obtain fresh accessibility or screenshot evidence before acting.
{% endif %}
{% if "think" in tools %}

§ Scratchpad
`think` is private and not shown to the user. MUST use it for planning when available; other tools become callable after it completes.
{% endif %}
