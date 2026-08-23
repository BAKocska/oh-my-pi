//! Proves verified manifests retain CONTROL declarations and runtime drift
//! cannot gain authority.
use std::collections::BTreeMap;

use omp_agent::HookPhase;
use omp_core::{ArtifactDigest, Provenance, Str, sf};
use omp_envd::{
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, HookDeclarationKey, ServiceKey,
		ServiceManifest, ToolDeclarationKey,
	},
	site::VerifiedDeclarationSnapshot,
};
use omp_ext::config::{StaticDeclarationClass, StaticDeclarations};

fn verified_properties() -> BTreeMap<Str, serde_json::Value> {
	serde_json::from_value(serde_json::json!({
		"capabilities": {"net": ["api.example.test:443"], "workers": ["gpu"]},
		"declarations": [
			{"id": "tool", "kind": "soft", "module": "extension.tool"},
			{"id": "hook", "kind": "hook", "module": "extension.hook"},
			{"id": "service", "kind": "service", "module": "extension.service"},
			{
				"id": "provider",
				"kind": "provider",
				"module": "extension.provider",
				"trigger": "first_reach",
				"api": 2,
				"grants": ["network.provider"],
				"models": ["test-model"]
			},
			{"id": "campaign", "kind": "campaign"},
			{"id": "command", "kind": "command"},
			{"id": "shortcut", "kind": "shortcut"},
			{"id": "renderer", "kind": "verdict_renderer"},
			{"id": "completion", "kind": "completion"},
			{"id": "subscription", "kind": "telemetry_subscription"},
			{"id": "export", "kind": "telemetry_export"},
			{"id": "prompt", "kind": "prompt_slot"},
			{"id": "credential", "kind": "credential"},
			{"id": "secret", "kind": "secret"},
			{"id": "worker", "kind": "worker"},
			{"id": "placement", "kind": "placement"}
		]
	}))
	.expect("static manifest properties")
}

fn provenance() -> Provenance {
	Provenance::new(
		sf!("publisher-key"),
		sf!("publisher.extension"),
		sf!("1.0.0"),
		ArtifactDigest::new([7; 32]),
		sf!("workspace"),
		sf!("verified"),
		4,
	)
}

#[test]
fn verified_manifest_retains_every_control_declaration_before_runtime_import() {
	let properties = verified_properties();
	let static_declarations =
		StaticDeclarations::from_properties(&properties).expect("verified declaration projection");
	assert_eq!(static_declarations.rows().count(), 16);
	assert_eq!(static_declarations.providers[0].module, "extension.provider");
	assert_eq!(static_declarations.providers[0].grants.as_ref(), ["network.provider"]);
	assert_eq!(
		static_declarations.capability_grants["net"],
		serde_json::json!(["api.example.test:443"])
	);
	assert_eq!(
		static_declarations.providers[0].properties["models"],
		serde_json::json!(["test-model"])
	);

	let manifest = ExtensionManifest::new_with_static(
		provenance(),
		sf!("extension.entry"),
		[sf!("extension.provider"), sf!("extension.ui")],
		DeclarationSet::new([ToolDeclarationKey::new("declared-tool", "control", 1)], [
			HookDeclarationKey::new("before_tool", HookPhase::Precheck),
		]),
		ServiceManifest::new([ServiceKey::new("publisher.service", 1)], []),
		static_declarations.clone(),
		[],
		[ActivationTrigger::FirstReach],
	);

	assert_eq!(manifest.declaration_modules.as_ref(), [
		"extension.tool",
		"extension.hook",
		"extension.service",
		"extension.provider",
		"extension.ui",
	]);
	assert_eq!(manifest.declarations.tools().len(), 1);
	assert_eq!(manifest.declarations.hooks().len(), 1);
	assert_eq!(manifest.services.provides().len(), 1);
	assert_eq!(manifest.static_declarations().rows().count(), 16);

	assert_eq!(
		manifest.activation_triggers,
		[
			ActivationTrigger::Static,
			ActivationTrigger::FirstReach,
			ActivationTrigger::BeforeFirstPrompt,
			ActivationTrigger::BeforeUiInput,
		]
		.into_iter()
		.collect()
	);

	let site = VerifiedDeclarationSnapshot::from_verified_manifest(
		ArtifactDigest::new([7; 32]),
		manifest.declaration_modules.iter().cloned(),
		&properties,
	)
	.expect("verified site declaration snapshot");
	assert_eq!(site.artifact_digest(), &ArtifactDigest::new([7; 32]));
	assert_eq!(site.declaration_modules(), manifest.declaration_modules.as_ref());
	assert_eq!(site.declarations().rows().count(), 16);
}

#[test]
fn runtime_observation_reports_drift_without_gaining_manifest_authority() {
	let expected =
		StaticDeclarations::from_properties(&verified_properties()).expect("expected declarations");
	let mut runtime = expected.clone();
	runtime.providers = Box::new([]);
	runtime.ui.commands[0].id = sf!("runtime-only-command");

	let drift = expected.drift(&runtime);
	assert!(
		drift
			.missing
			.contains(&(StaticDeclarationClass::Provider, sf!("provider")))
	);
	assert!(
		drift
			.missing
			.contains(&(StaticDeclarationClass::UiCommand, sf!("command")))
	);
	assert!(
		drift
			.unexpected
			.contains(&(StaticDeclarationClass::UiCommand, sf!("runtime-only-command")))
	);
	assert_eq!(expected.providers[0].id, "provider");
}
