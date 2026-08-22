//! Typed device-tree paths shared by dispatch, journals, and provenance.

use std::{fmt, str::FromStr};

use omp_core::Str;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A parsed device-tree address.
///
/// The canonical spelling is `name[/sub][@publisher/extension]`. A claimant
/// qualifies an implementation, never a schema revision.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "Str", into = "Str")]
pub struct DevicePath {
	/// Root device token.
	pub name:     Str,
	/// Optional one-level sub-tool token.
	pub sub:      Option<Str>,
	/// Optional `publisher/extension` claimant qualifier.
	pub claimant: Option<Str>,
}

impl DevicePath {
	/// Parses one canonical device-tree address.
	pub fn parse(value: &str) -> Result<Self, DevicePathError> {
		value.parse()
	}

	/// Returns the unqualified root device token.
	pub const fn root(&self) -> &Str {
		&self.name
	}
}

/// A malformed device-tree address.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("invalid device path: {value}")]
pub struct DevicePathError {
	/// Rejected path spelling.
	pub value: Str,
}

impl FromStr for DevicePath {
	type Err = DevicePathError;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let invalid = || DevicePathError { value: Str::new(value) };
		let (path, claimant) = match value.split_once('@') {
			Some((path, claimant)) => {
				if path.is_empty()
					|| claimant.is_empty()
					|| claimant.contains('@')
					|| !valid_claimant(claimant)
				{
					return Err(invalid());
				}
				(path, Some(Str::new(claimant)))
			},
			None => (value, None),
		};
		let mut segments = path.split('/');
		let Some(name) = segments.next() else {
			return Err(invalid());
		};
		let sub = segments.next();
		if segments.next().is_some() || !valid_device_name(name) || !sub.is_none_or(valid_device_name)
		{
			return Err(invalid());
		}
		Ok(Self { name: Str::new(name), sub: sub.map(Str::new), claimant })
	}
}

impl fmt::Display for DevicePath {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.name.as_str())?;
		if let Some(sub) = &self.sub {
			write!(formatter, "/{sub}")?;
		}
		if let Some(claimant) = &self.claimant {
			write!(formatter, "@{claimant}")?;
		}
		Ok(())
	}
}

impl From<DevicePath> for Str {
	fn from(value: DevicePath) -> Self {
		Self::new(value.to_string())
	}
}

impl TryFrom<Str> for DevicePath {
	type Error = DevicePathError;

	fn try_from(value: Str) -> Result<Self, Self::Error> {
		value.as_str().parse()
	}
}

fn valid_device_name(value: &str) -> bool {
	let mut bytes = value.bytes();
	matches!(bytes.next(), Some(byte) if byte.is_ascii_lowercase())
		&& value.len() <= 64
		&& bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_claimant(value: &str) -> bool {
	let Some((publisher, extension)) = value.split_once('/') else {
		return false;
	};
	!publisher.is_empty()
		&& !extension.is_empty()
		&& !extension.contains('/')
		&& publisher.bytes().all(valid_claimant_byte)
		&& extension.bytes().all(valid_claimant_byte)
}

const fn valid_claimant_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

#[cfg(test)]
mod tests {
	use super::DevicePath;

	#[test]
	fn parses_and_renders_canonical_paths() {
		for input in ["lint", "jira/create", "grep@ff-labs/fff", "jira/create@ff-labs/fff"] {
			let path: DevicePath = input.parse().expect("path is valid");
			assert_eq!(path.to_string(), input);
		}
	}

	#[test]
	fn rejects_path_and_claimant_ambiguity() {
		for input in [
			"",
			"Lint",
			"lint/",
			"lint/a/b",
			"lint@publisher",
			"lint@publisher/extension/extra",
			"lint@publisher/extension@other/extension",
			"lint@publisher//extension",
			"lint@publisher/extension/",
			"lint@publisher/extension?x",
		] {
			assert!(input.parse::<DevicePath>().is_err(), "{input} should be invalid");
		}
	}
}
