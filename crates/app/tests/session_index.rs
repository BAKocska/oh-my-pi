//! Session-index integration checks for attached chat journals.

use std::sync::Arc;

use omp_agent::Journal;
use omp_core::Str;
use omp_proto::{inference::v1 as pb, thread::v1 as thread_pb};
use omp_storage::{
	index::{NewSession, SessionFilter, SessionIndex, SessionKind},
	transcript::{Header, SessionId, TitleSource, TurnInputRecord, TurnOptionsRecord, TurnStart},
};
use tempfile::tempdir;

#[test]
fn attached_chat_journal_publishes_title_and_canonical_usage_to_project_index() {
	let scratch = tempdir().expect("temporary project state");
	let root = scratch.path().join("project");
	let sessions = scratch.path().join("sessions");
	std::fs::create_dir_all(&root).expect("project root");
	std::fs::create_dir_all(&sessions).expect("sessions directory");

	let index = Arc::new(
		SessionIndex::open(scratch.path().join("sessions.sqlite3"))
			.expect("authoritative project index"),
	);
	let id = SessionId(Str::from("01K3CHATINDEX00000000000000"));
	let path = sessions.join("01K3CHATINDEX00000000000000.jsonl");
	let root_text = root.to_string_lossy();
	let mut journal = index
		.create_session(
			&NewSession {
				id:         &id,
				cwd:        root_text.as_ref(),
				project:    root_text.as_ref(),
				created_ms: 1,
				kind:       SessionKind::Interactive,
				parent:     None,
				remote:     false,
			},
			|| {
				let journal = Journal::create(&path, &Header {
					v:       4,
					id:      id.clone(),
					created: 1,
					cwd:     root.clone(),
				})?;
				let watermark = journal.byte_watermark()?;
				Ok::<_, omp_agent::JournalError>((journal, watermark))
			},
		)
		.unwrap_or_else(|error| panic!("journal header and index row failed: {error}"));
	journal.attach_session_index(Arc::clone(&index), id);

	journal
		.append_title(2, Str::from("Indexed chat"), TitleSource::Assistant)
		.expect("title and index commit");
	journal
		.start_turn(3, TurnStart {
			turn_id:            Str::from("turn-1"),
			item_events:        Vec::new(),
			prompt_hash:        [7; 32],
			prompt_head_events: Vec::new(),
			toolset_hash:       [8; 32],
			enabled_tools:      Vec::new(),
			sequence_targets:   Vec::new(),
			input:              TurnInputRecord::Full {
				thread: thread_pb::Thread { items: Vec::new() },
			},
			options:            TurnOptionsRecord {
				context_id: None,
				params:     pb::ChatParams::default(),
				executor:   None,
				props:      None,
			},
		})
		.expect("durable turn start");
	journal
		.append_gateway_outcome(4, "turn-1", pb::Outcome {
			stop: pb::StopReason::StopEndTurn as i32,
			usage: Some(pb::Usage {
				input_tokens: 120,
				output_tokens: 30,
				cache_read_tokens: 40,
				cache_write_tokens: 5,
				accuracy: pb::usage::Accuracy::Exact as i32,
				total_tokens: Some(195),
				context_tokens: Some(8_000),
				premium_requests: Some(1),
				reasoning_tokens: Some(9),
				..pb::Usage::default()
			}),
			cost: Some(pb::Cost { nanos_usd: 42_000, ..pb::Cost::default() }),
			provider: "anthropic".to_owned(),
			model: "claude-opus".to_owned(),
			duration_ms: Some(250),
			..pb::Outcome::default()
		})
		.expect("receipt and index commit");

	let page = index
		.list(&SessionFilter {
			project: Some(Str::from(root_text.as_ref())),
			..SessionFilter::default()
		})
		.expect("list from write-time index");
	assert_eq!(page.sessions.len(), 1);
	let session = &page.sessions[0];
	assert_eq!(session.title.as_deref(), Some("Indexed chat"));
	assert_eq!(session.title_source, Some(TitleSource::Assistant));
	assert_eq!(session.turns, 1);
	assert_eq!(session.usage.input_tokens, 120);
	assert_eq!(session.usage.reasoning_tokens, Some(9));
	assert_eq!(session.cost.nanos_usd, 42_000);
	assert_eq!(session.models.as_slice(), [Str::from("anthropic/claude-opus")]);
}
