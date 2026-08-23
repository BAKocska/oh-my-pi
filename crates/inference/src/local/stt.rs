//! Whisper.cpp-backed local speech recognition.

use std::{
	ffi,
	path::PathBuf,
	ptr,
	str::FromStr as _,
	sync::{Arc, LazyLock},
	time::{Duration, Instant},
};

use omp_core::Str;
use parking_lot::Mutex;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{
	artifact::ArtifactStore,
	runtime::{
		LocalCancellation, LocalError, LocalErrorKind, LocalExecutionReceipt, LocalResult,
		LocalRuntime, MemoryPool,
	},
	sherpa,
	sherpa::{SherpaAdapter, SherpaConfig},
	speech_catalog::{DEFAULT_STT_PRESET, SpeechArtifactManifests, SttPreset},
};

const SAMPLE_RATE: usize = 16_000;
const WHISPER_WINDOW_SAMPLES: usize = 30 * SAMPLE_RATE;
const WHISPER_STRIDE_SAMPLES: usize = 5 * SAMPLE_RATE;
/// Process-wide serialization shared by Whisper and Parakeet recognizers.
pub(super) static STT_INFERENCE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Configuration for a verified whisper.cpp checkpoint.
#[derive(Clone, Debug)]
pub struct WhisperConfig {
	/// Path to a ggml Whisper checkpoint.
	pub model_path:      PathBuf,
	/// CPU worker count used by decoding.
	pub threads:         usize,
	/// Whether whisper.cpp may use its compiled GPU backend.
	pub use_gpu:         bool,
	/// Whether fused attention is enabled.
	pub flash_attention: bool,
	/// Estimated resident bytes charged before loading.
	pub resident_bytes:  usize,
	/// Admission limit; currently must be one because Whisper access is
	/// serialized.
	pub max_concurrency: usize,
	/// Explicit idle-unload interval.
	pub idle_timeout:    Duration,
}

impl WhisperConfig {
	/// Verifies and binds one Whisper preset from the canonical speech manifest.
	pub fn from_verified_artifacts(
		store: &ArtifactStore,
		artifacts: &SpeechArtifactManifests,
		preset: SttPreset,
		threads: usize,
		use_gpu: bool,
		flash_attention: bool,
		idle_timeout: Duration,
		cancel: &LocalCancellation,
	) -> LocalResult<Self> {
		if preset == SttPreset::Parakeet {
			return Err(LocalError::new(
				LocalErrorKind::InvalidInput,
				"Parakeet requires the sherpa-onnx adapter",
			));
		}
		let mut paths = artifacts.verified_stt_paths(store, preset, cancel)?;
		let model_path = paths.pop().ok_or_else(|| {
			LocalError::new(LocalErrorKind::Artifact, "Whisper manifest contains no checkpoint")
		})?;
		let resident_bytes = usize::try_from(
			artifacts
				.stt_manifest(preset)
				.total_bytes()
				.map_err(|_| LocalError::new(LocalErrorKind::Artifact, "invalid Whisper manifest"))?,
		)
		.map_err(|_| {
			LocalError::new(LocalErrorKind::Overloaded, "Whisper checkpoint exceeds address space")
		})?;
		Ok(Self {
			model_path,
			threads,
			use_gpu,
			flash_attention,
			resident_bytes,
			max_concurrency: 1,
			idle_timeout,
		})
	}
}

/// Resolves persisted preset ids, falling back to Parakeet for stale values.
pub fn resolve_stt_preset(id: Option<&str>) -> SttPreset {
	id.and_then(|id| SttPreset::from_str(id).ok())
		.unwrap_or(DEFAULT_STT_PRESET)
}

/// Controls one transcription.
#[derive(Clone, Debug, Default)]
pub struct TranscriptionOptions {
	/// Optional ISO-639-1 language code; absent enables detection.
	pub language:       Option<Str>,
	/// Translate recognized speech to English.
	pub translate:      bool,
	/// Include segment timestamps.
	pub timestamps:     bool,
	/// Optional initial decoder prompt.
	pub initial_prompt: Option<Str>,
	/// Sampling temperature in `[0, 1]`.
	pub temperature:    Option<f32>,
}

/// One timestamped transcription segment.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptionSegment {
	/// Recognized text.
	pub text:                  Str,
	/// Start offset from the audio beginning.
	pub start:                 Duration,
	/// End offset from the audio beginning.
	pub end:                   Duration,
	/// Model probability that the interval contains no speech.
	pub no_speech_probability: f32,
}

/// Complete transcription and local execution evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcription {
	/// Concatenated recognized text.
	pub text:     Str,
	/// Timestamped segments, empty when timestamps were disabled.
	pub segments: Vec<TranscriptionSegment>,
	/// Detected or requested language.
	pub language: Option<Str>,
	/// Local runtime receipt.
	pub receipt:  LocalExecutionReceipt,
}

/// Shared lifecycle controls for all four STT presets.
#[derive(Clone, Copy, Debug)]
pub struct SttRuntimeOptions {
	/// CPU inference threads.
	pub threads:         usize,
	/// Whether Whisper may use its compiled GPU backend.
	pub whisper_gpu:     bool,
	/// Whether Whisper fused attention is enabled.
	pub flash_attention: bool,
	/// Idle interval before unloading a recognizer.
	pub idle_timeout:    Duration,
}

/// Concrete adapter selected from the stable four-preset catalog.
#[derive(Clone)]
pub enum SpeechToTextAdapter {
	/// A Whisper fast, balanced, or turbo checkpoint.
	Whisper {
		/// Resolved stable preset.
		preset:  SttPreset,
		/// Native whisper.cpp adapter.
		adapter: WhisperAdapter,
	},
	/// Default Parakeet TDT recognizer.
	Parakeet(SherpaAdapter),
}

impl SpeechToTextAdapter {
	/// Resolves stale persisted ids, verifies that preset's manifest, and
	/// constructs the matching native adapter.
	pub fn from_verified_artifacts(
		store: &ArtifactStore,
		artifacts: &SpeechArtifactManifests,
		selected_id: Option<&str>,
		options: SttRuntimeOptions,
		memory: Arc<MemoryPool>,
		cancel: &LocalCancellation,
	) -> LocalResult<Self> {
		let preset = resolve_stt_preset(selected_id);
		match preset {
			SttPreset::Parakeet => {
				let evidence = sherpa::availability();
				if !evidence.available {
					return Err(LocalError::new(LocalErrorKind::Unsupported, evidence.detail));
				}
				let config = SherpaConfig::from_verified_artifacts(
					store,
					artifacts,
					options.threads,
					options.idle_timeout,
					cancel,
				)?;
				Ok(Self::Parakeet(SherpaAdapter::new(config, memory)?))
			},
			SttPreset::Fast | SttPreset::Balanced | SttPreset::Turbo => {
				let config = WhisperConfig::from_verified_artifacts(
					store,
					artifacts,
					preset,
					options.threads,
					options.whisper_gpu,
					options.flash_attention,
					options.idle_timeout,
					cancel,
				)?;
				Ok(Self::Whisper { preset, adapter: WhisperAdapter::new(config, memory)? })
			},
		}
	}

	/// Returns the resolved preset, including stale-id fallback.
	pub const fn preset(&self) -> SttPreset {
		match self {
			Self::Whisper { preset, .. } => *preset,
			Self::Parakeet(_) => SttPreset::Parakeet,
		}
	}

	/// Transcribes mono 16 kHz audio with the selected concrete engine.
	pub fn transcribe_mono_16khz(
		&self,
		samples: &[f32],
		options: &TranscriptionOptions,
		cancel: &LocalCancellation,
	) -> LocalResult<Transcription> {
		match self {
			Self::Whisper { adapter, .. } => adapter.transcribe_mono_16khz(samples, options, cancel),
			Self::Parakeet(adapter) => adapter.transcribe_mono_16khz(samples, options, cancel),
		}
	}

	/// Prewarms the resolved concrete recognizer.
	pub fn prewarm(&self, cancel: &LocalCancellation) -> LocalResult<LocalExecutionReceipt> {
		match self {
			Self::Whisper { adapter, .. } => adapter.prewarm(cancel),
			Self::Parakeet(adapter) => adapter.prewarm(cancel),
		}
	}

	/// Unloads the selected engine after its configured idle interval.
	pub fn unload_if_idle(&self, now: Instant) -> bool {
		match self {
			Self::Whisper { adapter, .. } => adapter.unload_if_idle(now),
			Self::Parakeet(adapter) => adapter.unload_if_idle(now),
		}
	}
}

struct WhisperEngine {
	context: WhisperContext,
	threads: usize,
}

/// Lazy, bounded adapter over whisper.cpp.
#[derive(Clone)]
pub struct WhisperAdapter {
	runtime: LocalRuntime<WhisperEngine>,
}

unsafe extern "C" fn whisper_abort(user_data: *mut ffi::c_void) -> bool {
	if user_data.is_null() {
		return false;
	}
	// SAFETY: transcribe keeps this cancellation token alive until whisper.cpp
	// returns.
	let cancel = unsafe { &*user_data.cast::<LocalCancellation>() };
	cancel.is_cancelled()
}

impl WhisperAdapter {
	/// Creates a lazy adapter for a local checkpoint.
	pub fn new(config: WhisperConfig, memory: Arc<MemoryPool>) -> LocalResult<Self> {
		if config.threads == 0 {
			return Err(LocalError::new(
				LocalErrorKind::InvalidInput,
				"Whisper thread count must be non-zero",
			));
		}
		let resident_bytes = config.resident_bytes;
		let concurrency = config.max_concurrency;
		let idle = config.idle_timeout;
		let runtime = LocalRuntime::new(
			move || {
				whisper_rs::install_logging_hooks();
				let mut parameters = WhisperContextParameters::new();
				parameters.use_gpu(config.use_gpu);
				parameters.gpu_device(0);
				parameters.flash_attn(config.flash_attention);
				let context = WhisperContext::new_with_params(&config.model_path, parameters).map_err(
					|error| {
						LocalError::new(LocalErrorKind::Backend, format!("Whisper load failed: {error}"))
					},
				)?;
				Ok(WhisperEngine { context, threads: config.threads })
			},
			memory,
			resident_bytes,
			concurrency,
			idle,
		)?;
		Ok(Self { runtime })
	}

	/// Transcribes mono 16 kHz floating-point PCM using whisper.cpp.
	pub fn transcribe_mono_16khz(
		&self,
		samples: &[f32],
		options: &TranscriptionOptions,
		cancel: &LocalCancellation,
	) -> LocalResult<Transcription> {
		if samples.is_empty() {
			return Err(LocalError::new(
				LocalErrorKind::InvalidInput,
				"transcription requires audio samples",
			));
		}
		if options
			.temperature
			.is_some_and(|temperature| !temperature.is_finite() || !(0.0..=1.0).contains(&temperature))
		{
			return Err(LocalError::new(
				LocalErrorKind::InvalidInput,
				"transcription temperature must be in [0, 1]",
			));
		}
		let _serialized = STT_INFERENCE_LOCK.lock();
		let lease = self.runtime.acquire(cancel)?;
		let receipt = lease.receipt();
		let (text, segments, language) =
			lease.with_engine(|engine| transcribe_long_form(engine, samples, options, cancel))?;
		Ok(Transcription { text, segments, language, receipt })
	}

	/// Loads and validates the Whisper checkpoint ahead of first capture.
	pub fn prewarm(&self, cancel: &LocalCancellation) -> LocalResult<LocalExecutionReceipt> {
		self.runtime.prewarm(cancel)
	}

	/// Returns the blacklisted first-load failure, if loading has failed.
	pub fn load_failure(&self) -> Option<LocalError> {
		self.runtime.load_failure()
	}

	/// Clears the failure blacklist after explicit artifact/config repair.
	pub fn clear_load_failure(&self) -> bool {
		self.runtime.clear_load_failure()
	}

	/// Unloads the checkpoint when inactive for its configured interval.
	pub fn unload_if_idle(&self, now: Instant) -> bool {
		self.runtime.unload_if_idle(now)
	}

	/// Returns whether the Whisper checkpoint is resident.
	pub fn is_loaded(&self) -> bool {
		self.runtime.is_loaded()
	}
}

fn transcribe_long_form(
	engine: &WhisperEngine,
	samples: &[f32],
	options: &TranscriptionOptions,
	cancel: &LocalCancellation,
) -> LocalResult<(Str, Vec<TranscriptionSegment>, Option<Str>)> {
	let mut text = String::new();
	let mut segments = Vec::new();
	let mut language = None;
	let mut start = 0_usize;
	while start < samples.len() {
		if cancel.is_cancelled() {
			return Err(LocalError::cancelled());
		}
		let end = start
			.saturating_add(WHISPER_WINDOW_SAMPLES)
			.min(samples.len());
		let skip = if start == 0 {
			Duration::ZERO
		} else {
			Duration::from_secs(5)
		};
		let (chunk_segments, detected) =
			transcribe_window(engine, &samples[start..end], options, cancel)?;
		if language.is_none() {
			language = detected;
		}
		let offset = Duration::from_secs_f64(start as f64 / SAMPLE_RATE as f64);
		for mut segment in chunk_segments {
			if segment.end <= skip {
				continue;
			}
			text.push_str(segment.text.as_str());
			if options.timestamps {
				segment.start = offset.saturating_add(segment.start);
				segment.end = offset.saturating_add(segment.end);
				segments.push(segment);
			}
		}
		if end == samples.len() {
			break;
		}
		start = start.saturating_add(WHISPER_WINDOW_SAMPLES - WHISPER_STRIDE_SAMPLES);
	}
	Ok((Str::new(text.trim()), segments, language))
}

fn transcribe_window(
	engine: &WhisperEngine,
	samples: &[f32],
	options: &TranscriptionOptions,
	cancel: &LocalCancellation,
) -> LocalResult<(Vec<TranscriptionSegment>, Option<Str>)> {
	let mut state = engine.context.create_state().map_err(|error| {
		LocalError::new(LocalErrorKind::Backend, format!("Whisper state failed: {error}"))
	})?;
	let mut parameters = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
	parameters.set_n_threads(engine.threads.min(i32::MAX as usize) as i32);
	parameters.set_translate(options.translate);
	parameters.set_no_timestamps(false);
	parameters.set_print_progress(false);
	parameters.set_print_realtime(false);
	parameters.set_print_timestamps(false);
	parameters.set_language(options.language.as_ref().map(Str::as_str));
	if let Some(temperature) = options.temperature {
		parameters.set_temperature(temperature);
	}
	if let Some(prompt) = options.initial_prompt.as_ref() {
		parameters.set_initial_prompt(prompt.as_str());
	}
	// SAFETY: whisper.cpp invokes the callback only during `full`; `cancel`
	// remains at a stable address for that synchronous call.
	unsafe {
		parameters.set_abort_callback(Some(whisper_abort));
		parameters.set_abort_callback_user_data(ptr::from_ref(cancel).cast_mut().cast());
	}
	state.full(parameters, samples).map_err(|error| {
		LocalError::new(LocalErrorKind::Backend, format!("Whisper inference failed: {error}"))
	})?;
	if cancel.is_cancelled() {
		return Err(LocalError::cancelled());
	}
	let mut segments = Vec::with_capacity(state.full_n_segments().max(0) as usize);
	for segment in state.as_iter() {
		let segment_text = segment.to_str().map_err(|error| {
			LocalError::new(LocalErrorKind::Backend, format!("Whisper text failed: {error}"))
		})?;
		segments.push(TranscriptionSegment {
			text:                  Str::new(segment_text),
			start:                 whisper_timestamp(segment.start_timestamp()),
			end:                   whisper_timestamp(segment.end_timestamp()),
			no_speech_probability: segment.no_speech_probability(),
		});
	}
	let language = whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(Str::new);
	Ok((segments, language))
}

fn whisper_timestamp(timestamp: i64) -> Duration {
	Duration::from_millis(timestamp.max(0) as u64 * 10)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stale_presets_fall_back_to_parakeet() {
		assert_eq!(resolve_stt_preset(None), SttPreset::Parakeet);
		assert_eq!(resolve_stt_preset(Some("removed-model")), SttPreset::Parakeet);
		assert_eq!(resolve_stt_preset(Some("fast")), SttPreset::Fast);
	}

	#[test]
	fn whisper_long_form_window_is_thirty_seconds_with_five_second_stride() {
		assert_eq!(WHISPER_WINDOW_SAMPLES, 480_000);
		assert_eq!(WHISPER_STRIDE_SAMPLES, 80_000);
		assert_eq!(WHISPER_WINDOW_SAMPLES - WHISPER_STRIDE_SAMPLES, 400_000);
	}
}
