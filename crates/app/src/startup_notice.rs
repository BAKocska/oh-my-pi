//! Data-driven interactive startup notices, persisted once per application
//! version.

use std::{
	fs,
	io::{self, IsTerminal as _},
	path::Path,
};

use omp_core::Str;

const NOTICE_VERSION_FILE: &str = "startup-notice-version";

/// A startup notice selected by runtime state rather than hard-coded dispatch
/// branches.
pub struct StartupNotice {
	/// Stable notice identifier.
	pub id:   &'static str,
	/// Human-facing body.
	pub body: &'static str,
}

const NOTICES: &[StartupNotice] =
	&[StartupNotice { id: "splash", body: "OMP coding agent" }, StartupNotice {
		id:   "changelog",
		body: "See release notes with `omp --help`.",
	}];

/// Emits splash/changelog/model-scope notices once for this binary version.
pub fn show_once(data_dir: &Path, model: Option<&Str>) -> io::Result<()> {
	if !std::io::stderr().is_terminal() || seen(data_dir)? {
		return Ok(());
	}
	for notice in NOTICES {
		eprintln!("{}", notice.body);
	}
	if let Some(model) = model {
		eprintln!("Model scope: {model}");
	}
	fs::create_dir_all(data_dir)?;
	fs::write(data_dir.join(NOTICE_VERSION_FILE), env!("CARGO_PKG_VERSION"))
}

fn seen(data_dir: &Path) -> io::Result<bool> {
	match fs::read_to_string(data_dir.join(NOTICE_VERSION_FILE)) {
		Ok(version) => Ok(version == env!("CARGO_PKG_VERSION")),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
		Err(error) => Err(error),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn version_marker_is_recognized() {
		let state = tempfile::tempdir().expect("state");
		fs::create_dir_all(state.path()).expect("create");
		fs::write(state.path().join(NOTICE_VERSION_FILE), env!("CARGO_PKG_VERSION")).expect("marker");
		assert!(seen(state.path()).expect("read"));
	}
}
