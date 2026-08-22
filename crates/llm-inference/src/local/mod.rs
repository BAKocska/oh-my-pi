//! In-process inference with shared bounded lifecycle and verified artifacts.

/// Apple Foundation Models dynamic runtime.
#[cfg(feature = "local-applefm")]
pub mod applefm;
/// Verified, root-confined model artifacts.
pub mod artifact;
/// FastEmbed local embeddings.
#[cfg(feature = "local-embedding")]
pub mod embedding;
/// Shared admission, memory, cancellation, and idle-unload lifecycle.
pub mod runtime;
/// sherpa-onnx Parakeet speech recognition.
#[cfg(feature = "local-stt")]
pub mod sherpa;
/// Backend-neutral speech catalog and artifact-backed cache snapshots.
pub mod speech_catalog;
/// Whisper.cpp speech recognition.
#[cfg(feature = "local-stt")]
pub mod stt;
/// llama.cpp GGUF text generation.
#[cfg(feature = "local-text")]
pub mod text;
/// Curated GGUF title, memory, and classifier artifacts.
pub mod tiny_catalog;
/// Kokoro-82M speech synthesis.
#[cfg(feature = "local-tts")]
pub mod tts;

pub use artifact::{
	ArtifactCacheState, ArtifactCacheStatus, ArtifactError, ArtifactFetchRequest,
	ArtifactFetchResponse, ArtifactFetcher, ArtifactIoOperation, ArtifactManifest,
	ArtifactManifestReceipt, ArtifactProgress, ArtifactReceipt, ArtifactResult, ArtifactShard,
	ArtifactSpec, ArtifactStore, SystemArtifactBody, SystemArtifactFetcher, VerifiedArtifact,
	sha256_digest,
};
pub use runtime::{
	AdmissionControl, AvailabilityEvidence, LocalCancellation, LocalError, LocalErrorKind,
	LocalExecutionReceipt, LocalResult, LocalRuntime, MemoryPool, MemoryReservation, RuntimeLease,
};
pub use speech_catalog::{
	ArtifactDownloadCapabilities, DEFAULT_KOKORO_VOICE, DEFAULT_LIVE_VOICE, DEFAULT_STT_PRESET,
	DEFAULT_TTS_MODEL, DEFAULT_TTS_PROVIDER, DEFAULT_XAI_VOICE, KOKORO_VOICES, KokoroVoice,
	LIVE_VOICE_SETTING, LIVE_VOICES, LiveSpeechCatalog, LiveVoice, SPEECH_VOICE_SETTING,
	STT_MODEL_SETTING, STT_PRESETS, SpeechArtifactManifests, SpeechCatalog, SpeechCatalogError,
	SpeechCatalogSnapshot, SpeechDefaults, SpeechSettingKeys, SpeechToTextCatalog,
	SpeechToTextModelOption, SpeechVoiceOption, SttPreset, TTS_MODEL_SETTING, TTS_PROVIDER_SETTING,
	TTS_VOICE_SETTING, TextToSpeechCatalog, TextToSpeechModelOption, XAI_VOICES, XaiSpeechCatalog,
	XaiVoice,
};
pub use tiny_catalog::{
	CLASSIFIER_MODELS, DEFAULT_MEMORY_LOCAL_MODEL, DEFAULT_TITLE_LOCAL_MODEL, MEMORY_MODEL_SETTING,
	MEMORY_MODELS, ONLINE_TINY_MODEL, TINY_MODEL_SETTING, TITLE_MODELS, TinyArtifact,
	TinyBlockedEvidence, TinyModelSpec, TinyWorkload, model as tiny_model, models as tiny_models,
};
