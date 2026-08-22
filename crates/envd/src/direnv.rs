//! Bounded, allow-list-respecting direnv environment preflight.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	process::Stdio,
	time::Duration,
};

use omp_core::Str;

/// Environment changes emitted by `direnv export json`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DirenvDelta {
	pub(crate) set:   BTreeMap<Str, Str>,
	pub(crate) unset: Vec<Str>,
}

/// Finds the nearest regular `.envrc` while walking toward the filesystem root.
pub(crate) async fn find_envrc(start: &Path) -> Option<PathBuf> {
	let mut directory = start.to_path_buf();
	loop {
		let candidate = directory.join(".envrc");
		if tokio::fs::metadata(&candidate)
			.await
			.is_ok_and(|metadata| metadata.is_file())
		{
			return Some(candidate);
		}
		if !directory.pop() {
			return None;
		}
	}
}

/// Runs `direnv export json` in the nearest `.envrc` directory.
///
/// A blocked or failing environment is ignored: direnv's own allow list is the
/// authority and this function never invokes `direnv allow`.
pub(crate) async fn load(cwd: &Path, limit: Duration) -> Option<DirenvDelta> {
	let envrc = find_envrc(cwd).await?;
	let directory = envrc.parent()?;
	let mut command = tokio::process::Command::new("direnv");
	command
		.args(["export", "json"])
		.current_dir(directory)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);
	for (key, _) in
		std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("DIRENV_"))
	{
		command.env_remove(key);
	}
	let output = tokio::time::timeout(limit, command.output())
		.await
		.ok()?
		.ok()?;
	if !output.status.success() {
		return None;
	}
	parse(&output.stdout)
}

fn parse(bytes: &[u8]) -> Option<DirenvDelta> {
	if bytes.iter().all(u8::is_ascii_whitespace) {
		return Some(DirenvDelta::default());
	}
	let values = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(bytes).ok()?;
	let mut delta = DirenvDelta::default();
	for (key, value) in values {
		match value {
			serde_json::Value::String(value) => {
				delta.set.insert(Str::from(key), Str::from(value));
			},
			serde_json::Value::Null => delta.unset.push(Str::from(key)),
			_ => {},
		}
	}
	Some(delta)
}

#[cfg(test)]
mod tests {
	use super::parse;

	#[test]
	fn parses_set_and_unset_halves() {
		let delta = parse(br#"{"A":"one","B":null,"ignored":2}"#).unwrap();
		assert_eq!(delta.set.get("A").map(|value| value.as_str()), Some("one"));
		assert_eq!(delta.unset, ["B"]);
	}

	#[test]
	fn malformed_output_is_ignored() {
		assert!(parse(b"not json").is_none());
	}
}
