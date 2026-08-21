#[cfg(unix)]
pub(crate) fn get() -> std::io::Result<std::ffi::OsString> {
	nix::unistd::gethostname().map_err(std::io::Error::from)
}

#[cfg(windows)]
pub(crate) fn get() -> std::io::Result<std::ffi::OsString> {
	use std::os::windows::ffi::OsStringExt as _;

	use windows_sys::Win32::System::SystemInformation::GetComputerNameW;

	let mut buffer = [0_u16; 256];
	let mut length = buffer.len() as u32;
	// SAFETY: `buffer` contains `length` writable UTF-16 code units.
	if unsafe { GetComputerNameW(buffer.as_mut_ptr(), &mut length) } == 0 {
		return Err(std::io::Error::last_os_error());
	}
	Ok(std::ffi::OsString::from_wide(&buffer[..length as usize]))
}
