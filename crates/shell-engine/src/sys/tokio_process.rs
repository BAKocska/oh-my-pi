//! Process management utilities

use std::path::Path;

pub(crate) type ProcessId = i32;
pub(crate) use tokio::process::Child;
/// Validates the Windows ConPTY executable contract.
///
/// `CreateProcessW` cannot launch batch files directly under ConPTY. Callers
/// must use `cmd.exe /c <batch>` so quoting and command lookup remain owned by
/// the Windows command processor.
pub(crate) fn validate_pty_application(application: &Path) -> std::io::Result<()> {
	#[cfg(windows)]
	if application
		.extension()
		.and_then(std::ffi::OsStr::to_str)
		.is_some_and(|extension| {
			extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
		}) {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"Windows PTY batch files require cmd.exe with the batch path after /c",
		));
	}
	#[cfg(not(windows))]
	let _ = application;
	Ok(())
}

/// Returns the ConPTY input sequence that emulates SIGINT on Windows.
///
/// ConPTY does not expose Unix process-group signals. Writing ETX follows the
/// terminal path and gives the foreground console process the same Ctrl+C
/// event it receives from an interactive keyboard.
pub(crate) fn pty_sigint_input(signal: &str) -> Option<&'static [u8]> {
	#[cfg(windows)]
	if signal.eq_ignore_ascii_case("SIGINT") {
		return Some(b"\x03");
	}
	#[cfg(not(windows))]
	let _ = signal;
	None
}

pub(crate) fn spawn(command: std::process::Command) -> std::io::Result<Child> {
	let mut command = tokio::process::Command::from(command);
	// `ChildProcess` owns termination policy so disowned children can detach.
	command.kill_on_drop(false);
	// Isolate every external child from the host's console:
	//
	// - `CREATE_NO_WINDOW` gives the child its own *invisible* console instead of
	//   attaching it to ours. Console-sharing children can mutate shared console
	//   state behind the host's back — most notably the output codepage (PHP >=7.1
	//   CLI issues the equivalent of `chcp` and skips the restore when killed;
	//   php.net request #73716), which degraded every non-ASCII glyph a hosting TUI
	//   painted into CP437 mojibake (`Γöé`). Inherited stdio handles are unaffected
	//   (handle-routed, not console-routed); interactive commands belong to the PTY
	//   path, which provisions a dedicated ConPTY anyway.
	// - `CREATE_NEW_PROCESS_GROUP` makes the child a ctrl-event group root. Windows
	//   cannot join an existing group, so this is applied uniformly here rather
	//   than per-command (`creation_flags` replaces rather than ORs; the
	//   `sys::windows::commands` ext traits intentionally leave creation flags
	//   alone).
	#[cfg(windows)]
	{
		use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
		command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
	}
	command.spawn()
}
