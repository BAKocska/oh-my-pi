//! Session-local scratch resource resolver.

use std::{
	ffi, fs, io,
	path::{self, Component, Path, PathBuf},
	str,
};

use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	resolver::{
		LineOffsetCache, Resolve, ResourceCompletion, ResourceEntry, ResourceList, fuzzy_score,
	},
	selector::ParsedSelector,
};
use url::Url;

const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const SNIFF_BYTES: usize = 8 * 1024;

/// Copies session-local artifacts across a session handoff.
///
/// Only regular files and directories are migrated; symbolic links and other
/// filesystem objects are ignored rather than followed.
pub(crate) fn migrate_session_artifacts(
	sessions_dir: &Path,
	source_session: &str,
	destination_session: &str,
) -> Result<(), io::Error> {
	if source_session == destination_session {
		return Ok(());
	}
	let source = sessions_dir.join(source_session).join("local");
	let destination = sessions_dir.join(destination_session).join("local");
	match fs::symlink_metadata(&source) {
		Ok(metadata) if metadata.file_type().is_dir() => {},
		Ok(_) => return Ok(()),
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error),
	}
	fs::create_dir_all(&destination)?;
	copy_artifact_entries(&source, &destination)
}

fn copy_artifact_entries(source: &Path, destination: &Path) -> Result<(), io::Error> {
	for entry in fs::read_dir(source)? {
		let entry = entry?;
		let file_type = entry.file_type()?;
		let destination = destination.join(entry.file_name());
		if file_type.is_dir() {
			if destination.exists() && !fs::symlink_metadata(&destination)?.file_type().is_dir() {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"local artifact destination collides with a non-directory",
				));
			}
			fs::create_dir_all(&destination)?;
			copy_artifact_entries(&entry.path(), &destination)?;
		} else if file_type.is_file() {
			if destination.exists() && fs::symlink_metadata(&destination)?.file_type().is_symlink() {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"local artifact destination is a symbolic link",
				));
			}
			fs::copy(entry.path(), destination)?;
		}
	}
	Ok(())
}

/// Confined resolver for one session's local scratch root.
#[derive(Debug)]
pub(crate) struct LocalResolver {
	root:  PathBuf,
	lines: LineOffsetCache,
}

impl LocalResolver {
	pub(super) fn open(root: PathBuf) -> Result<Self, io::Error> {
		fs::create_dir_all(&root)?;
		let root = fs::canonicalize(root)?;
		Ok(Self { root, lines: LineOffsetCache::default() })
	}

	fn target(&self, resource: &str) -> Result<PathBuf, Fault> {
		let relative = decode_relative(resource)?;
		let candidate = self.root.join(&relative);
		let canonical = fs::canonicalize(&candidate).map_err(|source| Fault::Source {
			message: Str::new(format!(
				"Local resource '{}' cannot be resolved: {source}",
				relative.display()
			)),
		})?;
		if !canonical.starts_with(&self.root) {
			return Err(Fault::Invalid {
				message: Str::new_static("local:// path escapes the session scratch root."),
			});
		}
		Ok(canonical)
	}

	fn entries(&self, directory: &Path) -> Result<Vec<ResourceEntry>, Fault> {
		let mut entries = Vec::new();
		for entry in fs::read_dir(directory).map_err(io_fault)? {
			let entry = entry.map_err(io_fault)?;
			let path = entry.path();
			let canonical = fs::canonicalize(&path).map_err(io_fault)?;
			if !canonical.starts_with(&self.root) {
				continue;
			}
			let metadata = fs::metadata(&canonical).map_err(io_fault)?;
			if !metadata.is_file() && !metadata.is_dir() {
				continue;
			}
			let relative = canonical
				.strip_prefix(&self.root)
				.expect("contained local path")
				.to_string_lossy()
				.replace(path::MAIN_SEPARATOR, "/");
			let directory = metadata.is_dir();
			let name = entry.file_name().to_string_lossy().into_owned();
			entries.push(ResourceEntry {
				uri: Str::new(format!("local://{}{}", relative, if directory { "/" } else { "" })),
				name: Str::new(name),
				directory,
				size: if directory { 0 } else { metadata.len() },
			});
		}
		entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
		Ok(entries)
	}

	fn completion_files(&self) -> Result<Vec<(Str, Str)>, Fault> {
		let mut pending = vec![self.root.clone()];
		let mut output = Vec::new();
		while let Some(directory) = pending.pop() {
			for entry in fs::read_dir(&directory).map_err(io_fault)? {
				let entry = entry.map_err(io_fault)?;
				let canonical = fs::canonicalize(entry.path()).map_err(io_fault)?;
				if !canonical.starts_with(&self.root) {
					continue;
				}
				let metadata = fs::metadata(&canonical).map_err(io_fault)?;
				if metadata.is_dir() {
					pending.push(canonical);
				} else if metadata.is_file() {
					let relative = canonical
						.strip_prefix(&self.root)
						.expect("contained local path")
						.to_string_lossy()
						.replace(path::MAIN_SEPARATOR, "/");
					output.push((Str::new(format!("local://{relative}")), Str::new(relative)));
				}
			}
		}
		output.sort_unstable_by(|left, right| left.0.cmp(&right.0));
		Ok(output)
	}
}

impl Resolve for LocalResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		use super::select_bytes;
		let target = self.target(resource)?;
		let metadata = fs::metadata(&target).map_err(io_fault)?;
		if metadata.is_dir() {
			let entries = self.entries(&target)?;
			let mut output = String::from("# Local\n\n");
			if entries.is_empty() {
				output.push_str("(empty)\n");
			} else {
				for entry in entries {
					output.push_str("- [");
					output.push_str(&entry.name);
					if entry.directory {
						output.push('/');
					}
					output.push_str("](");
					output.push_str(&entry.uri);
					output.push_str(")\n");
				}
			}
			return Ok(CowBytes::from(output.into_bytes()));
		}
		if !metadata.is_file() {
			return Err(Fault::Invalid {
				message: Str::new_static("local:// resources must be regular files or directories."),
			});
		}
		if known_binary(&target) {
			return Err(binary_fault(resource));
		}
		let bytes = fs::read(&target).map_err(io_fault)?;
		let sniff = &bytes[..bytes.len().min(SNIFF_BYTES)];
		if sniff.contains(&0) || str::from_utf8(sniff).is_err() {
			return Err(binary_fault(resource));
		}
		if metadata.len() > MAX_TEXT_BYTES
			&& matches!(selector, ParsedSelector::None | ParsedSelector::Raw)
		{
			return Err(Fault::Invalid {
				message: Str::new(format!(
					"local://{resource} is {} bytes; full text resolution is limited to \
					 {MAX_TEXT_BYTES} bytes. Use a line selector or path-only read.",
					metadata.len()
				)),
			});
		}
		select_bytes(&self.lines, resource, CowBytes::from(bytes), selector)
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		let target = self.target(resource)?;
		if !target.is_dir() {
			return Err(Fault::Invalid {
				message: Str::new_static("Only local:// directories can be listed."),
			});
		}
		let mut entries = self.entries(&target)?;
		let mut used = 0;
		let retain = entries
			.iter()
			.take(max_entries)
			.take_while(|entry| {
				let next = used + entry.uri.len() + entry.name.len();
				let keep = next <= max_bytes;
				if keep {
					used = next;
				}
				keep
			})
			.count();
		let truncated = retain < entries.len();
		entries.truncate(retain);
		Ok(ResourceList { entries, truncated })
	}

	async fn path(&self, resource: &str) -> Result<Option<Str>, Fault> {
		let target = self.target(resource)?;
		let url = Url::from_file_path(target).map_err(|()| Fault::Invalid {
			message: Str::new_static("local:// path cannot be represented as a file URI."),
		})?;
		Ok(Some(Str::new(url.as_str())))
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut matches = self
			.completion_files()?
			.into_iter()
			.filter_map(|(value, relative)| {
				let score = fuzzy_score(query, &relative)?;
				Some(ResourceCompletion { value, description: relative, score })
			})
			.collect::<Vec<_>>();
		matches.sort_unstable_by(|left, right| {
			right
				.score
				.cmp(&left.score)
				.then_with(|| left.value.cmp(&right.value))
		});
		matches.truncate(max_results);
		Ok(matches)
	}
}

fn decode_relative(resource: &str) -> Result<PathBuf, Fault> {
	let mut bytes = Vec::with_capacity(resource.len());
	let mut index = 0;
	while index < resource.len() {
		if resource.as_bytes()[index] == b'%' {
			let encoded = resource
				.as_bytes()
				.get(index + 1..index + 3)
				.ok_or_else(|| Fault::Invalid {
					message: Str::new_static("local:// path contains invalid percent encoding."),
				})?;
			let high = hex_nibble(encoded[0]).ok_or_else(|| Fault::Invalid {
				message: Str::new_static("local:// path contains invalid percent encoding."),
			})?;
			let low = hex_nibble(encoded[1]).ok_or_else(|| Fault::Invalid {
				message: Str::new_static("local:// path contains invalid percent encoding."),
			})?;
			bytes.push(high << 4 | low);
			index += 3;
		} else {
			bytes.push(resource.as_bytes()[index]);
			index += 1;
		}
	}
	let decoded = String::from_utf8(bytes).map_err(|_| Fault::Invalid {
		message: Str::new_static("local:// path contains invalid percent-encoded UTF-8."),
	})?;
	let path = Path::new(&decoded);
	if path.is_absolute()
		|| decoded.contains('\\')
		|| path
			.components()
			.any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
	{
		return Err(Fault::Invalid {
			message: Str::new_static("local:// path must be relative and cannot traverse its root."),
		});
	}
	Ok(path.to_path_buf())
}

fn hex_nibble(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}

fn known_binary(path: &Path) -> bool {
	path
		.extension()
		.and_then(ffi::OsStr::to_str)
		.is_some_and(|extension| {
			matches!(
				extension.to_ascii_lowercase().as_str(),
				"png"
					| "jpg" | "jpeg"
					| "gif" | "webp"
					| "pdf" | "zip"
					| "gz" | "mp3"
					| "mp4" | "mov"
					| "wasm" | "sqlite"
					| "db"
			)
		})
}

fn binary_fault(resource: &str) -> Fault {
	Fault::Invalid {
		message: Str::new(format!(
			"local://{resource} is not UTF-8 text; use a metadata or media-specific workflow."
		)),
	}
}

fn io_fault(source: io::Error) -> Fault {
	Fault::Source { message: Str::new(format!("local:// I/O failed: {source}")) }
}
