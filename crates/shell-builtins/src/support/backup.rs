//! GNU backup and update option handling shared by `ln` and `mv`.

use std::{
	ffi::{OsStr, OsString},
	path::{Path, PathBuf},
};

use clap::ArgMatches;
use strum::EnumString;
use thiserror::Error;

/// Accepted values for `--backup` and `VERSION_CONTROL`.
pub(crate) static BACKUP_CONTROL_VALUES: &[&str] =
	&["simple", "never", "numbered", "t", "existing", "nil", "none", "off"];

/// Extended GNU help for backup selection.
pub(crate) const BACKUP_CONTROL_LONG_HELP: &str = "The backup suffix is '~', unless set with \
                                                   --suffix or SIMPLE_BACKUP_SUFFIX.
The version control method may be selected via the --backup option or through
the VERSION_CONTROL environment variable.  Here are the values:

  none, off       never make backups (even if --backup is given)
  numbered, t     make numbered backups
  existing, nil   numbered if numbered backups exist, simple otherwise
  simple, never   always make simple backups";

/// Default suffix for simple backups.
pub(crate) const DEFAULT_BACKUP_SUFFIX: &str = "~";

/// Backup naming strategy.
#[derive(Clone, Copy, Debug, Default, EnumString, Eq, PartialEq)]
pub(crate) enum BackupMode {
	/// Do not create a backup.
	#[default]
	#[strum(serialize = "none", serialize = "off")]
	None,
	/// Append the configured simple suffix.
	#[strum(serialize = "simple", serialize = "never")]
	Simple,
	/// Append an incrementing `.~N~` suffix.
	#[strum(serialize = "numbered", serialize = "t")]
	Numbered,
	/// Use numbered backups only when numbered backups already exist.
	#[strum(serialize = "existing", serialize = "nil")]
	Existing,
}

/// Failure to determine an unambiguous backup strategy.
#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum BackupError {
	/// The supplied control value does not name a strategy.
	#[error(
		"invalid argument '{method}' for '{origin}'\nValid arguments are:\n  - 'none', 'off'\n  - \
		 'simple', 'never'\n  - 'existing', 'nil'\n  - 'numbered', 't'"
	)]
	InvalidArgument {
		/// Invalid value.
		method: String,
		/// CLI or environment source.
		origin: &'static str,
	},
	/// The supplied abbreviation selects multiple accepted values.
	#[error(
		"ambiguous argument '{method}' for '{origin}'\nValid arguments are:\n  - 'none', 'off'\n  - \
		 'simple', 'never'\n  - 'existing', 'nil'\n  - 'numbered', 't'"
	)]
	AmbiguousArgument {
		/// Ambiguous value.
		method: String,
		/// CLI or environment source.
		origin: &'static str,
	},
}

/// Reusable clap arguments for backup controls.
pub(crate) mod arguments {
	use clap::ArgAction;

	/// Clap identifier for `--backup`.
	pub(crate) static OPT_BACKUP: &str = "backupopt_backup";
	/// Clap identifier for `-b`.
	pub(crate) static OPT_BACKUP_NO_ARG: &str = "backupopt_b";
	/// Clap identifier for `--suffix`.
	pub(crate) static OPT_SUFFIX: &str = "backupopt_suffix";

	/// Builds the optional-valued `--backup[=CONTROL]` argument.
	pub(crate) fn backup() -> clap::Arg {
		clap::Arg::new(OPT_BACKUP)
			.long("backup")
			.help("make a backup of each existing destination file")
			.action(ArgAction::Set)
			.require_equals(true)
			.num_args(0..=1)
			.value_name("CONTROL")
	}

	/// Builds the argument-less `-b` backup switch.
	pub(crate) fn backup_no_args() -> clap::Arg {
		clap::Arg::new(OPT_BACKUP_NO_ARG)
			.short('b')
			.help("like --backup but does not accept an argument")
			.action(ArgAction::SetTrue)
	}

	/// Builds the `-S, --suffix=SUFFIX` argument.
	pub(crate) fn suffix() -> clap::Arg {
		clap::Arg::new(OPT_SUFFIX)
			.short('S')
			.long("suffix")
			.help("override the usual backup suffix")
			.action(ArgAction::Set)
			.value_name("SUFFIX")
			.allow_hyphen_values(true)
	}
}

/// Determines the simple backup suffix from CLI, environment, and default
/// precedence.
pub(crate) fn determine_backup_suffix(matches: &ArgMatches) -> String {
	let suffix = matches
		.get_one::<String>(arguments::OPT_SUFFIX)
		.cloned()
		.or_else(|| std::env::var("SIMPLE_BACKUP_SUFFIX").ok())
		.unwrap_or_else(|| DEFAULT_BACKUP_SUFFIX.to_owned());
	if suffix.contains('/') {
		DEFAULT_BACKUP_SUFFIX.to_owned()
	} else {
		suffix
	}
}

/// Determines the backup strategy from clap matches and `VERSION_CONTROL`.
pub(crate) fn determine_backup_mode(matches: &ArgMatches) -> Result<BackupMode, BackupError> {
	if matches.contains_id(arguments::OPT_BACKUP) {
		if let Some(method) = matches.get_one::<String>(arguments::OPT_BACKUP) {
			match_method(method, "backup type")
		} else if let Ok(method) = std::env::var("VERSION_CONTROL") {
			match_method(&method, "$VERSION_CONTROL")
		} else {
			Ok(BackupMode::Existing)
		}
	} else if matches.get_flag(arguments::OPT_BACKUP_NO_ARG)
		|| matches.contains_id(arguments::OPT_SUFFIX)
	{
		if let Ok(method) = std::env::var("VERSION_CONTROL") {
			match_method(&method, "$VERSION_CONTROL")
		} else {
			Ok(BackupMode::Existing)
		}
	} else {
		Ok(BackupMode::None)
	}
}

fn match_method(method: &str, origin: &'static str) -> Result<BackupMode, BackupError> {
	let mut matches = BACKUP_CONTROL_VALUES
		.iter()
		.copied()
		.filter(|candidate| candidate.starts_with(method));
	let Some(first) = matches.next() else {
		return Err(BackupError::InvalidArgument { method: method.to_owned(), origin });
	};
	if matches.next().is_some() {
		return Err(BackupError::AmbiguousArgument { method: method.to_owned(), origin });
	}
	first
		.parse()
		.map_err(|_| BackupError::InvalidArgument { method: method.to_owned(), origin })
}

/// Computes the next backup path for `path` and the selected strategy.
pub(crate) fn get_backup_path(
	backup_mode: BackupMode,
	path: &Path,
	suffix: impl AsRef<OsStr>,
) -> Option<PathBuf> {
	match backup_mode {
		BackupMode::None => None,
		BackupMode::Simple => Some(simple_backup_path(path, suffix.as_ref())),
		BackupMode::Numbered => Some(numbered_backup_path(path)),
		BackupMode::Existing => {
			let numbered = simple_backup_path(path, OsStr::new(".~1~"));
			Some(if numbered.exists() {
				numbered_backup_path(path)
			} else {
				simple_backup_path(path, suffix.as_ref())
			})
		},
	}
}

fn simple_backup_path(path: &Path, suffix: &OsStr) -> PathBuf {
	let mut file_name = path.file_name().unwrap_or_default().to_owned();
	file_name.push(suffix);
	path.with_file_name(file_name)
}

fn numbered_backup_path(path: &Path) -> PathBuf {
	for index in 1_u64.. {
		let suffix = OsString::from(format!(".~{index}~"));
		let candidate = simple_backup_path(path, &suffix);
		if !candidate.exists() {
			return candidate;
		}
	}
	unreachable!("the backup sequence is unbounded")
}

/// Returns whether `source` is the simple-suffix backup path for `target`.
pub(crate) fn source_is_target_backup(source: &Path, target: &Path, suffix: &str) -> bool {
	let mut expected = target.as_os_str().to_owned();
	expected.push(suffix);
	source.as_os_str() == expected
}

/// GNU update strategy for replacing destination files.
#[derive(Clone, Debug, Default, EnumString, Eq, PartialEq)]
pub(crate) enum UpdateMode {
	/// Replace every destination.
	#[default]
	#[strum(serialize = "all")]
	All,
	/// Do not replace existing destinations.
	#[strum(serialize = "none")]
	None,
	/// Replace only when the source is newer.
	#[strum(serialize = "older")]
	IfOlder,
	/// Fail instead of replacing an existing destination.
	#[strum(serialize = "none-fail")]
	NoneFail,
}

/// Reusable clap arguments for update controls.
pub(crate) mod update_arguments {
	use clap::{ArgAction, builder::TypedValueParser as _};

	/// Clap identifier for `--update`.
	pub(crate) static OPT_UPDATE: &str = "update";
	/// Clap identifier for `-u`.
	pub(crate) static OPT_UPDATE_NO_ARG: &str = "u";

	/// Builds `--update[=CONTROL]`, accepting unambiguous GNU abbreviations.
	pub(crate) fn update() -> clap::Arg {
		let parser = clap::builder::StringValueParser::new().try_map(|value: String| {
			let mut matches = ["none", "all", "older", "none-fail"]
				.into_iter()
				.filter(|candidate| candidate.starts_with(&value));
			let first = matches
				.next()
				.ok_or_else(|| format!("invalid update mode '{value}'"))?;
			if matches.next().is_some() {
				return Err(format!("ambiguous update mode '{value}'"));
			}
			Ok(first.to_owned())
		});
		clap::Arg::new(OPT_UPDATE)
			.long("update")
			.help(
				"move only when the SOURCE file is newer than the destination file or when the \
				 destination file is missing",
			)
			.value_parser(parser)
			.num_args(0..=1)
			.default_missing_value("older")
			.require_equals(true)
			.overrides_with(OPT_UPDATE)
			.action(ArgAction::Set)
	}

	/// Builds the argument-less `-u` update switch.
	pub(crate) fn update_no_args() -> clap::Arg {
		clap::Arg::new(OPT_UPDATE_NO_ARG)
			.short('u')
			.help("like --update but does not accept an argument")
			.action(ArgAction::SetTrue)
	}
}

/// Update-mode (`--update`/`-u`) argument handling shared by `mv` and `ln`.
pub(crate) mod update_control {
	pub(crate) use super::{UpdateMode, determine_update_mode};
	/// Reusable update-related clap arguments.
	pub(crate) mod arguments {
		pub(crate) use super::super::update_arguments::*;
	}
}

/// Determines the update strategy from clap matches.
pub(crate) fn determine_update_mode(matches: &ArgMatches) -> UpdateMode {
	if let Some(mode) = matches.get_one::<String>(update_arguments::OPT_UPDATE) {
		mode
			.parse()
			.unwrap_or_else(|_| unreachable!("clap restricted update mode"))
	} else if matches.get_flag(update_arguments::OPT_UPDATE_NO_ARG) {
		UpdateMode::IfOlder
	} else {
		UpdateMode::All
	}
}

#[cfg(test)]
mod tests {
	use clap::Command;
	use parking_lot::Mutex;

	use super::*;

	static ENV_LOCK: Mutex<()> = Mutex::new(());

	fn command() -> Command {
		Command::new("test")
			.arg(arguments::backup())
			.arg(arguments::backup_no_args())
			.arg(arguments::suffix())
	}

	#[test]
	fn backup_determination_matrix() {
		let _guard = ENV_LOCK.lock();
		// SAFETY: this test serializes every environment mutation in this module.
		unsafe { std::env::remove_var("VERSION_CONTROL") };
		for (args, expected) in [
			(&["test"][..], BackupMode::None),
			(&["test", "--backup"][..], BackupMode::Existing),
			(&["test", "--backup=simple"][..], BackupMode::Simple),
			(&["test", "--backup=t"][..], BackupMode::Numbered),
			(&["test", "-b"][..], BackupMode::Existing),
			(&["test", "-S.bak"][..], BackupMode::Existing),
		] {
			let matches = command().try_get_matches_from(args).unwrap();
			assert_eq!(determine_backup_mode(&matches).unwrap(), expected, "{args:?}");
		}
		// SAFETY: guarded as above.
		unsafe { std::env::set_var("VERSION_CONTROL", "numbered") };
		let matches = command().try_get_matches_from(["test", "-b"]).unwrap();
		assert_eq!(determine_backup_mode(&matches).unwrap(), BackupMode::Numbered);
		// SAFETY: guarded as above.
		unsafe { std::env::remove_var("VERSION_CONTROL") };
	}

	#[test]
	fn suffix_precedence_and_validation() {
		let _guard = ENV_LOCK.lock();
		// SAFETY: this test serializes every environment mutation in this module.
		unsafe { std::env::set_var("SIMPLE_BACKUP_SUFFIX", ".env") };
		let matches = command().try_get_matches_from(["test"]).unwrap();
		assert_eq!(determine_backup_suffix(&matches), ".env");
		let matches = command()
			.try_get_matches_from(["test", "--suffix=.cli"])
			.unwrap();
		assert_eq!(determine_backup_suffix(&matches), ".cli");
		let matches = command()
			.try_get_matches_from(["test", "--suffix=bad/name"])
			.unwrap();
		assert_eq!(determine_backup_suffix(&matches), "~");
		// SAFETY: guarded as above.
		unsafe { std::env::remove_var("SIMPLE_BACKUP_SUFFIX") };
	}

	#[test]
	fn chooses_simple_existing_and_next_numbered_paths() {
		let directory = tempfile::tempdir().unwrap();
		let path = directory.path().join("file");
		assert_eq!(
			get_backup_path(BackupMode::Existing, &path, "~").unwrap(),
			directory.path().join("file~")
		);
		std::fs::write(directory.path().join("file.~1~"), b"").unwrap();
		assert_eq!(
			get_backup_path(BackupMode::Existing, &path, "~").unwrap(),
			directory.path().join("file.~2~")
		);
	}
}
