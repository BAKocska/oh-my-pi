//! Executor-neutral Unix signal delivery through an async self-pipe.

use std::{
	collections::VecDeque,
	fs::File,
	io,
	os::fd::{FromRawFd as _, OwnedFd, RawFd},
	pin::Pin,
	sync::atomic::{AtomicI32, Ordering},
	task::{Context, Poll},
};

use futures_lite::{Stream, io::AsyncRead as _};
pub use nix::sys::signal::Signal;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet};

const SIGNAL_SLOTS: usize = 128;
static WRITERS: [AtomicI32; SIGNAL_SLOTS] = [const { AtomicI32::new(-1) }; SIGNAL_SLOTS];

extern "C" fn signal_handler(number: nix::libc::c_int) {
	let Ok(index) = usize::try_from(number) else { return };
	let Some(writer) = WRITERS.get(index) else { return };
	let writer = writer.load(Ordering::Relaxed);
	if writer < 0 {
		return;
	}
	let byte = number as u8;
	// SAFETY: the descriptor is installed before the handler and `write` is
	// async-signal-safe. A full nonblocking pipe deliberately drops the wake.
	unsafe {
		nix::libc::write(writer, (&raw const byte).cast(), 1);
	}
}

/// An asynchronous stream of installed Unix signals.
///
/// One process-level stream may own any particular signal at a time. Signal
/// handlers and descriptors are restored when the stream is dropped.
pub struct Signals {
	reader:  async_io::Async<File>,
	writers: Vec<OwnedFd>,
	old:     Vec<(Signal, SigAction)>,
	pending: VecDeque<Signal>,
}

impl Signals {
	/// Installs handlers for `signals` and returns their asynchronous stream.
	///
	/// # Panics
	///
	/// Panics if a pipe or handler cannot be installed, or if another `Signals`
	/// stream already owns one of the requested signals.
	#[must_use]
	pub fn new(signals: &[Signal]) -> Self {
		assert!(!signals.is_empty(), "at least one signal is required");
		let (reader, writer) = pipe().expect("failed to create signal self-pipe");
		let writer_fd = raw_fd(&writer);
		let action = SigAction::new(
			SigHandler::Handler(signal_handler),
			SaFlags::SA_RESTART,
			SigSet::empty(),
		);
		let mut old = Vec::with_capacity(signals.len());
		for &signal in signals {
			let index = signal as usize;
			let slot = WRITERS.get(index).expect("signal number exceeds self-pipe slots");
			slot.compare_exchange(-1, writer_fd, Ordering::AcqRel, Ordering::Acquire)
				.expect("signal already owned by another Signals stream");
			// SAFETY: `action` uses an async-signal-safe handler with C ABI.
			let previous = unsafe { nix::sys::signal::sigaction(signal, &action) }
				.expect("failed to install signal handler");
			old.push((signal, previous));
		}
		let reader = async_io::Async::new(File::from(reader))
			.expect("failed to register signal self-pipe");
		Self { reader, writers: vec![writer], old, pending: VecDeque::new() }
	}
}

impl Stream for Signals {
	type Item = Signal;

	fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		if let Some(signal) = self.pending.pop_front() {
			return Poll::Ready(Some(signal));
		}
		let mut bytes = [0_u8; 64];
		match Pin::new(&mut self.reader).poll_read(context, &mut bytes) {
			Poll::Ready(Ok(0)) => Poll::Ready(None),
			Poll::Ready(Ok(read)) => {
				self.pending.extend(
					bytes[..read]
						.iter()
						.filter_map(|number| Signal::try_from(i32::from(*number)).ok()),
				);
				Poll::Ready(self.pending.pop_front())
			},
			Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {
				context.waker().wake_by_ref();
				Poll::Pending
			},
			Poll::Ready(Err(_)) => Poll::Ready(None),
			Poll::Pending => Poll::Pending,
		}
	}
}

impl Drop for Signals {
	fn drop(&mut self) {
		for (signal, previous) in self.old.iter().rev() {
			// SAFETY: this restores the action returned by `sigaction` at install.
			unsafe {
				let _ = nix::sys::signal::sigaction(*signal, previous);
			}
			WRITERS[*signal as usize].store(-1, Ordering::Release);
		}
		self.writers.clear();
	}
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
	let mut descriptors = [-1; 2];
	// SAFETY: `descriptors` has space for the two descriptors returned by pipe.
	if unsafe { nix::libc::pipe(descriptors.as_mut_ptr()) } != 0 {
		return Err(io::Error::last_os_error());
	}
	for descriptor in descriptors {
		// SAFETY: fcntl operates on a live descriptor returned by pipe.
		let status_flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFL) };
		let descriptor_flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFD) };
		if status_flags < 0
			|| descriptor_flags < 0
			|| unsafe {
				nix::libc::fcntl(
					descriptor,
					nix::libc::F_SETFL,
					status_flags | nix::libc::O_NONBLOCK,
				)
			} < 0
			|| unsafe {
				nix::libc::fcntl(
					descriptor,
					nix::libc::F_SETFD,
					descriptor_flags | nix::libc::FD_CLOEXEC,
				)
			} < 0
		{
			let error = io::Error::last_os_error();
			// SAFETY: both descriptors remain owned by this function on failure.
			unsafe {
				nix::libc::close(descriptors[0]);
				nix::libc::close(descriptors[1]);
			}
			return Err(error);
		}
	}
	// SAFETY: each successful pipe descriptor is transferred exactly once.
	Ok(unsafe { (OwnedFd::from_raw_fd(descriptors[0]), OwnedFd::from_raw_fd(descriptors[1])) })
}

fn raw_fd(fd: &OwnedFd) -> RawFd {
	use std::os::fd::AsRawFd as _;
	fd.as_raw_fd()
}
