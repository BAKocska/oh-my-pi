//! Filesystem identity, path resolution, and mode-display helpers.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
	collections::VecDeque,
	fs,
	hash::{Hash, Hasher},
	io,
	path::{Component, Path, PathBuf},
};

/// Information that uniquely identifies a file on its filesystem.
#[derive(Clone, Debug)]
pub(crate) struct FileInformation {
	#[cfg(unix)]
	dev:    u64,
	#[cfg(unix)]
	ino:    u64,
	#[cfg(not(unix))]
	len:    u64,
	#[cfg(not(unix))]
	is_dir: bool,
}

impl FileInformation {
	/// Reads identity information for `path`, optionally following its final
	/// symlink.
	pub(crate) fn from_path(path: impl AsRef<Path>, dereference: bool) -> io::Result<Self> {
		let metadata = if dereference {
			fs::metadata(path)
		} else {
			fs::symlink_metadata(path)
		}?;
		#[cfg(unix)]
		return Ok(Self { dev: metadata.dev(), ino: metadata.ino() });
		#[cfg(not(unix))]
		return Ok(Self { len: metadata.len(), is_dir: metadata.is_dir() });
	}
}

impl PartialEq for FileInformation {
	fn eq(&self, other: &Self) -> bool {
		#[cfg(unix)]
		return self.dev == other.dev && self.ino == other.ino;
		#[cfg(not(unix))]
		return self.len == other.len && self.is_dir == other.is_dir;
	}
}

impl Eq for FileInformation {}

impl Hash for FileInformation {
	fn hash<H: Hasher>(&self, state: &mut H) {
		#[cfg(unix)]
		{
			self.dev.hash(state);
			self.ino.hash(state);
		}
		#[cfg(not(unix))]
		{
			self.len.hash(state);
			self.is_dir.hash(state);
		}
	}
}

/// Controls which missing components are accepted during canonicalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MissingHandling {
	/// Require all but the final component to exist.
	Normal,
	/// Require every component to exist.
	Existing,
	/// Permit any missing component.
	Missing,
}

/// Controls when symbolic links are resolved during canonicalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolveMode {
	/// Do not resolve symbolic links.
	None,
	/// Resolve symbolic links as each component is encountered.
	Physical,
	/// Resolve lexical parent components before resolving symbolic links.
	Logical,
}

#[derive(Clone, Debug)]
enum OwnedComponent {
	Prefix(std::ffi::OsString),
	Root,
	Current,
	Parent,
	Normal(std::ffi::OsString),
}

impl OwnedComponent {
	fn from_component(component: Component<'_>) -> Self {
		match component {
			Component::Prefix(_) => Self::Prefix(component.as_os_str().to_owned()),
			Component::RootDir => Self::Root,
			Component::CurDir => Self::Current,
			Component::ParentDir => Self::Parent,
			Component::Normal(value) => Self::Normal(value.to_owned()),
		}
	}

	fn push_onto(&self, path: &mut PathBuf) {
		match self {
			Self::Prefix(value) | Self::Normal(value) => path.push(value),
			Self::Root => path.push(Component::RootDir.as_os_str()),
			Self::Current => {},
			Self::Parent => {
				path.pop();
			},
		}
	}
}

fn normalize_lexically(path: &Path) -> PathBuf {
	let mut result = PathBuf::new();
	for component in path.components() {
		match component {
			Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
				result.push(component.as_os_str());
			},
			Component::CurDir => {},
			Component::ParentDir => {
				if result.as_os_str().is_empty()
					|| matches!(result.components().next_back(), Some(Component::ParentDir))
				{
					result.push("..");
				} else {
					result.pop();
				}
			},
		}
	}
	if result.as_os_str().is_empty() {
		result.push(".");
	}
	result
}

/// Returns an absolute, normalized path with GNU-compatible missing-component
/// handling.
pub(crate) fn canonicalize(
	original: impl AsRef<Path>,
	missing: MissingHandling,
	resolve: ResolveMode,
) -> io::Result<PathBuf> {
	const MAX_SYMLINKS: usize = 40;
	let original = original.as_ref();
	let requires_directory =
		missing != MissingHandling::Missing && path_ends_with_terminator(original);
	let absolute = if original.is_absolute() {
		original.to_owned()
	} else {
		fs::canonicalize(std::env::current_dir()?)?.join(original)
	};
	let path = if resolve == ResolveMode::Logical {
		normalize_lexically(&absolute)
	} else {
		absolute
	};
	let mut pending: VecDeque<_> = path
		.components()
		.map(OwnedComponent::from_component)
		.collect();
	let mut result = PathBuf::new();
	let mut followed = 0_usize;

	while let Some(component) = pending.pop_front() {
		component.push_onto(&mut result);
		if matches!(component, OwnedComponent::Current | OwnedComponent::Parent) {
			continue;
		}
		match fs::symlink_metadata(&result) {
			Ok(metadata) if resolve != ResolveMode::None && metadata.file_type().is_symlink() => {
				followed += 1;
				if followed > MAX_SYMLINKS {
					return Err(symlink_loop_error());
				}
				let target = fs::read_link(&result)?;
				result.pop();
				if target.is_absolute() {
					result.clear();
				}
				for target_component in target.components().rev() {
					pending.push_front(OwnedComponent::from_component(target_component));
				}
			},
			Ok(_) => {},
			Err(error) => {
				let may_be_missing = missing == MissingHandling::Missing
					|| (missing == MissingHandling::Normal && pending.is_empty());
				if !may_be_missing {
					return Err(error);
				}
			},
		}
	}

	if requires_directory {
		fs::read_dir(&result)?;
	} else if missing == MissingHandling::Normal && !result.exists() {
		if let Some(parent) = result.parent() {
			fs::read_dir(parent)?;
		}
	}
	Ok(result)
}

/// Converts absolute `path` to a path relative to absolute `base`.
pub(crate) fn make_path_relative_to(path: impl AsRef<Path>, base: impl AsRef<Path>) -> PathBuf {
	let path = path.as_ref();
	let base = base.as_ref();
	let common = path
		.components()
		.zip(base.components())
		.take_while(|(left, right)| left == right)
		.count();
	let mut relative = PathBuf::new();
	for _ in base.components().skip(common) {
		relative.push("..");
	}
	for component in path.components().skip(common) {
		relative.push(component.as_os_str());
	}
	if relative.as_os_str().is_empty() {
		relative.push(".");
	}
	relative
}

/// Returns whether two paths identify the same file.
pub(crate) fn paths_refer_to_same_file(
	left: impl AsRef<Path>,
	right: impl AsRef<Path>,
	dereference: bool,
) -> bool {
	matches!(
		(FileInformation::from_path(left, dereference), FileInformation::from_path(right, dereference)),
		(Ok(left), Ok(right)) if left == right
	)
}

/// Returns whether two paths are hard links to the same inode.
#[cfg(unix)]
pub(crate) fn are_hardlinks_to_same_file(source: &Path, target: &Path) -> bool {
	let Ok(source) = fs::symlink_metadata(source) else {
		return false;
	};
	let Ok(target) = fs::symlink_metadata(target) else {
		return false;
	};
	source.dev() == target.dev() && source.ino() == target.ino()
}

/// Returns `false` because portable hard-link identity is unavailable.
#[cfg(not(unix))]
pub(crate) fn are_hardlinks_to_same_file(_source: &Path, _target: &Path) -> bool {
	false
}

/// Returns whether paths are hard links, or `source` resolves to `target` in
/// one direction.
#[cfg(unix)]
pub(crate) fn are_hardlinks_or_one_way_symlink_to_same_file(source: &Path, target: &Path) -> bool {
	let Ok(source) = fs::metadata(source) else {
		return false;
	};
	let Ok(target) = fs::symlink_metadata(target) else {
		return false;
	};
	source.dev() == target.dev() && source.ino() == target.ino()
}

/// Returns `false` because portable link identity is unavailable.
#[cfg(not(unix))]
pub(crate) fn are_hardlinks_or_one_way_symlink_to_same_file(
	_source: &Path,
	_target: &Path,
) -> bool {
	false
}

/// Formats metadata permissions, optionally prefixed with the file type.
pub(crate) fn display_permissions(metadata: &fs::Metadata, display_file_type: bool) -> String {
	#[cfg(unix)]
	return display_permissions_unix(metadata.mode(), display_file_type);
	#[cfg(not(unix))]
	{
		let write = !metadata.permissions().readonly();
		let mut result = String::with_capacity(if display_file_type { 10 } else { 9 });
		if display_file_type {
			result.push(if metadata.is_dir() { 'd' } else { '-' });
		}
		result.push_str(if write { "rwxrwxrwx" } else { "r-xr-xr-x" });
		result
	}
}

fn file_type_character(mode: u32) -> char {
	match mode & 0o170_000 {
		0o040_000 => 'd',
		0o020_000 => 'c',
		0o060_000 => 'b',
		0o100_000 => '-',
		0o010_000 => 'p',
		0o120_000 => 'l',
		0o140_000 => 's',
		_ => '?',
	}
}

/// Formats a Unix mode as `-rwxr-xr-x`, including set-id and sticky letters.
pub(crate) fn display_permissions_unix(mode: u32, display_file_type: bool) -> String {
	let mut result = String::with_capacity(if display_file_type { 10 } else { 9 });
	if display_file_type {
		result.push(file_type_character(mode));
	}
	for (read, write, execute, special, lower, upper) in [
		(0o400, 0o200, 0o100, 0o4000, 's', 'S'),
		(0o040, 0o020, 0o010, 0o2000, 's', 'S'),
		(0o004, 0o002, 0o001, 0o1000, 't', 'T'),
	] {
		result.push(if mode & read != 0 { 'r' } else { '-' });
		result.push(if mode & write != 0 { 'w' } else { '-' });
		result.push(match (mode & execute != 0, mode & special != 0) {
			(true, true) => lower,
			(false, true) => upper,
			(true, false) => 'x',
			(false, false) => '-',
		});
	}
	result
}

/// Strips a final `/.` spelling so directory creation accepts GNU's `dir/.`
/// case.
pub(crate) fn dir_strip_dot_for_creation(path: &Path) -> PathBuf {
	let bytes = path.as_os_str().to_string_lossy();
	if bytes.ends_with("/.") || bytes.ends_with("/./") {
		path.components().collect()
	} else {
		path.to_owned()
	}
}

/// Returns whether the original path spelling ends in a directory separator.
pub(crate) fn path_ends_with_terminator(path: &Path) -> bool {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		path.as_os_str().as_bytes().last() == Some(&b'/')
	}
	#[cfg(windows)]
	{
		use std::os::windows::ffi::OsStrExt;
		matches!(path.as_os_str().encode_wide().last(), Some(value) if value == u16::from(b'/') || value == u16::from(b'\\'))
	}
	#[cfg(not(any(unix, windows)))]
	{
		path.to_string_lossy().ends_with('/')
	}
}

/// Creates a FIFO with mode `0666`, subject to the process umask.
#[cfg(unix)]
pub(crate) fn make_fifo(path: &Path) -> io::Result<()> {
	use std::{ffi::CString, os::unix::ffi::OsStrExt};
	let path = CString::new(path.as_os_str().as_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
	// SAFETY: `path` is a live, NUL-terminated C string and `mkfifo` retains no
	// pointer.
	if unsafe { libc::mkfifo(path.as_ptr(), 0o666) } == 0 {
		Ok(())
	} else {
		Err(io::Error::last_os_error())
	}
}

/// Block-size sanity helpers used by streaming builtins.
pub(crate) mod sane_blksize {
	use std::fs::Metadata;

	/// Minimum accepted filesystem I/O block size.
	pub(crate) const MIN: u64 = 512;
	/// Maximum accepted filesystem I/O block size.
	pub(crate) const MAX: u64 = 4 * 1024 * 1024;

	/// Returns a metadata block size clamped to the safe streaming range.
	pub(crate) fn sane_blksize_from_metadata(metadata: &Metadata) -> u64 {
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;
			metadata.blksize().clamp(MIN, MAX)
		}
		#[cfg(not(unix))]
		{
			let _ = metadata;
			MIN
		}
	}
}

#[cfg(unix)]
pub(crate) use libc::{major, minor};

fn symlink_loop_error() -> io::Error {
	#[cfg(unix)]
	{
		io::Error::from_raw_os_error(libc::ELOOP)
	}
	#[cfg(not(unix))]
	{
		io::Error::new(io::ErrorKind::InvalidInput, "too many levels of symbolic links")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn permission_vectors_cover_special_bits() {
		for (mode, expected) in [
			(0o100_000 | 0o755, "-rwxr-xr-x"),
			(0o040_000, "d---------"),
			(0o100_000 | 0o7777, "-rwsrwsrwt"),
			(0o100_000 | 0o7000, "---S--S--T"),
			(0o100_000 | 0o4644, "-rwSr--r--"),
			(0o100_000 | 0o2755, "-rwxr-sr-x"),
			(0o100_000 | 0o1755, "-rwxr-xr-t"),
		] {
			assert_eq!(display_permissions_unix(mode, true), expected);
		}
	}

	#[test]
	fn canonicalizes_existing_and_missing_paths() {
		let directory = tempfile::tempdir().unwrap();
		let canonical_root = fs::canonicalize(directory.path()).unwrap();
		fs::create_dir(directory.path().join("dir")).unwrap();
		fs::write(directory.path().join("dir/file"), b"x").unwrap();
		let existing = canonicalize(
			directory.path().join("dir/../dir/file"),
			MissingHandling::Existing,
			ResolveMode::Physical,
		)
		.unwrap();
		assert_eq!(existing, canonical_root.join("dir/file"));
		let missing = canonicalize(
			directory.path().join("dir/new"),
			MissingHandling::Normal,
			ResolveMode::Physical,
		)
		.unwrap();
		assert_eq!(missing, canonical_root.join("dir/new"));
		assert!(
			canonicalize(
				directory.path().join("absent/new"),
				MissingHandling::Normal,
				ResolveMode::Physical,
			)
			.is_err()
		);
		assert!(
			canonicalize(
				directory.path().join("absent"),
				MissingHandling::Existing,
				ResolveMode::None,
			)
			.is_err()
		);
	}

	#[cfg(unix)]
	#[test]
	fn canonicalize_reports_symlink_loop() {
		use std::os::unix::fs::symlink;
		let directory = tempfile::tempdir().unwrap();
		symlink("b", directory.path().join("a")).unwrap();
		symlink("a", directory.path().join("b")).unwrap();
		let error =
			canonicalize(directory.path().join("a"), MissingHandling::Existing, ResolveMode::Physical)
				.unwrap_err();
		assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
	}
}
