//! Direct default-device audio backends.
//!
//! Backends invoke callbacks on their own realtime threads, queue no more than
//! three playback periods in the operating system, and guarantee that an
//! externally initiated `stop` waits out any in-flight callback.

#[cfg(all(feature = "native-audio", target_os = "macos"))]
#[path = "device/coreaudio.rs"]
mod coreaudio;
#[cfg(all(feature = "native-audio", target_os = "macos"))]
use coreaudio as imp;

#[cfg(all(feature = "native-audio", target_os = "windows"))]
#[path = "device/wasapi.rs"]
mod wasapi;
#[cfg(all(feature = "native-audio", target_os = "windows"))]
use wasapi as imp;

#[cfg(all(feature = "native-audio", target_os = "linux"))]
#[path = "device/linux.rs"]
mod linux;
#[cfg(all(feature = "native-audio", target_os = "linux"))]
use linux as imp;

#[cfg(not(all(
	feature = "native-audio",
	any(target_os = "macos", target_os = "windows", target_os = "linux")
)))]
mod unsupported {
	use super::{CaptureSink, DeviceConfig, PlaybackFill};
	use crate::{VoiceError, VoiceResult};

	pub(super) struct PlaybackDevice;

	impl PlaybackDevice {
		pub(super) fn start(config: DeviceConfig, _fill: PlaybackFill) -> VoiceResult<Self> {
			let _ = config.period_samples();

			Err(VoiceError::UnsupportedPlatform { platform: std::env::consts::OS })
		}

		pub(super) fn stop(&mut self) -> VoiceResult<()> {
			Ok(())
		}
	}

	pub(super) struct CaptureDevice;

	impl CaptureDevice {
		pub(super) fn start(config: DeviceConfig, _sink: CaptureSink) -> VoiceResult<Self> {
			let _ = config.period_samples();

			Err(VoiceError::UnsupportedPlatform { platform: std::env::consts::OS })
		}

		pub(super) fn stop(&mut self) -> VoiceResult<()> {
			Ok(())
		}
	}
}
#[cfg(not(all(
	feature = "native-audio",
	any(target_os = "macos", target_os = "windows", target_os = "linux")
)))]
use unsupported as imp;

#[cfg(feature = "native-audio")]
use crate::VoiceError;
use crate::VoiceResult;

#[cfg(all(
	feature = "native-audio",
	any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
pub(super) type BackendResult<T> = std::result::Result<T, String>;

pub(super) type PlaybackFill = Box<dyn FnMut(&mut [f32]) + Send + 'static>;
pub(super) type CaptureSink = Box<dyn FnMut(&[f32]) + Send + 'static>;

#[derive(Clone, Copy)]
pub(super) struct DeviceConfig {
	pub(super) sample_rate: u32,
	pub(super) period_ms:   u32,
}

impl DeviceConfig {
	pub(super) fn period_samples(self) -> usize {
		((self.sample_rate as usize * self.period_ms as usize) / 1000).max(1)
	}
}

pub(super) struct PlaybackDevice {
	inner: imp::PlaybackDevice,
}

impl PlaybackDevice {
	pub(super) fn start(config: DeviceConfig, fill: PlaybackFill) -> VoiceResult<Self> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		let inner = imp::PlaybackDevice::start(config, fill).map_err(VoiceError::backend)?;
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		let inner = imp::PlaybackDevice::start(config, fill)?;
		Ok(Self { inner })
	}

	pub(super) fn stop(&mut self) -> VoiceResult<()> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		return self.inner.stop().map_err(VoiceError::backend);
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		self.inner.stop()
	}
}

pub(super) struct CaptureDevice {
	inner: imp::CaptureDevice,
}

impl CaptureDevice {
	pub(super) fn start(config: DeviceConfig, sink: CaptureSink) -> VoiceResult<Self> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		let inner = imp::CaptureDevice::start(config, sink).map_err(VoiceError::backend)?;
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		let inner = imp::CaptureDevice::start(config, sink)?;
		Ok(Self { inner })
	}

	pub(super) fn stop(&mut self) -> VoiceResult<()> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		return self.inner.stop().map_err(VoiceError::backend);
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		self.inner.stop()
	}
}
