//! Descriptor-relative directory traversal used by recursive removal.

#![cfg(unix)]

use std::{
	ffi::{CStr, CString, OsStr, OsString},
	io,
	mem::MaybeUninit,
	os::{
		fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
		unix::ffi::{OsStrExt, OsStringExt},
	},
	path::Path,
};

use rustix::fs::{AtFlags, CWD, Mode, OFlags};

/// Selects whether descriptor-relative operations follow a final symlink.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SymlinkBehavior {
	/// Follow the final symbolic link.
	#[default]
	Follow,
	/// Operate on the symbolic link itself.
	NoFollow,
}

/// An owned directory descriptor for race-resistant relative operations.
#[derive(Debug)]
pub(crate) struct DirFd {
	fd: OwnedFd,
}

impl DirFd {
	/// Opens a directory, optionally refusing a final symbolic link.
	pub(crate) fn open(path: &Path, symlink_behavior: SymlinkBehavior) -> io::Result<Self> {
		let _ = symlink_behavior;
		let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
		let fd = rustix::fs::openat(CWD, path, flags, Mode::empty()).map_err(io::Error::from)?;
		Ok(Self { fd })
	}

	/// Opens a child directory relative to this descriptor.
	pub(crate) fn open_subdir(
		&self,
		name: &OsStr,
		symlink_behavior: SymlinkBehavior,
	) -> io::Result<Self> {
		validate_name(name)?;
		let _ = symlink_behavior;
		let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
		let fd = rustix::fs::openat(&self.fd, name, flags, Mode::empty()).map_err(io::Error::from)?;
		Ok(Self { fd })
	}

	/// Stats a child relative to this descriptor.
	pub(crate) fn stat_at(
		&self,
		name: &OsStr,
		symlink_behavior: SymlinkBehavior,
	) -> io::Result<libc::stat> {
		let name = os_str_to_cstring(name)?;
		let mut stat = MaybeUninit::<libc::stat>::uninit();
		let flags = if symlink_behavior == SymlinkBehavior::NoFollow {
			libc::AT_SYMLINK_NOFOLLOW
		} else {
			0
		};
		// SAFETY: the descriptor and C string are valid; `stat` points to writable
		// storage.
		let result =
			unsafe { libc::fstatat(self.fd.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), flags) };
		if result == 0 {
			// SAFETY: successful `fstatat` initialized the entire stat structure.
			Ok(unsafe { stat.assume_init() })
		} else {
			Err(io::Error::last_os_error())
		}
	}

	/// Lists child names, excluding `.` and `..`, without resolving another
	/// path.
	pub(crate) fn read_dir(&self) -> io::Result<Vec<OsString>> {
		#[cfg(any(target_os = "linux", target_os = "android"))]
		{
			self.read_dir_raw()
		}
		#[cfg(not(any(target_os = "linux", target_os = "android")))]
		{
			self.read_dir_libc()
		}
	}

	#[cfg(any(target_os = "linux", target_os = "android"))]
	fn read_dir_raw(&self) -> io::Result<Vec<OsString>> {
		let duplicate = rustix::io::dup(&self.fd).map_err(io::Error::from)?;
		let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
		let mut directory = rustix::fs::RawDir::new(duplicate, &mut buffer);
		let mut entries = Vec::new();
		while let Some(entry) = directory.next() {
			let entry = entry.map_err(io::Error::from)?;
			let bytes = entry.file_name().to_bytes();
			if bytes != b"." && bytes != b".." {
				entries.push(OsString::from_vec(bytes.to_vec()));
			}
		}
		Ok(entries)
	}

	#[cfg(not(any(target_os = "linux", target_os = "android")))]
	fn read_dir_libc(&self) -> io::Result<Vec<OsString>> {
		let duplicate = rustix::io::dup(&self.fd).map_err(io::Error::from)?;
		let raw = duplicate.into_raw_fd();
		// SAFETY: `raw` is an owned directory descriptor; `fdopendir` consumes it on
		// success.
		let directory = unsafe { libc::fdopendir(raw) };
		if directory.is_null() {
			let error = io::Error::last_os_error();
			// SAFETY: `fdopendir` failed and therefore did not consume `raw`.
			drop(unsafe { OwnedFd::from_raw_fd(raw) });
			return Err(error);
		}

		let mut entries = Vec::new();
		loop {
			set_errno_zero();
			// SAFETY: `directory` is live until `closedir` below.
			let entry = unsafe { libc::readdir(directory) };
			if entry.is_null() {
				let error = io::Error::last_os_error();
				// SAFETY: `directory` is a valid DIR pointer and is closed exactly once.
				unsafe { libc::closedir(directory) };
				return if error.raw_os_error() == Some(0) {
					Ok(entries)
				} else {
					Err(error)
				};
			}
			// SAFETY: `d_name` is a NUL-terminated array owned by the live directory
			// stream.
			let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
			if name != b"." && name != b".." {
				entries.push(OsString::from_vec(name.to_vec()));
			}
		}
	}

	/// Removes a child file or empty child directory relative to this
	/// descriptor.
	pub(crate) fn unlink_at(&self, name: &OsStr, is_dir: bool) -> io::Result<()> {
		validate_name(name)?;
		let flags = if is_dir {
			AtFlags::REMOVEDIR
		} else {
			AtFlags::empty()
		};
		rustix::fs::unlinkat(&self.fd, name, flags).map_err(io::Error::from)
	}
}

fn validate_name(name: &OsStr) -> io::Result<()> {
	if name.as_bytes().contains(&0) {
		Err(io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
	} else {
		Ok(())
	}
}

fn os_str_to_cstring(name: &OsStr) -> io::Result<CString> {
	CString::new(name.as_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_errno_zero() {
	#[cfg(target_vendor = "apple")]
	// SAFETY: the returned pointer is thread-local and valid for one integer write.
	unsafe {
		*libc::__error() = 0;
	}
	#[cfg(not(target_vendor = "apple"))]
	// SAFETY: the returned pointer is thread-local and valid for one integer write.
	unsafe {
		*libc::__errno_location() = 0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn traverses_and_unlinks_temp_tree_by_descriptor() {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("child")).unwrap();
		std::fs::write(root.path().join("child/file"), b"data").unwrap();

		let root_fd = DirFd::open(root.path(), SymlinkBehavior::NoFollow).unwrap();
		assert_eq!(root_fd.read_dir().unwrap(), [OsString::from("child")]);
		let child = root_fd
			.open_subdir(OsStr::new("child"), SymlinkBehavior::NoFollow)
			.unwrap();
		let stat = child
			.stat_at(OsStr::new("file"), SymlinkBehavior::NoFollow)
			.unwrap();
		assert_eq!(stat.st_size, 4);
		child.unlink_at(OsStr::new("file"), false).unwrap();
		drop(child);
		root_fd.unlink_at(OsStr::new("child"), true).unwrap();
		assert!(root_fd.read_dir().unwrap().is_empty());
	}
}
