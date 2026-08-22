//! Extensions to clap's built-in value parsers.

use std::{ffi::OsStr, fmt::Write as _};

use clap::{
	Arg, Command,
	builder::{PossibleValue, TypedValueParser},
	error::{ContextKind, ContextValue, ErrorKind},
};
use smallvec::SmallVec;

/// A value parser accepting every unambiguous prefix of its possible values and
/// aliases.
#[derive(Clone, Debug)]
pub(crate) struct ShortcutValueParser(SmallVec<PossibleValue, 8>);

impl ShortcutValueParser {
	/// Creates a parser from names or [`PossibleValue`] definitions.
	pub(crate) fn new(values: impl Into<Self>) -> Self {
		values.into()
	}

	fn error(
		&self,
		cmd: &Command,
		arg: Option<&Arg>,
		value: &str,
		matches: &[&PossibleValue],
	) -> clap::Error {
		let mut error = clap::Error::new(ErrorKind::InvalidValue).with_cmd(cmd);
		if let Some(arg) = arg {
			error.insert(ContextKind::InvalidArg, ContextValue::String(arg.to_string()));
		}
		error.insert(ContextKind::InvalidValue, ContextValue::String(value.to_owned()));
		error.insert(
			ContextKind::ValidValue,
			ContextValue::Strings(
				self
					.0
					.iter()
					.map(|possible| possible.get_name().to_owned())
					.collect(),
			),
		);

		if !matches.is_empty() {
			let mut choices = String::new();
			for (index, possible) in matches.iter().enumerate() {
				if index > 0 {
					choices.push_str(if index + 1 == matches.len() {
						" or "
					} else {
						", "
					});
				}
				write!(choices, "'{}'", possible.get_name()).expect("writing to a String cannot fail");
			}
			error.insert(
				ContextKind::Suggested,
				ContextValue::StyledStrs(vec![
					format!(
						"It looks like '{value}' could match several values. Did you mean {choices}?"
					)
					.into(),
				]),
			);
		}
		error
	}
}

impl TypedValueParser for ShortcutValueParser {
	type Value = String;

	fn parse_ref(
		&self,
		cmd: &Command,
		arg: Option<&Arg>,
		value: &OsStr,
	) -> Result<Self::Value, clap::Error> {
		let value = value
			.to_str()
			.ok_or_else(|| clap::Error::new(ErrorKind::InvalidUtf8))?;
		let matches = self
			.0
			.iter()
			.filter(|possible| {
				possible
					.get_name_and_aliases()
					.any(|candidate| candidate.starts_with(value))
			})
			.collect::<SmallVec<_, 8>>();

		match matches.as_slice() {
			[] => Err(self.error(cmd, arg, value, &[])),
			[matched] => Ok(matched.get_name().to_owned()),
			many => many
				.iter()
				.find(|possible| possible.get_name() == value)
				.map(|possible| possible.get_name().to_owned())
				.ok_or_else(|| self.error(cmd, arg, value, many)),
		}
	}

	fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
		Some(Box::new(self.0.iter().cloned()))
	}
}

impl<I, T> From<I> for ShortcutValueParser
where
	I: IntoIterator<Item = T>,
	T: Into<PossibleValue>,
{
	fn from(values: I) -> Self {
		Self(values.into_iter().map(Into::into).collect())
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;

	use clap::{
		Command,
		builder::{PossibleValue, TypedValueParser},
		error::ErrorKind,
	};

	use super::ShortcutValueParser;

	#[test]
	fn accepts_unique_prefixes_and_exact_matches() {
		let command = Command::new("cmd");
		let parser = ShortcutValueParser::new(["abcd", "abef"]);
		assert_eq!(parser.parse_ref(&command, None, OsStr::new("abc")).unwrap(), "abcd");
		assert_eq!(parser.parse_ref(&command, None, OsStr::new("abe")).unwrap(), "abef");

		let parser = ShortcutValueParser::new(["abcd", "abcdefgh"]);
		assert_eq!(
			parser
				.parse_ref(&command, None, OsStr::new("abcd"))
				.unwrap(),
			"abcd"
		);
	}

	#[test]
	fn rejects_ambiguous_prefixes_with_a_tip() {
		let command = Command::new("cmd");
		let parser = ShortcutValueParser::new(["abcd", "abef"]);
		let error = parser
			.parse_ref(&command, None, OsStr::new("ab"))
			.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidValue);
		assert!(error.to_string().contains("Did you mean 'abcd' or 'abef'?"));
	}

	#[test]
	fn aliases_of_one_value_are_not_ambiguous() {
		let command = Command::new("cmd");
		let parser = ShortcutValueParser::new([
			PossibleValue::new("atime").alias("access"),
			PossibleValue::new("status"),
		]);
		assert_eq!(parser.parse_ref(&command, None, OsStr::new("a")).unwrap(), "atime");
	}
}
