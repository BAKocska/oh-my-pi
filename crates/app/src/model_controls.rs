//! Durable model preferences and journal-restored session overrides.

use std::{collections::BTreeMap, sync::Arc};

use omp_core::Str;
use omp_llm_catalog::{ModelKey, ThinkingEffort};

/// Direction for model and role cycling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CycleDirection {
	/// Advance and wrap at the end.
	Forward,
	/// Move backward and wrap at the beginning.
	Backward,
}

/// One enabled model in a temporary cycle scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedModel {
	/// Catalog model key.
	pub model:    ModelKey,
	/// Optional role-specified thinking selection.
	pub thinking: Option<ThinkingEffort>,
}

/// Journal payload for a session-only model override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournaledModelOverride {
	/// Role whose configured model was temporarily replaced.
	pub role:     Str,
	/// Effective session model.
	pub model:    ModelKey,
	/// Optional temporary thinking selection.
	pub thinking: Option<ThinkingEffort>,
}

/// Result of bidirectional role cycling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleCycleSelection {
	/// Selected configured role.
	pub role:  Str,
	/// Model assigned to the role.
	pub model: ModelKey,
}

/// Split authority for durable preferences and journaled effective overrides.
#[derive(Clone, Debug, Default)]
pub struct ModelControls {
	durable_roles: BTreeMap<Str, ModelKey>,
	override_:     Option<JournaledModelOverride>,
	scoped:        Arc<[ScopedModel]>,
	active_role:   Option<Str>,
}

impl ModelControls {
	/// Restores durable settings without creating a journal event.
	#[must_use]
	pub fn from_durable(durable_roles: BTreeMap<Str, ModelKey>) -> Self {
		Self { durable_roles, ..Self::default() }
	}

	/// Replaces one durable `/model` preference.
	///
	/// The caller persists this through settings authority. This operation never
	/// creates or changes a session override.
	pub fn set_durable(&mut self, role: impl Into<Str>, model: ModelKey) {
		self.durable_roles.insert(role.into(), model);
	}

	/// Applies a temporary Ctrl-P or `/switch` selection and returns its journal
	/// payload.
	pub fn switch_session(
		&mut self,
		role: impl Into<Str>,
		model: ModelKey,
		thinking: Option<ThinkingEffort>,
	) -> JournaledModelOverride {
		let override_ = JournaledModelOverride { role: role.into(), model, thinking };
		self.active_role = Some(override_.role.clone());
		self.override_ = Some(override_.clone());
		override_
	}

	/// Restores the latest live override from the journal without rewriting
	/// settings.
	pub fn restore_override(&mut self, override_: Option<JournaledModelOverride>) {
		self.active_role = override_.as_ref().map(|selection| selection.role.clone());
		self.override_ = override_;
	}

	/// Clears the effective override while retaining durable preferences.
	pub fn clear_override(&mut self) {
		self.override_ = None;
		self.active_role = None;
	}

	/// Returns the effective model for a role.
	#[must_use]
	pub fn effective(&self, role: &str) -> Option<&ModelKey> {
		self
			.override_
			.as_ref()
			.filter(|selection| selection.role.as_str() == role)
			.map(|selection| &selection.model)
			.or_else(|| self.durable_roles.get(role))
	}

	/// Returns the active journaled override.
	#[must_use]
	pub const fn session_override(&self) -> Option<&JournaledModelOverride> {
		self.override_.as_ref()
	}

	/// Replaces the already-enabled temporary cycle scope.
	///
	/// Enabled-model filtering happens before this call, so disabled models
	/// cannot re-enter through cycling.
	pub fn set_scoped_models(&mut self, scoped: Arc<[ScopedModel]>) {
		self.scoped = scoped;
	}

	/// Cycles the filtered scope in either direction and journals the result.
	pub fn cycle_scoped(
		&mut self,
		current: &ModelKey<str>,
		direction: CycleDirection,
	) -> Option<JournaledModelOverride> {
		if self.scoped.len() <= 1 {
			return None;
		}
		let current_index = self
			.scoped
			.iter()
			.position(|entry| &entry.model == current)
			.unwrap_or(0);
		let index = cycle_index(current_index, self.scoped.len(), direction);
		let next = self.scoped[index].clone();
		Some(self.switch_session("temporary", next.model, next.thinking))
	}

	/// Cycles configured role models in fixed role order and either direction.
	///
	/// Missing roles and roles filtered out of `enabled` are skipped before the
	/// current position is selected.
	pub fn cycle_roles(
		&mut self,
		role_order: &[Str],
		enabled: impl Fn(&ModelKey<str>) -> bool,
		direction: CycleDirection,
	) -> Option<RoleCycleSelection> {
		let available: Vec<_> = role_order
			.iter()
			.filter_map(|role| {
				self
					.durable_roles
					.get(role)
					.filter(|model| enabled(model))
					.map(|model| (role.clone(), model.clone()))
			})
			.collect();
		if available.len() <= 1 {
			return None;
		}
		let current_index = self
			.active_role
			.as_ref()
			.and_then(|role| {
				available
					.iter()
					.position(|(candidate, _)| candidate == role)
			})
			.or_else(|| {
				self.override_.as_ref().and_then(|active| {
					available
						.iter()
						.position(|(_, model)| model == &active.model)
				})
			})
			.unwrap_or(0);
		let index = cycle_index(current_index, available.len(), direction);
		let (role, model) = available[index].clone();
		self.switch_session(role.clone(), model.clone(), None);
		Some(RoleCycleSelection { role, model })
	}
}

fn cycle_index(current: usize, len: usize, direction: CycleDirection) -> usize {
	match direction {
		CycleDirection::Forward => (current + 1) % len,
		CycleDirection::Backward => (current + len - 1) % len,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn model(value: &str) -> ModelKey {
		ModelKey::from(value)
	}

	#[test]
	fn durable_preference_and_override_are_independent_across_restore() {
		let mut controls = ModelControls::from_durable(BTreeMap::from([(
			"default".into(),
			model("provider/preferred"),
		)]));
		let journaled = controls.switch_session("default", model("provider/temporary"), None);
		assert_eq!(controls.effective("default"), Some(&model("provider/temporary")));

		let mut resumed = ModelControls::from_durable(BTreeMap::from([(
			"default".into(),
			model("provider/preferred"),
		)]));
		resumed.restore_override(Some(journaled));
		assert_eq!(resumed.effective("default"), Some(&model("provider/temporary")));
		resumed.clear_override();
		assert_eq!(resumed.effective("default"), Some(&model("provider/preferred")));
	}

	#[test]
	fn scoped_and_role_cycles_wrap_in_both_directions() {
		let mut controls = ModelControls::default();
		controls.set_scoped_models(Arc::from([
			ScopedModel { model: model("p/a"), thinking: None },
			ScopedModel { model: model("p/b"), thinking: Some(ThinkingEffort::High) },
		]));
		let backward = controls
			.cycle_scoped(ModelKey::from_ref("p/a"), CycleDirection::Backward)
			.unwrap();
		assert_eq!(backward.model, model("p/b"));
		assert_eq!(backward.thinking, Some(ThinkingEffort::High));

		controls.set_durable("slow", model("p/slow"));
		controls.set_durable("default", model("p/default"));
		controls.set_durable("smol", model("p/smol"));
		controls.active_role = Some("default".into());
		let roles: Vec<Str> = ["slow", "default", "smol"]
			.into_iter()
			.map(Str::new)
			.collect();
		let previous = controls
			.cycle_roles(
				&roles,
				|model| model != ModelKey::from_ref("p/slow"),
				CycleDirection::Backward,
			)
			.unwrap();
		assert_eq!(previous.role, "smol");
	}
}
