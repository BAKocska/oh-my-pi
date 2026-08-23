//! Builtin filters, functions, and block helpers.

use std::iter;

use omp_core::{Str, sf};

use crate::{
	Engine, Value,
	error::{Error, HelperError},
};

/// Registers every builtin on a fresh engine.
pub fn install(engine: &mut Engine) {
	engine.add_filter("join", |args| {
		let items = list_input(args, "join")?;
		let separator = match args.get(1) {
			None => ", ",
			Some(Value::Str(separator)) => separator.as_str(),
			Some(_) => return Err(Error::helper("join", HelperError::ExpectedString)),
		};
		let mut out = String::new();
		for (index, item) in items.iter().enumerate() {
			if index > 0 {
				out.push_str(separator);
			}
			item.write_display(&mut out);
		}
		Ok(Value::Str(Str::from(out)))
	});

	engine.add_filter("length", |args| match &args[0] {
		Value::Str(text) => Ok(Value::Int(text.as_str().chars().count() as i64)),
		Value::List(items) => Ok(Value::Int(items.len() as i64)),
		Value::Map(entries) => Ok(Value::Int(entries.len() as i64)),
		_ => Err(Error::helper("length", HelperError::NoLength)),
	});

	engine.add_filter("default", |args| {
		let fallback = args.get(1).ok_or_else(|| {
			Error::helper("default", HelperError::Arity { expected: 1, got: 0 })
		})?;
		Ok(match &args[0] {
			Value::None => fallback.clone(),
			value => value.clone(),
		})
	});

	engine.add_filter("pluralize", |args| {
		let count = match &args[0] {
			Value::Int(count) => *count,
			Value::List(items) => items.len() as i64,
			_ => return Err(Error::helper("pluralize", HelperError::ExpectedCount)),
		};
		let singular = str_arg(args, 1, "pluralize")?;
		let word = if count == 1 {
			Str::new(singular)
		} else {
			match args.get(2) {
				None => sf!("{singular}s"),
				Some(Value::Str(plural)) => plural.clone(),
				Some(_) => return Err(Error::helper("pluralize", HelperError::ExpectedString)),
			}
		};
		Ok(Value::Str(sf!("{count} {word}")))
	});

	engine.add_filter("json", |args| {
		serde_json::to_string(&args[0])
			.map(|json| Value::Str(Str::from(json)))
			.map_err(|error| Error::helper("json", error))
	});

	engine.add_filter("escape_xml", |args| {
		let text = args[0].display();
		let mut out = String::with_capacity(text.len());
		for character in text.as_str().chars() {
			match character {
				'&' => out.push_str("&amp;"),
				'<' => out.push_str("&lt;"),
				'>' => out.push_str("&gt;"),
				'"' => out.push_str("&quot;"),
				'\'' => out.push_str("&apos;"),
				other => out.push(other),
			}
		}
		Ok(Value::Str(Str::from(out)))
	});

	engine.add_filter("trim", |args| {
		Ok(Value::Str(match &args[0] {
			// Zero-copy for the common string case.
			Value::Str(text) => text.trim(),
			other => Str::new(other.display().trim()),
		}))
	});

	engine.add_filter("indent", |args| {
		let Some(Value::Int(width)) = args.get(1) else {
			return Err(Error::helper("indent", HelperError::ExpectedInt));
		};
		let width = usize::try_from(*width).unwrap_or(0);
		let indent_first = match args.get(2) {
			None => true,
			Some(Value::Bool(first)) => *first,
			Some(_) => return Err(Error::helper("indent", HelperError::ExpectedBool)),
		};
		let text = args[0].display();
		let mut out = String::with_capacity(text.len() + width * 4);
		for (index, line) in text.as_str().split('\n').enumerate() {
			if index > 0 {
				out.push('\n');
			}
			if !line.is_empty() && (indent_first || index > 0) {
				for _ in 0..width {
					out.push(' ');
				}
			}
			if !line.is_empty() {
				out.push_str(line);
			}
		}
		Ok(Value::Str(Str::from(out)))
	});

	engine.add_filter("bullets", |args| {
		let items = list_input(args, "bullets")?;
		let marker = match args.get(1) {
			None => "- ",
			Some(Value::Str(marker)) => marker.as_str(),
			Some(_) => return Err(Error::helper("bullets", HelperError::ExpectedString)),
		};
		let mut out = String::new();
		for (index, item) in items.iter().enumerate() {
			if index > 0 {
				out.push('\n');
			}
			out.push_str(marker);
			item.write_display(&mut out);
		}
		Ok(Value::Str(Str::from(out)))
	});

	engine.add_function("table", |args| {
		let Some(Value::List(rows)) = args.first() else {
			return Err(Error::helper("table", HelperError::ExpectedList));
		};
		let headers = match args.get(1) {
			None => None,
			Some(Value::List(headers)) => Some(headers),
			Some(_) => return Err(Error::helper("table", HelperError::ExpectedList)),
		};
		let mut out = String::new();
		let mut separated = if let Some(headers) = headers {
			let columns = write_table_row(&mut out, headers.iter());
			write_table_separator(&mut out, columns);
			true
		} else {
			false
		};
		for row in rows {
			let columns = match row {
				Value::List(cells) => write_table_row(&mut out, cells.iter()),
				cell => write_table_row(&mut out, iter::once(cell)),
			};
			if !separated {
				write_table_separator(&mut out, columns);
				separated = true;
			}
		}
		Ok(Value::Str(Str::from(out)))
	});

	engine.add_block("xml", |args, body, out| {
		let tag = str_arg(args, 0, "xml")?;
		if tag.is_empty()
			|| !tag
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
		{
			return Err(Error::helper("xml", HelperError::InvalidTagName));
		}
		let body = body.trim();
		if body.is_empty() {
			return Ok(());
		}
		out.push('<');
		out.push_str(tag);
		out.push_str(">\n");
		out.push_str(body);
		out.push_str("\n</");
		out.push_str(tag);
		out.push('>');
		Ok(())
	});

	engine.add_block("codeblock", |args, body, out| {
		let language = match args.first() {
			None => "",
			Some(Value::Str(language)) => language.as_str(),
			Some(_) => return Err(Error::helper("codeblock", HelperError::ExpectedString)),
		};
		out.push_str("```");
		out.push_str(language);
		out.push('\n');
		out.push_str(body.trim());
		out.push_str("\n```");
		Ok(())
	});
}

/// `| cell | cell |\n`; returns the column count for the separator row.
fn write_table_row<'v>(out: &mut String, cells: impl Iterator<Item = &'v Value>) -> usize {
	let mut columns = 0;
	out.push('|');
	for cell in cells {
		out.push(' ');
		cell.write_display(out);
		out.push_str(" |");
		columns += 1;
	}
	out.push('\n');
	columns
}

fn write_table_separator(out: &mut String, columns: usize) {
	out.push('|');
	for _ in 0..columns {
		out.push_str(" --- |");
	}
	out.push('\n');
}

/// The piped input of a list-shaped filter.
fn list_input<'a>(args: &'a [Value], name: &'static str) -> Result<&'a im::Vector<Value>, Error> {
	match &args[0] {
		Value::List(items) => Ok(items),
		_ => Err(Error::helper(name, HelperError::ExpectedList)),
	}
}

/// A required string argument at `index`.
fn str_arg<'a>(args: &'a [Value], index: usize, name: &'static str) -> Result<&'a str, Error> {
	match args.get(index) {
		Some(Value::Str(text)) => Ok(text.as_str()),
		Some(_) => Err(Error::helper(name, HelperError::ExpectedString)),
		None => Err(Error::helper(name, HelperError::Arity {
			expected: index,
			got:      index.saturating_sub(1),
		})),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Props, list};

	fn render(source: &'static str, props: &Props) -> Str {
		let engine = Engine::new();
		engine
			.compile("test", source)
			.expect("compile")
			.render_str(&engine, props)
			.expect("render")
	}

	fn items() -> Props {
		let mut props = Props::new();
		props.set("items", list!["one", "two", "three"]);
		props
	}

	#[test]
	fn join_defaults_to_comma_space() {
		assert_eq!(render("{{ items | join }}", &items()), "one, two, three");
		assert_eq!(render("{{ items | join(\"|\") }}", &items()), "one|two|three");
	}

	#[test]
	fn length_counts_chars_items_and_entries() {
		let mut props = items();
		props.set("text", "héllo");
		assert_eq!(render("{{ items | length }} {{ text | length }}", &props), "3 5");
	}

	#[test]
	fn pluralize_pairs_count_with_word() {
		let mut props = items();
		props.set("one", 1);
		props.set("many", 3);
		assert_eq!(
			render("{{ one | pluralize(\"item\") }}, {{ many | pluralize(\"item\") }}", &props),
			"1 item, 3 items"
		);
		assert_eq!(render("{{ items | pluralize(\"entry\", \"entries\") }}", &props), "3 entries");
	}

	#[test]
	fn json_escape_trim_indent_bullets_shape_text() {
		let mut props = items();
		props.set("padded", "  <a & \"b\">  ");
		props.set("multi", "x\n\ny");
		assert_eq!(render("{{ items | json }}", &props), "[\"one\",\"two\",\"three\"]");
		assert_eq!(
			render("{{ padded | trim | escape_xml }}", &props),
			"&lt;a &amp; &quot;b&quot;&gt;"
		);
		assert_eq!(render("{{ multi | indent(2, false) }}", &props), "x\n\n  y");
		assert_eq!(render("{{ items | bullets(\"* \") }}", &props), "* one\n* two\n* three");
	}

	#[test]
	fn table_uses_first_row_or_explicit_headers() {
		let mut props = Props::new();
		props.set("rows", list![list!["a", "b"], list!["1", "2"]]);
		props.set("heads", list!["x", "y"]);
		assert_eq!(render("{{ table(rows) }}", &props), "| a | b |\n| --- | --- |\n| 1 | 2 |\n");
		assert_eq!(
			render("{{ table(rows, heads) }}", &props),
			"| x | y |\n| --- | --- |\n| a | b |\n| 1 | 2 |\n"
		);
		// Scalar rows become single-cell rows.
		props.set("scalars", list!["only"]);
		assert_eq!(render("{{ table(scalars) }}", &props), "| only |\n| --- |\n");
	}

	#[test]
	fn xml_block_validates_tags_and_elides_empty_bodies() {
		let props = Props::new();
		assert_eq!(render("{% xml \"a-b_1\" %}x{% endxml %}", &props), "<a-b_1>\nx\n</a-b_1>");
		let engine = Engine::new();
		let template = engine
			.compile("test", "{% xml \"bad tag\" %}x{% endxml %}")
			.expect("compile");
		let error = template.render_str(&engine, &props).unwrap_err();
		assert!(matches!(error, Error::Helper { ref name, .. } if name == "xml"), "{error}");
	}

	#[test]
	fn codeblock_fences_trimmed_bodies() {
		let props = Props::new();
		assert_eq!(
			render("{% codeblock \"sh\" %}  ls -la  {% endcodeblock %}", &props),
			"```sh\nls -la\n```"
		);
		assert_eq!(render("{% codeblock %}x{% endcodeblock %}", &props), "```\nx\n```");
	}
}
