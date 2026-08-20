//! Revision-keyed, synchronous tool renderer folds.

use std::{any::Any, collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use omp_core::Str;
use serde::de::DeserializeOwned;
use smallvec::SmallVec;
use thiserror::Error;

use crate::ToolIdentity;

/// A typed, pure renderer for one exact tool revision.
///
/// Updates are folded into [`State`](Self::State) synchronously. `view`
/// receives the current state and, after settlement, the typed durable outcome.
/// Returning `None` declines to the generic data fallback.
pub trait Render: Send + Sync + 'static {
	/// Incrementally retained fold state.
	type State: Default + Send + Sync + 'static;
	/// Typed ephemeral update decoded from exact JSON bytes.
	type Update: DeserializeOwned;
	/// Typed durable outcome decoded from exact JSON bytes.
	type Outcome: DeserializeOwned;

	/// Incorporates one update in arrival order.
	fn fold(&self, state: &mut Self::State, update: Self::Update);

	/// Produces TML or deterministic display data for the current fold position.
	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str>;
}

/// Compact retained state for one live or settled rendered call.
///
/// Unknown revisions retain their few raw updates inline for the generic
/// fallback. A registered renderer replaces those bytes with its typed
/// accumulator on the first fold.
#[derive(Default)]
pub struct ViewState {
	identity: Option<Arc<ToolIdentity>>,
	fold:     FoldState,
}

enum FoldState {
	Updates(SmallVec<Bytes, 4>),
	Reduced(Box<dyn Any + Send + Sync>),
}

impl Default for FoldState {
	fn default() -> Self {
		Self::Updates(SmallVec::new())
	}
}

impl ViewState {
	/// Creates an empty fold state, not yet bound to an identity.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Borrows the exact identity bound by the first fold or view.
	#[must_use]
	pub fn identity(&self) -> Option<&ToolIdentity> {
		self.identity.as_deref()
	}

	/// Returns the number of raw updates retained for a generic fallback.
	#[must_use]
	pub const fn raw_update_count(&self) -> usize {
		match &self.fold {
			FoldState::Updates(updates) => updates.len(),
			FoldState::Reduced(_) => 0,
		}
	}

	fn bind(&mut self, identity: &ToolIdentity) -> Result<(), RenderRegistryError> {
		match &self.identity {
			Some(bound) if bound.as_ref() != identity => Err(RenderRegistryError::StateIdentity {
				bound:     bound.clone(),
				requested: identity.clone(),
			}),
			Some(_) => Ok(()),
			None => {
				self.identity = Some(Arc::new(identity.clone()));
				Ok(())
			},
		}
	}

	fn check(&self, identity: &ToolIdentity) -> Result<(), RenderRegistryError> {
		match &self.identity {
			Some(bound) if bound.as_ref() != identity => Err(RenderRegistryError::StateIdentity {
				bound:     bound.clone(),
				requested: identity.clone(),
			}),
			_ => Ok(()),
		}
	}
}

/// Deterministic renderer registration or dispatch failure.
#[derive(Debug, Error)]
pub enum RenderRegistryError {
	/// The exact `(name, revision)` key already has a renderer.
	#[error("renderer already registered: {0:?}")]
	Duplicate(ToolIdentity),
	/// One retained state was presented under another exact identity.
	#[error("render state for {bound:?} cannot be dispatched as {requested:?}")]
	StateIdentity {
		/// Identity which first bound the state.
		bound:     Arc<ToolIdentity>,
		/// Identity requested by the caller.
		requested: ToolIdentity,
	},
	/// An update did not match the renderer's typed update vocabulary.
	#[error("renderer update decode failed for {identity:?}: {source}")]
	Update {
		/// Exact renderer identity.
		identity: ToolIdentity,
		/// JSON decoder failure.
		source:   serde_json::Error,
	},
	/// A durable outcome did not match the renderer's typed outcome vocabulary.
	#[error("renderer outcome decode failed for {identity:?}: {source}")]
	Outcome {
		/// Exact renderer identity.
		identity: ToolIdentity,
		/// JSON decoder failure.
		source:   serde_json::Error,
	},
	/// Retained erased state did not belong to its registered renderer.
	#[error("renderer state type mismatch for {0:?}")]
	StateType(ToolIdentity),
	/// Generic fallback data was not UTF-8 JSON.
	#[error("generic renderer data is not UTF-8 for {identity:?}: {source}")]
	Utf8 {
		/// Exact requested identity.
		identity: ToolIdentity,
		/// UTF-8 decoder failure.
		source:   std::str::Utf8Error,
	},
}

const _: () = assert!(
	std::mem::size_of::<RenderRegistryError>() <= 128,
	"RenderRegistryError must stay compact"
);

trait ErasedRender: Send + Sync {
	fn initial(&self) -> Box<dyn Any + Send + Sync>;
	fn fold(
		&self,
		identity: &ToolIdentity,
		state: &mut dyn Any,
		update: &[u8],
	) -> Result<(), RenderRegistryError>;
	fn view(
		&self,
		identity: &ToolIdentity,
		state: &dyn Any,
		outcome: Option<&[u8]>,
	) -> Result<Option<Str>, RenderRegistryError>;
}

struct RegisteredRender<R>(R);

impl<R: Render> ErasedRender for RegisteredRender<R> {
	fn initial(&self) -> Box<dyn Any + Send + Sync> {
		Box::new(R::State::default())
	}

	fn fold(
		&self,
		identity: &ToolIdentity,
		state: &mut dyn Any,
		update: &[u8],
	) -> Result<(), RenderRegistryError> {
		let state = state
			.downcast_mut::<R::State>()
			.ok_or_else(|| RenderRegistryError::StateType(identity.clone()))?;
		let update = serde_json::from_slice(update)
			.map_err(|source| RenderRegistryError::Update { identity: identity.clone(), source })?;
		self.0.fold(state, update);
		Ok(())
	}

	fn view(
		&self,
		identity: &ToolIdentity,
		state: &dyn Any,
		outcome: Option<&[u8]>,
	) -> Result<Option<Str>, RenderRegistryError> {
		let state = state
			.downcast_ref::<R::State>()
			.ok_or_else(|| RenderRegistryError::StateType(identity.clone()))?;
		let outcome = outcome
			.map(serde_json::from_slice::<R::Outcome>)
			.transpose()
			.map_err(|source| RenderRegistryError::Outcome { identity: identity.clone(), source })?;
		Ok(self.0.view(state, outcome.as_ref()))
	}
}

/// Borrowed, cached exact-revision renderer lookup.
#[derive(Clone, Copy)]
pub struct RenderEntry<'a> {
	identity: &'a Arc<ToolIdentity>,
	render:   &'a dyn ErasedRender,
}

impl RenderEntry<'_> {
	/// Borrows the exact registered identity.
	#[must_use]
	pub fn identity(&self) -> &ToolIdentity {
		self.identity
	}

	/// Folds one serialized update synchronously into retained state.
	pub fn fold(self, state: &mut ViewState, update: &[u8]) -> Result<(), RenderRegistryError> {
		state.bind(self.identity)?;
		match &mut state.fold {
			FoldState::Reduced(reduced) => {
				self
					.render
					.fold(self.identity.as_ref(), reduced.as_mut(), update)
			},
			FoldState::Updates(updates) => {
				let mut reduced = self.render.initial();
				for prior in updates.iter() {
					self
						.render
						.fold(self.identity.as_ref(), reduced.as_mut(), prior)?;
				}
				self
					.render
					.fold(self.identity.as_ref(), reduced.as_mut(), update)?;
				state.fold = FoldState::Reduced(reduced);
				Ok(())
			},
		}
	}

	/// Renders the current state and optional serialized durable outcome.
	pub fn view(
		self,
		state: &ViewState,
		outcome: Option<&[u8]>,
	) -> Result<Option<Str>, RenderRegistryError> {
		state.check(self.identity)?;
		match &state.fold {
			FoldState::Reduced(reduced) => {
				self
					.render
					.view(self.identity.as_ref(), reduced.as_ref(), outcome)
			},
			FoldState::Updates(updates) => {
				let mut reduced = self.render.initial();
				for update in updates {
					self
						.render
						.fold(self.identity.as_ref(), reduced.as_mut(), update)?;
				}
				self
					.render
					.view(self.identity.as_ref(), reduced.as_ref(), outcome)
			},
		}
	}
}

/// Immutable-by-key registry of exact-revision renderer folds.
#[derive(Default)]
pub struct RenderRegistry {
	entries: BTreeMap<Arc<ToolIdentity>, Box<dyn ErasedRender>>,
}

impl RenderRegistry {
	/// Creates an empty renderer registry.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Registers one renderer for one exact `(name, revision)` identity.
	pub fn register<R: Render>(
		&mut self,
		identity: ToolIdentity,
		render: R,
	) -> Result<(), RenderRegistryError> {
		let identity = Arc::new(identity);
		if self.entries.contains_key(&identity) {
			return Err(RenderRegistryError::Duplicate((*identity).clone()));
		}
		self
			.entries
			.insert(identity, Box::new(RegisteredRender(render)));
		Ok(())
	}

	/// Borrows the renderer cached for this exact identity.
	#[must_use]
	pub fn get(&self, identity: &ToolIdentity) -> Option<RenderEntry<'_>> {
		let (stored, render) = self.entries.get_key_value(identity)?;
		Some(RenderEntry { identity: stored, render: render.as_ref() })
	}

	/// Folds one update, retaining raw bytes only when the exact revision is
	/// unknown.
	pub fn fold(
		&self,
		identity: &ToolIdentity,
		state: &mut ViewState,
		update: Bytes,
	) -> Result<(), RenderRegistryError> {
		if let Some(entry) = self.get(identity) {
			return entry.fold(state, &update);
		}
		state.bind(identity)?;
		match &mut state.fold {
			FoldState::Updates(updates) => {
				if updates.len() == 4 {
					let _ = updates.remove(0);
				}
				updates.push(update);
				Ok(())
			},
			FoldState::Reduced(_) => Err(RenderRegistryError::StateType(identity.clone())),
		}
	}

	/// Dispatches an exact-revision renderer or the generic built-in fallback.
	///
	/// No name-only lookup is attempted when the revision is unknown.
	pub fn view(
		&self,
		identity: &ToolIdentity,
		state: &ViewState,
		outcome: Option<&[u8]>,
	) -> Result<Str, RenderRegistryError> {
		if let Some(entry) = self.get(identity) {
			if let Some(rendered) = entry.view(state, outcome)? {
				return Ok(rendered);
			}
		} else {
			state.check(identity)?;
		}
		generic_view(identity, state, outcome)
	}
}

fn generic_view(
	identity: &ToolIdentity,
	state: &ViewState,
	outcome: Option<&[u8]>,
) -> Result<Str, RenderRegistryError> {
	let data = outcome.or_else(|| match &state.fold {
		FoldState::Updates(updates) => updates.last().map(Bytes::as_ref),
		FoldState::Reduced(_) => None,
	});
	let Some(data) = data else {
		return Ok(Str::new_static("{}"));
	};
	let data = std::str::from_utf8(data)
		.map_err(|source| RenderRegistryError::Utf8 { identity: identity.clone(), source })?;
	Ok(Str::new(data))
}
