//! Native-window adapter for the production retained chat surface.

use std::{cell::RefCell, future::Future, rc::Rc, time::Duration};

use futures::{executor::LocalPool, task::LocalSpawnExt};
use omp_chat_ui::host::{HostExit, HostOutcome, RetainedChat, RetainedChatEffect};
use omp_gui::{Effect, HostConfig, Scene, SceneFrame};
use omp_tui::{Key, MouseReport, Size, UiContext};

/// Runs one production chat scene in a single native GPU window.
pub(crate) fn run<F>(
	build: impl FnOnce(&UiContext) -> RetainedChat,
	bridge: F,
) -> (HostOutcome, miette::Result<()>)
where
	F: Future<Output = miette::Result<()>> + 'static,
{
	let build = Rc::new(RefCell::new(Some(build)));
	let outcome = Rc::new(RefCell::new(None));
	let (result_tx, result_rx) = flume::bounded(1);
	let pool = LocalPool::new();
	pool
		.spawner()
		.spawn_local(async move {
			let _ = result_tx.send(bridge.await);
		})
		.expect("local GUI bridge executor accepted one task");
	let pool = Rc::new(RefCell::new(pool));
	omp_gui::run(HostConfig { multiplex: false, ..HostConfig::default() }, {
		let build = Rc::clone(&build);
		let outcome = Rc::clone(&outcome);
		let pool = Rc::clone(&pool);
		move |ctx| {
			let build = build
				.borrow_mut()
				.take()
				.expect("single-scene GUI host requested another chat scene");
			GuiScene::new(build(ctx), Rc::clone(&outcome), Rc::clone(&pool))
		}
	});
	let outcome = outcome
		.borrow_mut()
		.take()
		.expect("closed GUI scene must report a chat outcome");
	let bridge = pool.borrow_mut().run_until(async move {
		result_rx
			.recv_async()
			.await
			.expect("GUI bridge must complete after its scene drops")
	});
	(outcome, bridge)
}

struct GuiScene {
	chat:        RetainedChat,
	outcome:     Rc<RefCell<Option<HostOutcome>>>,
	bridge_pool: Rc<RefCell<LocalPool>>,
}

impl GuiScene {
	fn new(
		chat: RetainedChat,
		outcome: Rc<RefCell<Option<HostOutcome>>>,
		bridge_pool: Rc<RefCell<LocalPool>>,
	) -> Self {
		Self { chat, outcome, bridge_pool }
	}

	fn apply(&mut self, effect: RetainedChatEffect) -> Effect {
		match effect {
			RetainedChatEffect::Ignored => Effect::Ignored,
			RetainedChatEffect::Consumed => Effect::Consumed,
			RetainedChatEffect::Quit(exit) => {
				self.record_exit(exit);
				Effect::Quit
			},
			RetainedChatEffect::Clipboard(scope) => Effect::Clipboard(scope),
			RetainedChatEffect::SetClipboard(text) => Effect::SetClipboard(text),
		}
	}

	fn record_exit(&self, exit: HostExit) {
		let mut outcome = self.outcome.borrow_mut();
		if outcome.is_none() {
			*outcome = Some(self.chat.outcome(exit));
		}
	}
}

impl Drop for GuiScene {
	fn drop(&mut self) {
		self.record_exit(HostExit::Quit);
	}
}

impl Scene for GuiScene {
	fn resize(&mut self, viewport: Size, settled: bool) {
		self.chat.resize(viewport, settled);
	}

	fn render(&mut self) -> SceneFrame<'_> {
		let frame = self.chat.render();
		SceneFrame {
			frame:       frame.frame,
			viewport:    frame.viewport,
			editor_rows: frame.editor_rows,
			layers:      frame.layers,
		}
	}

	fn key(&mut self, key: Key) -> Effect {
		let effect = self.chat.key(key);
		self.apply(effect)
	}

	fn mouse(&mut self, report: MouseReport) -> Effect {
		let effect = self.chat.mouse(report);
		self.apply(effect)
	}

	fn paste(&mut self, text: &str, raw: bool) -> Effect {
		let effect = self.chat.paste(text, raw);
		self.apply(effect)
	}

	fn poll(&mut self) -> Effect {
		self.bridge_pool.borrow_mut().run_until_stalled();
		let effect = self.chat.poll();
		self.apply(effect)
	}

	fn tick(&self) -> Duration {
		self.chat.tick()
	}
}

#[cfg(test)]
mod tests {
	use omp_chat_ui::{BackendEvent, Chat, host::HostOptions};

	use super::*;

	#[test]
	fn backend_session_transition_closes_the_native_scene() {
		let ctx = UiContext::default();
		let (events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		let outcome = Rc::new(RefCell::new(None));
		let mut scene =
			GuiScene::new(chat, Rc::clone(&outcome), Rc::new(RefCell::new(LocalPool::new())));
		events
			.send(BackendEvent::NewSessionRequested)
			.expect("retained chat receiver remains connected");

		assert_eq!(scene.poll(), Effect::Quit);
		let outcome = outcome.borrow();
		assert_eq!(outcome.as_ref().map(|outcome| &outcome.exit), Some(&HostExit::NewSession));
	}
}
