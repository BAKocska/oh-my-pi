//! Production composition boundary for shared audio ownership.
//!
//! The policy state machine remains in [`omp_voice::coordinator`]. This module
//! only adapts its suspension and gain transitions to the application's local
//! text-to-speech controller.

use std::sync::Arc;

use omp_voice::coordinator::{AudioCoordinator, AudioEffects};

/// Application-side local text-to-speech controls consumed by the voice
/// coordinator adapter.
pub trait LocalTtsControl: Send + Sync + 'static {
	/// Suspend or resume creation and playback of local speech.
	fn set_suspended(&self, suspended: bool);

	/// Set render-time playback gain for current and future local speech.
	fn set_gain(&self, gain: f32);
}

struct ApplicationAudioEffects<C> {
	control: Arc<C>,
}

impl<C> AudioEffects for ApplicationAudioEffects<C>
where
	C: LocalTtsControl,
{
	fn set_tts_suspended(&self, suspended: bool) {
		self.control.set_suspended(suspended);
	}

	fn set_tts_gain(&self, gain: f32) {
		self.control.set_gain(gain);
	}
}

/// Application wrapper around the domain-owned audio coordinator.
#[derive(Clone)]
pub struct AppAudioCoordinator {
	domain: AudioCoordinator,
}

impl AppAudioCoordinator {
	/// Compose audio ownership policy with the production local-TTS controller.
	pub fn new<C>(control: Arc<C>) -> Self
	where
		C: LocalTtsControl,
	{
		let effects = Arc::new(ApplicationAudioEffects { control });
		Self { domain: AudioCoordinator::new(effects) }
	}

	/// Borrow the domain coordinator used by STT, live voice, and vocalization
	/// controllers to acquire their leases.
	pub fn domain(&self) -> &AudioCoordinator {
		&self.domain
	}
}
