//! Production prompt invalidation authority and its consumed generation state.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use omp_agent::{BandHash, PromptError, PromptSource, SlotClass, SlotDecl, SlotId};
use omp_core::{Hash32, hash32::Hasher};
use omp_envd::worker::ExtHostSpec;
use omp_scribe::Props;
use parking_lot::Mutex;

use crate::rulebook::{PromptHeadAuthority, PromptInvalidationError};

/// Shared prompt-slot generations consumed by prompt assembly and cache keys.
///
/// A prompt consumer must read the generation for the same session and slot
/// while assembling its prompt snapshot. The value returned by
/// [`PromptHeadAuthority::invalidate`] is the value subsequently observed here.
#[derive(Clone, Default)]
pub struct PromptGenerationStore {
	generations: Arc<Mutex<BTreeMap<(u64, SlotId), u64>>>,
}

impl PromptGenerationStore {
	/// Returns the current generation for one session-scoped prompt slot.
	///
	/// A slot which has not yet been invalidated is at generation zero.
	pub fn generation(&self, session_generation: u64, slot: SlotId) -> u64 {
		self
			.generations
			.lock()
			.get(&(session_generation, slot))
			.copied()
			.unwrap_or(0)
	}

	fn advance(
		&self,
		session_generation: u64,
		slot: SlotId,
	) -> Result<u64, PromptInvalidationError> {
		let mut generations = self.generations.lock();
		let generation = generations.entry((session_generation, slot)).or_default();
		let next = generation
			.checked_add(1)
			.ok_or_else(|| PromptInvalidationError::Head("prompt-slot generation exhausted".into()))?;
		*generation = next;
		Ok(next)
	}

	fn fold_bands(&self, declarations: &[SlotDecl], bands: &mut [BandHash; 4]) {
		let generations = self.generations.lock();
		for (class, band) in bands.iter_mut().enumerate() {
			let mut relevant = generations.iter().filter(|((_, slot), _)| {
				declarations
					.iter()
					.any(|declaration| declaration.slot == *slot && declaration.class as usize == class)
			});
			let Some((first_key, first_generation)) = relevant.next() else {
				continue;
			};
			let mut hasher = Hash32::hasher();
			hasher.update(b"omp.prompt-slot-generation.v1");
			hasher.update(band.as_bytes());
			hash_generation(&mut hasher, first_key, *first_generation);
			for (key, generation) in relevant {
				hash_generation(&mut hasher, key, *generation);
			}
			*band = hasher.finalize().into();
		}
	}
}

fn hash_generation(hasher: &mut Hasher, key: &(u64, SlotId), generation: u64) {
	hasher.update(&key.0.to_le_bytes());
	hasher.update(&[key.1 as u8]);
	hasher.update(&generation.to_le_bytes());
}

/// Production authority over the prompt declarations used by assembly.
///
/// The declarations are retained as the single authoritative declaration
/// table. Invalidation validates directly against them instead of maintaining
/// a second slot or ownership registry.
#[derive(Clone)]
pub struct ProductionPromptHead {
	declarations: Arc<[SlotDecl]>,
	generations:  PromptGenerationStore,
}

impl ProductionPromptHead {
	/// Creates an authority over the declarations supplied to prompt assembly.
	pub fn new(declarations: Vec<SlotDecl>) -> Self {
		Self { declarations: declarations.into(), generations: PromptGenerationStore::default() }
	}

	/// Creates an authority from the sealed declarations admitted for extension
	/// hosts.
	///
	/// Static manifests retain one row per owning extension and prompt slot.
	/// Class and priority properties are used when present; otherwise the
	/// canonical extension slot catalog supplies the class and priority zero.
	/// A malformed declared class fails closed as frozen.
	pub fn from_extension_specs(specs: &[ExtHostSpec]) -> Self {
		let declarations = specs
			.iter()
			.flat_map(|spec| {
				spec
					.manifest
					.static_declarations()
					.prompt_slots
					.iter()
					.filter_map(move |declaration| {
						let name = declaration
							.properties
							.get("slot")
							.and_then(serde_json::Value::as_str)
							.or_else(|| (!declaration.key.is_empty()).then_some(declaration.key.as_str()))
							.unwrap_or(declaration.id.as_str());
						let slot = slot_id(name)?;
						let class = match declaration
							.properties
							.get("class")
							.or_else(|| declaration.properties.get("cls"))
						{
							Some(class) => class
								.as_str()
								.and_then(slot_class)
								.unwrap_or(SlotClass::Frozen),
							None => extension_slot_class(slot),
						};
						let priority = declaration
							.properties
							.get("priority")
							.and_then(serde_json::Value::as_i64)
							.and_then(|priority| i16::try_from(priority).ok())
							.unwrap_or(0);
						Some(SlotDecl { slot, class, owner: spec.key.extension().clone(), priority })
					})
			})
			.collect();
		Self::new(declarations)
	}

	/// Returns the shared generation store prompt assembly and cache keys
	/// consume.
	pub fn generation_store(&self) -> PromptGenerationStore {
		self.generations.clone()
	}

	/// Wraps the assembled prompt source so accepted generations enter its
	/// semantic band hashes without changing the wire prompt items.
	///
	/// Production prompt sources must expose semantic bands. An unbanded source
	/// fails rather than silently accepting invalidations its cache key cannot
	/// consume.
	pub fn wrap_prompt_source(&self, source: Arc<dyn PromptSource>) -> Arc<dyn PromptSource> {
		Arc::new(GenerationPromptSource {
			source,
			declarations: Arc::clone(&self.declarations),
			generations: self.generations.clone(),
		})
	}
}

struct GenerationPromptSource {
	source:       Arc<dyn PromptSource>,
	declarations: Arc<[SlotDecl]>,
	generations:  PromptGenerationStore,
}

impl PromptSource for GenerationPromptSource {
	fn render(&self, props: &Props) -> Result<Vec<omp_agent::Item>, PromptError> {
		self.source.render(props)
	}

	fn banded_render(
		&self,
		props: &Props,
	) -> Result<Option<(Vec<omp_agent::Item>, [BandHash; 4])>, PromptError> {
		let Some((items, mut bands)) = self.source.banded_render(props)? else {
			return Err(PromptError::Source(
				"prompt invalidation requires a banded prompt source".into(),
			));
		};
		self.generations.fold_bands(&self.declarations, &mut bands);
		Ok(Some((items, bands)))
	}
}

#[async_trait]
impl PromptHeadAuthority for ProductionPromptHead {
	async fn invalidate(
		&self,
		extension: &str,
		session_generation: u64,
		slot: &str,
	) -> Result<u64, PromptInvalidationError> {
		let slot = slot_id(slot).ok_or(PromptInvalidationError::UnknownSlot)?;
		let mut declared = false;
		let mut owned = false;
		let mut frozen = false;
		for declaration in self
			.declarations
			.iter()
			.filter(|declaration| declaration.slot == slot)
		{
			declared = true;
			if declaration.owner.as_str() == extension {
				owned = true;
				frozen |= declaration.class == SlotClass::Frozen;
			}
		}
		if !declared {
			return Err(PromptInvalidationError::UnknownSlot);
		}
		if !owned {
			return Err(PromptInvalidationError::NotOwner);
		}
		if frozen {
			return Err(PromptInvalidationError::FrozenSlot);
		}
		self.generations.advance(session_generation, slot)
	}
}

fn slot_id(slot: &str) -> Option<SlotId> {
	match slot {
		"conventions" => Some(SlotId::Conventions),
		"role" => Some(SlotId::Role),
		"runtime" => Some(SlotId::Runtime),
		"tools" => Some(SlotId::Tools),
		"policy" => Some(SlotId::Policy),
		"workflow" => Some(SlotId::Workflow),
		"skills" => Some(SlotId::Skills),
		"rules" => Some(SlotId::Rules),
		"guidance" => Some(SlotId::Guidance),
		"workspace" => Some(SlotId::Workspace),
		"memory" => Some(SlotId::Memory),
		"standing" => Some(SlotId::Standing),
		"recall" => Some(SlotId::Recall),
		"status" => Some(SlotId::Status),
		"delivery" => Some(SlotId::Delivery),
		_ => None,
	}
}
fn slot_class(class: &str) -> Option<SlotClass> {
	match class {
		"frozen" => Some(SlotClass::Frozen),
		"stable" => Some(SlotClass::Stable),
		"epochal" => Some(SlotClass::Epochal),
		"volatile" => Some(SlotClass::Volatile),
		_ => None,
	}
}

const fn extension_slot_class(slot: SlotId) -> SlotClass {
	match slot {
		SlotId::Runtime | SlotId::Workflow => SlotClass::Frozen,
		SlotId::Policy | SlotId::Skills | SlotId::Rules | SlotId::Guidance | SlotId::Workspace => {
			SlotClass::Stable
		},
		SlotId::Memory | SlotId::Standing => SlotClass::Epochal,
		SlotId::Recall | SlotId::Status => SlotClass::Volatile,
		SlotId::Conventions | SlotId::Role | SlotId::Tools | SlotId::Delivery => SlotClass::Frozen,
	}
}
