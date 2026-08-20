//! Human-facing model selector parsing and deterministic catalog matching.
//!
//! This module deliberately sits above [`crate::resolve`]: it turns a user's
//! loose selector into an exact provider/model pair, then the constraint
//! resolver remains the only authority that makes a route usable.

use std::collections::{BTreeMap, BTreeSet};

use omp_core::{Str, sf};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CatalogAlias, ModelAvailability, ModelKey, ModelSpec, ProviderId, RouteDef, RouteId};

/// A configured or built-in role that expands to an ordered selector chain.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRole {
	/// Stable role identifier without its leading `@`.
	pub id:        Str,
	/// Ordered selectors; the first usable match wins.
	pub selectors: Box<[Str]>,
}
impl ModelRole {
	/// Creates a single-selector role assignment with an optional explicit
	/// thinking level.
	///
	/// `auto` is retained in the selector itself. This keeps non-default role
	/// configuration independent from an active session's thinking state.
	pub fn assignment(
		id: impl Into<Str>,
		selector: &str,
		thinking: Option<&str>,
	) -> Result<Self, SelectionError> {
		let id = id.into();
		let mut chars = id.chars();
		if !chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
			|| !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
		{
			return Err(SelectionError::Invalid(id));
		}
		let selector = role_assignment_selector(selector, thinking)?;
		Ok(Self { id, selectors: Box::new([selector]) })
	}
}

/// Formats one persisted role selector with an explicit thinking annotation.
///
/// A route annotation and thinking annotation cannot both occupy the selector
/// suffix, so attempting to add thinking to a route-qualified selector fails
/// rather than silently discarding either choice.
pub fn role_assignment_selector(
	selector: &str,
	thinking: Option<&str>,
) -> Result<Str, SelectionError> {
	let selector = selector.trim();
	let parsed = parse_selector(selector)?;
	let Some(thinking) = thinking else {
		return Ok(Str::new(selector));
	};
	if !is_thinking_level(thinking) || parsed.route.is_some() {
		return Err(SelectionError::Invalid(Str::new(selector)));
	}
	let mut formatted = String::with_capacity(
		parsed.model.len()
			+ thinking.len()
			+ parsed
				.upstream
				.as_ref()
				.map_or(1, |upstream| upstream.len().saturating_add(2)),
	);
	formatted.push_str(&parsed.model);
	formatted.push(':');
	formatted.push_str(thinking);
	if let Some(upstream) = parsed.upstream {
		formatted.push('@');
		formatted.push_str(&upstream);
	}
	Ok(formatted.into())
}

/// Inserts or replaces one role assignment, returning whether it changed.
///
/// This updates configuration data only. In particular, updating a
/// non-default role never mutates an active session model or thinking level.
pub fn upsert_role_assignment(
	roles: &mut Vec<ModelRole>,
	id: impl Into<Str>,
	selector: &str,
	thinking: Option<&str>,
) -> Result<bool, SelectionError> {
	let replacement = ModelRole::assignment(id, selector, thinking)?;
	if let Some(existing) = roles.iter_mut().find(|role| role.id == replacement.id) {
		if *existing == replacement {
			return Ok(false);
		}
		*existing = replacement;
		return Ok(true);
	}
	roles.push(replacement);
	Ok(true)
}

/// The built-in role vocabulary.  Values remain user-configurable; these ids
/// are the stable public contract used by selectors and persisted settings.
pub const BUILTIN_ROLE_IDS: &[&str] =
	&["default", "smol", "slow", "vision", "plan", "designer", "commit", "tiny", "task", "advisor"];

/// Parsed, syntax-only model selector annotations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSelector {
	/// Model spelling before annotations.
	pub model:    Str,
	/// Optional upstream provider/routing target.
	pub upstream: Option<Str>,
	/// Optional thinking level.
	pub thinking: Option<Str>,
	/// Optional explicit route identifier.
	pub route:    Option<RouteId>,
}

/// Exact catalog identity with annotations retained for the request planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedModel {
	/// Provider owning the selected route.
	pub provider: ProviderId,
	/// Canonical normalized model key.
	pub model:    ModelKey,
	/// Requested upstream routing annotation.
	pub upstream: Option<Str>,
	/// Requested thinking level.
	pub thinking: Option<Str>,
	/// Route requested by the selector, if any.
	pub route:    Option<RouteId>,
}

/// Initial-selection inputs, already collected by the CLI/settings boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct InitialModel<'a> {
	/// `--model` value.
	pub cli_model:    Option<&'a str>,
	/// `--provider` value, applied to an otherwise bare CLI model.
	pub cli_provider: Option<&'a str>,
	/// Persisted default-model setting.
	pub setting:      Option<&'a str>,
	/// `OMP_DEFAULT_MODEL`, then `OMP_MODEL` in that order.
	pub environment:  &'a [Option<&'a str>],
}

/// Selection failures are precise enough for a caller to render a useful
/// picker/error without guessing a fallback.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SelectionError {
	/// A selector has no model portion.
	#[error("model selector is empty")]
	Empty,
	/// Annotation syntax is malformed or duplicated.
	#[error("invalid model selector `{0}`")]
	Invalid(Str),
	/// A role points back to itself through one or more configured roles.
	#[error("model role cycle at @{0}")]
	RoleCycle(Str),
	/// No role with this id is configured or built in.
	#[error("unknown model role @{0}")]
	UnknownRole(Str),
	/// No available model matches the requested selector.
	#[error("unknown model `{0}`")]
	NotFound(Str),
}

/// Parses `:level`, `:route`, and `@upstream` annotations without looking up a
/// catalog. `:max` and `:auto` remain ordinary model text until the caller
/// confirms they are not literal model ids.
pub fn parse_selector(input: &str) -> Result<ParsedSelector, SelectionError> {
	let input = input.trim();
	if input.is_empty() {
		return Err(SelectionError::Empty);
	}
	let (before_upstream, upstream) = split_upstream(input)?;
	let (model, suffix) = before_upstream
		.rsplit_once(':')
		.unwrap_or((before_upstream, ""));
	if model.is_empty() || model.ends_with(':') {
		return Err(SelectionError::Invalid(Str::new(input)));
	}
	let mut parsed =
		ParsedSelector { model: Str::new(model), upstream, thinking: None, route: None };
	if !suffix.is_empty() {
		if is_thinking_level(suffix) {
			parsed.thinking = Some(Str::new(suffix));
		} else if suffix
			.chars()
			.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
		{
			parsed.route = Some(RouteId::new(suffix));
		} else {
			return Err(SelectionError::Invalid(Str::new(input)));
		}
	} else if before_upstream.ends_with(':') {
		return Err(SelectionError::Invalid(Str::new(input)));
	}
	Ok(parsed)
}

fn split_upstream(input: &str) -> Result<(&str, Option<Str>), SelectionError> {
	if let Some(rest) = input.strip_prefix('@') {
		let (upstream, model) = rest
			.split_once('/')
			.ok_or_else(|| SelectionError::Invalid(Str::new(input)))?;
		if upstream.is_empty() || model.is_empty() || model.contains('@') {
			return Err(SelectionError::Invalid(Str::new(input)));
		}
		return Ok((model, Some(Str::new(upstream))));
	}
	match input.rsplit_once('@') {
		None => Ok((input, None)),
		Some((model, upstream))
			if !model.is_empty() && !upstream.is_empty() && !upstream.contains('/') =>
		{
			Ok((model, Some(Str::new(upstream))))
		},
		Some(_) => Err(SelectionError::Invalid(Str::new(input))),
	}
}

/// Matches a selector by pi's ordered cascade: exact provider/id, bare id,
/// alias, provider-scoped fuzzy match, then substring.
///
/// Ambiguity is ranked by MRU, route priority, and canonical identity, never
/// iteration order.
pub fn select_model(
	models: &[ModelSpec],
	routes: &[RouteDef],
	aliases: &[CatalogAlias],
	roles: &[ModelRole],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
	selector: &str,
) -> Result<SelectedModel, SelectionError> {
	select_inner(models, routes, aliases, roles, mru, selector, &mut BTreeSet::new())
}

fn select_inner(
	models: &[ModelSpec],
	routes: &[RouteDef],
	aliases: &[CatalogAlias],
	roles: &[ModelRole],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
	selector: &str,
	visiting: &mut BTreeSet<Str>,
) -> Result<SelectedModel, SelectionError> {
	if let Some(role) = selector
		.strip_prefix('@')
		.or_else(|| selector.strip_prefix("pi/"))
	{
		if role.contains('/') || role.contains(':') {
			return Err(SelectionError::Invalid(Str::new(selector)));
		}
		if !visiting.insert(Str::new(role)) {
			return Err(SelectionError::RoleCycle(Str::new(role)));
		}
		let found = roles
			.iter()
			.find(|candidate| candidate.id == role)
			.ok_or_else(|| SelectionError::UnknownRole(Str::new(role)))?;
		let result = found
			.selectors
			.iter()
			.find_map(|pattern| {
				select_inner(models, routes, aliases, roles, mru, pattern, visiting).ok()
			})
			.ok_or_else(|| SelectionError::NotFound(Str::new(selector)));
		visiting.remove(role);
		return result;
	}
	let parsed = parse_selector(selector)?;
	// Guard `:max`/`:auto`: catalog literals win over suffix interpretation.
	let literal = ModelKey::from(selector);
	if matches!(parsed.thinking.as_deref(), Some("max" | "auto"))
		&& models
			.iter()
			.any(|model| model.key == literal || logical_id(&model.key) == selector)
	{
		return choose(models, routes, mru, selector, None, None, None, &literal, selector);
	}
	let (provider, id) = parsed
		.model
		.split_once('/')
		.map_or((None, parsed.model.as_str()), |(provider, id)| (Some(provider), id));
	if id.is_empty() {
		return Err(SelectionError::Invalid(Str::new(selector)));
	}
	// Catalog keys are `provider/logical` composites; a provider-qualified
	// selector reconstructs the composite while a bare selector matches the
	// logical portion exactly (see `choose`).
	let exact = match provider {
		Some(provider) => ModelKey::new(format!("{provider}/{id}")),
		None => ModelKey::from(id),
	};
	if let Ok(found) = choose(
		models,
		routes,
		mru,
		id,
		provider,
		parsed.route.as_ref(),
		parsed.upstream.clone(),
		&exact,
		selector,
	) {
		return Ok(with_annotations(found, parsed));
	}
	if provider.is_none()
		&& let Some(alias) = aliases.iter().find(|alias| alias.alias == parsed.model)
		&& let Ok(found) = choose(
			models,
			routes,
			mru,
			alias.target.as_str(),
			None,
			parsed.route.as_ref(),
			parsed.upstream.clone(),
			&alias.target,
			selector,
		) {
		return Ok(with_annotations(found, parsed));
	}
	let mut matches = candidates(models, routes, provider, id, parsed.route.as_ref());
	if matches.is_empty() && provider.is_some() {
		matches = candidates(models, routes, provider, id, None);
	}
	if matches.is_empty() {
		matches = candidates(models, routes, provider, id, parsed.route.as_ref());
	}
	matches.retain(|(_, model)| model.key.as_str().contains(id));
	choose_candidates(matches, routes, mru, parsed, selector)
}

fn with_annotations(mut selected: SelectedModel, parsed: ParsedSelector) -> SelectedModel {
	selected.thinking = parsed.thinking;
	selected.upstream = parsed.upstream;
	selected.route = parsed.route;
	selected
}

fn choose(
	models: &[ModelSpec],
	routes: &[RouteDef],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
	_id: &str,
	provider: Option<&str>,
	route: Option<&RouteId>,
	upstream: Option<Str>,
	exact: &ModelKey,
	original: &str,
) -> Result<SelectedModel, SelectionError> {
	let candidates = candidates(models, routes, provider, exact.as_str(), route)
		.into_iter()
		.filter(|(_, model)| model.key == *exact || logical_id(&model.key) == exact.as_str())
		.collect();
	choose_candidates(
		candidates,
		routes,
		mru,
		ParsedSelector {
			model: exact.clone().into_inner(),
			upstream,
			thinking: None,
			route: route.cloned(),
		},
		original,
	)
}

fn candidates<'a>(
	models: &'a [ModelSpec],
	routes: &'a [RouteDef],
	provider: Option<&str>,
	_id: &str,
	route: Option<&RouteId>,
) -> Vec<(ProviderId, &'a ModelSpec)> {
	models
		.iter()
		.filter_map(|model| {
			let route = model
				.routes
				.iter()
				.filter_map(|id| routes.iter().find(|candidate| candidate.id == *id))
				.find(|candidate| {
					provider.is_none_or(|wanted| candidate.provider == wanted)
						&& route.is_none_or(|wanted| candidate.id == *wanted)
				})?;
			Some((route.provider.clone(), model))
		})
		.filter(|(_, model)| model.availability != ModelAvailability::Disabled)
		.collect()
}

/// The key's logical portion, without its provider prefix.
fn logical_id(key: &ModelKey) -> &str {
	key.as_str()
		.split_once('/')
		.map_or(key.as_str(), |(_, rest)| rest)
}

fn choose_candidates(
	candidates: Vec<(ProviderId, &ModelSpec)>,
	routes: &[RouteDef],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
	parsed: ParsedSelector,
	original: &str,
) -> Result<SelectedModel, SelectionError> {
	let Some((provider, model)) = candidates
		.into_iter()
		.max_by(|left, right| rank(left, routes, mru).cmp(&rank(right, routes, mru)))
	else {
		return Err(SelectionError::NotFound(Str::new(original)));
	};
	Ok(SelectedModel {
		provider,
		model: model.key.clone(),
		upstream: parsed.upstream,
		thinking: parsed.thinking,
		route: parsed.route,
	})
}

fn rank(
	candidate: &(ProviderId, &ModelSpec),
	routes: &[RouteDef],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
) -> (u8, u64, u32, std::cmp::Reverse<ProviderId>, std::cmp::Reverse<ModelKey>) {
	let availability = u8::from(candidate.1.availability == ModelAvailability::Available);
	let recent = *mru
		.get(&(candidate.0.clone(), candidate.1.key.clone()))
		.unwrap_or(&0);
	let priority = candidate
		.1
		.routes
		.iter()
		.filter_map(|id| {
			routes
				.iter()
				.find(|route| route.id == *id && route.provider == candidate.0)
		})
		.filter_map(|route| route.priority)
		.max()
		.unwrap_or(0);
	(
		availability,
		recent,
		priority,
		std::cmp::Reverse(candidate.0.clone()),
		std::cmp::Reverse(candidate.1.key.clone()),
	)
}

/// Resolves the initial choice with strict source precedence. Environment
/// values are supplied by the caller to keep this pure and testable.
pub fn select_initial(
	models: &[ModelSpec],
	routes: &[RouteDef],
	aliases: &[CatalogAlias],
	roles: &[ModelRole],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
	initial: InitialModel<'_>,
) -> Result<Option<SelectedModel>, SelectionError> {
	let cli = initial.cli_model.map(|model| match initial.cli_provider {
		Some(provider) if !model.contains('/') => format!("{provider}/{model}"),
		_ => model.to_owned(),
	});
	let choice = cli
		.as_deref()
		.or(initial.setting)
		.or_else(|| initial.environment.iter().flatten().copied().next());
	match choice {
		Some(selector) => select_model(models, routes, aliases, roles, mru, selector).map(Some),
		None => pick_default(models, routes, mru)
			.map(Some)
			.ok_or_else(|| SelectionError::NotFound(sf!("default"))),
	}
}

/// Picks the preferred available catalog model, using MRU only as a tiebreak.
pub fn pick_default(
	models: &[ModelSpec],
	routes: &[RouteDef],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
) -> Option<SelectedModel> {
	let candidates = models
		.iter()
		.filter(|model| model.availability != ModelAvailability::Disabled)
		.flat_map(move |model| {
			model.routes.iter().filter_map(move |route_id| {
				routes
					.iter()
					.find(|route| route.id == *route_id)
					.map(move |route| (route.provider.clone(), model))
			})
		})
		.collect();
	choose_candidates(
		candidates,
		routes,
		mru,
		ParsedSelector { model: Str::default(), upstream: None, thinking: None, route: None },
		"default",
	)
	.ok()
}

/// Finds a cheap/fast fallback. Explicit `@smol` still takes precedence at the
/// role layer; this is only its data-driven fallback.
pub fn find_smol(
	models: &[ModelSpec],
	routes: &[RouteDef],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
) -> Option<SelectedModel> {
	let candidates = models
		.iter()
		.filter(|model| {
			model.availability != ModelAvailability::Disabled && {
				let key = model.key.as_str().to_ascii_lowercase();
				key.contains("mini")
					|| key.contains("small")
					|| key.contains("flash")
					|| key.contains("haiku")
					|| key.contains("nano")
			}
		})
		.flat_map(move |model| {
			model.routes.iter().filter_map(move |id| {
				routes
					.iter()
					.find(|route| route.id == *id)
					.map(move |route| (route.provider.clone(), model))
			})
		})
		.collect();
	choose_candidates(
		candidates,
		routes,
		mru,
		ParsedSelector { model: Str::default(), upstream: None, thinking: None, route: None },
		"smol",
	)
	.ok()
	.or_else(|| pick_default(models, routes, mru))
}

/// Finds a reasoning-capable fallback; the capability fact, not a model-name
/// convention, is authoritative.
pub fn find_slow(
	models: &[ModelSpec],
	routes: &[RouteDef],
	mru: &BTreeMap<(ProviderId, ModelKey), u64>,
) -> Option<SelectedModel> {
	let candidates = models
		.iter()
		.filter(|model| model.availability != ModelAvailability::Disabled && model.thinking.is_some())
		.flat_map(move |model| {
			model.routes.iter().filter_map(move |id| {
				routes
					.iter()
					.find(|route| route.id == *id)
					.map(move |route| (route.provider.clone(), model))
			})
		})
		.collect();
	choose_candidates(
		candidates,
		routes,
		mru,
		ParsedSelector { model: Str::default(), upstream: None, thinking: None, route: None },
		"slow",
	)
	.ok()
	.or_else(|| pick_default(models, routes, mru))
}

fn is_thinking_level(value: &str) -> bool {
	matches!(value, "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "auto")
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn selector_grammar_is_table_driven() {
		for (input, model, upstream, thinking, route) in [
			("gpt", "gpt", None, None, None),
			("gpt:high", "gpt", None, Some("high"), None),
			("gpt:west", "gpt", None, None, Some("west")),
			("gpt@openrouter", "gpt", Some("openrouter"), None, None),
			("@openrouter/gpt:low", "gpt", Some("openrouter"), Some("low"), None),
		] {
			let parsed = parse_selector(input).expect(input);
			assert_eq!(parsed.model.as_str(), model);
			assert_eq!(parsed.upstream.as_deref(), upstream);
			assert_eq!(parsed.thinking.as_deref(), thinking);
			assert_eq!(parsed.route.as_deref(), route);
		}
		for invalid in ["", "@upstream", "gpt@", "gpt::high"] {
			assert!(parse_selector(invalid).is_err(), "{invalid}");
		}
	}

	#[test]
	fn non_default_role_retains_explicit_auto_thinking() {
		let mut roles = vec![
			ModelRole::assignment(Str::new_static("default"), "openai/primary", Some("high"))
				.expect("default assignment"),
		];
		assert!(
			upsert_role_assignment(
				&mut roles,
				Str::new_static("task"),
				"openai-codex/worker",
				Some("auto")
			)
			.expect("task assignment")
		);
		assert_eq!(roles[0].selectors[0].as_str(), "openai/primary:high");
		assert_eq!(roles[1].selectors[0].as_str(), "openai-codex/worker:auto");
		assert!(
			!upsert_role_assignment(
				&mut roles,
				Str::new_static("task"),
				"openai-codex/worker",
				Some("auto")
			)
			.expect("unchanged task assignment")
		);
	}

	#[test]
	fn role_thinking_replacement_preserves_upstream() {
		assert_eq!(
			role_assignment_selector("worker:high@openrouter", Some("auto"))
				.expect("selector")
				.as_str(),
			"worker:auto@openrouter"
		);
		assert!(role_assignment_selector("worker:west", Some("auto")).is_err());
	}

	#[test]
	fn matching_cascade_is_table_driven() {
		let catalog = crate::Catalog::embedded();
		let models = catalog.models();
		// Catalog keys are `provider/logical` composites. Pick a model whose
		// logical id is unambiguous and whose key prefix owns a real route, so
		// both the bare and provider-qualified rungs resolve deterministically.
		let model = models
			.iter()
			.find(|model| {
				let Some((prefix, rest)) = model.key.as_str().split_once('/') else {
					return false;
				};
				!rest.contains('/')
					&& models
						.iter()
						.filter(|other| logical_id(&other.key) == rest)
						.count() == 1
					&& model.routes.iter().any(|id| {
						catalog
							.route(id)
							.is_some_and(|route| route.provider == prefix)
					})
			})
			.expect("uniquely keyed catalog model");
		let (provider, bare) = model.key.as_str().split_once('/').expect("composite key");
		let mru = BTreeMap::new();
		for selector in [bare.to_owned(), model.key.as_str().to_owned()] {
			let selected = select_model(
				catalog.models(),
				catalog.routes(),
				catalog.aliases(),
				&[],
				&mru,
				&selector,
			)
			.expect(&selector);
			assert_eq!(selected.provider.as_str(), provider, "{selector}");
			assert_eq!(selected.model, model.key, "{selector}");
		}
	}
}
