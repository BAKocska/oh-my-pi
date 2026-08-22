//! Thin terminal host for the designed chat scene with a scripted backend.

mod mock;

use std::io;

use omp_chat_ui::Chat;
use omp_tui::{UiContext, detect};

fn main() -> io::Result<()> {
	let executor = omp_executor::Executor::new(None);
	executor.clone().block_on(run(executor))
}

async fn run(executor: omp_executor::Executor) -> io::Result<()> {
	let caps = detect();
	let ctx = UiContext::default().with_terminal_caps(&caps);
	let chat = Chat::new(&ctx);
	let (events, intents) = mock::start(&executor);
	omp_chat_ui::host::run(executor, chat, ctx, events, intents).await
}
