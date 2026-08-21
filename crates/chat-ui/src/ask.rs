//! Interactive `ask@1` presentation over the core `UiRequest` dialog path.

use std::sync::{
	Arc, LazyLock,
	atomic::{AtomicU64, Ordering},
};

use omp_core::{IntoStr, Str, sf};
use omp_proto::omp::ui::v1::{Dialog, UiRequest, ui_request};
use omp_tools::ask::{Answer, AskPresenter, Fault, HeadlessPresenter, Presentation, Question};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};
use parking_lot::Mutex;

use crate::{OverlayPanel, panel_divider};

struct ActiveBinding {
	generation: u64,
	sender:     flume::Sender<AskRequest>,
}

static ACTIVE: LazyLock<Mutex<Option<ActiveBinding>>> = LazyLock::new(|| Mutex::new(None));
static NEXT_BINDING: AtomicU64 = AtomicU64::new(1);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// One typed ask question carried beside its canonical protobuf request.
pub struct AskRequest {
	/// Canonical environment/UI protocol request.
	pub request:  UiRequest,
	/// Typed question used by the core-owned renderer.
	pub question: Question,
	reply:        flume::Sender<Result<Answer, Fault>>,
}

impl AskRequest {
	/// Resolves the blocked tool invocation with a UI answer.
	pub fn answer(self, answer: Answer) {
		let _ = self.reply.send(Ok(answer));
	}

	/// Resolves the blocked tool invocation with a presentation fault.
	pub fn fail(self, message: impl IntoStr) {
		let _ = self
			.reply
			.send(Err(Fault::Presenter { message: message.into_str() }));
	}
}

/// Exclusive binding installed by the active terminal host.
pub struct AskBinding {
	receiver:   flume::Receiver<AskRequest>,
	generation: u64,
}

impl AskBinding {
	/// Receives the next canonical dialog request.
	pub async fn recv(&self) -> Result<AskRequest, flume::RecvError> {
		self.receiver.recv_async().await
	}
}

impl Drop for AskBinding {
	fn drop(&mut self) {
		let mut active = ACTIVE.lock();
		if active
			.as_ref()
			.is_some_and(|binding| binding.generation == self.generation)
		{
			*active = None;
		}
	}
}

/// Binds the process-wide Ask presenter to the currently active TUI host.
/// A later binding cleanly replaces a stale/disconnected host.
#[must_use]
pub fn bind() -> AskBinding {
	let (sender, receiver) = flume::unbounded();
	let generation = NEXT_BINDING.fetch_add(1, Ordering::Relaxed);
	*ACTIVE.lock() = Some(ActiveBinding { generation, sender });
	AskBinding { receiver, generation }
}

/// Presenter registered with `ask@1` during production registry assembly.
#[derive(Default)]
pub struct UiRequestPresenter;

/// Creates the shared presenter used by environment tool registration.
///
/// When no interactive host is bound, it deliberately delegates to the
/// documented deterministic headless policy instead of inventing UI answers.
#[must_use]
pub fn presenter() -> Arc<dyn AskPresenter> {
	Arc::new(UiRequestPresenter)
}

impl AskPresenter for UiRequestPresenter {
	fn present(&self, questions: &[Question]) -> Result<Presentation, Fault> {
		let Some(sender) = ACTIVE.lock().as_ref().map(|binding| binding.sender.clone()) else {
			return HeadlessPresenter.present(questions);
		};
		let mut answers = Vec::with_capacity(questions.len());
		for question in questions {
			let (reply, result) = flume::bounded(1);
			let request =
				AskRequest { request: dialog_request(question), question: question.clone(), reply };
			sender
				.send(request)
				.map_err(|_| presenter_fault("interactive UI disconnected"))?;
			answers.push(
				result
					.recv()
					.map_err(|_| presenter_fault("Ask dialog was dismissed"))??,
			);
		}
		Ok(Presentation { answers, headless: false })
	}
}

fn dialog_request(question: &Question) -> UiRequest {
	UiRequest {
		owner_invocation: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
		kind:             Some(ui_request::Kind::Dialog(Dialog {
			kind:    "ask".to_owned(),
			title:   question.header.as_deref().unwrap_or("Ask").to_owned(),
			content: None,
			choices: question
				.options
				.iter()
				.map(|option| option.label.to_string())
				.collect(),
		})),
		props:            None,
	}
}

const fn presenter_fault(message: &'static str) -> Fault {
	Fault::Presenter { message: sf!(message) }
}

/// Result of routing input through an Ask dialog.
pub enum AskDialogEvent {
	/// Event consumed while the dialog remains open.
	Consumed,
	/// User cancelled the dialog.
	Cancel,
	/// User submitted selected labels.
	Submit(Vec<Str>),
}

/// Core-owned Ask dialog supporting single and multi selection.
pub struct AskDialog {
	ui:       Ui,
	options:  OverlayOptions,
	question: Question,
	selected: Vec<Str>,
	width:    u16,
}

impl AskDialog {
	/// Opens a typed Ask question.
	#[must_use]
	pub fn open(question: Question, ctx: &UiContext) -> Self {
		let width = 72;
		Self {
			ui: build_dialog(&question, width, ctx),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.z(30),
			question,
			selected: Vec::new(),
			width,
		}
	}

	/// Routes a key through the dialog.
	pub fn handle_key(&mut self, key: Key) -> AskDialogEvent {
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted custom input.
	pub fn handle_paste(&mut self, text: &str) -> AskDialogEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer input; an outside click cancels.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> AskDialogEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => AskDialogEvent::Cancel,
			None => AskDialogEvent::Consumed,
		}
	}

	/// Returns the centered composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = self.width.min(viewport.width);
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&mut self, event: UiEvent) -> AskDialogEvent {
		match event {
			UiEvent::Cancel => AskDialogEvent::Cancel,
			UiEvent::Submit if self.question.multi => {
				AskDialogEvent::Submit(std::mem::take(&mut self.selected))
			},
			UiEvent::Changed { value, .. } if self.question.multi => {
				if let Some(at) = self.selected.iter().position(|chosen| chosen == &value) {
					self.selected.remove(at);
				} else {
					self.selected.push(value);
				}
				AskDialogEvent::Consumed
			},
			UiEvent::Changed { value, .. } => AskDialogEvent::Submit(vec![value]),
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Filtered { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_) => AskDialogEvent::Consumed,
		}
	}
}

fn build_dialog(question: &Question, width: u16, ctx: &UiContext) -> Ui {
	let title = question.header.clone().unwrap_or_else(|| sf!("Ask"));
	let prompt = question.question.clone();
	let multi = question.multi;
	let rows = u16::try_from(question.options.len())
		.unwrap_or(u16::MAX)
		.clamp(3, 12);
	let choices = question.options.clone();
	Ui::from_root(
		OverlayPanel::new(title).child(dom! {
			<col>
				<markdown>{prompt}</markdown>
				{panel_divider()}
				<select id="ask" multi={multi} h={rows}>
					for choice in choices {
						<option value={choice.label.clone()} label={choice.label}>
							<col>
								if let Some(description) = choice.description { <text dim>{description}</text> }
								if let Some(preview) = choice.preview { <markdown dim>{preview}</markdown> }
							</col>
						</option>
					}
				</select>
				{panel_divider()}
				<text dim>{if multi { "Space toggle · Enter confirm · Esc cancel" } else { "Enter choose · Esc cancel" }}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

#[cfg(test)]
mod tests {
	use omp_tools::ask::OptionItem;

	use super::*;

	fn question(multi: bool) -> Question {
		Question {
			id: sf!("language"),
			question: sf!("Choose a language"),
			header: Some(sf!("Language")),
			options: vec![
				OptionItem { label: sf!("Rust"), description: None, preview: None },
				OptionItem { label: sf!("Python"), description: None, preview: None },
			],
			multi,
			recommended: Some(0),
		}
	}

	#[test]
	fn presenter_uses_headless_policy_without_bound_host() {
		*ACTIVE.lock() = None;
		let result = UiRequestPresenter.present(&[question(false)]).unwrap();
		assert!(result.headless);
		assert_eq!(result.answers[0].selected, ["Rust"]);
	}

	#[test]
	fn dialog_request_uses_canonical_ui_protocol() {
		let request = dialog_request(&question(true));
		let Some(ui_request::Kind::Dialog(dialog)) = request.kind else {
			panic!("not dialog")
		};
		assert_eq!(dialog.kind, "ask");
		assert_eq!(dialog.choices, ["Rust", "Python"]);
	}
}
