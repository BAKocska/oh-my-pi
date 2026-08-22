//! Standalone alternate-screen pickers shared by CLI startup flows.

use std::{io, path::PathBuf};

use omp_chat_ui::{ListPicker, ListRow, PickerEvent};
use omp_core::Str;
use omp_storage::index::{SessionFilter, SessionIndex, SessionInfo};
use omp_tui::{
	Frame, InputEvent, Renderer, Size, Terminal, TerminalEvent, TerminalOptions, TtyOut, UiContext,
};
use thiserror::Error;

/// A session selected before project-scoped authorities are started.
#[derive(Clone, Debug)]
pub(crate) struct SessionSelection {
	pub(crate) session:       SessionInfo,
	pub(crate) sessions_dir:  PathBuf,
	pub(crate) database_path: PathBuf,
}

/// Failure to discover or render a standalone picker.
#[derive(Debug, Error)]
pub(crate) enum PickerError {
	/// Session metadata could not be read from an authoritative index.
	#[error(transparent)]
	Storage(#[from] omp_storage::index::Error),
	/// Terminal entry, input, or rendering failed.
	#[error(transparent)]
	Terminal(#[from] io::Error),
}

/// Lists sessions from every native project index and asks the user to choose
/// one.
pub(crate) async fn pick_session(
	data_dir: &std::path::Path,
	explicit_session_dir: Option<&std::path::Path>,
) -> Result<Option<SessionSelection>, PickerError> {
	let mut sessions = Vec::new();
	if let Some(directory) = explicit_session_dir {
		read_index(directory, &directory.join("sessions.sqlite3"), &mut sessions)?;
	} else {
		let projects = data_dir.join("projects");
		match std::fs::read_dir(&projects) {
			Ok(entries) => {
				for entry in entries.flatten() {
					let path = entry.path();
					if path.is_dir() {
						read_index(
							&path.join("sessions"),
							&path.join("sessions.sqlite3"),
							&mut sessions,
						)?;
					}
				}
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(error) => return Err(error.into()),
		}
	}
	sessions.sort_unstable_by(|left, right| {
		right
			.session
			.updated_ms
			.cmp(&left.session.updated_ms)
			.then_with(|| left.session.id.0.cmp(&right.session.id.0))
	});
	let rows: Vec<ListRow> = sessions
		.iter()
		.map(|selection| ListRow {
			key:    selection.session.id.0.clone(),
			label:  selection
				.session
				.title
				.clone()
				.unwrap_or_else(|| selection.session.id.0.clone()),
			detail: selection.session.cwd.clone(),
		})
		.collect();
	let picked = run_list("Resume session", &rows).await?;
	Ok(picked.and_then(|index| sessions.into_iter().nth(index)))
}

fn read_index(
	sessions_dir: &std::path::Path,
	database: &std::path::Path,
	output: &mut Vec<SessionSelection>,
) -> Result<(), PickerError> {
	if !database.is_file() {
		return Ok(());
	}
	let index = SessionIndex::open_authoritative_reader(database)?;
	let page = index.list(&SessionFilter { limit: 200, ..SessionFilter::default() })?;
	output.extend(page.sessions.into_iter().map(|session| SessionSelection {
		session,
		sessions_dir: sessions_dir.to_owned(),
		database_path: database.to_owned(),
	}));
	Ok(())
}

/// Runs the shared fuzzy picker on an alternate terminal screen.
pub(crate) async fn run_list(title: &str, rows: &[ListRow]) -> Result<Option<usize>, PickerError> {
	if rows.is_empty() {
		return Ok(None);
	}
	let mut terminal = Terminal::enter(TerminalOptions::default())?;
	let mut renderer = Renderer::new(TtyOut::new()?);
	renderer.apply_caps(&terminal.caps())?;
	terminal.enter_alt()?;
	let mut viewport = terminal.size()?;
	let context = UiContext::default().with_terminal_caps(&terminal.caps());
	let mut picker = ListPicker::open(title, rows, 0, &context);
	loop {
		paint(&mut picker, &mut renderer, viewport)?;
		match terminal.next().await? {
			TerminalEvent::Resize => {
				viewport = terminal.take_resize()?.unwrap_or(terminal.size()?);
			},
			TerminalEvent::Input(InputEvent::Key(key)) => match picker.handle_key(key) {
				PickerEvent::Consumed => {},
				PickerEvent::Close => return Ok(None),
				PickerEvent::Pick(index) => return Ok(Some(index)),
			},
			TerminalEvent::Input(InputEvent::Paste(text)) => match picker.handle_paste(&text) {
				PickerEvent::Consumed => {},
				PickerEvent::Close => return Ok(None),
				PickerEvent::Pick(index) => return Ok(Some(index)),
			},
			TerminalEvent::Input(InputEvent::Mouse(report)) => {
				match picker.handle_mouse(report.col, report.row, report.kind, viewport) {
					PickerEvent::Consumed => {},
					PickerEvent::Close => return Ok(None),
					PickerEvent::Pick(index) => return Ok(Some(index)),
				}
			},
			TerminalEvent::Input(event) => {
				let _ = terminal.handle_input_event(&event, &mut renderer)?;
			},
			TerminalEvent::Debug(_) | TerminalEvent::Effect(_) => {},
			TerminalEvent::Closed => return Ok(None),
		}
	}
}

fn paint(
	picker: &mut ListPicker,
	renderer: &mut Renderer<TtyOut>,
	viewport: Size,
) -> io::Result<()> {
	let base = Frame::new(Size::new(viewport.width, 1));
	renderer.preview_overlaid(&base, &[picker.layer(viewport)], viewport.height, "")?;
	Ok(())
}
