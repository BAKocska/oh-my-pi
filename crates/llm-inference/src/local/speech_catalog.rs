//! Backend-neutral speech catalog and artifact-backed cache snapshots.

use std::collections::HashSet;

use omp_core::Str;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use super::{
	artifact::{ArtifactCacheState, ArtifactManifest, ArtifactResult, ArtifactStore},
	runtime::LocalCancellation,
};

/// Stable setting key for the selected speech-to-text preset.
pub const STT_MODEL_SETTING: &str = "stt.modelName";
/// Stable setting key for the selected local text-to-speech model.
pub const TTS_MODEL_SETTING: &str = "tts.localModel";
/// Stable setting key for the local text-to-speech voice.
pub const TTS_VOICE_SETTING: &str = "tts.localVoice";
/// Stable setting key for assistant-output vocalization voice.
pub const SPEECH_VOICE_SETTING: &str = "speech.voice";
/// Stable setting key for realtime voice.
pub const LIVE_VOICE_SETTING: &str = "live.voice";
/// Stable setting key for local/cloud text-to-speech routing.
pub const TTS_PROVIDER_SETTING: &str = "providers.tts";

/// Stable default speech-to-text preset.
pub const DEFAULT_STT_PRESET: SttPreset = SttPreset::Parakeet;
/// Stable default local text-to-speech model.
pub const DEFAULT_TTS_MODEL: &str = "kokoro";
/// Stable default Kokoro voice.
pub const DEFAULT_KOKORO_VOICE: KokoroVoice = KokoroVoice::AfHeart;
/// Stable default realtime voice.
pub const DEFAULT_LIVE_VOICE: LiveVoice = LiveVoice::Sol;
/// Stable default xAI Grok Voice built-in voice.
pub const DEFAULT_XAI_VOICE: XaiVoice = XaiVoice::Eve;
/// Stable default text-to-speech provider routing policy.
pub const DEFAULT_TTS_PROVIDER: &str = "auto";

/// Stable local speech-to-text preset id.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SttPreset {
	/// Whisper base multilingual preset.
	Fast,
	/// Whisper small multilingual preset.
	Balanced,
	/// Whisper large-v3-turbo multilingual preset.
	Turbo,
	/// NVIDIA Parakeet TDT 0.6B v3 preset.
	Parakeet,
}

/// Stable curated Kokoro voice id.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum KokoroVoice {
	/// Heart, American female.
	AfHeart,
	/// Bella, American female.
	AfBella,
	/// Nicole, American female.
	AfNicole,
	/// Aoede, American female.
	AfAoede,
	/// Kore, American female.
	AfKore,
	/// Sarah, American female.
	AfSarah,
	/// Michael, American male.
	AmMichael,
	/// Fenrir, American male.
	AmFenrir,
	/// Puck, American male.
	AmPuck,
	/// Emma, British female.
	BfEmma,
	/// George, British male.
	BmGeorge,
	/// Fable, British male.
	BmFable,
}

/// Stable realtime voice id.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum LiveVoice {
	/// Arbor realtime voice.
	Arbor,
	/// Breeze realtime voice.
	Breeze,
	/// Cove realtime voice.
	Cove,
	/// Ember realtime voice.
	Ember,
	/// Juniper realtime voice.
	Juniper,
	/// Maple realtime voice.
	Maple,
	/// Sol realtime voice.
	Sol,
	/// Spruce realtime voice.
	Spruce,
	/// Vale realtime voice.
	Vale,
}

/// Built-in xAI Grok Voice id. xAI may additionally accept custom ids.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum XaiVoice {
	/// Ara built-in voice.
	Ara,
	/// Eve built-in voice.
	Eve,
	/// Leo built-in voice.
	Leo,
	/// Rex built-in voice.
	Rex,
	/// Sal built-in voice.
	Sal,
}

/// Exactly four supported local speech-to-text presets in picker order.
pub const STT_PRESETS: [SttPreset; 4] =
	[SttPreset::Fast, SttPreset::Balanced, SttPreset::Turbo, SttPreset::Parakeet];

/// Exactly twelve curated Kokoro voices in picker order.
pub const KOKORO_VOICES: [KokoroVoice; 12] = [
	KokoroVoice::AfHeart,
	KokoroVoice::AfBella,
	KokoroVoice::AfNicole,
	KokoroVoice::AfAoede,
	KokoroVoice::AfKore,
	KokoroVoice::AfSarah,
	KokoroVoice::AmMichael,
	KokoroVoice::AmFenrir,
	KokoroVoice::AmPuck,
	KokoroVoice::BfEmma,
	KokoroVoice::BmGeorge,
	KokoroVoice::BmFable,
];

/// Exactly nine supported realtime voices in picker order.
pub const LIVE_VOICES: [LiveVoice; 9] = [
	LiveVoice::Arbor,
	LiveVoice::Breeze,
	LiveVoice::Cove,
	LiveVoice::Ember,
	LiveVoice::Juniper,
	LiveVoice::Maple,
	LiveVoice::Sol,
	LiveVoice::Spruce,
	LiveVoice::Vale,
];

/// xAI's documented built-in voices in picker order.
pub const XAI_VOICES: [XaiVoice; 5] =
	[XaiVoice::Ara, XaiVoice::Eve, XaiVoice::Leo, XaiVoice::Rex, XaiVoice::Sal];

#[derive(Clone, Copy)]
struct SttMetadata {
	id:          SttPreset,
	label:       &'static str,
	description: &'static str,
}

const STT_METADATA: [SttMetadata; 4] = [
	SttMetadata {
		id:          SttPreset::Fast,
		label:       "Fast (Whisper base)",
		description: "Whisper base, multilingual. Smallest and fastest; best for low-resource \
		              machines.",
	},
	SttMetadata {
		id:          SttPreset::Balanced,
		label:       "Balanced (Whisper small)",
		description: "Whisper small, multilingual. More accurate than Fast while remaining light on \
		              CPU and memory.",
	},
	SttMetadata {
		id:          SttPreset::Turbo,
		label:       "Turbo (Whisper large-v3)",
		description: "Whisper large-v3-turbo, multilingual. Widest language coverage and largest \
		              download.",
	},
	SttMetadata {
		id:          SttPreset::Parakeet,
		label:       "Parakeet TDT v3 (SoTA)",
		description: "NVIDIA Parakeet TDT 0.6B v3, 25 languages. Default for accuracy and decoding \
		              throughput.",
	},
];

#[derive(Clone, Copy)]
struct VoiceMetadata<I> {
	id:    I,
	label: &'static str,
}

const KOKORO_METADATA: [VoiceMetadata<KokoroVoice>; 12] = [
	VoiceMetadata { id: KokoroVoice::AfHeart, label: "Heart (American female)" },
	VoiceMetadata { id: KokoroVoice::AfBella, label: "Bella (American female)" },
	VoiceMetadata { id: KokoroVoice::AfNicole, label: "Nicole (American female)" },
	VoiceMetadata { id: KokoroVoice::AfAoede, label: "Aoede (American female)" },
	VoiceMetadata { id: KokoroVoice::AfKore, label: "Kore (American female)" },
	VoiceMetadata { id: KokoroVoice::AfSarah, label: "Sarah (American female)" },
	VoiceMetadata { id: KokoroVoice::AmMichael, label: "Michael (American male)" },
	VoiceMetadata { id: KokoroVoice::AmFenrir, label: "Fenrir (American male)" },
	VoiceMetadata { id: KokoroVoice::AmPuck, label: "Puck (American male)" },
	VoiceMetadata { id: KokoroVoice::BfEmma, label: "Emma (British female)" },
	VoiceMetadata { id: KokoroVoice::BmGeorge, label: "George (British male)" },
	VoiceMetadata { id: KokoroVoice::BmFable, label: "Fable (British male)" },
];

const LIVE_METADATA: [VoiceMetadata<LiveVoice>; 9] = [
	VoiceMetadata { id: LiveVoice::Arbor, label: "Arbor" },
	VoiceMetadata { id: LiveVoice::Breeze, label: "Breeze" },
	VoiceMetadata { id: LiveVoice::Cove, label: "Cove" },
	VoiceMetadata { id: LiveVoice::Ember, label: "Ember" },
	VoiceMetadata { id: LiveVoice::Juniper, label: "Juniper" },
	VoiceMetadata { id: LiveVoice::Maple, label: "Maple" },
	VoiceMetadata { id: LiveVoice::Sol, label: "Sol" },
	VoiceMetadata { id: LiveVoice::Spruce, label: "Spruce" },
	VoiceMetadata { id: LiveVoice::Vale, label: "Vale" },
];

const XAI_METADATA: [VoiceMetadata<XaiVoice>; 5] = [
	VoiceMetadata { id: XaiVoice::Ara, label: "Ara" },
	VoiceMetadata { id: XaiVoice::Eve, label: "Eve" },
	VoiceMetadata { id: XaiVoice::Leo, label: "Leo" },
	VoiceMetadata { id: XaiVoice::Rex, label: "Rex" },
	VoiceMetadata { id: XaiVoice::Sal, label: "Sal" },
];

/// Platform/backend manifests associated with backend-neutral catalog ids.
#[derive(Clone, Debug)]
pub struct SpeechArtifactManifests {
	stt:    [(SttPreset, ArtifactManifest); 4],
	kokoro: ArtifactManifest,
}

impl SpeechArtifactManifests {
	/// Constructs bindings and enforces exactly one manifest per STT preset.
	pub fn new(
		stt: [(SttPreset, ArtifactManifest); 4],
		kokoro: ArtifactManifest,
	) -> Result<Self, SpeechCatalogError> {
		let mut ids = HashSet::with_capacity(stt.len());
		for (id, manifest) in &stt {
			manifest
				.validate()
				.map_err(|source| SpeechCatalogError::Artifact { source })?;
			if !ids.insert(*id) {
				return Err(SpeechCatalogError::DuplicateSttPreset { preset: *id });
			}
		}
		if STT_PRESETS.iter().any(|id| !ids.contains(id)) {
			return Err(SpeechCatalogError::MissingSttPreset);
		}
		kokoro
			.validate()
			.map_err(|source| SpeechCatalogError::Artifact { source })?;
		Ok(Self { stt, kokoro })
	}

	/// Returns the platform manifest for one STT preset.
	pub fn stt_manifest(&self, preset: SttPreset) -> &ArtifactManifest {
		self
			.stt
			.iter()
			.find_map(|(id, manifest)| (*id == preset).then_some(manifest))
			.expect("constructor proves every STT preset has one manifest")
	}

	/// Returns the platform manifest for Kokoro-82M.
	pub const fn kokoro_manifest(&self) -> &ArtifactManifest {
		&self.kokoro
	}
}

/// Invalid platform artifact bindings for the speech catalog.
#[derive(Debug, thiserror::Error)]
pub enum SpeechCatalogError {
	/// The same STT preset was bound more than once.
	#[error("speech artifact bindings contain duplicate STT preset {preset}")]
	DuplicateSttPreset {
		/// Repeated preset.
		preset: SttPreset,
	},
	/// One of the four required STT presets had no binding.
	#[error("speech artifact bindings must contain every STT preset exactly once")]
	MissingSttPreset,
	/// An artifact manifest was invalid.
	#[error("speech artifact manifest is invalid")]
	Artifact {
		/// Typed manifest failure.
		#[source]
		source: super::artifact::ArtifactError,
	},
}

/// Stable settings-key projection for ACP, setup, and native frontends.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechSettingKeys {
	/// Speech-to-text model setting.
	pub speech_to_text_model:    Str,
	/// Local text-to-speech model setting.
	pub text_to_speech_model:    Str,
	/// Local text-to-speech voice setting.
	pub text_to_speech_voice:    Str,
	/// Assistant vocalization voice setting.
	pub speech_voice:            Str,
	/// Realtime voice setting.
	pub live_voice:              Str,
	/// Text-to-speech provider route setting.
	pub text_to_speech_provider: Str,
}

/// Stable catalog defaults.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechDefaults {
	/// Default speech-to-text preset.
	pub speech_to_text_model:    SttPreset,
	/// Default local text-to-speech model.
	pub text_to_speech_model:    Str,
	/// Default Kokoro voice.
	pub text_to_speech_voice:    KokoroVoice,
	/// Default assistant-vocalization voice.
	pub speech_voice:            KokoroVoice,
	/// Default realtime voice.
	pub live_voice:              LiveVoice,
	/// Default xAI built-in voice.
	pub xai_voice:               XaiVoice,
	/// Default text-to-speech provider route.
	pub text_to_speech_provider: Str,
}

/// Capabilities of the shared verified artifact downloader.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDownloadCapabilities {
	/// Downloads may be cancelled without publishing incomplete files.
	pub cancellable:        bool,
	/// Valid sidecars may be resumed.
	pub resumable:          bool,
	/// Every promoted file is length- and SHA-256-verified.
	pub checksum_verified:  bool,
	/// Multi-shard progress is aggregate and monotonic.
	pub aggregate_progress: bool,
	/// Promotion replaces final paths atomically.
	pub atomic_promotion:   bool,
	/// Whether this particular model has more than one shard.
	pub multi_shard:        bool,
}

impl ArtifactDownloadCapabilities {
	fn for_manifest(manifest: &ArtifactManifest) -> Self {
		Self {
			cancellable:        true,
			resumable:          true,
			checksum_verified:  true,
			aggregate_progress: true,
			atomic_promotion:   true,
			multi_shard:        manifest.shards.len() > 1,
		}
	}
}

/// Serializable labeled voice option.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpeechVoiceOption {
	/// Stable voice id.
	pub value: Str,
	/// Human-readable label.
	pub label: Str,
}

/// Serializable STT model option with actual artifact cache evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechToTextModelOption {
	/// Stable preset id.
	pub value:       SttPreset,
	/// Human-readable label.
	pub label:       Str,
	/// Concise picker description.
	pub description: Str,
	/// Cache evidence derived from manifest files and checksums.
	pub cache:       ArtifactCacheState,
	/// Shared downloader capabilities.
	pub download:    ArtifactDownloadCapabilities,
}

/// Serializable local TTS model option with voices and cache evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextToSpeechModelOption {
	/// Stable model id.
	pub value:       Str,
	/// Human-readable label.
	pub label:       Str,
	/// Concise picker description.
	pub description: Str,
	/// Native PCM sample rate.
	pub sample_rate: u32,
	/// Voice choices which require no additional download.
	pub voices:      Vec<SpeechVoiceOption>,
	/// Cache evidence derived from manifest files and checksums.
	pub cache:       ArtifactCacheState,
	/// Shared downloader capabilities.
	pub download:    ArtifactDownloadCapabilities,
}

/// Serializable STT section.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechToTextCatalog {
	/// Owning setting key.
	pub setting:       Str,
	/// Stable default preset.
	pub default_value: SttPreset,
	/// Exactly four preset options.
	pub models:        Vec<SpeechToTextModelOption>,
}

/// Serializable local TTS section.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextToSpeechCatalog {
	/// Owning model setting key.
	pub model_setting:        Str,
	/// Owning direct-TTS voice setting key.
	pub voice_setting:        Str,
	/// Owning assistant-vocalization voice setting key.
	pub speech_voice_setting: Str,
	/// Stable default model.
	pub default_model:        Str,
	/// Stable default voice.
	pub default_voice:        KokoroVoice,
	/// Kokoro model entry.
	pub models:               Vec<TextToSpeechModelOption>,
	/// Default-model voice options.
	pub voices:               Vec<SpeechVoiceOption>,
}

/// Serializable realtime-voice section.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSpeechCatalog {
	/// Owning setting key.
	pub setting:       Str,
	/// Stable default voice.
	pub default_voice: LiveVoice,
	/// Exactly nine realtime voices.
	pub voices:        Vec<SpeechVoiceOption>,
}

/// Serializable xAI speech section.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XaiSpeechCatalog {
	/// Stable default built-in voice.
	pub default_voice:    XaiVoice,
	/// Documented built-in voices.
	pub built_in_voices:  Vec<SpeechVoiceOption>,
	/// Whether caller-supplied custom voice ids are accepted.
	pub custom_voice_ids: bool,
}

/// Serializable backend-neutral speech catalog snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechCatalogSnapshot {
	/// Stable settings-key map.
	pub settings:       SpeechSettingKeys,
	/// Stable defaults.
	pub defaults:       SpeechDefaults,
	/// Local STT section.
	pub speech_to_text: SpeechToTextCatalog,
	/// Local TTS section.
	pub text_to_speech: TextToSpeechCatalog,
	/// Realtime voice section.
	pub live:           LiveSpeechCatalog,
	/// Hosted xAI voice section.
	pub xai:            XaiSpeechCatalog,
}

/// Stateless owner of the canonical speech catalog.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpeechCatalog;

impl SpeechCatalog {
	/// Builds a serializable snapshot whose cache fields are derived from actual
	/// manifest artifacts in `store`.
	pub fn snapshot(
		&self,
		store: &ArtifactStore,
		artifacts: &SpeechArtifactManifests,
		cancel: &LocalCancellation,
	) -> ArtifactResult<SpeechCatalogSnapshot> {
		let mut stt_models = Vec::with_capacity(STT_METADATA.len());
		for metadata in STT_METADATA {
			let manifest = artifacts.stt_manifest(metadata.id);
			stt_models.push(SpeechToTextModelOption {
				value:       metadata.id,
				label:       Str::from(metadata.label),
				description: Str::from(metadata.description),
				cache:       store.inspect_manifest(manifest, cancel)?,
				download:    ArtifactDownloadCapabilities::for_manifest(manifest),
			});
		}
		let voices = KOKORO_METADATA
			.iter()
			.map(|voice| voice_option(voice.id.into(), voice.label))
			.collect::<Vec<_>>();
		let kokoro = artifacts.kokoro_manifest();
		let tts_model = TextToSpeechModelOption {
			value:       Str::from(DEFAULT_TTS_MODEL),
			label:       Str::from("Kokoro-82M"),
			description: Str::from(
				"Kokoro-82M neural TTS with multi-voice, fully local 24 kHz synthesis.",
			),
			sample_rate: 24_000,
			voices:      voices.clone(),
			cache:       store.inspect_manifest(kokoro, cancel)?,
			download:    ArtifactDownloadCapabilities::for_manifest(kokoro),
		};
		Ok(SpeechCatalogSnapshot {
			settings:       SpeechSettingKeys {
				speech_to_text_model:    Str::from(STT_MODEL_SETTING),
				text_to_speech_model:    Str::from(TTS_MODEL_SETTING),
				text_to_speech_voice:    Str::from(TTS_VOICE_SETTING),
				speech_voice:            Str::from(SPEECH_VOICE_SETTING),
				live_voice:              Str::from(LIVE_VOICE_SETTING),
				text_to_speech_provider: Str::from(TTS_PROVIDER_SETTING),
			},
			defaults:       SpeechDefaults {
				speech_to_text_model:    DEFAULT_STT_PRESET,
				text_to_speech_model:    Str::from(DEFAULT_TTS_MODEL),
				text_to_speech_voice:    DEFAULT_KOKORO_VOICE,
				speech_voice:            DEFAULT_KOKORO_VOICE,
				live_voice:              DEFAULT_LIVE_VOICE,
				xai_voice:               DEFAULT_XAI_VOICE,
				text_to_speech_provider: Str::from(DEFAULT_TTS_PROVIDER),
			},
			speech_to_text: SpeechToTextCatalog {
				setting:       Str::from(STT_MODEL_SETTING),
				default_value: DEFAULT_STT_PRESET,
				models:        stt_models,
			},
			text_to_speech: TextToSpeechCatalog {
				model_setting: Str::from(TTS_MODEL_SETTING),
				voice_setting: Str::from(TTS_VOICE_SETTING),
				speech_voice_setting: Str::from(SPEECH_VOICE_SETTING),
				default_model: Str::from(DEFAULT_TTS_MODEL),
				default_voice: DEFAULT_KOKORO_VOICE,
				models: vec![tts_model],
				voices,
			},
			live:           LiveSpeechCatalog {
				setting:       Str::from(LIVE_VOICE_SETTING),
				default_voice: DEFAULT_LIVE_VOICE,
				voices:        LIVE_METADATA
					.iter()
					.map(|voice| voice_option(voice.id.into(), voice.label))
					.collect(),
			},
			xai:            XaiSpeechCatalog {
				default_voice:    DEFAULT_XAI_VOICE,
				built_in_voices:  XAI_METADATA
					.iter()
					.map(|voice| voice_option(voice.id.into(), voice.label))
					.collect(),
				custom_voice_ids: true,
			},
		})
	}
}

fn voice_option(id: &'static str, label: &'static str) -> SpeechVoiceOption {
	SpeechVoiceOption { value: Str::from(id), label: Str::from(label) }
}

#[cfg(test)]
mod tests {
	use std::fs;

	use sha2::{Digest as _, Sha256};
	use tempfile::tempdir;

	use super::*;
	use crate::local::artifact::{ArtifactCacheStatus, ArtifactShard, ArtifactSpec};

	fn manifest(id: &str, path: &str, bytes: &[u8]) -> ArtifactManifest {
		ArtifactManifest::new(id, vec![ArtifactShard {
			spec:   ArtifactSpec {
				path:   path.into(),
				bytes:  bytes.len() as u64,
				sha256: Sha256::digest(bytes).into(),
			},
			source: Str::from("https://fixtures.invalid/artifact"),
		}])
		.unwrap()
	}

	#[test]
	fn catalog_has_exact_stable_ids_defaults_and_setting_keys() {
		assert_eq!(STT_PRESETS.len(), 4);
		assert_eq!(HashSet::from(STT_PRESETS).len(), 4);
		assert_eq!(DEFAULT_STT_PRESET, SttPreset::Parakeet);
		assert_eq!(KOKORO_VOICES.len(), 12);
		assert_eq!(HashSet::from(KOKORO_VOICES).len(), 12);
		assert_eq!(<&'static str>::from(DEFAULT_KOKORO_VOICE), "af_heart");
		assert_eq!(LIVE_VOICES.len(), 9);
		assert_eq!(HashSet::from(LIVE_VOICES).len(), 9);
		assert_eq!(<&'static str>::from(DEFAULT_LIVE_VOICE), "sol");
		assert_eq!(HashSet::from(XAI_VOICES).len(), XAI_VOICES.len());
		assert_eq!(<&'static str>::from(DEFAULT_XAI_VOICE), "eve");
		assert_eq!(STT_MODEL_SETTING, "stt.modelName");
		assert_eq!(TTS_MODEL_SETTING, "tts.localModel");
		assert_eq!(TTS_VOICE_SETTING, "tts.localVoice");
		assert_eq!(SPEECH_VOICE_SETTING, "speech.voice");
		assert_eq!(LIVE_VOICE_SETTING, "live.voice");
	}

	#[test]
	fn snapshot_cache_state_comes_from_verified_files_and_sidecars() {
		let directory = tempdir().expect("temporary artifact root");
		let store = ArtifactStore::open(directory.path()).unwrap();
		let fast = manifest("fast", "fast.bin", b"fast");
		let balanced = manifest("balanced", "balanced.bin", b"balanced");
		let turbo = manifest("turbo", "turbo.bin", b"turbo");
		let parakeet = manifest("parakeet", "parakeet.bin", b"parakeet");
		let kokoro = manifest("kokoro", "kokoro.bin", b"kokoro");
		fs::write(directory.path().join("fast.bin"), b"fast").unwrap();
		fs::write(directory.path().join("balanced.bin.part"), b"bal").unwrap();
		fs::write(directory.path().join("turbo.bin"), b"wrong").unwrap();
		fs::write(directory.path().join("kokoro.bin"), b"kokoro").unwrap();
		let artifacts = SpeechArtifactManifests::new(
			[
				(SttPreset::Fast, fast),
				(SttPreset::Balanced, balanced),
				(SttPreset::Turbo, turbo),
				(SttPreset::Parakeet, parakeet),
			],
			kokoro,
		)
		.unwrap();
		let snapshot = SpeechCatalog
			.snapshot(&store, &artifacts, &LocalCancellation::new())
			.unwrap();
		assert_eq!(snapshot.speech_to_text.models.len(), 4);
		assert_eq!(snapshot.speech_to_text.models[0].cache.status, ArtifactCacheStatus::Ready);
		assert_eq!(snapshot.speech_to_text.models[1].cache.status, ArtifactCacheStatus::Partial);
		assert_eq!(snapshot.speech_to_text.models[2].cache.status, ArtifactCacheStatus::Corrupt);
		assert_eq!(snapshot.speech_to_text.models[3].cache.status, ArtifactCacheStatus::Missing);
		assert_eq!(snapshot.text_to_speech.models.len(), 1);
		assert_eq!(snapshot.text_to_speech.models[0].voices.len(), 12);
		assert_eq!(snapshot.text_to_speech.models[0].cache.status, ArtifactCacheStatus::Ready);
		assert_eq!(snapshot.live.voices.len(), 9);
		assert_eq!(snapshot.xai.built_in_voices.len(), 5);
		assert!(snapshot.xai.custom_voice_ids);
	}

	#[test]
	fn artifact_bindings_reject_duplicate_or_missing_presets() {
		let fixture = || manifest("fixture", "fixture.bin", b"fixture");
		let result = SpeechArtifactManifests::new(
			[
				(SttPreset::Fast, fixture()),
				(SttPreset::Fast, fixture()),
				(SttPreset::Turbo, fixture()),
				(SttPreset::Parakeet, fixture()),
			],
			fixture(),
		);
		assert!(matches!(result, Err(SpeechCatalogError::DuplicateSttPreset { .. })));
	}
}
