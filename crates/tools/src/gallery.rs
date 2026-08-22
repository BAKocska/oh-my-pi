//! Native renderer lifecycle fixtures used by the visual QA gallery.

use omp_core::Str;
use omp_tool::{Rev, ToolIdentity};

use crate::BuiltinRendererIdentities;

/// One native renderer's synthetic lifecycle inputs for visual QA.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RendererGalleryFixture {
	/// Exact production-compatible renderer identity.
	pub identity:        ToolIdentity,
	/// Human-readable card title.
	pub title:           &'static str,
	/// Serialized typed progress update, when this renderer streams updates.
	pub progress_update: Option<&'static [u8]>,
	/// Serialized typed successful tool outcome.
	pub success_outcome: &'static [u8],
	/// Serialized typed faulted tool outcome.
	pub error_outcome:   &'static [u8],
}

/// Complete native renderer gallery fixture set and its registration keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinRendererGallery {
	/// Exact identities passed through production renderer registration.
	pub identities: BuiltinRendererIdentities,
	/// One four-state fixture contract for each registered renderer.
	pub fixtures:   Vec<RendererGalleryFixture>,
}

/// Builds the complete native renderer fixture contract.
///
/// Serialized values use the production update and durable outcome wire
/// formats. Dispatching them through the native render registry validates every
/// fixture against its renderer's typed fold.
pub fn builtin_renderer_gallery() -> BuiltinRendererGallery {
	fn identity(name: &'static str, family: &'static str) -> ToolIdentity {
		ToolIdentity {
			name: Str::new_static(name),
			rev:  Rev { family: Str::new_static(family), n: 1 },
		}
	}

	let edit = identity("edit", "hl");
	let grep = identity("grep", "");
	let web_search = identity("web_search", "");
	let glob = identity("glob", "");
	let shell = identity("shell", "");
	let hub = identity("hub", "");
	let write = identity("write", "");
	let read = identity("read", "");
	let eval = identity("eval", "");
	let identities = BuiltinRendererIdentities {
		edit:       Some(edit.clone()),
		grep:       Some(grep.clone()),
		web_search: Some(web_search.clone()),
		glob:       Some(glob.clone()),
		shell:      Some(shell.clone()),
		hub:        Some(hub.clone()),
		write:      Some(write.clone()),
		read:       Some(read.clone()),
		eval:       Some(eval.clone()),
	};
	let fixtures = vec![
		RendererGalleryFixture {
			identity: edit,
			title: "edit src/gallery.rs",
			progress_update: Some(
				br#"{"applied_ops":2,"preview":"--- src/gallery.rs\n+++ src/gallery.rs\n+gallery","added_lines":1,"removed_lines":0}"#,
			),
			success_outcome: br#"{"kind":"ok","value":{"sections":[]}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"reason":{"kind":"invalid_patch","message":"gallery patch rejected"},"conflicts":[]}}"#,
		},
		RendererGalleryFixture {
			identity: grep,
			title: "grep gallery",
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"files":[],"total_files":1,"total_files_lower_bound":false,"multi_scope":true,"skip":0,"file_limit_reached":false,"per_file_limit_reached":false,"notes":[],"projected_text":"src/gallery.rs:1:gallery","output_blob":null,"output_shown_lines":1,"output_total_lines":1}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"invalid_regex","message":"unclosed group"}}"#,
		},
		RendererGalleryFixture {
			identity: web_search,
			title: "web_search native gallery",
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"response":{"engine":"gallery","answer":"Native renderer gallery result.","sources":[],"citations":[],"searchQueries":[],"related":[],"warnings":[],"unsupported":[],"account":"","authMode":"","failures":[]}}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"search","code":"gallery","message":"sample provider failure"}}"#,
		},
		RendererGalleryFixture {
			identity: glob,
			title: "glob crates/**/*.rs",
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"matches":[],"missing_paths":[],"timed_out":false,"truncated":false,"result_limit_reached":null,"partial_match_count":1,"timeout_ms":30000,"projected_text":"crates/app/src/gallery_cmd.rs","output_blob":null,"output_shown_lines":1,"output_total_lines":1}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"invalid_pattern","pattern":"[","message":"unclosed character class"}}"#,
		},
		RendererGalleryFixture {
			identity: shell,
			title: "shell printf gallery",
			progress_update: Some(
				br#"{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1,"exec_id":[1],"started":true,"terminal":false}"#,
			),
			success_outcome: br#"{"kind":"ok","value":{"session_id":[1],"exec_id":[1],"command":"printf gallery","transcript":[{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1}],"adjustments":[],"status":{"outcome":"exited","exit_code":0,"signal":null,"wall_clock_ms":12,"spilled_output":null,"aborted":false,"effects_unknown":false,"final_cwd_uri":null,"final_cwd_revision":0}}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"resource","operation":"execute","message":"sample process failure"}}"#,
		},
		RendererGalleryFixture {
			identity: hub,
			title: "hub jobs",
			progress_update: Some(
				br#"{"text":"{\"waitingMs\":250,\"jobs\":[]}","useless":true}"#,
			),
			success_outcome: br#"{"kind":"ok","value":{"text":"{\"peers\":[{\"name\":\"Gallery\",\"status\":\"running\",\"unreadCount\":0,\"parent\":\"Main\"}]}","useless":false}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"message":"sample coordination failure"}}"#,
		},
		RendererGalleryFixture {
			identity: write,
			title: "write gallery.txt",
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"resolved_path":"/tmp/gallery.txt","display_path":"gallery.txt","byte_len":7,"reported_len":7,"disposition":"created","stripped_wrapper":false,"made_executable":false,"snapshot_tag":"ABCD","operation":{"kind":"plain"}}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"document","message":"sample write failure"}}"#,
		},
		RendererGalleryFixture {
			identity: read,
			title: "read src/gallery.rs",
			progress_update: Some(br#"{"phase":"reading source"}"#),
			success_outcome: br#"{"kind":"ok","value":{"parts":[{"kind":"text","text":"1: gallery fixture"}]}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"source","message":"sample source failure"}}"#,
		},
		RendererGalleryFixture {
			identity: eval,
			title: "eval gallery",
			progress_update: Some(
				br#"{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1}"#,
			),
			success_outcome: br#"{"kind":"ok","value":{"session_id":[1],"cell_id":[1],"language":"py","title":"gallery","code":"print('gallery')","reset":false,"frames":[{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1}],"result":null,"display_outputs":[],"status":{"outcome":"complete","exit_code":0,"duration_ms":4,"exception":null},"truncated":false,"spilled_output":null,"total_lines":1,"total_bytes":8}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"resource","operation":"run","message":"sample eval failure"}}"#,
		},
	];
	BuiltinRendererGallery { identities, fixtures }
}
#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use bytes::Bytes;
	use omp_tool::render::{RenderRegistry, ViewState};

	use super::builtin_renderer_gallery;
	use crate::register_builtin_renderers;

	#[test]
	fn fixtures_cover_every_registered_renderer_and_decode_all_states() {
		let gallery = builtin_renderer_gallery();
		let fixture_identities = gallery
			.fixtures
			.iter()
			.map(|fixture| fixture.identity.clone())
			.collect::<BTreeSet<_>>();
		let mut registry = RenderRegistry::new();
		register_builtin_renderers(&mut registry, gallery.identities).unwrap();
		let registry_identities = registry.identities().cloned().collect::<BTreeSet<_>>();
		assert_eq!(fixture_identities, registry_identities);

		for fixture in &gallery.fixtures {
			let mut fold = ViewState::new();
			registry.view(&fixture.identity, &fold, None).unwrap();
			if let Some(update) = fixture.progress_update {
				registry
					.fold(&fixture.identity, &mut fold, Bytes::from_static(update))
					.unwrap();
			}
			registry.view(&fixture.identity, &fold, None).unwrap();
			registry
				.view(&fixture.identity, &fold, Some(fixture.success_outcome))
				.unwrap();
			registry
				.view(&fixture.identity, &fold, Some(fixture.error_outcome))
				.unwrap();
		}
	}
}
