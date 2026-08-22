//! Resource-owned extension-host cancellation escalation.

use std::{collections::BTreeMap, time::Duration};

use thiserror::Error;
use tokio::process::Child;

use crate::envd::worker::HostKey;

/// Courtesy cancellation grace used by the extension protocol.
pub const CANCEL_GRACE: Duration = Duration::from_millis(150);
/// Maximum forced process-group kills before the extension is disabled.
pub const MAX_KILL_ESCALATIONS_PER_SESSION: u8 = 2;

/// Cancellation stage recorded by the supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelStage {
	/// `CancelDispatch` reached the Python task scope.
	AsyncTask,
	/// The Python runtime was asked to raise `KeyboardInterrupt` in its thread.
	AsyncException,
	/// The process group was killed and must be respawned.
	ProcessGroupKill,
}

/// Journal-ready cancellation escalation fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationJournal {
	/// Extension whose host was terminated.
	pub extension:  HostKey,
	/// Last CONTROL frame correlation observed for the child.
	pub last_frame: u64,
	/// Stage that made cancellation real.
	pub stage:      CancelStage,
}

/// Result of escalating one cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationOutcome {
	/// Courtesy cancellation frame should be sent now.
	DispatchCancel,
	/// Async thread interruption should be sent after the first grace.
	InterruptThread,
	/// The process group was killed; the host must respawn unless disabled.
	Killed(CancellationJournal),
	/// Two forced kills occurred this session; no further host is admitted.
	Disabled(CancellationJournal),
}

/// Cancellation errors.
#[derive(Debug, Error)]
pub enum CancellationError {
	/// The process did not expose a process-group leader id.
	#[error("extension host process has no pid")]
	MissingPid,
	/// The operating system refused the group kill.
	#[error("process-group kill failed: {0}")]
	Kill(#[from] std::io::Error),
}

/// Session-local cancellation state for one extension.
#[derive(Debug, Default)]
pub struct CancellationLadder {
	forced_kills: BTreeMap<HostKey, u8>,
}

impl CancellationLadder {
	/// Begins cancellation by sending `CancelDispatch` to the Python task scope.
	pub const fn begin(&self) -> CancellationOutcome {
		CancellationOutcome::DispatchCancel
	}

	/// Advances after the first grace to the Python asynchronous exception.
	pub const fn interrupt_after_grace(&self) -> CancellationOutcome {
		CancellationOutcome::InterruptThread
	}

	/// Waits the fixed courtesy grace before escalating cancellation.
	pub async fn grace_timer() {
		tokio::time::sleep(CANCEL_GRACE).await;
	}

	/// Kills the child process group after the second grace, journals the event,
	/// and disables an extension after its second forced kill in this session.
	pub fn kill_after_grace(
		&mut self,
		extension: HostKey,
		child: &mut Child,
		last_frame: u64,
	) -> Result<CancellationOutcome, CancellationError> {
		let pid = child.id().ok_or(CancellationError::MissingPid)?;
		#[cfg(unix)]
		{
			nix::sys::signal::killpg(
				nix::unistd::Pid::from_raw(pid.cast_signed()),
				nix::sys::signal::Signal::SIGKILL,
			)
			.map_err(|error| CancellationError::Kill(std::io::Error::other(error)))?;
		}
		#[cfg(windows)]
		child.start_kill()?;
		let kills = self.forced_kills.entry(extension.clone()).or_default();
		*kills = kills.saturating_add(1);
		let journal =
			CancellationJournal { extension, last_frame, stage: CancelStage::ProcessGroupKill };
		if *kills >= MAX_KILL_ESCALATIONS_PER_SESSION {
			Ok(CancellationOutcome::Disabled(journal))
		} else {
			Ok(CancellationOutcome::Killed(journal))
		}
	}

	/// Returns whether the extension was disabled by repeated forced kills.
	pub fn disabled(&self, extension: &HostKey) -> bool {
		self
			.forced_kills
			.get(extension)
			.is_some_and(|kills| *kills >= MAX_KILL_ESCALATIONS_PER_SESSION)
	}
}
