//! Embedded behavior contract for the `omp.scribe` template surface.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn scribe_templates_render_deterministically() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import omp
import omp.scribe as scribe


def expect_raises(error_type, call):
    try:
        call()
    except error_type as error:
        return error
    raise AssertionError(f"expected {error_type.__name__}")


# Export closure.
for name in scribe.__all__:
    getattr(scribe, name)
assert scribe.TemplateError.__mro__.index(omp.OmpError) > 0

# Compile once, render repeatedly, deterministically.
template = scribe.Template(
    "{% if items %}{{ items | length | pluralize('item') }}:\n"
    "{{ items | bullets }}{% endif %}",
    name="summary",
)
assert template.name == "summary"
assert template.referenced_keys == ("items",)
props = {"items": ["alpha", "beta"]}
first = template.render(props)
assert first == "2 items:\n- alpha\n- beta"
assert first == template.render(props)

# Empty props and missing keys are falsy in conditions.
assert template.render() == ""
assert template.render({}) == ""

# One-shot render, full value model, key-ordered map iteration.
assert scribe.render("{{ a }} {{ b }} {{ c }} {{ d }}",
                     {"a": True, "b": 3, "c": 2.5, "d": "x"}) == "true 3 2.5 x"
assert scribe.render("{{ v }}", {"v": None}) == ""
assert scribe.render("{{ v }}", {"v": {"b": [1, 2], "a": "s"}}) == '{"a":"s","b":[1,2]}'
assert scribe.render("{{ v[1] }}", {"v": (10, 20)}) == "20"
assert (
    scribe.render("{% for pair in m %}{{ pair[0] }}={{ pair[1] }};{% endfor %}",
                  {"m": {"z": 1, "a": 2}})
    == "a=2;z=1;"
)

# Blocks and standalone-line whitespace stripping.
assert (
    scribe.render("{% xml \"note\" %}\n{{ text }}\n{% endxml %}", {"text": "hi"})
    == "<note>\nhi\n</note>"
)
assert scribe.render("{% xml \"note\" %}\n\n{% endxml %}", {}) == ""
assert (
    scribe.render("{% codeblock \"py\" %}\nx = 1\n{% endcodeblock %}", {})
    == "```py\nx = 1\n```"
)

# Compile-time failures: syntax and unknown helpers.
syntax = expect_raises(scribe.TemplateError, lambda: scribe.Template("{% if x %}y", name="bad"))
assert "bad" in str(syntax)
unknown = expect_raises(scribe.TemplateError, lambda: scribe.Template("{{ x | nope }}"))
assert "nope" in str(unknown) or "filter" in str(unknown)

# Render-time failure: undefined value at a strict sink, with location.
undefined = expect_raises(
    scribe.TemplateError, lambda: scribe.render("{{ user.name }}", {}, name="greet")
)
message = str(undefined)
assert "user.name" in message and "greet" in message and "1:" in message

# Optional chaining and default keep absence lenient.
assert scribe.render("{{ user?.name | default('anon') }}", {}) == "anon"

# Props shape errors are Python-typed at the boundary.
expect_raises(TypeError, lambda: scribe.render("{{ v }}", {1: "x"}))
expect_raises(TypeError, lambda: scribe.render("{{ v }}", {"v": {1: "x"}}))
expect_raises(TypeError, lambda: scribe.render("{{ v }}", {"v": object()}))
expect_raises(OverflowError, lambda: scribe.render("{{ v }}", {"v": 2**80}))

# Canonicalization is opt-in and never touches code fences.
canon = scribe.canonicalize(
    "Head\n\n\n<!-- gone -->\nYou MUST NOT stall.\n\n```\nMUST NOT  \n```\n\n"
)
assert canon == "Head\n\nYou NEVER stall.\n\n```\nMUST NOT  \n```"
assert "MUST NOT  " in canon.split("```")[1]
assert scribe.Template("x").render() == "x"
assert repr(scribe.Template("x", name="n")) == 'Template(name="n")'
"#
				),
				None,
				None,
			)
		})
		.expect("scribe template contract");
}
