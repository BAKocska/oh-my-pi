//! Private user-shell alias/function/PATH snapshots for Brush sessions.

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::{
	env,
	fs::{self, OpenOptions},
	io,
	path::{Path, PathBuf},
	process::Stdio,
	time::Duration,
};

use omp_core::Str;
use tokio::{process, time};

const CAPTURE_LIMIT: usize = 1024 * 1024;

/// Captures a bounded, sanitized shell snapshot and returns its private path.
pub(crate) async fn capture(executable: &str, home: &Path) -> io::Result<Option<PathBuf>> {
	let shell = Path::new(executable)
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default();
	if !matches!(shell, "bash" | "zsh") {
		return Ok(None);
	}
	let rc = home.join(if shell == "zsh" { ".zshrc" } else { ".bashrc" });
	let rc = shell_quote(&rc.to_string_lossy());
	let introspect = if shell == "zsh" {
		"alias; functions; printf 'export PATH=%s\\n' \"$PATH\""
	} else {
		"alias -p; declare -f; printf 'export PATH=%q\\n' \"$PATH\""
	};
	let script = format!("umask 077; [ ! -f {rc} ] || . {rc} </dev/null 2>/dev/null; {introspect}");
	let mut command = process::Command::new(executable);
	command
		.args(["-c", &script])
		.env_remove("BASH_ENV")
		.env_remove("ENV")
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.kill_on_drop(true);
	let output = time::timeout(Duration::from_secs(2), command.output())
		.await
		.map_err(io::Error::other)??;
	if !output.status.success() || output.stdout.len() > CAPTURE_LIMIT {
		return Ok(None);
	}
	let sanitized = sanitize(&String::from_utf8_lossy(&output.stdout));
	let directory = env::temp_dir().join(format!("omp-shell-snapshots-{}", std::process::id()));
	fs::create_dir_all(&directory)?;
	#[cfg(unix)]
	fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
	let path = directory.join(format!("snapshot-{shell}-{}.sh", monotonic_name()));
	use std::io::Write as _;
	let mut options = OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	options.mode(0o600);
	let mut file = options.open(&path)?;
	file.write_all(sanitized.as_bytes())?;
	file.sync_all()?;
	Ok(Some(path))
}

fn sanitize(snapshot: &str) -> String {
	let mut output = String::new();
	for line in snapshot.lines().take(4_000) {
		let upper = line.to_ascii_uppercase();
		if ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "PRIVATE_KEY", "BASH_ENV", "ENV="]
			.iter()
			.any(|needle| upper.contains(needle))
		{
			continue;
		}
		if line.starts_with("alias ") {
			let declaration = line.trim_start_matches("alias ").trim_start_matches("-- ");
			let name = declaration.split('=').next().unwrap_or_default();
			let common = matches!(
				name,
				"ls"
					| "cat" | "head"
					| "tail" | "less"
					| "more" | "grep"
					| "rg" | "find"
					| "fd" | "sed"
					| "cp" | "mv"
					| "rm" | "mkdir"
					| "chmod" | "chown"
			);
			let incompatible = declaration.split_once('=').is_some_and(|(_, body)| {
				body
					.chars()
					.any(|character| matches!(character, '(' | ')' | '|' | '&' | ';' | '<' | '>' | '`'))
			});
			if common || incompatible {
				continue;
			}
		}
		if cfg!(windows) && line.contains("='winpty ") {
			continue;
		}
		output.push_str(line);
		output.push('\n');
	}
	output
}

fn shell_quote(value: &str) -> String {
	format!("'{}'", value.replace('\'', "'\\''"))
}

fn monotonic_name() -> Str {
	use std::sync::atomic::{AtomicU64, Ordering};
	static NEXT: AtomicU64 = AtomicU64::new(1);
	Str::from(NEXT.fetch_add(1, Ordering::Relaxed).to_string())
}

#[cfg(test)]
mod tests {
	use super::sanitize;

	#[test]
	fn removes_secrets_and_brush_incompatible_aliases() {
		let output = sanitize("alias ok='ls -la'\nalias bad='(cd /; pwd)'\nexport API_TOKEN=x\n");
		assert!(output.contains("alias ok"));
		assert!(!output.contains("alias bad"));
		assert!(!output.contains("API_TOKEN"));
	}
}
