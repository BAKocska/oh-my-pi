//! Native Markdown command rendering with a deliberately small helper surface.

use std::fmt::Write as _;

use handlebars::{
	Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason, Renderable,
};
use omp_core::{Str, StrMut};
use serde_json::Value;

/// Parsed arguments supplied to a native Markdown command template.
#[derive(Clone, Copy, Debug)]
pub struct TemplateArguments<'a> {
	/// Original argument tail, with quoting preserved.
	pub raw:   &'a str,
	/// Tokenized arguments after quote grouping.
	pub words: &'a [Str],
}

/// Renders one native command template with the approved helper vocabulary.
pub fn render(template: &str, arguments: TemplateArguments<'_>) -> miette::Result<Str> {
	let mut registry = Handlebars::new();
	registry.set_strict_mode(true);
	registry.register_escape_fn(handlebars::no_escape);
	registry.register_helper("args", Box::new(args_helper));
	registry.register_helper("list", Box::new(list_helper));
	registry.register_helper("join", Box::new(join_helper));
	registry.register_helper("when", Box::new(when_helper));
	registry.register_helper("table", Box::new(table_helper));
	registry.register_helper("codeblock", Box::new(codeblock_helper));
	registry.register_helper("xml", Box::new(xml_helper));
	let data = serde_json::json!({
		"raw": arguments.raw,
		"arguments": arguments.words,
	});
	registry
		.render_template(template, &data)
		.map(Str::from)
		.map_err(|error| miette::miette!("command template rendering failed: {error}"))
}

fn args_helper(
	h: &Helper<'_>,
	_: &Handlebars<'_>,
	ctx: &Context,
	_: &mut RenderContext<'_, '_>,
	out: &mut dyn Output,
) -> HelperResult {
	let value = match h.param(0).and_then(|param| param.value().as_u64()) {
		Some(index) => ctx
			.data()
			.get("arguments")
			.and_then(Value::as_array)
			.and_then(|values| {
				usize::try_from(index)
					.ok()
					.and_then(|index| values.get(index))
			})
			.and_then(Value::as_str)
			.unwrap_or_default(),
		None => ctx
			.data()
			.get("raw")
			.and_then(Value::as_str)
			.unwrap_or_default(),
	};
	out.write(value)?;
	Ok(())
}

fn list_helper(
	h: &Helper<'_>,
	_: &Handlebars<'_>,
	ctx: &Context,
	_: &mut RenderContext<'_, '_>,
	out: &mut dyn Output,
) -> HelperResult {
	let values = values(h, ctx);
	for (index, value) in values.iter().enumerate() {
		if index != 0 {
			out.write("\n")?;
		}
		out.write("- ")?;
		out.write(value_text(value))?;
	}
	Ok(())
}

fn join_helper(
	h: &Helper<'_>,
	_: &Handlebars<'_>,
	ctx: &Context,
	_: &mut RenderContext<'_, '_>,
	out: &mut dyn Output,
) -> HelperResult {
	let separator = h
		.param(1)
		.and_then(|param| param.value().as_str())
		.unwrap_or(" ");
	for (index, value) in values(h, ctx).iter().enumerate() {
		if index != 0 {
			out.write(separator)?;
		}
		out.write(value_text(value))?;
	}
	Ok(())
}

fn when_helper<'reg, 'rc>(
	h: &Helper<'rc>,
	registry: &'reg Handlebars<'reg>,
	ctx: &'rc Context,
	render: &mut RenderContext<'reg, 'rc>,
	out: &mut dyn Output,
) -> HelperResult {
	let condition = h.param(0).is_some_and(|param| truthy(param.value()));
	let template = if condition { h.template() } else { h.inverse() };
	if let Some(template) = template {
		template.render(registry, ctx, render, out)?;
	}
	Ok(())
}

fn table_helper(
	h: &Helper<'_>,
	_: &Handlebars<'_>,
	ctx: &Context,
	_: &mut RenderContext<'_, '_>,
	out: &mut dyn Output,
) -> HelperResult {
	let rows = values(h, ctx);
	for (row_index, row) in rows.iter().enumerate() {
		let columns = row
			.as_array()
			.map(Vec::as_slice)
			.unwrap_or_else(|| std::slice::from_ref(row));
		out.write("|")?;
		for column in columns {
			out.write(" ")?;
			out.write(value_text(column))?;
			out.write(" |")?;
		}
		out.write("\n")?;
		if row_index == 0 {
			out.write("|")?;
			for _ in columns {
				out.write(" --- |")?;
			}
			out.write("\n")?;
		}
	}
	Ok(())
}

fn codeblock_helper(
	h: &Helper<'_>,
	_: &Handlebars<'_>,
	_: &Context,
	_: &mut RenderContext<'_, '_>,
	out: &mut dyn Output,
) -> HelperResult {
	let (language, text) = match (h.param(0), h.param(1)) {
		(Some(language), Some(text)) => {
			(language.value().as_str().unwrap_or_default(), value_text(text.value()))
		},
		(Some(text), None) => ("", value_text(text.value())),
		_ => ("", ""),
	};
	out.write("```")?;
	out.write(language)?;
	out.write("\n")?;
	out.write(text)?;
	if !text.ends_with('\n') {
		out.write("\n")?;
	}
	out.write("```")?;
	Ok(())
}

fn xml_helper(
	h: &Helper<'_>,
	_: &Handlebars<'_>,
	_: &Context,
	_: &mut RenderContext<'_, '_>,
	out: &mut dyn Output,
) -> HelperResult {
	let tag = h
		.param(0)
		.and_then(|param| param.value().as_str())
		.unwrap_or_default();
	if tag.is_empty()
		|| !tag
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
	{
		return Err(
			RenderErrorReason::Other("xml helper requires a simple tag name".to_owned()).into(),
		);
	}
	let text = h
		.param(1)
		.map(|param| value_text(param.value()))
		.unwrap_or_default();
	let mut escaped = StrMut::new("");
	for character in text.chars() {
		match character {
			'&' => escaped.push_str("&amp;"),
			'<' => escaped.push_str("&lt;"),
			'>' => escaped.push_str("&gt;"),
			'\"' => escaped.push_str("&quot;"),
			'\'' => escaped.push_str("&apos;"),
			_ => write!(escaped, "{character}").expect("writing to an in-memory string cannot fail"),
		}
	}
	out.write("<")?;
	out.write(tag)?;
	out.write(">")?;
	out.write(escaped.as_str())?;
	out.write("</")?;
	out.write(tag)?;
	out.write(">")?;
	Ok(())
}

fn values<'a>(h: &'a Helper<'_>, ctx: &'a Context) -> &'a [Value] {
	h.param(0)
		.and_then(|param| param.value().as_array())
		.or_else(|| ctx.data().get("arguments").and_then(Value::as_array))
		.map(Vec::as_slice)
		.unwrap_or_default()
}

fn value_text(value: &Value) -> &str {
	value.as_str().unwrap_or_default()
}

fn truthy(value: &Value) -> bool {
	match value {
		Value::Null => false,
		Value::Bool(value) => *value,
		Value::Number(value) => value.as_i64().is_none_or(|value| value != 0),
		Value::String(value) => !value.is_empty(),
		Value::Array(value) => !value.is_empty(),
		Value::Object(value) => !value.is_empty(),
	}
}
#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn renders_only_the_native_helper_vocabulary() {
		let words = [sf!("one"), sf!("two")];
		let rendered = render(
			"{{args}}|{{join arguments \",\"}}|{{#when arguments}}yes{{else}}no{{/when}}|{{codeblock \
			 \"rs\" \"fn main() {}\"}}|{{xml \"note\" \"<ok>\"}}",
			TemplateArguments { raw: "\"one\" two", words: &words },
		)
		.expect("render");
		assert_eq!(
			rendered,
			"\"one\" two|one,two|yes|```rs\nfn main() {}\n```|<note>&lt;ok&gt;</note>",
		);
	}
}
