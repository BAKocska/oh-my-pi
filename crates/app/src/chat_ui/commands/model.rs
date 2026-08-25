//! Structural durable and session model-selection routes.

use super::command;

command!(model, 200, "model", icon: Model, ["models"], "Change the durable default model", [Model, Owner], false, optional("[model]") => |host, selector| host.model(selector));
command!(switch, 210, "switch", icon: Swap, [], "Change this session's model", [Model, Session], false, optional("[model]") => |host, selector| host.switch(selector));
command!(extended_context, 215, "extended-context", icon: Expand, [], "Toggle premium long-context windows", [Model, Session, Owner], false, typed("[on|off|status]", ["on", "off", "status"], parse_extended_context) => |host, action| host.extended_context(action));

/// Catalog-backed extended-context selection for the current model.
#[derive(Debug)]
pub(crate) struct ExtendedContextSelection {
	/// Whether the current selection is the extended member of its model pair.
	pub enabled: bool,
	/// Canonical model key to select, absent when no switch is needed.
	pub target:  Option<omp_core::Str>,
	/// Effective context window of the selected member.
	pub window:  Option<u64>,
}

/// Resolves a catalog model's standard/extended pair and the requested action.
pub(crate) fn resolve_extended_context(
	current: &str,
	action: &str,
) -> miette::Result<ExtendedContextSelection> {
	use omp_catalog::{Catalog, ModelKey};

	let catalog = Catalog::embedded();
	let current_spec = catalog
		.model(ModelKey::from_ref(current))
		.or_else(|| catalog.resolve_alias(current))
		.ok_or_else(|| miette::miette!("unknown current model `{current}`"))?;
	let pair = extended_pair(catalog, current_spec).ok_or_else(|| {
		miette::miette!("Model `{}` does not support extended context.", current_spec.key)
	})?;
	let enabled = current_spec.key == pair.1.key;
	let desired = match action {
		"on" => true,
		"off" => false,
		"status" => enabled,
		"toggle" => !enabled,
		_ => return Err(miette::miette!("usage: /extended-context [on|off|status]")),
	};
	let target = if desired == enabled {
		None
	} else if desired {
		Some(omp_core::Str::from(pair.1.key.to_string()))
	} else {
		Some(omp_core::Str::from(pair.0.key.to_string()))
	};
	let selected = if desired { pair.1 } else { pair.0 };
	Ok(ExtendedContextSelection { enabled: desired, target, window: selected.limits.context_window })
}

fn extended_pair<'a>(
	catalog: &'a omp_catalog::Catalog,
	current: &'a omp_catalog::ModelSpec,
) -> Option<(&'a omp_catalog::ModelSpec, &'a omp_catalog::ModelSpec)> {
	use omp_catalog::ModelKey;

	if let Some(target) = current
		.context_promotion_target
		.as_ref()
		.and_then(|target| catalog.model(target))
		.filter(|target| target.limits.context_window > current.limits.context_window)
	{
		return Some((current, target));
	}
	if let Some(standard) = catalog.models().iter().find(|candidate| {
		candidate.context_promotion_target.as_ref() == Some(&current.key)
			&& candidate.limits.context_window < current.limits.context_window
	}) {
		return Some((standard, current));
	}
	let key = current.key.as_str();
	if let Some(standard_key) = key.strip_suffix("-1m")
		&& let Some(standard) = catalog.model(ModelKey::from_ref(standard_key))
	{
		return Some((standard, current));
	}
	let extended_key = format!("{key}-1m");
	catalog
		.model(ModelKey::from_ref(&extended_key))
		.map(|extended| (current, extended))
}

fn parse_extended_context(args: &str) -> miette::Result<omp_core::Str> {
	let action = match args.trim().to_ascii_lowercase().as_str() {
		"" | "toggle" => "toggle",
		"on" => "on",
		"off" => "off",
		"status" => "status",
		_ => return Err(miette::miette!("usage: /extended-context [on|off|status]")),
	};
	Ok(omp_core::Str::new(action))
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extended_context_uses_catalog_promotion_pairs() {
		let catalog = omp_catalog::Catalog::embedded();
		let standard = catalog
			.models()
			.iter()
			.find(|model| {
				extended_pair(catalog, model).is_some_and(|(standard, _)| standard.key == model.key)
			})
			.expect("embedded catalog has an extended-context promotion pair");
		let extended = extended_pair(catalog, standard)
			.expect("selected standard model retains its pair")
			.1;
		let selection =
			resolve_extended_context(standard.key.as_str(), "on").expect("model supports promotion");
		assert!(selection.enabled);
		assert_eq!(selection.target.as_deref(), Some(extended.key.as_str()),);
	}

	#[test]
	fn extended_context_rejects_models_without_a_catalog_capability() {
		let catalog = omp_catalog::Catalog::embedded();
		let unsupported = catalog
			.models()
			.iter()
			.find(|model| extended_pair(catalog, model).is_none())
			.expect("embedded catalog has an ordinary model");
		let error = resolve_extended_context(unsupported.key.as_str(), "on")
			.expect_err("ordinary model must reject extended context");
		assert!(
			error
				.to_string()
				.contains("does not support extended context")
		);
	}
}
