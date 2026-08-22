//! Embedded behavior contract for eval-prelude helper declarations.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn prelude_declarations_are_frozen_and_validated() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import dataclasses

import omp
import omp._registry as registry_module


def expect_raises(error_type, call):
    try:
        call()
    except error_type as error:
        return error
    raise AssertionError(f"expected {error_type.__name__}")


registry_module.configure_manifest(
    extension="prelude-contract",
    tools=(
        ("bare_helper", "prelude", 1),
        ("configured_helper", "prelude", 3),
    ),
)


@omp.prelude
def bare_helper(value: int):
    """Return the bare value.

    The complete docstring is retained for generated help.
    """
    return {"value": value}


@omp.prelude(name="configured_helper", rev=3, summary="Configured helper.")
async def internal_name(value: int = 3, *, mode: str = "fast"):
    """This first line is overridden by the explicit summary."""
    return {"value": value, "mode": mode}


assert bare_helper.__name__ == "bare_helper"
assert internal_name.__name__ == "internal_name"
assert "prelude" in omp.__all__


def candidate(value=None):
    return value


invalid_error = expect_raises(
    omp.DeviceNameError,
    lambda: omp.prelude("Invalid-Name")(candidate),
)
assert "Invalid-Name" in str(invalid_error)
reserved_error = expect_raises(
    omp.DeviceNameError,
    lambda: omp.prelude("resolve")(candidate),
)
assert "resolve" in str(reserved_error)
keyword_error = expect_raises(
    omp.DeviceNameError,
    lambda: omp.prelude("class")(candidate),
)
assert "class" in str(keyword_error)
revision_error = expect_raises(
    ValueError,
    lambda: omp.prelude("zero_revision", rev=0)(candidate),
)
assert "positive unsigned 16-bit integer" in str(revision_error)


def positional_only(value, /):
    return value


def variadic(*values):
    return values


def keyword_variadic(**values):
    return values


for function, parameter_name in (
    (positional_only, "value"),
    (variadic, "values"),
    (keyword_variadic, "values"),
):
    error = expect_raises(omp.SchemaError, lambda function=function: omp.prelude(function))
    assert parameter_name in str(error)

def unicode_parameter(é):
    return é


unicode_parameter_error = expect_raises(
    omp.SchemaError,
    lambda: omp.prelude(unicode_parameter),
)
assert "é" in str(unicode_parameter_error)


sentinel = object()


def non_json_default(value=sentinel):
    return value


json_error = expect_raises(
    omp.SchemaError,
    lambda: omp.prelude(non_json_default),
)
assert "value" in str(json_error)

def non_finite_default(value=float("nan")):
    return value


non_finite_error = expect_raises(
    omp.SchemaError,
    lambda: omp.prelude(non_finite_default),
)
assert "value" in str(non_finite_error)


duplicate_error = expect_raises(
    omp.DuplicateRegistration,
    lambda: omp.prelude("bare_helper")(candidate),
)
assert "bare_helper" in str(duplicate_error)

snapshot = registry_module.freeze_declarations()
definitions = registry_module.prelude_definitions()
assert definitions == snapshot.preludes
assert snapshot.tools == frozenset()
assert [definition.name for definition in definitions] == [
    "bare_helper",
    "configured_helper",
]

bare_definition, configured_definition = definitions
assert dataclasses.is_dataclass(bare_definition)
assert bare_definition.body is bare_helper
assert bare_definition.handler is not bare_helper
assert bare_definition.rev == 1
assert bare_definition.doc.startswith("Return the bare value.")
assert "complete docstring" in bare_definition.doc
assert bare_definition.summary == "Return the bare value."
assert bare_definition.module == "__main__"
assert [(param.name, param.kind, param.default_json, param.annotation) for param in bare_definition.params] == [
    ("value", "positional_or_keyword", None, "int"),
]
assert bare_definition.handler({"value": 7}) == {"value": 7}

assert configured_definition.body is internal_name
assert configured_definition.rev == 3
assert configured_definition.summary == "Configured helper."
assert configured_definition.doc == "This first line is overridden by the explicit summary."
assert configured_definition.module == "__main__"
assert [
    (param.name, param.kind, param.default_json, param.annotation)
    for param in configured_definition.params
] == [
    ("value", "positional_or_keyword", "3", "int"),
    ("mode", "keyword_only", "\"fast\"", "str"),
]
assert asyncio.run(
    configured_definition.handler({"value": 9, "mode": "careful"})
) == {"value": 9, "mode": "careful"}

expect_raises(
    dataclasses.FrozenInstanceError,
    lambda: setattr(configured_definition.params[0], "name", "changed"),
)
"#
				),
				None,
				None,
			)
		})
		.expect("prelude SDK contract");
}
