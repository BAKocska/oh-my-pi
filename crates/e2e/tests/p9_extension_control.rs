//! Joined-system proof for Python extension CONTROL and DATA authority wiring.

#![cfg(unix)]

#[path = "../src/support/extension.rs"]
mod extension;

use std::time::Duration;

use bytes::Bytes;
use omp_agent::HookPhase;
use omp_core::{ArtifactDigest, Principal, Provenance, sf};
use omp_e2e::{Context as _, Result, error, support::{DEFAULT_TIMEOUT, Scratch, install_omp_binary_env, omp_binary, within}};
use omp_env::{Invocation, InvocationEvent};
use omp_envd::{
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, HookDeclarationKey, QuotaBehavior,
		QuotaSpec, ServiceManifest, ToolDeclarationKey, quota::names,
	},
	policy::Grants,
	worker::{ExtHostConfig, ExtHostSpec, ExternalDomainControlFactories, HostKey},
};
use omp_ext::config::StaticDeclarations;
use omp_proto::{
	env::v1::{InvokeTool, Verdict},
	policy::v1::{DocEffects, EffectEnvelope},
};
use omp_tool::CallOutcome;
use serde_json::{Value, json};

use extension::{ExtensionHarness, recording_ui_factory};

const MODULE: &str = "p9_extension_control";
const SESSION: &str = "p9-extension-control-session";
const DEVICE: &str = "extension_probe";
const BLOCKER: &str = "extension_block";
const PY_EXTENSION: &str = r#"
import asyncio
from dataclasses import dataclass

import omp


@omp.entry_kind("e2e.extension_proof", rev="1", display=False)
@dataclass(frozen=True)
class ExtensionProof:
    message: str


@omp.ui.command("extension-proof", description="E2E manifest verification command")
async def extension_proof_command(*_args, **_kwargs):
    return None


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.REVIEW,
    on_failure=omp.OnFailure.DENY,
    name="e2e.extension_review",
)
async def extension_review(event, ctx):
    assert isinstance(event, omp.ToolCallEvent)
    assert isinstance(event.target, omp.DeviceCall)
    assert ctx.extension == "p9_extension_control"
    return omp.Deny("reviewed by Python extension", code="E2E_REVIEW")


@omp.device(
    name="extension_probe",
    family="proof",
    rev=1,
    schema={
        "type": "object",
        "properties": {"message": {"type": "string"}},
        "required": ["message"],
        "additionalProperties": False,
    },
    effects=omp.Effects(documents=omp.DocEffects(read=True)),
)
async def extension_probe(params, ctx):
    message = params["message"]
    appended = await omp.journal.append(
        ExtensionProof(message), idempotency_key="p9-extension-proof"
    )
    latest = await omp.journal.latest(ExtensionProof)

    artifact_ref = await omp.artifacts.put(
        "artifact:" + message,
        media_type="text/plain",
        description="P9 extension DATA proof",
    )
    artifact_text = await omp.artifacts.read(artifact_ref)

    payload = {
        "call_id": "nested-hook-call",
        "invocation_id": ctx.invocation,
        "target": {
            "kind": "device",
            "name": "extension_probe",
            "family": "proof",
            "rev": "proof.1",
            "args": {"message": message},
        },
        "kind": "device",
        "args": {"message": message},
        "raw_args": "{\"message\":\"joined\"}",
        "repaired": False,
        "turn_id": "turn-p9",
        "session_id": ctx.session,
        "cwd": "/workspace",
        "origin": "model",
        "batch": [],
        "deadline": None,
        "bash": None,
    }
    decision = await omp.hooks.dispatch_hook("tool_call", payload)
    assert isinstance(decision, omp.Deny)

    omp.ui.submit("extension authority is live")
    return {
        "parts": [],
        "details": {
            "entry_id": str(appended),
            "journal_message": latest.value.message,
            "artifact_id": artifact_ref.id,
            "artifact_text": artifact_text,
            "decision": {
                "kind": "deny",
                "reason": decision.reason,
                "code": decision.code,
            },
        },
    }


@omp.device(
    name="extension_block",
    family="proof",
    rev=1,
    schema={
        "type": "object",
        "properties": {"started": {"type": "string"}},
        "required": ["started"],
        "additionalProperties": False,
    },
)
async def extension_block(params, ctx):
    with open(params["started"], "w", encoding="utf-8") as marker:
        marker.write(ctx.invocation)
        marker.flush()
    await asyncio.Event().wait()
"#;

fn extension_config(scratch: &Scratch) -> Result<(ExtHostConfig, flume::Receiver<omp_app::chat_ui::presentation_authority::PresentationEffect>)> {
	install_omp_binary_env().context("exposing worker-capable e2e host")?;
	let mut config = ExtHostConfig::new(
		omp_binary().context("resolving worker-capable e2e host")?,
		Principal::new(sf!("p9-e2e"), sf!("P9 E2E")),
		sf!(SESSION),
		1,
	);
	let key = HostKey::new("workspace", "trusted", MODULE);
	let provenance = Provenance::new(
		sf!("omp-e2e"),
		key.extension().clone(),
		sf!("1.0.0"),
		ArtifactDigest::new([9; 32]),
		key.layer().clone(),
		key.tier().clone(),
		1,
	);
	let properties = serde_json::from_value(json!({
		"declarations": [{"id": "extension-proof", "kind": "command"}]
	}))?;
	let static_declarations = StaticDeclarations::from_properties(&properties)
		.map_err(|source| error(format!("building authenticated manifest declarations: {source}")))?;
	let manifest = ExtensionManifest::new_with_static(
		provenance,
		sf!(MODULE),
		[],
		DeclarationSet::new(
			[
				ToolDeclarationKey::new(DEVICE, "proof", 1),
				ToolDeclarationKey::new(BLOCKER, "proof", 1),
			],
			[HookDeclarationKey::new("tool_call", HookPhase::Review)],
		),
		ServiceManifest::default(),
		static_declarations,
		[
			QuotaSpec::new(names::JOURNAL_APPENDS, 4, 4, None, QuotaBehavior::Hard),
			QuotaSpec::new(names::UI_EFFECTS, 4, 4, None, QuotaBehavior::Hard),
		],
		[ActivationTrigger::FirstReach],
	);
	let mut spec = ExtHostSpec::new(key, manifest);
	spec.python_site = Some(scratch.project().to_owned());
	spec.data_grants = Grants::supported(["env.blob"]);
	config.extensions.push(spec);
	let (ui, effects) = recording_ui_factory();
	config.bind_domain_control_factories(ExternalDomainControlFactories {
		ui: Some(ui),
		..ExternalDomainControlFactories::default()
	});
	Ok((config, effects))
}

fn blob_effects() -> EffectEnvelope {
	EffectEnvelope {
		documents: Some(DocEffects { read: true, write_globs: Vec::new(), props: None }),
		..EffectEnvelope::default()
	}
}

async fn open_invocation(
	client: &omp_env::EnvClient,
	id: &str,
	name: &str,
	args: Value,
	effects: Option<EffectEnvelope>,
) -> Result<Invocation> {
	let mut invocation = within(
		"opening Python extension invocation",
		DEFAULT_TIMEOUT,
		client.invoke(InvokeTool {
			invocation_id: id.to_owned(),
			name: name.to_owned(),
			rev: "proof.1".to_owned(),
			deadline_ms: 10_000,
			..InvokeTool::default()
		}),
	)
	.await??;
	match within("accepting Python extension invocation", DEFAULT_TIMEOUT, invocation.next_event()).await?? {
		Some(InvocationEvent::Accepted(_)) => {},
		other => return Err(error(format!("extension invocation was not accepted: {other:?}"))),
	}
	within(
		"committing Python extension arguments",
		DEFAULT_TIMEOUT,
		invocation.commit_args(
			Bytes::from(serde_json::to_vec(&args)?),
			Bytes::from_static(b"p9-extension-control-token"),
			1,
			effects,
		),
	)
	.await??;
	Ok(invocation)
}

async fn terminal(invocation: &mut Invocation) -> Result<Verdict> {
	loop {
		match within("waiting for Python extension verdict", DEFAULT_TIMEOUT, invocation.next_event()).await?? {
			Some(InvocationEvent::Verdict(verdict)) => return Ok(verdict),
			Some(InvocationEvent::Update(_)) => {},
			Some(other) => return Err(error(format!("unexpected extension event: {other:?}"))),
			None => return Err(error("extension invocation closed before its verdict")),
		}
	}
}

#[tokio::test]
async fn p9_python_extension_exercises_joined_control_and_data_authorities() -> Result<()> {
	let scratch = Scratch::new().context("creating extension control scratch project")?;
	scratch.write(format!("{MODULE}.py"), PY_EXTENSION.as_bytes())?;
	let (config, ui_effects) = extension_config(&scratch)?;
	let harness = ExtensionHarness::spawn(&scratch, config).await?;

	let declarations = harness
		.registry()
		.live_identities()
		.filter(|(name, _)| matches!(name.as_str(), DEVICE | BLOCKER))
		.map(|(name, rev)| (name.to_string(), rev.to_string()))
		.collect::<Vec<_>>();
	assert_eq!(
		declarations,
		vec![(BLOCKER.to_owned(), "proof.1".to_owned()), (DEVICE.to_owned(), "proof.1".to_owned())],
		"manifest-verified Python declarations were not published into the live registry",
	);

	let mut proof = open_invocation(
		harness.client(),
		"p9-extension-proof",
		DEVICE,
		json!({"message": "joined"}),
		Some(blob_effects()),
	)
	.await?;
	let verdict = terminal(&mut proof).await?;
	assert!(!verdict.is_error, "joined Python device faulted: {}", String::from_utf8_lossy(&verdict.json));
	let CallOutcome::Ok(details) = serde_json::from_slice::<CallOutcome<Value, Value>>(&verdict.json)? else {
		return Err(error("joined Python device returned a non-success outcome"));
	};
	assert_eq!(details["journal_message"], "joined");
	assert!(details["entry_id"].as_str().is_some_and(|id| id.starts_with(SESSION)));
	assert_eq!(details["artifact_text"], "artifact:joined");
	assert!(details["artifact_id"].as_str().is_some_and(|id| !id.is_empty()));
	assert_eq!(details["decision"], json!({
		"kind": "deny",
		"reason": "reviewed by Python extension",
		"code": "E2E_REVIEW",
	}));

	let effect = within("observing authoritative UI effect", DEFAULT_TIMEOUT, ui_effects.recv_async()).await??;
	assert_eq!(effect.kind, "submit");
	assert_eq!(effect.body["text"], "extension authority is live");
	let started = scratch.project().join("extension-block-started");
	let mut blocked = open_invocation(
		harness.client(),
		"p9-extension-cancel",
		BLOCKER,
		json!({"started": started}),
		None,
	)
	.await?;
	within("waiting for in-flight Python marker", DEFAULT_TIMEOUT, async {
		loop {
			if started.exists() {
				break;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await?;
	blocked.guard().cancel();
	let cancelled = terminal(&mut blocked).await?;
	let outcome: CallOutcome<Value, Value> = serde_json::from_slice(&cancelled.json)?;
	assert!(matches!(outcome, CallOutcome::Aborted { .. }), "in-flight Python call did not cancel: {outcome:?}");

	harness.shutdown().await;
	Ok(())
}
