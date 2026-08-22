//! In-process sherpa-onnx Parakeet offline transcription.

use std::{path::PathBuf, sync::Arc, time::Duration};

use omp_core::Str;
#[cfg(any(
	all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "windows", target_arch = "x86_64")
))]
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

#[cfg(any(
	all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "windows", target_arch = "x86_64")
))]
use super::runtime::LocalRuntime;
use super::{
	artifact::ArtifactStore,
	runtime::{
		AvailabilityEvidence, LocalCancellation, LocalError, LocalErrorKind, LocalResult, MemoryPool,
	},
	speech_catalog::{SpeechArtifactManifests, SttPreset},
	stt::{Transcription, TranscriptionOptions},
};

/// Verified files and lifecycle controls for Parakeet TDT.
#[derive(Clone, Debug)]
pub struct SherpaConfig {
	/// Quantized encoder path.
	pub encoder_path:    PathBuf,
	/// Quantized decoder path.
	pub decoder_path:    PathBuf,
	/// Quantized joiner path.
	pub joiner_path:     PathBuf,
	/// Token vocabulary path.
	pub tokens_path:     PathBuf,
	/// CPU worker count.
	pub threads:         usize,
	/// Estimated resident bytes charged before loading.
	pub resident_bytes:  usize,
	/// Admission limit; Parakeet inference is serialized and requires one.
	pub max_concurrency: usize,
	/// Explicit idle-unload interval.
	pub idle_timeout:    Duration,
}

impl SherpaConfig {
	/// Verifies and binds the canonical Parakeet artifact manifest.
	pub fn from_verified_artifacts(
		store: &ArtifactStore,
		artifacts: &SpeechArtifactManifests,
		threads: usize,
		idle_timeout: Duration,
		cancel: &LocalCancellation,
	) -> LocalResult<Self> {
		let paths = artifacts.verified_stt_paths(store, SttPreset::Parakeet, cancel)?;
		let resident_bytes = usize::try_from(
			artifacts
				.stt_manifest(SttPreset::Parakeet)
				.total_bytes()
				.map_err(|_| LocalError::new(LocalErrorKind::Artifact, "invalid Parakeet manifest"))?,
		)
		.map_err(|_| {
			LocalError::new(LocalErrorKind::Overloaded, "Parakeet artifacts exceed address space")
		})?;
		Ok(Self {
			encoder_path: required_path(&paths, "encoder.int8.onnx")?,
			decoder_path: required_path(&paths, "decoder.int8.onnx")?,
			joiner_path: required_path(&paths, "joiner.int8.onnx")?,
			tokens_path: required_path(&paths, "tokens.txt")?,
			threads,
			resident_bytes,
			max_concurrency: 1,
			idle_timeout,
		})
	}
}

/// Returns typed target support before a caller attempts artifact acquisition.
pub fn availability() -> AvailabilityEvidence {
	#[cfg(any(
		all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
		all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
		all(target_os = "windows", target_arch = "x86_64")
	))]
	{
		AvailabilityEvidence::available("sherpa-onnx has a packaged native runtime for this target")
	}
	#[cfg(not(any(
		all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
		all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
		all(target_os = "windows", target_arch = "x86_64")
	)))]
	{
		AvailabilityEvidence::unavailable(
			"unsupported_sherpa_target",
			"Parakeet requires Linux x86_64/aarch64, macOS x86_64/aarch64, or Windows x86_64",
		)
	}
}

#[cfg(any(
	all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "windows", target_arch = "x86_64")
))]
struct SherpaEngine {
	recognizer: OfflineRecognizer,
}

/// Lazy, serialized Parakeet adapter.
#[derive(Clone)]
pub struct SherpaAdapter {
	#[cfg(any(
		all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
		all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
		all(target_os = "windows", target_arch = "x86_64")
	))]
	runtime: LocalRuntime<SherpaEngine>,
}

impl SherpaAdapter {
	/// Creates a lazy in-process NeMo transducer recognizer.
	pub fn new(config: SherpaConfig, memory: Arc<MemoryPool>) -> LocalResult<Self> {
		if config.threads == 0 {
			return Err(LocalError::new(
				LocalErrorKind::InvalidInput,
				"Parakeet thread count must be non-zero",
			));
		}
		#[cfg(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		))]
		{
			let resident_bytes = config.resident_bytes;
			let concurrency = config.max_concurrency;
			let idle_timeout = config.idle_timeout;
			let runtime = LocalRuntime::new(
				move || {
					let mut recognizer_config = OfflineRecognizerConfig::default();
					recognizer_config.model_config.transducer = OfflineTransducerModelConfig {
						encoder: Some(path_string(&config.encoder_path)?),
						decoder: Some(path_string(&config.decoder_path)?),
						joiner:  Some(path_string(&config.joiner_path)?),
					};
					recognizer_config.model_config.tokens = Some(path_string(&config.tokens_path)?);
					recognizer_config.model_config.provider = Some("cpu".to_owned());
					recognizer_config.model_config.model_type = Some("nemo_transducer".to_owned());
					recognizer_config.model_config.num_threads =
						config.threads.min(i32::MAX as usize) as i32;
					recognizer_config.decoding_method = Some("greedy_search".to_owned());
					let recognizer = OfflineRecognizer::create(&recognizer_config).ok_or_else(|| {
						LocalError::new(
							LocalErrorKind::Backend,
							"sherpa-onnx failed to load Parakeet; verify the packaged native runtime and \
							 model artifacts",
						)
					})?;
					Ok(SherpaEngine { recognizer })
				},
				memory,
				resident_bytes,
				concurrency,
				idle_timeout,
			)?;
			Ok(Self { runtime })
		}
		#[cfg(not(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		)))]
		{
			let _ = (config, memory);
			Err(LocalError::new(LocalErrorKind::Unsupported, availability().detail))
		}
	}

	/// Transcribes mono 16 kHz floating-point PCM through Parakeet.
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
		#[cfg(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		))]
		{
			let _serialized = super::stt::STT_INFERENCE_LOCK.lock();
			let lease = self.runtime.acquire(cancel)?;
			let receipt = lease.receipt();
			let text = lease.with_engine(|engine| {
				if cancel.is_cancelled() {
					return Err(LocalError::cancelled());
				}
				let stream = engine.recognizer.create_stream();
				stream.accept_waveform(16_000, samples);
				engine.recognizer.decode(&stream);
				if cancel.is_cancelled() {
					return Err(LocalError::cancelled());
				}
				let result = stream.get_result().ok_or_else(|| {
					LocalError::new(LocalErrorKind::Backend, "Parakeet produced no result")
				})?;
				Ok(Str::new(result.text.trim()))
			})?;
			Ok(Transcription {
				text,
				segments: Vec::new(),
				language: options.language.clone(),
				receipt,
			})
		}
		#[cfg(not(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		)))]
		{
			let _ = (samples, options, cancel);
			Err(LocalError::new(LocalErrorKind::Unsupported, availability().detail))
		}
	}

	/// Loads and validates Parakeet ahead of first capture.
	pub fn prewarm(
		&self,
		cancel: &LocalCancellation,
	) -> LocalResult<super::runtime::LocalExecutionReceipt> {
		#[cfg(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		))]
		{
			self.runtime.prewarm(cancel)
		}
		#[cfg(not(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		)))]
		{
			let _ = cancel;
			Err(LocalError::new(LocalErrorKind::Unsupported, availability().detail))
		}
	}

	/// Unloads Parakeet after its configured idle interval.
	pub fn unload_if_idle(&self, now: std::time::Instant) -> bool {
		#[cfg(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		))]
		{
			self.runtime.unload_if_idle(now)
		}
		#[cfg(not(any(
			all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
			all(target_os = "windows", target_arch = "x86_64")
		)))]
		{
			let _ = now;
			false
		}
	}
}

fn required_path(paths: &[PathBuf], filename: &str) -> LocalResult<PathBuf> {
	paths
		.iter()
		.find(|path| {
			path
				.file_name()
				.is_some_and(|candidate| candidate == filename)
		})
		.cloned()
		.ok_or_else(|| {
			LocalError::new(LocalErrorKind::Artifact, "Parakeet manifest is missing a runtime file")
		})
}

#[cfg(any(
	all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
	all(target_os = "windows", target_arch = "x86_64")
))]
fn path_string(path: &std::path::Path) -> LocalResult<String> {
	path.to_str().map(str::to_owned).ok_or_else(|| {
		LocalError::new(LocalErrorKind::Artifact, "Parakeet artifact path is not UTF-8")
	})
}
