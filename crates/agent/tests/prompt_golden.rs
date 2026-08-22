//! Golden parity oracle for the typed canonical prompt pipeline.

use std::{path::PathBuf, sync::Arc};

use bytes::Bytes;
use omp_agent::{
	ActiveRepositoryInput, BandHash, CanonicalPromptSource, ContextFile, EagerTaskPolicy,
	HostInfoInput, ModelPromptInput, MutationPromptInput, Personality, PromptCapabilitiesInput,
	PromptDelegationInput, PromptMemoryInput, PromptMemorySlotInput, PromptNamedInput, PromptOut,
	PromptPatchSet, PromptSchemeInput, PromptSettingsInput, PromptSource, PromptToolExampleInput,
	PromptToolInput, RepositoryInput, SlotAssembler, SlotClass, SlotDecl, SlotId, SlotPatch,
	SlotRegistration, SlotSource, ToolInventoryMode, VcsIdentity, PromptFacts, Props,
	WorkspaceRootInput,
	WorkspaceRootsInput, WorkspaceTreeInput,
};
use omp_core::Str;
use omp_proto::thread::v1 as thread;
use omp_scribe::canon::canonicalize_prompt;

#[derive(Debug)]
#[allow(dead_code, reason = "fields are serialized through Debug by insta")]
struct GoldenItem {
	index: usize,
	band: &'static str,
	text: String,
}

fn item_text(item: &thread::Item) -> &str {
	let Some(thread::item::Kind::Message(message)) = item.kind.as_ref() else {
		panic!("prompt item must be a message");
	};
	let Some(thread::part::Kind::Text(text)) =
		message.parts.first().and_then(|part| part.kind.as_ref())
	else {
		panic!("prompt message must contain text");
	};
	text
}

fn canonical_snapshot(workspace: &PromptFacts) -> Vec<GoldenItem> {
	let props = workspace.props().expect("golden facts");
	let (items, _bands) = CanonicalPromptSource
		.banded_render(&props)
		.expect("canonical prompt render")
		.expect("canonical source is banded");
	items
		.iter()
		.enumerate()
		.map(|(index, item)| GoldenItem {
			index,
			band: match index {
				0 => "frozen+stable",
				1..=3 => "stable",
				4 | 5 => "epochal",
				_ => "volatile",
			},
			text: canonicalize_prompt(item_text(item)),
		})
		.collect()
}

fn tool(name: &'static str, family: &'static str) -> PromptToolInput {
	PromptToolInput {
		name: Str::new_static(name),
		revision: omp_tool::Rev { family: Str::new_static(family), n: 1 },
		description: Str::new_static("Golden tool declaration."),
		schema: Bytes::from_static(
			br#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
		),
		examples: Arc::from([PromptToolExampleInput {
			label: Some(Str::new_static("path lookup")),
			arguments: Bytes::from_static(br#"{"path":"src/lib.rs"}"#),
		}]),
		docs: Some(Str::new_static("Golden long-form documentation.")),
	}
}

fn full_workspace(
	personality: Personality,
	tool_inventory: ToolInventoryMode,
	codex_task_policy: bool,
) -> PromptFacts {
	PromptFacts {
		cwd: PathBuf::from("/workspace/project"),
		vcs: Some(VcsIdentity::new("/workspace/project", "main@abc123")),
		context_files: Arc::from([
			ContextFile::new(
				"AGENTS.md",
				Bytes::from_static(b"Unique context paragraph.\n\nShared duplicate paragraph."),
			)
			.with_origin("discovery://agents"),
			ContextFile::new("notes.txt", Bytes::from_static(b"Second context file."))
				.with_origin("user://context"),
		]),
		roots: WorkspaceRootsInput {
			revision: 17,
			primary: Some(WorkspaceRootInput::new(
				"file:///workspace/project",
				Bytes::from_static(b"primary"),
			)),
			roots: Arc::from([
				WorkspaceRootInput::new(
					"file:///workspace/project",
					Bytes::from_static(b"primary"),
				),
				WorkspaceRootInput::new("file:///workspace/shared", Bytes::from_static(b"shared")),
			]),
		},
		host: HostInfoInput {
			os: Str::new_static("darwin 25.6"),
			kernel: Str::new_static("Darwin 25.6"),
			architecture: Str::new_static("arm64"),
			cpu: Str::new_static("Apple M4 Max"),
			gpus: Arc::from([Str::new_static("Apple M4 Max")]),
			terminal: Str::new_static("kitty"),
		},
		repositories: Arc::from([
			RepositoryInput {
				root_uri: Str::new_static("file:///workspace/project"),
				worktree_root_uri: Str::new_static("file:///workspace/project"),
				primary_root_uri: Str::new_static("file:///workspace/project"),
				head: Str::new_static("abc123"),
				branch: Str::new_static("main"),
				staged: 1,
				unstaged: 2,
				untracked: 3,
				revision: 9,
				truncated: false,
			},
			RepositoryInput {
				root_uri: Str::new_static("file:///workspace/shared"),
				worktree_root_uri: Str::new_static("file:///workspace/shared"),
				primary_root_uri: Str::new_static("file:///workspace/shared"),
				head: Str::new_static("def456"),
				branch: Str::new_static("feature"),
				truncated: true,
				..Default::default()
			},
		]),
		directory_context: Arc::from([
			Str::new_static("nested/AGENTS.md"),
			Str::new_static("nested/deeper/RULES.md"),
		]),
		workspace_trees: Arc::from([
			WorkspaceTreeInput {
				root_uri: Str::new_static("file:///workspace/project"),
				rendered: Str::new_static("src/\n  lib.rs\ntests/"),
				truncated: false,
			},
			WorkspaceTreeInput {
				root_uri: Str::new_static("file:///workspace/shared"),
				rendered: Str::new_static("fixtures/\n"),
				truncated: true,
			},
		]),
		active_repository: Some(ActiveRepositoryInput {
			relative_root: Str::new_static("nested/repository"),
		}),
		rules: Arc::from([
			PromptNamedInput {
				id: Str::new_static("rust"),
				origin: Str::new_static("rule://rust"),
				content: Str::new_static("Shared duplicate paragraph.\n\nUse typed errors."),
			},
			PromptNamedInput {
				id: Str::new_static("tests"),
				origin: Str::new_static("rule://tests"),
				content: Str::new_static("Test observable behavior."),
			},
		]),
		skills: Arc::from([
			PromptNamedInput {
				id: Str::new_static("react"),
				origin: Str::new_static("skill://react"),
				content: Str::new_static("React implementation guidance."),
			},
			PromptNamedInput {
				id: Str::new_static("tla"),
				origin: Str::new_static("skill://tla"),
				content: Str::new_static("TLA specification guidance."),
			},
		]),
		model: ModelPromptInput {
			identifier: Str::new_static("openai-codex/gpt-5.6-sol"),
			codex_task_policy,
		},
		capabilities: PromptCapabilitiesInput {
			registry_revision: 31,
			tools: Arc::from([
				tool("ast_edit", "ast"),
				tool("bash", "shell"),
				tool("dyn", "device"),
				tool("edit", "hl"),
				tool("glob", "glob"),
				tool("grep", "regex"),
				tool("inspect_image", "vision"),
				tool("read", "read"),
				tool("task", "task"),
				tool("write", "write"),
			]),
			devices: Arc::from([]),
			schemes: Arc::from([
				PromptSchemeInput {
					name: Str::new_static("artifact"),
					readable: true,
					mintable: true,
					selectors: true,
					description: Str::new_static("durable artifacts"),
				},
				PromptSchemeInput {
					name: Str::new_static("skill"),
					readable: true,
					mintable: false,
					selectors: false,
					description: Str::new_static("installed skills"),
				},
			]),
			computer: true,
			delegation: PromptDelegationInput {
				enabled: true,
				eager: EagerTaskPolicy::Always,
				batch: true,
				concurrency: 8,
				queued: 2,
				scout_available: true,
				coordination: true,
			},
			mutations: MutationPromptInput {
				format_on_write: true,
				fetch: true,
				editor: true,
				escalation: true,
			},
			device_guidance: Some(Str::new_static("Use mounted dynamic devices deliberately.")),
			auto_qa_guidance: Some(Str::new_static("File inconsistent tool behavior through AutoQA.")),
		},
		settings: PromptSettingsInput {
			personality,
			personality_override: None,
			include_model: true,
			include_workstation: true,
			include_workspace_tree: true,
			render_mermaid: true,
			include_skills: true,
			tool_inventory,
			intent_field: Some(Str::new_static("intent")),
			secrets_enabled: true,
			custom_prompt: None,
			append_prompt: None,
			null_prompt: false,
		},
		memory: PromptMemoryInput {
			memory: PromptMemorySlotInput {
				generation: 3,
				content: Some(Str::new_static("<memory>Remember architecture.</memory>")),
			},
			standing: PromptMemorySlotInput {
				generation: 4,
				content: Some(Str::new_static("<standing>Preserve behavior.</standing>")),
			},
			recall: PromptMemorySlotInput {
				generation: 5,
				content: Some(Str::new_static("<recall>Current target.</recall>")),
			},
		},
	}
}

#[test]
fn canonical_prompt_full_matrix() {
	insta::assert_debug_snapshot!("canonical_default", canonical_snapshot(&PromptFacts::default()));
	for personality in [
		Personality::Default,
		Personality::Friendly,
		Personality::Pragmatic,
		Personality::None,
	] {
		for inventory in [ToolInventoryMode::Compact, ToolInventoryMode::Full] {
			for codex in [false, true] {
				let name = format!(
					"canonical_full_{}_{}_codex_{codex}",
					personality.to_string(),
					inventory.to_string(),
				);
				insta::assert_debug_snapshot!(name, canonical_snapshot(&full_workspace(personality, inventory, codex)));
			}
		}
	}
	let mut overridden = full_workspace(Personality::Friendly, ToolInventoryMode::Compact, false);
	overridden.settings.personality_override = Some(Str::new_static("Golden personality override."));
	insta::assert_debug_snapshot!("canonical_personality_override", canonical_snapshot(&overridden));

	let mut custom = full_workspace(Personality::Pragmatic, ToolInventoryMode::Compact, true);
	custom.settings.custom_prompt = Some(Str::new_static(
		"Custom role paragraph.\n\nShared duplicate paragraph.",
	));
	insta::assert_debug_snapshot!("canonical_custom_role", canonical_snapshot(&custom));

	let mut appended = full_workspace(Personality::Default, ToolInventoryMode::Compact, false);
	appended.settings.append_prompt = Some(Str::new_static("Appended golden guidance."));
	insta::assert_debug_snapshot!("canonical_append_guidance", canonical_snapshot(&appended));

	let mut null = full_workspace(Personality::Default, ToolInventoryMode::Compact, false);
	null.settings.null_prompt = true;
	insta::assert_debug_snapshot!("canonical_null_prompt", canonical_snapshot(&null));
}

#[derive(Clone)]
struct TextSource(&'static str);

impl SlotSource for TextSource {
	fn render(
		&self,
		_props: &Props,
		out: &mut dyn PromptOut,
	) -> Result<(), omp_agent::PromptError> {
		out.write_str(self.0);
		Ok(())
	}
}

fn registration(slot: SlotId, class: SlotClass, owner: &'static str, text: &'static str) -> SlotRegistration {
	SlotRegistration {
		decl: SlotDecl { slot, class, owner: Str::new_static(owner), priority: 0 },
		source: Arc::new(TextSource(text)),
	}
}

#[test]
fn slot_patch_matrix() {
	let patches = PromptPatchSet::new(
		vec![
			SlotPatch::Prepend { slot: SlotId::Policy, content: Str::new_static("pre-"), priority: 2 },
			SlotPatch::Append { slot: SlotId::Policy, content: Str::new_static("-post"), priority: 1 },
			SlotPatch::Override { slot: SlotId::Workflow, content: Str::new_static("replacement") },
			SlotPatch::Elide { slot: SlotId::Recall },
		],
		PromptPatchSet::DEFAULT_MAX_BYTE_EXPANSION,
	)
	.expect("valid golden patches");
	let assembler = SlotAssembler::new(vec![
		registration(SlotId::Policy, SlotClass::Stable, "policy", "base"),
		registration(SlotId::Workflow, SlotClass::Stable, "workflow", "old"),
		registration(SlotId::Recall, SlotClass::Volatile, "recall", "elided"),
	])
	.with_patches(patches);
	let (rendered, bands): (_, [BandHash; 4]) = assembler
		.render_banded(&Props::new())
		.expect("patched slot render");
	let snapshot = rendered
		.items
		.iter()
		.enumerate()
		.map(|(index, item)| GoldenItem {
			index,
			band: match index { 0 => "stable", 1 => "epochal", _ => "volatile" },
			text: canonicalize_prompt(item_text(item)),
		})
		.collect::<Vec<_>>();
	assert_ne!(bands[1].as_bytes(), &[0; 32]);
	insta::assert_debug_snapshot!("slot_patch_append_prepend_override_elide", snapshot);
}
