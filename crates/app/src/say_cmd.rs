//! Standalone Kokoro sentence synthesis with speaker or atomic WAV output.

use std::{path::Path, sync::Arc, time::Duration};

use miette::{IntoDiagnostic as _, miette};
use omp_llm_inference::local::{
	ArtifactStore, LocalCancellation, MemoryPool,
	speech_catalog::{DEFAULT_KOKORO_VOICE, SpeechArtifactManifests},
	tts::{KokoroAdapter, KokoroConfig, KokoroDevice, SynthesisOptions},
};

use crate::cli::SayArgs;

/// Synthesizes text with one verified Kokoro model/voice.
pub async fn run(args: SayArgs) -> miette::Result<()> {
	if !args.speed.is_finite() || args.speed <= 0.0 {
		return Err(miette!("--speed must be a finite positive number"));
	}
	let data_dir = crate::cli::data_dir(args.data_dir)?;
	let root = data_dir.join("models");
	std::fs::create_dir_all(&root).into_diagnostic()?;
	let store = ArtifactStore::open(&root).into_diagnostic()?;
	let artifacts = SpeechArtifactManifests::pi_parity().into_diagnostic()?;
	let cancel = LocalCancellation::new();
	let config = KokoroConfig::from_verified_artifacts(
		&store,
		&artifacts,
		device(),
		Duration::from_secs(60),
		&cancel,
	)
	.into_diagnostic()?;
	let memory = Arc::new(MemoryPool::new(config.resident_bytes));
	let adapter = KokoroAdapter::new(config, memory).into_diagnostic()?;
	let voice = args
		.voice
		.unwrap_or_else(|| DEFAULT_KOKORO_VOICE.to_string());
	let options = SynthesisOptions {
		speed:           args.speed,
		max_chunk_chars: args.max_chunk_chars,
		deterministic:   args.deterministic,
	};
	if let Some(path) = args.output {
		let output = adapter
			.synthesize(args.text.as_str(), &voice, options, &cancel)
			.into_diagnostic()?;
		write_wav_atomic(&path, output.sample_rate, &output.samples)?;
		println!("wrote {} samples to {}", output.samples.len(), path.display());
		return Ok(());
	}
	let mut playback = omp_voice::audio::PlaybackStream::start(24_000).into_diagnostic()?;
	let writer = playback.writer().into_diagnostic()?;
	let mut playback_error = None;
	let receipt = adapter
		.synthesize_streaming(args.text.as_str(), &voice, options, &cancel, |chunk, _| {
			match writer.write(chunk) {
				Ok(()) => true,
				Err(error) => {
					playback_error = Some(error);
					false
				},
			}
		})
		.into_diagnostic()?;
	if let Some(error) = playback_error {
		return Err(error).into_diagnostic();
	}
	playback.drain().await.into_diagnostic()?;
	println!("played {} samples in {} chunk(s)", receipt.samples, receipt.chunks);
	Ok(())
}

const fn device() -> KokoroDevice {
	#[cfg(target_os = "macos")]
	{
		KokoroDevice::Metal
	}
	#[cfg(not(target_os = "macos"))]
	{
		KokoroDevice::Cpu
	}
}

fn write_wav_atomic(path: &Path, sample_rate: u32, samples: &[f32]) -> miette::Result<()> {
	let sample_bytes = samples
		.len()
		.checked_mul(2)
		.and_then(|bytes| u32::try_from(bytes).ok())
		.ok_or_else(|| miette!("WAV output exceeds RIFF size limits"))?;
	let riff_bytes = sample_bytes
		.checked_add(36)
		.ok_or_else(|| miette!("WAV output exceeds RIFF size limits"))?;
	let mut wav = Vec::with_capacity(sample_bytes as usize + 44);
	wav.extend_from_slice(b"RIFF");
	wav.extend_from_slice(&riff_bytes.to_le_bytes());
	wav.extend_from_slice(b"WAVEfmt ");
	wav.extend_from_slice(&16_u32.to_le_bytes());
	wav.extend_from_slice(&1_u16.to_le_bytes());
	wav.extend_from_slice(&1_u16.to_le_bytes());
	wav.extend_from_slice(&sample_rate.to_le_bytes());
	wav.extend_from_slice(&sample_rate.saturating_mul(2).to_le_bytes());
	wav.extend_from_slice(&2_u16.to_le_bytes());
	wav.extend_from_slice(&16_u16.to_le_bytes());
	wav.extend_from_slice(b"data");
	wav.extend_from_slice(&sample_bytes.to_le_bytes());
	for sample in samples {
		let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
		wav.extend_from_slice(&pcm.to_le_bytes());
	}
	if let Some(parent) = path
		.parent()
		.filter(|parent| !parent.as_os_str().is_empty())
	{
		std::fs::create_dir_all(parent).into_diagnostic()?;
	}
	let temporary = path.with_extension(format!("wav.tmp-{}", std::process::id()));
	std::fs::write(&temporary, wav).into_diagnostic()?;
	std::fs::rename(&temporary, path).into_diagnostic()?;
	Ok(())
}
