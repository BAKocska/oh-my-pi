//! `CPython` async interruption shared by embedded Python hosts.

use std::ffi::{c_long, c_ulong};

use pyo3::{Python, ffi};

unsafe extern "C" {
	fn PyThread_get_thread_ident() -> c_ulong;
}

/// Returns `CPython`'s identifier for the attached current thread.
pub fn current_thread_id() -> u64 {
	// SAFETY: callers use this only while their thread is attached to CPython.
	unsafe { PyThread_get_thread_ident() as u64 }
}

/// Raises `KeyboardInterrupt` asynchronously in the identified live Python
/// thread.
///
/// Returns `true` only when `CPython` selected exactly one thread state.
/// `CPython`'s documented recovery for an ambiguous selection is performed
/// before returning.
pub fn interrupt(_py: Python<'_>, thread_id: u64) -> bool {
	let Ok(id) = c_long::try_from(thread_id) else {
		return false;
	};
	// SAFETY: the caller is attached; the exception type is immortal and owned by
	// CPython. An ambiguous result is immediately cleared as required by CPython.
	let changed = unsafe { ffi::PyThreadState_SetAsyncExc(id, ffi::PyExc_KeyboardInterrupt) };
	if changed > 1 {
		// SAFETY: clears only the ambiguous exception set directly above.
		unsafe { ffi::PyThreadState_SetAsyncExc(id, std::ptr::null_mut()) };
	}
	changed == 1
}
